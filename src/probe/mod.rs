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
    /// Copied only, for a player that decodes it: nothing on the box converts AV1.
    Av1,
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
    /// The RFC 6381 string for the copied video — `avc1.640028`, `hvc1.2.4.L150.B0`, `av01.0.08M.08` — from the
    /// codec configuration record. `None` when the file carries none.
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

/// What something says of a stream's colours, as H.273 code points — primaries, transfer, matrix — and whether its
/// range is full. 0 is "doesn't say", and so is 2, H.273's "unspecified".
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Colour {
    pub primaries: u64,
    pub transfer: u64,
    pub matrix: u64,
    pub full_range: Option<bool>,
}

impl Colour {
    /// What this says, and where it says nothing, what `other` says.
    fn or(self, other: Colour) -> Colour {
        let pick = |a: u64, b: u64| if matches!(a, 0 | 2) { b } else { a };
        Colour {
            primaries: pick(self.primaries, other.primaries),
            transfer: pick(self.transfer, other.transfer),
            matrix: pick(self.matrix, other.matrix),
            full_range: self.full_range.or(other.full_range),
        }
    }

    fn says(&self) -> bool {
        [self.primaries, self.transfer, self.matrix].iter().any(|c| !matches!(c, 0 | 2))
    }
}

/// An AV1 track's codec string and whether it is HDR, from its `av1C` record — Matroska's CodecPrivate for `V_AV1`,
/// or the MP4 box — and what the container says of its colours. Where the container is silent, the colour
/// description in the record's sequence header speaks instead: a muxer that writes no Matroska Colour element or MP4
/// `colr` box still carries that.
pub fn av1_track(av1c: &[u8], container: Colour) -> (Option<String>, bool) {
    let colour = container.or(av1c.get(4..).and_then(av1_sequence_colour).unwrap_or_default());
    (av1_codecs(av1c, colour), is_hdr(colour.transfer, colour.primaries, colour.matrix))
}

/// `av01.P.LLT.DD` from an `av1C` record, per AV1-ISOBMFF's codecs parameter string: the profile, `seq_level_idx_0`
/// (two digits), the tier (`M`/`H`) and the bit depth. The optional fields — monochrome, chroma subsampling and
/// sample position, primaries, transfer, matrix, full range — go all together or not at all, and absent they mean
/// 4:2:0 BT.709: so they're written whenever the stream says anything of its colours, or isn't 4:2:0 in colour. An
/// HDR stream's string then names its PQ or HLG, which is how a player knows it from SDR.
pub fn av1_codecs(av1c: &[u8], colour: Colour) -> Option<String> {
    // marker (1) and version (1); seq_profile (3) and seq_level_idx_0 (5); seq_tier_0, high_bitdepth, twelve_bit,
    // monochrome, chroma_subsampling_x, chroma_subsampling_y, chroma_sample_position (2).
    let b = av1c.get(..4)?;
    if b[0] != 0x81 {
        return None;
    }
    let (profile, level) = (b[1] >> 5, b[1] & 0x1f);
    let tier = if b[2] & 0x80 != 0 { 'H' } else { 'M' };
    let depth = match (b[2] & 0x40 != 0, b[2] & 0x20 != 0) {
        (false, _) => 8,
        (true, false) => 10,
        (true, true) => 12,
    };
    let mut s = format!("av01.{profile}.{level:02}{tier}.{depth:02}");
    let (mono, x, y) = ((b[2] >> 4) & 1, (b[2] >> 3) & 1, (b[2] >> 2) & 1);
    if colour.says() || mono == 1 || (x, y) != (1, 1) {
        // The sample position counts only for 4:2:0.
        let position = if (x, y) == (1, 1) { b[2] & 3 } else { 0 };
        let code = |c: u64| if c == 0 { 2 } else { c };
        s.push_str(&format!(
            ".{mono}.{x}{y}{position}.{:02}.{:02}.{:02}.{}",
            code(colour.primaries),
            code(colour.transfer),
            code(colour.matrix),
            u8::from(colour.full_range == Some(true))
        ));
    }
    Some(s)
}

