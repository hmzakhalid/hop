//! Service mode, so the Mac can also use the lock screen, UAC prompts and admin windows.
//!
//! A normal program cannot send input to those: the lock screen and UAC run on a separate
//! secure desktop, and admin windows ignore input from lower-privileged programs. So:
//!
//! - `hop-agent install` (run once as administrator) copies the agent to Program Files, moves
//!   its keys and pairing to ProgramData (readable by SYSTEM and Administrators only), registers
//!   the `HopAgent` service and a firewall rule, and starts it.
//! - The service runs as LocalSystem in session 0 and keeps one helper alive in the active
//!   console session: the same agent, with a SYSTEM token, so it can switch to whichever
//!   desktop has input and inject there.

use crate::platform::STOP_EVENT_PREFIX;
use crate::scm::{open_service, service_state, wide, Missing, SERVICE_NAME};
use crate::backoff::Backoff;
use crate::role::{append_service_log as log_line, computer_name, pairing_banner, system_data_dir, Role, HELPER_COMMAND, STOP_EVENT_FLAG};
use hop_core::agent::{self, PairingWindow};
use hop_core::crypto::{fingerprint, new_pairing_code};
use hop_core::store::{AgentStore, MacRecord};
use std::sync::Arc;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::time::Duration;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_ALREADY_EXISTS, ERROR_SERVICE_MARKED_FOR_DELETE, HANDLE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SecurityImpersonation, SetTokenInformation, TokenElevation, TokenPrimary,
    TokenSessionId, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_ALL_ACCESS, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows_sys::Win32::System::Services::{
    DeleteService, RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW, SERVICE_ACCEPT_SHUTDOWN,
    SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_STATUS,
    SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, SetEvent, TerminateProcess,
    WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW,
};

/// Display name of the service and of its firewall rule.
const DISPLAY_NAME: &str = "Hop Agent";
/// How long to wait for the service to change state.
const STATE_TIMEOUT: Duration = Duration::from_secs(15);
/// Owner Administrators; protected DACL: full control for SYSTEM and Administrators only.
const PRIVATE_SDDL: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
/// Stop events: only SYSTEM may open them.
const EVENT_SDDL: &str = "D:P(A;;GA;;;SY)";


pub fn installed_exe() -> PathBuf {
    let pf = std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\Program Files"));
    pf.join("Hop").join("hop-agent.exe")
}


pub fn is_elevated() -> bool {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(token, TokenElevation, &mut e as *mut _ as *mut _, size_of::<TOKEN_ELEVATION>() as u32, &mut len);
        CloseHandle(token);
        ok != 0 && e.TokenIsElevated != 0
    }
}

/// Absolute path of a Windows tool, so an elevated installer never runs a look-alike planted
/// next to it.
fn sys(tool: &str) -> PathBuf {
    let mut buf = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    let dir = if n > 0 && n < buf.len() { PathBuf::from(String::from_utf16_lossy(&buf[..n])) } else { PathBuf::from("C:\\Windows\\System32") };
    dir.join(tool)
}

fn powershell() -> PathBuf {
    sys("WindowsPowerShell\\v1.0\\powershell.exe")
}

/// Run a Windows tool by full path; report its output if it fails.
fn run(path: &Path, args: &[&str]) -> bool {
    let program = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match Command::new(path).args(args).output() {
        Ok(o) => {
            if !o.status.success() {
                let text = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
                let text = text.trim();
                if !text.is_empty() {
                    eprintln!("  ({program} {}: {text})", args.first().unwrap_or(&""));
                }
            }
            o.status.success()
        }
        Err(e) => {
            eprintln!("  ({program}: {e})");
            false
        }
    }
}

