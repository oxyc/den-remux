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
const AV1_KF: [f64; 10] = [0.0, 2.5, 6.0, 8.5, 12.0, 14.0, 18.5, 21.0, 24.0, 27.5];

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
    assert_eq!(info.frame_rate.map(|f| (f * 1000.0).round()), Some(24_000.0), "Matroska's DefaultDuration");
    assert!(info.dolby_vision.is_none());
    assert!(crate::session::tonemaps(&info), "a conversion of it has to tone-map, or it plays nowhere");
}

#[tokio::test]
async fn surround_matroska_reads_each_tracks_channels() {
    let info = probe_with_head(&fixture("surround.mkv"), crate::scout::HEAD_BYTES as usize).await;
    assert_keyframes(&info.keyframes, &H264_MKV_KF);
    let tracks: Vec<_> = info.audio.iter().map(|a| (a.codec.as_str(), a.channels)).collect();
    assert_eq!(tracks, [("A_AAC", 6), ("A_AAC", 8)]);
}

/// `subs.mkv` carries SRT English, ASS Finnish and a forced SRT Swedish; `h264.mkv` has none.
#[tokio::test]
async fn matroska_subtitle_tracks_are_listed_with_their_kind_and_flags() {
    let info = probe_with_head(&fixture("subs.mkv"), crate::scout::HEAD_BYTES as usize).await;
    let tracks: Vec<_> =
        info.subtitles.iter().map(|t| (t.codec.as_str(), t.language.as_deref(), t.text, t.forced)).collect();
    assert_eq!(
        tracks,
        [
            ("S_TEXT/UTF8", Some("eng"), true, false),
            ("S_TEXT/ASS", Some("fin"), true, false),
            ("S_TEXT/UTF8", Some("swe"), true, true),
        ]
    );
    assert!(probe_with_head(&fixture("h264.mkv"), crate::scout::HEAD_BYTES as usize)
        .await
        .subtitles
        .is_empty());
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
async fn av1_matroska_and_mp4_give_the_same_av01_codec_string() {
    let mkv = probe_with_head(&fixture("av1.mkv"), crate::scout::HEAD_BYTES as usize).await;
    assert_keyframes(&mkv.keyframes, &AV1_KF);
    assert_eq!(mkv.video, VideoCodec::Av1);
    let codecs = mkv.codecs.clone().unwrap();
    assert!(codecs.starts_with("av01.0.") && codecs.ends_with("M.10.0.110.09.16.09.0"), "HDR10: {codecs}");
    assert!(mkv.hdr, "the PQ transfer");
    assert_eq!((mkv.width, mkv.height), (320, 180));
    assert_eq!(mkv.audio[0].codec, "A_OPUS");
    let mp4 = probe_with_head(&fixture("av1.mp4"), crate::scout::HEAD_BYTES as usize).await;
    assert_keyframes(&mp4.keyframes, &AV1_KF);
    assert_eq!(mp4.video, VideoCodec::Av1);
    assert_eq!(mp4.codecs, mkv.codecs, "the av01 sample entry's av1C and colr");
    assert!(mp4.hdr);
    assert_eq!(
        mp4.frame_rate.map(|f| format!("{f:.2}")).as_deref(),
        Some("24.00"),
        "the samples over stts's time"
    );
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

#[tokio::test]
async fn a_report_takes_the_old_shape_and_stats_and_refuses_what_is_neither_or_too_big() {
    let post = |body: String| crate::report("abcdef", Full::new(Bytes::from(body)));
    assert_eq!(post(r#"{"code":3,"message":"DECODE"}"#.into()).await.status(), StatusCode::NO_CONTENT);
    let stats = r#"{"stats":{"engine":"hls.js","fragments":{"count":3},"stalls":[{"at":1,"ms":900}]}}"#;
    assert_eq!(post(stats.into()).await.status(), StatusCode::NO_CONTENT);
    let den_web = r#"{"code":0,"message":"playback stats (stall)","stats":{"event":"stall","stallCount":1,
        "stalls":[{"at":3,"kind":"frozen","ms":null}]}}"#;
    assert_eq!(post(den_web.into()).await.status(), StatusCode::NO_CONTENT);
    for garbage in ["{}", "nonsense", r#"{"stats":7}"#] {
        assert_eq!(post(garbage.into()).await.status(), StatusCode::BAD_REQUEST, "{garbage}");
    }
    // Past the 16 KiB other requests get, within the report's own cap.
    let errors = |n: usize| vec![r#"{"details":"bufferStalledError","fatal":false}"#; n].join(",");
    let big = format!(r#"{{"stats":{{"errors":[{}]}}}}"#, errors(500));
    assert!(big.len() > 16 * 1024 && big.len() < crate::report::MAX_BODY);
    assert_eq!(post(big).await.status(), StatusCode::NO_CONTENT);
    let oversized = format!(r#"{{"stats":{{"errors":[{}]}}}}"#, errors(2000));
    assert!(oversized.len() > crate::report::MAX_BODY);
    assert_eq!(post(oversized).await.status(), StatusCode::BAD_REQUEST);
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
/// Play URLs the fake scout was asked to follow under `/p/av1/`.
static AV1_PLAYS: AtomicU32 = AtomicU32::new(0);
/// Play URLs the fake scout was asked to follow under `/p/unplayable/`, `/p/errorpage/`.
static UNPLAYABLE_PLAYS: AtomicU32 = AtomicU32::new(0);
static ERROR_PAGE_PLAYS: AtomicU32 = AtomicU32::new(0);
/// Lists and subtitles the `flaky` den-subtitles install was asked for.
static FLAKY_LISTS: AtomicU32 = AtomicU32::new(0);
static FLAKY_SUBTITLES: AtomicU32 = AtomicU32::new(0);

/// The fake scout's scope=availability config segment ("scoped", base64url) and the service key it
/// asks for.
const SCOPED: &str = "c2NvcGVk";
const SCOUT_KEY: &str = "test-scout-key";
const EDGE_SECRET: &str = "test-edge-secret";

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
    // The same install for the release that carries its own subtitles: it has English and nothing else.
    if path.starts_with("/subs/subtitles/movie/tt0000015/") {
        let body = serde_json::json!({"subtitles": [
            {"id": "1", "url": format!("http://{addr}/subs/subtitle/1.srt?lang=eng"), "lang": "eng"}
        ]});
        return origin_full(200, body.to_string());
    }
    if path == "/subs/subtitle/1.vtt" {
        return origin_full(200, "WEBVTT\n\n00:00:01.000 --> 00:00:02.500\nHello\n");
    }
    // den-subtitles, install `flaky`: its list and its subtitle each fail the first time they are asked for.
    if path.starts_with("/flaky/subtitles/movie/tt0000001/") {
        if FLAKY_LISTS.fetch_add(1, Relaxed) == 0 {
            return origin_full(500, "");
        }
        let url = format!("http://{addr}/flaky/subtitle/1.srt?lang=eng");
        return origin_full(
            200,
            serde_json::json!({"subtitles": [{"id": "1", "url": url, "lang": "eng"}]}).to_string(),
        );
    }
    if path == "/flaky/subtitle/1.vtt" {
        if FLAKY_SUBTITLES.fetch_add(1, Relaxed) == 0 {
            return origin_full(502, "");
        }
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
        if imdb == "tt0000014" {
            // One title in two releases: h264.mkv, ranked first, needs 217 kbit/s; hevc.mkv needs 165.
            let body = serde_json::json!({"streams": [
                {"url": format!("http://{addr}/{p}/h264.mkv"),
                 "attributes": {"cached": true, "codec": "h264", "label": "H.264"},
                 "behaviorHints": {"filename": "h264.mkv"}},
                {"url": format!("http://{addr}/{p}/hevc.mkv"),
                 "attributes": {"cached": true, "codec": "hevc", "label": "HEVC"},
                 "behaviorHints": {"filename": "hevc.mkv"}}
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
        if imdb == "tt0000012" {
            // Scout names the first release AV1: opened only for a player that decodes it.
            let body = serde_json::json!({"streams": [
                {"url": format!("http://{addr}/{p}/av1/av1.mkv"),
                 "attributes": {"cached": true, "codec": "av1", "hdr": true, "bitDepth": 10, "label": "AV1"},
                 "behaviorHints": {"filename": "av1-named.mkv"}},
                {"url": format!("http://{addr}/{p}/h264.mkv"),
                 "attributes": {"cached": true, "codec": "h264", "label": "H.264"},
                 "behaviorHints": {"filename": "h264.mkv"}}
            ]});
            return origin_full(200, body.to_string());
        }
        if imdb == "tt0000016" {
            // Ahead of one that plays: a release named .m2ts, one whose bytes are MPEG-TS, and one whose debrid
            // answers with an error page.
            let release = |path: &str, filename: &str| {
                serde_json::json!({"url": format!("http://{addr}/{p}/{path}"),
                                   "attributes": {"cached": true, "codec": "h264", "sizeBytes": 307_200,
                                                  "label": filename},
                                   "behaviorHints": {"filename": filename}})
            };
            let body = serde_json::json!({"streams": [
                release("unplayable/mpegts", "Film.BluRay.m2ts"),
                release("unplayable/mpegts", "Film.mkv"),
                release("errorpage/errorpage", "Film.WEB.mkv"),
                release("h264.mkv", "h264.mkv"),
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
            "tt0000011" => "av1.mkv",
            "tt0000013" => "surround.mkv",
            "tt0000015" => "subs.mkv",
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
        let rest = match rest.strip_prefix("av1/") {
            Some(r) => {
                AV1_PLAYS.fetch_add(1, Relaxed);
                r
            }
            None => rest,
        };
        if rest.starts_with("unplayable/") {
            UNPLAYABLE_PLAYS.fetch_add(1, Relaxed);
        }
        if rest.starts_with("errorpage/") {
            ERROR_PAGE_PLAYS.fetch_add(1, Relaxed);
        }
        let rest = rest.trim_start_matches("unplayable/").trim_start_matches("errorpage/");
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
    if name == "errorpage" {
        return origin_full(200, "<html><body>Service temporarily unavailable</body></html>");
    }
    // MPEG-TS sync bytes: a file that is neither Matroska nor MP4.
    let data = match name {
        "mpegts" => vec![0x47; 300 * 1024],
        _ => match std::fs::read(testdata(name)) {
            Ok(data) => data,
            Err(_) => return origin_full(404, ""),
        },
    };
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
    state_from(test_config(origin, max_sessions, idle, scout_key))
}

fn state_from(cfg: Config) -> Arc<AppState> {
    let dir = cfg.scratch_dir.clone();
    let state = AppState::new(cfg);
    state.scratch_ok.store(crate::state::check_scratch(&dir), Relaxed);
    state.ffmpeg_ok.store(true, Relaxed);
    state
}

fn test_config(origin: &str, max_sessions: usize, idle: Duration, scout_key: Option<&str>) -> Config {
    let dir = temp_dir();
    Config {
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
        scratch_dir: dir,
        scratch_max_bytes: 1 << 30,
        ffmpeg: tool("FFMPEG_PATH", "ffmpeg"),
        max_transcodes: 1,
        vaapi_device: PathBuf::from("/dev/dri/renderD128"),
        trusted_proxies: vec!["192.168.86.149".parse().unwrap()],
        public_session_proxy: Some("10.89.0.10".parse().unwrap()),
        web_origins: vec!["https://d.example".into()],
        metrics_token: None,
        log_requests: false,
        edge_secret: Some(EDGE_SECRET.into()),
        guest_max_sessions: 2,
    }
}

#[test]
fn only_the_dedicated_edge_peer_can_mark_a_session_public() {
    let state = test_state("http://127.0.0.1:9", 2, Duration::from_secs(600));
    let request = |peer: &str| {
        let (mut parts, _) =
            Request::builder().header("x-den-public-session", "1").body(()).unwrap().into_parts();
        parts.extensions.insert(crate::Peer(peer.parse().unwrap()));
        parts
    };
    assert!(crate::public_session_request(&state, &request("10.89.0.10")));
    assert!(!crate::public_session_request(&state, &request("192.168.86.149")));
    assert!(!crate::public_session_request(&state, &request("10.89.0.11")));
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
    call_with(state, method, path, cookie, &[], body).await
}

async fn call_with(
    state: &Arc<AppState>,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    headers: &[(&str, &str)],
    body: &str,
) -> Reply {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(c) = cookie {
        b = b.header("cookie", c);
    }
    for (name, value) in headers {
        b = b.header(*name, *value);
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
/// behind itself, then at zero — and check every segment with ffprobe. `extra` goes into the request
/// (`,"playable":{…}`); the master playlist, the init and the joined file come back for codec-specific checks.
async fn end_to_end(
    imdb: &str,
    fixture_name: &str,
    codec_prefix: &str,
    scoped: bool,
    extra: &str,
) -> (String, Bytes, PathBuf) {
    let origin = origin().await;
    let state = test_state(&origin, 2, Duration::from_secs(600));
    let cookie = login(&state, "phone-key").await;
    // The scoped scout URL is the primary path; without one the server's own install is the fallback.
    let body = match scoped {
        true => format!(r#"{{"imdb":"{imdb}","scout":"{origin}/{SCOPED}"{extra}}}"#),
        false => format!(r#"{{"imdb":"{imdb}"{extra}}}"#),
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
        assert!(r.headers["cache-control"].to_str().unwrap().ends_with(", immutable"), "seg{i}");
        assert_eq!(r.headers["accept-ranges"], "none");
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
    // The audio has no hole at any join, those between two runs included: each packet starts where the one before it
    // ended, give or take a few samples of the encoder's priming. A run restarted at a keyframe reads its audio from the
    // demuxer's position a little before it (`-noaccurate_seek`), so where it joins the run before, one packet may
    // overlap the last one that run made — at most one frame, which a player drops.
    let audio: Vec<(f64, f64)> = ffprobe(
        &["-select_streams", "a:0", "-show_entries", "packet=pts_time,duration_time", "-of", "csv=p=0"],
        &whole,
    )
    .lines()
    .filter_map(|l| {
        let (pts, d) = l.split_once(',')?;
        Some((pts.parse().ok()?, d.parse().ok()?))
    })
    .collect();
    for w in audio.windows(2) {
        let gap = w[1].0 - (w[0].0 + w[0].1);
        assert!(gap < 0.005 && gap > -(w[0].1 + 0.005), "the audio jumps {gap:.4}s at {:.3}", w[1].0);
    }

    // A player that can't play it says why, into the log; a report that isn't one is refused.
    let report =
        call(&state, "POST", &format!("{base}report"), None, r#"{"code":3,"message":"DECODE"}"#).await;
    assert_eq!(report.status, StatusCode::NO_CONTENT);
    assert_eq!(
        call(&state, "POST", &format!("{base}report"), None, "{}").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&state, "POST", &format!("{base}report"), None, r#"{"code":3,"message":"again"}"#).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&state, "POST", &format!("{base}report"), None, r#"{"code":3,"message":"fourth"}"#).await.status,
        StatusCode::TOO_MANY_REQUESTS,
        "a signed URL cannot turn reports into an unbounded log writer"
    );

    let signed_speed = call(&state, "GET", &format!("{base}speed?bytes=1024"), None, "").await;
    assert_eq!(signed_speed.status, StatusCode::OK);
    assert_eq!(signed_speed.body.len(), 1024);
    assert_eq!(signed_speed.headers["access-control-allow-origin"], "*");
    assert_eq!(
        call(&state, "GET", &format!("{base}speed?bytes=1024"), None, "").await.status,
        StatusCode::OK,
        "one interrupted measurement may retry"
    );
    assert_eq!(
        call(&state, "GET", &format!("{base}speed?bytes=1024"), None, "").await.status,
        StatusCode::TOO_MANY_REQUESTS,
        "a signed session has a bounded speed-probe budget"
    );

    // Ending it: 204, then 410 for anything under its URL, and its scratch is gone.
    let sid = created["sid"].as_str().unwrap();
    let dir = state.cfg.scratch_dir.join(format!("s-{sid}"));
    assert!(dir.exists());
    let del = call(&state, "DELETE", base.trim_end_matches('/'), None, "").await;
    assert_eq!(del.status, StatusCode::NO_CONTENT);
    assert_eq!(call(&state, "GET", &format!("{base}seg0.m4s"), None, "").await.status, StatusCode::GONE);
    assert_eq!(call(&state, "GET", &playlist, None, "").await.status, StatusCode::GONE);
    // Tombstones reveal only that a holder's session is gone; they never retain a reporting endpoint.
    let late = call(&state, "POST", &format!("{base}report"), None, r#"{"code":3,"message":"DECODE"}"#).await;
    assert_eq!(late.status, StatusCode::GONE, "a report after the end is refused");
    assert!(!dir.exists(), "scratch left behind");
    assert_eq!(state.scratch_bytes.load(Relaxed), 0);
    (master.text(), init.body, whole)
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

/// Each release says whether it plays for the player that reported what it decodes, and stays a plain `yes` for one
/// that reported nothing.
#[tokio::test]
async fn the_releases_list_says_how_each_plays_for_the_player_that_asked() {
    let origin = origin().await;
    let state = test_state(&origin, 2, Duration::from_secs(60));
    // tt0000006: a release scout says is Dolby Vision profile 5, and an H.264 one.
    let ask = |extra: &'static str| {
        let (state, body) =
            (state.clone(), format!(r#"{{"imdb":"tt0000006","scout":"{origin}/cfg"{extra}}}"#));
        async move {
            let r = call(&state, "POST", "/remux/releases", None, &body).await;
            let list = r.json()["releases"].as_array().unwrap().clone();
            let by = |name: &str| list.iter().find(|r| r["filename"] == name).unwrap().clone();
            (by("dv5.mkv"), by("h264.mkv"))
        }
    };
    let (dv5, h264) = ask("").await;
    assert_eq!((dv5["plays"].as_str(), h264["plays"].as_str()), (Some("yes"), Some("yes")), "no report");
    assert!(dv5["why"].is_null() && h264["label"] == "H.264", "the existing fields stay: {h264}");
    let chrome = r#","playable":{"h264":51,"hevcMain10":153,"hdr":true}"#;
    let (dv5, h264) = ask(chrome).await;
    assert_eq!(h264["plays"], "yes", "{h264}");
    assert_ne!(dv5["plays"], "yes", "{dv5}");
    assert!(dv5["why"].as_str().is_some_and(|w| w.contains("Dolby Vision profile 5")), "{dv5}");
    let safari = r#","playable":{"h264":51,"hevcMain10":153,"hdr":true,"dolbyVision":{"p5":true}}"#;
    assert_eq!(ask(safari).await.0["plays"], "yes");
    // A release opened and ruled out is `no` for a browser that can't show it, `yes` for one that can.
    let recordless = probe::MediaInfo {
        container: "matroska",
        duration: 1.0,
        video: probe::VideoCodec::Hevc,
        codecs: Some("hvc1.2.4.L153.B0".into()),
        width: 3840,
        height: 2160,
        hdr: false,
        hlg: false,
        frame_rate: None,
        dolby_vision: Some(probe::DolbyVision { profile: 5, compat: 0, level: 6 }),
        dolby_vision_record_mismatch: false,
        dolby_vision_recordless: true,
        audio: Vec::new(),
        subtitles: Vec::new(),
        keyframes: Vec::new(),
    };
    state.known().put("tt0000006/dv5.mkv".into(), &recordless);
    let (dv5, _) = ask(chrome).await;
    assert_eq!(dv5["plays"], "no", "{dv5}");
    assert_eq!(dv5["why"], "Dolby Vision profile 5 (no record in the file) — this browser can't show it");
    assert_eq!(ask(safari).await.0["plays"], "yes");
}

#[tokio::test]
#[ignore]
async fn h264_matroska_end_to_end() {
    end_to_end("tt0000001", "h264.mkv", "avc1.64", true, "").await;
}

#[tokio::test]
#[ignore]
async fn hevc_matroska_end_to_end() {
    end_to_end("tt0000002", "hevc.mkv", "hvc1.1.6.L", false, "").await;
}

#[tokio::test]
#[ignore]
async fn mp4_end_to_end() {
    end_to_end("tt0000003", "h264.mp4", "avc1.64", true, "").await;
}

/// AV1 copied: an `av01` sample entry with its `av1C` and HDR10's colours in the init, the same keyframe cuts and
/// continuous timestamps as H.264 and HEVC, and a master naming the long codec string and PQ.
#[tokio::test]
#[ignore]
async fn av1_matroska_end_to_end() {
    let playable = r#","playable":{"h264":51,"av1":13,"av1Main10":13,"av1Hdr":true}"#;
    let (master, init, _) = end_to_end("tt0000011", "av1.mkv", "av01.0.", true, playable).await;
    assert!(master.contains("M.10.0.110.09.16.09.0,mp4a.40.2\",VIDEO-RANGE=PQ,"), "{master}");
    let has = |fourcc: &[u8]| init.windows(4).any(|w| w == fourcc);
    assert!(has(b"av01") && has(b"av1C") && has(b"colr"), "the av01 sample entry, its av1C and colr");
    assert!(!has(b"hvc1") && !has(b"dvcC"));
    let file = temp_dir().join("init.mp4");
    std::fs::write(&file, &init).unwrap();
    let stream = ffprobe(
        &["-select_streams", "v:0", "-show_entries", "stream=codec_name,color_transfer", "-of", "csv=p=0"],
        &file,
    );
    assert_eq!(stream.trim(), "av1,smpte2084", "the init alone describes AV1 in PQ");
}

/// 5.1 for a player that plays multichannel AAC: surround.mkv's 5.1 track, and its 7.1 one folded down, come out as
/// six-channel AAC-LC in 5.1 — in the init, so every run's segments share it — named in the master with CHANNELS="6",
/// and cut, restarted and joined as the stereo remuxes are.
#[tokio::test]
#[ignore]
async fn surround_matroska_end_to_end() {
    for track in [0, 1] {
        let extra = format!(r#","audioTrack":{track},"playable":{{"h264":51,"aacMultichannel":true}}"#);
        let (master, init, whole) = end_to_end("tt0000013", "surround.mkv", "avc1.64", true, &extra).await;
        assert!(master.contains("CHANNELS=\"6\"") && master.contains(",AUDIO=\"audio\""), "{master}");
        let file = temp_dir().join("init.mp4");
        std::fs::write(&file, &init).unwrap();
        let entries =
            ["-select_streams", "a:0", "-show_entries", "stream=codec_name,channels,channel_layout"];
        let stream = ffprobe(&[&entries[..], &["-of", "csv=p=0"]].concat(), &file);
        assert_eq!(stream.trim(), "aac,6,5.1", "track {track}: the init alone describes 5.1 AAC");
        let profile =
            ffprobe(&["-select_streams", "a:0", "-show_entries", "stream=profile", "-of", "csv=p=0"], &whole);
        // A full ffprobe names the profile; the image's restricted build, with no profile names compiled in, prints
        // its number, and AAC-LC is profile 1 (`AV_PROFILE_AAC_LOW`). Either says the same thing.
        assert!(matches!(profile.trim(), "LC" | "1"), "track {track}: AAC-LC, got {profile}");
    }
}

/// An AV1 release plays for a player whose report says it decodes it — its level, its 10 bits, its PQ — named so in
/// the master; for any other player it's passed over, never converted, and one scout names AV1 isn't even opened.
/// Creating a session starts no ffmpeg.
#[tokio::test]
async fn av1_plays_only_for_a_player_that_reports_it() {
    let origin = origin().await;
    let state = test_state(&origin, 4, Duration::from_secs(600));
    state.transcode_ok.store(true, Relaxed);
    let body =
        |imdb: &str, playable: &str| format!(r#"{{"imdb":"{imdb}","scout":"{origin}/cfg"{playable}}}"#);
    let chrome = r#","playable":{"h264":51,"av1":13,"av1Main10":13,"av1Hdr":true}"#;
    let r = call(&state, "POST", "/remux/session", None, &body("tt0000011", chrome)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let j = r.json();
    let copied = serde_json::json!({
        "codec": "av1", "transcoded": false, "width": 320, "height": 180, "tonemapped": false
    });
    assert_eq!(j["video"], copied);
    let master = call(&state, "GET", j["playlist"].as_str().unwrap(), None, "").await.text();
    assert!(
        master.contains("CODECS=\"av01.0.")
            && master.contains(",VIDEO-RANGE=PQ,RESOLUTION=320x180,FRAME-RATE=24.000\n"),
        "{master}"
    );
    state.end_all("test").await;

    // Found by the probe alone — scout names no codec — and beyond the player: nothing plays.
    for refused in [
        r#","playable":{"h264":51,"av1":13,"av1Main10":13}"#,
        r#","playable":{"h264":51,"av1":13,"av1Hdr":true}"#,
        r#","playable":{"h264":51,"hevcMain10":153,"hdr":true}"#,
        "",
    ] {
        let r = call(&state, "POST", "/remux/session", None, &body("tt0000011", refused)).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{refused}: {}", r.text());
        assert_eq!(r.json()["error"], "no_playable_release", "{refused}");
    }

    // Named AV1 by scout: unopened for a player without AV1, which gets the H.264 release; first for one with it.
    let r =
        call(&state, "POST", "/remux/session", None, &body("tt0000012", r#","playable":{"h264":51}"#)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(r.json()["release"]["filename"], "h264.mkv");
    assert_eq!(AV1_PLAYS.load(Relaxed), 0, "the AV1 release was opened for a player without AV1");
    state.end_all("test").await;
    let r = call(&state, "POST", "/remux/session", None, &body("tt0000012", chrome)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    // Its own name: the release tt0000011 opened above is remembered, and would be taken without following /p/av1/.
    assert_eq!(r.json()["release"]["filename"], "av1-named.mkv");
    assert_eq!(AV1_PLAYS.load(Relaxed), 1);
    state.end_all("test").await;
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

/// A title is tried past the first three releases that won't open. No ffmpeg needed.
#[tokio::test]
async fn the_search_goes_past_three_releases_that_wont_open() {
    let origin = origin().await;
    let state = test_state(&origin, 2, Duration::from_secs(600));
    let with = |imdb: &str| format!(r#"{{"imdb":"{imdb}","scout":"{origin}/cfg"}}"#);
    let r = call(&state, "POST", "/remux/session", None, &with("tt0000008")).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(r.json()["release"]["filename"], "h264.mkv", "the fifth release");
    state.end_all("test").await;
}

/// A release whose file is in neither container is opened once: the next session skips it unopened, and so does the
/// next process, which reads the verdict back from scratch. A release named .m2ts is never opened, and one whose
/// debrid sent an error page in its place is tried again. No ffmpeg needed.
#[tokio::test]
async fn a_release_that_plays_nowhere_is_opened_once() {
    let origin = origin().await;
    let state = test_state(&origin, 4, Duration::from_secs(600));
    let body = format!(r#"{{"imdb":"tt0000016","scout":"{origin}/cfg"}}"#);
    for _ in 0..2 {
        let r = call(&state, "POST", "/remux/session", None, &body).await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
        assert_eq!(r.json()["release"]["filename"], "h264.mkv");
        state.end_all("test").await;
    }
    assert_eq!(UNPLAYABLE_PLAYS.load(Relaxed), 1, "the MPEG-TS file was opened again, or the .m2ts at all");
    assert_eq!(ERROR_PAGE_PLAYS.load(Relaxed), 2, "an error page is the moment's, not the file's");

    let key = "tt0000016/307200/Film.mkv";
    let now = crate::state::unix_now();
    let why = state.unplayable().get(key, now).expect("remembered");
    assert!(why.contains(probe::NEITHER), "{why}");
    let path = state.cfg.scratch_dir.join(crate::state::UNPLAYABLE_FILE);
    for _ in 0..50 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let restarted = state_from(Config {
        scratch_dir: state.cfg.scratch_dir.clone(),
        ..test_config(&origin, 4, Duration::ZERO, None)
    });
    restarted.load_unplayable();
    assert_eq!(restarted.unplayable().get(key, now), Some(why));
    assert!(restarted.unplayable().get("tt0000016/307200/Film.WEB.mkv", now).is_none());
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

fn edge(owner: &str) -> [(&str, &str); 2] {
    [("x-den-edge-secret", EDGE_SECRET), ("x-den-owner", owner)]
}

/// `POST /remux/session` for the scout install at `{origin}/cfg`, with `headers`.
async fn start_with(
    state: &Arc<AppState>,
    origin: &str,
    cookie: Option<&str>,
    headers: &[(&str, &str)],
) -> Reply {
    let body = format!(r#"{{"imdb":"tt0000001","scout":"{origin}/cfg"}}"#);
    call_with(state, "POST", "/remux/session", cookie, headers, &body).await
}

/// How many published sessions each owner has, by owner.
fn owned(state: &Arc<AppState>) -> std::collections::BTreeMap<String, usize> {
    let mut by = std::collections::BTreeMap::new();
    for s in state.sessions().values() {
        *by.entry(s.owner.clone()).or_default() += 1;
    }
    by
}

/// A guest's owner is honoured only with the shared secret and a `grant:<8 hex>` name; anything else plays as
/// the install the request names, as if the headers were not there.
#[tokio::test]
async fn a_guest_owner_is_honoured_only_with_the_edge_secret() {
    let origin = origin().await;
    let state = test_state(&origin, 4, Duration::from_secs(600));
    let r = start_with(&state, &origin, None, &edge("grant:0a1b2c3d")).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(owned(&state), [("grant:0a1b2c3d".to_string(), 1)].into());
    state.end_all("test").await;

    let ignored = [
        vec![("x-den-owner", "grant:0a1b2c3d")],
        vec![("x-den-edge-secret", "wrong"), ("x-den-owner", "grant:0a1b2c3d")],
        vec![("x-den-edge-secret", EDGE_SECRET)],
        vec![("x-den-edge-secret", EDGE_SECRET), ("x-den-owner", "grant:0A1B2C3D")],
        vec![("x-den-edge-secret", EDGE_SECRET), ("x-den-owner", "grant:0a1b2c3")],
        vec![("x-den-edge-secret", EDGE_SECRET), ("x-den-owner", "install:0a1b2c3d0a1b2c3d")],
    ];
    for headers in ignored {
        let r = start_with(&state, &origin, None, &headers).await;
        assert_eq!(r.status, StatusCode::CREATED, "{headers:?}: {}", r.text());
        let owners = owned(&state);
        assert!(
            owners.keys().all(|o| o.starts_with("install:") && o != "install:0a1b2c3d0a1b2c3d"),
            "{headers:?}: {owners:?}"
        );
        state.end_all("test").await;
    }

    let off =
        state_from(Config { edge_secret: None, ..test_config(&origin, 4, Duration::from_secs(600), None) });
    let r = start_with(&off, &origin, None, &edge("grant:0a1b2c3d")).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert!(owned(&off).keys().all(|o| o.starts_with("install:")), "{:?}", owned(&off));
    off.end_all("test").await;

    let body = format!(r#"{{"imdb":"tt0000001","scout":"{origin}/cfg"}}"#);
    let r = call_with(&state, "POST", "/remux/releases", None, &edge("grant:0a1b2c3d"), &body).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
}

/// A grant plays `GUEST_MAX_SESSIONS` at once and its oldest gives way; that never touches a host's sessions,
/// and a host starting more never touches a grant's.
#[tokio::test]
async fn a_guest_has_its_own_cap_and_never_evicts_a_host() {
    let origin = origin().await;
    let state = test_state(&origin, 10, Duration::from_secs(600));
    let phone = login(&state, "phone-key").await;
    let host = start_with(&state, &origin, Some(&phone), &[]).await;
    assert_eq!(host.status, StatusCode::CREATED, "{}", host.text());
    let host_sid = host.json()["sid"].as_str().unwrap().to_string();

    let mut guest_sids = Vec::new();
    for _ in 0..3 {
        let r = start_with(&state, &origin, None, &edge("grant:0a1b2c3d")).await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
        guest_sids.push(r.json()["sid"].as_str().unwrap().to_string());
    }
    assert!(state.session(&guest_sids[0]).is_none(), "the grant's oldest gave way");
    assert!(state.session(&guest_sids[1]).is_some() && state.session(&guest_sids[2]).is_some());
    assert!(state.session(&host_sid).is_some(), "a guest evicted a host");

    // The host's own limits move without the grant: its browser replaces its own session, its installs fill theirs.
    for _ in 0..3 {
        let r = start_with(&state, &origin, Some(&phone), &[]).await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    }
    for _ in 0..3 {
        let r = start_with(&state, &origin, None, &[]).await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    }
    assert!(guest_sids[1..].iter().all(|s| state.session(s).is_some()), "a host evicted a guest");
    let owners = owned(&state);
    assert_eq!(owners["grant:0a1b2c3d"], 2);
    assert_eq!(owners.iter().filter(|(o, _)| o.starts_with("install:")).map(|(_, n)| n).sum::<usize>(), 2);

    // Another grant has a cap of its own.
    for _ in 0..2 {
        let r = start_with(&state, &origin, None, &edge("grant:deadbeef")).await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    }
    assert_eq!(owned(&state)["grant:0a1b2c3d"], 2);
    assert_eq!(owned(&state)["grant:deadbeef"], 2);
    state.end_all("test").await;
}

/// A grant never takes the GPU: a release that needs converting is refused as it is when no transcode is free,
/// and the transcode stays there for a host.
#[tokio::test]
async fn a_guest_is_never_transcoded() {
    let origin = origin().await;
    let state = test_state(&origin, 4, Duration::from_secs(600));
    state.transcode_ok.store(true, Relaxed);
    let body = format!(r#"{{"imdb":"tt0000002","scout":"{origin}/cfg","videoCodecs":["h264"]}}"#);
    let r = call_with(&state, "POST", "/remux/session", None, &edge("grant:0a1b2c3d"), &body).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{}", r.text());
    assert_eq!(r.json()["error"], "transcode_unavailable");
    assert_eq!(state.transcodes.load(Relaxed), 0);

    let r = call(&state, "POST", "/remux/session", None, &body).await;
    assert_eq!(r.status, StatusCode::CREATED, "the host has the transcode: {}", r.text());
    assert_eq!(r.json()["video"]["transcoded"], true);
    state.end_all("test").await;
}

/// `/remux/admin/kill` ends one grant's sessions as a DELETE does and no one else's.
#[tokio::test]
async fn kill_ends_the_sessions_of_one_grant_only() {
    let origin = origin().await;
    let state = test_state(&origin, 6, Duration::from_secs(600));
    let phone = login(&state, "phone-key").await;
    let mut sids = Vec::new();
    for (cookie, owner) in [
        (None, "grant:0a1b2c3d"),
        (None, "grant:0a1b2c3d"),
        (None, "grant:deadbeef"),
        (Some(phone.as_str()), ""),
    ] {
        let headers = if owner.is_empty() { Vec::new() } else { edge(owner).to_vec() };
        let r = start_with(&state, &origin, cookie, &headers).await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
        sids.push((
            r.json()["sid"].as_str().unwrap().to_string(),
            r.json()["playlist"].as_str().unwrap().to_string(),
        ));
    }
    let scratch = |sid: &str| state.cfg.scratch_dir.join(format!("s-{sid}"));
    assert!(sids.iter().all(|(sid, _)| scratch(sid).exists()));

    let kill = |owner: &str| {
        let body = format!(r#"{{"owner":"{owner}"}}"#);
        let state = state.clone();
        async move {
            call_with(&state, "POST", "/remux/admin/kill", None, &[("x-den-edge-secret", EDGE_SECRET)], &body)
                .await
        }
    };
    let r = kill("grant:0a1b2c3d").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(r.json(), serde_json::json!({"ended": 2}));
    for (sid, playlist) in &sids[..2] {
        assert!(state.session(sid).is_none() && !scratch(sid).exists(), "scratch left behind");
        assert_eq!(call(&state, "GET", playlist, None, "").await.status, StatusCode::GONE);
    }
    for (sid, _) in &sids[2..] {
        assert!(state.session(sid).is_some() && scratch(sid).exists(), "another owner's session was ended");
    }
    assert_eq!(kill("grant:0a1b2c3d").await.json(), serde_json::json!({"ended": 0}));
    // Only a grant's owner can be named: never a host's browser or install.
    assert_eq!(kill("install:0a1b2c3d0a1b2c3d").await.status, StatusCode::BAD_REQUEST);
    assert_eq!(state.sessions().len(), 2);
    state.end_all("test").await;
}

/// Without the secret — wrong, missing, or the feature off — the admin route answers exactly as a route that
/// does not exist, CORS and allow headers included.
#[tokio::test]
async fn kill_without_the_secret_looks_like_an_unknown_route() {
    let origin = origin().await;
    let state = test_state(&origin, 2, Duration::from_secs(600));
    let off =
        state_from(Config { edge_secret: None, ..test_config(&origin, 2, Duration::from_secs(600), None) });
    let body = r#"{"owner":"grant:0a1b2c3d"}"#;
    let unknown = call(&state, "POST", "/remux/admin/nothing", None, body).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    for (label, st, method, headers) in [
        ("wrong", &state, "POST", vec![("x-den-edge-secret", "wrong")]),
        ("empty", &state, "POST", vec![("x-den-edge-secret", "")]),
        ("missing", &state, "POST", vec![]),
        ("get with the secret", &state, "GET", vec![("x-den-edge-secret", EDGE_SECRET)]),
        ("feature off", &off, "POST", vec![("x-den-edge-secret", EDGE_SECRET)]),
        ("feature off, empty", &off, "POST", vec![("x-den-edge-secret", "")]),
        ("with an origin", &state, "POST", vec![("origin", "https://d.example")]),
    ] {
        let r = call_with(st, method, "/remux/admin/kill", None, &headers, body).await;
        assert_eq!(r.status, unknown.status, "{label}");
        assert_eq!(r.headers, unknown.headers, "{label}");
        assert_eq!(r.body, unknown.body, "{label}");
    }
    assert!(!unknown.headers.keys().any(|k| k.as_str().starts_with("access-control") || k == "allow"));
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

/// Safari and Apple's other native players give up on an init.mp4 that takes more than about five seconds, so a
/// native player's session is answered only once its init is written; hls.js's is answered at once, with no ffmpeg.
#[tokio::test]
#[ignore]
async fn a_native_players_session_answers_with_its_init_ready() {
    let origin = origin().await;
    let state = test_state(&origin, 2, Duration::from_secs(600));
    let cookie = login(&state, "phone-key").await;
    let before = state.jobs_started.load(Relaxed);
    let r =
        call(&state, "POST", "/remux/session", Some(&cookie), r#"{"imdb":"tt0000001","player":"hls.js"}"#)
            .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(state.jobs_started.load(Relaxed), before, "hls.js waits for init.mp4 itself");
    assert!(!state.session(r.json()["sid"].as_str().unwrap()).unwrap().init_ready());

    let r =
        call(&state, "POST", "/remux/session", Some(&cookie), r#"{"imdb":"tt0000001","player":"native"}"#)
            .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let j = r.json();
    assert!(
        state.session(j["sid"].as_str().unwrap()).unwrap().init_ready(),
        "the init is written before the answer"
    );
    let base = j["playlist"].as_str().unwrap().trim_end_matches("master.m3u8").to_string();
    let started = std::time::Instant::now();
    let init = call(&state, "GET", &format!("{base}init.mp4"), None, "").await;
    assert_eq!(init.status, StatusCode::OK);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "served from what was prepared: {:?}",
        started.elapsed()
    );
    state.end_all("test").await;
}

/// A player that plays Dolby audio in fMP4 HLS gets an E-AC-3 (hevc.mkv) or AC-3 (h264.mkv's first track) track
/// copied, named in the master with an audio rendition giving its channels. Another track, or a player that
/// doesn't say so, gets AAC stereo. Creating a session starts no ffmpeg.
#[tokio::test]
async fn dolby_audio_is_copied_for_a_player_that_plays_it() {
    let origin = origin().await;
    let state = test_state(&origin, 4, Duration::from_secs(600));
    let cookie = login(&state, "phone-key").await;
    let apple = r#""playable":{"h264":51,"hevcMain":153,"hevcMain10":153,"eac3":true}"#;
    let cases = [
        (format!(r#"{{"imdb":"tt0000002",{apple}}}"#), "ec-3"),
        (format!(r#"{{"imdb":"tt0000001",{apple}}}"#), "ac-3"),
        (format!(r#"{{"imdb":"tt0000001","audioTrack":1,{apple}}}"#), "mp4a.40.2"),
        (r#"{"imdb":"tt0000002","playable":{"hevcMain":153}}"#.to_string(), "mp4a.40.2"),
    ];
    for (body, audio) in cases {
        let r = call(&state, "POST", "/remux/session", Some(&cookie), &body).await;
        assert_eq!(r.status, StatusCode::CREATED, "{body}: {}", r.text());
        let master = call(&state, "GET", r.json()["playlist"].as_str().unwrap(), None, "").await.text();
        assert!(master.contains(&format!(",{audio}\"")), "{body}: {master}");
        let named = master.contains("TYPE=AUDIO") && master.contains("CHANNELS=\"2\"");
        assert_eq!(named, audio != "mp4a.40.2", "{body}: {master}");
    }
    state.end_all("test").await;
}

/// Converted audio stays 5.1 for a player that plays multichannel AAC, from surround.mkv's 5.1 track and its 7.1 one.
/// A player that doesn't say so gets stereo, as does a mono track (h264.mkv's second); a copied Dolby track keeps its
/// own channels. The answer names what the session carries beside each track's own. Creating a session starts no ffmpeg.
#[tokio::test]
async fn converted_audio_stays_5_1_for_a_player_that_plays_it() {
    let origin = origin().await;
    let state = test_state(&origin, 4, Duration::from_secs(600));
    let cookie = login(&state, "phone-key").await;
    let chrome = r#""playable":{"h264":51,"aacMultichannel":true}"#;
    let apple = r#""playable":{"h264":51,"hevcMain":153,"eac3":true,"aacMultichannel":true}"#;
    let cases = [
        (format!(r#"{{"imdb":"tt0000013",{chrome}}}"#), 6, 6),
        (format!(r#"{{"imdb":"tt0000013","audioTrack":1,{chrome}}}"#), 8, 6),
        (r#"{"imdb":"tt0000013","playable":{"h264":51}}"#.to_string(), 6, 2),
        (format!(r#"{{"imdb":"tt0000001","audioTrack":1,{chrome}}}"#), 1, 2),
        (format!(r#"{{"imdb":"tt0000002",{apple}}}"#), 2, 2),
    ];
    for (body, source, carried) in cases {
        let r = call(&state, "POST", "/remux/session", Some(&cookie), &body).await;
        assert_eq!(r.status, StatusCode::CREATED, "{body}: {}", r.text());
        let j = r.json();
        let track = j["audioTrack"].as_u64().unwrap() as usize;
        assert_eq!(j["audioTracks"][track]["channels"], source, "{body}");
        assert_eq!(j["audioChannels"], carried, "{body}");
        let master = call(&state, "GET", j["playlist"].as_str().unwrap(), None, "").await.text();
        assert_eq!(master.contains("CHANNELS=\"6\""), carried == 6, "{body}: {master}");
    }
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
    assert!(master.contains("LANGUAGE=\"en\",DEFAULT=NO,AUTOSELECT=YES"), "the first preference: {master}");
    let pl = call(&state, "GET", &format!("{base}sub0.m3u8"), None, "").await;
    assert_eq!(pl.status, StatusCode::OK);
    assert!(pl.text().contains("sub0.vtt"), "{}", pl.text());
    let en = call(&state, "GET", &format!("{base}sub0.vtt"), None, "").await;
    assert_eq!(en.headers["content-type"], "text/vtt; charset=utf-8");
    assert_eq!(en.headers["access-control-allow-origin"], "*");
    assert!(en.text().contains("X-TIMESTAMP-MAP=MPEGTS:0") && en.text().contains("Hello"), "{}", en.text());
    // Fixed for the session, so kept for as long as it lives and no longer.
    let expires = j["expiresAt"].as_u64().unwrap();
    for file in ["master.m3u8", "media.m3u8", "sub0.m3u8", "sub0.vtt"] {
        let r = call(&state, "GET", &format!("{base}{file}"), None, "").await;
        assert_kept_for_session(&r, expires, file);
    }
    let fi = call(&state, "GET", &format!("{base}sub1.vtt"), None, "").await;
    assert_eq!(fi.text(), crate::subs::EMPTY, "the Finnish subtitle is on a refused origin");
    assert_eq!(fi.headers["cache-control"], "no-store", "an empty subtitle is not kept");
    assert_eq!(call(&state, "GET", &format!("{base}sub2.vtt"), None, "").await.status, StatusCode::NOT_FOUND);
    state.end_all("test").await;

    let off = r#"{"imdb":"tt0000001","subtitles":"http://169.254.169.254/subs","subtitleLanguages":["en"]}"#;
    let r = call(&state, "POST", "/remux/session", Some(&cookie), off).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["error"], "bad_subtitles");
}

/// `Cache-Control: private, max-age=<n>` with n the session's remaining life.
fn assert_kept_for_session(r: &Reply, expires: u64, what: &str) {
    assert_eq!(r.status, StatusCode::OK, "{what}: {}", r.text());
    let cc = r.headers["cache-control"].to_str().unwrap();
    let age: u64 = cc
        .strip_prefix("private, max-age=")
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("{what}: {cc}"));
    let left = expires - crate::state::unix_now();
    assert!(age > 0 && age <= left + 1 && age + 2 >= left, "{what}: {cc}, the session has {left}s left");
}

/// A den-subtitles list or subtitle that fails is asked for again on the player's next request, and served empty
/// and unkept meanwhile; once found, the subtitle is kept for the session and not fetched again.
#[tokio::test]
async fn a_subtitle_that_failed_once_is_tried_again() {
    let origin = origin().await;
    let state = test_state(&origin, 2, Duration::from_secs(600));
    let cookie = login(&state, "phone-key").await;
    let body = format!(r#"{{"imdb":"tt0000001","subtitles":"{origin}/flaky","subtitleLanguages":["en"]}}"#);
    let r = call(&state, "POST", "/remux/session", Some(&cookie), &body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let j = r.json();
    let vtt = format!("{}sub0.vtt", j["playlist"].as_str().unwrap().trim_end_matches("master.m3u8"));
    for why in ["the list failed", "the subtitle failed"] {
        let r = call(&state, "GET", &vtt, None, "").await;
        assert_eq!(r.text(), crate::subs::EMPTY, "{why}");
        assert_eq!(r.headers["cache-control"], "no-store", "{why}");
    }
    let found = call(&state, "GET", &vtt, None, "").await;
    assert!(found.text().contains("Hello"), "{}", found.text());
    assert_kept_for_session(&found, j["expiresAt"].as_u64().unwrap(), "sub0.vtt");
    assert!(call(&state, "GET", &vtt, None, "").await.text().contains("Hello"));
    assert_eq!((FLAKY_LISTS.load(Relaxed), FLAKY_SUBTITLES.load(Relaxed)), (2, 2), "kept once found");
    state.end_all("test").await;
}

/// A release that carries its own text subtitles offers them: a language den-subtitles has nothing in is the release's
/// track, cut per video segment; one den-subtitles has stays its one document; and the release's other languages are
/// offered with no den-subtitles or language named at all. Forced tracks are never a rendition.
#[tokio::test]
async fn a_release_offers_its_own_subtitles_where_den_subtitles_has_none() {
    let origin = origin().await;
    let state = test_state(&origin, 2, Duration::from_secs(600));
    let cookie = login(&state, "phone-key").await;
    let body =
        format!(r#"{{"imdb":"tt0000015","subtitles":"{origin}/subs","subtitleLanguages":["en","fi"]}}"#);
    let r = call(&state, "POST", "/remux/session", Some(&cookie), &body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let j = r.json();
    assert_eq!(j["release"]["filename"], "subs.mkv");
    assert_eq!(
        j["subtitles"],
        serde_json::json!([{"language": "en", "name": "English"}, {"language": "fi", "name": "Finnish"}]),
        "the forced Swedish track is no rendition"
    );
    let base = j["playlist"].as_str().unwrap().trim_end_matches("master.m3u8").to_string();
    let master = call(&state, "GET", &format!("{base}master.m3u8"), None, "").await.text();
    assert!(master.contains("URI=\"sub0.m3u8\"") && master.contains("URI=\"sub1.m3u8\""), "{master}");
    assert!(!master.contains("URI=\"sub2.m3u8\""), "{master}");
    let en = call(&state, "GET", &format!("{base}sub0.m3u8"), None, "").await.text();
    assert!(en.contains("\nsub0.vtt\n") && !en.contains("sub0_0.vtt"), "den-subtitles has English: {en}");
    let fi = call(&state, "GET", &format!("{base}sub1.m3u8"), None, "").await;
    assert_kept_for_session(&fi, j["expiresAt"].as_u64().unwrap(), "sub1.m3u8");
    let media = call(&state, "GET", &format!("{base}media.m3u8"), None, "").await.text();
    let fi = fi.text();
    for i in 0..extinfs(&media).len() {
        assert!(fi.contains(&format!("\nsub1_{i}.vtt\n")), "a segment for video segment {i}: {fi}");
    }
    assert!(!fi.contains("sub1.vtt"), "{fi}");
    state.end_all("test").await;

    // Nothing asked for, nothing to ask of: the release's own languages, in its order.
    let r = call(&state, "POST", "/remux/session", Some(&cookie), r#"{"imdb":"tt0000015"}"#).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(
        r.json()["subtitles"],
        serde_json::json!([{"language": "en", "name": "English"}, {"language": "fi", "name": "Finnish"}])
    );
    let base = r.json()["playlist"].as_str().unwrap().trim_end_matches("master.m3u8").to_string();
    let en = call(&state, "GET", &format!("{base}sub0.m3u8"), None, "").await.text();
    assert!(en.contains("\nsub0_0.vtt\n"), "{en}");
    // No den-subtitles behind it: the whole-film document is empty, as any language's with nothing to offer.
    assert_eq!(call(&state, "GET", &format!("{base}sub0.vtt"), None, "").await.text(), crate::subs::EMPTY);
    // A segment past the playlist's, and a rendition past the last, are not there.
    let past = extinfs(&call(&state, "GET", &format!("{base}media.m3u8"), None, "").await.text()).len();
    assert_eq!(
        call(&state, "GET", &format!("{base}sub0_{past}.vtt"), None, "").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&state, "GET", &format!("{base}sub2_0.vtt"), None, "").await.status,
        StatusCode::NOT_FOUND
    );
    state.end_all("test").await;

    // A release with no text tracks, and a player that names none: no renditions, as before.
    let r = call(&state, "POST", "/remux/session", Some(&cookie), r#"{"imdb":"tt0000001"}"#).await;
    assert_eq!(r.json()["subtitles"], serde_json::json!([]));
    state.end_all("test").await;
}

/// The release's own subtitles come out of the same ffmpeg runs that copy its video, whatever order the player asks
/// in: the first segment asked for starts a run mid-file, one behind it another from zero, and every subtitle segment
/// then holds the cues showing in its video segment, over a cut included. Real ffmpeg, so `#[ignore]`d.
#[tokio::test]
#[ignore]
async fn a_releases_own_subtitles_arrive_with_its_video() {
    let origin = origin().await;
    let state = test_state(&origin, 2, Duration::from_secs(600));
    let cookie = login(&state, "phone-key").await;
    let r = call(
        &state,
        "POST",
        "/remux/session",
        Some(&cookie),
        r#"{"imdb":"tt0000015","subtitleLanguages":["fi"]}"#,
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let j = r.json();
    assert_eq!(
        j["subtitles"],
        serde_json::json!([{"language": "fi", "name": "Finnish"}, {"language": "en", "name": "English"}])
    );
    let base = j["playlist"].as_str().unwrap().trim_end_matches("master.m3u8").to_string();
    let media = call(&state, "GET", &format!("{base}media.m3u8"), None, "").await.text();
    let n = extinfs(&media).len();
    assert!(n >= 5, "{media}");
    let get = |file: String| {
        let (state, base) = (state.clone(), base.clone());
        async move { call(&state, "GET", &format!("{base}{file}"), None, "").await }
    };
    // Finnish is rendition 0, English 1. A window is the cues showing in it, on the film's timeline.
    let window = |r: Reply| {
        assert_eq!(r.status, StatusCode::OK, "{}", r.text());
        assert_eq!(r.headers["content-type"], "text/vtt; charset=utf-8");
        assert!(
            r.text().starts_with("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n"),
            "{}",
            r.text()
        );
        r.text()
    };

    // A seek: the run starts at segment 3's keyframe (19.5 s), and its Finnish cue at 20 s arrives.
    assert_eq!(get("init.mp4".into()).await.status, StatusCode::OK);
    assert_eq!(get("seg3.m4s".into()).await.status, StatusCode::OK);
    let w3 = window(get("sub0_3.vtt".into()).await);
    assert!(w3.contains("Hyvää yötä") && !w3.contains("Moi"), "{w3}");
    // Behind it: another run, from zero, brings the start of the film.
    assert_eq!(get("seg0.m4s".into()).await.status, StatusCode::OK);
    let (fi0, en0) = (window(get("sub0_0.vtt".into()).await), window(get("sub1_0.vtt".into()).await));
    assert!(fi0.contains("Moi") && fi0.contains("kaikille"), "an ASS event, as WebVTT text: {fi0}");
    assert!(en0.contains("Hello there") && !en0.contains("Inside"), "{en0}");
    assert!(state.jobs_started.load(Relaxed) >= 2, "the seek restarted the run");

    // The rest of the film, in order, on the last run: every segment's subtitles come with its video.
    let mut all = Vec::new();
    for i in 0..n {
        assert_eq!(get(format!("seg{i}.m4s")).await.status, StatusCode::OK, "seg{i}");
        all.push(window(get(format!("sub1_{i}.vtt")).await));
        window(get(format!("sub0_{i}.vtt")).await);
    }
    // Segments run 0–8, 8–13, 13–19.5, 19.5–24, 24–30.
    assert!(
        all[1].contains("Inside the second segment") && all[1].contains("Over the cut at thirteen"),
        "{}",
        all[1]
    );
    assert!(all[2].contains("Over the cut at thirteen") && all[2].contains("After the seek"), "{}", all[2]);
    assert!(all[4].contains("The end") && !all[4].contains("After the seek"), "{}", all[4]);
    assert!(!all.concat().contains("främmande"), "a forced track is not offered");
    let kept = get("sub1_2.vtt".into()).await;
    assert_kept_for_session(&kept, j["expiresAt"].as_u64().unwrap(), "sub1_2.vtt");
    state.end_all("test").await;
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
    // A tombstone does not keep its report endpoint alive.
    let ended = j["playlist"].as_str().unwrap();
    let late =
        call(&state, "POST", &ended.replace("master.m3u8", "report"), None, r#"{"code":3,"message":"x"}"#)
            .await;
    assert_eq!(late.status, StatusCode::GONE, "a report after the end is refused");
    assert_eq!(call(&state, "GET", ended, None, "").await.status, StatusCode::GONE);
    let r = call(&state, "POST", "/remux/session", Some(&laptop), h264_only).await;
    assert_eq!(r.status, StatusCode::CREATED, "the ended session gave its transcode back: {}", r.text());
    state.end_all("test").await;
}

/// With `maxBitrate` a release is copied only where its average bitrate fits: h264.mkv (217 kbit/s, ranked first)
/// gives way to hevc.mkv (165). With nothing that fits, what plays only converted is transcoded at 720p; with the GPU
/// off, the copy that needs least plays rather than none — as it does when a 720p transcode would need more than the
/// copy. Without `maxBitrate` nothing changes. Creating a session starts no ffmpeg.
#[tokio::test]
async fn a_player_naming_its_link_gets_a_release_that_fits_it() {
    let origin = origin().await;
    let state = test_state(&origin, 4, Duration::from_secs(600));
    let hevc = r#""playable":{"h264":51,"hevcMain":153}"#;
    let h264 = r#""playable":{"h264":51}"#;
    let start = |extra: String| {
        let (state, origin) = (state.clone(), origin.clone());
        async move {
            let body = format!(r#"{{"imdb":"tt0000014","scout":"{origin}/cfg",{extra}}}"#);
            let r = call(&state, "POST", "/remux/session", None, &body).await;
            assert_eq!(r.status, StatusCode::CREATED, "{body}: {}", r.text());
            let j = r.json();
            let master = call(&state, "GET", j["playlist"].as_str().unwrap(), None, "").await.text();
            state.end_all("test").await;
            (j, master)
        }
    };
    let cases = [
        (hevc.to_string(), "h264.mkv"),
        (format!(r#"{hevc},"maxBitrate":190000"#), "hevc.mkv"),
        (format!(r#"{hevc},"maxBitrate":100000"#), "hevc.mkv"),
        (format!(r#"{h264},"maxBitrate":100000"#), "h264.mkv"),
    ];
    for (extra, filename) in cases {
        let (j, _) = start(extra.clone()).await;
        assert_eq!(j["release"]["filename"], filename, "{extra}");
        assert_eq!(j["video"]["transcoded"], false, "{extra}");
    }

    state.transcode_ok.store(true, Relaxed);
    let (j, master) = start(format!(r#"{h264},"maxBitrate":100000"#)).await;
    assert_eq!(j["release"]["filename"], "hevc.mkv");
    assert_eq!(j["video"]["transcoded"], true);
    assert!(master.contains("BANDWIDTH=4692000,AVERAGE-BANDWIDTH=3000000,"), "the 720p preset: {master}");
    let (j, master) = start(h264.to_string()).await;
    assert_eq!(
        (j["release"]["filename"].as_str(), j["video"]["transcoded"].as_bool()),
        (Some("h264.mkv"), Some(false))
    );
    assert!(!master.contains("BANDWIDTH=4692000"), "{master}");

    let body = format!(r#"{{"imdb":"tt0000014","scout":"{origin}/cfg","maxBitrate":0}}"#);
    assert_eq!(call(&state, "POST", "/remux/session", None, &body).await.status, StatusCode::BAD_REQUEST);
}

/// `/remux/speed` sends the bytes asked for — 2 MiB unasked, 8 MiB at most — random and never stored, to anyone, as
/// `/health` answers anyone; the web app's origin may read it, and a visitor gets a few a minute.
#[tokio::test]
async fn the_speed_probe_sends_what_it_is_asked_for_and_no_more() {
    let state = test_state("http://127.0.0.1:9", 2, Duration::from_secs(600));
    let r = call(&state, "GET", "/remux/speed?bytes=100000", None, "").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!((r.body.len(), &r.headers["content-length"]), (100_000, &"100000".parse().unwrap()));
    assert_eq!(r.headers["cache-control"], "no-store");
    let again = call(&state, "GET", "/remux/speed?bytes=100000", None, "").await;
    assert_ne!(r.body, again.body, "random, so nothing on the way can compress it");
    assert_eq!(call(&state, "GET", "/remux/speed", None, "").await.body.len(), 2 * 1024 * 1024);
    assert_eq!(
        call(&state, "GET", "/remux/speed?bytes=99999999", None, "").await.body.len(),
        8 * 1024 * 1024
    );
    assert_eq!(
        call(&state, "GET", "/remux/speed?bytes=lots", None, "").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(call(&state, "POST", "/remux/speed", None, "").await.status, StatusCode::METHOD_NOT_ALLOWED);
    let head = call(&state, "HEAD", "/remux/speed?bytes=5", None, "").await;
    assert!(head.body.is_empty() && head.headers["content-length"] == "5");

    let from = |ip: &str| {
        Request::builder()
            .uri("/remux/speed?bytes=1")
            .header("origin", "https://d.example")
            .extension(crate::Peer(ip.parse().unwrap()))
            .body(Full::new(Bytes::new()))
            .unwrap()
    };
    let r = crate::handle_request(state.clone(), from("100.64.0.7")).await;
    assert_eq!(r.headers()["access-control-allow-origin"], "https://d.example");
    for _ in 1..crate::state::STARTS_PER_MINUTE {
        assert_eq!(crate::handle_request(state.clone(), from("100.64.0.7")).await.status(), StatusCode::OK);
    }
    let limited = crate::handle_request(state.clone(), from("100.64.0.7")).await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
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
    assert!(crate::metrics_body(&state).contains("remux_public_sessions 0\n"));
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
    // An unknown session is a plain 404. Only a verified signed path gets receiver CORS.
    let r =
        call(&state, "GET", "/remux/s/AAAAAAAAAAAAAAAAAAAAAA/AAAAAAAAAAAAAAAAAAAAAA/master.m3u8", None, "")
            .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(r.body.is_empty(), "unknown signed paths are bare 404s");
    assert!(r.headers.get("access-control-allow-origin").is_none());
    assert!(bad.headers.get("access-control-allow-origin").is_none());
    assert_eq!(call(&state, "GET", "/remux/login", None, "").await.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(call(&state, "GET", "/nope", None, "").await.json()["error"], "not_found");
    // /metrics without a token configured is the same 404.
    assert_eq!(call(&state, "GET", "/metrics", None, "").await.status, StatusCode::NOT_FOUND);
    assert_eq!(call(&state, "GET", "/health", None, "").await.json()["status"], "ok");
}
