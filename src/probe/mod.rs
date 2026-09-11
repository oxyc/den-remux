//! What a release actually is, read from the file over HTTP Range: duration, tracks, codec strings,
//! and the keyframe index the playlist is cut from.
//!
//! The index is what makes a seek cheap. Matroska keeps it in `Cues`, found through the `SeekHead` —
//! one ranged read for a two-hour film, the approach of Jellyfin's `MatroskaKeyframeExtractor`. MP4
//! keeps it in `stss` plus `stts`/`ctts`, inside a `moov` that is at the front or, without faststart,
//! after the media data.
//!
//! Hand-written parsers rather than ffprobe, for the same reasons den-scout gives: the structures
//! needed are small, and ffprobe would be another binary parsing untrusted bytes before we have
//! decided to trust the file at all.

pub mod mkv;
pub mod mp4;

use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum VideoCodec {
    H264,
    Hevc,
    Other(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioTrack {
    /// The container's own name for it: a Matroska `A_…` codec id, or an MP4 sample-entry fourcc.
    pub codec: String,
    pub language: Option<String>,
    pub channels: u32,
    /// The muxer's title for the track ("Director's Commentary", "English 5.1").
    pub name: Option<String>,
    /// Matroska's FlagDefault (which is on unless a muxer turned it off); always on for MP4.
    pub default: bool,
    /// Flagged a commentary (Matroska FlagCommentary) or titled as one.
    pub commentary: bool,
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub container: &'static str,
    pub duration: f64,
    pub video: VideoCodec,
    /// The RFC 6381 string for the copied video — `avc1.640028`, `hvc1.2.4.L150.B0` — from the codec
    /// configuration record. `None` when the file carries none.
    pub codecs: Option<String>,
    pub width: u32,
    pub height: u32,
    /// HDR by what the container says of the colours (`is_hdr`): a conversion to SDR H.264 tone-maps it.
    pub hdr: bool,
    /// The video's Dolby Vision configuration, when it carries one.
    pub dolby_vision: Option<DolbyVision>,
    pub audio: Vec<AudioTrack>,
    /// Keyframe presentation times in seconds, ascending — the timeline ffmpeg reports with
    /// `-copyts -start_at_zero`, which is the one the segments are cut on.
    pub keyframes: Vec<f64>,
}

#[derive(Debug)]
pub enum ProbeError {
    /// A file we understand but will not play: another container, codec or layout.
    Unsupported(String),
    /// The bytes ran out before a structure did.
    Truncated(&'static str),
    /// The read itself failed. The text has had its URL removed.
    Fetch(String),
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProbeError::Unsupported(why) => write!(f, "unsupported: {why}"),
            ProbeError::Truncated(what) => write!(f, "truncated {what}"),
            ProbeError::Fetch(why) => write!(f, "read failed: {why}"),
        }
    }
}

/// Is this what a container says of the colours HDR (H.273 code points)? The transfer says so outright — PQ (16)
/// or HLG (18) — but a muxer often writes none: ffmpeg leaves it to the video stream's own headers, which this
/// doesn't read. Rec. 2020 primaries (9) or matrix (9) then stand for it: in a release they mean HDR, and SDR is
/// Rec. 709.
pub fn is_hdr(transfer: u64, primaries: u64, matrix: u64) -> bool {
    matches!(transfer, 16 | 18) || primaries == 9 || matrix == 9
}

/// A Dolby Vision stream's profile, and what its base layer is without it — written `8.1`, `7.6`, as Dolby does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DolbyVision {
    pub profile: u8,
    /// The base layer's compatibility id: 0 none (profile 5, whose picture needs the RPU), 1 HDR10, 2 SDR, 4 HLG,
    /// 6 a UHD Blu-ray's HDR10.
    pub compat: u8,
}

impl DolbyVision {
    /// Whether the base layer shows on its own once the Dolby Vision parts are stripped. Profile 5's doesn't:
    /// without the RPU its colours come out green and purple.
    pub fn has_fallback(&self) -> bool {
        self.compat != 0
    }
}

impl fmt::Display for DolbyVision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Dolby Vision {}.{}", self.profile, self.compat)
    }
}