/// Install the service. On a first install it takes the identity and pairing kept in `console`;
/// an upgrade keeps the service's own.
pub fn install(console: &AgentStore) -> Result<(), String> {
    if !is_elevated() {
        return Err("run this from an administrator terminal (right-click Terminal → Run as administrator)".into());
    }
    let exe = installed_exe();
    let data = system_data_dir();
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    println!("Installing the Hop service…");
    stop_service()?;
    // Stop other agents (they hold the port and the program file), but not this process.
    let not_me = format!("PID ne {}", std::process::id());
    let others = ["/FI", "IMAGENAME eq hop-agent.exe", "/FI", &not_me];
    if other_agents_running(&others)? {
        run(&sys("taskkill.exe"), &[&["/F"][..], &others[..]].concat());
        if !wait_until(|| Ok(!other_agents_running(&others)?))? {
            return Err("another hop-agent is still running; close it and try again".into());
        }
    }

    std::fs::create_dir_all(exe.parent().unwrap()).map_err(|e| format!("{}: {e}", exe.display()))?;
    // The installer already put us there (possibly spelled differently): never copy onto ourselves.
    let already_there = matches!((me.canonicalize(), exe.canonicalize()), (Ok(a), Ok(b)) if a == b);
    if !already_there {
        std::fs::copy(&me, &exe).map_err(|e| format!("copying to {}: {e}", exe.display()))?;
    }
    prepare_private_dir(&data)?;
    let d = data.display().to_string();
    let service = AgentStore::new(data.clone());
    if service.adopt(console).map_err(|e| format!("copying the pairing into {d}: {e}"))? {
        println!("  took this PC's key and pairing from {}", console.dir.display());
    }
    let bin = format!("\"{}\" service", exe.display());
    if !delete_service()? {
        return Err("Windows is still removing the old Hop service (close the Services window, or restart), then try again".into());
    }
    if !run(&sys("sc.exe"), &["create", SERVICE_NAME, "binPath=", &bin, "start=", "auto", "DisplayName=", DISPLAY_NAME]) {
        return Err("could not create the service".into());
    }
    run(&sys("sc.exe"), &["description", SERVICE_NAME, "Lets your paired Mac use this PC's keyboard and mouse, including the lock screen."]);
    run(&sys("sc.exe"), &["failure", SERVICE_NAME, "reset=", "86400", "actions=", "restart/2000/restart/5000/restart/10000"]);
    if !set_firewall_rule(Some(&exe)) {
        eprintln!("warning: could not add the firewall rule; Windows may ask when the agent starts.");
    }
    if !run(&sys("sc.exe"), &["start", SERVICE_NAME]) {
        return Err("the service did not start (see Event Viewer → Windows Logs → System)".into());
    }
    println!("Done. The agent now runs as a service and starts with Windows.");
    println!("  program: {}", exe.display());
    println!("  keys and pairing: {d}");
    if let Some(log) = Role::Service.log_path() {
        println!("  log: {}", log.display());
    }
    Ok(())
}

/// Replace the firewall rule with one allowing `program`, or just remove it. True on success;
/// an absent rule is not a failure.
fn set_firewall_rule(program: Option<&Path>) -> bool {
    let mut script = format!(
        "$r = Get-NetFirewallRule -DisplayName '{DISPLAY_NAME}' -ErrorAction SilentlyContinue; \
         if ($r) {{ $r | Remove-NetFirewallRule -ErrorAction Stop }}; "
    );
    if let Some(p) = program {
        script += &format!(
            "New-NetFirewallRule -DisplayName '{DISPLAY_NAME}' -Direction Inbound -Action Allow -Program '{}' -Profile Any -ErrorAction Stop | Out-Null; ",
            p.display()
        );
    }
    // A failure above stops the script with exit code 1 before this.
    script += "exit 0";
    run(&powershell(), &["-NoProfile", "-NonInteractive", "-Command", &script])
}

/// Whether any process matches the `tasklist` filters. A failed listing is an error, never
/// "none running".
fn other_agents_running(filters: &[&str]) -> Result<bool, String> {
    let o = Command::new(sys("tasklist.exe")).args(filters).args(["/NH", "/FO", "CSV"]).output().map_err(|e| format!("tasklist: {e}"))?;
    if !o.status.success() {
        return Err(format!("tasklist failed: {}", String::from_utf8_lossy(&o.stderr).trim()));
    }
    // CSV rows quote the image name; the "no tasks" message (localized) never does.
    Ok(String::from_utf8_lossy(&o.stdout).to_ascii_lowercase().contains("\"hop-agent.exe\""))
}

