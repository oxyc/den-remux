//! Tests that need the fixtures in testdata/ or a whole AppState.
//!
//! The probe tests run everywhere. The `#[ignore]`d ones run real ffmpeg against a local origin that
//! plays scout (a stream list, a 302 play URL, Range-served files). They count only with the ffmpeg the
//! image ships, since the segment alignment depends on how it seeks: `docker build --target test .`.

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, StatusCode};

use crate::config::Config;
use crate::probe::{self, Source, VideoCodec};
use crate::state::AppState;

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name)
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(testdata(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// ffprobe's keyframe lists for the fixtures (testdata/README.md records the command).
const H264_MKV_KF: [f64; 11] = [0.0, 2.5, 5.0, 8.0, 10.5, 13.0, 16.0, 19.5, 22.0, 24.0, 27.5];
const HEVC_MKV_KF: [f64; 10] = [0.0, 3.0, 6.5, 9.0, 12.0, 15.5, 18.0, 21.0, 24.5, 27.0];
const H264_MP4_KF: [f64; 10] = [0.0, 3.5, 6.0, 9.0, 11.5, 15.0, 18.0, 20.5, 24.0, 27.0];
const MOOV_END_KF: [f64; 3] = [0.0, 2.0, 5.0];

fn assert_keyframes(got: &[f64], want: &[f64]) {
    assert_eq!(got.len(), want.len(), "keyframes: got {got:?}, want {want:?}");
    for (g, w) in got.iter().zip(want) {
        assert!((g - w).abs() < 1e-3, "keyframes: got {got:?}, want {want:?}");
    }
}

async fn probe_with_head(bytes: &[u8], head: usize) -> probe::MediaInfo {
    probe::probe(&Source::Mem(bytes), &bytes[..head.min(bytes.len())]).await.expect("probes")
}

#[tokio::test]
async fn matroska_cues_match_ffprobes_keyframes() {
    let bytes = fixture("h264.mkv");
    // The whole head, and a head too short to hold even Tracks — everything then comes through the
    // SeekHead, which is the path a real 60 GB file takes for its Cues.
    for head in [crate::scout::HEAD_BYTES as usize, 200] {
        let info = probe_with_head(&bytes, head).await;
        assert_keyframes(&info.keyframes, &H264_MKV_KF);
        assert_eq!(info.video, VideoCodec::H264);
        assert!(info.codecs.as_deref().is_some_and(|c| c.starts_with("avc1.64")), "{:?}", info.codecs);
        assert!((info.duration - 30.021).abs() < 1e-3, "{}", info.duration);
        assert_eq!((info.width, info.height), (320, 180));
        let langs: Vec<_> = info.audio.iter().map(|a| a.language.as_deref()).collect();
        assert_eq!(langs, [Some("eng"), Some("swe")]);
        assert_eq!(info.audio[0].codec, "A_AC3");
        assert_eq!(info.audio[1].channels, 1);
    }
}

#[tokio::test]
async fn an_hdr10_release_reads_as_hdr_and_is_tone_mapped() {
    let info = probe_with_head(&fixture("hdr10.mkv"), crate::scout::HEAD_BYTES as usize).await;
    assert_keyframes(&info.keyframes, &HEVC_MKV_KF);
    assert_eq!(info.video, VideoCodec::Hevc);
    assert!(
        info.codecs.as_deref().is_some_and(|c| c.starts_with("hvc1.2.4.L")),
        "Main 10: {:?}",
        info.codecs
    );
    assert!(info.hdr, "the PQ transfer");
    assert!(info.dolby_vision.is_none());
    assert!(crate::session::tonemaps(&info), "a conversion of it has to tone-map, or it plays nowhere");
}

#[tokio::test]
async fn hevc_matroska_gives_an_hvc1_codec_string() {
    let info = probe_with_head(&fixture("hevc.mkv"), crate::scout::HEAD_BYTES as usize).await;
    assert_keyframes(&info.keyframes, &HEVC_MKV_KF);
    assert_eq!(info.video, VideoCodec::Hevc);
    assert!(info.codecs.as_deref().is_some_and(|c| c.starts_with("hvc1.1.6.L")), "{:?}", info.codecs);
    assert_eq!(info.audio[0].codec, "A_EAC3");
    assert!(!info.hdr, "the fixture is SDR");
}

#[tokio::test]
async fn mp4_sample_tables_match_ffprobes_keyframes() {
    let info = probe_with_head(&fixture("h264.mp4"), crate::scout::HEAD_BYTES as usize).await;
    assert_keyframes(&info.keyframes, &H264_MP4_KF);
    assert_eq!(info.container, "mp4");
    assert!((info.duration - 30.0).abs() < 0.05, "{}", info.duration);
    assert_eq!(info.audio[0].language.as_deref(), Some("fra"));
    assert_eq!(info.audio[0].codec, "mp4a");
    assert_eq!(info.audio[0].channels, 2);
}

#[tokio::test]
async fn a_moov_after_the_media_is_found_by_walking_box_headers() {
    let bytes = fixture("moov-at-end.mp4");
    let info = probe_with_head(&bytes, 4096).await;
    assert_keyframes(&info.keyframes, &MOOV_END_KF);
    assert!((info.duration - 8.0).abs() < 0.05);
}

#[tokio::test]
async fn neither_container_is_refused() {
    let err = probe::probe(&Source::Mem(b"RIFF....AVI "), b"RIFF....AVI ").await.unwrap_err();
    assert!(matches!(err, probe::ProbeError::Unsupported(_)), "{err}");
}

// ---- integration: real ffmpeg ------------------------------------------------------------------

fn tool(env: &str, default: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| default.to_string())
}

fn temp_dir() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "den-remux-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A local origin standing in for scout and the debrid: `/<config>/stream/movie/<imdb>.json` lists an
/// uncached decoy and then the fixture; `/p/<name>` 302s to `/f/<name>` on a second port — another
/// origin, as a debrid always is — which serves the fixture with Range. `/p/slow/<name>` leads to a copy
/// that trickles out, for a job that is still running when the test wants it to be. Config `cfg` is an
/// ordinary install; config `SCOPED` behaves as den-scout's scope=availability config.
async fn origin() -> String {
    let scout = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let files = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (sa, fa) = (scout.local_addr().unwrap(), files.local_addr().unwrap());
    for listener in [scout, files] {
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { continue };
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |req| async move {
                        Ok::<_, Infallible>(origin_handle(sa, fa, req).await)
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .await;
                });
            }
        });
    }
    format!("http://{sa}")
}

type OriginBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

/// Play URLs the fake scout was asked to follow for a release its attributes rule out.
static NEVER_OPENED: AtomicU32 = AtomicU32::new(0);
/// Play URLs the fake scout was asked to follow under `/p/counted/`.
static COUNTED_PLAYS: AtomicU32 = AtomicU32::new(0);

/// The fake scout's scope=availability config segment ("scoped", base64url) and the service key it
/// asks for.
const SCOPED: &str = "c2NvcGVk";
const SCOUT_KEY: &str = "test-scout-key";

