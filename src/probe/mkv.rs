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
const CLUSTER: u32 = 0x1F43B675;

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
    language: Option<String>,
    name: Option<String>,
    default: bool,
    commentary: bool,
    width: u32,
    height: u32,
    transfer: u64,
    primaries: u64,
    matrix: u64,
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

/// CueTimes (in timestamp-scale units) of the cue points that index `track`.
fn parse_cues(b: &[u8], track: u64) -> Vec<u64> {
    children(b)
        .filter(|(id, _)| *id == CUE_POINT)
        .filter_map(|(_, point)| {
            let time = child(point, CUE_TIME).map(uint)?;
            children(point)
                .filter(|(id, _)| *id == CUE_TRACK_POSITIONS)
                .any(|(_, pos)| child(pos, CUE_TRACK).map(uint) == Some(track))
                .then_some(time)
        })
        .collect()
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
    let mut keyframes: Vec<f64> =
        parse_cues(&cues, video.number).into_iter().map(|t| secs(t as f64)).collect();
    keyframes.sort_by(f64::total_cmp);
    keyframes.dedup();
    if keyframes.is_empty() {
        return Err(ProbeError::Unsupported("the Cues index no video keyframes".into()));
    }
    let (codec, codecs) = if video.codec_id.starts_with("V_MPEG4/ISO/AVC") {
        (VideoCodec::H264, super::avc_codecs(&video.private))
    } else if video.codec_id.starts_with("V_MPEGH/ISO/HEVC") {
        (VideoCodec::Hevc, super::hevc_codecs(&video.private))
    } else {
        (VideoCodec::Other(video.codec_id.clone()), None)
    };
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
        hdr: super::is_hdr(video.transfer, video.primaries, video.matrix),
        dolby_vision: video.dovi,
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
        assert_eq!(tracks[0].dovi, Some(super::super::DolbyVision { profile: 8, compat: 1 }));
        assert_eq!(parse_tracks(&track(b"mvcC"))[0].dovi, None, "another block addition");
    }
}
