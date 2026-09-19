//! Matroska: EBML header → Segment → SeekHead, Info, Tracks, and the Cues the keyframes come from.
//!
//! Top-level elements are WALKED, never searched for: a SeekHead stores the ids of the elements it
//! points at as data, so scanning for the four bytes of an id finds the pointer before the element
//! (den-scout's probe learned that the hard way). What the walk of the head does not reach — Cues are
//! usually written at the end — comes through the SeekHead, one ranged read per element.

use super::{AudioTrack, MediaInfo, ProbeError, Source, VideoCodec, MAX_ELEMENT};

const EBML: u32 = 0x1A45DFA3;
const SEGMENT: u32 = 0x18538067;
const SEEK_HEAD: u32 = 0x114D9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549A966;
const TIMESTAMP_SCALE: u32 = 0x2AD7B1;
const DURATION: u32 = 0x4489;
const TRACKS: u32 = 0x1654AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const DEFAULT_DURATION: u32 = 0x23E383;
const LANGUAGE: u32 = 0x22B59C;
const LANGUAGE_BCP47: u32 = 0x22B59D;
const NAME: u32 = 0x536E;
const FLAG_DEFAULT: u32 = 0x88;
const FLAG_COMMENTARY: u32 = 0x55AF;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const COLOUR: u32 = 0x55B0;
const TRANSFER_CHARACTERISTICS: u32 = 0x55BA;
const PRIMARIES: u32 = 0x55BB;
const MATRIX_COEFFICIENTS: u32 = 0x55B1;
const RANGE: u32 = 0x55B9;
const BITS_PER_CHANNEL: u32 = 0x55B2;
const AUDIO: u32 = 0xE1;
const CHANNELS: u32 = 0x9F;
const BLOCK_ADDITION_MAPPING: u32 = 0x41E4;
const BLOCK_ADD_ID_TYPE: u32 = 0x41E7;
const BLOCK_ADD_ID_EXTRA_DATA: u32 = 0x41ED;
/// The block-addition types that carry a Dolby Vision configuration record.
const DOVI_TYPES: [&[u8; 4]; 3] = [b"dvcC", b"dvvC", b"dvwC"];
const CUES: u32 = 0x1C53BB6B;
const CUE_POINT: u32 = 0xBB;
const CUE_TIME: u32 = 0xB3;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const CUE_TRACK: u32 = 0xF7;
const CUE_CLUSTER_POSITION: u32 = 0xF1;
const CLUSTER: u32 = 0x1F43B675;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
/// A 4K keyframe is ordinarily much smaller. This bounds the one extra range read made only for a contradictory P5.
const RPU_AUDIT_BYTES: u64 = 16 * 1024 * 1024;

/// An element header: id, payload size (`None` = EBML's "unknown size"), and the header's length.
pub(crate) fn header(b: &[u8]) -> Option<(u32, Option<u64>, usize)> {
    let first = *b.first()?;
    let n = first.leading_zeros() as usize + 1;
    if n > 4 || b.len() < n {
        return None;
    }
    let id = b[..n].iter().fold(0u32, |acc, &c| acc << 8 | c as u32);
    let s = *b.get(n)?;
    let m = s.leading_zeros() as usize + 1;
    if m > 8 || b.len() < n + m {
        return None;
    }
    // Widened before shifting: an 8-byte size leaves no value bits in the first byte, and a u8
    // shifted by 8 overflows.
    let mask = (0xFFu32 >> m) as u8;
    let mut v = (s & mask) as u64;
    let mut unknown = s & mask == mask;
    for &c in &b[n + 1..n + m] {
        v = v << 8 | c as u64;
        unknown &= c == 0xFF;
    }
    Some((id, (!unknown).then_some(v), n + m))
}

/// The children of a master element's payload, as `(id, payload)`. Stops at the first malformed or
/// truncated child rather than guessing past it.
fn children(b: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        let (id, size, hl) = header(b.get(pos..)?)?;
        let start = pos + hl;
        let end = start.checked_add(usize::try_from(size?).ok()?)?;
        if end > b.len() {
            return None;
        }
        pos = end;
        Some((id, &b[start..end]))
    })
}

