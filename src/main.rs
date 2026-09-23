//! den-remux — Den's playback for browsers, AirPlay and Cast receivers.
//!
//!   POST /remux/login   {key}               → cookie for what needs this service to vouch
//!   POST /remux/session {imdb, season?, episode?, filename?, scout?, maxBitrate?} → a signed session: /remux/s/<sid>/<sig>/master.m3u8
//!   POST /remux/releases {imdb, season?, episode?, scout?, videoCodecs?, playable?} → what a session could play:
//!                        [{label, filename, size, plays: "yes"|"convert"|"no", why?}], no URLs. `plays` is decided by
//!                        `playable` as a session is (and by what opening a release has shown of it); without either
//!                        every release is "yes". A session's `release.requested {filename, why}` says why the release
//!                        it was asked for was passed over.
//!   GET  /remux/speed?bytes=<n>             → n random bytes (8 MiB at most), for a remote player to time its link
//!   GET  /remux/s/<sid>/<sig>/…             → HLS (fMP4) and a session-bound speed probe
//!   POST /remux/s/<sid>/<sig>/report {code?, message?, stats?} → why the player couldn't play it, and how playing
//!                        went (`report::stats_line`), into the log
//!   DELETE /remux/s/<sid>/<sig>             → end it (410 from then on)
//!   GET  /health, /metrics
//!
//! A release comes from den-scout — the full install the request names, which is its own credential; for a
//! logged-in browser also a scope=availability install opened with this service's key, or this service's
//! own install. Its video is copied, its audio re-encoded to AAC — stereo, or 5.1 or 7.1 for a player that plays it — and it
//! is served as a VOD playlist cut on its own keyframes.

mod auth;
mod client;
mod config;
mod httputil;
mod job;
mod lang;
mod playlist;
mod probe;
mod redact;
mod report;
mod scout;
mod session;
mod source;
mod state;
mod subs;

#[cfg(test)]
mod tests;

use std::convert::Infallible;
use std::future::Future;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use serde::Deserialize;
use tokio::net::TcpListener;

use crate::config::Config;
use crate::httputil::Body;
use crate::session::SubFile;
use crate::state::{unix_now, AppState};

/// How often one failure condition may write a line.
const LOG_EVERY: Duration = Duration::from_secs(60);

/// Log a failure at most once per `LOG_EVERY` per `condition`, saying how many were held back — the
/// log is for state changes, and in an outage every request fails the same way. den-reel's helper.
pub fn log_limited(condition: &str, line: impl FnOnce() -> String) {
    static SEEN: std::sync::Mutex<Vec<(String, std::time::Instant, u32)>> = std::sync::Mutex::new(Vec::new());
    let now = std::time::Instant::now();
    let held = {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        match seen.iter_mut().find(|(c, _, _)| c == condition) {
            Some((_, at, held)) if now.duration_since(*at) < LOG_EVERY => {
                *held += 1;
                return;
            }
            Some((_, at, held)) => {
                *at = now;
                std::mem::take(held)
            }
            None => {
                seen.push((condition.to_string(), now, 0));
                0
            }
        }
    };
    match held {
        0 => eprintln!("{}", line()),
        n => eprintln!("{} ({n} more like it since the last line)", line()),
    }
}

/// The /health verdict: `None` when ok, else the first reason sessions cannot work and what to do.
fn health_verdict(state: &AppState) -> Option<(&'static str, &'static str)> {
    if !state.ffmpeg_ok.load(Relaxed) {
        Some((
            "ffmpeg_unavailable",
            "ffmpeg is missing or lacks matroska/mov/hls/aac/https — check FFMPEG_PATH and the image",
        ))
    } else if !state.scratch_ok.load(Relaxed) {
        Some(("scratch_unwritable", "SCRATCH_DIR cannot be written — check the volume and its owner (65532)"))
    } else if state.cfg.scout_install_url.is_none() && state.cfg.scout_origins.is_empty() {
        Some((
            "scout_unconfigured",
            "set SCOUT_ORIGINS and REMUX_SCOUT_KEY (or a fallback SCOUT_INSTALL_URL)",
        ))
    } else if !state.cfg.scout_origins.is_empty() && state.cfg.scout_key.is_none() {
        Some(("scout_key_missing", "set REMUX_SCOUT_KEY, or a scoped scout config refuses to list or play"))
    } else if state.cfg.url_key_ephemeral {
        Some(("url_key_ephemeral", "set REMUX_URL_KEY, or every restart logs the browsers out"))
    } else {
        None
    }
}

fn health_body(state: &AppState) -> serde_json::Value {
    match health_verdict(state) {
        Some((reason, detail)) => {
            serde_json::json!({"status": "degraded", "reason": reason, "detail": detail})
        }
        None => serde_json::json!({"status": "ok"}),
    }
}

/// Only with a configured token presented as `Authorization: Bearer <token>`, compared in constant
/// time — how every Den addon gates it.
fn metrics_authorized(state: &AppState, headers: &hyper::HeaderMap) -> bool {
    use subtle::ConstantTimeEq;
    let Some(token) = state.cfg.metrics_token.as_deref() else { return false };
    let presented = headers
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    presented.is_some_and(|p| p.as_bytes().ct_eq(token.as_bytes()).into())
}

