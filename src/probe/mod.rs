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
    /// Copied only, as AV1 is — and 10-bit only through hls.js, since it is unmeasured in Safari's native player.
    Vp9,
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

/// A subtitle track of the release. Every one is listed, bitmap or not, because ffmpeg numbers its subtitle streams
/// (`-map 0:s:N`) across all of them.
#[derive(Clone, Debug, PartialEq)]
pub struct SubtitleTrack {
    /// The container's own name for it: a Matroska `S_…` codec id, or an MP4 sample-entry fourcc.
    pub codec: String,
    pub language: Option<String>,
    /// SRT, ASS/SSA, WebVTT or `mov_text`: what ffmpeg converts to WebVTT. PGS, VOBSUB and the like are bitmaps.
    pub text: bool,
    /// Flagged forced (foreign-language parts only), so no full track of its language.
    pub forced: bool,
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub container: &'static str,
    pub duration: f64,
    pub video: VideoCodec,
    /// The RFC 6381 string for the copied video — `avc1.640028`, `hvc1.2.4.L150.B0`, `av01.0.08M.08`, `vp09.00.41.08` —
    /// from the codec configuration record. `None` when the file carries none; VP9's is always named (`vp9_track`).
    pub codecs: Option<String>,
    pub width: u32,
    pub height: u32,
    /// HDR by what the container — or, where it is silent, the stream's own header — says of the colours
    /// (`is_hdr`): a conversion to SDR H.264 tone-maps it.
    pub hdr: bool,
    /// Whether the transfer is named HLG (18). An HDR stream that isn't is taken as PQ.
    pub hlg: bool,
    /// Frames a second, from Matroska's DefaultDuration or an MP4's sample timing: the master's FRAME-RATE,
    /// without which Safari passes an HDR variant over and plays nothing.
    pub frame_rate: Option<f64>,
    /// The video's Dolby Vision configuration, when it carries one.
    pub dolby_vision: Option<DolbyVision>,
    /// The container claims Profile 5 while the HEVC VUI proves there is a normal BT.2020 PQ/HLG base layer.
    /// Such a record is unsafe to keep: when the first RPU cannot correct it, playback still uses the base layer.
    pub dolby_vision_record_mismatch: bool,
    /// `dolby_vision` was not read from a configuration record but proven from the first frame's RPU: the container
    /// carries no `dvcC`/`dvvC`, and the HEVC VUI names no colours. Keeping it would need a record ffmpeg cannot
    /// write from nothing, and stripped or converted its picture is tinted, so such a release is not played.
    pub dolby_vision_recordless: bool,
    pub audio: Vec<AudioTrack>,
    /// The subtitle tracks, in the container's order.
    pub subtitles: Vec<SubtitleTrack>,
    /// Keyframe presentation times in seconds, ascending — the timeline ffmpeg reports with
    /// `-copyts -start_at_zero`, which is the one the segments are cut on.
    pub keyframes: Vec<f64>,
    /// Every segment, starting on a keyframe, decodes without the one before it: seen in the file, not assumed. A
    /// keyframe past the first is looked at — Matroska's picture NAL unit (`idr_keyframe`), an MP4's composition times
    /// (no picture after it presented before it) — and AV1 and VP9 key frames reset every reference. False for an open
    /// GOP, whose leading pictures reach into the segment before, and wherever the probe could not tell.
    pub closed_gops: bool,
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

/// Whether a keyframe is an IDR — no picture decoded after it refers to one before it — by the first picture NAL unit
/// in `frame`, whose units are length-prefixed with the length size its `avcC`/`hvcC` (`config`) names. H.264's IDR is
/// type 5, where an open GOP's keyframe is a plain I slice (1) at a recovery point. HEVC's are 19 and 20 (an IDR's
/// RADL leading pictures reach back to nothing), where an open GOP's is a CRA (21), whose RASL leading pictures
/// reference the GOP before; a BLA (16–18) is taken as open too. `None` for another codec, or no picture in `frame`.
pub fn idr_keyframe(codec: &VideoCodec, config: &[u8], frame: &[u8]) -> Option<bool> {
    let (length_size, hevc) = match codec {
        VideoCodec::H264 => ((*config.get(4)? & 3) as usize + 1, false),
        VideoCodec::Hevc => ((*config.get(21)? & 3) as usize + 1, true),
        _ => return None,
    };
    let mut at = 0usize;
    while at + length_size <= frame.len() {
        let len = frame[at..at + length_size].iter().fold(0usize, |n, b| n << 8 | *b as usize);
        at += length_size;
        let first = *frame.get(at)?;
        match hevc {
            true if (first >> 1) & 0x3f < 32 => return Some(matches!((first >> 1) & 0x3f, 19 | 20)),
            false if (1..=5).contains(&(first & 0x1f)) => return Some(first & 0x1f == 5),
            _ => {}
        }
        at = at.checked_add(len)?;
    }
    None
}

/// Is this what a container says of the colours HDR (H.273 code points)? The transfer says so outright — PQ (16)
/// or HLG (18) — but a muxer often writes none, leaving it to the video stream's own headers, which `av1_track` and
/// `hevc_track` read where the container is silent. Rec. 2020 primaries (9) or matrix (9) then stand for it: in a
/// release they mean HDR, and SDR is Rec. 709.
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

/// An AV1 codec string's bit depth: `av01.0.13M.10` is 10.
pub fn av1_bit_depth(codecs: &str) -> Option<u8> {
    codecs.strip_prefix("av01.")?.split('.').nth(2)?.parse().ok()
}

/// HLS's VIDEO-RANGE for an AV1 codec string that names its transfer: PQ (16) or HLG (18). `None` for SDR, and for a
/// string that doesn't say.
pub fn av1_video_range(codecs: &str) -> Option<&'static str> {
    match codecs.strip_prefix("av01.")?.split('.').nth(6)? {
        "16" => Some("PQ"),
        "18" => Some("HLG"),
        _ => None,
    }
}

