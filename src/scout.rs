//! The stream source: den-scout.
//!
//! A session's releases come from the scout install the web app names — its library's full install URL,
//! or, for a logged-in browser, a scope=availability one that scout honours only with `X-Den-Remux-Key` —
//! or, as a fallback, this service's own `SCOUT_INSTALL_URL`. The browser never gets a ticket or a debrid link: it asks for a
//! title and gets back a signed session for one release. Scout's ranking is re-ranked for a phone
//! (`phone_first`), and the first release that is cached on the debrid, probes cleanly and plays in the browser
//! as it is wins; one it can only have converted is the last resort.

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
    /// `2160p`, `1080p`, … as the release name gives it.
    pub resolution: Option<String>,
    /// HDR by the name or the file; Dolby Vision counts.
    #[serde(default)]
    pub hdr: bool,
    #[serde(default, rename = "dolbyVision")]
    pub dolby_vision: bool,
    /// 8 or 10, 0 when nobody read it. The name supplies 10 for "10bit"/"Hi10P" and for any HDR or Dolby Vision
    /// release; scout's probe replaces that with what the codec's configuration record says.
    #[serde(default, rename = "bitDepth")]
    pub bit_depth: u32,
    /// The Dolby Vision profile scout's probe read from the file (5, 7, 8); 0 when unknown, never "none".
    #[serde(default, rename = "dvProfile")]
    pub dv_profile: u32,
    /// Scout read the file itself, so the codec, depth and profile above are the file's, not the name's.
    #[serde(default)]
    pub probed: bool,
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

/// Scout's id for a title: the IMDb id for a movie, `<imdb>:<season>:<episode>` for an episode.
pub fn title_id(imdb: &str, episode: Option<(u32, u32)>) -> String {
    match episode {
        Some((s, e)) => format!("{imdb}:{s}:{e}"),
        None => imdb.to_string(),
    }
}

/// The Stremio type a title id names.
pub fn kind(id: &str) -> &'static str {
    if id.contains(':') {
        "series"
    } else {
        "movie"
    }
}

pub fn stream_list_url(install: &str, id: &str) -> String {
    format!("{install}/stream/{}/{id}.json", kind(id))
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
/// playable here, then scout's order as `phone_first` ranks it.
pub fn candidates(streams: &[Stream], filename: Option<&str>) -> Vec<Stream> {
    let mut out: Vec<Stream> = streams.iter().filter(|s| remuxable(s)).cloned().collect();
    phone_first(&mut out);
    if let Some(want) = filename {
        if let Some(i) = out.iter().position(|s| s.filename() == want) {
            let chosen = out.remove(i);
            out.insert(0, chosen);
        }
    }
    out
}

/// Scout ranks for a TV, best first. Playback here is a phone's or a laptop's, often over the tailnet from outside
/// the house, so a 1080p release comes before a 720p or unnamed one and those before 4K, and within each a web
/// release before a remux and one without Dolby Vision before one with: smaller, and far more often in a form a
/// browser plays as it is, where a UHD Blu-ray remux is High tier HEVC it can only have converted. Stable, so
/// scout's order holds within each.
pub fn phone_first(c: &mut [Stream]) {
    c.sort_by_key(|s| {
        let text = format!("{} {}", s.attributes.label, s.filename()).to_ascii_lowercase();
        let has = |words: &[&str]| words.iter().any(|w| text.contains(w));
        let resolution = match () {
            _ if has(&["2160p", "4k", "uhd"]) => 2,
            _ if has(&["1080p"]) => 0,
            _ => 1,
        };
        let dolby_vision = s.attributes.dolby_vision || has(&["dolby vision", "dovi", ".dv.", " dv "]);
        (resolution, has(&["remux"]), dolby_vision)
    });
}

/// For a player that cannot take HEVC: H.264 releases first, then those scout named no codec for, then
/// HEVC, which needs the GPU. Stable, so scout's order holds within each.
pub fn h264_first(c: &mut [Stream]) {
    c.sort_by_key(|s| match s.attributes.codec.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("h264") => 0,
        None => 1,
        _ => 2,
    });
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

/// The header a scope=availability scout config asks for before it lists or plays.
pub const KEY_HEADER: &str = "x-den-remux-key";

/// Where a session's releases come from: a scout base URL (config segment included) and the service key
/// to present to it.
pub struct ScoutSource {
    pub base: String,
    pub key: Option<String>,
}

impl ScoutSource {
    /// Is `url` on this scout's origin — the only place the key may be sent?
    fn is_scout(&self, url: &reqwest::Url) -> bool {
        reqwest::Url::parse(&self.base).is_ok_and(|b| b.origin() == url.origin())
    }
}

/// A scout base URL a browser handed over, accepted only as `<allowed origin>/<config>`: an origin in
/// `SCOUT_ORIGINS`, exactly one path segment of base64url (den-scout's sealed config alphabet), and no
/// credentials, query or fragment. This is the SSRF guard — without it a logged-in browser could make
/// this service fetch any URL on the LAN. Returns the normalised base.
pub fn validate_scoped(url: &str, origins: &[String]) -> Result<String, &'static str> {
    if url.len() > 4096 {
        return Err("too long");
    }
    if url.contains(['?', '#']) {
        return Err("a query or fragment");
    }
    let (scheme, rest) = url.split_once("://").ok_or("not an absolute URL")?;
    let (authority, config) = rest.split_once('/').ok_or("no config segment")?;
    if authority.contains('@') {
        return Err("credentials in the URL");
    }
    let origin = format!("{}://{}", scheme.to_ascii_lowercase(), authority.to_ascii_lowercase());
    if !origins.contains(&origin) {
        return Err("an origin not in SCOUT_ORIGINS");
    }
    // One segment only: this also refuses a trailing slash and any further path.
    if config.is_empty() || !config.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
        return Err("not a single base64url config segment");
    }
    Ok(format!("{origin}/{config}"))
}

