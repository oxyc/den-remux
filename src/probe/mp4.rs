//! MP4: walk the top-level boxes to `moov` (reading headers across the `mdat` without reading it),
//! then take the keyframes from the video track's sample tables.
//!
//! A keyframe's time is its decode time (`stts`) plus its composition offset (`ctts`), less the edit
//! list's `media_time` — the same shift ffmpeg's demuxer applies, so the numbers match the timeline
//! ffmpeg cuts on.

use super::{AudioTrack, MediaInfo, ProbeError, Source, VideoCodec, MAX_ELEMENT};

/// Box size, type and header length. A size of 0 ("runs to the end of the file") is reported as 0.
fn box_header(b: &[u8]) -> Option<(u64, [u8; 4], usize)> {
    let size = u32::from_be_bytes(b.get(0..4)?.try_into().ok()?) as u64;
    let typ: [u8; 4] = b.get(4..8)?.try_into().ok()?;
    if size == 1 {
        return Some((u64::from_be_bytes(b.get(8..16)?.try_into().ok()?), typ, 16));
    }
    Some((size, typ, 8))
}

fn boxes(b: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        let (size, typ, hl) = box_header(b.get(pos..)?)?;
        let size = if size == 0 { (b.len() - pos) as u64 } else { size };
        let end = pos.checked_add(usize::try_from(size).ok()?)?;
        if end > b.len() || (size as usize) < hl {
            return None;
        }
        let body = &b[pos + hl..end];
        pos = end;
        Some((typ, body))
    })
}

fn child<'a>(b: &'a [u8], typ: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(b).find(|(t, _)| t == typ).map(|(_, body)| body)
}

fn path<'a>(b: &'a [u8], p: &[&[u8; 4]]) -> Option<&'a [u8]> {
    p.iter().try_fold(b, |cur, typ| child(cur, typ))
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(i..i + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], i: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(i..i + 8)?.try_into().ok()?))
}

pub async fn probe(src: &Source<'_>, head: &[u8]) -> Result<MediaInfo, ProbeError> {
    let mut off = 0u64;
    // A real file has a handful of top-level boxes; the bound only stops a malformed one looping.
    for _ in 0..64 {
        off.checked_add(16).ok_or_else(|| ProbeError::Unsupported("box offset overflow".into()))?;
        let hdr = match head.get(off as usize..(off as usize).saturating_add(16)) {
            Some(h) if off + 16 <= head.len() as u64 => h.to_vec(),
            _ => src.read(off, 16).await?,
        };
        let Some((size, typ, hl)) = box_header(&hdr) else { break };
        if &typ == b"moov" {
            if size < hl as u64 || size > MAX_ELEMENT {
                return Err(ProbeError::Unsupported(format!("a {size}-byte moov")));
            }
            let end =
                off.checked_add(size).ok_or_else(|| ProbeError::Unsupported("box size overflow".into()))?;
            let body = if end <= head.len() as u64 {
                head[(off as usize + hl)..end as usize].to_vec()
            } else {
                src.read(off + hl as u64, size - hl as u64).await?
            };
            if (body.len() as u64) < size - hl as u64 {
                return Err(ProbeError::Truncated("moov"));
            }
            return parse_moov(&body);
        }
        if size < hl as u64 {
            break; // 0 = "to the end of the file": whatever this is, there is no moov after it
        }
        off = off.checked_add(size).ok_or_else(|| ProbeError::Unsupported("box size overflow".into()))?;
    }
    Err(ProbeError::Unsupported("no moov box".into()))
}

struct Trak<'a> {
    handler: [u8; 4],
    timescale: u32,
    language: Option<String>,
    entry: &'a [u8],
    fourcc: [u8; 4],
    /// The edit list: total empty-edit time (movie timescale), then the first real edit's media_time.
    empty: u64,
    media_time: i64,
    stbl: &'a [u8],
}