/// What a VP9 stream records of itself — its profile, level (× 10), bit depth and chroma subsampling, as the VP codec
/// ISO media binding numbers them — and of its colours. `None` where nothing records it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vp9Config {
    pub profile: Option<u8>,
    pub level: Option<u8>,
    pub depth: Option<u8>,
    pub chroma: Option<u8>,
    pub colour: Colour,
}

/// A `vpcC` box's payload: version and flags, the profile, the level (0 names none) and the bit depth in 4 bits, then
/// — in version 1, what muxers write — the chroma subsampling in 3, the full-range flag, and the primaries, transfer
/// and matrix.
pub fn vpcc_config(vpcc: &[u8]) -> Option<Vp9Config> {
    let b = vpcc.get(..7)?;
    let mut config = Vp9Config {
        profile: Some(b[4]),
        level: Some(b[5]).filter(|l| *l > 0),
        depth: Some(b[6] >> 4),
        ..Vp9Config::default()
    };
    if let (1, Some(c)) = (b[0], vpcc.get(7..10)) {
        config.chroma = Some((b[6] >> 1) & 7);
        config.colour = Colour {
            primaries: c[0] as u64,
            transfer: c[1] as u64,
            matrix: c[2] as u64,
            full_range: Some(b[6] & 1 == 1),
        };
    }
    Some(config)
}

/// Matroska's CodecPrivate for `V_VP9`: WebM's codec features, each an id, a length and a value — 1 the profile, 2 the
/// level, 3 the bit depth, 4 the chroma subsampling. Muxers often write none (ffmpeg's Matroska muxer didn't, for a
/// libvpx encode), and then nothing is recorded.
pub fn vp9_features(private: &[u8]) -> Vp9Config {
    let mut config = Vp9Config::default();
    let mut rest = private;
    while let [id, len, tail @ ..] = rest {
        let Some(value) = tail.get(..*len as usize) else { break };
        let v = value.first().copied().filter(|_| *len == 1);
        match *id {
            1 => config.profile = v,
            2 => config.level = v.filter(|l| *l > 0),
            3 => config.depth = v,
            4 => config.chroma = v,
            _ => {}
        }
        rest = &tail[*len as usize..];
    }
    config
}