/// Why a stream list could not be had: scout's status when it answered one, and what happened.
pub struct ListError {
    pub status: Option<u16>,
    pub detail: String,
}

/// The stream list for `imdb`, with the service key when the source has one. `client` must not follow
/// redirects: one would carry the key off scout's origin.
pub async fn list(client: &reqwest::Client, src: &ScoutSource, imdb: &str) -> Result<Vec<Stream>, ListError> {
    let fail = |status: Option<u16>, detail: String| ListError { status, detail };
    let mut req = client.get(stream_list_url(&src.base, imdb));
    if let Some(k) = &src.key {
        req = req.header(KEY_HEADER, k);
    }
    let resp = req.send().await.map_err(|e| fail(None, e.without_url().to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        return Err(fail(Some(status), format!("scout answered {status}")));
    }
    let body = crate::probe::read_capped(resp, 8 << 20).await.map_err(|d| fail(None, d))?;
    parse(&body).map_err(|d| fail(None, d))
}

/// How many redirects a play URL may take to reach the file. Scout's is one 302; a debrid may add one.
const MAX_HOPS: usize = 5;

/// Follow a play URL (scout's `/p/<ticket>`, a 302 to the debrid) and read the head of the file in the
/// same request.
///
/// Redirects are followed here rather than by the client, because the service key must reach scout
/// and nothing else: an HTTP client forwards custom headers across a cross-origin redirect, which would
/// hand `X-Den-Remux-Key` to the debrid's CDN. `client` must not follow redirects itself.
pub async fn resolve(
    client: &reqwest::Client,
    play_url: &str,
    src: &ScoutSource,
) -> Result<Resolved, String> {
    let mut url = reqwest::Url::parse(play_url).map_err(|_| "the play URL does not parse".to_string())?;
    let mut hops = 0;
    let resp = loop {
        let mut req =
            client.get(url.clone()).header(reqwest::header::RANGE, format!("bytes=0-{}", HEAD_BYTES - 1));
        if let Some(k) = src.key.as_deref().filter(|_| src.is_scout(&url)) {
            req = req.header(KEY_HEADER, k);
        }
        let resp = req.send().await.map_err(|e| e.without_url().to_string())?;
        if !resp.status().is_redirection() {
            break resp;
        }
        hops += 1;
        let next = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|l| url.join(l).ok());
        match next {
            Some(n) if hops <= MAX_HOPS => url = n,
            _ => return Err(format!("the play URL redirected badly ({})", resp.status().as_u16())),
        }
    };
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
        assert!(!is_imdb("tt0111161:1:2"), "an episode comes as season and episode fields");
    }

    #[test]
    fn an_episode_lists_from_scouts_series_route() {
        let base = "http://scout/cfg";
        assert_eq!(stream_list_url(base, &title_id("tt1", None)), "http://scout/cfg/stream/movie/tt1.json");
        assert_eq!(
            stream_list_url(base, &title_id("tt1", Some((2, 5)))),
            "http://scout/cfg/stream/series/tt1:2:5.json"
        );
    }

    #[test]
    fn only_cached_remuxable_releases_are_candidates() {
        let names: Vec<String> =
            candidates(&fixture(), None).iter().map(|s| s.filename().to_string()).collect();
        assert_eq!(
            names,
            [
                "Film.2019.1080p.WEB-DL.DDP5.1.H.264.mkv",
                "Film.2019.1080p.BluRay.mkv",
                "Film.2019.2160p.UHD.BluRay.REMUX.HEVC.TrueHD.7.1.mkv",
            ],
            "scout's order ranked for a phone, minus uncached, cache-unknown, AV1, XviD, AVI and 3D"
        );
    }

    #[test]
    fn a_phone_gets_1080p_before_4k_and_web_before_remux_or_dolby_vision() {
        let stream = |label: &str| Stream {
            title: String::new(),
            url: String::new(),
            attributes: Attributes { label: label.into(), ..Attributes::default() },
            hints: Hints::default(),
        };
        let mut c: Vec<Stream> = [
            "4K • REMUX • Dolby Vision • Atmos • 75 GB",
            "4K • WEB-DL • 18 GB",
            "1080p • REMUX • 30 GB",
            "1080p • WEB-DL • Dolby Vision • 6 GB",
            "720p • WEB-DL • 2 GB",
            "1080p • WEB-DL • 4 GB",
        ]
        .map(stream)
        .to_vec();
        phone_first(&mut c);
        let labels: Vec<&str> = c.iter().map(|s| s.attributes.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "1080p • WEB-DL • 4 GB",
                "1080p • WEB-DL • Dolby Vision • 6 GB",
                "1080p • REMUX • 30 GB",
                "720p • WEB-DL • 2 GB",
                "4K • WEB-DL • 18 GB",
                "4K • REMUX • Dolby Vision • Atmos • 75 GB",
            ]
        );
    }

    #[test]
    fn a_player_without_hevc_gets_h264_releases_first() {
        let mut c = candidates(&fixture(), None);
        h264_first(&mut c);
        assert_eq!(c[0].filename(), "Film.2019.1080p.WEB-DL.DDP5.1.H.264.mkv");
        assert_eq!(c.last().unwrap().filename(), "Film.2019.2160p.UHD.BluRay.REMUX.HEVC.TrueHD.7.1.mkv");
    }

    #[test]
    fn a_named_release_goes_first_when_it_is_playable() {
        let pick = candidates(&fixture(), Some("Film.2019.1080p.BluRay.mkv"));
        assert_eq!(pick[0].filename(), "Film.2019.1080p.BluRay.mkv");
        assert_eq!(pick.len(), 3, "the others stay as fallbacks");
        // Named but uncached: the best cached one for a phone instead.
        let pick = candidates(&fixture(), Some("Film.2019.1080p.WEB.x264-UNCACHED.mkv"));
        assert_eq!(pick[0].filename(), "Film.2019.1080p.WEB-DL.DDP5.1.H.264.mkv");
        // Named 4K: it still goes first, ahead of the ranking.
        let pick = candidates(&fixture(), Some("Film.2019.2160p.UHD.BluRay.REMUX.HEVC.TrueHD.7.1.mkv"));
        assert_eq!(pick[0].filename(), "Film.2019.2160p.UHD.BluRay.REMUX.HEVC.TrueHD.7.1.mkv");
    }

    #[test]
    fn a_scoped_scout_url_must_be_an_allowed_origin_and_one_config_segment() {
        let allowed = ["http://192.168.86.193:8080".to_string()];
        let ok = |u: &str| validate_scoped(u, &allowed);
        assert_eq!(
            ok("http://192.168.86.193:8080/eyJ2IjoxfQ-_x"),
            Ok("http://192.168.86.193:8080/eyJ2IjoxfQ-_x".into())
        );
        assert_eq!(
            ok("HTTP://192.168.86.193:8080/abc"),
            Ok("http://192.168.86.193:8080/abc".into()),
            "normalised"
        );
        for refused in [
            "http://192.168.86.193:8081/abc",          // another port
            "https://192.168.86.193:8080/abc",         // another scheme
            "http://169.254.169.254/abc",              // anywhere else
            "http://x@192.168.86.193:8080/abc",        // credentials
            "http://192.168.86.193:8080@evil.lan/abc", // credentials disguising the host
            "http://192.168.86.193:8080/abc?x=1",      // query
            "http://192.168.86.193:8080/abc#f",        // fragment
            "http://192.168.86.193:8080/abc/stream",   // two segments
            "http://192.168.86.193:8080/abc/",         // trailing slash
            "http://192.168.86.193:8080/",             // no config
            "http://192.168.86.193:8080",              // no path at all
            "http://192.168.86.193:8080/%2e%2e",       // not base64url
            "192.168.86.193:8080/abc",                 // not absolute
        ] {
            assert!(ok(refused).is_err(), "{refused} was accepted");
        }
        assert!(
            validate_scoped("http://192.168.86.193:8080/abc", &[]).is_err(),
            "no origins, no scoped URLs"
        );
    }

    #[test]
    fn the_stream_fields_parse() {
        let s = &fixture()[1];
        assert_eq!(s.attributes.size_bytes, Some(58_000_000_000));
        assert!(s.url.contains("/p/"));
        assert!(!s.attributes.label.is_empty());
    }
}
