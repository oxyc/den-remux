//! Subtitles from den-subtitles, as HLS WebVTT renditions — what AirPlay and Cast receivers show, which a
//! page's own `<track>` never reaches.
//!
//! The master playlist is fixed when the session starts, so it names one rendition per language the
//! browser asked for, and nothing is fetched until a player opens one. Then den-remux asks den-subtitles
//! for the title with the release's OpenSubtitles hash, size and filename — the hints the Apple TV sends,
//! so an exact-encode match (in sync by construction) ranks first — and serves the first subtitle in that
//! language as one WebVTT segment spanning the film. A language with nothing to offer serves an empty
//! document rather than an error: the playlist has already promised it.

use serde::Deserialize;

/// OpenSubtitles' hash reads this much from each end of the file.
pub const HASH_CHUNK: u64 = 64 * 1024;
/// Renditions per session.
pub const MAX_LANGUAGES: usize = 4;
/// Larger than any real subtitle file.
pub const MAX_SUBTITLE: u64 = 4 << 20;
pub const MAX_LIST: u64 = 1 << 20;
/// What a rendition serves when there is nothing for its language.
pub const EMPTY: &str = "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n\n";

/// The subtitle side of a session.
pub struct Subs {
    /// den-subtitles' install URL, sealed config included — a secret.
    pub base: String,
    /// The languages asked for, as `lang::canonical` gives them, in rendition order.
    pub langs: Vec<String>,
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

    #[test]
    fn a_vtt_gets_its_timestamp_map_and_anything_else_is_refused() {
        let v = for_hls("\u{feff}WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHi\n".as_bytes()).unwrap();
        assert!(v.starts_with("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n\n00:00:01.000"), "{v}");
        assert!(for_hls(b"1\n00:00:01,000 --> 00:00:02,000\nHi\n").is_none(), "SRT is not WebVTT");
        assert!(EMPTY.starts_with("WEBVTT\n"));
    }
}
