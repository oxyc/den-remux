//! A loopback door to each session's release, for its own ffmpeg.
//!
//! ffmpeg reading a debrid link itself opens a fresh TCP and TLS connection for every seek, and a Matroska start
//! seeks three times before its first GOP: to the Cues at the end of the file, back to the first cluster to learn
//! the streams, and on to the keyframe it starts at. Four connections one after another, each a TCP and a TLS
//! handshake before its request, were most of a session's start on a long path. Through this door ffmpeg reads the
//! file's head from the bytes the probe already holds, and the rest over den-remux's own pooled connections to the
//! host, warm from the probe: a round trip a seek. It also keeps the debrid link off ffmpeg's command line.
//!
//! The door listens on 127.0.0.1 only, and a session's path is a random token that never leaves this process.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};

use crate::httputil::Body;

/// The first upstream request of a read: small, because ffmpeg often reads a few kilobytes and seeks away — the Cues,
/// the first cluster — and a request it abandons is a connection lost unless what is left of it is read out.
const FIRST_CHUNK: u64 = 1024 * 1024;
/// Later requests of one uninterrupted read grow as it proves sequential. This keeps speculative seeks cheap, while a
/// long pull amortises a remote round trip over enough bytes for a high-bandwidth, high-latency path.
const CHUNKS: [u64; 3] = [16 * 1024 * 1024, 64 * 1024 * 1024, 128 * 1024 * 1024];
/// How much of an abandoned request is still read out, to hand its connection back to the pool rather than close it.
const DRAIN_MAX: u64 = 2 * 1024 * 1024;
/// Frames held between the upstream reader and ffmpeg: ffmpeg paused is backpressure on the upstream, not memory.
const QUEUE: usize = 8;

#[derive(Default)]
pub struct Stats {
    pub requested: AtomicU64,
    pub consumed: AtomicU64,
    pub abandoned: AtomicU64,
}

struct Pull {
    rx: tokio::sync::mpsc::Receiver<std::io::Result<Bytes>>,
    requested: Arc<AtomicU64>,
    consumed: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
    accounted: Arc<AtomicU64>,
    stats: Arc<Stats>,
}

impl Pull {
    fn account_abandoned(&self) {
        account_abandoned(&self.requested, &self.consumed, &self.failed, &self.accounted, &self.stats);
    }
}

impl Drop for Pull {
    fn drop(&mut self) {
        self.account_abandoned();
    }
}

fn account_abandoned(
    requested: &AtomicU64,
    consumed: &AtomicU64,
    failed: &AtomicU64,
    accounted: &AtomicU64,
    stats: &Stats,
) {
    let outstanding =
        requested.load(Relaxed).saturating_sub(consumed.load(Relaxed)).saturating_sub(failed.load(Relaxed));
    let before = accounted.fetch_max(outstanding, Relaxed);
    stats.abandoned.fetch_add(outstanding.saturating_sub(before), Relaxed);
}

/// What a session's door opens on.
struct Entry {
    client: reqwest::Client,
    /// The debrid link, replaced when the session fetches a fresh one.
    upstream: Mutex<String>,
    head: Bytes,
    size: u64,
    stats: Arc<Stats>,
}

/// Every open door, and the loopback port they are served on (bound on first use; `None` where it couldn't be).
#[derive(Default)]
pub struct Doors {
    entries: Arc<Mutex<HashMap<String, Arc<Entry>>>>,
    port: OnceLock<Option<u16>>,
    stats: Arc<Stats>,
}

/// One session's door; closed when dropped.
pub struct Door {
    token: String,
    entry: Arc<Entry>,
    entries: Arc<Mutex<HashMap<String, Arc<Entry>>>>,
}

