//! The screen-view window. HEVC or H.264 frames from the PC go straight to an
//! `AVSampleBufferDisplayLayer`, marked display-immediately, so the Mac's hardware decodes and
//! shows each one as it arrives with no player buffering.
//!
//! The PC cursor is not in the captured image (Desktop Duplication leaves it out). The window
//! draws it as an overlay that moves on its own updates, so it never waits for a video frame.

use hop_core::h264;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSBackingStoreType, NSCursor, NSWindow, NSWindowDelegate, NSWindowStyleMask};
use objc2_av_foundation::{
    AVLayerVideoGravityResizeAspect, AVQueuedSampleBufferRendering, AVQueuedSampleBufferRenderingStatus, AVSampleBufferDisplayLayer,
    AVSampleBufferVideoRenderer,
};
use objc2_core_foundation::{kCFBooleanTrue, CFDictionary, CFRetained};
use objc2_core_graphics::CGColor;
use objc2_core_media::{
    kCMSampleAttachmentKey_DisplayImmediately, CMBlockBuffer, CMFormatDescription, CMSampleBuffer,
    CMVideoFormatDescriptionCreateFromH264ParameterSets, CMVideoFormatDescriptionCreateFromHEVCParameterSets,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::CALayer;
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr::NonNull;

/// What the caller should do after pushing a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    Shown,
    /// The decoder cannot continue from here (no parameter sets yet, or it failed): it needs a
    /// keyframe.
    NeedKeyframe,
}

/// Feeds frames to the display layer. Lives on the receiving thread.
pub struct Decoder {
    renderer: Retained<AVSampleBufferVideoRenderer>,
    format: Option<CFRetained<CMFormatDescription>>,
    /// The parameter sets `format` was made from (VPS for HEVC, SPS, PPS).
    params: Vec<Vec<u8>>,
}

// AVSampleBufferDisplayLayer accepts samples from any thread (that is its documented use).
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn push(&mut self, annexb: &[u8], hevc: bool) -> Pushed {
        let f = h264::to_length_prefixed(annexb, hevc);
        if let Some(params) = parameter_sets(&f, hevc) {
            if params != self.params || self.format.is_none() {
                self.format = make_format(&params, hevc);
                self.params = params;
            }
        }
        let Some(format) = self.format.as_ref() else { return Pushed::NeedKeyframe };
        if f.avcc.is_empty() {
            return Pushed::Shown;
        }
        unsafe {
            if self.renderer.status() == AVQueuedSampleBufferRenderingStatus::Failed {
                self.renderer.flush();
                return Pushed::NeedKeyframe;
            }
            let Some(sample) = make_sample(&f.avcc, format) else { return Pushed::NeedKeyframe };
            self.renderer.enqueueSampleBuffer(&sample);
        }
        Pushed::Shown
    }

    /// Forget the decoder state after loss, until the next keyframe.
    pub fn reset(&mut self) {
        unsafe { self.renderer.flush() };
    }
}

/// A keyframe's parameter sets, in the order the format wants them; `None` if any is missing.
fn parameter_sets(f: &h264::AvccFrame, hevc: bool) -> Option<Vec<Vec<u8>>> {
    let mut v = Vec::with_capacity(3);
    if hevc {
        v.push(f.vps.clone()?);
    }
    v.push(f.sps.clone()?);
    v.push(f.pps.clone()?);
    Some(v)
}

fn make_format(params: &[Vec<u8>], hevc: bool) -> Option<CFRetained<CMFormatDescription>> {
    if params.iter().any(|p| p.is_empty()) {
        return None;
    }
    let ptrs: Vec<NonNull<u8>> = params.iter().map(|p| NonNull::from(&p[0])).collect();
    let sizes: Vec<usize> = params.iter().map(|p| p.len()).collect();
    let mut out: *const CMFormatDescription = std::ptr::null();
    let (ptrs_at, sizes_at, out_at) = (NonNull::from(&ptrs[0]), NonNull::from(&sizes[0]), NonNull::from(&mut out));
    let status = unsafe {
        if hevc {
            CMVideoFormatDescriptionCreateFromHEVCParameterSets(None, params.len(), ptrs_at, sizes_at, 4, None, out_at)
        } else {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(None, params.len(), ptrs_at, sizes_at, 4, out_at)
        }
    };
    if status != 0 || out.is_null() {
        return None;
    }
    Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(out as *mut CMFormatDescription)) })
}

