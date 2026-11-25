//! Screen → H.264 on the GPU: DXGI Desktop Duplication captures a monitor, the D3D11 video
//! processor converts BGRA to NV12, and a Media Foundation encoder (NVENC/AMF/QuickSync when
//! present, Microsoft's software encoder otherwise) produces low-latency H.264 (Annex B).

use std::mem::ManuallyDrop;
use std::time::Instant;
use windows::core::{Interface, Result, GUID, PWSTR};
use windows::Win32::Foundation::{E_FAIL, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;

#[derive(Clone, Debug)]
pub struct Settings {
    /// Index into [`list_monitors`] (0 = primary).
    pub monitor: usize,
    pub fps: u32,
    pub bitrate: u32,
    /// Prefer a hardware encoder (falls back to software if none).
    pub hardware: bool,
    /// Capture whichever desktop has input (the lock screen, a UAC prompt). Only a SYSTEM
    /// process (the service's helper) may.
    pub follow_input_desktop: bool,
    /// Prefer HEVC (H.265): the same picture at about half the bitrate. Falls back to H.264
    /// when there is no hardware HEVC encoder.
    pub hevc: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { monitor: 0, fps: 60, bitrate: 20_000_000, hardware: true, follow_input_desktop: false, hevc: false }
    }
}

#[derive(Clone, Debug)]
pub struct EncodedFrame {
    /// Annex B (start codes). Keyframes carry the parameter sets.
    pub data: Vec<u8>,
    pub keyframe: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Timing {
    pub convert_us: u32,
    pub encode_us: u32,
    /// When the image was grabbed (`hop_core::clock::now_us`).
    pub captured_us: u64,
}

#[derive(Clone, Debug)]
pub struct MonitorInfo {
    pub name: String,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

fn outputs() -> Result<Vec<(IDXGIAdapter1, IDXGIOutput, DXGI_OUTPUT_DESC)>> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut v = Vec::new();
        let mut ai = 0;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            let mut oi = 0;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                let d = output.GetDesc()?;
                if d.AttachedToDesktop.as_bool() {
                    v.push((adapter.clone(), output, d));
                }
                oi += 1;
            }
            ai += 1;
        }
        v.sort_by_key(|(_, _, d)| !(d.DesktopCoordinates.left == 0 && d.DesktopCoordinates.top == 0));
        Ok(v)
    }
}

pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
    Ok(outputs()?
        .into_iter()
        .map(|(_, _, d)| {
            let r = d.DesktopCoordinates;
            let len = d.DeviceName.iter().position(|&c| c == 0).unwrap_or(d.DeviceName.len());
            MonitorInfo {
                name: String::from_utf16_lossy(&d.DeviceName[..len]),
                left: r.left,
                top: r.top,
                width: (r.right - r.left) as u32,
                height: (r.bottom - r.top) as u32,
            }
        })
        .collect())
}

struct Encoder {
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    codec: Option<ICodecAPI>,
    _manager: Option<IMFDXGIDeviceManager>,
    provides_samples: bool,
    out_size: u32,
    need_input: u32,
    have_output: u32,
    fps: u32,
    index: u64,
    name: String,
    /// Settings the encoder refused, reported with its name.
    refused: Vec<&'static str>,
    hevc: bool,
}

/// Apply one encoder setting; false if the encoder refused it (or has no settings at all).
fn set_codec(codec: &Option<ICodecAPI>, key: &GUID, v: VARIANT) -> bool {
    codec.as_ref().is_some_and(|c| unsafe { c.SetValue(key, &v).is_ok() })
}