/// A `DOVIDecoderConfigurationRecord` (the `dvcC`/`dvvC` payload, or Matroska's BlockAddIDExtraData): two version
/// bytes, the profile in the top 7 bits of the third, the compatibility id in the top 4 bits of the fifth.
pub fn dovi_config(record: &[u8]) -> Option<DolbyVision> {
    let b = record.get(..5)?;
    Some(DolbyVision { profile: b[2] >> 1, compat: b[4] >> 4 })
}

/// The largest single structure we will read: a `moov` or `Cues` for a long film is a few MB, and
/// anything claiming far more is a broken or hostile file, not one to buffer.
pub const MAX_ELEMENT: u64 = 64 << 20;

/// Where the bytes come from: the release over HTTP, or a buffer in tests.
pub enum Source<'a> {
    Http {
        client: &'a reqwest::Client,
        url: &'a str,
    },
    #[cfg(test)]
    Mem(&'a [u8]),
}

impl Source<'_> {
    /// `len` bytes from `start`, or fewer at the end of the file.
    pub async fn read(&self, start: u64, len: u64) -> Result<Vec<u8>, ProbeError> {
        match self {
            #[cfg(test)]
            Source::Mem(b) => {
                let s = start.min(b.len() as u64) as usize;
                let e = start.saturating_add(len).min(b.len() as u64) as usize;
                Ok(b[s..e].to_vec())
            }
            Source::Http { client, url } => {
                read_range(client, url, start, len).await.map_err(ProbeError::Fetch)
            }
        }
    }
}

/// One ranged GET. A 200 is accepted only for a read from the start, because a server that ignores
/// Range is sending the whole file and the first bytes are all that is wanted.
pub async fn read_range(
    client: &reqwest::Client,
    url: &str,
    start: u64,
    len: u64,
) -> Result<Vec<u8>, String> {
    let end = start + len.max(1) - 1;
    let resp = client
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    let status = resp.status().as_u16();
    if !(status == 206 || (status == 200 && start == 0)) {
        return Err(format!("a range read answered {status}"));
    }
    read_capped(resp, len).await
}

/// At most `cap` bytes of a body, stopping as soon as that many arrived — never the rest of a 60 GB
/// remux behind a server that answered 200.
pub async fn read_capped(mut resp: reqwest::Response, cap: u64) -> Result<Vec<u8>, String> {
    let cap = cap as usize;
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.without_url().to_string())? {
        let room = cap - out.len();
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.len() >= cap {
            break;
        }
    }
    Ok(out)
}

/// Probe a file whose first bytes are `head`, dispatching on its magic rather than its name.
pub async fn probe(src: &Source<'_>, head: &[u8]) -> Result<MediaInfo, ProbeError> {
    if head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return mkv::probe(src, head).await;
    }
    if head.len() >= 8 && matches!(&head[4..8], b"ftyp" | b"moov" | b"free" | b"mdat" | b"wide" | b"skip") {
        return mp4::probe(src, head).await;
    }
    Err(ProbeError::Unsupported("neither Matroska nor MP4".into()))
}

/// `avc1.PPCCLL` from an `avcC` record: profile, constraint flags, level.
pub fn avc_codecs(avcc: &[u8]) -> Option<String> {
    (avcc.len() >= 4 && avcc[0] == 1).then(|| format!("avc1.{:02x}{:02x}{:02x}", avcc[1], avcc[2], avcc[3]))
}

/// The profile, level and tier an RFC 6381 string names: `avc1.640033` is (100, 51, false), level 5.1 as
/// `level_idc`; `hvc1.2.4.H153.B0` is (2, 153, true), Main 10 at level 5.1 (`general_level_idc`, level × 30) in
/// HEVC's High tier.
pub fn profile_level(codecs: &str) -> Option<(u8, u16, bool)> {
    let mut parts = codecs.split('.');
    match parts.next()? {
        "avc1" | "avc3" => {
            let p = parts.next()?;
            let byte = |i: usize| u8::from_str_radix(p.get(i..i + 2)?, 16).ok();
            Some((byte(0)?, byte(4)? as u16, false))
        }
        "hvc1" | "hev1" => {
            let profile = parts.next()?.trim_start_matches(['A', 'B', 'C']).parse().ok()?;
            let tier_level = parts.nth(1)?;
            let level = tier_level.get(1..)?.parse().ok()?;
            Some((profile, level, tier_level.starts_with('H')))
        }
        _ => None,
    }
}