fn make_sample(avcc: &[u8], format: &CMFormatDescription) -> Option<CFRetained<CMSampleBuffer>> {
    unsafe {
        let mut bb: *mut CMBlockBuffer = std::ptr::null_mut();
        let s = CMBlockBuffer::create_with_memory_block(
            None,
            std::ptr::null_mut(),
            avcc.len(),
            None,
            std::ptr::null(),
            0,
            avcc.len(),
            1, // kCMBlockBufferAssureMemoryNowFlag
            NonNull::from(&mut bb),
        );
        if s != 0 || bb.is_null() {
            return None;
        }
        let bb = CFRetained::from_raw(NonNull::new_unchecked(bb));
        if CMBlockBuffer::replace_data_bytes(NonNull::new_unchecked(avcc.as_ptr() as *mut c_void), &bb, 0, avcc.len()) != 0 {
            return None;
        }
        let size = avcc.len();
        let mut sb: *mut CMSampleBuffer = std::ptr::null_mut();
        let s = CMSampleBuffer::create_ready(None, Some(&bb), Some(format), 1, 0, std::ptr::null(), 1, &size, NonNull::from(&mut sb));
        if s != 0 || sb.is_null() {
            return None;
        }
        let sb = CFRetained::from_raw(NonNull::new_unchecked(sb));
        if let Some(arr) = sb.sample_attachments_array(true) {
            let dict = arr.value_at_index(0) as *const CFDictionary;
            if !dict.is_null() {
                if let Some(t) = kCFBooleanTrue {
                    CFDictionarySetValue(dict, kCMSampleAttachmentKey_DisplayImmediately as *const _ as *const c_void, t as *const _ as *const c_void);
                }
            }
        }
        Some(sb)
    }
}

extern "C" {
    fn CFDictionarySetValue(dict: *const CFDictionary, key: *const c_void, value: *const c_void);
    fn CMVideoFormatDescriptionGetDimensions(desc: *const CMFormatDescription) -> Dims;
}

#[repr(C)]
struct Dims {
    width: i32,
    height: i32,
}

/// Picture size from a keyframe's parameter sets.
pub fn dimensions(annexb: &[u8], hevc: bool) -> Option<(u32, u32)> {
    let f = h264::to_length_prefixed(annexb, hevc);
    let fmt = make_format(&parameter_sets(&f, hevc)?, hevc)?;
    let d = unsafe { CMVideoFormatDescriptionGetDimensions(&*fmt) };
    Some((d.width as u32, d.height as u32))
}

struct CloseIvars {
    on_close: RefCell<Option<Box<dyn FnOnce()>>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `CloseWatcher` does not implement Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = CloseIvars]
    struct CloseWatcher;

    unsafe impl NSObjectProtocol for CloseWatcher {}

    unsafe impl NSWindowDelegate for CloseWatcher {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _: &NSNotification) {
            if let Some(f) = self.ivars().on_close.borrow_mut().take() {
                f();
            }
        }
    }
);

/// The window and its layers. Main thread only.
pub struct Window {
    window: Retained<NSWindow>,
    video: Retained<AVSampleBufferDisplayLayer>,
    cursor: Retained<CALayer>,
    size: Cell<(f64, f64)>,
    /// The window's delegate, which it holds only weakly.
    _watcher: Retained<CloseWatcher>,
}

