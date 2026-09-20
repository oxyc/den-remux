//! Subtitles from den-subtitles, as HLS WebVTT renditions — what AirPlay and Cast receivers show, which a
//! page's own `<track>` never reaches.
//!
//! The master playlist is fixed when the session starts, so it names one rendition per language the
//! browser asked for, and nothing is fetched until a player opens one. Then den-remux asks den-subtitles
//! for the title with the release's OpenSubtitles hash, size and filename — the hints the Apple TV sends,
//! so an exact-encode match (in sync by construction) ranks first — and serves the first subtitle in that
//! language as one WebVTT segment spanning the film. A language with nothing to offer serves an empty
//! document rather than an error: the playlist has already promised it.
//!
//! The release's own text tracks (SRT, ASS/SSA, WebVTT, `mov_text`) are the fallback for a language den-subtitles has
//! nothing in, and add the languages only the release carries. They are written by the same ffmpeg runs that copy the
//! video (`job::Spec::text`), so they arrive as the video does — a rendition is a WebVTT segment per video segment,
//! made once the runs have read past it. [`OwnCues`] keeps what the runs wrote and which stretches of the film they
//! have covered.

use std::collections::{BTreeSet, HashMap};

use serde::Deserialize;

use crate::probe::SubtitleTrack;

/// OpenSubtitles' hash reads this much from each end of the file.
pub const HASH_CHUNK: u64 = 64 * 1024;
/// Renditions per session. A single digit: `sub<N>` names them.
pub const MAX_LANGUAGES: usize = 8;
/// Larger than any real subtitle file.
pub const MAX_SUBTITLE: u64 = 4 << 20;
pub const MAX_LIST: u64 = 1 << 20;
/// What a rendition serves when there is nothing for its language.
pub const EMPTY: &str = "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n\n";

/// The most an own track's WebVTT file may grow to before it is left unread: a feature film's cues are a few hundred KB.
pub const MAX_OWN: u64 = 8 << 20;
/// How close to a cue's window its coverage has to reach, seconds. The same tolerance as the playlist's keyframes.
const SNAP: f64 = 0.05;
/// A run's own track is trusted this far behind the video it has finished (seconds): the demuxer hands over a
/// subtitle block with the cluster it is in, and a block a moment before a keyframe can trail it.
pub const TRAIL: f64 = 1.0;

/// One thing a player can pick: a language, and where its cues come from.
#[derive(Clone, Debug, PartialEq)]
pub struct Rendition {
    /// As `lang::canonical` gives it.
    pub lang: String,
    /// den-subtitles is asked for it: the browser named the language and sent an install.
    pub den: bool,
    /// The release's own text track for it, as its index among the subtitle tracks (ffmpeg's `0:s:N`).
    pub own: Option<usize>,
}

/// The renditions of a session, most wanted first: each language the browser named that den-subtitles can be asked for
/// or the release carries, then the release's other languages. A full track of its language only — never one flagged
/// forced (foreign-language parts), a bitmap, or one that names no language, which cannot be labelled.
pub fn plan(requested: &[String], den: bool, tracks: &[SubtitleTrack]) -> Vec<Rendition> {
    let mut own: Vec<(String, usize)> = Vec::new();
    for (n, t) in tracks.iter().enumerate() {
        let Some(lang) = t.language.as_deref().map(crate::lang::canonical).filter(|l| !l.is_empty()) else {
            continue;
        };
        if t.text && !t.forced && !own.iter().any(|(l, _)| *l == lang) {
            own.push((lang, n));
        }
    }
    let found = |lang: &str| own.iter().find(|(l, _)| l == lang).map(|(_, n)| *n);
    let mut out: Vec<Rendition> = requested
        .iter()
        .filter_map(|l| {
            let own = found(l);
            (den || own.is_some()).then(|| Rendition { lang: l.clone(), den, own })
        })
        .collect();
    for (lang, n) in &own {
        if out.len() < MAX_LANGUAGES && !out.iter().any(|r| r.lang == *lang) {
            out.push(Rendition { lang: lang.clone(), den: false, own: Some(*n) });
        }
    }
    out.truncate(MAX_LANGUAGES);
    out
}