impl Doors {
    /// A door on `upstream`, `size` bytes long, whose first `head.len()` bytes are `head`.
    pub fn open(&self, client: reqwest::Client, upstream: &str, head: &[u8], size: u64) -> Door {
        let token = crate::auth::random_id();
        let entry = Arc::new(Entry {
            client,
            upstream: Mutex::new(upstream.to_string()),
            head: Bytes::copy_from_slice(&head[..head.len().min(size as usize)]),
            size,
            stats: self.stats.clone(),
        });
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).insert(token.clone(), entry.clone());
        Door { token, entry, entries: self.entries.clone() }
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// The door's URL for ffmpeg, or `None` where no loopback listener could be had, and ffmpeg reads the link itself.
    pub fn url(&self, door: &Door) -> Option<String> {
        let port = (*self.port.get_or_init(|| listen(self.entries.clone())))?;
        Some(format!("http://127.0.0.1:{port}/{}", door.token))
    }
}

impl Door {
    /// Read from `upstream` from now on: a fresh debrid link for a session whose old one stopped working.
    pub fn set_upstream(&self, upstream: &str) {
        *self.entry.upstream.lock().unwrap_or_else(|e| e.into_inner()) = upstream.to_string();
    }
}

impl Drop for Door {
    fn drop(&mut self) {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.token);
    }
}

/// Bind the loopback listener and serve it on the current runtime; `None` outside one, or where binding fails.
fn listen(entries: Arc<Mutex<HashMap<String, Arc<Entry>>>>) -> Option<u16> {
    tokio::runtime::Handle::try_current().ok()?;
    let bound = std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.set_nonblocking(true).map(|_| l))
        .and_then(|l| Ok((l.local_addr()?.port(), tokio::net::TcpListener::from_std(l)?)));
    let (port, listener) = match bound {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("source: no loopback listener, ffmpeg reads each link itself: {e}");
            return None;
        }
    };
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            let entries = entries.clone();
            let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                let entry = req
                    .uri()
                    .path()
                    .strip_prefix('/')
                    .and_then(|t| entries.lock().unwrap_or_else(|e| e.into_inner()).get(t).cloned());
                async move { Ok::<_, Infallible>(answer(entry, &req)) }
            });
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .serve_connection(TokioIo::new(stream), service);
            tokio::spawn(async move {
                let _ = conn.await;
            });
        }
    });
    Some(port)
}

/// The first and last byte a `Range` header asks of `size` bytes: `bytes=a-` or `bytes=a-b` (one range; ffmpeg asks
/// no other kind). `Ok(None)` for no header, the whole file; `Err` for one that can't be served.
fn range(header: Option<&hyper::header::HeaderValue>, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(v) = header else { return Ok(None) };
    let spec = v.to_str().map_err(|_| ())?.strip_prefix("bytes=").ok_or(())?;
    let (a, b) = spec.split_once('-').ok_or(())?;
    let start: u64 = a.trim().parse().map_err(|_| ())?;
    let end = match b.trim() {
        "" => size.saturating_sub(1),
        b => b.parse::<u64>().map_err(|_| ())?.min(size.saturating_sub(1)),
    };
    if start >= size || end < start {
        return Err(());
    }
    Ok(Some((start, end)))
}

fn answer(entry: Option<Arc<Entry>>, req: &Request<hyper::body::Incoming>) -> Response<Body> {
    let Some(entry) = entry else { return crate::httputil::not_found() };
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return crate::httputil::error(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", "GET or HEAD.");
    }
    let size = entry.size;
    let (status, start, end) = match range(req.headers().get(hyper::header::RANGE), size) {
        Ok(Some((a, b))) => (StatusCode::PARTIAL_CONTENT, a, b),
        Ok(None) => (StatusCode::OK, 0, size.saturating_sub(1)),
        Err(()) => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("content-range", format!("bytes */{size}"))
                .body(crate::httputil::full(""))
                .unwrap()
        }
    };
    let mut resp = Response::builder()
        .status(status)
        .header("accept-ranges", "bytes")
        .header("content-type", "application/octet-stream")
        .header("content-length", end - start + 1);
    if status == StatusCode::PARTIAL_CONTENT {
        resp = resp.header("content-range", format!("bytes {start}-{end}/{size}"));
    }
    let body =
        if *req.method() == Method::HEAD { crate::httputil::full("") } else { bytes(entry, start, end) };
    resp.body(body).unwrap()
}