/// Stop the service if it is installed; `Ok` once it is stopped or absent.
fn stop_service() -> Result<(), String> {
    if service_state()?.is_some_and(|s| s != SERVICE_STOPPED) {
        run(&sys("sc.exe"), &["stop", SERVICE_NAME]);
    }
    if wait_for(|s| s.is_none_or(|s| s == SERVICE_STOPPED))? {
        Ok(())
    } else {
        Err("the Hop service did not stop".into())
    }
}

/// Delete the (stopped) service. `Ok(false)` if Windows has only marked it for deletion,
/// because something still has it open.
fn delete_service() -> Result<bool, String> {
    match open_service(windows_sys::Win32::Storage::FileSystem::DELETE) {
        Ok((svc, _scm)) => unsafe {
            if DeleteService(svc.0) == 0 {
                let code = GetLastError();
                if code != ERROR_SERVICE_MARKED_FOR_DELETE {
                    return Err(format!("could not delete the Hop service: {}", std::io::Error::from_raw_os_error(code as i32)));
                }
            }
        }, // our handles close here, or the deletion could never finish
        Err(Missing::NotInstalled) => return Ok(true),
        Err(Missing::BeingDeleted) => {}
        Err(Missing::Error(e)) => return Err(e),
    }
    wait_for(|s| s.is_none())
}

/// True if `path` and everything directly in it are owned by SYSTEM or Administrators and grant
/// access to nobody else.
fn private_dir_is_safe(path: &Path) -> bool {
    let p = path.display().to_string().replace('\'', "''");
    // Any error at all (unreadable item, unreadable ACL, unresolvable SID) counts as unsafe.
    let script = format!(
        "$ErrorActionPreference='Stop'; try {{ $ok=@('S-1-5-18','S-1-5-32-544'); \
         $items=@(Get-Item -LiteralPath '{p}' -Force) + @(Get-ChildItem -LiteralPath '{p}' -Force); \
         foreach($i in $items){{ if($i.Attributes -band [IO.FileAttributes]::ReparsePoint){{ throw 'reparse point' }}; \
         $a=Get-Acl -LiteralPath $i.FullName; if($null -eq $a){{ throw 'no ACL' }}; \
         if($ok -notcontains $a.GetOwner([Security.Principal.SecurityIdentifier]).Value){{ throw 'owner' }}; \
         foreach($r in $a.Access){{ if($ok -notcontains $r.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value){{ throw 'grant' }} }} }}; \
         'SAFE' }} catch {{ 'UNSAFE'; exit 1 }}"
    );
    match Command::new(powershell()).args(["-NoProfile", "-NonInteractive", "-Command", &script]).output() {
        Ok(o) => o.status.success() && o.stderr.is_empty() && String::from_utf8_lossy(&o.stdout).trim() == "SAFE",
        Err(_) => false,
    }
}

/// Create `path` atomically with the private security descriptor (no window in which another
/// user could get in), or keep an existing one only if it is provably private.
fn prepare_private_dir(path: &Path) -> Result<(), String> {
    if let Ok(md) = std::fs::symlink_metadata(path) {
        use std::os::windows::fs::MetadataExt;
        if md.file_attributes() & 0x400 != 0 || !md.is_dir() {
            return Err(format!("{} is a link or a file; remove it and run install again", path.display()));
        }
        if private_dir_is_safe(path) {
            return Ok(());
        }
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let aside = path.with_file_name(format!("Hop.unsafe-{secs}"));
        std::fs::rename(path, &aside).map_err(|e| format!("{} has unsafe permissions and could not be moved aside: {e}", path.display()))?;
        println!("  {} had permissions other users could use; moved it to {}", path.display(), aside.display());
    }
    create_private_dir(path)
}

/// Create `path` already restricted to SYSTEM and Administrators, in one step.
fn create_private_dir(path: &Path) -> Result<(), String> {
    unsafe {
        let sddl = wide(PRIVATE_SDDL);
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) == 0 {
            return Err("could not build the security descriptor".into());
        }
        let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
        let p = wide(&path.display().to_string());
        let ok = CreateDirectoryW(p.as_ptr(), &sa);
        let err = std::io::Error::last_os_error();
        LocalFree(sd as _);
        if ok == 0 {
            return Err(format!("could not create {}: {err}", path.display()));
        }
    }
    Ok(())
}