fn parse_trak(trak: &[u8]) -> Option<Trak<'_>> {
    let mdia = child(trak, b"mdia")?;
    let mdhd = child(mdia, b"mdhd")?;
    let (timescale, lang_at) =
        if mdhd.first() == Some(&1) { (u32_at(mdhd, 20)?, 32) } else { (u32_at(mdhd, 12)?, 20) };
    let packed = u16_at(mdhd, lang_at).unwrap_or(0);
    let language: String = [(packed >> 10) & 31, (packed >> 5) & 31, packed & 31]
        .iter()
        .map(|c| char::from(0x60 + *c as u8))
        .collect();
    let handler: [u8; 4] = child(mdia, b"hdlr")?.get(8..12)?.try_into().ok()?;
    let stbl = path(mdia, &[b"minf", b"stbl"])?;
    let stsd = child(stbl, b"stsd")?;
    // version/flags, entry_count, then the first sample entry as a box.
    let (etyp, entry) = boxes(stsd.get(8..)?).next()?;
    let (mut empty, mut media_time) = (0u64, 0i64);
    if let Some(elst) = path(trak, &[b"edts", b"elst"]) {
        let v1 = elst.first() == Some(&1);
        let n = u32_at(elst, 4).unwrap_or(0) as usize;
        let step = if v1 { 20 } else { 12 };
        for i in 0..n.min(16) {
            let at = 8 + i * step;
            let (dur, mt) = if v1 {
                (u64_at(elst, at)?, u64_at(elst, at + 8)? as i64)
            } else {
                (u32_at(elst, at)? as u64, u32_at(elst, at + 4)? as i32 as i64)
            };
            if mt == -1 {
                empty += dur;
            } else {
                media_time = mt;
                break;
            }
        }
    }
    Some(Trak {
        handler,
        timescale,
        language: (language != "und" && language.bytes().all(|c| c.is_ascii_lowercase())).then_some(language),
        entry,
        fourcc: etyp,
        empty,
        media_time,
        stbl,
    })
}

// Decoded limits, separate from MAX_ELEMENT's encoded-byte cap. Three million samples covers six
// hours at 120 fps. Even an all-intra file may retain at most 8 MB of keyframe ticks, rather than
// expanding a single hostile stts run into gigabytes. Timing tables stay borrowed from the moov.
const MAX_SAMPLES: u64 = 3_000_000;
const MAX_KEYFRAMES: usize = 1_000_000;

fn timing_runs(b: &[u8]) -> Option<(&[u8], u64)> {
    let n = u32_at(b, 4)? as usize;
    if n == 0 || n as u64 > MAX_SAMPLES {
        return None;
    }
    let runs = b.get(8..8usize.checked_add(n.checked_mul(8)?)?)?;
    let total = runs.as_chunks::<8>().0.iter().try_fold(0u64, |total, run| {
        let count = u32_at(run, 0)? as u64;
        let next = total.checked_add(count)?;
        (count > 0 && next <= MAX_SAMPLES).then_some(next)
    })?;
    Some((runs, total))
}

/// Presentation times, in the track's timescale, of the sync samples. Advance whole runs between
/// keyframes: a large sync index must not cause millions of iterations on the HTTP runtime thread.
fn keyframe_ticks(stbl: &[u8], media_time: i64) -> Option<Vec<i64>> {
    let (stts, total) = timing_runs(child(stbl, b"stts")?)?;
    // ctts offsets are signed in version 1 and, in practice, written as signed in version 0 too.
    let ctts = match child(stbl, b"ctts") {
        Some(b) => {
            let (runs, count) = timing_runs(b)?;
            if count != total {
                return None;
            }
            Some(runs)
        }
        None => None,
    };
    let sync = child(stbl, b"stss");
    let count = match sync {
        Some(b) => {
            let n = u32_at(b, 4)? as usize;
            b.get(8..8usize.checked_add(n.checked_mul(4)?)?)?;
            n
        }
        // No stss: every sample is a sync sample.
        None => usize::try_from(total).ok()?,
    };
    if count > MAX_KEYFRAMES {
        return None;
    }
    let (mut ti, mut t_start, mut ticks) = (0usize, 1u64, 0i64);
    let (mut ci, mut c_start) = (0usize, 1u64);
    let mut previous = 0;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let sample = match sync {
            Some(b) => u32_at(b, 8 + i * 4)? as u64,
            None => i as u64 + 1,
        };
        if sample <= previous || sample > total {
            return None;
        }
        previous = sample;
        while sample >= t_start + u32_at(stts, ti)? as u64 {
            let n = u32_at(stts, ti)? as u64;
            ticks = ticks.checked_add((n as i64).checked_mul(u32_at(stts, ti + 4)? as i64)?)?;
            t_start += n;
            ti += 8;
        }
        let dts =
            ticks.checked_add(((sample - t_start) as i64).checked_mul(u32_at(stts, ti + 4)? as i64)?)?;
        let cto = match ctts {
            Some(runs) => {
                while sample >= c_start + u32_at(runs, ci)? as u64 {
                    c_start += u32_at(runs, ci)? as u64;
                    ci += 8;
                }
                u32_at(runs, ci + 4)? as i32 as i64
            }
            None => 0,
        };
        out.push(dts.checked_add(cto)?.checked_sub(media_time)?);
    }
    Some(out)
}

