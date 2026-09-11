//! The stream source: this service's own den-scout install.
//!
//! den-remux holds a scout install of its own, server-side (`SCOUT_INSTALL_URL`), so the browser never
//! holds a config that can list or play anything — it asks for a title, and gets back a signed session
//! for one release. Scout's ranking is kept as-is: the first release that is cached on the debrid, in
//! a codec the browser can take copied, and that probes cleanly, wins.

use serde::Deserialize;

#[derive(Deserialize)]
struct StreamList {
    #[serde(default)]
    streams: Vec<Stream>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Stream {
    #[serde(default)]
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub attributes: Attributes,
    #[serde(default, rename = "behaviorHints")]
    pub hints: Hints,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct Attributes {
    pub codec: Option<String>,
    /// Three answers, as scout sends them: held by the debrid, not held, or nobody could ask (absent).
    pub cached: Option<bool>,
    #[serde(rename = "sizeBytes")]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub label: String,
    #[serde(default, rename = "threeD")]
    pub three_d: bool,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct Hints {
    pub filename: Option<String>,
}

impl Stream {
    /// The release name: scout's `behaviorHints.filename`, else the stream title.
    pub fn filename(&self) -> &str {
        self.hints.filename.as_deref().unwrap_or(&self.title)
    }
}

pub fn is_imdb(id: &str) -> bool {
    id.len() >= 3 && id.len() <= 12 && id.starts_with("tt") && id[2..].bytes().all(|c| c.is_ascii_digit())
}

pub fn stream_list_url(install: &str, imdb: &str) -> String {
    format!("{install}/stream/movie/{imdb}.json")
}

pub fn parse(body: &[u8]) -> Result<Vec<Stream>, String> {
    serde_json::from_slice::<StreamList>(body)
        .map(|l| l.streams)
        .map_err(|e| format!("not a stream list: {e}"))
}

/// Could the browser take this release with only the audio re-encoded? Cached — an uncached one would
/// start a debrid download and play nothing — and H.264 or HEVC, or a codec the title does not name
/// (the probe decides those). AV1, VP9, MPEG-4 Part 2 and VC-1 need the video re-encoded, which this
/// service does not do. Containers other than Matroska and MP4 are left out by name.
fn remuxable(s: &Stream) -> bool {
    let codec_ok = match s.attributes.codec.as_deref().map(str::to_ascii_lowercase) {
        None => true,
        Some(c) => c == "h264" || c == "hevc",
    };
    let name = s.filename().to_ascii_lowercase();
    let container_ok = ![".avi", ".ts", ".m2ts", ".iso", ".wmv", ".mpg", ".mpeg", ".vob", ".webm"]
        .iter()
        .any(|ext| name.ends_with(ext));
    s.attributes.cached == Some(true) && codec_ok && container_ok && !s.attributes.three_d
}

/// The releases worth trying, in the order to try them: the one the browser named first when it is
/// playable here, then scout's order.
pub fn candidates(streams: &[Stream], filename: Option<&str>) -> Vec<Stream> {
    let mut out: Vec<Stream> = streams.iter().filter(|s| remuxable(s)).cloned().collect();
    if let Some(want) = filename {
        if let Some(i) = out.iter().position(|s| s.filename() == want) {
            let chosen = out.remove(i);
            out.insert(0, chosen);
        }
    }
    out
}

/// How much of the file to read when resolving it. Enough for the Matroska SeekHead, Info and Tracks,
/// or a faststart `moov` for a short file; anything larger is fetched by what points at it.
pub const HEAD_BYTES: u64 = 256 * 1024;

pub struct Resolved {
    /// Where the play URL led — the debrid's link. ffmpeg reads this directly, so a seek does not
    /// go back through scout.
    pub url: String,
    pub head: Vec<u8>,
    pub size: Option<u64>,
}

/// Follow a play URL (scout's `/p/<ticket>`, a 302 to the debrid) and read the head of the file in the
/// same request.
pub async fn resolve(client: &reqwest::Client, play_url: &str) -> Result<Resolved, String> {
    let resp = client
        .get(play_url)
        .header(reqwest::header::RANGE, format!("bytes=0-{}", HEAD_BYTES - 1))
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    let status = resp.status().as_u16();
    // Scout answers an uncached or failed resolve with JSON, not media.
    let json = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("json"));
    if !(status == 200 || status == 206) || json {
        return Err(format!(
            "the play URL answered {status}{}",
            if json { " (JSON, not media)" } else { "" }
        ));
    }
    let size = match status {
        206 => resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next())
            .and_then(|t| t.parse().ok()),
        _ => resp.content_length(),
    };
    let url = resp.url().to_string();
    let head = crate::probe::read_capped(resp, HEAD_BYTES).await?;
    Ok(Resolved { url, head, size })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<Stream> {
        parse(include_bytes!("../testdata/scout-streams.json")).expect("fixture parses")
    }

    #[test]
    fn imdb_ids_are_validated() {
        assert!(is_imdb("tt0111161"));
        assert!(!is_imdb("tt"));
        assert!(!is_imdb("tt01x"));
        assert!(!is_imdb("../etc"));
        assert!(!is_imdb("tt0111161:1:2"), "movies only");
    }

    #[test]
    fn only_cached_remuxable_releases_are_candidates() {
        let names: Vec<String> =
            candidates(&fixture(), None).iter().map(|s| s.filename().to_string()).collect();
        assert_eq!(
            names,
            [
                "Film.2019.2160p.UHD.BluRay.REMUX.HEVC.TrueHD.7.1.mkv",
                "Film.2019.1080p.WEB-DL.DDP5.1.H.264.mkv",
                "Film.2019.1080p.BluRay.mkv",
            ],
            "scout's order, minus uncached, cache-unknown, AV1, XviD, AVI and 3D"
        );
    }

    #[test]
    fn a_named_release_goes_first_when_it_is_playable() {
        let pick = candidates(&fixture(), Some("Film.2019.1080p.BluRay.mkv"));
        assert_eq!(pick[0].filename(), "Film.2019.1080p.BluRay.mkv");
        assert_eq!(pick.len(), 3, "the others stay as fallbacks");
        // Named but uncached: scout's best cached one instead.
        let pick = candidates(&fixture(), Some("Film.2019.1080p.WEB.x264-UNCACHED.mkv"));
        assert_eq!(pick[0].filename(), "Film.2019.2160p.UHD.BluRay.REMUX.HEVC.TrueHD.7.1.mkv");
    }

    #[test]
    fn the_stream_fields_parse() {
        let s = &fixture()[1];
        assert_eq!(s.attributes.size_bytes, Some(58_000_000_000));
        assert!(s.url.contains("/p/"));
        assert!(!s.attributes.label.is_empty());
    }
}