fn child(b: &[u8], want: u32) -> Option<&[u8]> {
    children(b).find(|(id, _)| *id == want).map(|(_, body)| body)
}

fn uint(b: &[u8]) -> u64 {
    b.iter().take(8).fold(0u64, |acc, &c| acc << 8 | c as u64)
}

fn float(b: &[u8]) -> Option<f64> {
    match b.len() {
        4 => Some(f32::from_be_bytes(b.try_into().ok()?) as f64),
        8 => Some(f64::from_be_bytes(b.try_into().ok()?)),
        _ => None,
    }
}

fn string(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim_end_matches('\0').trim().to_string()
}

#[derive(Default)]
struct Track {
    number: u64,
    kind: u64,
    codec_id: String,
    private: Vec<u8>,
    /// Nanoseconds a frame; 0 where the muxer wrote none.
    default_duration: u64,
    language: Option<String>,
    name: Option<String>,
    default: bool,
    commentary: bool,
    width: u32,
    height: u32,
    transfer: u64,
    primaries: u64,
    matrix: u64,
    /// Matroska's Range: 1 broadcast, 2 full; 0 (and 3, "defined by the matrix and transfer") says neither.
    range: u64,
    /// The Colour element's BitsPerChannel; 0 where it says nothing.
    bits: u8,
    channels: u32,
    dovi: Option<super::DolbyVision>,
}

fn parse_tracks(b: &[u8]) -> Vec<Track> {
    children(b)
        .filter(|(id, _)| *id == TRACK_ENTRY)
        .map(|(_, entry)| {
            // FlagDefault defaults to on: a track is a default unless its muxer said otherwise.
            let mut t = Track { channels: 1, default: true, ..Track::default() };
            let mut bcp47 = None;
            for (id, v) in children(entry) {
                match id {
                    TRACK_NUMBER => t.number = uint(v),
                    TRACK_TYPE => t.kind = uint(v),
                    CODEC_ID => t.codec_id = string(v),
                    CODEC_PRIVATE => t.private = v.to_vec(),
                    DEFAULT_DURATION => t.default_duration = uint(v),
                    LANGUAGE => t.language = Some(string(v)),
                    LANGUAGE_BCP47 => bcp47 = Some(string(v)),
                    NAME => t.name = Some(string(v)).filter(|n| !n.is_empty()),
                    FLAG_DEFAULT => t.default = uint(v) != 0,
                    FLAG_COMMENTARY => t.commentary = uint(v) != 0,
                    VIDEO => {
                        t.width = child(v, PIXEL_WIDTH).map(uint).unwrap_or(0) as u32;
                        t.height = child(v, PIXEL_HEIGHT).map(uint).unwrap_or(0) as u32;
                        let colour = child(v, COLOUR);
                        let of = |id| colour.and_then(|c| child(c, id)).map(uint).unwrap_or(0);
                        t.transfer = of(TRANSFER_CHARACTERISTICS);
                        t.primaries = of(PRIMARIES);
                        t.matrix = of(MATRIX_COEFFICIENTS);
                        t.range = of(RANGE);
                        t.bits = of(BITS_PER_CHANNEL).min(255) as u8;
                    }
                    AUDIO => t.channels = child(v, CHANNELS).map(uint).unwrap_or(1) as u32,
                    BLOCK_ADDITION_MAPPING => {
                        let kind = child(v, BLOCK_ADD_ID_TYPE).map(uint);
                        if DOVI_TYPES.iter().any(|d| kind == Some(u32::from_be_bytes(**d) as u64)) {
                            t.dovi = child(v, BLOCK_ADD_ID_EXTRA_DATA).and_then(super::dovi_config);
                        }
                    }
                    _ => {}
                }
            }
            // BCP-47 wins where both exist: it is the field a modern muxer fills in. An absent tag is
            // left unknown rather than taken as the spec's default "eng" — in practice it means nobody
            // wrote one down, not that the audio is English.
            t.language = bcp47.or(t.language).filter(|l| !l.is_empty() && l != "und");
            t
        })
        .collect()
}