fn parse_moov(moov: &[u8]) -> Result<MediaInfo, ProbeError> {
    let mvhd = child(moov, b"mvhd").ok_or(ProbeError::Truncated("mvhd"))?;
    let (movie_ts, movie_dur) = if mvhd.first() == Some(&1) {
        (u32_at(mvhd, 20), u64_at(mvhd, 24))
    } else {
        (u32_at(mvhd, 12), u32_at(mvhd, 16).map(u64::from))
    };
    let (movie_ts, movie_dur) = match (movie_ts, movie_dur) {
        (Some(ts), Some(d)) if ts > 0 => (ts as f64, d as f64),
        _ => return Err(ProbeError::Truncated("mvhd")),
    };
    let traks: Vec<Trak> =
        boxes(moov).filter(|(t, _)| t == b"trak").filter_map(|(_, b)| parse_trak(b)).collect();
    let video = traks
        .iter()
        .find(|t| &t.handler == b"vide")
        .ok_or_else(|| ProbeError::Unsupported("no video track".into()))?;
    // Visual sample entry: 8 bytes of SampleEntry, then 70 of VisualSampleEntry; width/height at 24.
    let config = |typ: &[u8; 4]| video.entry.get(78..).and_then(|kids| child(kids, typ));
    // `colr` of type `nclx`: primaries, transfer, matrix as u16s, then the full-range flag in the top bit.
    let colr = config(b"colr").filter(|c| c.get(0..4) == Some(b"nclx"));
    let of = |at| colr.and_then(|c| u16_at(c, at)).unwrap_or(0) as u64;
    let container = super::Colour {
        primaries: of(4),
        transfer: of(6),
        matrix: of(8),
        full_range: colr.and_then(|c| c.get(10)).map(|b| b & 0x80 != 0),
    };
    let mut hdr = super::is_hdr(of(6), of(4), of(8));
    let mut hlg = of(6) == 18;
    let (codec, codecs) = match &video.fourcc {
        b"avc1" | b"avc3" => (VideoCodec::H264, config(b"avcC").and_then(super::avc_codecs)),
        // dvh1/dvhe carry an hvcC base layer; `dolby_vision` below says whether it shows on its own.
        b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" => match config(b"hvcC") {
            Some(hvcc) => {
                let (codecs, hevc_hdr, hevc_hlg) = super::hevc_track(hvcc, container);
                (hdr, hlg) = (hevc_hdr, hevc_hlg);
                (VideoCodec::Hevc, codecs)
            }
            None => (VideoCodec::Hevc, None),
        },
        b"av01" => {
            let (codecs, av1_hdr) = config(b"av1C").map_or((None, hdr), |c| super::av1_track(c, container));
            hdr = av1_hdr;
            (VideoCodec::Av1, codecs)
        }
        other => (VideoCodec::Other(String::from_utf8_lossy(other).into_owned()), None),
    };
    if video.timescale == 0 {
        return Err(ProbeError::Truncated("mdhd"));
    }
    let ts = video.timescale as f64;
    let offset = video.empty as f64 / movie_ts;
    let mut keyframes: Vec<f64> = keyframe_ticks(video.stbl, video.media_time)
        .ok_or(ProbeError::Truncated("sample tables"))?
        .into_iter()
        .map(|t| t as f64 / ts + offset)
        .collect();
    keyframes.sort_by(f64::total_cmp);
    keyframes.dedup();
    if keyframes.is_empty() {
        return Err(ProbeError::Unsupported("no sync samples".into()));
    }
    let audio = traks
        .iter()
        .filter(|t| &t.handler == b"soun")
        .map(|t| AudioTrack {
            codec: String::from_utf8_lossy(&t.fourcc).into_owned(),
            language: t.language.clone(),
            // AudioSampleEntry: 8 bytes of SampleEntry, 8 reserved, then channelcount.
            channels: u16_at(t.entry, 16).unwrap_or(2) as u32,
            name: None,
            default: true,
            commentary: false,
        })
        .collect();
    Ok(MediaInfo {
        container: "mp4",
        duration: movie_dur / movie_ts,
        video: codec,
        codecs,
        width: u16_at(video.entry, 24).unwrap_or(0) as u32,
        height: u16_at(video.entry, 26).unwrap_or(0) as u32,
        hdr,
        hlg,
        // The samples over all of stts's time: a first sample shorter than the rest, as an encoder's often is, doesn't
        // skew it.
        frame_rate: child(video.stbl, b"stts").and_then(timing_runs).and_then(|(runs, frames)| {
            let ticks = runs.as_chunks::<8>().0.iter().try_fold(0u64, |ticks, run| {
                ticks.checked_add((u32_at(run, 0)? as u64).checked_mul(u32_at(run, 4)? as u64)?)
            })?;
            (ticks > 0).then(|| frames as f64 * video.timescale as f64 / ticks as f64)
        }),
        dolby_vision: [b"dvcC", b"dvvC", b"dvwC"].into_iter().find_map(config).and_then(super::dovi_config),
        audio,
        keyframes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(typ: &[u8; 4], entries: &[u32], count: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&(16 + entries.len() as u32 * 4).to_be_bytes());
        b.extend_from_slice(typ);
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&count.to_be_bytes());
        for v in entries {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b
    }

    #[test]
    fn tiny_sample_tables_cannot_expand_past_decoded_limits() {
        assert!(keyframe_ticks(&table(b"stts", &[u32::MAX, 1], 1), 0).is_none());
        assert!(keyframe_ticks(&table(b"stts", &[MAX_KEYFRAMES as u32 + 1, 1], 1), 0).is_none());
        // A truncated declared table and zero-length runs are malformed, not empty/default timing.
        assert!(keyframe_ticks(&table(b"stts", &[1, 1], u32::MAX), 0).is_none());
        assert!(keyframe_ticks(&table(b"stts", &[0, 1], 1), 0).is_none());
    }

    #[test]
    fn sync_indices_must_be_ordered_and_inside_the_timing_table() {
        for indices in [&[0][..], &[u32::MAX], &[2, 1], &[1, 1]] {
            let mut b = table(b"stts", &[3, 10], 1);
            b.extend(table(b"stss", indices, indices.len() as u32));
            assert!(keyframe_ticks(&b, 0).is_none(), "accepted {indices:?}");
        }
        let mut b = table(b"stts", &[3, 10], 1);
        b.extend(table(b"ctts", &[2, 1], 1));
        assert!(keyframe_ticks(&b, 0).is_none(), "ctts did not cover all samples");
    }

    #[test]
    fn sparse_sync_samples_jump_timing_runs_and_keep_composition_offsets() {
        let mut b = table(b"stts", &[MAX_SAMPLES as u32, 10], 1);
        b.extend(table(b"ctts", &[1, 2, MAX_SAMPLES as u32 - 1, (-2i32) as u32], 2));
        b.extend(table(b"stss", &[1, MAX_SAMPLES as u32], 2));
        assert_eq!(keyframe_ticks(&b, 0), Some(vec![2, (MAX_SAMPLES as i64 - 1) * 10 - 2]));

        let mut b = table(b"stts", &[2, 10, 3, 20], 2);
        b.extend(table(b"ctts", &[1, 3, 4, (-1i32) as u32], 2));
        assert_eq!(keyframe_ticks(&b, 4), Some(vec![-1, 5, 15, 35, 55]));
        b.extend(table(b"stss", &[1, 2, 3, 5], 4));
        assert_eq!(keyframe_ticks(&b, 4), Some(vec![-1, 5, 15, 55]));
        assert!(keyframe_ticks(&b, i64::MIN).is_none(), "timestamp overflow was accepted");
    }

    #[test]
    fn a_64_bit_box_size_is_read() {
        let mut b = vec![0, 0, 0, 1];
        b.extend_from_slice(b"mdat");
        b.extend_from_slice(&(5u64 << 32).to_be_bytes());
        assert_eq!(box_header(&b), Some((5u64 << 32, *b"mdat", 16)));
        assert_eq!(box_header(&b[..6]), None);
    }
}
