//! Open at login, through `SMAppService`. Works only for an app bundle.

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::msg_send;
use objc2_foundation::NSError;

#[link(name = "ServiceManagement", kind = "framework")]
extern "C" {}

/// `SMAppServiceStatus`
const ENABLED: isize = 1;
const REQUIRES_APPROVAL: isize = 2;

fn main_app_service() -> Option<Retained<AnyObject>> {
    let cls = AnyClass::get(c"SMAppService")?;
    unsafe { msg_send![cls, mainAppService] }
}

/// Whether Hop opens at login (or is waiting for the user's approval in System Settings).
pub fn enabled() -> bool {
    main_app_service().is_some_and(|s| {
        let status: isize = unsafe { msg_send![&*s, status] };
        status == ENABLED || status == REQUIRES_APPROVAL
    })
}

pub fn set(on: bool) -> Result<(), String> {
    let s = main_app_service().ok_or("macOS did not let Hop manage its login item")?;
    let r: Result<(), Retained<NSError>> =
        unsafe { if on { msg_send![&*s, registerAndReturnError: _] } else { msg_send![&*s, unregisterAndReturnError: _] } };
    r.map_err(|e| e.localizedDescription().to_string())
}