/// What a track's Colour element says.
fn colour(t: &Track) -> super::Colour {
    super::Colour {
        primaries: t.primaries,
        transfer: t.transfer,
        matrix: t.matrix,
        full_range: match t.range {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        },
    }
}

#[derive(Clone, Copy)]
struct Cue {
    time: u64,
    cluster: u64,
}

/// CueTimes and cluster positions (relative to the Segment payload) that index `track`.
fn parse_cues(b: &[u8], track: u64) -> Vec<Cue> {
    children(b)
        .filter(|(id, _)| *id == CUE_POINT)
        .filter_map(|(_, point)| {
            let time = child(point, CUE_TIME).map(uint)?;
            children(point).filter(|(id, _)| *id == CUE_TRACK_POSITIONS).find_map(|(_, pos)| {
                if child(pos, CUE_TRACK).map(uint) != Some(track) {
                    return None;
                }
                Some(Cue { time, cluster: child(pos, CUE_CLUSTER_POSITION).map(uint)? })
            })
        })
        .collect()
}

/// The first frame in an unlaced Matroska Block for `track`.
fn block_frame(block: &[u8], track: u64) -> Option<&[u8]> {
    let first = *block.first()?;
    let n = first.leading_zeros() as usize + 1;
    if n > 8 || block.len() < n + 3 {
        return None;
    }
    let mask = (0xFFu16 >> n) as u8;
    let number = block[..n]
        .iter()
        .enumerate()
        .fold(0u64, |v, (i, b)| v << 8 | if i == 0 { (b & mask) as u64 } else { *b as u64 });
    let flags = block[n + 2];
    (number == track && flags & 0x06 == 0).then_some(&block[n + 3..])
}

/// The first UNSPEC62 NAL in a frame whose NAL units use the length size from `hvcC`.
fn rpu_nal<'a>(frame: &'a [u8], hvcc: &[u8]) -> Option<&'a [u8]> {
    let length_size = (*hvcc.get(21)? & 3) as usize + 1;
    let mut at = 0;
    while at + length_size <= frame.len() {
        let len = frame[at..at + length_size].iter().fold(0usize, |n, b| n << 8 | *b as usize);
        at += length_size;
        let nal = frame.get(at..at.checked_add(len)?)?;
        if nal.first().is_some_and(|b| (b >> 1) & 0x3f == 62) {
            return Some(nal);
        }
        at += len;
    }
    None
}

/// Walk a possibly truncated Cluster/BlockGroup prefix. A block can declare more bytes than the bounded audit read;
/// its available prefix is still enough when the RPU occurs before the cutoff.
fn rpu_from_elements<'a>(b: &'a [u8], track: u64, hvcc: &[u8], nested: bool) -> Option<&'a [u8]> {
    let mut at = 0;
    while let Some((id, size, hl)) = b.get(at..).and_then(header) {
        let size = usize::try_from(size?).ok()?;
        let start = at.checked_add(hl)?;
        let declared_end = start.checked_add(size)?;
        let end = declared_end.min(b.len());
        let body = b.get(start..end)?;
        let frame = match id {
            SIMPLE_BLOCK if !nested => block_frame(body, track),
            BLOCK if nested => block_frame(body, track),
            _ => None,
        };
        if let Some(nal) = frame.and_then(|f| rpu_nal(f, hvcc)) {
            return Some(nal);
        }
        if id == BLOCK_GROUP && !nested {
            if let Some(nal) = rpu_from_elements(body, track, hvcc, true) {
                return Some(nal);
            }
        }
        if declared_end > b.len() {
            break;
        }
        at = declared_end;
    }
    None
}