impl Window {
    /// Open a window for a `width`×`height` PC screen, sized to fit the Mac's screen. `on_close`
    /// runs once, when the user closes it (not when it is minimised).
    pub fn open(mtm: MainThreadMarker, title: &str, width: u32, height: u32, on_close: impl FnOnce() + 'static) -> (Window, Decoder) {
        let (w, h) = (width.max(2) as f64, height.max(2) as f64);
        let scale = (1280.0 / w).min(800.0 / h).min(1.0);
        let rect = NSRect::new(NSPoint::new(120.0, 120.0), NSSize::new(w * scale, h * scale));
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable
                    | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        let watcher = CloseWatcher::alloc(mtm).set_ivars(CloseIvars { on_close: RefCell::new(Some(Box::new(on_close))) });
        let watcher: Retained<CloseWatcher> = unsafe { msg_send![super(watcher), init] };
        window.setDelegate(Some(ProtocolObject::from_ref(&*watcher)));
        window.setTitle(&NSString::from_str(title));
        window.setContentAspectRatio(NSSize::new(w, h));
        let video = unsafe { AVSampleBufferDisplayLayer::new() };
        unsafe {
            if let Some(g) = AVLayerVideoGravityResizeAspect {
                video.setVideoGravity(g);
            }
        }
        let black = CGColor::new_srgb(0.0, 0.0, 0.0, 1.0);
        video.setBackgroundColor(Some(&black));
        // The Mac's own arrow, tip on the PC pointer's position.
        let cursor = CALayer::new();
        let arrow = NSCursor::arrowCursor();
        let image = arrow.image();
        let (size, hot) = (image.size(), arrow.hotSpot());
        let contents: &AnyObject = image.as_ref();
        unsafe { cursor.setContents(Some(contents)) };
        cursor.setContentsScale(window.backingScaleFactor());
        cursor.setBounds(objc2_core_foundation::CGRect::new(
            objc2_core_foundation::CGPoint::new(0.0, 0.0),
            objc2_core_foundation::CGSize::new(size.width, size.height),
        ));
        // The hot spot is measured from the image's top left; the layer's anchor from its bottom left.
        cursor.setAnchorPoint(objc2_core_foundation::CGPoint::new(hot.x / size.width.max(1.0), 1.0 - hot.y / size.height.max(1.0)));
        cursor.setHidden(true);
        cursor.setZPosition(10.0);
        video.addSublayer(&cursor);
        if let Some(view) = window.contentView() {
            view.setLayer(Some(&video));
            view.setWantsLayer(true);
        }
        window.center();
        window.makeKeyAndOrderFront(None);
        NSApplication::sharedApplication(mtm).activate();
        let renderer = unsafe { video.sampleBufferRenderer() };
        let decoder = Decoder { renderer, format: None, params: Vec::new() };
        (Window { window, video, cursor, size: Cell::new((w, h)), _watcher: watcher }, decoder)
    }

    pub fn set_title(&self, title: &str) {
        self.window.setTitle(&NSString::from_str(title));
    }

    /// The PC's picture size changed (or became known): keep the window's shape.
    pub fn set_picture_size(&self, width: u32, height: u32) {
        let (w, h) = (width.max(2) as f64, height.max(2) as f64);
        if self.size.get() == (w, h) {
            return;
        }
        self.size.set((w, h));
        self.window.setContentAspectRatio(NSSize::new(w, h));
        let frame = self.window.frame();
        let width_now = frame.size.width;
        self.window.setContentSize(NSSize::new(width_now, width_now * h / w));
    }

    /// Show the PC cursor at PC-screen pixel `p` (relative to the streamed monitor), or hide it.
    pub fn set_cursor(&self, p: Option<(f64, f64)>) {
        let Some((x, y)) = p else {
            self.cursor.setHidden(true);
            return;
        };
        let (pw, ph) = self.size.get();
        if x < 0.0 || y < 0.0 || x >= pw || y >= ph {
            self.cursor.setHidden(true); // on another monitor
            return;
        }
        let b = self.video.bounds();
        let s = (b.size.width / pw).min(b.size.height / ph);
        let ox = (b.size.width - pw * s) / 2.0;
        let oy = (b.size.height - ph * s) / 2.0;
        // Layer coordinates are bottom-up.
        let pos = objc2_core_foundation::CGPoint::new(ox + x * s, b.size.height - (oy + y * s));
        objc2_quartz_core::CATransaction::begin();
        objc2_quartz_core::CATransaction::setDisableActions(true);
        self.cursor.setPosition(pos);
        self.cursor.setHidden(false);
        objc2_quartz_core::CATransaction::commit();
    }
}
