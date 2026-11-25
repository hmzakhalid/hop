//! Updates through Sparkle 2, when the app bundle ships it. Packaging embeds
//! `Contents/Frameworks/Sparkle.framework` and puts the feed URL and the EdDSA public key in
//! `Info.plist` (`SUFeedURL`, `SUPublicEDKey`). Development builds have neither and run without.

use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::msg_send;
use objc2_foundation::{NSBundle, NSString};

/// Sparkle's `SPUStandardUpdaterController`, which checks on a schedule by itself. The menu's
/// "Check for Updates…" item targets it with `checkForUpdates:`.
pub fn start() -> Option<Retained<AnyObject>> {
    let main = NSBundle::mainBundle();
    main.objectForInfoDictionaryKey(&NSString::from_str("SUFeedURL"))?;
    let path = main.privateFrameworksPath()?.to_string() + "/Sparkle.framework";
    let sparkle = NSBundle::bundleWithPath(&NSString::from_str(&path))?;
    // SAFETY: loading a framework the bundle ships and the packaging step signed.
    if !unsafe { sparkle.load() } {
        return None;
    }
    let cls = AnyClass::get(c"SPUStandardUpdaterController")?;
    unsafe {
        let alloc: Allocated<AnyObject> = msg_send![cls, alloc];
        let none: Option<&AnyObject> = None;
        msg_send![alloc, initWithStartingUpdater: true, updaterDelegate: none, userDriverDelegate: none]
    }
}