impl Encoder {
    fn new(device: &ID3D11Device, w: u32, h: u32, s: &Settings) -> Result<Encoder> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
            let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
            // (codec, where to look) in order of preference: hardware HEVC when asked, hardware
            // H.264, software H.264. No software HEVC: far too slow for this.
            let mut tries = Vec::new();
            if s.hardware && s.hevc {
                tries.push((MFVideoFormat_HEVC, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER));
            }
            if s.hardware {
                tries.push((MFVideoFormat_H264, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER));
            }
            tries.push((MFVideoFormat_H264, MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER));
            let mut last_err = windows::core::Error::new(E_FAIL, "no video encoder");
            for (codec, flags) in tries {
                let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: codec };
                let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
                let mut n = 0u32;
                if MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &mut pp, &mut n).is_err() || n == 0 {
                    continue;
                }
                let acts = std::slice::from_raw_parts_mut(pp, n as usize);
                let mut made = None;
                for act in acts.iter().flatten() {
                    let mut name = PWSTR::null();
                    let mut len = 0u32;
                    let label = if act.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len).is_ok() {
                        let s = name.to_string().unwrap_or_default();
                        CoTaskMemFree(Some(name.0 as _));
                        s
                    } else {
                        "video encoder".into()
                    };
                    // Listed is not enough: an encoder can still refuse the size or the settings.
                    match act.ActivateObject::<IMFTransform>().and_then(|t| Encoder::configure(t, label, codec, device, w, h, s)) {
                        Ok(e) => {
                            made = Some(e);
                            break;
                        }
                        Err(e) => {
                            let _ = act.ShutdownObject();
                            last_err = e;
                        }
                    }
                }
                for a in acts.iter_mut() {
                    drop(a.take());
                }
                CoTaskMemFree(Some(pp as _));
                if let Some(e) = made {
                    return Ok(e);
                }
            }
            Err(last_err)
        }
    }

    /// Set up one activated encoder. An error means: try the next one.
    fn configure(transform: IMFTransform, name: String, subtype: windows::core::GUID, device: &ID3D11Device, w: u32, h: u32, s: &Settings) -> Result<Encoder> {
        unsafe {
            let hevc = subtype == MFVideoFormat_HEVC;
            let attrs = transform.GetAttributes().ok();
            let is_async = attrs.as_ref().and_then(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).ok()).unwrap_or(0) != 0;
            if is_async {
                if let Some(a) = &attrs {
                    a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
                }
            }
            let mut manager = None;
            let d3d_aware = attrs.as_ref().and_then(|a| a.GetUINT32(&MF_SA_D3D11_AWARE).ok()).unwrap_or(0) != 0;
            if d3d_aware {
                let mut token = 0u32;
                let mut m: Option<IMFDXGIDeviceManager> = None;
                MFCreateDXGIDeviceManager(&mut token, &mut m)?;
                let m = m.ok_or_else(|| windows::core::Error::new(E_FAIL, "no DXGI device manager"))?;
                m.ResetDevice(device, token)?;
                transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, m.as_raw() as usize)?;
                manager = Some(m);
            }
            let codec = transform.cast::<ICodecAPI>().ok();
            let configure = |codec: &Option<ICodecAPI>| -> Vec<&'static str> {
                let settings = [
                    (&CODECAPI_AVLowLatencyMode, VARIANT::from(true), "low latency"),
                    // 0 = fastest … 100 = best quality; screen content compresses well either way.
                    (&CODECAPI_AVEncCommonQualityVsSpeed, VARIANT::from(20u32), "speed preset"),
                    (&CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32), "CBR"),
                    (&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(s.bitrate), "bitrate"),
                    (&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32), "no B-frames"),
                    (&CODECAPI_AVEncMPVGOPSize, VARIANT::from(s.fps * 120), "GOP size"),
                ];
                settings.into_iter().filter(|(key, v, _)| !set_codec(codec, key, v.clone())).map(|(_, _, what)| what).collect()
            };
            // Some encoders only take settings before the media types are set, some only after.
            configure(&codec);

            let ot = MFCreateMediaType()?;
            ot.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            ot.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
            ot.SetUINT32(&MF_MT_AVG_BITRATE, s.bitrate)?;
            ot.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
            ot.SetUINT64(&MF_MT_FRAME_RATE, ((s.fps as u64) << 32) | 1)?;
            ot.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            ot.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            let profile = if hevc { eAVEncH265VProfile_Main_420_8.0 } else { eAVEncH264VProfile_Main.0 };
            ot.SetUINT32(&MF_MT_MPEG2_PROFILE, profile as u32)?;
            ot.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            ot.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            transform.SetOutputType(0, &ot, 0)?;

            let it = MFCreateMediaType()?;
            it.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            it.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            it.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
            it.SetUINT64(&MF_MT_FRAME_RATE, ((s.fps as u64) << 32) | 1)?;
            it.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            it.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(0, &it, 0)?;
            let refused = configure(&codec);

            let info = transform.GetOutputStreamInfo(0)?;
            let provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let events = if is_async { Some(transform.cast::<IMFMediaEventGenerator>()?) } else { None };
            let codec_name = if hevc { "HEVC" } else { "H.264" };
            let label = format!("{name}{}{}, {codec_name}", if is_async { " (async" } else { " (sync" }, if d3d_aware { ", GPU input)" } else { ", CPU input)" });
            Ok(Encoder {
                transform,
                events,
                codec,
                _manager: manager,
                provides_samples,
                out_size: info.cbSize.max(1 << 20),
                need_input: 0,
                have_output: 0,
                fps: s.fps,
                index: 0,
                name: label,
                refused,
                hevc,
            })
        }
    }

    fn pump_event(&mut self, wait: bool) -> Result<bool> {
        let Some(ev) = &self.events else { return Ok(false) };
        let flags = if wait { MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0) } else { MF_EVENT_FLAG_NO_WAIT };
        let e = match unsafe { ev.GetEvent(flags) } {
            Ok(e) => e,
            Err(e) if !wait && e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(false),
            Err(e) => return Err(e),
        };
        let t = unsafe { e.GetType()? };
        if t == METransformNeedInput.0 as u32 {
            self.need_input += 1;
        } else if t == METransformHaveOutput.0 as u32 {
            self.have_output += 1;
        }
        Ok(true)
    }

    fn output(&mut self) -> Result<Option<EncodedFrame>> {
        unsafe {
            loop {
                let mut buf = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(None),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                };
                if !self.provides_samples {
                    let s = MFCreateSample()?;
                    s.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
                    buf.pSample = ManuallyDrop::new(Some(s));
                }
                let mut status = 0u32;
                let mut bufs = [buf];
                let r = self.transform.ProcessOutput(0, &mut bufs, &mut status);
                let sample = ManuallyDrop::take(&mut bufs[0].pSample);
                drop(ManuallyDrop::take(&mut bufs[0].pEvents));
                match r {
                    Ok(()) => {}
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        let t = self.transform.GetOutputAvailableType(0, 0)?;
                        self.transform.SetOutputType(0, &t, 0)?;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
                let Some(sample) = sample else { return Ok(None) };
                let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
                let b = sample.ConvertToContiguousBuffer()?;
                let mut p = std::ptr::null_mut();
                let mut len = 0u32;
                b.Lock(&mut p, None, Some(&mut len))?;
                let data = std::slice::from_raw_parts(p, len as usize).to_vec();
                b.Unlock()?;
                return Ok(Some(EncodedFrame { data, keyframe }));
            }
        }
    }

    fn encode(&mut self, tex: &ID3D11Texture2D, force_key: bool) -> Result<Vec<EncodedFrame>> {
        unsafe {
            if force_key && !set_codec(&self.codec, &CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(1u32)) && !self.refused.contains(&"forced keyframes") {
                self.refused.push("forced keyframes");
            }
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                if let Ok(n) = b2.GetContiguousLength() {
                    let _ = buffer.SetCurrentLength(n);
                }
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            let dur = 10_000_000i64 / self.fps as i64;
            sample.SetSampleTime(self.index as i64 * dur)?;
            sample.SetSampleDuration(dur)?;
            self.index += 1;
            let mut out = Vec::new();
            if self.events.is_none() {
                self.transform.ProcessInput(0, &sample, 0)?;
                while let Some(f) = self.output()? {
                    out.push(f);
                }
                return Ok(out);
            }
            while self.need_input == 0 {
                self.pump_event(true)?;
                while self.have_output > 0 {
                    self.have_output -= 1;
                    if let Some(f) = self.output()? {
                        out.push(f);
                    }
                }
            }
            self.transform.ProcessInput(0, &sample, 0)?;
            self.need_input -= 1;
            // Low-latency mode returns this frame's output before asking for more input.
            let start = Instant::now();
            loop {
                while self.have_output > 0 {
                    self.have_output -= 1;
                    if let Some(f) = self.output()? {
                        out.push(f);
                    }
                }
                if !out.is_empty() || start.elapsed().as_millis() > 200 {
                    break;
                }
                self.pump_event(true)?;
            }
            while self.pump_event(false)? {}
            Ok(out)
        }
    }
}

