//! The HLS playlists: a VOD media playlist whose segments start on the file's real keyframes, and a
//! one-variant master.
//!
//! Cut points are "the first keyframe at or after n × target" — Jellyfin's approach for copied video.
//! With the video copied, a segment can only begin on a keyframe, and a playlist that promised any
//! other boundary would be lying about where each segment starts.

pub const TARGET_SECS: f64 = 6.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
}

/// Segment boundaries from the keyframe times (seconds, ascending). The first segment starts at 0
/// whatever the first keyframe says; the last runs to `duration`.
///
/// After a cut the next target is the first multiple of `target` past that cut, not the previous
/// target plus one: a file with a keyframe only every 20 s would otherwise owe several targets at
/// once and cut a run of one-GOP segments to catch up.
pub fn segments(keyframes: &[f64], duration: f64, target: f64) -> Vec<Segment> {
    let mut cuts = vec![0.0];
    let mut next = target;
    for &k in keyframes {
        if k <= 0.0 || k >= duration {
            continue;
        }
        if k + 1e-9 >= next {
            cuts.push(k);
            next = ((k / target).floor() + 1.0) * target;
        }
    }
    cuts.iter()
        .enumerate()
        .map(|(i, &start)| Segment { start, end: cuts.get(i + 1).copied().unwrap_or(duration) })
        .collect()
}

/// The media playlist. VOD, so the player knows the whole timeline up front and can seek anywhere
/// before a single segment exists. A `start` past zero is a resume: `EXT-X-START` with `PRECISE=YES`
/// has a native HLS player (Safari's) begin there instead of at zero and then seeking.
pub fn media(segs: &[Segment], start: f64) -> String {
    let target = segs.iter().map(|s| s.end - s.start).fold(0.0, f64::max).ceil().max(1.0) as u64;
    let mut out = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n"
    );
    if start > 0.0 {
        out.push_str(&format!("#EXT-X-START:TIME-OFFSET={start:.3},PRECISE=YES\n"));
    }
    out.push_str("#EXT-X-MAP:URI=\"init.mp4\"\n");
    for (i, s) in segs.iter().enumerate() {
        out.push_str(&format!("#EXTINF:{:.6},\nseg{i}.m4s\n", s.end - s.start));
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

/// The audio in the variant's segments.
pub enum Audio<'a> {
    /// The track re-encoded to AAC-LC stereo.
    Aac,
    /// The track re-encoded to AAC-LC 5.1 or 7.1: its channels, 6 or 8, and its language.
    AacSurround { channels: u32, language: Option<&'a str> },
    /// A track copied as it is: `ec-3`, `ac-3` or `fLaC`, with its channel count and language.
    Copy { codec: &'static str, channels: u32, language: Option<&'a str> },
}

/// Dolby Vision kept in a copied variant, named as RFC 8216bis and Apple's devices read it: a profile 8 base
/// layer's `hvc1…` stays in CODECS with `dvh1.08.LL/<brand>` in SUPPLEMENTAL-CODECS; profile 5, which has no base
/// layer, is its own `dvh1.05.LL` in CODECS and has none. VIDEO-RANGE is what the picture is: PQ, HLG or SDR. A
/// copied HDR AV1 or HEVC is named the same way, by its range alone.
pub struct DolbyVision {
    pub supplemental: Option<String>,
    pub range: &'static str,
}

/// The master playlist: one variant — the copied video plus its audio — and a WebVTT rendition per
/// `(language, name)` in `subs`, most wanted first. The first is the default, so a player shows it without being
/// asked; the rest are there to choose.
///
/// Copied and multichannel AAC audio is also named by an AUDIO rendition with no URI — its media is in the variant's own segments —
/// so the player learns its CHANNELS, which CODECS does not carry. Stereo AAC, which every player assumes, is not.
///
/// FRAME-RATE is named wherever the file says it: Safari passes over a VIDEO-RANGE=PQ variant without one, with no
/// error, and plays nothing.
#[allow(clippy::too_many_arguments)]
pub fn master(
    video_codecs: &str,
    dolby_vision: Option<&DolbyVision>,
    audio: &Audio<'_>,
    bandwidth: u64,
    average: u64,
    resolution: Option<(u32, u32)>,
    frame_rate: Option<f64>,
    subs: &[(String, String)],
) -> String {
    let res = resolution.filter(|(w, h)| *w > 0 && *h > 0).map(|(w, h)| format!(",RESOLUTION={w}x{h}"));
    let rate = frame_rate.filter(|f| *f > 0.0 && *f < 1000.0).map(|f| format!(",FRAME-RATE={f:.3}"));
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    let (audio_codec, rendition) = match audio {
        Audio::Aac => ("mp4a.40.2", None),
        Audio::AacSurround { channels, language } => ("mp4a.40.2", Some((*channels, language))),
        Audio::Copy { codec, channels, language } => (*codec, Some((*channels, language))),
    };
    if let Some((channels, language)) = rendition {
        let (name, lang) = match language {
            Some(l) => (crate::lang::name(l), format!(",LANGUAGE=\"{l}\"")),
            None => ("Audio".to_string(), String::new()),
        };
        out.push_str(&format!(
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"{name}\"{lang},DEFAULT=YES,AUTOSELECT=YES,\
             CHANNELS=\"{channels}\"\n"
        ));
    }
    for (i, (lang, name)) in subs.iter().enumerate() {
        let default = if i == 0 { "YES" } else { "NO" };
        out.push_str(&format!(
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{name}\",LANGUAGE=\"{lang}\",\
             DEFAULT={default},AUTOSELECT=YES,FORCED=NO,URI=\"sub{i}.m3u8\"\n"
        ));
    }
    let dv = dolby_vision.map_or_else(String::new, |dv| {
        let supplemental =
            dv.supplemental.as_deref().map(|s| format!(",SUPPLEMENTAL-CODECS=\"{s}\"")).unwrap_or_default();
        format!("{supplemental},VIDEO-RANGE={}", dv.range)
    });
    out.push_str(&format!(
        "#EXT-X-STREAM-INF:BANDWIDTH={bandwidth},AVERAGE-BANDWIDTH={average},CODECS=\"{video_codecs},{audio_codec}\"{dv}{}{}{}{}\n\
         media.m3u8\n",
        res.unwrap_or_default(),
        rate.unwrap_or_default(),
        if rendition.is_none() { "" } else { ",AUDIO=\"audio\"" },
        if subs.is_empty() { "" } else { ",SUBTITLES=\"subs\"" }
    ));
    out
}

/// A subtitle rendition's playlist: one WebVTT segment spanning the film.
pub fn subtitle_media(duration: f64, n: usize) -> String {
    format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n\
         #EXTINF:{duration:.6},\nsub{n}.vtt\n#EXT-X-ENDLIST\n",
        duration.ceil().max(1.0) as u64
    )
}

