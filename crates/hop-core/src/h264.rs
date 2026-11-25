//! Just enough H.264 and HEVC (H.265) bitstream handling for the viewer: split Annex B into NAL
//! units, pull out the parameter sets, and repackage a frame with 4-byte lengths (AVCC / hvcC)
//! for VideoToolbox.

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| b & 0x1F).unwrap_or(0)
}

/// NAL units of an Annex B stream, without start codes.
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut end = if k + 1 < starts.len() { starts[k + 1] - 3 } else { data.len() };
        // Drop the extra zero of a 4-byte start code and any trailing zeros.
        while end > s && data[end - 1] == 0 {
            end -= 1;
        }
        if end > s {
            out.push(&data[s..end]);
        }
    }
    out
}

/// One frame ready for the decoder.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AvccFrame {
    /// HEVC only.
    pub vps: Option<Vec<u8>>,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
    /// Slice (and SEI) NAL units, each prefixed with a 4-byte big-endian length.
    pub avcc: Vec<u8>,
    pub keyframe: bool,
}

/// HEVC NAL unit types this needs (the type is in bits 1–6 of the first header byte).
pub mod hevc {
    pub const IDR_W_RADL: u8 = 19;
    pub const IDR_N_LP: u8 = 20;
    pub const VPS: u8 = 32;
    pub const SPS: u8 = 33;
    pub const PPS: u8 = 34;
    pub const AUD: u8 = 35;

    pub fn nal_type(nal: &[u8]) -> u8 {
        nal.first().map(|b| (b >> 1) & 0x3F).unwrap_or(0)
    }
}

/// Convert one Annex B access unit to 4-byte length prefixes, H.264 or (`hevc`) H.265.
pub fn to_length_prefixed(frame: &[u8], is_hevc: bool) -> AvccFrame {
    let mut f = AvccFrame::default();
    for nal in nal_units(frame) {
        let (kind, key) = if is_hevc {
            match hevc::nal_type(nal) {
                hevc::VPS => (Some(0), false),
                hevc::SPS => (Some(1), false),
                hevc::PPS => (Some(2), false),
                hevc::AUD => (Some(3), false),
                t => (None, t == hevc::IDR_W_RADL || t == hevc::IDR_N_LP),
            }
        } else {
            match nal_type(nal) {
                NAL_SPS => (Some(1), false),
                NAL_PPS => (Some(2), false),
                NAL_AUD => (Some(3), false),
                t => (None, t == NAL_IDR),
            }
        };
        match kind {
            Some(0) => f.vps = Some(nal.to_vec()),
            Some(1) => f.sps = Some(nal.to_vec()),
            Some(2) => f.pps = Some(nal.to_vec()),
            Some(_) => {}
            None => {
                f.keyframe |= key;
                f.avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                f.avcc.extend_from_slice(nal);
            }
        }
    }
    f
}

/// Split a raw Annex B file into access units (for playing back a capture file).
pub fn access_units(data: &[u8]) -> Vec<Vec<u8>> {
    let mut frames: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut cur_has_slice = false;
    for nal in nal_units(data) {
        let t = nal_type(nal);
        // A new frame starts at an AUD, at parameter sets after a slice, or at a slice whose
        // first_mb_in_slice is 0 (its first bit after the header is 1) after a slice.
        let first_mb_zero = nal.len() > 1 && nal[1] & 0x80 != 0;
        let starts_new = match t {
            NAL_AUD => true,
            NAL_SPS | NAL_PPS | NAL_SEI => cur_has_slice,
            NAL_SLICE | NAL_IDR => cur_has_slice && first_mb_zero,
            _ => false,
        };
        if starts_new && !cur.is_empty() {
            frames.push(std::mem::take(&mut cur));
            cur_has_slice = false;
        }
        cur.extend_from_slice(&[0, 0, 0, 1]);
        cur.extend_from_slice(nal);
        if matches!(t, NAL_SLICE | NAL_IDR) {
            cur_has_slice = true;
        }
    }
    if !cur.is_empty() {
        frames.push(cur);
    }
    frames
}

#[cfg(test)]
mod tests {
    #[test]
    fn hevc_frames_split_into_parameter_sets_and_slices() {
        // VPS, SPS, PPS, then an IDR slice (type 19), each after a start code.
        let nal = |t: u8, body: &[u8]| [&[0u8, 0, 0, 1, t << 1, 1][..], body].concat();
        let frame = [nal(32, &[7]), nal(33, &[8]), nal(34, &[9]), nal(19, &[0xAA, 0xBB])].concat();
        let f = super::to_length_prefixed(&frame, true);
        assert_eq!(f.vps.as_deref(), Some(&[64, 1, 7][..]));
        assert_eq!((f.sps.is_some(), f.pps.is_some(), f.keyframe), (true, true, true));
        assert_eq!(f.avcc, [&[0, 0, 0, 4][..], &[38, 1, 0xAA, 0xBB]].concat());
    }

    use super::*;

    fn stream() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28]); // SPS
        v.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80]); // PPS
        v.extend_from_slice(&[0, 0, 1, 0x65, 0x88, 0x84, 0x00]); // IDR slice, first_mb 0
        v.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9A, 0x02]); // P slice, first_mb 0
        v.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9A, 0x04]); // P slice
        v
    }

    #[test]
    fn splits_nal_units_with_both_start_code_lengths() {
        let s = stream();
        let n = nal_units(&s);
        assert_eq!(n.len(), 5);
        assert_eq!(n.iter().map(|x| nal_type(x)).collect::<Vec<_>>(), vec![7, 8, 5, 1, 1]);
        assert_eq!(n[2], &[0x65, 0x88, 0x84]); // trailing zero (next 4-byte start code) dropped
    }

    #[test]
    fn keyframe_to_avcc_keeps_parameter_sets_apart() {
        let aus = access_units(&stream());
        assert_eq!(aus.len(), 3);
        let f = to_length_prefixed(&aus[0], false);
        assert!(f.keyframe);
        assert_eq!(f.sps.as_deref(), Some(&[0x67, 0x64, 0x00, 0x28][..]));
        assert_eq!(f.pps.as_deref(), Some(&[0x68, 0xEE, 0x3C, 0x80][..]));
        assert_eq!(f.avcc, vec![0, 0, 0, 3, 0x65, 0x88, 0x84]);
        let p = to_length_prefixed(&aus[1], false);
        assert!(!p.keyframe && p.sps.is_none());
        assert_eq!(p.avcc, vec![0, 0, 0, 3, 0x41, 0x9A, 0x02]);
    }
}
