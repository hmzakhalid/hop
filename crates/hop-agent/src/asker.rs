//! Who answers when a Mac asks to pair: the person at the terminal (the console agent), or an
//! administrator signed in at the PC (the service's helper), in a Windows prompt.

use crate::role::Role;
use hop_core::agent::Asker;
use hop_core::ask::{Answer, ANSWER_TIME};
use std::sync::{mpsc, Arc, Mutex};

/// The asker for `role`; `None` where nobody could answer.
pub fn for_role(role: Role) -> Option<Arc<dyn Asker>> {
    match role {
        Role::Console => Some(Arc::new(Terminal::new())),
        #[cfg(windows)]
        Role::Service => Some(Arc::new(session::SignedIn)),
        #[cfg(not(windows))]
        Role::Service => None,
    }
}

/// The question, the same wherever it is asked.
fn question(mac: &str, check: &str, replaces: Option<&str>) -> String {
    let mut q = format!("\u{201C}{mac}\u{201D} wants to use this PC's keyboard and mouse.\n\nAllow it only if your Mac shows the same number:   {check}");
    if let Some(old) = replaces {
        q += &format!("\n\nThis replaces \u{201C}{old}\u{201D}, which will stop working here.");
    }
    q
}

/// The console agent asks in its terminal.
struct Terminal {
    lines: Mutex<mpsc::Receiver<String>>,
}

impl Terminal {
    fn new() -> Terminal {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            while std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
                if tx.send(std::mem::take(&mut line)).is_err() {
                    break;
                }
            }
        });
        Terminal { lines: Mutex::new(rx) }
    }
}

impl Asker for Terminal {
    fn ask(&self, mac: &str, check: &str, replaces: Option<&str>) -> Answer {
        let lines = self.lines.lock().unwrap();
        while lines.try_recv().is_ok() {} // nothing typed before the question counts
        println!("\n{}\nAllow? Type y and press Enter (anything else says no): ", question(mac, check, replaces));
        match lines.recv_timeout(ANSWER_TIME) {
            Ok(l) if matches!(l.trim().to_ascii_lowercase().as_str(), "y" | "yes") => Answer::Allowed,
            Ok(_) => Answer::Declined,
            Err(_) => Answer::NoAnswer,
        }
    }
}

#[cfg(windows)]
mod session {
    use super::*;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{CreateWellKnownSid, EqualSid, GetTokenInformation, TokenGroups, WinBuiltinAdministratorsSid, TOKEN_GROUPS};
    use windows_sys::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken, WTSSendMessageW, WTS_CURRENT_SERVER_HANDLE};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{IDNO, IDYES, MB_DEFBUTTON2, MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO};

    /// Asks whoever is signed in to the helper's session, if they are an administrator: a Mac
    /// paired with the service can type on the lock screen and into UAC prompts, so allowing one
    /// needs the same rights as a code shown beside the service.
    pub struct SignedIn;

    impl Asker for SignedIn {
        fn ask(&self, mac: &str, check: &str, replaces: Option<&str>) -> Answer {
            let mut session = 0u32;
            if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 {
                return Answer::NoAnswer;
            }
            match signed_in_admin(session) {
                Some(true) => {}
                Some(false) => return Answer::NeedsAdmin,
                None => return Answer::NoAnswer, // nobody signed in
            }
            let title: Vec<u16> = "Hop: allow a Mac?".encode_utf16().collect();
            let text: Vec<u16> = question(mac, check, replaces).encode_utf16().collect();
            let mut response = 0;
            // Lengths are in bytes. Waits for the answer or the timeout (seconds).
            let ok = unsafe {
                WTSSendMessageW(
                    WTS_CURRENT_SERVER_HANDLE,
                    session,
                    title.as_ptr(),
                    (title.len() * 2) as u32,
                    text.as_ptr(),
                    (text.len() * 2) as u32,
                    MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2 | MB_TOPMOST | MB_SETFOREGROUND,
                    ANSWER_TIME.as_secs() as u32,
                    &mut response,
                    1,
                )
            };
            match (ok != 0, response) {
                (true, IDYES) => Answer::Allowed,
                (true, IDNO) => Answer::Declined,
                _ => Answer::NoAnswer,
            }
        }
    }

    /// Whether the user signed in to `session` is an administrator; `None` if nobody is.
    fn signed_in_admin(session: u32) -> Option<bool> {
        let mut token: HANDLE = std::ptr::null_mut();
        if unsafe { WTSQueryUserToken(session, &mut token) } == 0 {
            return None;
        }
        let admin = token_has_admins(token);
        unsafe { CloseHandle(token) };
        Some(admin)
    }

    /// Whether `token` lists the Administrators group. With UAC an administrator's everyday token
    /// lists it for deny only; it still counts, as it does for a UAC prompt.
    pub(crate) fn token_has_admins(token: HANDLE) -> bool {
        unsafe {
            let mut admins = [0u8; 68]; // SECURITY_MAX_SID_SIZE
            let mut admins_len = admins.len() as u32;
            if CreateWellKnownSid(WinBuiltinAdministratorsSid, std::ptr::null_mut(), admins.as_mut_ptr() as _, &mut admins_len) == 0 {
                return false;
            }
            let mut len = 0u32;
            GetTokenInformation(token, TokenGroups, std::ptr::null_mut(), 0, &mut len);
            if len == 0 {
                return false;
            }
            // u64s, so the buffer is aligned for TOKEN_GROUPS.
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            if GetTokenInformation(token, TokenGroups, buf.as_mut_ptr() as _, len, &mut len) == 0 {
                return false;
            }
            let groups = &*(buf.as_ptr() as *const TOKEN_GROUPS);
            let all = std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize);
            all.iter().any(|g| EqualSid(g.Sid, admins.as_mut_ptr() as _) != 0)
        }
    }

    #[cfg(test)]
    mod tests {
        use windows_sys::Win32::Security::TOKEN_QUERY;
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        /// Run by hand on a Windows PC (`--ignored`): prints whether this user counts as an
        /// administrator, which should match Settings → Accounts.
        #[test]
        #[ignore]
        fn reads_this_users_token() {
            let mut token = std::ptr::null_mut();
            assert_ne!(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }, 0);
            println!("administrator: {}", super::token_has_admins(token));
        }
    }
}
