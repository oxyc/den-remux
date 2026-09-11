//! Tests that need the fixtures in testdata/ or a whole AppState.
//!
//! The probe tests run everywhere. The `#[ignore]`d ones run real ffmpeg against a local origin that
//! plays scout (a stream list, a 302 play URL, Range-served files) — run them with ffmpeg and ffprobe
//! on PATH: `cargo test -- --ignored`.

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
async fn hevc_matroska_gives_an_hvc1_codec_string() {
    let info = probe_with_head(&fixture("hevc.mkv"), crate::scout::HEAD_BYTES as usize).await;
    assert_keyframes(&info.keyframes, &HEVC_MKV_KF);
    assert_eq!(info.video, VideoCodec::Hevc);
    assert!(info.codecs.as_deref().is_some_and(|c| c.starts_with("hvc1.1.6.L")), "{:?}", info.codecs);
    assert_eq!(info.audio[0].codec, "A_EAC3");
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
        browser_key_hashes: vec![crate::auth::sha256(b"phone-key"), crate::auth::sha256(b"laptop-key")],
        url_key: b"integration-test-key".to_vec(),
        url_key_ephemeral: false,
        max_sessions,
        session_idle: idle,
        scratch_dir: dir.clone(),
        scratch_max_bytes: 1 << 30,
        ffmpeg: tool("FFMPEG_PATH", "ffmpeg"),
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
    assert!(!state.cfg.scratch_dir.join(format!("s-{sid}")).exists());
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

    // Without the key the scoped config will not list, and that is scout being unavailable.
    let keyless = state_with(&origin, 2, Duration::from_secs(600), None);
    let cookie = login(&keyless, "phone-key").await;
    let r = call(&keyless, "POST", "/remux/session", Some(&cookie), &scoped(&good)).await;
    assert_eq!(r.status, StatusCode::BAD_GATEWAY, "{}", r.text());
    assert_eq!(r.json()["error"], "scout_unavailable");
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