/// Bits of an OBU, most significant first.
struct Bits<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn read(&mut self, n: u32) -> Option<u64> {
        let mut v = 0u64;
        for _ in 0..n {
            let byte = *self.b.get(self.pos / 8)?;
            v = v << 1 | ((byte >> (7 - self.pos % 8)) & 1) as u64;
            self.pos += 1;
        }
        Some(v)
    }

    fn flag(&mut self) -> Option<bool> {
        self.read(1).map(|v| v == 1)
    }

    /// AV1's `uvlc()`.
    fn uvlc(&mut self) -> Option<u64> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros >= 32 {
                return None;
            }
        }
        Some(self.read(zeros)? + (1 << zeros) - 1)
    }
}

/// The colours the sequence header among an `av1C` record's configOBUs describes, walked field by field to its
/// `color_config` (AV1 spec 5.5). `None` without a sequence header, or with one that ends early.
fn av1_sequence_colour(mut obus: &[u8]) -> Option<Colour> {
    // An OBU: its header (type in bits 1–4, then an extension flag and a has-size flag), an optional extension
    // byte, a leb128 size when it has one — else it runs to the end.
    while let Some(&head) = obus.first() {
        let mut at = 1 + usize::from(head & 0x04 != 0);
        let size = match head & 0x02 != 0 {
            true => {
                let mut size = 0usize;
                for i in 0..8 {
                    let byte = *obus.get(at)?;
                    at += 1;
                    size |= ((byte & 0x7f) as usize) << (7 * i);
                    if byte & 0x80 == 0 {
                        break;
                    }
                }
                size
            }
            false => obus.len().checked_sub(at)?,
        };
        let payload = obus.get(at..at.checked_add(size)?)?;
        if (head >> 3) & 0x0f == 1 {
            return sequence_colour(payload);
        }
        obus = &obus[at + size..];
    }
    None
}

fn sequence_colour(payload: &[u8]) -> Option<Colour> {
    let mut r = Bits { b: payload, pos: 0 };
    let profile = r.read(3)?;
    let _still_picture = r.flag()?;
    let reduced = r.flag()?;
    if reduced {
        r.read(5)?; // seq_level_idx[0]
    } else {
        let mut decoder_model = false;
        let mut buffer_delay_bits = 0;
        if r.flag()? {
            // timing_info: num_units_in_display_tick, time_scale, equal_picture_interval.
            r.read(64)?;
            if r.flag()? {
                r.uvlc()?;
            }
            decoder_model = r.flag()?;
            if decoder_model {
                buffer_delay_bits = r.read(5)? as u32 + 1;
                r.read(32 + 5 + 5)?;
            }
        }
        let initial_display_delay = r.flag()?;
        for _ in 0..=r.read(5)? {
            r.read(12)?; // operating_point_idc
            if r.read(5)? > 7 {
                r.read(1)?; // seq_tier
            }
            if decoder_model && r.flag()? {
                r.read(2 * buffer_delay_bits + 1)?;
            }
            if initial_display_delay && r.flag()? {
                r.read(4)?;
            }
        }
    }
    let width_bits = r.read(4)? as u32 + 1;
    let height_bits = r.read(4)? as u32 + 1;
    r.read(width_bits + height_bits)?;
    if !reduced && r.flag()? {
        r.read(4 + 3)?; // frame id lengths
    }
    r.read(3)?; // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
    if !reduced {
        r.read(4)?; // interintra, masked compound, warped motion, dual filter
        let order_hint = r.flag()?;
        if order_hint {
            r.read(2)?; // jnt_comp, ref_frame_mvs
        }
        let screen_content = if r.flag()? { 2 } else { r.read(1)? };
        if screen_content > 0 && !r.flag()? {
            r.read(1)?; // seq_force_integer_mv
        }
        if order_hint {
            r.read(3)?;
        }
    }
    r.read(3)?; // enable_superres, enable_cdef, enable_restoration
    let high_bitdepth = r.flag()?;
    if profile == 2 && high_bitdepth {
        r.read(1)?;
    }
    let mono = profile != 1 && r.flag()?;
    let mut colour = Colour::default();
    if r.flag()? {
        colour.primaries = r.read(8)?;
        colour.transfer = r.read(8)?;
        colour.matrix = r.read(8)?;
    }
    // sRGB (BT.709 primaries, the sRGB transfer, the identity matrix) is full range without saying so.
    let srgb = !mono && (colour.primaries, colour.transfer, colour.matrix) == (1, 13, 0);
    colour.full_range = Some(srgb || r.flag()?);
    Some(colour)
}