/// Forget the paired Mac in the service's protected copy too, and restart the service so the
/// running helper drops it. `Ok(false)` when there is no service and no protected copy.
pub fn revoke_service_pairing() -> Result<bool, String> {
    let store = Role::Service.store();
    let installed = service_state()?.is_some();
    match std::fs::metadata(&store.dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !installed => return Ok(false),
        _ => {}
    }
    if !is_elevated() {
        return Err("the Hop service still holds the pairing; run `hop-agent unpair` as administrator to revoke it".into());
    }
    store.lock().and_then(|mut l| l.forget_peer()).map_err(|e| format!("{}: {e}", store.dir.display()))?;
    if installed {
        restart_service().map_err(|e| format!("the pairing is revoked, but {e}; the old pairing may still be active until it restarts"))?;
    }
    Ok(true)
}

/// Pair a Mac while the service runs, into the service's own protected store. The service's
/// helper holds the UDP port but never the TCP pairing port, so the two do not collide; and if
/// this is cancelled, the service is untouched. Needs an administrator: a Mac paired with the
/// service can type on the lock screen and into UAC prompts.
pub fn pair_beside_service(name: Option<String>, port: u16) -> Result<(), String> {
    if !is_elevated() {
        return Err("the Hop service is installed, so pairing needs an administrator: choose \"Pair a Mac\" from the Hop icon in the taskbar, or run this in an administrator terminal".into());
    }
    let store = Role::Service.store();
    if !store.dir.is_dir() {
        return Err(format!("{} is missing; run hop-agent install again", store.dir.display()));
    }
    let keys = store.keys().map_err(|e| format!("cannot read the service's key in {}: {e}", store.dir.display()))?;
    let name = name.unwrap_or_else(computer_name);
    let listener = std::net::TcpListener::bind(std::net::SocketAddr::from(([0, 0, 0, 0], port)))
        .map_err(|e| format!("cannot listen for pairing on port {port}: {e}"))?;
    let code = new_pairing_code();
    println!("{}", pairing_banner(&code));
    let stop = Arc::new(AtomicBool::new(false));
    crate::platform::on_ctrl_c(stop.clone());
    let paired = agent::pair(listener, &PairingWindow::new(code), &keys, &name, &stop, |why| println!("Pairing attempt failed: {why}"));
    let Some(p) = paired else { return Err("no Mac paired before the code expired".into()) };
    store
        .lock()
        .and_then(|l| l.save_peer(&MacRecord { public: p.public, name: p.name.clone() }))
        .map_err(|e| format!("paired, but saving failed: {e}"))?;
    println!("Paired with {} (key {}).", p.name, fingerprint(&p.public));
    restart_service().map_err(|e| format!("paired, but {e}"))?;
    println!("The Hop service is using the new pairing.");
    Ok(())
}

/// Stop and start the service, so its helper reloads the pairing.
fn restart_service() -> Result<(), String> {
    stop_service()?;
    if run(&sys("sc.exe"), &["start", SERVICE_NAME]) && wait_for(|s| s == Some(SERVICE_RUNNING))? {
        Ok(())
    } else {
        Err("the Hop service did not start again (try: sc start HopAgent)".into())
    }
}

/// Wait until the service's state (`None`: not installed) satisfies `ok`.
fn wait_for(ok: impl Fn(Option<u32>) -> bool) -> Result<bool, String> {
    wait_until(|| Ok(ok(service_state()?)))
}