/// Prometheus text, by hand. Every number is an atomic or a map length; nothing walks the disk.
fn metrics_body(state: &AppState) -> String {
    use std::fmt::Write;
    let mut b = String::with_capacity(2048);
    let mut metric = |name: &str, kind: &str, help: &str, labels: &str, v: u64| {
        let _ = writeln!(b, "# HELP {name} {help}\n# TYPE {name} {kind}\n{name}{labels} {v}");
    };
    metric(
        "remux_build_info",
        "gauge",
        "The running build.",
        concat!("{version=\"", env!("CARGO_PKG_VERSION"), "\"}"),
        1,
    );
    let sessions = state.sessions();
    metric("remux_sessions", "gauge", "Sessions playing now.", "", sessions.len() as u64);
    metric(
        "remux_public_sessions",
        "gauge",
        "Sessions created through den-edge's authenticated public origin.",
        "",
        sessions.values().filter(|session| session.public).count() as u64,
    );
    drop(sessions);
    metric("remux_sessions_max", "gauge", "MAX_SESSIONS.", "", state.cfg.max_sessions as u64);
    metric(
        "remux_ffmpeg_running",
        "gauge",
        "ffmpeg processes alive (running or paused).",
        "",
        job::live_groups() as u64,
    );
    metric(
        "remux_scratch_bytes",
        "gauge",
        "Bytes of GOP files on the scratch volume.",
        "",
        state.scratch_bytes.load(Relaxed),
    );
    metric("remux_scratch_max_bytes", "gauge", "SCRATCH_MAX_BYTES.", "", state.cfg.scratch_max_bytes);
    metric(
        "remux_transcodes",
        "gauge",
        "Sessions transcoding on the GPU now.",
        "",
        state.transcodes.load(Relaxed) as u64,
    );
    metric(
        "remux_transcodes_max",
        "gauge",
        "MAX_TRANSCODES, or 0 when transcoding is off.",
        "",
        if state.transcode_ok.load(Relaxed) { state.cfg.max_transcodes as u64 } else { 0 },
    );
    metric(
        "remux_sessions_started_total",
        "counter",
        "Sessions created since start.",
        "",
        state.sessions_started.load(Relaxed),
    );
    metric(
        "remux_ffmpeg_runs_total",
        "counter",
        "ffmpeg runs started since start, restarts included.",
        "",
        state.jobs_started.load(Relaxed),
    );
    b
}

/// The CORS a receiver needs on a session's files: any origin (a Cast receiver's is Google's), and
/// `Range` allowed with `Content-Range` readable. POST is the player's error report.
fn add_cors(resp: &mut Response<Body>) {
    use hyper::header::HeaderValue;
    let h = resp.headers_mut();
    h.insert("access-control-allow-origin", HeaderValue::from_static("*"));
    h.insert("access-control-allow-methods", HeaderValue::from_static("GET, HEAD, POST, DELETE, OPTIONS"));
    h.insert("access-control-allow-headers", HeaderValue::from_static("Range, X-Request-Id"));
    h.insert("access-control-expose-headers", HeaderValue::from_static("Content-Range, Content-Length"));
    h.insert("timing-allow-origin", HeaderValue::from_static("*"));
    h.insert("access-control-max-age", HeaderValue::from_static("86400"));
}

pub async fn handle_request<B>(state: Arc<AppState>, req: Request<B>) -> Response<Body>
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let start = std::time::Instant::now();
    let (parts, body) = req.into_parts();
    let mut resp = route(&state, &parts, body).await;
    // The route sets this private marker only after the session signature verifies. Removing it before the answer
    // leaves malformed and forged paths without CORS, so a public listener cannot be used as a cross-origin oracle.
    if resp.headers_mut().remove("x-den-valid-session").is_some() {
        add_cors(&mut resp);
    } else if matches!(
        parts.uri.path(),
        "/remux/login" | "/remux/session" | "/remux/releases" | "/remux/health" | "/remux/speed"
    ) {
        if let Some(origin) = web_origin(&state, &parts.headers) {
            resp.headers_mut().insert("access-control-allow-origin", origin);
            resp.headers_mut().insert(
                "access-control-expose-headers",
                hyper::header::HeaderValue::from_static(auth::BROWSER_TOKEN_HEADER),
            );
        }
        resp.headers_mut().append("vary", hyper::header::HeaderValue::from_static("origin"));
    }
    if state.cfg.log_requests {
        let mut line = format!(
            "{} {} {} {}ms",
            parts.method,
            redact::path(parts.uri.path()),
            resp.status().as_u16(),
            start.elapsed().as_millis()
        );
        if let Some(rid) = redact::request_id(&parts.headers) {
            line.push_str(" rid=");
            line.push_str(&rid);
        }
        eprintln!("{line}");
    }
    resp
}

/// The request's `Origin` when it is one of `WEB_ORIGINS`.
fn web_origin(state: &AppState, headers: &hyper::HeaderMap) -> Option<hyper::header::HeaderValue> {
    let origin = headers.get(hyper::header::ORIGIN)?;
    let value = origin.to_str().ok()?.to_ascii_lowercase();
    state.cfg.web_origins.contains(&value).then(|| origin.clone())
}