/// The subtitle side of a session.
pub struct Subs {
    /// den-subtitles' install URL, sealed config included — a secret.
    pub base: String,
    /// The first half of the release's OpenSubtitles hash, from the head read when it was opened.
    pub head_sum: Option<u64>,
    pub cache: tokio::sync::Mutex<Cache>,
}

#[derive(Default)]
pub struct Cache {
    /// den-subtitles' answer for the title, once asked.
    pub list: Option<Vec<Entry>>,
    /// Each rendition's document, once made.
    pub docs: Vec<Option<String>>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Entry {
    pub url: String,
    #[serde(default)]
    pub lang: String,
}

#[derive(Deserialize)]
struct List {
    #[serde(default)]
    subtitles: Vec<Entry>,
}

/// The sum of one end's little-endian u64 words: half of the OpenSubtitles hash.
pub fn chunk_sum(b: &[u8]) -> u64 {
    b.as_chunks::<8>().0.iter().fold(0u64, |acc, w| acc.wrapping_add(u64::from_le_bytes(*w)))
}

/// OpenSubtitles' movie hash: the file size plus both ends' sums, as 16 hex digits.
pub fn movie_hash(size: u64, head_sum: u64, tail_sum: u64) -> String {
    format!("{:016x}", size.wrapping_add(head_sum).wrapping_add(tail_sum))
}

/// `<install>/subtitles/<type>/<id>/videoHash=…&videoSize=…&filename=….json`: Stremio's extras are a path
/// segment, not a query.
pub fn list_url(
    base: &str,
    id: &str,
    hash: Option<&str>,
    size: Option<u64>,
    filename: &str,
) -> Option<String> {
    let mut url = reqwest::Url::parse(base).ok()?;
    let mut extras = Vec::new();
    if let Some(h) = hash {
        extras.push(format!("videoHash={h}"));
    }
    if let Some(s) = size {
        extras.push(format!("videoSize={s}"));
    }
    // One field of one segment: these would split the extras or the path (the Apple TV blanks them too).
    let safe: String =
        filename.chars().map(|c| if matches!(c, '/' | '&' | '=' | '?' | '#') { ' ' } else { c }).collect();
    extras.push(format!("filename={safe}"));
    url.path_segments_mut()
        .ok()?
        .push("subtitles")
        .push(crate::scout::kind(id))
        .push(id)
        .push(&format!("{}.json", extras.join("&")));
    Some(url.into())
}

pub fn parse_list(body: &[u8]) -> Vec<Entry> {
    serde_json::from_slice::<List>(body).map(|l| l.subtitles).unwrap_or_default()
}

/// den-subtitles serves every subtitle as `.srt` or, the same document, `.vtt`.
pub fn vtt_url(url: &str) -> String {
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (url, None),
    };
    let path = path.strip_suffix(".srt").map_or_else(|| path.to_string(), |p| format!("{p}.vtt"));
    match query {
        Some(q) => format!("{path}?{q}"),
        None => path,
    }
}

/// Is `url` on one of `origins`, with no credentials? A subtitle URL comes from den-subtitles' answer,
/// so it is checked like one the browser sent.
pub fn on_origin(url: &str, origins: &[String]) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| {
        u.username().is_empty()
            && u.password().is_none()
            && origins.contains(&u.origin().ascii_serialization())
    })
}

/// The document as an HLS WebVTT segment, or `None` if it is not WebVTT. A timestamp map pins its cue
/// times to the media timeline, which starts at 0 (the jobs run `-copyts -start_at_zero`).
pub fn for_hls(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?.trim_start_matches('\u{feff}');
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    if !first.trim_end().starts_with("WEBVTT") {
        return None;
    }
    if rest.contains("X-TIMESTAMP-MAP") {
        return Some(text.to_string());
    }
    Some(format!("{}\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n{rest}", first.trim_end()))
}

/// One cue of an own track: its times in milliseconds on the film's timeline and the block as ffmpeg wrote it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cue {
    pub start: u64,
    pub end: u64,
    pub block: String,
}