/// Poll `ok` until it holds or [`STATE_TIMEOUT`] passes.
fn wait_until(mut ok: impl FnMut() -> Result<bool, String>) -> Result<bool, String> {
    let start = std::time::Instant::now();
    while start.elapsed() < STATE_TIMEOUT {
        if ok()? {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(false)
}

pub fn uninstall() -> Result<(), String> {
    if !is_elevated() {
        return Err("run this from an administrator terminal".into());
    }
    stop_service()?;
    let mut left = Vec::new();
    if !delete_service()? {
        left.push("the service entry (Windows removes it once the Services window is closed, or after a restart)".to_string());
    }
    if !set_firewall_rule(None) {
        left.push(format!("the firewall rule \"{DISPLAY_NAME}\""));
    }
    let exe = installed_exe();
    if std::env::current_exe().ok().as_deref() == Some(exe.as_path()) {
        println!("{} is running this command; delete it afterwards.", exe.display());
    } else {
        match std::fs::remove_file(&exe) {
            Ok(()) => {
                let _ = std::fs::remove_dir(exe.parent().unwrap()); // only if now empty
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => left.push(format!("{}: {e}", exe.display())),
        }
    }
    if !left.is_empty() {
        return Err(format!("the service is stopped, but some parts remain: {}", left.join("; ")));
    }
    println!("Removed the service. Keys and pairing are kept in {}.", system_data_dir().display());
    Ok(())
}

// ---- the service process -------------------------------------------------------------------

static STATUS: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
static STOP: AtomicBool = AtomicBool::new(false);

fn set_status(state: u32) {
    let s = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 },
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: if state == SERVICE_STOP_PENDING { 5000 } else { 0 },
    };
    unsafe {
        SetServiceStatus(STATUS.load(Ordering::SeqCst) as SERVICE_STATUS_HANDLE, &s);
    }
}

unsafe extern "system" fn handler(control: u32, _: u32, _: *mut core::ffi::c_void, _: *mut core::ffi::c_void) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            STOP.store(true, Ordering::SeqCst);
            set_status(SERVICE_STOP_PENDING);
            0
        }
        SERVICE_CONTROL_INTERROGATE => 0,
        _ => 120, // ERROR_CALL_NOT_IMPLEMENTED
    }
}

/// Start the helper in `session` with this service's SYSTEM token. Returns the process and its
/// stop event: a named event with a random name that only SYSTEM can open.
fn launch_helper(session: u32) -> Result<(HANDLE, HANDLE), String> {
    unsafe {
        let (stop, stop_name) = new_stop_event().ok_or("could not create a stop event")?;
        let fail = |h: HANDLE, what: &str| {
            let e = format!("{what}: {}", std::io::Error::last_os_error());
            CloseHandle(h);
            Err(e)
        };
        let mut tok: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut tok) == 0 {
            return fail(stop, "OpenProcessToken");
        }
        let mut dup: HANDLE = std::ptr::null_mut();
        let ok = DuplicateTokenEx(tok, TOKEN_ALL_ACCESS, std::ptr::null(), SecurityImpersonation, TokenPrimary, &mut dup);
        CloseHandle(tok);
        if ok == 0 {
            return fail(stop, "DuplicateTokenEx");
        }
        if SetTokenInformation(dup, TokenSessionId, &session as *const u32 as *const _, 4) == 0 {
            let e = fail(stop, "SetTokenInformation");
            CloseHandle(dup);
            return e;
        }
        let mut desktop = wide("winsta0\\default");
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        si.lpDesktop = desktop.as_mut_ptr();
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let exe = std::env::current_exe().unwrap_or_else(|_| installed_exe());
        let mut cmd = wide(&format!("\"{}\" {HELPER_COMMAND} {STOP_EVENT_FLAG} {stop_name}", exe.display()));
        let ok = CreateProcessAsUserW(
            dup,
            std::ptr::null(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0, // no handle inheritance (not allowed across sessions)
            CREATE_NO_WINDOW,
            std::ptr::null(),
            std::ptr::null(),
            &si,
            &mut pi,
        );
        if ok == 0 {
            let e = fail(stop, "CreateProcessAsUserW");
            CloseHandle(dup);
            return e;
        }
        CloseHandle(dup);
        CloseHandle(pi.hThread);
        Ok((pi.hProcess, stop))
    }
}