/// A web origin's preflight for login or a session: a JSON POST, remembered for a day. The allow-origin comes
/// from `handle_request`, and only for `WEB_ORIGINS`.
fn preflight() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("access-control-allow-methods", "POST")
        .header("access-control-allow-headers", "content-type, authorization")
        .header("access-control-max-age", "86400")
        .body(httputil::full(""))
        .unwrap()
}

fn rate_limited(detail: &str) -> Response<Body> {
    httputil::json(
        StatusCode::TOO_MANY_REQUESTS,
        &serde_json::json!({"error": "rate_limited", "detail": detail}),
        &[("retry-after", "60")],
    )
}

/// Most bytes `/remux/speed` sends, and what it sends when no count is named.
const SPEED_MAX_BYTES: u64 = 8 * 1024 * 1024;
const SPEED_DEFAULT_BYTES: u64 = 2 * 1024 * 1024;

/// `GET /remux/speed?bytes=<n>`: n random bytes, for a player away from home to time the link it would play over and
/// ask for a session that fits it (`maxBitrate`). It reveals nothing, so it asks for no more than `/health` does; every
/// byte crosses the home upload, so it is counted per visitor with logins and new sessions.
fn speed(parts: &hyper::http::request::Parts) -> Response<Body> {
    let named = parts.uri.query().unwrap_or("").split('&').find_map(|p| p.strip_prefix("bytes="));
    let bytes = match named.map(str::parse::<u64>) {
        None => SPEED_DEFAULT_BYTES,
        Some(Ok(n)) => n.min(SPEED_MAX_BYTES),
        Some(Err(_)) => return bad_request("bytes is a count of bytes."),
    };
    let body = if parts.method == Method::HEAD { httputil::full("") } else { httputil::random_body(bytes) };
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("content-length", bytes)
        .header("cache-control", "no-store")
        .body(body)
        .unwrap()
}

fn method_not_allowed(allow: &str) -> Response<Body> {
    httputil::json(
        StatusCode::METHOD_NOT_ALLOWED,
        &serde_json::json!({"error": "method_not_allowed"}),
        &[("allow", allow)],
    )
}

async fn route<B>(state: &Arc<AppState>, parts: &hyper::http::request::Parts, body: B) -> Response<Body>
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let path = parts.uri.path();
    let get = matches!(parts.method, Method::GET | Method::HEAD);
    match path {
        // `/remux/health` too: under tailscale serve and on the LAN the service is mounted at `/remux`, and clients
        // probe each route's `<url>/health` (den-spec routes-v1).
        "/health" | "/remux/health" if get => httputil::json(StatusCode::OK, &health_body(state), &[]),
        "/metrics" if get => {
            if !metrics_authorized(state, &parts.headers) {
                return httputil::not_found();
            }
            let body = metrics_body(state);
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
                .header("content-length", body.len())
                .header("cache-control", "no-store")
                .body(httputil::full(body))
                .unwrap()
        }
        "/remux/speed" if get && !state.admit(visitor(state, parts)) => {
            rate_limited("Too many speed tests, logins or new sessions from here; wait a minute.")
        }
        "/remux/speed" if get => speed(parts),
        "/health" | "/remux/health" | "/metrics" | "/remux/speed" => method_not_allowed("GET, HEAD"),
        "/remux/login" | "/remux/session" | "/remux/releases" if parts.method == Method::OPTIONS => {
            preflight()
        }
        "/remux/login" | "/remux/session" | "/remux/releases"
            if parts.method == Method::POST && !state.admit(visitor(state, parts)) =>
        {
            rate_limited("Too many logins or new sessions from here; wait a minute.")
        }
        "/remux/login" if parts.method == Method::POST => login(state, body).await,
        "/remux/session" if parts.method == Method::POST => create_session(state, parts, body).await,
        "/remux/releases" if parts.method == Method::POST => list_releases(state, parts, body).await,
        "/remux/admin/kill" if parts.method == Method::POST && edge_authorized(state, &parts.headers) => {
            admin_kill(state, body).await
        }
        "/remux/login" | "/remux/session" | "/remux/releases" => method_not_allowed("POST"),
        _ => match path.strip_prefix("/remux/s/") {
            Some(rest) => session_route(state, parts, rest, body).await,
            None => httputil::not_found(),
        },
    }
}

/// The connection's peer address, which the server puts on every request.
#[derive(Clone, Copy)]
pub struct Peer(pub std::net::IpAddr);

/// Who is asking, for the per-visitor limit: the connection's address, or — through a proxy in
/// `TRUSTED_PROXIES` — the last address its `X-Forwarded-For` names, the one that proxy saw.
pub(crate) fn visitor(state: &AppState, parts: &hyper::http::request::Parts) -> Option<std::net::IpAddr> {
    let peer = parts.extensions.get::<Peer>()?.0;
    if !state.cfg.trusted_proxies.contains(&peer) {
        return Some(peer);
    }
    let forwarded = parts
        .headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .rfind(|s| !s.is_empty());
    Some(forwarded.and_then(|s| s.parse().ok()).unwrap_or(peer))
}

fn public_session_request(state: &AppState, parts: &hyper::http::request::Parts) -> bool {
    parts.headers.get("x-den-public-session").is_some_and(|value| value.as_bytes() == b"1")
        && parts.extensions.get::<Peer>().map(|peer| peer.0) == state.cfg.public_session_proxy
}

