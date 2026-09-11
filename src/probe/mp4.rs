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
        let hdr = match head.get(off as usize..(off as usize).saturating_add(16)) {
            Some(h) if off + 16 <= head.len() as u64 => h.to_vec(),
            _ => src.read(off, 16).await?,
        };
        let Some((size, typ, hl)) = box_header(&hdr) else { break };
        if &typ == b"moov" {
            if size < hl as u64 || size > MAX_ELEMENT {
                return Err(ProbeError::Unsupported(format!("a {size}-byte moov")));
            }
            let end = off + size;
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
        off += size;
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

/// Presentation times, in the track's timescale, of the sync samples.
fn keyframe_ticks(stbl: &[u8], media_time: i64) -> Option<Vec<i64>> {
    let runs = |typ: &[u8; 4]| -> Option<Vec<(u32, u32)>> {
        let b = child(stbl, typ)?;
        let n = u32_at(b, 4)? as usize;
        (0..n).map(|i| Some((u32_at(b, 8 + i * 8)?, u32_at(b, 12 + i * 8)?))).collect()
    };
    let stts = runs(b"stts")?;
    // ctts offsets are signed in version 1 and, in practice, written as signed in version 0 too.
    let ctts = runs(b"ctts").unwrap_or_default();
    let total: u64 = stts.iter().map(|(n, _)| *n as u64).sum();
    let sync: Vec<u64> = match child(stbl, b"stss") {
        Some(b) => {
            let n = u32_at(b, 4)? as usize;
            (0..n).map(|i| u32_at(b, 8 + i * 4).map(u64::from)).collect::<Option<_>>()?
        }
        // No stss: every sample is a sync sample.
        None => (1..=total).collect(),
    };
    let (mut ti, mut t_left, mut dts) = (0usize, stts.first()?.0 as u64, 0i64);
    let (mut ci, mut c_left) = (0usize, ctts.first().map(|r| r.0 as u64).unwrap_or(0));
    let mut sample = 1u64;
    let mut out = Vec::with_capacity(sync.len());
    for s in sync {
        while sample < s {
            // Advance one sample through both run tables.
            dts += stts.get(ti)?.1 as i64;
            t_left -= 1;
            while t_left == 0 && ti + 1 < stts.len() {
                ti += 1;
                t_left = stts[ti].0 as u64;
            }
            if !ctts.is_empty() {
                c_left = c_left.saturating_sub(1);
                while c_left == 0 && ci + 1 < ctts.len() {
                    ci += 1;
                    c_left = ctts[ci].0 as u64;
                }
            }
            sample += 1;
        }
        let cto = ctts.get(ci).map(|r| r.1 as i32 as i64).unwrap_or(0);
        out.push(dts + cto - media_time);
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
    let (codec, codecs) = match &video.fourcc {
        b"avc1" | b"avc3" => (VideoCodec::H264, config(b"avcC").and_then(super::avc_codecs)),
        // dvh1/dvhe carry an hvcC base layer; `dolby_vision` below says whether it shows on its own.
        b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" => {
            (VideoCodec::Hevc, config(b"hvcC").and_then(super::hevc_codecs))
        }
        other => (VideoCodec::Other(String::from_utf8_lossy(other).into_owned()), None),
    };
    // `colr` of type `nclx`: primaries, transfer, matrix as u16s.
    let colr = config(b"colr").filter(|c| c.get(0..4) == Some(b"nclx"));
    let of = |at| colr.and_then(|c| u16_at(c, at)).unwrap_or(0) as u64;
    let hdr = super::is_hdr(of(6), of(4), of(8));
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
        dolby_vision: [b"dvcC", b"dvvC", b"dvwC"].into_iter().find_map(config).and_then(super::dovi_config),
        audio,
        keyframes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_64_bit_box_size_is_read() {
        let mut b = vec![0, 0, 0, 1];
        b.extend_from_slice(b"mdat");
        b.extend_from_slice(&(5u64 << 32).to_be_bytes());
        assert_eq!(box_header(&b), Some((5u64 << 32, *b"mdat", 16)));
        assert_eq!(box_header(&b[..6]), None);
    }
}