/// Bytes `start..=end` of the entry: what the head holds from memory, the rest from upstream, fetched only once the
/// reader gets past the head.
fn bytes(entry: Arc<Entry>, start: u64, end: u64) -> Body {
    enum State {
        Head(Arc<Entry>, u64, u64),
        Upstream(Pull),
        Done,
    }
    let held = entry.head.len() as u64;
    let first = match start < held {
        true => State::Head(entry, start, end),
        false => State::Upstream(fetch(entry, start, end, &CHUNKS)),
    };
    let stream = futures_util::stream::unfold(first, |state| async move {
        match state {
            State::Head(entry, start, end) => {
                let held = entry.head.len() as u64;
                let upto = end.min(held - 1);
                let part = entry.head.slice(start as usize..=upto as usize);
                let next = match upto < end {
                    true => State::Upstream(fetch(entry, upto + 1, end, &CHUNKS)),
                    false => State::Done,
                };
                Some((Ok(Frame::data(part)), next))
            }
            State::Upstream(mut pull) => {
                let item = pull.rx.recv().await?;
                if let Ok(bytes) = &item {
                    let n = bytes.len() as u64;
                    pull.consumed.fetch_add(n, Relaxed);
                    pull.stats.consumed.fetch_add(n, Relaxed);
                }
                Some((item.map(Frame::data), State::Upstream(pull)))
            }
            State::Done => None,
        }
    });
    BodyExt::boxed(StreamBody::new(stream))
}