fn origin_full(status: u16, body: impl Into<Bytes>) -> Response<OriginBody> {
    Response::builder().status(status).body(Full::new(body.into()).boxed()).unwrap()
}

async fn origin_handle(
    addr: std::net::SocketAddr,
    files: std::net::SocketAddr,
    req: Request<hyper::body::Incoming>,
) -> Response<OriginBody> {
    let path = req.uri().path().to_string();
    let keyed = req.headers().get("x-den-remux-key").and_then(|v| v.to_str().ok()) == Some(SCOUT_KEY);
    // The key is for scout alone: the file host — the debrid, here — must never be sent it.
    if path.starts_with("/f/") && req.headers().contains_key("x-den-remux-key") {
        return origin_full(400, "the service key reached the file host");
    }
    // den-subtitles, install `subs`: an English subtitle, listed only with the file's hash and size; a
    // Finnish one on an origin den-remux must refuse.
    if let Some(extras) = path.strip_prefix("/subs/subtitles/movie/tt0000001/") {
        if !(extras.contains("videoHash=")
            && extras.contains("videoSize=")
            && extras.contains("filename=h264.mkv"))
        {
            return origin_full(200, r#"{"subtitles":[]}"#);
        }
        let body = serde_json::json!({"subtitles": [
            {"id": "1", "url": format!("http://{addr}/subs/subtitle/1.srt?lang=eng"), "lang": "eng"},
            {"id": "2", "url": "http://169.254.169.254/subs/subtitle/2.srt", "lang": "fin"}
        ]});
        return origin_full(200, body.to_string());
    }
    if path == "/subs/subtitle/1.vtt" {
        return origin_full(200, "WEBVTT\n\n00:00:01.000 --> 00:00:02.500\nHello\n");
    }
    let listing = path
        .strip_suffix(".json")
        .and_then(|p| p.strip_prefix('/'))
        .and_then(|p| p.split_once("/stream/movie/").or_else(|| p.split_once("/stream/series/")));
    if let Some((config, imdb)) = listing {
        // den-scout's contract for a scope=availability config: it lists only for the service key.
        let scoped = config == SCOPED;
        if scoped && !keyed {
            return origin_full(403, r#"{"error":"key_required"}"#);
        }
        if !scoped && config != "cfg" {
            return origin_full(400, r#"{"error":"bad_config"}"#);
        }
        let p = if scoped { "p/s" } else { "p" };
        if imdb == "tt0000005" {
            return origin_full(
                200,
                serde_json::json!({"streams": [
                    {"url": format!("http://{addr}/{p}/hevc.mkv"),
                     "attributes": {"cached": true, "codec": "hevc", "label": "HEVC"},
                     "behaviorHints": {"filename": "hevc.mkv"}},
                    {"url": format!("http://{addr}/{p}/h264.mkv"),
                     "attributes": {"cached": true, "codec": "h264", "label": "H.264"},
                     "behaviorHints": {"filename": "h264.mkv"}}
                ]})
                .to_string(),
            );
        }
        if imdb == "tt0000006" {
            // Scout's probe read Dolby Vision profile 5 off the first release: it must not be opened.
            let body = serde_json::json!({"streams": [
                {"url": format!("http://{addr}/{p}/never/hevc.mkv"),
                 "attributes": {"cached": true, "codec": "hevc", "dvProfile": 5, "dolbyVision": true, "probed": true,
                                "label": "DV5"},
                 "behaviorHints": {"filename": "dv5.mkv"}},
                {"url": format!("http://{addr}/{p}/h264.mkv"),
                 "attributes": {"cached": true, "codec": "h264", "label": "H.264"},
                 "behaviorHints": {"filename": "h264.mkv"}}
            ]});
            return origin_full(200, body.to_string());
        }
        if imdb == "tt0000010" {
            let body = serde_json::json!({"streams": [
                {"url": format!("http://{addr}/{p}/counted/h264.mkv"),
                 "attributes": {"cached": true, "codec": "h264", "label": "H.264"},
                 "behaviorHints": {"filename": "h264.mkv"}}
            ]});
            return origin_full(200, body.to_string());
        }
        if imdb == "tt0000008" {
            // Four releases that won't open, ahead of one that does.
            let release = |url: String, filename: String| {
                serde_json::json!({"url": url, "attributes": {"cached": true, "codec": "h264", "label": "x"},
                                   "behaviorHints": {"filename": filename}})
            };
            let gone = |i| release(format!("http://{addr}/{p}/missing{i}.mkv"), format!("gone{i}.mkv"));
            let mut streams: Vec<_> = (0..4).map(gone).collect();
            streams.push(release(format!("http://{addr}/{p}/h264.mkv"), "h264.mkv".into()));
            return origin_full(200, serde_json::json!({ "streams": streams }).to_string());
        }
        let file = match imdb {
            "tt0000001" => "h264.mkv",
            "tt0000002" => "hevc.mkv",
            "tt0000003" => "h264.mp4",
            "tt0000009" => "slow/h264.mkv",
            "tt0000004:1:2" if path.contains("/stream/series/") => "h264.mkv",
            _ => return origin_full(200, r#"{"streams":[]}"#),
        };
        let body = serde_json::json!({"streams": [
            {"title": "decoy.mkv", "url": format!("http://{addr}/{p}/missing.mkv"),
             "attributes": {"codec": "h264", "cached": false, "label": "uncached"}, "behaviorHints": {"filename": "decoy.mkv"}},
            {"title": file, "url": format!("http://{addr}/{p}/{file}"),
             "attributes": {"cached": true, "label": format!("fixture {file}")}, "behaviorHints": {"filename": file}}
        ]});
        return origin_full(200, body.to_string());
    }
    if let Some(rest) = path.strip_prefix("/p/") {
        // A scoped config's ticket plays only for the service key, too.
        let rest = match rest.strip_prefix("s/") {
            Some(_) if !keyed => return origin_full(403, r#"{"error":"key_required"}"#),
            Some(r) => r,
            None => rest,
        };
        if rest.starts_with("never/") {
            NEVER_OPENED.fetch_add(1, Relaxed);
        }
        let rest = match rest.strip_prefix("counted/") {
            Some(r) => {
                COUNTED_PLAYS.fetch_add(1, Relaxed);
                r
            }
            None => rest,
        };
        return Response::builder()
            .status(302)
            .header("location", format!("http://{files}/f/{rest}"))
            .body(Full::new(Bytes::new()).boxed())
            .unwrap();
    }
    let Some(rest) = path.strip_prefix("/f/") else { return origin_full(404, "") };
    let (slow, name) = match rest.strip_prefix("slow/") {
        Some(n) => (true, n),
        None => (false, rest),
    };
    let Ok(data) = std::fs::read(testdata(name)) else { return origin_full(404, "") };
    let size = data.len();
    let range =
        req.headers().get("range").and_then(|v| v.to_str().ok()).and_then(|r| r.strip_prefix("bytes=")).map(
            |r| {
                let (a, b) = r.split_once('-').unwrap();
                let a: usize = a.parse().unwrap();
                let b: usize = b.parse().map(|b: usize| b.min(size - 1)).unwrap_or(size - 1);
                (a, b)
            },
        );
    let (status, start, end) = match range {
        Some((a, _)) if a >= size => return origin_full(416, ""),
        Some((a, b)) => (206, a, b),
        None => (200, 0, size - 1),
    };
    let slice = Bytes::copy_from_slice(&data[start..=end]);
    let mut b = Response::builder()
        .status(status)
        .header("content-type", "video/x-matroska")
        .header("accept-ranges", "bytes")
        .header("content-length", slice.len());
    if status == 206 {
        b = b.header("content-range", format!("bytes {start}-{end}/{size}"));
    }
    if !slow || slice.len() < 64 * 1024 {
        return b.body(Full::new(slice).boxed()).unwrap();
    }
    // 32 KiB every 100 ms: the fixture takes a couple of seconds to read.
    let chunks: Vec<Bytes> = slice.chunks(32 * 1024).map(Bytes::copy_from_slice).collect();
    let stream = futures_util::stream::unfold(chunks.into_iter(), |mut it| async move {
        let c = it.next()?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        Some((Ok::<_, Infallible>(hyper::body::Frame::data(c)), it))
    });
    b.body(http_body_util::StreamBody::new(stream).boxed()).unwrap()
}

fn test_state(origin: &str, max_sessions: usize, idle: Duration) -> Arc<AppState> {
    state_with(origin, max_sessions, idle, Some(SCOUT_KEY))
}

fn state_with(origin: &str, max_sessions: usize, idle: Duration, scout_key: Option<&str>) -> Arc<AppState> {
    let dir = temp_dir();
    let state = AppState::new(Config {
        port: 0,
        scout_origins: vec![origin.to_string()],
        scout_key: scout_key.map(String::from),
        scout_install_url: Some(format!("{origin}/cfg")),
        subtitle_origins: vec![origin.to_string()],
        origin_aliases: Vec::new(),
        browser_key_hashes: vec![crate::auth::sha256(b"phone-key"), crate::auth::sha256(b"laptop-key")],
        url_key: b"integration-test-key".to_vec(),
        url_key_ephemeral: false,
        max_sessions,
        max_sessions_per_install: 2,
        session_idle: idle,
        scratch_dir: dir.clone(),
        scratch_max_bytes: 1 << 30,
        ffmpeg: tool("FFMPEG_PATH", "ffmpeg"),
        max_transcodes: 1,
        vaapi_device: PathBuf::from("/dev/dri/renderD128"),
        trusted_proxies: vec!["192.168.86.149".parse().unwrap()],
        web_origins: vec!["https://d.example".into()],
        metrics_token: None,
        log_requests: false,
    });
    state.scratch_ok.store(crate::state::check_scratch(&dir), Relaxed);
    state.ffmpeg_ok.store(true, Relaxed);
    state
}

struct Reply {
    status: StatusCode,
    headers: hyper::HeaderMap,
    body: Bytes,
}

impl Reply {
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("not JSON: {:?}", self.body))
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

async fn call(state: &Arc<AppState>, method: &str, path: &str, cookie: Option<&str>, body: &str) -> Reply {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(c) = cookie {
        b = b.header("cookie", c);
    }
    let resp =
        crate::handle_request(state.clone(), b.body(Full::new(Bytes::from(body.to_string()))).unwrap()).await;
    let (parts, body) = resp.into_parts();
    Reply { status: parts.status, headers: parts.headers, body: body.collect().await.unwrap().to_bytes() }
}

async fn login(state: &Arc<AppState>, key: &str) -> String {
    let r = call(state, "POST", "/remux/login", None, &format!(r#"{{"key":"{key}"}}"#)).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "login: {}", r.text());
    let set = r.headers.get("set-cookie").unwrap().to_str().unwrap();
    set.split(';').next().unwrap().to_string()
}

fn ffprobe(args: &[&str], file: &Path) -> String {
    let out = std::process::Command::new(tool("FFPROBE_PATH", "ffprobe"))
        .args(["-v", "error"])
        .args(args)
        .arg(file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ffprobe {args:?} {}: {}",
        file.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `(pts, is_keyframe)` of every video packet.
fn video_packets(file: &Path) -> Vec<(f64, bool)> {
    ffprobe(&["-select_streams", "v:0", "-show_entries", "packet=pts_time,flags", "-of", "csv=p=0"], file)
        .lines()
        .filter_map(|l| {
            let (pts, flags) = l.split_once(',')?;
            Some((pts.parse().ok()?, flags.contains('K')))
        })
        .collect()
}

fn extinfs(media: &str) -> Vec<f64> {
    media
        .lines()
        .filter_map(|l| l.strip_prefix("#EXTINF:"))
        .map(|v| v.trim_end_matches(',').parse().unwrap())
        .collect()
}

/// The whole path for one fixture: log in, create a session against the fake scout, fetch the
/// playlists, then fetch segments out of order so the job has to restart — first mid-file, then
/// behind itself, then at zero — and check every segment with ffprobe.
async fn end_to_end(imdb: &str, fixture_name: &str, codec_prefix: &str, scoped: bool) {
    let origin = origin().await;
    let state = test_state(&origin, 2, Duration::from_secs(600));
    let cookie = login(&state, "phone-key").await;
    // The scoped scout URL is the primary path; without one the server's own install is the fallback.
    let body = match scoped {
        true => format!(r#"{{"imdb":"{imdb}","scout":"{origin}/{SCOPED}"}}"#),
        false => format!(r#"{{"imdb":"{imdb}"}}"#),
    };
    let r = call(&state, "POST", "/remux/session", Some(&cookie), &body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let created = r.json();
    let playlist = created["playlist"].as_str().unwrap().to_string();
    let base = playlist.trim_end_matches("master.m3u8").to_string();
    let duration = created["duration"].as_f64().unwrap();
    assert_eq!(created["release"]["filename"], fixture_name, "the uncached decoy must be skipped");

    let master = call(&state, "GET", &playlist, None, "").await;
    assert_eq!(master.status, StatusCode::OK);
    assert_eq!(master.headers["content-type"], "application/vnd.apple.mpegurl");
    assert_eq!(master.headers["access-control-allow-origin"], "*");
    assert!(master.text().contains(codec_prefix) && master.text().contains("mp4a.40.2"), "{}", master.text());

    let media = call(&state, "GET", &format!("{base}media.m3u8"), None, "").await.text();
    let durs = extinfs(&media);
    assert!((durs.iter().sum::<f64>() - duration).abs() < 1e-4, "EXTINF sum vs {duration}");
    let n = durs.len();
    assert!(n >= 4, "the fixture should cut into several segments: {media}");
    let starts: Vec<f64> = durs
        .iter()
        .scan(0.0, |t, d| {
            let s = *t;
            *t += d;
            Some(s)
        })
        .collect();

    // Out of order on purpose: 3 starts a job mid-file (-ss), 1 is behind that job (restart), 0 is the
    // start of the file (restart without -ss), and the rest ride the last job.
    let mut order = vec![3, 1, 0];
    order.extend((0..n).filter(|i| ![3, 1, 0].contains(i)));
    let out = temp_dir();
    let init = call(&state, "GET", &format!("{base}init.mp4"), None, "").await;
    assert_eq!(init.status, StatusCode::OK);
    let mut segs = vec![Bytes::new(); n];
    for i in order {
        let r = call(&state, "GET", &format!("{base}seg{i}.m4s"), None, "").await;
        assert_eq!(r.status, StatusCode::OK, "seg{i}: {}", r.text());
        assert_eq!(r.headers["content-type"], "video/mp4");
        segs[i] = r.body;
    }
    assert!(state.jobs_started.load(Relaxed) >= 3, "the out-of-order fetch should have restarted the job");

    let frame = 1.0 / 24.0 + 0.002;
    for (i, seg) in segs.iter().enumerate() {
        let f = out.join(format!("seg{i}.mp4"));
        std::fs::write(&f, [init.body.as_ref(), seg.as_ref()].concat()).unwrap();
        let pkts = video_packets(&f);
        let (first_pts, first_key) = pkts[0];
        assert!(first_key, "seg{i} must start with a keyframe");
        assert!(
            (first_pts - starts[i]).abs() <= frame,
            "seg{i} starts at {first_pts}, the playlist says {}",
            starts[i]
        );
        let last = pkts.iter().map(|p| p.0).fold(f64::MIN, f64::max);
        assert!(last < starts[i] + durs[i], "seg{i} runs to {last}, past its end {}", starts[i] + durs[i]);
        let audio = ffprobe(
            &[
                "-select_streams",
                "a:0",
                "-count_packets",
                "-show_entries",
                "stream=nb_read_packets",
                "-of",
                "csv=p=0",
            ],
            &f,
        );
        assert!(audio.trim().parse::<u32>().unwrap() > 0, "seg{i} has no audio");
    }

    // All of it, back to back: every source frame exactly once, and the whole duration.
    let whole = out.join("whole.mp4");
    let mut all = init.body.to_vec();
    for s in &segs {
        all.extend_from_slice(s);
    }
    std::fs::write(&whole, all).unwrap();
    let count = |f: &Path| {
        ffprobe(
            &[
                "-select_streams",
                "v:0",
                "-count_packets",
                "-show_entries",
                "stream=nb_read_packets",
                "-of",
                "csv=p=0",
            ],
            f,
        )
    };
    assert_eq!(count(&whole).trim(), count(&testdata(fixture_name)).trim(), "every video frame, once");
    let pkts = video_packets(&whole);
    for w in pkts.windows(2).filter(|w| w[1].1) {
        assert!(w[1].0 > w[0].0 - 0.2, "timestamps jump backwards at a segment join: {w:?}");
    }
    let dur: f64 =
        ffprobe(&["-show_entries", "format=duration", "-of", "csv=p=0"], &whole).trim().parse().unwrap();
    assert!((dur - duration).abs() < 0.2, "joined duration {dur} vs {duration}");

    // A player that can't play it says why, into the log; a report that isn't one is refused.
    let report =
        call(&state, "POST", &format!("{base}report"), None, r#"{"code":3,"message":"DECODE"}"#).await;
    assert_eq!(report.status, StatusCode::NO_CONTENT);
    assert_eq!(
        call(&state, "POST", &format!("{base}report"), None, "{}").await.status,
        StatusCode::BAD_REQUEST
    );

    // Ending it: 204, then 410 for anything under its URL, and its scratch is gone.
    let sid = created["sid"].as_str().unwrap();
    let dir = state.cfg.scratch_dir.join(format!("s-{sid}"));
    assert!(dir.exists());
    let del = call(&state, "DELETE", base.trim_end_matches('/'), None, "").await;
    assert_eq!(del.status, StatusCode::NO_CONTENT);
    assert_eq!(call(&state, "GET", &format!("{base}seg0.m4s"), None, "").await.status, StatusCode::GONE);
    assert_eq!(call(&state, "GET", &playlist, None, "").await.status, StatusCode::GONE);
    assert!(!dir.exists(), "scratch left behind");
    assert_eq!(state.scratch_bytes.load(Relaxed), 0);
}

#[tokio::test]
async fn the_releases_list_names_and_labels_never_a_url() {
    let origin = origin().await;
    let state = test_state(&origin, 2, Duration::from_secs(60));
    let body = format!(r#"{{"imdb":"tt0000001","scout":"{origin}/cfg"}}"#);
    let r = call(&state, "POST", "/remux/releases", None, &body).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let releases = r.json()["releases"].as_array().unwrap().clone();
    assert_eq!(releases.len(), 1, "the uncached decoy is not one: {}", r.text());
    assert_eq!(releases[0]["filename"], "h264.mkv");
    assert!(!r.text().contains("http"), "no URL reaches the browser: {}", r.text());
    let anonymous = call(&state, "POST", "/remux/releases", None, r#"{"imdb":"tt0000001"}"#).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "no cookie and no install");
}

#[tokio::test]
#[ignore]
async fn h264_matroska_end_to_end() {
    end_to_end("tt0000001", "h264.mkv", "avc1.64", true).await;
}

#[tokio::test]
#[ignore]
async fn hevc_matroska_end_to_end() {
    end_to_end("tt0000002", "hevc.mkv", "hvc1.1.6.L", false).await;
}

#[tokio::test]
#[ignore]
async fn mp4_end_to_end() {
    end_to_end("tt0000003", "h264.mp4", "avc1.64", true).await;
}

#[tokio::test]
#[ignore]
async fn sessions_are_capped_but_a_browser_may_switch_titles() {
    let origin = origin().await;
    let state = test_state(&origin, 1, Duration::from_secs(600));
    let phone = login(&state, "phone-key").await;
    let laptop = login(&state, "laptop-key").await;
    let body = r#"{"imdb":"tt0000001"}"#;
    assert_eq!(call(&state, "POST", "/remux/session", Some(&phone), body).await.status, StatusCode::CREATED);
    let full = call(&state, "POST", "/remux/session", Some(&laptop), body).await;
    assert_eq!(full.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(full.json()["error"], "too_many_sessions");
    // The phone starting something else replaces its own session instead of hitting the cap.
    let again = call(&state, "POST", "/remux/session", Some(&phone), r#"{"imdb":"tt0000003"}"#).await;
    assert_eq!(again.status, StatusCode::CREATED, "{}", again.text());
    assert_eq!(state.sessions().len(), 1);
    state.end_all("test").await;
}

#[tokio::test]
#[ignore]
async fn an_idle_session_is_ended_and_its_ffmpeg_does_not_outlive_it() {
    let origin = origin().await;
    // One second of idleness, far below the env floor: only a test can ask for it.
    let state = test_state(&origin, 2, Duration::from_secs(1));
    let cookie = login(&state, "phone-key").await;
    let created =
        call(&state, "POST", "/remux/session", Some(&cookie), r#"{"imdb":"tt0000009"}"#).await.json();
    let sid = created["sid"].as_str().unwrap().to_string();
    let base = created["playlist"].as_str().unwrap().trim_end_matches("master.m3u8").to_string();
    // Segment 0 arrives while the trickling source keeps ffmpeg busy with the rest.
    assert_eq!(call(&state, "GET", &format!("{base}seg0.m4s"), None, "").await.status, StatusCode::OK);
    let pid = state
        .session(&sid)
        .and_then(|s| s.job_pid())
        .expect("ffmpeg should still be reading the slow source");
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0, "the premise: ffmpeg is alive");

    let mut gone = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if state.session(&sid).is_none() {
            gone = true;
            break;
        }
    }
    assert!(gone, "the session outlived its idle window");
    // Killed AND reaped: not even a zombie holds the pid.
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1, "ffmpeg {pid} outlived its session");
    assert_eq!(call(&state, "GET", &format!("{base}seg1.m4s"), None, "").await.status, StatusCode::GONE);
    // The session leaves the map first; its directory goes once ffmpeg is reaped, off the runtime thread.
    let dir = state.cfg.scratch_dir.join(format!("s-{sid}"));
    for _ in 0..50 {
        if !dir.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!dir.exists(), "the session's scratch outlived it");
}

/// den-scout's contract for a scope=availability config: it lists and plays only for the service key.
/// den-remux must present the key to scout — and only to scout: the fake file host fails any request
/// that carries it, so a session that probes at all proves the key stayed behind at the redirect. No
/// ffmpeg needed: creating a session resolves and probes, and starts nothing.
#[tokio::test]
async fn a_scoped_scout_gets_the_service_key_and_nothing_else_does() {
    let origin = origin().await;
    let scoped = |scout: &str| format!(r#"{{"imdb":"tt0000001","scout":"{scout}"}}"#);
    let good = format!("{origin}/{SCOPED}");

    let state = test_state(&origin, 2, Duration::from_secs(600));
    let cookie = login(&state, "phone-key").await;
    let r = call(&state, "POST", "/remux/session", Some(&cookie), &scoped(&good)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(r.json()["release"]["filename"], "h264.mkv");
    // Anything but an allowed origin and one config segment is refused before a request is made.
    let refused =
        ["http://169.254.169.254/latest".to_string(), format!("{good}/stream"), format!("{good}?x=1")];
    for bad in &refused {
        let r = call(&state, "POST", "/remux/session", Some(&cookie), &scoped(bad)).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(r.json()["error"], "bad_scout");
    }
    state.end_all("test").await;

    // Without the key the scoped config will not list: scout refuses it, and den-remux says so.
    let keyless = state_with(&origin, 2, Duration::from_secs(600), None);
    let cookie = login(&keyless, "phone-key").await;
    let r = call(&keyless, "POST", "/remux/session", Some(&cookie), &scoped(&good)).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.text());
    assert_eq!(r.json()["error"], "scout_refused");
}

/// With no cookie the install URL is the credential: a full install plays; an availability-only one needs
/// a logged-in browser, since den-remux's key is sent for nobody else; and one scout won't open (revoked,
/// or not its own) is refused as such. No ffmpeg needed.
#[tokio::test]
async fn a_full_install_url_is_its_own_credential() {
    let origin = origin().await;
    let state = test_state(&origin, 4, Duration::from_secs(600));
    let with = |config: &str| format!(r#"{{"imdb":"tt0000001","scout":"{origin}/{config}"}}"#);
    let r = call(&state, "POST", "/remux/session", None, &with("cfg")).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(r.json()["release"]["filename"], "h264.mkv");
    let r = call(&state, "POST", "/remux/session", None, &with(SCOPED)).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{}", r.text());
    assert_eq!(r.json()["error"], "not_logged_in");
    let r = call(&state, "POST", "/remux/session", None, &with("cmV2b2tlZA")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.text());
    assert_eq!(r.json()["error"], "scout_refused");
    state.end_all("test").await;
}

/// An install plays `MAX_SESSIONS_PER_INSTALL` (2 here) at once and its oldest gives way to a new one, so
/// one household — or one leaked URL — cannot hold every slot; `MAX_SESSIONS` still caps the whole box.
#[tokio::test]
async fn an_install_plays_its_share_and_its_oldest_gives_way() {
    let origin = origin().await;
    let state = test_state(&origin, 3, Duration::from_secs(600));
    let full = format!(r#"{{"imdb":"tt0000001","scout":"{origin}/cfg"}}"#);
    let mut sids = Vec::new();
    for _ in 0..3 {
        let r = call(&state, "POST", "/remux/session", None, &full).await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
        sids.push(r.json()["sid"].as_str().unwrap().to_string());
    }
    assert!(state.session(&sids[0]).is_none(), "the install's oldest gave way");
    assert!(state.session(&sids[1]).is_some() && state.session(&sids[2]).is_some());
    // The third slot goes to a browser; then the box is full for anyone else.
    let phone = login(&state, "phone-key").await;
    let r = call(&state, "POST", "/remux/session", Some(&phone), r#"{"imdb":"tt0000001"}"#).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let laptop = login(&state, "laptop-key").await;
    let r = call(&state, "POST", "/remux/session", Some(&laptop), r#"{"imdb":"tt0000001"}"#).await;
    assert_eq!(r.json()["error"], "too_many_sessions");
    state.end_all("test").await;
}

#[tokio::test]
async fn concurrent_starts_count_against_the_install_and_browser_shares() {
    let origin = origin().await;
    let state = test_state(&origin, 4, Duration::from_secs(600));
    let body = format!(r#"{{"imdb":"tt0000001","scout":"{origin}/cfg"}}"#);
    let (a, b, c) = tokio::join!(
        call(&state, "POST", "/remux/session", None, &body),
        call(&state, "POST", "/remux/session", None, &body),
        call(&state, "POST", "/remux/session", None, &body),
    );
    let statuses = [a.status, b.status, c.status];
    assert_eq!(statuses.iter().filter(|s| **s == StatusCode::CREATED).count(), 2);
    assert_eq!(statuses.iter().filter(|s| **s == StatusCode::TOO_MANY_REQUESTS).count(), 1);
    assert_eq!(state.sessions().len(), 2);
    state.end_all("test").await;

    let cookie = login(&state, "phone-key").await;
    let (a, b) = tokio::join!(
        call(&state, "POST", "/remux/session", Some(&cookie), &body),
        call(&state, "POST", "/remux/session", Some(&cookie), &body),
    );
    assert_eq!([a.status, b.status].iter().filter(|s| **s == StatusCode::CREATED).count(), 1);
    assert_eq!(state.sessions().len(), 1);
    state.end_all("test").await;
    // A failed setup drops its reservation as well, so a correction can start immediately.
    assert_eq!(
        call(&state, "POST", "/remux/session", Some(&cookie), r#"{"imdb":"tt9999999"}"#).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&state, "POST", "/remux/session", Some(&cookie), &body).await.status,
        StatusCode::CREATED
    );
    state.end_all("test").await;
}

/// A release scout's attributes rule out (here Dolby Vision profile 5, which has no picture without it) is never
/// opened, and a title is tried past the first three releases that won't open. No ffmpeg needed.
#[tokio::test]
async fn ruled_out_releases_stay_unopened_and_the_search_goes_past_three() {
    let origin = origin().await;
    let state = test_state(&origin, 2, Duration::from_secs(600));
    let with = |imdb: &str| format!(r#"{{"imdb":"{imdb}","scout":"{origin}/cfg"}}"#);
    let r = call(&state, "POST", "/remux/session", None, &with("tt0000006")).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(r.json()["release"]["filename"], "h264.mkv");
    assert_eq!(NEVER_OPENED.load(Relaxed), 0, "the profile 5 release was opened");
    state.end_all("test").await;
    let r = call(&state, "POST", "/remux/session", None, &with("tt0000008")).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(r.json()["release"]["filename"], "h264.mkv", "the fifth release");
    state.end_all("test").await;
}

/// Another audio track of the release playing — a second session naming it — opens from what den-remux
/// remembers of the first: scout's play URL is followed once, and the file isn't probed again.
#[tokio::test]
async fn reopening_a_release_takes_its_link_and_probe_from_the_first_open() {
    let origin = origin().await;
    let state = test_state(&origin, 4, Duration::from_secs(600));
    let body = |extra: &str| format!(r#"{{"imdb":"tt0000010","scout":"{origin}/cfg"{extra}}}"#);
    for (extra, track) in [("", 0), (r#","filename":"h264.mkv","audioTrack":1"#, 1)] {
        let r = call(&state, "POST", "/remux/session", None, &body(extra)).await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
        assert_eq!(r.json()["audioTrack"], track);
    }
    assert_eq!(COUNTED_PLAYS.load(Relaxed), 1, "the second session followed scout's play URL again");
    state.end_all("test").await;
}

#[tokio::test]
async fn cancelling_a_reservation_gives_back_both_limits() {
    let state = test_state("http://127.0.0.1:9", 2, Duration::from_secs(600));
    let first = state.reserve("phone", 1).await.unwrap();
    assert!(state.reserve("phone", 1).await.is_none());
    let second = state.reserve("laptop", 1).await.unwrap();
    assert!(state.reserve("other", 1).await.is_none());
    drop(first);
    let replacement = state.reserve("phone", 1).await.unwrap();
    drop((second, replacement));
    assert!(state.reserve("other", 1).await.is_some());
}

#[tokio::test]
async fn an_explicit_release_survives_the_players_codec_preference() {
    let origin = origin().await;
    let state = test_state(&origin, 2, Duration::from_secs(600));
    state.transcode_ok.store(true, Relaxed); // creation reserves the GPU; it does not start ffmpeg
    let body = format!(
        r#"{{"imdb":"tt0000005","scout":"{origin}/cfg","filename":"hevc.mkv","audioTrack":0,"videoCodecs":["h264"]}}"#
    );
    let response = call(&state, "POST", "/remux/session", None, &body).await;
    assert_eq!(response.status, StatusCode::CREATED, "{}", response.text());
    assert_eq!(response.json()["release"]["filename"], "hevc.mkv");
    assert_eq!(response.json()["video"]["transcoded"], true);
    state.end_all("test").await;
    let body = format!(r#"{{"imdb":"tt0000005","scout":"{origin}/cfg","videoCodecs":["h264"]}}"#);
    let response = call(&state, "POST", "/remux/session", None, &body).await;
    assert_eq!(response.status, StatusCode::CREATED, "{}", response.text());
    assert_eq!(response.json()["release"]["filename"], "h264.mkv");
    assert_eq!(response.json()["video"]["transcoded"], false);
    state.end_all("test").await;
}

#[tokio::test]
async fn a_cross_origin_login_admits_sessions_without_cookies() {
    let origin = origin().await;
    let state = test_state(&origin, 2, Duration::from_secs(600));
    let login_request = Request::builder()
        .method("POST")
        .uri("/remux/login")
        .header("origin", "https://d.example")
        .body(Full::new(Bytes::from_static(br#"{"key":"phone-key"}"#)))
        .unwrap();
    let response = crate::handle_request(state.clone(), login_request).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.headers()["access-control-expose-headers"], crate::auth::BROWSER_TOKEN_HEADER);
    assert!(response.headers()["set-cookie"].to_str().unwrap().contains("HttpOnly"));
    let token = response.headers()[crate::auth::BROWSER_TOKEN_HEADER].to_str().unwrap();
    let body = format!(r#"{{"imdb":"tt0000001","scout":"{origin}/{SCOPED}"}}"#);
    for path in ["/remux/releases", "/remux/session"] {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("origin", "https://d.example")
            .header("authorization", format!("Bearer {token}"))
            .body(Full::new(Bytes::from(body.clone())))
            .unwrap();
        let response = crate::handle_request(state.clone(), request).await;
        assert!(response.status().is_success(), "{path}: {}", response.status());
        assert_eq!(response.headers()["access-control-allow-origin"], "https://d.example");
    }
    let request = Request::builder()
        .method("POST")
        .uri("/remux/session")
        .header("authorization", "Bearer invalid")
        .body(Full::new(Bytes::from(body)))
        .unwrap();
    assert_eq!(crate::handle_request(state.clone(), request).await.status(), StatusCode::UNAUTHORIZED);
    state.end_all("test").await;
}

/// The web app on its public name plays from this service's tailnet address, so it logs in and starts sessions
/// cross-origin: those two routes answer its CORS, and no other origin's.
#[tokio::test]
async fn the_web_app_on_another_origin_may_start_sessions() {
    let state = test_state("http://127.0.0.1:9", 2, Duration::from_secs(600));
    let send = |method: &str, origin: &str| {
        let req = Request::builder()
            .method(method)
            .uri("/remux/session")
            .header("origin", origin)
            .header("access-control-request-method", "POST")
            .body(Full::new(Bytes::new()))
            .unwrap();
        crate::handle_request(state.clone(), req)
    };
    let preflight = send("OPTIONS", "https://d.example").await;
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    assert_eq!(preflight.headers()["access-control-allow-origin"], "https://d.example");
    assert_eq!(preflight.headers()["access-control-allow-headers"], "content-type, authorization");
    let elsewhere = send("OPTIONS", "https://elsewhere.example").await;
    assert!(elsewhere.headers().get("access-control-allow-origin").is_none());
    // The answer itself is readable too — here a refusal of the empty body.
    let post = send("POST", "https://d.example").await;
    assert_eq!(post.status(), StatusCode::BAD_REQUEST);
    assert_eq!(post.headers()["access-control-allow-origin"], "https://d.example");
    // Its health under the mount path, which the web app probes to find this service (den-spec routes-v1).
    let health = crate::handle_request(
        state.clone(),
        Request::builder()
            .uri("/remux/health")
            .header("origin", "https://d.example")
            .body(Full::new(Bytes::new()))
            .unwrap(),
    )
    .await;
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(health.headers()["access-control-allow-origin"], "https://d.example");
}

#[tokio::test]
async fn a_visitor_gets_a_few_starts_a_minute() {
    let state = test_state("http://127.0.0.1:9", 2, Duration::from_secs(600));
    let ip = Some("100.64.0.7".parse().unwrap());
    for _ in 0..crate::state::STARTS_PER_MINUTE {
        assert!(state.admit(ip));
    }
    assert!(!state.admit(ip));
    assert!(state.admit(Some("100.64.0.8".parse().unwrap())), "another visitor has their own");
    assert!(state.admit(None), "no address, no limit: only a test's request comes without one");
}

/// Through a trusted proxy the visitor is the last address it forwarded; anyone else is their own address.
#[tokio::test]
async fn the_visitor_is_the_address_a_trusted_proxy_saw() {
    let state = test_state("http://127.0.0.1:9", 2, Duration::from_secs(600));
    let parts = |peer: &str, forwarded: Option<&str>| {
        let mut b = Request::builder().extension(crate::Peer(peer.parse().unwrap()));
        if let Some(f) = forwarded {
            b = b.header("x-forwarded-for", f);
        }
        b.body(()).unwrap().into_parts().0
    };
    let ip = |s: &str| Some(s.parse().unwrap());
    let visitor = |p| crate::visitor(&state, &p);
    assert_eq!(visitor(parts("192.168.86.149", Some("100.64.0.7"))), ip("100.64.0.7"));
    assert_eq!(
        visitor(parts("192.168.86.149", Some("6.6.6.6, 100.64.0.7"))),
        ip("100.64.0.7"),
        "the last hop"
    );
    assert_eq!(visitor(parts("192.168.86.149", None)), ip("192.168.86.149"));
    assert_eq!(visitor(parts("192.168.86.50", Some("100.64.0.7"))), ip("192.168.86.50"), "not a proxy");
    assert_eq!(crate::visitor(&state, &Request::builder().body(()).unwrap().into_parts().0), None);
}

/// The fixture carries English then Swedish: a preference picks the track, `audioTrack` overrides it,
/// and an index past the release's tracks is refused. Creating a session starts no ffmpeg.
#[tokio::test]
async fn the_audio_track_follows_the_browsers_languages() {
    let origin = origin().await;
    let state = test_state(&origin, 2, Duration::from_secs(600));
    let cookie = login(&state, "phone-key").await;
    let create = |extra: &str| format!(r#"{{"imdb":"tt0000001"{extra}}}"#);
    for (extra, want) in [
        ("", 0),
        (r#","audio":["sv-SE","en"]"#, 1),
        (r#","audio":["fi","en"]"#, 0),
        (r#","audioTrack":1"#, 1),
    ] {
        let r = call(&state, "POST", "/remux/session", Some(&cookie), &create(extra)).await;
        assert_eq!(r.status, StatusCode::CREATED, "{extra}: {}", r.text());
        let j = r.json();
        assert_eq!(j["audioTrack"], want, "{extra}");
        assert_eq!(j["audioTracks"][1]["language"], "swe");
    }
    let r = call(&state, "POST", "/remux/session", Some(&cookie), &create(r#","audioTrack":2"#)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["error"], "bad_audio_track");
    state.end_all("test").await;
}

/// A resume names its point: the media playlist starts a native player there, and the session's first job heads
/// for the segment it plays (h264.mkv's 13 → 19.5 for 14.5, less the slack of a second). Past the end, or
/// without `startAt`, it starts from zero; a negative point is refused. Creating a session starts no ffmpeg.
#[tokio::test]
async fn a_resume_starts_where_the_player_does() {
    let origin = origin().await;
    let state = test_state(&origin, 2, Duration::from_secs(600));
    let cookie = login(&state, "phone-key").await;
    let cases = [(Some(14.5), Some("14.500"), 2), (Some(90.0), None, 0), (None, None, 0)];
    for (start, offset, segment) in cases {
        let extra = start.map(|s| format!(r#","startAt":{s}"#)).unwrap_or_default();
        let body = format!(r#"{{"imdb":"tt0000001"{extra}}}"#);
        let r = call(&state, "POST", "/remux/session", Some(&cookie), &body).await;
        assert_eq!(r.status, StatusCode::CREATED, "{extra}: {}", r.text());
        let j = r.json();
        let base = j["playlist"].as_str().unwrap().trim_end_matches("master.m3u8").to_string();
        let media = call(&state, "GET", &format!("{base}media.m3u8"), None, "").await.text();
        let named = offset.map(|o| format!("#EXT-X-START:TIME-OFFSET={o},PRECISE=YES\n"));
        match named {
            Some(tag) => assert!(media.contains(&tag), "{media}"),
            None => assert!(!media.contains("EXT-X-START"), "{extra}: {media}"),
        }
        assert_eq!(state.session(j["sid"].as_str().unwrap()).unwrap().wanted(), segment, "{extra}");
    }
    let negative = r#"{"imdb":"tt0000001","startAt":-3}"#;
    let r = call(&state, "POST", "/remux/session", Some(&cookie), negative).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    state.end_all("test").await;
}

/// Subtitle renditions: named in the master, one WebVTT segment each, fetched from den-subtitles with the
/// release's hash; a language whose subtitle is on another origin serves an empty document, and an install
/// off `SUBTITLE_ORIGINS` is refused up front.
#[tokio::test]
async fn subtitles_are_webvtt_renditions_from_den_subtitles() {
    let origin = origin().await;
    let state = test_state(&origin, 2, Duration::from_secs(600));
    let cookie = login(&state, "phone-key").await;
    let body = format!(
        r#"{{"imdb":"tt0000001","subtitles":"{origin}/subs","subtitleLanguages":["en-GB","fi","en"]}}"#
    );
    let r = call(&state, "POST", "/remux/session", Some(&cookie), &body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let j = r.json();
    assert_eq!(
        j["subtitles"],
        serde_json::json!([{"language": "en", "name": "English"}, {"language": "fi", "name": "Finnish"}])
    );
    let base = j["playlist"].as_str().unwrap().trim_end_matches("master.m3u8").to_string();

    let master = call(&state, "GET", &format!("{base}master.m3u8"), None, "").await.text();
    assert!(master.contains("LANGUAGE=\"fi\"") && master.contains("SUBTITLES=\"subs\""), "{master}");
    assert!(master.contains("LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES"), "the first preference: {master}");
    let pl = call(&state, "GET", &format!("{base}sub0.m3u8"), None, "").await;
    assert_eq!(pl.status, StatusCode::OK);
    assert!(pl.text().contains("sub0.vtt"), "{}", pl.text());
    let en = call(&state, "GET", &format!("{base}sub0.vtt"), None, "").await;
    assert_eq!(en.headers["content-type"], "text/vtt; charset=utf-8");
    assert_eq!(en.headers["access-control-allow-origin"], "*");
    assert!(en.text().contains("X-TIMESTAMP-MAP=MPEGTS:0") && en.text().contains("Hello"), "{}", en.text());
    let fi = call(&state, "GET", &format!("{base}sub1.vtt"), None, "").await.text();
    assert_eq!(fi, crate::subs::EMPTY, "the Finnish subtitle is on a refused origin");
    assert_eq!(call(&state, "GET", &format!("{base}sub2.vtt"), None, "").await.status, StatusCode::NOT_FOUND);
    state.end_all("test").await;

    let off = r#"{"imdb":"tt0000001","subtitles":"http://169.254.169.254/subs","subtitleLanguages":["en"]}"#;
    let r = call(&state, "POST", "/remux/session", Some(&cookie), off).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["error"], "bad_subtitles");
}

/// A player without HEVC gets an HEVC-only title through the GPU alone: refused while transcoding is
/// off, then one transcode at a time, given back when its session ends. Creating a session starts no
/// ffmpeg, so no GPU is needed here.
#[tokio::test]
async fn hevc_for_a_player_without_it_takes_the_one_transcode() {
    let origin = origin().await;
    let state = test_state(&origin, 2, Duration::from_secs(600));
    let h264_only = r#"{"imdb":"tt0000002","videoCodecs":["h264"]}"#;
    let phone = login(&state, "phone-key").await;
    let r = call(&state, "POST", "/remux/session", Some(&phone), h264_only).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{}", r.text());
    assert_eq!(r.json()["error"], "transcode_unavailable");

    state.transcode_ok.store(true, Relaxed);
    let r = call(&state, "POST", "/remux/session", Some(&phone), h264_only).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let j = r.json();
    // A fixture too small to come down any further, and SDR, so it plays at its own size untouched in colour.
    let played = serde_json::json!({
        "codec": "h264", "transcoded": true, "width": 320, "height": 180, "tonemapped": false
    });
    assert_eq!(j["video"], played);
    let master = call(&state, "GET", j["playlist"].as_str().unwrap(), None, "").await.text();
    assert!(master.contains("CODECS=\"avc1.640029,mp4a.40.2\""), "{master}");

    let laptop = login(&state, "laptop-key").await;
    let r = call(&state, "POST", "/remux/session", Some(&laptop), h264_only).await;
    assert_eq!(r.json()["error"], "transcode_unavailable", "MAX_TRANSCODES is 1");
    let r = call(&state, "POST", "/remux/session", Some(&laptop), r#"{"imdb":"tt0000002"}"#).await;
    let copied = serde_json::json!({
        "codec": "hevc", "transcoded": false, "width": 320, "height": 180, "tonemapped": false
    });
    assert_eq!(r.json()["video"], copied, "copied");

    state.end_session(j["sid"].as_str().unwrap(), "test").await;
    let r = call(&state, "POST", "/remux/session", Some(&laptop), h264_only).await;
    assert_eq!(r.status, StatusCode::CREATED, "the ended session gave its transcode back: {}", r.text());
    state.end_all("test").await;
}

/// An install URL on a public name (`ORIGIN_ALIASES`) is fetched at scout's LAN address: the session is
/// made, though the public name here resolves nowhere at all.
#[tokio::test]
async fn a_public_install_url_is_fetched_at_its_lan_address() {
    let origin = origin().await;
    let mut state = test_state(&origin, 2, Duration::from_secs(600));
    let cfg = &mut Arc::get_mut(&mut state).expect("only this test holds the state").cfg;
    cfg.scout_origins.push("https://d-scout.invalid".into());
    cfg.origin_aliases = vec![("https://d-scout.invalid".into(), origin.clone())];
    let cookie = login(&state, "phone-key").await;
    let body = r#"{"imdb":"tt0000001","scout":"https://d-scout.invalid/cfg"}"#;
    let r = call(&state, "POST", "/remux/session", Some(&cookie), body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    state.end_all("test").await;
}

/// An episode lists from scout's series route, through the full install URL a library holds (no key
/// needed); half an episode is refused before scout is asked.
#[tokio::test]
async fn an_episode_plays_through_the_librarys_install_url() {
    let origin = origin().await;
    let state = state_with(&origin, 2, Duration::from_secs(600), None);
    let cookie = login(&state, "phone-key").await;
    let body = format!(r#"{{"imdb":"tt0000004","season":1,"episode":2,"scout":"{origin}/cfg"}}"#);
    let r = call(&state, "POST", "/remux/session", Some(&cookie), &body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(r.json()["release"]["filename"], "h264.mkv");
    state.end_all("test").await;

    let half = format!(r#"{{"imdb":"tt0000004","season":1,"scout":"{origin}/cfg"}}"#);
    let r = call(&state, "POST", "/remux/session", Some(&cookie), &half).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text());
}

#[tokio::test]
async fn session_routes_refuse_without_the_right_credentials() {
    let state = test_state("http://127.0.0.1:9", 2, Duration::from_secs(600));
    // No cookie, a forged one, a bad key.
    assert_eq!(
        call(&state, "POST", "/remux/session", None, r#"{"imdb":"tt1"}"#).await.status,
        StatusCode::UNAUTHORIZED
    );
    let forged = "den_remux=0011223344556677.9999999999.AAAAAAAAAAAAAAAAAAAAAA";
    assert_eq!(
        call(&state, "POST", "/remux/session", Some(forged), r#"{"imdb":"tt1"}"#).await.status,
        StatusCode::UNAUTHORIZED
    );
    let bad = call(&state, "POST", "/remux/login", None, r#"{"key":"guess"}"#).await;
    assert_eq!(bad.status, StatusCode::UNAUTHORIZED);
    assert!(bad.headers.get("set-cookie").is_none());
    // A logged-in browser still has to name a movie.
    let cookie = login(&state, "laptop-key").await;
    assert_eq!(
        call(&state, "POST", "/remux/session", Some(&cookie), r#"{"imdb":"tt1:1:2"}"#).await.status,
        StatusCode::BAD_REQUEST
    );
    // An unknown session is a 404, with CORS so a receiver can read it; login is not cross-origin.
    let r =
        call(&state, "GET", "/remux/s/AAAAAAAAAAAAAAAAAAAAAA/AAAAAAAAAAAAAAAAAAAAAA/master.m3u8", None, "")
            .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.headers["access-control-allow-origin"], "*");
    assert!(bad.headers.get("access-control-allow-origin").is_none());
    assert_eq!(call(&state, "GET", "/remux/login", None, "").await.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(call(&state, "GET", "/nope", None, "").await.json()["error"], "not_found");
    // /metrics without a token configured is the same 404.
    assert_eq!(call(&state, "GET", "/metrics", None, "").await.status, StatusCode::NOT_FOUND);
    assert_eq!(call(&state, "GET", "/health", None, "").await.json()["status"], "ok");
}
