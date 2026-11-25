//! Talking to the Windows Service Control Manager about the Hop service. Shared by the agent
//! (install, uninstall, pairing) and the tray app (status).

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Foundation::{GetLastError, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_MARKED_FOR_DELETE};
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SC_HANDLE, SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO,
    SERVICE_QUERY_STATUS, SERVICE_STATUS_PROCESS, SERVICE_STOPPED,
};

pub const SERVICE_NAME: &str = "HopAgent";

pub fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// The service's state (`SERVICE_RUNNING`, `SERVICE_STOPPED`, …); `Ok(None)` only when it is
/// not installed. Any other failure is an error, never "not installed".
pub fn service_state() -> Result<Option<u32>, String> {
    let (svc, _scm) = match open_service(SERVICE_QUERY_STATUS) {
        Ok(handles) => handles,
        Err(Missing::NotInstalled) => return Ok(None),
        // Deleted once the last handle to it closes; stopped until then.
        Err(Missing::BeingDeleted) => return Ok(Some(SERVICE_STOPPED)),
        Err(Missing::Error(e)) => return Err(e),
    };
    unsafe {
        let mut st: SERVICE_STATUS_PROCESS = std::mem::zeroed();
        let mut needed = 0u32;
        let size = size_of::<SERVICE_STATUS_PROCESS>() as u32;
        if QueryServiceStatusEx(svc.0, SC_STATUS_PROCESS_INFO, &mut st as *mut _ as *mut u8, size, &mut needed) == 0 {
            return Err(format!("cannot query the Hop service: {}", std::io::Error::last_os_error()));
        }
        Ok(Some(st.dwCurrentState))
    }
}

/// A service-manager handle, closed on drop.
pub struct ScHandle(pub SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        unsafe { CloseServiceHandle(self.0) };
    }
}

/// Why the Hop service could not be opened.
pub enum Missing {
    NotInstalled,
    /// Marked for deletion: it goes away once nothing has it open.
    BeingDeleted,
    Error(String),
}

/// Open the Hop service with `access`. Returns (service, manager); the service closes first.
pub fn open_service(access: u32) -> Result<(ScHandle, ScHandle), Missing> {
    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err(Missing::Error(format!("cannot open the service manager: {}", std::io::Error::last_os_error())));
        }
        let scm = ScHandle(scm);
        let name = wide(SERVICE_NAME);
        let svc = OpenServiceW(scm.0, name.as_ptr(), access);
        if svc.is_null() {
            return Err(match GetLastError() {
                ERROR_SERVICE_DOES_NOT_EXIST => Missing::NotInstalled,
                ERROR_SERVICE_MARKED_FOR_DELETE => Missing::BeingDeleted,
                code => Missing::Error(format!("cannot open the Hop service: {}", std::io::Error::from_raw_os_error(code as i32))),
            });
        }
        Ok((ScHandle(svc), scm))
    }
}