async fn first_rpu_profile(
    src: &Source<'_>,
    cluster_at: u64,
    track: u64,
    hvcc: &[u8],
) -> Result<Option<u8>, ProbeError> {
    let bytes = src.read(cluster_at, RPU_AUDIT_BYTES).await?;
    let (id, _, hl) = header(&bytes).ok_or(ProbeError::Truncated("first Cluster"))?;
    if id != CLUSTER {
        return Err(ProbeError::Unsupported("the first video Cue points outside a Cluster".into()));
    }
    Ok(rpu_from_elements(&bytes[hl..], track, hvcc, false)
        .and_then(|nal| dolby_vision::rpu::dovi_rpu::DoviRpu::parse_unspec62_nalu(nal).ok())
        .map(|rpu| rpu.dovi_profile))
}

/// A genuine Profile 5 uses IPT-PQ-c2, which has no HEVC VUI code point. BT.2020 YCbCr plus PQ/HLG therefore proves
/// the container record is wrong before any packet is read (the exact gate used by AetherEngine #532).
fn contradictory_profile5(dv: Option<super::DolbyVision>, colour: super::Colour) -> bool {
    dv.is_some_and(|d| d.profile == 5 && d.compat == 0)
        && colour.primaries == 9
        && colour.matrix == 9
        && matches!(colour.transfer, 16 | 18)
}

fn parse_seek_head(b: &[u8]) -> Vec<(u32, u64)> {
    children(b)
        .filter(|(id, _)| *id == SEEK)
        .filter_map(|(_, seek)| {
            let id = child(seek, SEEK_ID)?;
            Some((uint(id) as u32, uint(child(seek, SEEK_POSITION)?)))
        })
        .collect()
}

/// Read the element the SeekHead says is at `abs`, checking it is the one we were promised.
async fn fetch(src: &Source<'_>, abs: u64, want: u32) -> Result<Vec<u8>, ProbeError> {
    let hdr = src.read(abs, 12).await?;
    let (id, size, hl) = header(&hdr).ok_or(ProbeError::Truncated("element header"))?;
    if id != want {
        return Err(ProbeError::Unsupported(format!("the SeekHead points at {id:#x}, not {want:#x}")));
    }
    let size = size.ok_or_else(|| ProbeError::Unsupported("an index element of unknown size".into()))?;
    if size > MAX_ELEMENT {
        return Err(ProbeError::Unsupported(format!("a {size}-byte index element")));
    }
    let body = src.read(abs + hl as u64, size).await?;
    if (body.len() as u64) < size {
        return Err(ProbeError::Truncated("index element"));
    }
    Ok(body)
}