/// A small JSON request body; `None` if it is larger than any real request.
async fn read_body<B>(body: B) -> Option<Bytes>
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    // Two install URLs with sealed configs fit well inside this.
    read_body_up_to(body, 16 * 1024).await
}

/// A request body of at most `cap` bytes; `None` past it.
async fn read_body_up_to<B>(body: B, cap: usize) -> Option<Bytes>
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    http_body_util::Limited::new(body, cap).collect().await.ok().map(|c| c.to_bytes())
}

/// A player's report on the session `short` — why it couldn't play it, how playing went — into the log.
async fn report<B>(short: &str, body: B) -> Response<Body>
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let Some(r) = read_body_up_to(body, report::MAX_BODY).await.and_then(|b| report::parse(&b)) else {
        return httputil::error(
            StatusCode::BAD_REQUEST,
            "bad_report",
            "Expected {code, message} or {stats}.",
        );
    };
    if let Some((code, message)) = r.failure() {
        eprintln!(
            "session {short}: the player couldn't play it: error {code} \"{}\"",
            redact::player_message(message)
        );
    }
    if let Some(stats) = &r.stats {
        eprintln!("{}", report::stats_line(short, stats));
    }
    Response::builder().status(StatusCode::NO_CONTENT).body(httputil::full("")).unwrap()
}

fn bad_request(detail: &str) -> Response<Body> {
    httputil::error(StatusCode::BAD_REQUEST, "bad_request", detail)
}

async fn login<B>(state: &AppState, body: B) -> Response<Body>
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    #[derive(Deserialize)]
    struct Login {
        key: String,
    }
    let Some(parsed) = read_body(body).await.and_then(|b| serde_json::from_slice::<Login>(&b).ok()) else {
        return bad_request("Expected {\"key\": \"…\"}.");
    };
    let Some(browser) = auth::browser_for_key(&state.cfg.browser_key_hashes, parsed.key.trim()) else {
        log_limited("login_refused", || {
            "login: refused a key that matches no BROWSER_KEY_HASHES entry".to_string()
        });
        return httputil::error(
            StatusCode::UNAUTHORIZED,
            "bad_key",
            "That key is not one of this server's browsers.",
        );
    };
    let value = auth::cookie_value(&state.cfg.url_key, &browser, unix_now() + auth::COOKIE_TTL_SECS);
    let token = auth::browser_token(&state.cfg.url_key, &browser, unix_now() + auth::BROWSER_TOKEN_TTL_SECS);
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("set-cookie", auth::set_cookie(&value))
        .header(auth::BROWSER_TOKEN_HEADER, token)
        .header("cache-control", "no-store")
        .body(httputil::full(""))
        .unwrap()
}

/// The picture's size as it plays: the release's, or what a conversion brings it down to.
fn played(s: &session::Session) -> (u32, u32) {
    match s.transcoded {
        true => session::transcode_size(s.info.width, s.info.height, s.preset),
        false => (s.info.width, s.info.height),
    }
}

/// The logged-in browser the request's cookie names, if any.
fn browser_of(state: &AppState, parts: &hyper::http::request::Parts) -> Option<String> {
    // An explicit credential takes precedence; a bad token cannot silently fall back to a cookie.
    if let Some(header) = parts.headers.get(hyper::header::AUTHORIZATION) {
        let token = header.to_str().ok()?.strip_prefix("Bearer ")?.trim();
        return auth::token_browser(&state.cfg.url_key, &state.cfg.browser_key_hashes, token, unix_now());
    }
    let cookies: Vec<&str> =
        parts.headers.get_all(hyper::header::COOKIE).iter().filter_map(|v| v.to_str().ok()).collect();
    let cookie = cookies.join("; ");
    auth::cookie_browser(&state.cfg.url_key, &state.cfg.browser_key_hashes, Some(&cookie), unix_now())
}

/// Does the request carry den-edge's `x-den-edge-secret`? False when `EDGE_SECRET` is unset.
fn edge_authorized(state: &AppState, headers: &hyper::HeaderMap) -> bool {
    auth::edge_secret_ok(
        state.cfg.edge_secret.as_deref(),
        headers.get("x-den-edge-secret").map(|v| v.as_bytes()),
    )
}

/// The grant owner den-edge names with `x-den-owner`, honoured only alongside a valid `x-den-edge-secret`.
fn guest_owner(state: &AppState, headers: &hyper::HeaderMap) -> Option<String> {
    if !edge_authorized(state, headers) {
        return None;
    }
    let owner = headers.get("x-den-owner")?.to_str().ok()?;
    auth::is_grant_owner(owner).then(|| owner.to_string())
}

/// Who a request plays as: a vouched-for guest grant naming a scout install, else a logged-in browser, else the
/// scout install it names. `None` when it has neither.
fn admission_of(
    state: &AppState,
    parts: &hyper::http::request::Parts,
    browser: Option<String>,
    scout: &Option<String>,
) -> Option<session::Admission> {
    match (guest_owner(state, &parts.headers), browser, scout) {
        (Some(owner), _, Some(_)) => Some(session::Admission::Guest(owner)),
        (_, Some(b), _) => Some(session::Admission::Browser(b)),
        (_, None, Some(_)) => Some(session::Admission::Install),
        _ => None,
    }
}