/// A VP9 track's codec string, whether it is HDR and whether that HDR is HLG, from what the file records of the stream
/// and what the container says of its colours. VP9 is HDR in profile 2 with a PQ or HLG transfer, and not otherwise.
///
/// A player is asked about the whole string, so what nothing records is assumed rather than left out: a bit depth of
/// 10 where the transfer is PQ or HLG (HDR VP9 is 10-bit) and 8 otherwise, the profile that depth needs (2 or 0), and
/// the level the picture needs at its frame rate — at 60 frames a second where that isn't known, so the level named
/// is never below what the release needs (`vp9_level`).
pub fn vp9_track(
    recorded: Vp9Config,
    container: Colour,
    width: u32,
    height: u32,
    frame_rate: Option<f64>,
) -> (String, bool, bool) {
    let colour = container.or(recorded.colour);
    let hdr_transfer = matches!(colour.transfer, 16 | 18);
    let depth = recorded.depth.unwrap_or(if hdr_transfer { 10 } else { 8 });
    let profile = recorded.profile.unwrap_or(if depth > 8 { 2 } else { 0 });
    let level = recorded.level.unwrap_or_else(|| vp9_level(width, height, frame_rate));
    let mut s = format!("vp09.{profile:02}.{level:02}.{depth:02}");
    // The optional fields — chroma subsampling, primaries, transfer, matrix, full range — go all together or not at
    // all, and absent they mean 4:2:0 (sited with the luma, 1) in BT.709: so they're written whenever the stream says
    // anything of its colours, or its chroma is otherwise. An HDR stream's string then names its PQ or HLG.
    let chroma = recorded.chroma.unwrap_or(1);
    if colour.says() || chroma != 1 {
        let code = |c: u64| if c == 0 { 2 } else { c };
        s.push_str(&format!(
            ".{chroma:02}.{:02}.{:02}.{:02}.{:02}",
            code(colour.primaries),
            code(colour.transfer),
            code(colour.matrix),
            u8::from(colour.full_range == Some(true))
        ));
    }
    (s, profile == 2 && hdr_transfer, profile == 2 && colour.transfer == 18)
}