pub struct Capturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    /// `None` only between losing it and duplicating again.
    dup: Option<IDXGIOutputDuplication>,
    bgra: ID3D11Texture2D,
    nv12: Vec<ID3D11Texture2D>,
    vctx: ID3D11VideoContext,
    vp: ID3D11VideoProcessor,
    vin: ID3D11VideoProcessorInputView,
    vout: Vec<ID3D11VideoProcessorOutputView>,
    enc: Encoder,
    have_image: bool,
    grabbed_us: u64,
    /// `grabbed_us` of the image last encoded.
    encoded_grab: u64,
    slot: usize,
    /// The pointer, in pixels of this monitor, while visible.
    pointer: Option<(i32, i32)>,
    follow_input_desktop: bool,
    pub width: u32,
    pub height: u32,
}

/// Move this thread onto whichever desktop has input. Duplication only works there.
/// Move this thread to whichever desktop has input. A failure is not fatal (the current
/// desktop can still be captured); it is reported if duplication then fails.
fn switch_to_input_desktop() -> Option<String> {
    use windows::Win32::Foundation::GENERIC_ALL;
    use windows::Win32::System::StationsAndDesktops::{OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS};
    unsafe {
        // The handle stays open while the thread uses the desktop; a switch leaks one handle.
        match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0)) {
            Ok(d) => SetThreadDesktop(d).err().map(|e| format!("cannot move to the input desktop: {e}")),
            Err(e) => Some(format!("cannot open the input desktop: {e}")),
        }
    }
}