pub async fn probe(src: &Source<'_>, head: &[u8]) -> Result<MediaInfo, ProbeError> {
    let (id, size, hl) = header(head).ok_or(ProbeError::Truncated("EBML header"))?;
    if id != EBML {
        return Err(ProbeError::Unsupported("not EBML".into()));
    }
    let pos = hl + size.ok_or(ProbeError::Truncated("EBML header"))? as usize;
    let (id, _, hl) = head.get(pos..).and_then(header).ok_or(ProbeError::Truncated("Segment"))?;
    if id != SEGMENT {
        return Err(ProbeError::Unsupported("no Segment after the EBML header".into()));
    }
    let seg_start = (pos + hl) as u64;

    let (mut info, mut tracks, mut cues) = (None, None, None);
    let mut seeks: Vec<(u32, u64)> = Vec::new();
    let mut p = seg_start as usize;
    while let Some((id, size, hl)) = head.get(p..).and_then(header) {
        // An unknown-size element is a cluster being streamed, and nothing past a cluster is
        // findable by walking; neither is anything past the bytes we read. The SeekHead covers both.
        let Some(size) = size else { break };
        let end = p + hl + size as usize;
        if id == CLUSTER || end > head.len() {
            break;
        }
        let body = &head[p + hl..end];
        match id {
            SEEK_HEAD => seeks.extend(parse_seek_head(body)),
            INFO => info = Some(body.to_vec()),
            TRACKS => tracks = Some(body.to_vec()),
            CUES => cues = Some(body.to_vec()),
            _ => {}
        }
        p = end;
    }

    let find = |seeks: &[(u32, u64)], id: u32| seeks.iter().find(|(i, _)| *i == id).map(|(_, p)| *p);
    if info.is_none() {
        if let Some(rel) = find(&seeks, INFO) {
            info = Some(fetch(src, seg_start + rel, INFO).await?);
        }
    }
    if tracks.is_none() {
        if let Some(rel) = find(&seeks, TRACKS) {
            tracks = Some(fetch(src, seg_start + rel, TRACKS).await?);
        }
    }
    // mkvmerge can leave the Cues out of the first SeekHead and list them in a second one at the end
    // of the file. Follow at most two of those; a loop of SeekHeads is a broken file.
    let mut followed: Vec<u64> = Vec::new();
    while cues.is_none() {
        if let Some(rel) = find(&seeks, CUES) {
            cues = Some(fetch(src, seg_start + rel, CUES).await?);
            break;
        }
        let next = seeks.iter().find(|(i, p)| *i == SEEK_HEAD && !followed.contains(p)).map(|(_, p)| *p);
        match next {
            Some(rel) if followed.len() < 2 => {
                followed.push(rel);
                seeks.extend(parse_seek_head(&fetch(src, seg_start + rel, SEEK_HEAD).await?));
            }
            _ => break,
        }
    }

    let info = info.ok_or(ProbeError::Truncated("Info"))?;
    let tracks = parse_tracks(&tracks.ok_or(ProbeError::Truncated("Tracks"))?);
    let cues = cues.ok_or_else(|| ProbeError::Unsupported("no keyframe index (Cues)".into()))?;

    let scale = child(&info, TIMESTAMP_SCALE).map(uint).filter(|s| *s > 0).unwrap_or(1_000_000) as f64;
    let secs = |t: f64| t * scale / 1e9;
    let duration = child(&info, DURATION)
        .and_then(float)
        .map(secs)
        .filter(|d| *d > 0.0)
        .ok_or_else(|| ProbeError::Unsupported("no duration".into()))?;
    let video = tracks
        .iter()
        .find(|t| t.kind == 1)
        .ok_or_else(|| ProbeError::Unsupported("no video track".into()))?;
    let parsed_cues = parse_cues(&cues, video.number);
    let mut keyframes: Vec<f64> = parsed_cues.iter().map(|c| secs(c.time as f64)).collect();
    keyframes.sort_by(f64::total_cmp);
    keyframes.dedup();
    if keyframes.is_empty() {
        return Err(ProbeError::Unsupported("the Cues index no video keyframes".into()));
    }
    let mut hdr = super::is_hdr(video.transfer, video.primaries, video.matrix);
    let mut hlg = video.transfer == 18;
    let mut hevc_colour = None;
    let (codec, codecs) = if video.codec_id.starts_with("V_MPEG4/ISO/AVC") {
        (VideoCodec::H264, super::avc_codecs(&video.private))
    } else if video.codec_id.starts_with("V_MPEGH/ISO/HEVC") {
        // V_MPEGH/ISO/HEVC's CodecPrivate is the `hvcC` record.
        let (codecs, hevc_hdr, hevc_hlg) = super::hevc_track(&video.private, colour(video));
        hevc_colour = Some(super::hevc_colour(&video.private, colour(video)));
        (hdr, hlg) = (hevc_hdr, hevc_hlg);
        (VideoCodec::Hevc, codecs)
    } else if video.codec_id == "V_AV1" {
        // V_AV1's CodecPrivate is the `av1C` record itself.
        let (codecs, av1_hdr) = super::av1_track(&video.private, colour(video));
        hdr = av1_hdr;
        (VideoCodec::Av1, codecs)
    } else if video.codec_id == "V_VP9" {
        // V_VP9's CodecPrivate, where there is one, is WebM's codec features; BitsPerChannel speaks for a depth they
        // don't record.
        let features = super::vp9_features(&video.private);
        let recorded =
            super::Vp9Config { depth: features.depth.or(Some(video.bits).filter(|b| *b > 0)), ..features };
        let rate = (video.default_duration > 0).then(|| 1e9 / video.default_duration as f64);
        let (codecs, vp9_hdr, vp9_hlg) =
            super::vp9_track(recorded, colour(video), video.width, video.height, rate);
        (hdr, hlg) = (vp9_hdr, vp9_hlg);
        (VideoCodec::Vp9, Some(codecs))
    } else {
        (VideoCodec::Other(video.codec_id.clone()), None)
    };
    let mut dolby_vision = video.dovi;
    let dolby_vision_record_mismatch = hevc_colour.is_some_and(|c| contradictory_profile5(dolby_vision, c));
    if dolby_vision_record_mismatch {
        let first_cluster = parsed_cues.iter().min_by_key(|c| c.time).map(|c| seg_start + c.cluster);
        let profile = match first_cluster {
            Some(at) => match first_rpu_profile(src, at, video.number, &video.private).await {
                Ok(profile) => profile,
                Err(e) => {
                    eprintln!("probe: Dolby Vision Profile 5 record contradicts the HEVC VUI; RPU audit failed: {e}");
                    None
                }
            },
            None => None,
        };
        if let Some(profile @ (7 | 8)) = profile {
            let old = dolby_vision.expect("the contradiction gate has a Dolby Vision record");
            dolby_vision =
                Some(super::DolbyVision { profile, compat: if hlg { 4 } else { 1 }, level: old.level });
            eprintln!(
                "probe: Dolby Vision Profile 5 record contradicted by its HEVC VUI and first RPU (profile {profile}); using the HDR base layer"
            );
        } else {
            eprintln!(
                "probe: Dolby Vision Profile 5 record contradicts the HEVC VUI; using the HDR base layer because the first RPU did not prove profile 7 or 8"
            );
        }
    }
    let audio = tracks
        .iter()
        .filter(|t| t.kind == 2)
        .map(|t| AudioTrack {
            codec: t.codec_id.clone(),
            language: t.language.clone(),
            channels: t.channels,
            name: t.name.clone(),
            default: t.default,
            // FlagCommentary is recent; most releases only say so in the track's title.
            commentary: t.commentary
                || t.name.as_deref().is_some_and(|n| n.to_ascii_lowercase().contains("commentary")),
        })
        .collect();
    Ok(MediaInfo {
        container: "matroska",
        duration,
        video: codec,
        codecs,
        width: video.width,
        height: video.height,
        hdr,
        hlg,
        frame_rate: (video.default_duration > 0).then(|| 1e9 / video.default_duration as f64),
        dolby_vision,
        dolby_vision_record_mismatch,
        audio,
        keyframes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn element_headers_decode_sizes_and_the_unknown_marker() {
        assert_eq!(header(&[0x1A, 0x45, 0xDF, 0xA3, 0x9F]), Some((EBML, Some(31), 5)));
        // A Segment of unknown size: all value bits of an 8-byte size set.
        let unknown = [0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(header(&unknown), Some((SEGMENT, None, 12)));
        assert_eq!(header(&[0x00]), None, "a zero byte is no valid id");
        assert_eq!(header(&[0x1A, 0x45]), None, "truncated");
    }

    #[test]
    fn a_seekhead_entry_is_data_not_an_element() {
        // A SeekHead pointing at Cues carries the Cues id as SeekID bytes; the walk must read it as a
        // pointer, not stop there thinking it found the Cues.
        let seek = [0x4D, 0xBB, 0x8B, 0x53, 0xAB, 0x84, 0x1C, 0x53, 0xBB, 0x6B, 0x53, 0xAC, 0x81, 0x40];
        assert_eq!(parse_seek_head(&seek), vec![(CUES, 0x40)]);
    }

    #[test]
    fn a_tracks_dolby_vision_record_is_read_from_its_block_addition_mapping() {
        let el = |id: &[u8], body: &[u8]| [id, &[0x80 | body.len() as u8], body].concat();
        let track = |kind: &[u8; 4]| {
            let record = [1, 0, 8 << 1, (6 << 3) | 0b101, 1 << 4, 0, 0, 0];
            let mapping = [el(&[0x41, 0xE7], kind), el(&[0x41, 0xED], &record)].concat();
            let body = [el(&[0xD7], &[1]), el(&[0x83], &[1]), el(&[0x41, 0xE4], &mapping)].concat();
            el(&[0xAE], &body)
        };
        let tracks = parse_tracks(&track(b"dvvC"));
        assert_eq!(tracks[0].dovi, Some(super::super::DolbyVision { profile: 8, compat: 1, level: 6 }));
        assert_eq!(parse_tracks(&track(b"mvcC"))[0].dovi, None, "another block addition");
    }

    #[test]
    fn only_a_profile5_record_over_a_bt2020_pq_or_hlg_vui_is_contradictory() {
        let p5 = Some(super::super::DolbyVision { profile: 5, compat: 0, level: 6 });
        let pq = super::super::Colour { primaries: 9, transfer: 16, matrix: 9, full_range: Some(false) };
        assert!(contradictory_profile5(p5, pq));
        assert!(contradictory_profile5(p5, super::super::Colour { transfer: 18, ..pq }));
        assert!(!contradictory_profile5(p5, super::super::Colour::default()), "a genuine P5 reads no packet");
        assert!(!contradictory_profile5(
            Some(super::super::DolbyVision { profile: 8, compat: 1, level: 6 }),
            pq
        ));
        assert!(!contradictory_profile5(p5, super::super::Colour { matrix: 1, ..pq }));
    }

    #[test]
    fn a_length_prefixed_first_rpu_is_parsed_by_libdovi() {
        use dolby_vision::rpu::{dovi_rpu::DoviRpu, generate::GenerateConfig};

        let mut hvcc = vec![0; 22];
        hvcc[21] = 3; // four-byte NAL lengths
        for (rpu, expected) in [
            (DoviRpu::profile5_config(&GenerateConfig::default()).unwrap(), 5),
            (DoviRpu::profile81_config(&GenerateConfig::default()).unwrap(), 8),
        ] {
            let nal = rpu.write_hevc_unspec62_nalu().unwrap();
            let mut frame = vec![0, 0, 0, 2, 0x26, 0x01]; // an ordinary HEVC NAL before the RPU
            frame.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            frame.extend_from_slice(&nal);
            let found = rpu_nal(&frame, &hvcc).expect("UNSPEC62");
            assert_eq!(DoviRpu::parse_unspec62_nalu(found).unwrap().dovi_profile, expected);
        }
        assert!(rpu_nal(&[0, 0, 0, 8, 0x7c], &hvcc).is_none(), "a NAL past the frame is rejected");
    }

    #[test]
    fn an_unlaced_video_block_yields_its_frame() {
        let mut block = vec![0x81, 0, 0, 0x80]; // track 1, timestamp 0, keyframe, no lacing
        block.extend_from_slice(&[1, 2, 3]);
        assert_eq!(block_frame(&block, 1), Some(&[1, 2, 3][..]));
        block[3] |= 0x02;
        assert_eq!(block_frame(&block, 1), None, "lacing needs a full frame split and is not guessed");
        block[3] = 0x80;
        assert_eq!(block_frame(&block, 2), None, "another track");
    }

    #[test]
    fn a_real_av1_sequence_header_names_the_colours_its_colour_element_does() {
        // av1.mkv is SVT-AV1's HDR10: its Colour element and its sequence header both say PQ and BT.2020.
        let file = include_bytes!("../../testdata/av1.mkv");
        let (_, size, hl) = header(file).unwrap();
        let segment = &file[hl + size.unwrap() as usize..];
        let (_, _, shl) = header(segment).unwrap();
        let tracks = children(&segment[shl..]).find(|(id, _)| *id == TRACKS).unwrap().1;
        let video = parse_tracks(tracks).into_iter().find(|t| t.kind == 1).unwrap();
        assert_eq!(video.codec_id, "V_AV1");
        let (from_container, hdr) = super::super::av1_track(&video.private, colour(&video));
        assert!(hdr);
        let silent = super::super::Colour::default();
        let (from_header, header_hdr) = super::super::av1_track(&video.private, silent);
        assert_eq!(from_header, from_container, "the sequence header alone gives the same string");
        assert!(header_hdr && from_header.unwrap().ends_with(".10.0.110.09.16.09.0"));
    }
}
