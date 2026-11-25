//! The operating system under the agent: input injection, Ctrl+C and service stop signals,
//! resource usage, and helper processes that end with the agent.

use hop_core::agent::Platform;

#[derive(Default)]
pub struct Usage {
    pub memory_mb: f64,
    pub private_mb: f64,
    pub cpu_seconds: f64,
}

/// Real input injection. `follow`: switch to whichever desktop has input (the SYSTEM helper).
/// `swallow`: catch our own input before apps see it, and print it.
#[cfg(windows)]
pub fn injector(follow: bool, swallow: bool) -> Result<Box<dyn Platform>, String> {
    let p = if follow { crate::win::WinPlatform::following() } else { crate::win::WinPlatform::new() };
    if !swallow {
        return Ok(Box::new(p));
    }
    let hooks = crate::hooks::Hooks::start(true).map_err(|e| format!("{e}; refusing to run in swallow mode"))?;
    Ok(Box::new(p.caught_by(hooks)))
}

#[cfg(not(windows))]
pub fn injector(_: bool, _: bool) -> Result<Box<dyn Platform>, String> {
    Err("injecting input needs Windows; use --dry-run here".into())
}

#[cfg(windows)]
pub fn on_ctrl_c(stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::Ordering;
    use std::sync::OnceLock;
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    static STOP: OnceLock<std::sync::Arc<std::sync::atomic::AtomicBool>> = OnceLock::new();
    let _ = STOP.set(stop);
    unsafe extern "system" fn handler(_: u32) -> windows_sys::core::BOOL {
        if let Some(s) = STOP.get() {
            s.store(true, Ordering::Relaxed);
        }
        // Windows ends the process as soon as this returns: give the loop time to release keys.
        std::thread::sleep(std::time::Duration::from_millis(150));
        1
    }
    unsafe {
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}

#[cfg(not(windows))]
pub fn on_ctrl_c(_: std::sync::Arc<std::sync::atomic::AtomicBool>) {}

/// Every service helper's stop event is this plus a random suffix (see `service.rs`).
#[cfg(windows)]
pub const STOP_EVENT_PREFIX: &str = "Global\\HopAgentStop-";

/// In the service helper: stop when the service signals our event. Fails if the event cannot
/// be opened, so a helper that could only be terminated never starts.
#[cfg(windows)]
pub fn on_service_stop(stop: std::sync::Arc<std::sync::atomic::AtomicBool>, name: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{OpenEventW, WaitForSingleObject, INFINITE};
    if !name.starts_with(STOP_EVENT_PREFIX) {
        return Err(format!("{name:?} is not a Hop stop event"));
    }
    let w: Vec<u16> = std::ffi::OsStr::new(name).encode_wide().chain(std::iter::once(0)).collect();
    let ev = unsafe { OpenEventW(SYNCHRONIZE, 0, w.as_ptr()) };
    if ev.is_null() {
        return Err(format!("cannot open the stop event: {}", std::io::Error::last_os_error()));
    }
    let ev = ev as usize; // a handle is just a number to the waiting thread
    std::thread::spawn(move || unsafe {
        let ev = ev as windows_sys::Win32::Foundation::HANDLE;
        let signalled = WaitForSingleObject(ev, INFINITE) == WAIT_OBJECT_0;
        CloseHandle(ev);
        if signalled {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });
    Ok(())
}

#[cfg(not(windows))]
pub fn on_service_stop(_: std::sync::Arc<std::sync::atomic::AtomicBool>, _: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
pub fn usage() -> Usage {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    unsafe {
        let mut m: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        m.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        K32GetProcessMemoryInfo(GetCurrentProcess(), &mut m, m.cb);
        let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut c, mut e, mut k, mut u) = (z, z, z, z);
        GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
        let secs = |f: FILETIME| (((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64) as f64 / 1e7;
        Usage {
            memory_mb: m.WorkingSetSize as f64 / 1_048_576.0,
            private_mb: m.PagefileUsage as f64 / 1_048_576.0,
            cpu_seconds: secs(k) + secs(u),
        }
    }
}

#[cfg(not(windows))]
pub fn usage() -> Usage {
    Usage::default()
}

// ---- helper processes ----------------------------------------------------------------------

/// Spawn a child that dies with this process (a job object that kills its members on close).
#[cfg(windows)]
pub fn spawn_tied(cmd: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::sync::OnceLock;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
    static JOB: OnceLock<usize> = OnceLock::new();
    let job = *JOB.get_or_init(|| unsafe {
        let j = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
        SetInformationJobObject(j, JobObjectExtendedLimitInformation, &info as *const _ as *const _, size);
        j as usize // never closed: closing it is what kills the children, at our exit
    });
    let child = cmd.creation_flags(CREATE_NO_WINDOW).spawn()?;
    unsafe { AssignProcessToJobObject(job as _, child.as_raw_handle() as _) };
    Ok(child)
}

#[cfg(not(windows))]
pub fn spawn_tied(cmd: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    cmd.spawn() // the child watches for its parent's exit (see `stop_with_parent`)
}

/// In the video process: stop when the agent that started it is gone.
pub fn stop_with_parent(stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    #[cfg(unix)]
    std::thread::spawn(move || {
        let parent = std::os::unix::process::parent_id();
        while std::os::unix::process::parent_id() == parent {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    #[cfg(windows)]
    let _ = stop; // the job object ends us with the agent
}