/// `hvc1.<space><profile>.<compat>.<tier><level>.<constraints>` from an `hvcC` record, per ISO/IEC
/// 14496-15 annex E: the compatibility flags bit-reversed in hex, trailing zero constraint bytes
/// dropped.
pub fn hevc_codecs(hvcc: &[u8]) -> Option<String> {
    if hvcc.len() < 13 {
        return None;
    }
    let space = ["", "A", "B", "C"][(hvcc[1] >> 6) as usize];
    let tier = if hvcc[1] & 0x20 != 0 { 'H' } else { 'L' };
    let profile = hvcc[1] & 0x1f;
    let compat = u32::from_be_bytes([hvcc[2], hvcc[3], hvcc[4], hvcc[5]]).reverse_bits();
    let level = hvcc[12];
    let mut constraints = hvcc[6..12].to_vec();
    while constraints.last() == Some(&0) {
        constraints.pop();
    }
    let mut s = format!("hvc1.{space}{profile}.{compat:X}.{tier}{level}");
    for c in constraints {
        s.push_str(&format!(".{c:X}"));
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_strings_follow_rfc_6381() {
        assert_eq!(avc_codecs(&[1, 0x64, 0x00, 0x28, 0xff]).as_deref(), Some("avc1.640028"));
        assert_eq!(avc_codecs(&[0, 0x64, 0, 0x28]), None, "not an avcC record");
        // Main 10, level 5.0 (150), progressive-source + frame-only constraints: the Apple spec example.
        let main10 = [1, 0x02, 0x20, 0, 0, 0, 0xB0, 0, 0, 0, 0, 0, 150];
        assert_eq!(hevc_codecs(&main10).as_deref(), Some("hvc1.2.4.L150.B0"));
        // Main, level 3.1 (93), compatible with Main and Main 10.
        let main = [1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 93];
        assert_eq!(hevc_codecs(&main).as_deref(), Some("hvc1.1.6.L93.90"));
        // High tier.
        let high = [1, 0x22, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 153];
        assert_eq!(hevc_codecs(&high).as_deref(), Some("hvc1.2.4.H153"));
    }

    #[test]
    fn a_codec_string_names_its_profile_and_level() {
        assert_eq!(profile_level("avc1.640033"), Some((100, 51, false)));
        assert_eq!(profile_level("hvc1.2.4.L153.B0"), Some((2, 153, false)));
        assert_eq!(profile_level("hvc1.2.4.H153.B0"), Some((2, 153, true)), "a UHD Blu-ray remux's");
        assert_eq!(profile_level("hvc1.1.6.L93.90"), Some((1, 93, false)));
        assert_eq!(profile_level("hev1.A1.60.H120"), Some((1, 120, true)));
        assert_eq!(profile_level("mp4a.40.2"), None);
        assert_eq!(profile_level("hvc1.2"), None);
    }

    #[test]
    fn a_dolby_vision_record_names_its_profile_and_base_layer() {
        // Profile 7 (a UHD Blu-ray's dual layer), level 6, RPU + EL + BL present, compatibility 6.
        let bluray = dovi_config(&[1, 0, 7 << 1, (6 << 3) | 0b111, 6 << 4, 0, 0, 0]).unwrap();
        assert_eq!(bluray, DolbyVision { profile: 7, compat: 6 });
        assert!(bluray.has_fallback());
        assert_eq!(bluray.to_string(), "Dolby Vision 7.6");
        let web = dovi_config(&[1, 0, 5 << 1, (6 << 3) | 0b101, 0, 0]).unwrap();
        assert!(!web.has_fallback(), "profile 5 needs its RPU");
        assert_eq!(dovi_config(&[1, 0, 16]), None, "truncated");
    }
}