/// `POST /remux/admin/kill`, with den-edge's secret: end every session of the grant named, as a client's DELETE
/// would. Answers only to a valid secret, so without one the route does not exist.
async fn admin_kill<B>(state: &Arc<AppState>, body: B) -> Response<Body>
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    #[derive(Deserialize)]
    struct Kill {
        owner: String,
    }
    let Some(req) = read_body(body)
        .await
        .and_then(|b| serde_json::from_slice::<Kill>(&b).ok())
        .filter(|k| auth::is_grant_owner(&k.owner))
    else {
        return bad_request("Expected {\"owner\": \"grant:<8 hex>\"}.");
    };
    let ended = state.end_owner(&req.owner, "grant ended").await;
    eprintln!("admin: ended {ended} session(s) of {}", req.owner);
    httputil::json(StatusCode::OK, &serde_json::json!({ "ended": ended }), &[])
}

/// `POST /remux/releases`: what a session for the title could play, admitted as a session is — by the cookie, or
/// the scout install named.
async fn list_releases<B>(
    state: &Arc<AppState>,
    parts: &hyper::http::request::Parts,
    body: B,
) -> Response<Body>
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Title {
        imdb: String,
        season: Option<u32>,
        episode: Option<u32>,
        scout: Option<String>,
        /// As in `POST /remux/session`: what the player decodes, so each release can say whether it plays.
        #[serde(default)]
        video_codecs: Vec<String>,
        playable: Option<session::Playable>,
    }
    let Some(req) = read_body(body).await.and_then(|b| serde_json::from_slice::<Title>(&b).ok()) else {
        return bad_request(
            "Expected {\"imdb\": \"tt…\", \"season\"?: n, \"episode\"?: n, \"scout\"?: \"…\", \
             \"videoCodecs\"?: [\"h264\", \"hevc\"], \"playable\"?: {…as in /remux/session}}.",
        );
    };
    if req.video_codecs.len() > 8 || req.video_codecs.iter().any(|c| c.len() > 35) {
        return bad_request("videoCodecs is at most 8 short tags.");
    }
    let admission = match admission_of(state, parts, browser_of(state, parts), &req.scout) {
        Some(a) => a,
        None => {
            return httputil::error(
                StatusCode::UNAUTHORIZED,
                "not_logged_in",
                "Name a scout install, or log in with this browser's key.",
            )
        }
    };
    if !scout::is_imdb(&req.imdb) {
        return bad_request("imdb must be an IMDb id, tt followed by digits.");
    }
    let episode = match (req.season, req.episode) {
        (Some(s), Some(e)) => Some((s, e)),
        (None, None) => None,
        _ => return bad_request("An episode needs both season and episode."),
    };
    let id = scout::title_id(&req.imdb, episode);
    let listed = session::releases(
        state,
        admission,
        req.scout.as_deref(),
        &id,
        req.playable.as_ref(),
        &req.video_codecs,
    )
    .await;
    match listed {
        Ok(list) => httputil::json(
            StatusCode::OK,
            &serde_json::json!({
                // `plays`: "yes" as it is, "convert" only converted on the server, "no" not at all; `why` says
                // so where it isn't a plain yes.
                "releases": list.iter().map(|v| serde_json::json!({
                    "label": v.stream.attributes.label,
                    "filename": v.stream.filename(),
                    "size": v.stream.attributes.size_bytes,
                    "plays": v.plays.as_str(),
                    "why": v.why,
                })).collect::<Vec<_>>(),
            }),
            &[],
        ),
        Err(e) => httputil::error(e.status, e.code, &e.detail),
    }
}