/// A Dolby Vision stream's profile, and what its base layer is without it — written `8.1`, `7.6`, as Dolby does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DolbyVision {
    pub profile: u8,
    /// The base layer's compatibility id: 0 none (profile 5, whose picture needs the RPU), 1 HDR10, 2 SDR, 4 HLG,
    /// 6 a UHD Blu-ray's HDR10.
    pub compat: u8,
    /// Dolby Vision's own level (1–13), which its HLS codec string names: `dvh1.08.06`.
    pub level: u8,
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
/// bytes, the profile in the top 7 bits of the third, the level in its last bit and the top 5 of the fourth, the
/// compatibility id in the top 4 bits of the fifth.
pub fn dovi_config(record: &[u8]) -> Option<DolbyVision> {
    let b = record.get(..5)?;
    Some(DolbyVision { profile: b[2] >> 1, compat: b[4] >> 4, level: ((b[2] & 1) << 5) | (b[3] >> 3) })
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
/// HEVC's High tier; `av01.0.13M.10` is (0, 13, false), Main profile at `seq_level_idx` 13 (level 5.1), Main tier.
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
        "av01" => {
            let profile = parts.next()?.parse().ok()?;
            let level_tier = parts.next()?;
            let level = level_tier.get(..2)?.parse().ok()?;
            Some((profile, level, level_tier.get(2..)? == "H"))
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
        assert_eq!(profile_level("av01.0.13M.10.0.110.09.16.09.0"), Some((0, 13, false)));
        assert_eq!(profile_level("av01.0.14H.10"), Some((0, 14, true)));
        assert_eq!(profile_level("av01.0"), None);
    }

    /// `fields` as (value, width) pairs, packed most significant bit first.
    fn pack(fields: &[(u64, u32)]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        let mut n = 0usize;
        for &(value, width) in fields {
            for i in (0..width).rev() {
                if n.is_multiple_of(8) {
                    out.push(0);
                }
                if (value >> i) & 1 == 1 {
                    *out.last_mut().unwrap() |= 0x80 >> (n % 8);
                }
                n += 1;
            }
        }
        out
    }

    #[test]
    fn an_av1_record_names_profile_level_tier_and_depth() {
        let none = Colour::default();
        // Main, level 4.0, 8-bit 4:2:0: nothing to say past the depth.
        assert_eq!(av1_codecs(&[0x81, 0x08, 0x0C, 0], none).as_deref(), Some("av01.0.08M.08"));
        assert_eq!(av1_codecs(&[0x01, 0x08, 0x0C, 0], none), None, "no marker bit");
        assert_eq!(av1_codecs(&[0x81, 0x08], none), None, "truncated");
        // Professional, level 6.2, High tier, 12-bit 4:2:2: the optional fields, unspecified colours and all.
        let pro = av1_codecs(&[0x81, (2 << 5) | 14, 0b1110_1000, 0], none);
        assert_eq!(pro.as_deref(), Some("av01.2.14H.12.0.100.02.02.02.0"));
        let mono = av1_codecs(&[0x81, 0x04, 0b0001_1100, 0], none);
        assert_eq!(mono.as_deref(), Some("av01.0.04M.08.1.110.02.02.02.0"));
        // HDR10: the colours written out, so the string says PQ.
        let pq = Colour { primaries: 9, transfer: 16, matrix: 9, full_range: Some(false) };
        let hdr10 = av1_codecs(&[0x81, 13, 0x4C, 0], pq).unwrap();
        assert_eq!(hdr10, "av01.0.13M.10.0.110.09.16.09.0");
    }

    #[test]
    fn an_av1_sequence_header_speaks_for_a_silent_container() {
        // A full sequence header: timing and decoder model info, two operating points (one past level 3.3, so
        // with a tier bit), order hints and screen content tools chosen per frame, then HDR10's colour config.
        #[rustfmt::skip]
        let header = pack(&[
            (0, 3), (0, 1), (0, 1),            // Main, not a still, not reduced
            (1, 1), (1001, 32), (24000, 32), (1, 1), (1, 1), // timing info, equal intervals, uvlc 0
            (1, 1), (9, 5), (1, 32), (4, 5), (4, 5), // decoder model info: 10-bit buffer delays
            (1, 1), (1, 5),                    // initial display delays; two operating points
            (0x101, 12), (13, 5), (0, 1), (1, 1), (5, 10), (7, 10), (0, 1), (1, 1), (9, 4),
            (0x100, 12), (5, 5), (0, 1), (0, 1),
            (11, 4), (10, 4), (3839, 12), (2159, 11), // frame size bits and maxima
            (0, 1), (0b011, 3), (0b1111, 4),   // no frame ids; superblock and intra tools; inter tools
            (1, 1), (0b11, 2), (1, 1), (1, 1), (6, 3), // order hint, jnt/ref mvs, screen content, integer mv
            (0b011, 3),                        // superres, cdef, restoration
            (1, 1), (0, 1), (1, 1), (9, 8), (16, 8), (9, 8), (0, 1), (0, 2), // 10-bit, colours, limited range
        ]);
        let obus = |header: &[u8]| [&[0x12, 0x00][..], &[0x0A, header.len() as u8], header].concat();
        let record = [&[0x81, 13, 0x4C, 0][..], &obus(&header)].concat();
        assert_eq!(
            av1_track(&record, Colour::default()),
            (Some("av01.0.13M.10.0.110.09.16.09.0".into()), true),
            "after a temporal delimiter, the sequence header's PQ"
        );
        let sdr = Colour { primaries: 1, transfer: 1, matrix: 1, full_range: Some(false) };
        assert_eq!(
            av1_track(&record, sdr),
            (Some("av01.0.13M.10.0.110.01.01.01.0".into()), false),
            "the container says otherwise, and wins"
        );
        let cut = [&[0x81, 13, 0x4C, 0][..], &obus(&header[..header.len() - 4])].concat();
        assert_eq!(
            av1_track(&cut, Colour::default()),
            (Some("av01.0.13M.10".into()), false),
            "a sequence header that ends early says nothing"
        );
        assert_eq!(av1_sequence_colour(&[0x0C]), None, "an extension byte that isn't there");
    }

    #[test]
    fn a_dolby_vision_record_names_its_profile_and_base_layer() {
        // Profile 7 (a UHD Blu-ray's dual layer), level 6, RPU + EL + BL present, compatibility 6.
        let bluray = dovi_config(&[1, 0, 7 << 1, (6 << 3) | 0b111, 6 << 4, 0, 0, 0]).unwrap();
        assert_eq!(bluray, DolbyVision { profile: 7, compat: 6, level: 6 });
        assert!(bluray.has_fallback());
        assert_eq!(bluray.to_string(), "Dolby Vision 7.6");
        let web = dovi_config(&[1, 0, 5 << 1, (6 << 3) | 0b101, 0, 0]).unwrap();
        assert!(!web.has_fallback(), "profile 5 needs its RPU");
        // Level 9 at 8.1, and a level with its top bit in the third byte.
        let hdr10 = dovi_config(&[1, 0, 8 << 1, (9 << 3) | 0b101, 1 << 4]).unwrap();
        assert_eq!(hdr10, DolbyVision { profile: 8, compat: 1, level: 9 });
        assert_eq!(dovi_config(&[1, 0, (8 << 1) | 1, 1 << 3, 1 << 4]).unwrap().level, 33);
        assert_eq!(dovi_config(&[1, 0, 16]), None, "truncated");
    }
}