/// The lowest VP9 level (× 10) whose largest picture and luma sample rate cover a `width` × `height` picture at
/// `frame_rate` (VP9 bitstream spec, Annex A) — at 60 frames a second where the rate isn't known, and for a 1080p
/// picture where the size isn't.
fn vp9_level(width: u32, height: u32, frame_rate: Option<f64>) -> u8 {
    // (level, largest picture, luma samples a second).
    const LEVELS: [(u8, u64, u64); 14] = [
        (10, 36_736, 829_440),
        (11, 73_856, 2_764_800),
        (20, 122_880, 4_608_000),
        (21, 245_760, 9_216_000),
        (30, 552_960, 20_736_000),
        (31, 983_040, 36_864_000),
        (40, 2_228_224, 83_558_400),
        (41, 2_228_224, 160_432_128),
        (50, 8_912_896, 311_951_360),
        (51, 8_912_896, 588_251_136),
        (52, 8_912_896, 1_176_502_272),
        (60, 35_651_584, 1_176_502_272),
        (61, 35_651_584, 2_353_004_544),
        (62, 35_651_584, 4_706_009_088),
    ];
    let picture = match width as u64 * height as u64 {
        0 => 1920 * 1080,
        p => p,
    };
    let samples = picture as f64 * frame_rate.filter(|f| *f > 0.0).unwrap_or(60.0);
    LEVELS.iter().find(|(_, size, rate)| picture <= *size && samples <= *rate as f64).map_or(62, |l| l.0)
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

    fn skip(&mut self, n: usize) -> Option<()> {
        self.pos += n;
        (self.pos <= self.b.len() * 8).then_some(())
    }

    /// AV1's `uvlc()`, which is also H.265's `ue(v)` — and as long as its `se(v)`, for skipping one.
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

/// An HEVC track's codec string, whether it is HDR and whether that HDR is HLG, from its `hvcC` record and what the
/// container says of its colours. Where the container is silent, the VUI of the record's SPS speaks instead: a muxer
/// that writes no Matroska Colour element or MP4 `colr` box still carries that, and a UHD Blu-ray remux often writes
/// neither — its HDR10 goes out named SDR, which Safari refuses outright.
pub fn hevc_track(hvcc: &[u8], container: Colour) -> (Option<String>, bool, bool) {
    let colour = hevc_colour(hvcc, container);
    (hevc_codecs(hvcc), is_hdr(colour.transfer, colour.primaries, colour.matrix), colour.transfer == 18)
}

/// The container's HEVC colour description completed by its SPS VUI. Kept separate from the display flags because
/// a malformed Dolby Vision Profile 5 record is contradictory only for BT.2020 YCbCr with PQ or HLG specifically.
pub(crate) fn hevc_colour(hvcc: &[u8], container: Colour) -> Colour {
    container.or(hevc_sps_colour(hvcc).unwrap_or_default())
}

/// The colours the first SPS among an `hvcC` record's parameter-set arrays (ISO/IEC 14496-15 8.3.3.1) describes,
/// walked field by field to the VUI's colour description (H.265 7.3.2.2, E.2.1). `None` without an SPS, or with one
/// that ends early.
fn hevc_sps_colour(hvcc: &[u8]) -> Option<Colour> {
    let mut at = 23;
    for _ in 0..*hvcc.get(22)? {
        let kind = hvcc.get(at)? & 0x3f;
        let count = u16::from_be_bytes([*hvcc.get(at + 1)?, *hvcc.get(at + 2)?]);
        at += 3;
        for _ in 0..count {
            let len = u16::from_be_bytes([*hvcc.get(at)?, *hvcc.get(at + 1)?]) as usize;
            let nal = hvcc.get(at + 2..at + 2 + len)?;
            if kind == 33 {
                // Past the two-byte NAL unit header.
                return sps_colour(&unescape(nal.get(2..)?));
            }
            at += 2 + len;
        }
    }
    None
}

/// A NAL unit's payload without its emulation prevention bytes: the `03` of every `00 00 03`.
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn sps_colour(rbsp: &[u8]) -> Option<Colour> {
    let mut r = Bits { b: rbsp, pos: 0 };
    r.read(4)?; // sps_video_parameter_set_id
    let sub_layers = r.read(3)? as usize;
    r.read(1)?; // sps_temporal_id_nesting_flag

    // profile_tier_level: the general profile (88 bits) and level (8), then each sub-layer's where it has them.
    r.skip(88 + 8)?;
    let mut present = [(false, false); 7];
    for p in present.iter_mut().take(sub_layers) {
        *p = (r.flag()?, r.flag()?);
    }
    if sub_layers > 0 {
        r.skip(2 * (8 - sub_layers))?;
    }
    for &(profile, level) in &present[..sub_layers] {
        r.skip(if profile { 88 } else { 0 } + if level { 8 } else { 0 })?;
    }
    r.uvlc()?; // sps_seq_parameter_set_id
    if r.uvlc()? == 3 {
        r.read(1)?; // separate_colour_plane_flag
    }
    r.uvlc()?; // pic_width_in_luma_samples
    r.uvlc()?; // pic_height_in_luma_samples
    if r.flag()? {
        for _ in 0..4 {
            r.uvlc()?; // conformance window offsets
        }
    }
    r.uvlc()?; // bit_depth_luma_minus8
    r.uvlc()?; // bit_depth_chroma_minus8
    let poc_lsb_bits = r.uvlc()? as u32 + 4;
    let ordering_from = if r.flag()? { 0 } else { sub_layers };
    for _ in ordering_from..=sub_layers {
        for _ in 0..3 {
            r.uvlc()?; // max_dec_pic_buffering, max_num_reorder_pics, max_latency_increase
        }
    }
    for _ in 0..6 {
        r.uvlc()?; // coding and transform block sizes, transform hierarchy depths
    }
    if r.flag()? && r.flag()? {
        scaling_list_data(&mut r)?;
    }
    r.read(2)?; // amp_enabled_flag, sample_adaptive_offset_enabled_flag
    if r.flag()? {
        r.read(8)?; // PCM sample bit depths
        r.uvlc()?;
        r.uvlc()?; // PCM coding block sizes
        r.read(1)?; // pcm_loop_filter_disabled_flag
    }
    let sets = r.uvlc()?;
    if sets > 64 {
        return None;
    }
    let mut rps = Vec::new();
    for idx in 0..sets as usize {
        let set = short_term_ref_pic_set(&mut r, &rps, idx)?;
        rps.push(set);
    }
    if r.flag()? {
        let long_term = r.uvlc()?;
        if long_term > 32 {
            return None;
        }
        for _ in 0..long_term {
            r.read(poc_lsb_bits + 1)?; // lt_ref_pic_poc_lsb_sps, used_by_curr_pic_lt_sps_flag
        }
    }
    r.read(2)?; // sps_temporal_mvp_enabled_flag, strong_intra_smoothing_enabled_flag
    let mut colour = Colour::default();
    if !r.flag()? {
        return Some(colour); // no VUI
    }
    if r.flag()? && r.read(8)? == 255 {
        r.read(32)?; // an extended sample aspect ratio's width and height
    }
    if r.flag()? {
        r.read(1)?; // overscan_appropriate_flag
    }
    if r.flag()? {
        r.read(3)?; // video_format
        colour.full_range = Some(r.flag()?);
        if r.flag()? {
            colour.primaries = r.read(8)?;
            colour.transfer = r.read(8)?;
            colour.matrix = r.read(8)?;
        }
    }
    Some(colour)
}

/// Past `scaling_list_data()` (H.265 7.3.4).
fn scaling_list_data(r: &mut Bits<'_>) -> Option<()> {
    for size in 0..4usize {
        for _ in (0..6).step_by(if size == 3 { 3 } else { 1 }) {
            if !r.flag()? {
                r.uvlc()?; // scaling_list_pred_matrix_id_delta
                continue;
            }
            if size > 1 {
                r.uvlc()?; // scaling_list_dc_coef_minus8
            }
            for _ in 0..64.min(1usize << (4 + 2 * size)) {
                r.uvlc()?; // scaling_list_delta_coef
            }
        }
    }
    Some(())
}

/// Reads `st_ref_pic_set(idx)` (H.265 7.3.7) into its pictures' POC deltas, negative and positive, in the order 7.4.8
/// derives them: a later set predicted from this one carries a flag for each.
fn short_term_ref_pic_set(
    r: &mut Bits<'_>,
    sets: &[(Vec<i64>, Vec<i64>)],
    idx: usize,
) -> Option<(Vec<i64>, Vec<i64>)> {
    if idx > 0 && r.flag()? {
        // Predicted from the set before it: an SPS's sets name no delta_idx.
        let (s0, s1) = sets.get(idx - 1)?;
        let negative_delta = r.flag()?;
        let magnitude = r.uvlc()? as i64 + 1;
        let delta = if negative_delta { -magnitude } else { magnitude };
        // use_delta_flag, which used_by_curr_pic_flag implies; the last is the reference picture itself.
        let mut used = Vec::with_capacity(s0.len() + s1.len() + 1);
        for _ in 0..=s0.len() + s1.len() {
            used.push(r.flag()? || r.flag()?);
        }
        let own = used[s0.len() + s1.len()];
        let at0 = |i: usize| used[i];
        let at1 = |i: usize| used[s0.len() + i];
        let mut negative = Vec::new();
        negative.extend(
            s1.iter().enumerate().rev().filter(|&(i, d)| d + delta < 0 && at1(i)).map(|(_, d)| d + delta),
        );
        negative.extend(Some(delta).filter(|d| *d < 0 && own));
        negative
            .extend(s0.iter().enumerate().filter(|&(i, d)| d + delta < 0 && at0(i)).map(|(_, d)| d + delta));
        let mut positive = Vec::new();
        positive.extend(
            s0.iter().enumerate().rev().filter(|&(i, d)| d + delta > 0 && at0(i)).map(|(_, d)| d + delta),
        );
        positive.extend(Some(delta).filter(|d| *d > 0 && own));
        positive
            .extend(s1.iter().enumerate().filter(|&(i, d)| d + delta > 0 && at1(i)).map(|(_, d)| d + delta));
        return Some((negative, positive));
    }
    let (negatives, positives) = (r.uvlc()?, r.uvlc()?);
    if negatives > 16 || positives > 16 {
        return None;
    }
    let mut deltas = |n: u64, sign: i64| -> Option<Vec<i64>> {
        let mut poc = 0;
        (0..n)
            .map(|_| {
                poc += sign * (r.uvlc()? as i64 + 1);
                r.read(1)?; // used_by_curr_pic_s0/s1_flag
                Some(poc)
            })
            .collect()
    };
    let negative = deltas(negatives, -1)?;
    let positive = deltas(positives, 1)?;
    Some((negative, positive))
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

/// Why `probe` refuses a head that starts neither container.
pub const NEITHER: &str = "neither Matroska nor MP4";

/// Probe a file whose first bytes are `head`, dispatching on its magic rather than its name.
pub async fn probe(src: &Source<'_>, head: &[u8]) -> Result<MediaInfo, ProbeError> {
    if head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return mkv::probe(src, head).await;
    }
    if head.len() >= 8 && matches!(&head[4..8], b"ftyp" | b"moov" | b"free" | b"mdat" | b"wide" | b"skip") {
        return mp4::probe(src, head).await;
    }
    Err(ProbeError::Unsupported(NEITHER.into()))
}

/// `avc1.PPCCLL` from an `avcC` record: profile, constraint flags, level.
pub fn avc_codecs(avcc: &[u8]) -> Option<String> {
    (avcc.len() >= 4 && avcc[0] == 1).then(|| format!("avc1.{:02x}{:02x}{:02x}", avcc[1], avcc[2], avcc[3]))
}

/// The profile, level and tier an RFC 6381 string names: `avc1.640033` is (100, 51, false), level 5.1 as
/// `level_idc`; `hvc1.2.4.H153.B0` is (2, 153, true), Main 10 at level 5.1 (`general_level_idc`, level × 30) in
/// HEVC's High tier; `av01.0.13M.10` is (0, 13, false), Main profile at `seq_level_idx` 13 (level 5.1), Main tier;
/// `vp09.02.51.10` is (2, 51, false), profile 2 at level 5.1 (level × 10), which has no tiers.
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
        "vp09" => Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, false)),
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
    fn an_idr_is_told_from_an_open_gops_keyframe_by_its_first_picture() {
        // 4-byte lengths: an avcC's byte 4, an hvcC's byte 21, low two bits 3.
        let avcc = [1, 0x64, 0, 0x28, 0xff];
        let mut hvcc = [0u8; 23];
        hvcc[21] = 3;
        let frame = |units: &[&[u8]]| {
            units
                .iter()
                .flat_map(|u| (u.len() as u32).to_be_bytes().into_iter().chain(u.iter().copied()))
                .collect()
        };
        // H.264: an SEI (6) first is passed over; an IDR slice (5) is closed, a plain I slice (1) is not.
        let idr: Vec<u8> = frame(&[&[0x06, 1], &[0x65, 0x88]]);
        assert_eq!(idr_keyframe(&VideoCodec::H264, &avcc, &idr), Some(true));
        assert_eq!(idr_keyframe(&VideoCodec::H264, &avcc, &frame(&[&[0x06, 1], &[0x41, 0x9a]])), Some(false));
        // HEVC: VPS/SEI (32, 39) passed over; IDR_W_RADL (19) and IDR_N_LP (20) closed, CRA (21) not.
        for (header, closed) in [(19u8 << 1, true), (20 << 1, true), (21 << 1, false), (16 << 1, false)] {
            let f = frame(&[&[32 << 1, 1], &[39 << 1, 1], &[header, 1]]);
            assert_eq!(idr_keyframe(&VideoCodec::Hevc, &hvcc, &f), Some(closed), "type {}", header >> 1);
        }
        assert_eq!(idr_keyframe(&VideoCodec::Hevc, &hvcc, &frame(&[&[32 << 1, 1]])), None, "no picture");
        assert_eq!(idr_keyframe(&VideoCodec::Av1, &hvcc, &idr), None);
        assert_eq!(idr_keyframe(&VideoCodec::Hevc, &[], &idr), None, "no configuration record");
    }

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
        assert_eq!(profile_level("vp09.02.51.10.01.09.16.09.00"), Some((2, 51, false)));
        assert_eq!(profile_level("vp09.00"), None);
    }

    #[test]
    fn a_vp9_stream_is_named_by_what_it_records_and_the_rest_is_assumed() {
        // ffmpeg 8.0's own vpcC boxes for 640 × 360 at 24 fps: profile 0, 8-bit, unspecified colours; and profile 2,
        // 10-bit, with only BT.2020's matrix.
        let p0 = vpcc_config(&unhex("010000000015820202020000")).unwrap();
        let unspecified = Colour { primaries: 2, transfer: 2, matrix: 2, full_range: Some(false) };
        let recorded = Vp9Config {
            profile: Some(0),
            level: Some(21),
            depth: Some(8),
            chroma: Some(1),
            colour: unspecified,
        };
        assert_eq!(p0, recorded);
        assert_eq!(vp9_track(p0, Colour::default(), 640, 360, None), ("vp09.00.21.08".into(), false, false));
        let p2 = vpcc_config(&unhex("010000000215a20202090000")).unwrap();
        assert_eq!(vp9_track(p2, Colour::default(), 640, 360, None).0, "vp09.02.21.10.01.02.02.09.00");
        let pq = Colour { primaries: 9, transfer: 16, matrix: 9, full_range: Some(false) };
        assert_eq!(vp9_track(p2, pq, 640, 360, None), ("vp09.02.21.10.01.09.16.09.00".into(), true, false));
        assert!(!vp9_track(p0, pq, 640, 360, None).1, "PQ in profile 0 is no HDR VP9 has");
        assert_eq!(vpcc_config(&[1, 0, 0, 0, 2, 51]), None, "truncated");
        let v0 = vpcc_config(&[0, 0, 0, 0, 2, 0, 0xa0]).unwrap();
        assert_eq!((v0.profile, v0.level, v0.depth, v0.chroma), (Some(2), None, Some(10), None), "version 0");

        let features = vp9_features(&[1, 1, 2, 2, 1, 51, 3, 1, 10, 4, 1, 1]);
        assert_eq!(
            (features.profile, features.level, features.depth, features.chroma),
            (Some(2), Some(51), Some(10), Some(1))
        );
        assert_eq!(vp9_features(&[1, 1]), Vp9Config::default(), "truncated");
        // Nothing recorded, as in most Matroska: 8-bit profile 0 at the level the picture needs, or 10-bit profile 2
        // where the transfer is HDR's.
        let none = Vp9Config::default();
        assert_eq!(vp9_track(none, Colour::default(), 1920, 1080, Some(24.0)).0, "vp09.00.40.08");
        assert_eq!(
            vp9_track(none, Colour::default(), 1920, 1080, None).0,
            "vp09.00.41.08",
            "60 fps when unknown"
        );
        assert_eq!(
            vp9_track(none, Colour::default(), 0, 0, Some(24.0)).0,
            "vp09.00.40.08",
            "an unknown size is 1080p"
        );
        let hlg = Colour { primaries: 9, transfer: 18, matrix: 9, full_range: None };
        assert_eq!(
            vp9_track(none, hlg, 3840, 2160, Some(24.0)),
            ("vp09.02.50.10.01.09.18.09.00".into(), true, true)
        );
        assert_eq!(vp9_level(3840, 2160, Some(60.0)), 51);
        assert_eq!(vp9_level(640, 360, Some(24.0)), 21, "as ffmpeg's muxer works it out");
        assert_eq!(vp9_level(15360, 8640, Some(120.0)), 62, "past every level");
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
        assert_eq!((av1_bit_depth(&hdr10), av1_video_range(&hdr10)), (Some(10), Some("PQ")));
        assert_eq!(av1_video_range("av01.0.13M.10.0.110.09.18.09.0"), Some("HLG"));
        assert_eq!((av1_bit_depth("av01.0.08M.08"), av1_video_range("av01.0.08M.08")), (Some(8), None));
        assert_eq!(av1_bit_depth("hvc1.2.4.L150.B0"), None);
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

    /// An `hvcC` record (Main 10, level 5.1) holding one SPS NAL unit, its payload escaped as an encoder would.
    fn hvcc_with_sps(rbsp: &[u8]) -> Vec<u8> {
        let mut nal = vec![0x42, 0x01];
        let mut zeros = 0;
        for &b in rbsp {
            if zeros >= 2 && b <= 3 {
                nal.push(3);
                zeros = 0;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            nal.push(b);
        }
        let mut record = vec![
            1, 0x02, 0x20, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 153, 0xf0, 0, 0xfc, 0xfd, 0xfa, 0xfa, 0, 0, 0x0f, 1,
        ];
        record.extend([0x80 | 33, 0, 1]);
        record.extend((nal.len() as u16).to_be_bytes());
        record.extend(nal);
        record
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn an_hevc_sps_speaks_for_a_silent_container() {
        // The Hobbit's UHD Blu-ray x265 encode, whose Matroska header names no colours; the SPS is as its hvcC
        // carries it, emulation prevention and all.
        let hobbit = unhex(
            "420101022000000300900000030000030099a001e0200649365959a4930bc05a848804db0800001f480002ee030526b2f000044aa20000895444",
        );
        let record = [
            &hvcc_with_sps(&[])[..hvcc_with_sps(&[]).len() - 4],
            &(hobbit.len() as u16).to_be_bytes(),
            &hobbit,
        ]
        .concat();
        assert_eq!(hevc_sps_colour(&record).map(|c| (c.primaries, c.transfer, c.matrix)), Some((9, 16, 9)));
        assert_eq!(hevc_track(&record, Colour::default()), (Some("hvc1.2.4.L153.90".into()), true, false));
        let sdr = Colour { primaries: 1, transfer: 1, matrix: 1, full_range: Some(false) };
        assert!(!hevc_track(&record, sdr).1, "the container says otherwise, and wins");
        let cut = [&record[..record.len() - hobbit.len() + 20]].concat();
        assert_eq!(hevc_sps_colour(&cut), None, "an SPS cut short says nothing");
        assert!(!hevc_track(&cut, Colour::default()).1);
        // libx265's own SPS for an SDR encode, which names no colours.
        let sdr_sps =
            unhex("420101022000000300900000030000030096a001e0200649365959a4932bc05a020000030002000003003010");
        let sdr_record =
            [&record[..record.len() - hobbit.len() - 2], &(sdr_sps.len() as u16).to_be_bytes(), &sdr_sps]
                .concat();
        assert_eq!(
            hevc_track(&sdr_record, Colour::default()),
            (Some("hvc1.2.4.L153.90".into()), false, false)
        );
        assert_eq!(hevc_sps_colour(&[1, 2, 3]), None, "no arrays");
    }

    #[test]
    fn an_hevc_sps_is_walked_past_every_optional_part() {
        let ue = |v: u64| (v + 1, 2 * (64 - (v + 1).leading_zeros()) - 1);
        #[rustfmt::skip]
        let mut f: Vec<(u64, u32)> = vec![
            (0, 4), (1, 3), (1, 1),                   // two sub-layers
            (0x0220_0000_0090_0000, 64), (153, 32),   // general profile_tier_level
            (1, 1), (1, 1), (0, 14),                  // sub-layer 0 has a profile and a level; reserved bits
            (0x0220_0000_0090_0000, 64), (150, 32),
            ue(0), ue(1), ue(3839), ue(2159),         // SPS 0, 4:2:0, size
            (1, 1), ue(0), ue(0), ue(0), ue(12),      // conformance window
            ue(2), ue(2), ue(4),                      // 10-bit, 8-bit POC LSBs
            (0, 1), ue(4), ue(2), ue(0),              // ordering info for the top sub-layer only
            ue(0), ue(3), ue(0), ue(3), ue(1), ue(1),
            (1, 1), (1, 1),                           // scaling lists, sent
        ];
        for size in 0..4u32 {
            for matrix in (0..6u64).step_by(if size == 3 { 3 } else { 1 }) {
                if matrix > 0 {
                    f.extend([(0, 1), ue(matrix)]);
                    continue;
                }
                f.push((1, 1));
                if size > 1 {
                    f.push(ue(7));
                }
                f.extend((0..64.min(1 << (4 + 2 * size))).map(|_| ue(2)));
            }
        }
        #[rustfmt::skip]
        f.extend([
            (0, 1), (1, 1),                           // amp, SAO
            (1, 1), (9, 4), (9, 4), ue(0), ue(2), (1, 1), // PCM
            ue(3),                                    // three short-term sets
            ue(2), ue(1), ue(0), (1, 1), ue(1), (1, 1), ue(0), (1, 1), // 0: −1, −3 and +1
            // 1: predicted from 0 by −1 — a flag each for −1, −3, +1 and the picture itself: −1, −2, −4, nothing after.
            (1, 1), (1, 1), ue(0), (1, 1), (0, 1), (1, 1), (0, 1), (0, 1), (1, 1),
            (1, 1), (0, 1), ue(1), (1, 1), (1, 1), (1, 1), (1, 1), // 2: predicted from 1's three by +2, so four flags
            (1, 1), ue(2), (5, 8), (1, 1), (9, 8), (0, 1), // two long-term pictures
            (1, 1), (1, 1),                           // temporal MVP, strong intra smoothing
            (1, 1),                                   // VUI
            (1, 1), (255, 8), (4, 16), (3, 16),       // extended sample aspect ratio
            (1, 1), (0, 1),                           // overscan
            (1, 1), (5, 3), (0, 1), (1, 1), (9, 8), (18, 8), (9, 8), // HLG
            (0, 8),
        ]);
        let colour = hevc_sps_colour(&hvcc_with_sps(&pack(&f)));
        assert_eq!(colour, Some(Colour { primaries: 9, transfer: 18, matrix: 9, full_range: Some(false) }));
        assert_eq!(hevc_track(&hvcc_with_sps(&pack(&f)), Colour::default()).1..=true, true..=true);
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