async fn create_session<B>(
    state: &Arc<AppState>,
    parts: &hyper::http::request::Parts,
    body: B,
) -> Response<Body>
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let browser = browser_of(state, parts);
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Create {
        imdb: String,
        season: Option<u32>,
        episode: Option<u32>,
        filename: Option<String>,
        scout: Option<String>,
        #[serde(default)]
        audio: Vec<String>,
        audio_track: Option<usize>,
        subtitles: Option<String>,
        #[serde(default)]
        subtitle_languages: Vec<String>,
        #[serde(default)]
        video_codecs: Vec<String>,
        playable: Option<session::Playable>,
        start_at: Option<f64>,
        max_bitrate: Option<u64>,
        /// The page's HLS player, `native` or `hls.js`: for the log, and a native player's session answers only
        /// once its `init.mp4` is ready (`Session::prepare`).
        player: Option<String>,
    }
    let Some(req) = read_body(body).await.and_then(|b| serde_json::from_slice::<Create>(&b).ok()) else {
        return bad_request(
            "Expected {\"imdb\": \"tt…\", \"season\"?: n, \"episode\"?: n, \"filename\"?: \"…\", \"scout\"?: \"…\", \
             \"audio\"?: [\"en\", …], \"audioTrack\"?: n, \"subtitles\"?: \"…\", \"subtitleLanguages\"?: [\"en\", …], \
             \"videoCodecs\"?: [\"h264\", \"hevc\"], \"playable\"?: {\"h264\", \"h264High10\", \"hevcMain\", \"hevcMain10\", \"hevcHighTier\", \"hdr\", \"eac3\", \"aacMultichannel\", \"dolbyVision\": {\"p5\", \"p8\"}, \"av1\", \"av1Main10\", \"av1Hdr\", \"flac\", \"aac71\", \"vp9\", \"vp9Profile2\"}, \
             \"startAt\"?: seconds, \"maxBitrate\"?: bits a second, \"player\"?: \"native\" | \"hls.js\"}.",
        );
    };
    if req.start_at.is_some_and(|t| !t.is_finite() || t < 0.0) {
        return bad_request("startAt is seconds into the title, 0 or more.");
    }
    if req.max_bitrate == Some(0) {
        return bad_request("maxBitrate is bits a second, more than 0.");
    }
    // A logged-in browser, or — with no cookie — whoever holds the scout install the request names: that
    // URL is the credential, as every addon's is. Without either there is nothing to play with.
    let admission = match admission_of(state, parts, browser, &req.scout) {
        Some(a) => a,
        None => {
            return httputil::error(
                StatusCode::UNAUTHORIZED,
                "not_logged_in",
                "Name a scout install, or log in with this browser's key.",
            )
        }
    };
    if !scout::is_imdb(&req.imdb) {
        return bad_request("imdb must be an IMDb id, tt followed by digits.");
    }
    let episode = match (req.season, req.episode) {
        (Some(s), Some(e)) => Some((s, e)),
        (None, None) => None,
        _ => return bad_request("An episode needs both season and episode."),
    };
    let tags_ok = |tags: &[String]| tags.len() <= 8 && tags.iter().all(|l| l.len() <= 35);
    if !tags_ok(&req.audio) || !tags_ok(&req.subtitle_languages) || !tags_ok(&req.video_codecs) {
        return bad_request("audio, subtitleLanguages and videoCodecs are at most 8 short tags each.");
    }
    let id = scout::title_id(&req.imdb, episode);
    // As the browser reported it, for every player. 10-bit VP9 used to be cleared for a native session because
    // nobody had played it in Apple's own player; codec lab has now done so, on an iPhone and on a Mac, and it
    // plays (oxyc/den#37).
    let playable = req.playable;
    let want = session::Want {
        id: &id,
        filename: req.filename.as_deref(),
        scout: req.scout.as_deref(),
        audio: &req.audio,
        audio_track: req.audio_track,
        subtitles: req.subtitles.as_deref(),
        subtitle_languages: &req.subtitle_languages,
        video_codecs: &req.video_codecs,
        playable: playable.as_ref(),
        start_at: req.start_at.unwrap_or(0.0),
        max_bitrate: req.max_bitrate,
        client: client::label(parts.headers.get(hyper::header::USER_AGENT), req.player.as_deref()),
    };
    let public = public_session_request(state, parts);
    let created = session::create(state, admission, &want, public).await;
    // Apple's players give up on an init.mp4 that takes more than about five seconds; hls.js doesn't need this.
    if let (Ok(s), Some("native")) = (&created, req.player.as_deref()) {
        s.prepare(state).await;
    }
    match created {
        Ok(s) => httputil::json(
            StatusCode::CREATED,
            &serde_json::json!({
                // What is playing, which a converted release is not what its name says: the size it came down to,
                // and whether its colours were tone-mapped to SDR.
                "video": {
                    "codec": match (s.transcoded, &s.info.video) {
                        (false, probe::VideoCodec::Hevc) => "hevc",
                        (false, probe::VideoCodec::Av1) => "av1",
                        (false, probe::VideoCodec::Vp9) => "vp9",
                        _ => "h264",
                    },
                    "transcoded": s.transcoded,
                    "width": played(&s).0,
                    "height": played(&s).1,
                    "tonemapped": s.transcoded && session::tonemaps(&s.info),
                },
                "sid": s.sid,
                "playlist": format!("/remux/s/{}/{}/master.m3u8", s.sid, s.sig),
                "speed": format!("/remux/s/{}/{}/speed", s.sid, s.sig),
                // `requested`: the release the player named, when another was played, and why it was passed over.
                "release": {
                    "label": s.release.label,
                    "filename": s.release.filename,
                    "size": s.release.size,
                    "requested": s.release.requested.as_ref().map(|r| serde_json::json!({
                        "filename": r.filename,
                        "why": r.why,
                    })),
                },
                "duration": s.info.duration,
                "expiresAt": s.exp,
                "audioTrack": s.audio,
                // The channels the session's audio carries, beside the track's own in `audioTracks`: fewer is a
                // conversion to stereo.
                "audioChannels": s.audio_channels,
                "audioTracks":s.info.audio.iter().map(|a| serde_json::json!({
                    "language": a.language,
                    "name": a.name,
                    "channels": a.channels,
                    "commentary": a.commentary,
                })).collect::<Vec<_>>(),
                "subtitles": s.renditions.iter().map(|r| serde_json::json!({
                    "language": r.lang,
                    "name": lang::name(&r.lang),
                })).collect::<Vec<_>>(),
            }),
            &[],
        ),
        Err(e) => httputil::error(e.status, e.code, &e.detail),
    }
}