/// A stop event with a random name (nobody can create it first) that only SYSTEM can open.
fn new_stop_event() -> Option<(HANDLE, String)> {
    unsafe {
        let sddl = wide(EVENT_SDDL);
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) == 0 {
            return None;
        }
        let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
        let mut result = None;
        for _ in 0..4 {
            let name = format!("{STOP_EVENT_PREFIX}{:032x}", hop_core::crypto::random_u128());
            let w = wide(&name);
            let h = CreateEventW(&sa, 1, 0, w.as_ptr());
            if h.is_null() {
                continue;
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(h); // someone got there first: never use their object
                continue;
            }
            result = Some((h, name));
            break;
        }
        LocalFree(sd as _);
        result
    }
}

/// The helper's exit code, or `None` while it runs.
fn exit_code(p: HANDLE) -> Option<u32> {
    let mut code = 0u32;
    let ok = unsafe { GetExitCodeProcess(p, &mut code) } != 0;
    (!ok || code != windows_sys::Win32::Foundation::STILL_ACTIVE as u32).then_some(code)
}

/// Ask the helper to release everything and exit; force it after a moment.
fn stop_helper(p: HANDLE, ev: HANDLE) {
    unsafe {
        SetEvent(ev);
        if WaitForSingleObject(p, 2000) != WAIT_OBJECT_0 {
            // Termination is asynchronous: wait until it has really happened.
            if TerminateProcess(p, 1) == 0 || WaitForSingleObject(p, 5000) != WAIT_OBJECT_0 {
                log_line("the helper did not exit when asked or when terminated");
            }
        }
        CloseHandle(p);
        CloseHandle(ev);
    }
}

/// The helper the service keeps in the console session.
struct Helper {
    process: HANDLE,
    stop: HANDLE,
    session: u32,
    started: std::time::Instant,
}


unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    let name = wide(SERVICE_NAME);
    let h = RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(handler), std::ptr::null());
    STATUS.store(h as *mut _, Ordering::SeqCst);
    set_status(SERVICE_RUNNING);
    // The helper needs its protected folder and refuses to create one; the installer made it,
    // but if it has gone, make it again here, the same protected way.
    let data = system_data_dir();
    if !data.exists() {
        // If this fails there is nowhere to log it; the helper keeps refusing, with backoff.
        let _ = create_private_dir(&data);
    }
    let mut child: Option<Helper> = None;
    let mut backoff = Backoff::new();
    let mut next_launch = std::time::Instant::now();
    while !STOP.load(Ordering::SeqCst) {
        let session = WTSGetActiveConsoleSessionId();
        if let Some(c) = &child {
            let ran = c.started.elapsed();
            if let Some(code) = exit_code(c.process) {
                let wait = backoff.failed_after(ran);
                log_line(&format!("helper exited with code {code} after {}s (see agent.log); next try in {}s", ran.as_secs(), wait.as_secs()));
                next_launch = std::time::Instant::now() + wait;
                let c = child.take().unwrap();
                CloseHandle(c.process);
                CloseHandle(c.stop);
            } else if c.session != session {
                backoff.stopped_after(ran);
                let c = child.take().unwrap();
                stop_helper(c.process, c.stop);
                next_launch = std::time::Instant::now();
            }
        }
        if child.is_none() && session != 0xFFFF_FFFF && std::time::Instant::now() >= next_launch {
            match launch_helper(session) {
                Ok((process, stop)) => {
                    log_line(&format!("helper started in session {session}"));
                    child = Some(Helper { process, stop, session, started: std::time::Instant::now() });
                }
                Err(e) => {
                    let wait = backoff.failed_after(Duration::ZERO);
                    log_line(&format!("starting the helper in session {session} failed: {e}; next try in {}s", wait.as_secs()));
                    next_launch = std::time::Instant::now() + wait;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
    if let Some(c) = child.take() {
        stop_helper(c.process, c.stop);
    }
    set_status(SERVICE_STOPPED);
}

/// Entry point when the Service Control Manager starts `hop-agent service`.
pub fn run_service() {
    let mut name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW { lpServiceName: std::ptr::null_mut(), lpServiceProc: None },
    ];
    unsafe {
        if StartServiceCtrlDispatcherW(table.as_ptr()) == 0 {
            eprintln!("`hop-agent service` is started by Windows; use `hop-agent install` instead.");
            std::process::exit(1);
        }
    }
}