/// Read `start..=end` from upstream on its own task, a bounded request at a time, into a short queue. One request after
/// another on the same connection: a request sent ahead needs a second connection, and on a long path a connection's
/// handshake costs more than the round trip it would save (measured: the first GOP came 0.3 s later).
fn fetch(entry: Arc<Entry>, start: u64, end: u64, chunks: &'static [u64]) -> Pull {
    let (tx, rx) = tokio::sync::mpsc::channel(QUEUE);
    let requested = Arc::new(AtomicU64::new(0));
    let consumed = Arc::new(AtomicU64::new(0));
    let failed_bytes = Arc::new(AtomicU64::new(0));
    let accounted = Arc::new(AtomicU64::new(0));
    let requested_by_task = requested.clone();
    let read = consumed.clone();
    let failed_by_task = failed_bytes.clone();
    let accounted_by_task = accounted.clone();
    let stats = entry.stats.clone();
    let task_stats = stats.clone();
    tokio::spawn(async move {
        let mut at = start;
        let mut chunk = 0usize;
        let mut to = end.min(at + FIRST_CHUNK - 1);
        while at <= end {
            let span = to - at + 1;
            requested_by_task.fetch_add(span, Relaxed);
            entry.stats.requested.fetch_add(span, Relaxed);
            let url = entry.upstream.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let sent =
                entry.client.get(url).header(reqwest::header::RANGE, format!("bytes={at}-{to}")).send().await;
            let mut resp = match sent {
                Ok(r) if r.status() == reqwest::StatusCode::PARTIAL_CONTENT => r,
                Ok(r) => {
                    let why = format!("upstream answered {} for bytes {at}-{to}", r.status().as_u16());
                    failed_by_task.fetch_add(span, Relaxed);
                    failed(&tx, why).await;
                    return;
                }
                Err(e) => {
                    failed_by_task.fetch_add(span, Relaxed);
                    failed(&tx, format!("upstream bytes {at}-{to}: {}", e.without_url())).await;
                    return;
                }
            };
            let expected = (at, to, entry.size);
            let actual = resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(crate::httputil::content_range);
            let content_length = resp.content_length();
            if actual != Some(expected) || content_length != Some(span) {
                let why = format!(
                    "upstream mismatched bytes {at}-{to}: Content-Range={actual:?} Content-Length={content_length:?}"
                );
                failed_by_task.fetch_add(span, Relaxed);
                failed(&tx, why).await;
                return;
            }
            let mut left = span;
            loop {
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        let n = chunk.len() as u64;
                        if n > left {
                            failed_by_task.fetch_add(left, Relaxed);
                            failed(&tx, format!("upstream sent {} extra bytes for {at}-{to}", n - left))
                                .await;
                            return;
                        }
                        left -= n;
                        at += n;
                        if tx.send(Ok(chunk)).await.is_err() {
                            account_abandoned(
                                &requested_by_task,
                                &read,
                                &failed_by_task,
                                &accounted_by_task,
                                &task_stats,
                            );
                            // ffmpeg went elsewhere. Read out a short remainder so the connection goes back warm.
                            if left <= DRAIN_MAX {
                                while let Ok(Some(_)) = resp.chunk().await {}
                            }
                            return;
                        }
                        if left == 0 {
                            match resp.chunk().await {
                                Ok(None) => break,
                                Ok(Some(extra)) => {
                                    failed(
                                        &tx,
                                        format!("upstream sent {} extra bytes after {to}", extra.len()),
                                    )
                                    .await;
                                    return;
                                }
                                Err(e) => {
                                    failed(&tx, format!("upstream body after {to}: {}", e.without_url()))
                                        .await;
                                    return;
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        failed_by_task.fetch_add(left, Relaxed);
                        failed(&tx, format!("upstream ended {left} bytes short of {to}")).await;
                        return;
                    }
                    Err(e) => {
                        let why = format!("upstream ended {left} bytes short of {to}: {}", e.without_url());
                        failed_by_task.fetch_add(left, Relaxed);
                        failed(&tx, why).await;
                        return;
                    }
                }
            }
            let span = chunks[chunk.min(chunks.len() - 1)];
            chunk += 1;
            to = end.min(at.saturating_add(span).saturating_sub(1));
        }
    });
    Pull { rx, requested, consumed, failed: failed_bytes, accounted, stats }
}

/// A read from upstream that failed: logged — the service's only record of it, ffmpeg seeing just a cut body — and
/// handed to the reader. The reason names bytes and a status, never the link.
async fn failed(tx: &tokio::sync::mpsc::Sender<std::io::Result<Bytes>>, why: String) {
    crate::log_limited("source_upstream", || format!("source: {why}"));
    let _ = tx.send(Err(std::io::Error::other(why))).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    /// A file of `len` bytes served with ranges at `/a` and `/b`, counting the requests to each.
    async fn upstream(len: usize) -> (String, Arc<Vec<u8>>, Arc<[AtomicUsize; 2]>) {
        let data: Arc<Vec<u8>> = Arc::new((0..len).map(|i| (i * 7 % 251) as u8).collect());
        let counts = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (d, c) = (data.clone(), counts.clone());
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (d, c) = (d.clone(), c.clone());
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let (d, c) = (d.clone(), c.clone());
                    async move {
                        c[usize::from(req.uri().path() == "/b")].fetch_add(1, Relaxed);
                        let (a, b) =
                            range(req.headers().get(hyper::header::RANGE), d.len() as u64).unwrap().unwrap();
                        let body = Bytes::copy_from_slice(&d[a as usize..=b as usize]);
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(206)
                                .header("content-range", format!("bytes {a}-{b}/{}", d.len()))
                                .body(http_body_util::Full::new(body))
                                .unwrap(),
                        )
                    }
                });
                tokio::spawn(
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service),
                );
            }
        });
        (base, data, counts)
    }

    /// A zero-filled ranged file with one controlled RTT before each response and bounded 64 KiB frames.
    async fn delayed_upstream(size: u64, delay: Duration) -> (String, Arc<AtomicUsize>) {
        let requests = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let counted = requests.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let counted = counted.clone();
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let counted = counted.clone();
                    async move {
                        counted.fetch_add(1, Relaxed);
                        let (start, end) =
                            range(req.headers().get(hyper::header::RANGE), size).unwrap().unwrap();
                        tokio::time::sleep(delay).await;
                        let stream = futures_util::stream::unfold(end - start + 1, |left| async move {
                            if left == 0 {
                                return None;
                            }
                            let n = left.min(64 * 1024);
                            let frame = Frame::data(Bytes::from(vec![0; n as usize]));
                            Some((Ok::<_, std::io::Error>(frame), left - n))
                        });
                        let body = BodyExt::boxed(StreamBody::new(stream));
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(StatusCode::PARTIAL_CONTENT)
                                .header("content-range", format!("bytes {start}-{end}/{size}"))
                                .header("content-length", end - start + 1)
                                .body(body)
                                .unwrap(),
                        )
                    }
                });
                tokio::spawn(
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service),
                );
            }
        });
        (base, requests)
    }

    /// One response to the adaptive reader's fixed bytes 100-199/1000 request.
    async fn range_response(
        content_range: Option<&str>,
        content_length: Option<u64>,
        body_len: usize,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let content_range = content_range.map(str::to_string);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let service = service_fn(move |_req: Request<hyper::body::Incoming>| {
                let content_range = content_range.clone();
                async move {
                    let frames = futures_util::stream::once(async move {
                        Ok::<_, std::io::Error>(Frame::data(Bytes::from(vec![7; body_len])))
                    });
                    let mut response = Response::builder().status(StatusCode::PARTIAL_CONTENT);
                    if let Some(value) = content_range {
                        response = response.header("content-range", value);
                    }
                    if let Some(value) = content_length {
                        response = response.header("content-length", value);
                    }
                    Ok::<_, Infallible>(response.body(BodyExt::boxed(StreamBody::new(frames))).unwrap())
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
        url
    }

    async fn fetch_one(
        content_range: Option<&str>,
        content_length: Option<u64>,
        body_len: usize,
    ) -> Result<Vec<u8>, ()> {
        let url = range_response(content_range, content_length, body_len).await;
        let entry = Arc::new(Entry {
            client: reqwest::Client::new(),
            upstream: Mutex::new(url),
            head: Bytes::new(),
            size: 1000,
            stats: Arc::new(Stats::default()),
        });
        let mut pull = fetch(entry, 100, 199, &CHUNKS);
        let mut out = Vec::new();
        while let Some(item) = pull.rx.recv().await {
            match item {
                Ok(bytes) => out.extend_from_slice(&bytes),
                Err(_) => return Err(()),
            }
        }
        Ok(out)
    }

    #[tokio::test]
    async fn an_adaptive_range_requires_exact_response_headers_and_body() {
        assert_eq!(fetch_one(Some("bytes 100-199/1000"), Some(100), 100).await.unwrap(), vec![7; 100]);
        for (name, content_range, content_length, body_len) in [
            ("wrong start", Some("bytes 101-199/1000"), Some(99), 99),
            ("wrong end", Some("bytes 100-198/1000"), Some(99), 99),
            ("wrong total", Some("bytes 100-199/999"), Some(100), 100),
            ("malformed", Some("not a range"), Some(100), 100),
            ("missing range", None, Some(100), 100),
            ("wrong length", Some("bytes 100-199/1000"), Some(99), 99),
            ("missing length", Some("bytes 100-199/1000"), None, 100),
            ("short body", Some("bytes 100-199/1000"), Some(100), 99),
        ] {
            assert!(fetch_one(content_range, content_length, body_len).await.is_err(), "{name}");
        }
    }

    #[tokio::test]
    async fn the_door_answers_ranges_byte_for_byte_and_the_head_from_memory() {
        let (base, data, counts) = upstream(3 * 1024 * 1024 + 17).await;
        let doors = Doors::default();
        let client = reqwest::Client::new();
        let door = doors.open(client.clone(), &format!("{base}/a"), &data[..256 * 1024], data.len() as u64);
        let url = doors.url(&door).expect("a loopback listener");
        let get = |range: Option<&str>| {
            let mut req = client.get(&url);
            if let Some(r) = range {
                req = req.header("range", r);
            }
            async move {
                let resp = req.send().await.unwrap();
                (resp.status().as_u16(), resp.bytes().await.unwrap().to_vec())
            }
        };
        assert_eq!(get(Some("bytes=1000-2000")).await, (206, data[1000..=2000].to_vec()));
        assert_eq!(get(Some("bytes=0-262143")).await, (206, data[..262_144].to_vec()));
        assert_eq!(counts[0].load(Relaxed), 0, "the head came from memory");
        let across = get(Some("bytes=200000-")).await;
        assert_eq!((across.0, across.1.len()), (206, data.len() - 200_000));
        assert!(across.1 == data[200_000..], "across the head and on upstream, in bounded requests");
        assert!(counts[0].load(Relaxed) >= 2, "a first small request, then larger ones");
        assert_eq!(get(Some("bytes=3000000-3000100")).await, (206, data[3_000_000..=3_000_100].to_vec()));
        assert_eq!(get(None).await, (200, data.to_vec()));
        assert_eq!(get(Some(&format!("bytes={}-", data.len()))).await.0, 416);
        door.set_upstream(&format!("{base}/b"));
        assert_eq!(get(Some("bytes=2000000-2000010")).await, (206, data[2_000_000..=2_000_010].to_vec()));
        assert_eq!(counts[1].load(Relaxed), 1, "a fresh link is read from then on");
        assert_eq!(doors.stats().requested.load(Relaxed), doors.stats().consumed.load(Relaxed));
        assert_eq!(doors.stats().abandoned.load(Relaxed), 0);
        drop(door);
        assert_eq!(client.get(&url).send().await.unwrap().status().as_u16(), 404, "closed with its session");
    }

    /// ffmpeg paused while the player is far ahead reads nothing for minutes, mid-request. A client with a total timeout
    /// cuts that request off once the pause outlasts it; the source client times only reads being made, and carries on.
    #[tokio::test]
    async fn a_read_paused_longer_than_any_total_timeout_carries_on() {
        let (base, data, _) = upstream(8 * 1024 * 1024).await;
        let read_through = |upstream_client: reqwest::Client| {
            let (base, data) = (base.clone(), data.clone());
            async move {
                let doors = Doors::default();
                let door =
                    doors.open(upstream_client, &format!("{base}/a"), &data[..1024], data.len() as u64);
                let url = doors.url(&door).unwrap();
                let mut resp = reqwest::get(&url).await.unwrap();
                let mut got = resp.chunk().await.unwrap().unwrap().to_vec();
                // Paused: nothing read for a second, the door's queue full and its upstream request left waiting.
                tokio::time::sleep(Duration::from_millis(1_000)).await;
                loop {
                    match resp.chunk().await {
                        Ok(Some(c)) => got.extend_from_slice(&c),
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }
                got.len()
            }
        };
        let total = reqwest::Client::builder().timeout(Duration::from_millis(300)).build().unwrap();
        assert!(
            read_through(total).await < data.len(),
            "the premise: a total timeout cuts the paused read off"
        );
        let source = crate::state::source_client(Duration::from_secs(5), Duration::from_millis(300));
        assert_eq!(read_through(source).await, data.len(), "read-idle only: the pause doesn't count");
    }

    #[test]
    fn a_range_is_one_span_inside_the_file() {
        let h = |s: &str| Some(hyper::header::HeaderValue::from_str(s).unwrap());
        assert_eq!(range(None, 100), Ok(None));
        assert_eq!(range(h("bytes=0-").as_ref(), 100), Ok(Some((0, 99))));
        assert_eq!(range(h("bytes=10-19").as_ref(), 100), Ok(Some((10, 19))));
        assert_eq!(range(h("bytes=90-500").as_ref(), 100), Ok(Some((90, 99))), "clamped to the file");
        for bad in ["bytes=100-", "bytes=20-10", "bytes=-5", "items=0-1", "bytes=a-"] {
            assert_eq!(range(h(bad).as_ref(), 100), Err(()), "{bad}");
        }
    }

    #[test]
    fn growing_ranges_remove_the_high_bdp_round_trip_ceiling() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let requests = |bytes: u64, growing: bool| {
            let mut left = bytes;
            let mut request = 0usize;
            while left > 0 {
                let span = match (request, growing) {
                    (0, _) => FIRST_CHUNK,
                    (_, false) => CHUNKS[0],
                    (n, true) => CHUNKS[(n - 1).min(CHUNKS.len() - 1)],
                };
                left = left.saturating_sub(span);
                request += 1;
            }
            request as f64
        };
        // Controlled model: a 2 GiB sequential pull on a 10 Gbit/s, 100 ms path. Requests are serial by design, so
        // elapsed time is wire time plus one RTT per range. This isolates the range policy from TLS and server noise.
        let bytes = 2 * GIB;
        let wire = bytes as f64 * 8.0 / 10e9;
        let fixed = wire + requests(bytes, false) * 0.1;
        let adaptive = wire + requests(bytes, true) * 0.1;
        assert!(fixed / adaptive > 4.0, "fixed={fixed:.2}s adaptive={adaptive:.2}s");
        assert!(requests(bytes, true) < requests(bytes, false) / 4.0);
    }

    #[tokio::test]
    async fn growing_ranges_are_over_twice_as_fast_on_a_controlled_high_bdp_path() {
        const SIZE: u64 = 512 * 1024 * 1024;
        const FIXED: [u64; 1] = [16 * 1024 * 1024];
        let (url, requests) = delayed_upstream(SIZE, Duration::from_millis(50)).await;
        let run = |chunks: &'static [u64]| {
            let entry = Arc::new(Entry {
                client: reqwest::Client::new(),
                upstream: Mutex::new(url.clone()),
                head: Bytes::new(),
                size: SIZE,
                stats: Arc::new(Stats::default()),
            });
            async move {
                let began = std::time::Instant::now();
                let mut pull = fetch(entry, 0, SIZE - 1, chunks);
                let mut bytes = 0u64;
                while let Some(part) = pull.rx.recv().await {
                    let part = part.unwrap();
                    bytes += part.len() as u64;
                    pull.consumed.fetch_add(part.len() as u64, Relaxed);
                }
                assert_eq!(bytes, SIZE);
                began.elapsed()
            }
        };
        let fixed = run(&FIXED).await;
        let fixed_requests = requests.swap(0, Relaxed);
        let adaptive = run(&CHUNKS).await;
        let adaptive_requests = requests.load(Relaxed);
        eprintln!(
            "controlled range pull: fixed={fixed:?}/{fixed_requests} requests adaptive={adaptive:?}/{adaptive_requests} requests"
        );
        assert!(fixed > adaptive * 2, "fixed={fixed:?} adaptive={adaptive:?}");
        assert!(
            fixed_requests > adaptive_requests * 4,
            "fixed={fixed_requests} adaptive={adaptive_requests}"
        );
    }

    #[tokio::test]
    async fn an_abandoned_range_is_counted_without_buffering_the_rest() {
        let (base, data, _) = upstream(20 * 1024 * 1024).await;
        let doors = Doors::default();
        let client = reqwest::Client::new();
        let door = doors.open(client.clone(), &format!("{base}/a"), &[], data.len() as u64);
        let url = doors.url(&door).unwrap();
        let mut response = client.get(url).send().await.unwrap();
        assert!(response.chunk().await.unwrap().is_some());
        drop(response);
        for _ in 0..100 {
            if doors.stats().abandoned.load(Relaxed) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let requested = doors.stats().requested.load(Relaxed);
        let consumed = doors.stats().consumed.load(Relaxed);
        let abandoned = doors.stats().abandoned.load(Relaxed);
        assert!(requested >= FIRST_CHUNK);
        assert!(consumed < requested, "the entire request was buffered after its reader left");
        assert!(abandoned > 0 && abandoned <= requested - consumed);
    }
}