/// `HH:MM:SS.mmm` or `MM:SS.mmm`, in milliseconds.
fn timestamp(s: &str) -> Option<u64> {
    let (clock, ms) = s.split_once('.')?;
    let mut parts = clock.split(':').rev();
    let seconds: u64 = parts.next()?.parse().ok()?;
    let minutes: u64 = parts.next()?.parse().ok()?;
    let hours: u64 = parts.next().map_or(Some(0), |h| h.parse().ok())?;
    let ms: u64 = format!("{:0<3}", ms.chars().take(3).collect::<String>()).parse().ok()?;
    Some(((hours * 60 + minutes) * 60 + seconds) * 1000 + ms)
}

/// The cues of a WebVTT file ffmpeg is writing. Its muxer puts the blank line before each cue and writes a cue in one
/// go, so a file that ends in the middle of a line has a cue being written, which the next look reads; a cue with no
/// text yet is the same. Notes, styles and regions are not cues.
pub fn parse_cues(text: &str) -> Vec<Cue> {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let mut blocks: Vec<&str> = text.split("\n\n").collect();
    if !text.ends_with('\n') {
        blocks.pop();
    }
    blocks
        .into_iter()
        .filter_map(|block| {
            let block = block.trim_matches('\n');
            let mut lines = block.lines().skip_while(|l| !l.contains("-->"));
            let timing = lines.next()?;
            if lines.all(|l| l.trim().is_empty()) {
                return None;
            }
            if matches!(block.split_whitespace().next(), Some("WEBVTT" | "NOTE" | "STYLE" | "REGION")) {
                return None;
            }
            let (start, rest) = timing.split_once("-->")?;
            let (start, end) = (timestamp(start.trim())?, timestamp(rest.split_whitespace().next()?)?);
            Some(Cue { start, end, block: block.to_string() })
        })
        .collect()
}

/// What one own track has so far: the cues its runs wrote, and the stretches of the film those runs have read past.
#[derive(Default)]
struct TrackCues {
    cues: BTreeSet<Cue>,
    covered: Vec<(f64, f64)>,
}

/// The release's own tracks as its runs have written them, for the session's life. A seek starts another run, which
/// writes cues from where it starts; the cues of the runs before it stay, as does the record of what they covered.
#[derive(Default)]
pub struct OwnCues {
    tracks: HashMap<usize, TrackCues>,
    /// How much of each run's file was read last time, so an unchanged one is not parsed again.
    read: HashMap<(u32, usize), u64>,
}

impl OwnCues {
    /// Take what run `job` has written to `path` for track `track` and note that it has read the film from `from` to
    /// `upto`. Cues past `upto` are kept as well: the demuxer is a little ahead of the video it has finished.
    pub fn take(&mut self, job: u32, track: usize, path: &std::path::Path, from: f64, upto: f64) {
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let entry = self.tracks.entry(track).or_default();
        if len > 0 && len <= MAX_OWN && self.read.get(&(job, track)) != Some(&len) {
            if let Ok(text) = std::fs::read_to_string(path) {
                entry.cues.extend(parse_cues(&text));
                self.read.insert((job, track), len);
            }
        }
        if upto > from {
            entry.covered.push((from, upto));
            entry.covered.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut merged: Vec<(f64, f64)> = Vec::with_capacity(entry.covered.len());
            for &(a, b) in &entry.covered {
                match merged.last_mut() {
                    Some(last) if a <= last.1 + SNAP => last.1 = last.1.max(b),
                    _ => merged.push((a, b)),
                }
            }
            entry.covered = merged;
        }
    }

    /// Have the runs read all of `from`..`to` for `track`?
    pub fn covers(&self, track: usize, from: f64, to: f64) -> bool {
        self.tracks
            .get(&track)
            .is_some_and(|t| t.covered.iter().any(|(a, b)| *a <= from + SNAP && *b >= to - SNAP))
    }