/// BANDWIDTH and AVERAGE-BANDWIDTH from the file's size and duration. The average is honest; the peak
/// is a guess (a quarter over, plus the AAC we add), because nothing short of reading the whole file
/// says what the busiest segment carries. `None` size assumes a 1080p WEB-DL.
pub fn bandwidth(size: Option<u64>, duration: f64) -> (u64, u64) {
    let avg = match size {
        Some(s) if duration > 0.0 => (s as f64 * 8.0 / duration) as u64,
        _ => 8_000_000,
    };
    (avg + avg / 4 + 192_000, avg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// h264.mkv's keyframes, as ffprobe reports them (testdata/README.md).
    const KF: [f64; 11] = [0.0, 2.5, 5.0, 8.0, 10.5, 13.0, 16.0, 19.5, 22.0, 24.0, 27.5];

    #[test]
    fn cuts_land_on_the_first_keyframe_at_or_after_each_target() {
        let segs = segments(&KF, 30.021, TARGET_SECS);
        let starts: Vec<f64> = segs.iter().map(|s| s.start).collect();
        // 6 → 8, then 12 → 13, 18 → 19.5, 24 → 24; 30 is past the last keyframe.
        assert_eq!(starts, [0.0, 8.0, 13.0, 19.5, 24.0]);
        assert_eq!(segs.last().unwrap().end, 30.021);
        for w in segs.windows(2) {
            assert_eq!(w[0].end, w[1].start, "no gap and no overlap");
        }
        for s in &segs[1..] {
            assert!(KF.contains(&s.start), "every boundary but the first is a real keyframe");
        }
    }

    #[test]
    fn sparse_keyframes_do_not_cause_a_run_of_catch_up_cuts() {
        let segs = segments(&[0.0, 20.0, 21.0, 22.0, 26.0], 40.0, 6.0);
        let starts: Vec<f64> = segs.iter().map(|s| s.start).collect();
        assert_eq!(starts, [0.0, 20.0, 26.0], "21 and 22 are not owed a target");
    }

    #[test]
    fn a_file_whose_first_keyframe_is_late_still_starts_at_zero() {
        let segs = segments(&[0.5, 7.0], 10.0, 6.0);
        assert_eq!(segs[0].start, 0.0);
        assert_eq!(segs[1].start, 7.0);
    }

    #[test]
    fn the_media_playlist_sums_to_the_duration() {
        let segs = segments(&KF, 30.021, TARGET_SECS);
        let m = media(&segs, 0.0);
        assert!(!m.contains("EXT-X-START"), "a start from zero names no start point");
        let total: f64 = m
            .lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .map(|v| v.trim_end_matches(',').parse::<f64>().unwrap())
            .sum();
        assert!((total - 30.021).abs() < 1e-5, "EXTINF sums to {total}");
        assert!(m.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(m.contains("#EXT-X-MAP:URI=\"init.mp4\""));
        assert!(m.trim_end().ends_with("#EXT-X-ENDLIST"));
        // The longest segment is the first, 0 → 8 (no keyframe between 6 and 8); the target covers it.
        assert!(m.contains("#EXT-X-TARGETDURATION:8\n"), "{m}");
        assert_eq!(m.matches(".m4s").count(), segs.len());
        assert!(m.contains("seg0.m4s") && m.contains("seg4.m4s"));
    }

    #[test]
    fn a_resume_names_its_start_point() {
        let m = media(&segments(&KF, 30.021, TARGET_SECS), 14.25);
        let start = "#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-START:TIME-OFFSET=14.250,PRECISE=YES\n#EXT-X-MAP";
        assert!(m.contains(start), "{m}");
    }

    #[test]
    fn the_master_names_both_codecs() {
        let (peak, avg) = bandwidth(Some(3_750_000), 30.0);
        assert_eq!(avg, 1_000_000);
        assert!(peak > avg);
        let m = master(
            "hvc1.1.6.L93.B0",
            None,
            &Audio::Aac,
            peak,
            avg,
            Some((320, 180)),
            Some(24000.0 / 1001.0),
            &[],
        );
        assert!(m.contains("CODECS=\"hvc1.1.6.L93.B0,mp4a.40.2\""), "{m}");
        assert!(m.contains("RESOLUTION=320x180,FRAME-RATE=23.976\n"), "{m}");
        assert!(m.contains(&format!("BANDWIDTH={peak},AVERAGE-BANDWIDTH={avg}")));
        assert!(m.ends_with("media.m3u8\n"));
        assert!(!m.contains("SUBTITLES") && !m.contains("TYPE=AUDIO") && !m.contains("AUDIO=\""));
        assert!(!master("avc1.64001e", None, &Audio::Aac, 1, 1, None, None, &[]).contains("RESOLUTION"));
        assert!(!m.contains("VIDEO-RANGE"), "nothing kept, nothing named");
    }

    #[test]
    fn kept_dolby_vision_is_named_beside_or_as_its_codec() {
        let p81 = DolbyVision { supplemental: Some("dvh1.08.06/db1p".into()), range: "PQ" };
        let m = master("hvc1.2.4.L153.B0", Some(&p81), &Audio::Aac, 1, 1, None, None, &[]);
        let supplemental = ",SUPPLEMENTAL-CODECS=\"dvh1.08.06/db1p\",VIDEO-RANGE=PQ\n";
        assert!(m.contains(&format!("CODECS=\"hvc1.2.4.L153.B0,mp4a.40.2\"{supplemental}")), "{m}");
        let p5 = DolbyVision { supplemental: None, range: "PQ" };
        let m = master("dvh1.05.06", Some(&p5), &Audio::Aac, 1, 1, None, None, &[]);
        assert!(m.contains("CODECS=\"dvh1.05.06,mp4a.40.2\",VIDEO-RANGE=PQ\n"), "{m}");
        assert!(!m.contains("SUPPLEMENTAL"), "{m}");
    }

    #[test]
    fn copied_dolby_audio_is_named_with_its_channels() {
        let eac3 = Audio::Copy { codec: "ec-3", channels: 6, language: Some("eng") };
        let m = master("hvc1.2.4.L150.B0", None, &eac3, 1, 1, None, None, &[]);
        assert!(
            m.contains(
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"English\",LANGUAGE=\"eng\",DEFAULT=YES,AUTOSELECT=YES,CHANNELS=\"6\"\n"
            ),
            "{m}"
        );
        assert!(m.contains("CODECS=\"hvc1.2.4.L150.B0,ec-3\",AUDIO=\"audio\"\nmedia.m3u8"), "{m}");
        assert!(!m.contains("URI="), "the audio is in the variant's own segments: {m}");
        let ac3 = Audio::Copy { codec: "ac-3", channels: 2, language: None };
        let m = master("avc1.640028", None, &ac3, 1, 1, None, None, &[]);
        assert!(m.contains("NAME=\"Audio\",DEFAULT=YES") && m.contains(",ac-3\""), "{m}");
    }

    #[test]
    fn aac_5_1_is_named_with_its_channels_and_stereo_is_not() {
        let surround = Audio::AacSurround { channels: 6, language: Some("swe") };
        let m = master("avc1.640028", None, &surround, 1, 1, None, None, &[]);
        assert!(
            m.contains(
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"Swedish\",LANGUAGE=\"swe\",DEFAULT=YES,AUTOSELECT=YES,CHANNELS=\"6\"\n"
            ),
            "{m}"
        );
        assert!(m.contains("CODECS=\"avc1.640028,mp4a.40.2\",AUDIO=\"audio\"\nmedia.m3u8"), "{m}");
        let stereo = master("avc1.640028", None, &Audio::Aac, 1, 1, None, None, &[]);
        assert_eq!(
            stereo,
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n\
             #EXT-X-STREAM-INF:BANDWIDTH=1,AVERAGE-BANDWIDTH=1,CODECS=\"avc1.640028,mp4a.40.2\"\nmedia.m3u8\n",
            "stereo is as it always was"
        );
    }

    #[test]
    fn aac_7_1_and_copied_flac_are_named_with_their_channels() {
        let aac71 = Audio::AacSurround { channels: 8, language: Some("eng") };
        let m = master("vp09.00.41.08", None, &aac71, 1, 1, None, None, &[]);
        assert!(m.contains("LANGUAGE=\"eng\",DEFAULT=YES,AUTOSELECT=YES,CHANNELS=\"8\"\n"), "{m}");
        assert!(m.contains("CODECS=\"vp09.00.41.08,mp4a.40.2\",AUDIO=\"audio\"\nmedia.m3u8"), "{m}");
        let flac = Audio::Copy { codec: "fLaC", channels: 6, language: None };
        let hdr = DolbyVision { supplemental: None, range: "PQ" };
        let m = master("vp09.02.51.10.01.09.16.09.00", Some(&hdr), &flac, 1, 1, None, None, &[]);
        assert!(m.contains("NAME=\"Audio\",DEFAULT=YES,AUTOSELECT=YES,CHANNELS=\"6\"\n"), "{m}");
        assert!(
            m.contains(
                "CODECS=\"vp09.02.51.10.01.09.16.09.00,fLaC\",VIDEO-RANGE=PQ,AUDIO=\"audio\"\nmedia.m3u8"
            ),
            "{m}"
        );
    }

    #[test]
    fn subtitles_are_renditions_of_one_group() {
        let subs = [("en".to_string(), "English".to_string()), ("fi".to_string(), "Finnish".to_string())];
        let m = master("avc1.640028", None, &Audio::Aac, 1, 1, None, None, &subs);
        assert!(m.contains("TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"Finnish\",LANGUAGE=\"fi\""), "{m}");
        assert!(m.contains("URI=\"sub1.m3u8\""));
        assert!(m.contains("LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES"), "the most wanted shows: {m}");
        assert!(m.contains("LANGUAGE=\"fi\",DEFAULT=NO,AUTOSELECT=YES"), "{m}");
        assert_eq!(m.matches("DEFAULT=YES").count(), 1, "one default in a group");
        assert!(m.contains(",SUBTITLES=\"subs\"\nmedia.m3u8"), "{m}");
        let s = subtitle_media(30.021, 1);
        assert!(
            s.contains("#EXT-X-TARGETDURATION:31\n") && s.contains("#EXTINF:30.021000,\nsub1.vtt\n"),
            "{s}"
        );
        assert!(s.ends_with("#EXT-X-ENDLIST\n"));
    }
}