async fn session_route<B>(
    state: &Arc<AppState>,
    parts: &hyper::http::request::Parts,
    rest: &str,
    body: B,
) -> Response<Body>
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let mut it = rest.splitn(3, '/');
    let (sid, sig, file) = (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next());
    if !auth::is_id(sid) || !auth::is_id(sig) {
        return session_not_found();
    }
    let key = &state.cfg.url_key;
    let Some(s) = state.session(sid) else {
        // An ended session answers 410 — but only to someone holding its URL; to anyone else it is as
        // absent as a session that never existed.
        return match state.tombstone(sid) {
            Some(exp) if auth::url_sig_ok(key, sid, exp, sig) => valid_session(session::gone()),
            _ => session_not_found(),
        };
    };
    if !auth::url_sig_ok(key, sid, s.exp, sig) {
        return session_not_found();
    }
    if parts.method == Method::OPTIONS {
        return valid_session(
            Response::builder().status(StatusCode::NO_CONTENT).body(httputil::full("")).unwrap(),
        );
    }
    if unix_now() >= s.exp {
        state.end_session(sid, "expired").await;
        return valid_session(session::gone());
    }
    let head = parts.method == Method::HEAD;
    let response = match (&parts.method, file) {
        (&Method::DELETE, None | Some("")) => {
            state.end_session(sid, "deleted").await;
            Response::builder().status(StatusCode::NO_CONTENT).body(httputil::full("")).unwrap()
        }
        // The player's verdict when it can't play what it was sent — a browser's MediaError, or hls.js's — which
        // no server log sees otherwise. Only the holder of the session's signed URL gets here.
        (&Method::POST, Some("report")) if s.take_report_slot() => report(s.short(), body).await,
        (&Method::POST, Some("report")) => rate_limited("This session already reported its player failures."),
        (&Method::HEAD, Some("speed")) => speed(parts),
        (&Method::GET, Some("speed")) if s.take_speed_slot() => {
            s.touch();
            speed(parts)
        }
        (&Method::GET, Some("speed")) => rate_limited("This session already measured its link."),
        (&Method::GET | &Method::HEAD, Some(f)) => {
            let resp = match f {
                // VOD with an ENDLIST, fixed when the session was made: kept for its life. Idleness is judged by
                // segment and init requests, so a player that doesn't fetch these again still counts as watching.
                "master.m3u8" | "media.m3u8" => {
                    s.touch();
                    let text = if f == "master.m3u8" { s.master.clone() } else { s.media.clone() };
                    httputil::text("application/vnd.apple.mpegurl", &s.cache_control(), text)
                }
                "init.mp4" => s.serve_init(state, head).await,
                _ => match session::sub_file(f).filter(|file| {
                    let n = match file {
                        SubFile::Playlist(n) | SubFile::Document(n) | SubFile::Window(n, _) => *n,
                    };
                    n < s.renditions.len()
                }) {
                    Some(SubFile::Playlist(n)) => {
                        s.touch();
                        httputil::text(
                            "application/vnd.apple.mpegurl",
                            &s.cache_control(),
                            s.subtitle_playlist(state, n).await,
                        )
                    }
                    // A found subtitle is kept for the session; an empty document is not, so a player that asks
                    // again after den-subtitles failed gets another try.
                    Some(SubFile::Document(n)) => {
                        s.touch();
                        match s.subtitle(state, n).await {
                            Some(doc) => httputil::text("text/vtt; charset=utf-8", &s.cache_control(), doc),
                            None => httputil::text("text/vtt; charset=utf-8", "no-store", subs::EMPTY.into()),
                        }
                    }
                    Some(SubFile::Window(n, w)) => {
                        s.touch();
                        s.serve_subtitle_window(state, n, w).await
                    }
                    None => match session::seg_index(f).filter(|n| *n < s.segments.len()) {
                        Some(n) => s.serve_segment(state, n, head).await,
                        None => httputil::not_found(),
                    },
                },
            };
            if head {
                let (p, _) = resp.into_parts();
                return valid_session(Response::from_parts(p, httputil::full("")));
            }
            resp
        }
        _ => method_not_allowed("GET, HEAD, POST, DELETE, OPTIONS"),
    };
    valid_session(response)
}

fn valid_session(mut response: Response<Body>) -> Response<Body> {
    response.headers_mut().insert("x-den-valid-session", hyper::header::HeaderValue::from_static("1"));
    response
}

/// A forged or unknown signed URL is intentionally identical to Caddy's public catch-all: no JSON clue,
/// no CORS, and no method advertisement. Only a verified session path reaches `valid_session`.
fn session_not_found() -> Response<Body> {
    Response::builder().status(StatusCode::NOT_FOUND).body(httputil::full("")).unwrap()
}