    /// The WebVTT segment for `from`..`to`: every cue showing at any moment of it, on the media timeline.
    pub fn window(&self, track: usize, from: f64, to: f64) -> String {
        let (from, to) = ((from * 1000.0) as u64, (to * 1000.0) as u64);
        let mut out = String::from(EMPTY);
        for cue in self.tracks.get(&track).into_iter().flat_map(|t| &t.cues) {
            if cue.start < to && cue.end.max(cue.start + 1) > from {
                out.push_str(&cue.block);
                out.push_str("\n\n");
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_size_plus_both_ends() {
        let mut head = vec![0u8; HASH_CHUNK as usize];
        head[..8].copy_from_slice(&1u64.to_le_bytes());
        let tail = vec![0xFFu8; HASH_CHUNK as usize];
        // The tail's 8192 words of u64::MAX wrap to -8192.
        let tail_sum = chunk_sum(&tail);
        assert_eq!(tail_sum, 0u64.wrapping_sub(8192));
        assert_eq!(
            movie_hash(1 << 30, chunk_sum(&head), tail_sum),
            format!("{:016x}", (1u64 << 30) + 1 - 8192)
        );
    }

    #[test]
    fn the_list_url_carries_the_hints_as_one_segment() {
        let u = list_url("http://subs.lan:8093/c2Vj", "tt1:2:3", Some("00ff"), Some(42), "A/B&C=D Film.mkv")
            .unwrap();
        assert_eq!(
            u,
            "http://subs.lan:8093/c2Vj/subtitles/series/tt1:2:3/videoHash=00ff&videoSize=42&filename=A%20B%20C%20D%20Film.mkv.json"
        );
        let u = list_url("http://subs.lan:8093/c2Vj", "tt1", None, None, "f.mkv").unwrap();
        assert!(u.ends_with("/subtitles/movie/tt1/filename=f.mkv.json"), "{u}");
    }

    #[test]
    fn a_subtitle_url_is_fetched_as_vtt_and_only_from_an_allowed_origin() {
        assert_eq!(vtt_url("http://s/c/subtitle/9.srt?lang=fin"), "http://s/c/subtitle/9.vtt?lang=fin");
        assert_eq!(vtt_url("http://s/c/subtitle/9.srt"), "http://s/c/subtitle/9.vtt");
        let allowed = ["http://192.168.86.193:8093".to_string()];
        assert!(on_origin("http://192.168.86.193:8093/c/subtitle/9.vtt?x=1", &allowed));
        assert!(!on_origin("http://169.254.169.254/latest", &allowed));
        assert!(!on_origin("http://u@192.168.86.193:8093/c", &allowed));
    }

    fn track(codec: &str, language: Option<&str>, text: bool, forced: bool) -> SubtitleTrack {
        SubtitleTrack { codec: codec.into(), language: language.map(String::from), text, forced }
    }

    #[test]
    fn a_language_the_release_carries_is_offered_and_one_it_lacks_is_not() {
        let tracks = [
            track("S_HDMV/PGS", Some("eng"), false, false),
            track("S_TEXT/UTF8", Some("swe"), true, true),
            track("S_TEXT/ASS", Some("fin"), true, false),
            track("S_TEXT/UTF8", Some("eng"), true, false),
            track("S_TEXT/UTF8", Some("eng"), true, false),
            track("S_TEXT/UTF8", None, true, false),
        ];
        let named = ["fi".to_string(), "de".to_string()];
        // No den-subtitles: only what the release has, the languages asked for first, then its others. Forced, bitmap
        // and unlabelled tracks make no rendition, and a language with two tracks takes the first.
        assert_eq!(
            plan(&named, false, &tracks),
            [
                Rendition { lang: "fi".into(), den: false, own: Some(2) },
                Rendition { lang: "en".into(), den: false, own: Some(3) },
            ]
        );
        // With it, every language asked for is a rendition — den-subtitles may have it — and the release's own track is
        // its fallback where there is one.
        assert_eq!(
            plan(&named, true, &tracks),
            [
                Rendition { lang: "fi".into(), den: true, own: Some(2) },
                Rendition { lang: "de".into(), den: true, own: None },
                Rendition { lang: "en".into(), den: false, own: Some(3) },
            ]
        );
        assert!(plan(&[], false, &[track("S_HDMV/PGS", Some("eng"), false, false)]).is_empty());
    }

    #[test]
    fn the_releases_languages_stop_at_the_most_a_session_names() {
        let tracks: Vec<_> = ["eng", "fin", "swe", "deu", "fra", "spa", "ita", "por", "nor", "dan"]
            .iter()
            .map(|l| track("S_TEXT/UTF8", Some(l), true, false))
            .collect();
        assert_eq!(plan(&[], false, &tracks).len(), MAX_LANGUAGES);
    }

    #[test]
    fn ffmpegs_webvtt_is_read_cue_by_cue_and_a_half_written_cue_waits() {
        // As the muxer writes it: the blank line before each cue, minutes and seconds until an hour.
        let file = "WEBVTT\n\n00:01.000 --> 00:03.000\nHello\n\n\
                    1:02:03.500 --> 1:02:04.000 line:90%\n<i>Later</i>\nlines\n";
        let cues = parse_cues(file);
        assert_eq!(cues.len(), 2, "{cues:?}");
        assert_eq!((cues[0].start, cues[0].end), (1000, 3000));
        assert_eq!(cues[0].block, "00:01.000 --> 00:03.000\nHello");
        assert_eq!(cues[1].start, (62 * 60 + 3) * 1000 + 500, "an hour, unpadded");
        assert!(cues[1].block.contains("<i>Later</i>\nlines"), "{}", cues[1].block);
        assert_eq!(parse_cues(&format!("{file}\n00:09.000 --> 00:10.000\n")).len(), 2, "no text yet");
        assert_eq!(parse_cues(&format!("{file}\n00:09.000 --> 00:1")).len(), 2, "cut mid-line");
        assert_eq!(parse_cues(&format!("{file}\n00:09.000 --> 00:10.000\nHalf\n")).len(), 3);
        assert!(parse_cues("WEBVTT\n\nNOTE a note --> here\n\n").is_empty());
        assert!(parse_cues("").is_empty());
    }

    #[test]
    fn a_window_holds_the_cues_showing_in_it_and_only_once_covered() {
        let dir = std::env::temp_dir().join(format!("den-remux-own-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t0.vtt");
        std::fs::write(
            &file,
            "WEBVTT\n\n00:01.000 --> 00:03.000\nfirst\n\n00:07.000 --> 00:09.000\nacross\n\n\
             00:20.000 --> 00:21.000\nlate\n",
        )
        .unwrap();
        let mut own = OwnCues::default();
        assert!(!own.covers(0, 0.0, 8.0), "nothing read yet");
        own.take(1, 0, &file, 0.0, 12.0);
        assert!(own.covers(0, 0.0, 8.0) && own.covers(0, 8.0, 12.0));
        assert!(!own.covers(0, 8.0, 13.0) && !own.covers(1, 0.0, 8.0));
        let first = own.window(0, 0.0, 8.0);
        assert!(first.starts_with(EMPTY), "the timestamp map, as den-subtitles' documents have: {first}");
        assert!(first.contains("first") && first.contains("across") && !first.contains("late"), "{first}");
        let second = own.window(0, 8.0, 13.0);
        assert!(
            second.contains("across") && !second.contains("first"),
            "a cue over the cut is in both: {second}"
        );
        // A second run that reads the same cues again, and further, adds no duplicate and joins the coverage.
        own.take(2, 0, &file, 10.0, 30.0);
        assert!(own.covers(0, 0.0, 30.0));
        assert_eq!(own.window(0, 0.0, 30.0).matches("across").count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_vtt_gets_its_timestamp_map_and_anything_else_is_refused() {
        let v = for_hls("\u{feff}WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHi\n".as_bytes()).unwrap();
        assert!(v.starts_with("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n\n00:00:01.000"), "{v}");
        assert!(for_hls(b"1\n00:00:01,000 --> 00:00:02,000\nHi\n").is_none(), "SRT is not WebVTT");
        assert!(EMPTY.starts_with("WEBVTT\n"));
    }
}