/// Duplicate `output`, explaining a failure with why the desktop switch failed, if it did.
unsafe fn duplicate(output: &IDXGIOutput1, device: &ID3D11Device, switch_failed: Option<String>) -> Result<IDXGIOutputDuplication> {
    output.DuplicateOutput(device).map_err(|e| match switch_failed {
        Some(why) => windows::core::Error::new(e.code(), format!("{} ({why})", e.message())),
        None => e,
    })
}

impl Capturer {
    pub fn new(s: &Settings) -> Result<Capturer> {
        let switch_failed = if s.follow_input_desktop { switch_to_input_desktop() } else { None };
        unsafe {
            let outs = outputs()?;
            let (adapter, output, _) = outs
                .into_iter()
                .nth(s.monitor)
                .ok_or_else(|| windows::core::Error::new(E_FAIL, "no such monitor"))?;
            let mut device = None;
            let mut context = None;
            // The device must live on the adapter that owns this output.
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.unwrap();
            let context = context.unwrap();
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            // Capture and encode ahead of other GPU work, so a busy GPU does not delay frames.
            if let Ok(d) = device.cast::<IDXGIDevice>() {
                let _ = d.SetGPUThreadPriority(7);
            }
            let output: IDXGIOutput1 = output.cast()?;
            let dup = duplicate(&output, &device, switch_failed)?;
            let dd = dup.GetDesc();
            // Even sizes for 4:2:0.
            let width = dd.ModeDesc.Width & !1;
            let height = dd.ModeDesc.Height & !1;

            let tex = |format: DXGI_FORMAT, bind: u32| -> Result<ID3D11Texture2D> {
                let d = D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: format,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: bind,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut t = None;
                device.CreateTexture2D(&d, None, Some(&mut t))?;
                Ok(t.unwrap())
            };
            let bgra = tex(DXGI_FORMAT_B8G8R8A8_UNORM, (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32)?;
            let nv12: Vec<ID3D11Texture2D> =
                (0..4).map(|_| tex(DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET.0 as u32)).collect::<Result<_>>()?;

            let vdev: ID3D11VideoDevice = device.cast()?;
            let vctx: ID3D11VideoContext = context.cast()?;
            let rate = DXGI_RATIONAL { Numerator: s.fps, Denominator: 1 };
            let cdesc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: width,
                InputHeight: height,
                OutputFrameRate: rate,
                OutputWidth: width,
                OutputHeight: height,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let venum = vdev.CreateVideoProcessorEnumerator(&cdesc)?;
            let vp = vdev.CreateVideoProcessor(&venum, 0)?;
            // Full-range RGB in; BT.709 limited-range YCbCr out (what the encoder signals).
            vctx.VideoProcessorSetStreamColorSpace(&vp, 0, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0b10_0000 });
            vctx.VideoProcessorSetOutputColorSpace(&vp, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0b01_0100 });
            let ivd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
            };
            let mut vin = None;
            vdev.CreateVideoProcessorInputView(&bgra, &venum, &ivd, Some(&mut vin))?;
            let ovd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut vout = Vec::new();
            for t in &nv12 {
                let mut v = None;
                vdev.CreateVideoProcessorOutputView(t, &venum, &ovd, Some(&mut v))?;
                vout.push(v.unwrap());
            }
            let enc = Encoder::new(&device, width, height, s)?;
            Ok(Capturer {
                device,
                context,
                output,
                dup: Some(dup),
                bgra,
                nv12,
                vctx,
                vp,
                vin: vin.unwrap(),
                vout,
                enc,
                have_image: false,
                grabbed_us: 0,
                encoded_grab: 0,
                slot: 0,
                pointer: None,
                follow_input_desktop: s.follow_input_desktop,
                width,
                height,
            })
        }
    }

    /// The pointer, in pixels of this monitor, while visible.
    pub fn pointer(&self) -> Option<(i32, i32)> {
        self.pointer
    }

    /// Change the target bitrate while streaming. False if the encoder refused.
    pub fn set_bitrate(&mut self, bps: u32) -> bool {
        set_codec(&self.enc.codec, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bps))
    }

    /// The encoder, and any settings it refused.
    pub fn encoder_name(&self) -> String {
        match self.enc.refused.as_slice() {
            [] => self.enc.name.clone(),
            refused => format!("{} — refused: {}", self.enc.name, refused.join(", ")),
        }
    }

    /// Whether frames are HEVC (H.265) rather than H.264.
    pub fn is_hevc(&self) -> bool {
        self.enc.hevc
    }

    /// Wait up to `timeout_ms` for the screen or the pointer to change, and keep the newest
    /// image. Returns whether the image changed. Cheap: a GPU copy at most.
    pub fn poll(&mut self, timeout_ms: u32) -> Result<bool> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            let dup = self.dup.as_ref().ok_or_else(|| windows::core::Error::new(E_FAIL, "the screen duplication was lost"))?;
            match dup.AcquireNextFrame(timeout_ms, &mut info, &mut res) {
                Ok(()) => {
                    if info.LastMouseUpdateTime != 0 {
                        let p = info.PointerPosition;
                        self.pointer = p.Visible.as_bool().then_some((p.Position.x, p.Position.y));
                    }
                    let mut updated = false;
                    if info.LastPresentTime != 0 {
                        if let Some(r) = &res {
                            let t: ID3D11Texture2D = r.cast()?;
                            self.context.CopyResource(&self.bgra, &t);
                            self.have_image = true;
                            self.grabbed_us = hop_core::clock::now_us();
                            updated = true;
                        }
                    }
                    drop(res);
                    let _ = dup.ReleaseFrame();
                    Ok(updated)
                }
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(false),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    // Mode change, full-screen app, or a desktop switch (lock screen, UAC):
                    // start over, on whichever desktop has input now. A process may hold one
                    // duplication per output, so the old one goes first.
                    self.dup = None;
                    let switch_failed = if self.follow_input_desktop { switch_to_input_desktop() } else { None };
                    let dup = duplicate(&self.output, &self.device, switch_failed)?;
                    // A new display mode (another resolution, a TV plugged in) needs textures, a
                    // converter and an encoder of the new size: the caller opens a new capturer.
                    let m = dup.GetDesc().ModeDesc;
                    let (w, h) = (m.Width & !1, m.Height & !1);
                    if (w, h) != (self.width, self.height) {
                        return Err(windows::core::Error::new(E_FAIL, format!("the screen changed to {w}×{h}; restarting capture")));
                    }
                    self.dup = Some(dup);
                    Ok(false)
                }
                Err(e) => Err(e),
            }
        }
    }

    /// Encode the newest image (as a keyframe with `force_key`). `None` before the first image.
    pub fn encode_latest(&mut self, force_key: bool) -> Result<Option<(Vec<EncodedFrame>, Timing)>> {
        if !self.have_image {
            return Ok(None);
        }
        unsafe {
            // Re-encoding an unchanged image (a keyframe on request): the screen still looks like
            // this now, so it counts as captured now.
            let captured_us = if self.grabbed_us == self.encoded_grab { hop_core::clock::now_us() } else { self.grabbed_us };
            self.encoded_grab = self.grabbed_us;
            let t0 = Instant::now();
            let slot = self.slot;
            self.slot = (self.slot + 1) % self.nv12.len();
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(self.vin.clone())),
                ..Default::default()
            };
            let mut streams = [stream];
            let r = self.vctx.VideoProcessorBlt(&self.vp, &self.vout[slot], 0, &streams);
            drop(ManuallyDrop::take(&mut streams[0].pInputSurface));
            r?;
            let t1 = Instant::now();
            let frames = self.enc.encode(&self.nv12[slot], force_key)?;
            let t2 = Instant::now();
            Ok(Some((
                frames,
                Timing { convert_us: (t1 - t0).as_micros() as u32, encode_us: (t2 - t1).as_micros() as u32, captured_us },
            )))
        }
    }

    /// Wait up to `timeout_ms` for the screen to change, then encode it. With `force_key` the
    /// last image is re-encoded as a keyframe even if nothing changed; `repeat` re-encodes an
    /// unchanged screen every time (benchmarks). Returns `None` when there is nothing to send.
    pub fn next(&mut self, timeout_ms: u32, force_key: bool, repeat: bool) -> Result<Option<(Vec<EncodedFrame>, Timing)>> {
        let updated = self.poll(timeout_ms)?;
        if !(updated || force_key || repeat) {
            return Ok(None);
        }
        self.encode_latest(force_key)
    }
}