async fn run(cfg: Config) -> std::io::Result<()> {
    let state = AppState::new(cfg);
    state::sweep_scratch(&state.cfg.scratch_dir);
    state.scratch_ok.store(state::check_scratch(&state.cfg.scratch_dir), Relaxed);
    state.load_unplayable();
    state.ffmpeg_ok.store(state::check_ffmpeg(&state.cfg.ffmpeg).await, Relaxed);
    let transcode = state.cfg.max_transcodes > 0
        && state::check_transcode(&state.cfg.ffmpeg, &state.cfg.vaapi_device).await;
    state.transcode_ok.store(transcode, Relaxed);
    if let Some((reason, detail)) = health_verdict(&state) {
        eprintln!("health: degraded ({reason}) — {detail}");
    }

    // Registered before the listener binds: until the handlers exist SIGTERM keeps its default
    // disposition, and a stop in that window would kill the process outright (den-reel's lesson).
    let shutdown = shutdown_signal();
    let listener = TcpListener::bind(("0.0.0.0", state.cfg.port)).await?;
    let on = |b: bool| if b { "on" } else { "off" };
    eprintln!(
        "den-remux {} listening on :{} — metrics={} log_requests={} scout_origins={} scout_key={} scout_install={} \
         browser_keys={} url_key={} trusted_proxies={} public_session_proxy={} \
         max_sessions={} per_install={} idle={}s scratch={} scratch_max={} ffmpeg={} transcode={}",
        env!("CARGO_PKG_VERSION"),
        state.cfg.port,
        on(state.cfg.metrics_token.is_some()),
        on(state.cfg.log_requests),
        state.cfg.scout_origins.len(),
        on(state.cfg.scout_key.is_some()),
        on(state.cfg.scout_install_url.is_some()),
        state.cfg.browser_key_hashes.len(),
        if state.cfg.url_key_ephemeral { "ephemeral" } else { "set" },
        state.cfg.trusted_proxies.len(),
        on(state.cfg.public_session_proxy.is_some()),
        state.cfg.max_sessions,
        state.cfg.max_sessions_per_install,
        state.cfg.session_idle.as_secs(),
        state.cfg.scratch_dir.display(),
        state.cfg.scratch_max_bytes,
        if state.ffmpeg_ok.load(Relaxed) { "ok" } else { "missing" },
        if transcode { format!("vaapi(max {})", state.cfg.max_transcodes) } else { "off".to_string() },
    );

    let drained = serve_until(listener, state.clone(), shutdown, DRAIN_GRACE, HEADER_READ_TIMEOUT).await;

    // Ending each session kills and reaps its ffmpeg and deletes its scratch; the registry catches
    // anything that slipped between them.
    state.end_all("shutdown").await;
    let killed = job::kill_live_groups();
    if killed > 0 {
        eprintln!("shutdown: killed {killed} ffmpeg process group(s)");
    }
    if drained {
        eprintln!("shut down cleanly");
    }
    Ok(())
}

/// How long in-flight requests get to finish after SIGTERM: under podman's default 10 s stop timeout.
const DRAIN_GRACE: Duration = Duration::from_secs(8);

/// How long a client may take to send a request head.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Serve until `shutdown` resolves, then let in-flight requests finish for at most `grace`; `true`
/// when they all did.
async fn serve_until(
    listener: TcpListener,
    state: Arc<AppState>,
    shutdown: impl Future<Output = ()>,
    grace: Duration,
    header_timeout: Duration,
) -> bool {
    let graceful = GracefulShutdown::new();
    tokio::pin!(shutdown);
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                Err(e) => {
                    log_limited("accept", || format!("accept: {e}"));
                    // The listener stays readable while the process is out of descriptors; back off
                    // rather than spin the one runtime thread.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            _ = &mut shutdown => break,
        };
        let state = state.clone();
        let peer = Peer(peer.ip());
        let service = service_fn(move |mut req: Request<hyper::body::Incoming>| {
            req.extensions_mut().insert(peer);
            let state = state.clone();
            async move { Ok::<_, Infallible>(handle_request(state, req).await) }
        });
        let conn = hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(header_timeout)
            .serve_connection(TokioIo::new(stream), service);
        let conn = graceful.watch(conn);
        tokio::spawn(async move {
            let _ = conn.await;
        });
    }
    drop(listener);
    tokio::select! {
        _ = graceful.shutdown() => true,
        _ = tokio::time::sleep(grace) => {
            eprintln!("drain deadline ({grace:?}) reached with requests still in flight");
            false
        }
    }
}

/// Resolves on SIGTERM (a redeploy) or SIGINT (a terminal); a second signal exits at once.
fn shutdown_signal() -> impl Future<Output = ()> {
    use tokio::signal::unix::{signal, SignalKind};
    let term = signal(SignalKind::terminate());
    let int = signal(SignalKind::interrupt());
    async move {
        tokio::select! {
            _ = wait_for(term, "SIGTERM") => {}
            _ = wait_for(int, "SIGINT") => {}
        }
        tokio::spawn(async move {
            tokio::select! {
                _ = quietly(signal(SignalKind::terminate())) => {}
                _ = quietly(signal(SignalKind::interrupt())) => {}
            }
            eprintln!("second signal — exiting without finishing the drain");
            job::kill_live_groups();
            std::process::exit(0);
        });
    }
}

async fn wait_for(registered: std::io::Result<tokio::signal::unix::Signal>, name: &str) {
    match registered {
        Ok(mut sig) => {
            sig.recv().await;
            eprintln!("{name} — draining in-flight requests");
        }
        Err(e) => {
            eprintln!("{name} handler unavailable ({e}); it will be a hard kill");
            std::future::pending::<()>().await
        }
    }
}

async fn quietly(registered: std::io::Result<tokio::signal::unix::Signal>) {
    match registered {
        Ok(mut sig) => {
            sig.recv().await;
        }
        Err(_) => std::future::pending::<()>().await,
    }
}

fn main() {
    let cfg = Config::from_env();
    // current_thread: one runtime thread keeps idle RAM low; the heavy lifting is in ffmpeg.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    if let Err(e) = rt.block_on(run(cfg)) {
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
}
