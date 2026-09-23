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
/// Every later request of the same read: long enough that a request's round trip costs nothing against its transfer.
const CHUNK: u64 = 16 * 1024 * 1024;
/// How much of an abandoned request is still read out, to hand its connection back to the pool rather than close it.
const DRAIN_MAX: u64 = 2 * 1024 * 1024;
/// Frames held between the upstream reader and ffmpeg: ffmpeg paused is backpressure on the upstream, not memory.
const QUEUE: usize = 8;

/// What a session's door opens on.
struct Entry {
    client: reqwest::Client,
    /// The debrid link, replaced when the session fetches a fresh one.
    upstream: Mutex<String>,
    head: Bytes,
    size: u64,
}

/// Every open door, and the loopback port they are served on (bound on first use; `None` where it couldn't be).
#[derive(Default)]
pub struct Doors {
    entries: Arc<Mutex<HashMap<String, Arc<Entry>>>>,
    port: OnceLock<Option<u16>>,
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
        });
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).insert(token.clone(), entry.clone());
        Door { token, entry, entries: self.entries.clone() }
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
        Upstream(tokio::sync::mpsc::Receiver<std::io::Result<Bytes>>),
        Done,
    }
    let held = entry.head.len() as u64;
    let first = match start < held {
        true => State::Head(entry, start, end),
        false => State::Upstream(fetch(entry, start, end)),
    };
    let stream = futures_util::stream::unfold(first, |state| async move {
        match state {
            State::Head(entry, start, end) => {
                let held = entry.head.len() as u64;
                let upto = end.min(held - 1);
                let part = entry.head.slice(start as usize..=upto as usize);
                let next = match upto < end {
                    true => State::Upstream(fetch(entry, upto + 1, end)),
                    false => State::Done,
                };
                Some((Ok(Frame::data(part)), next))
            }
            State::Upstream(mut rx) => {
                let item = rx.recv().await?;
                Some((item.map(Frame::data), State::Upstream(rx)))
            }
            State::Done => None,
        }
    });
    BodyExt::boxed(StreamBody::new(stream))
}

/// Read `start..=end` from upstream on its own task, a bounded request at a time, into a short queue. One request after
/// another on the same connection: a request sent ahead needs a second connection, and on a long path a connection's
/// handshake costs more than the round trip it would save (measured: the first GOP came 0.3 s later).
fn fetch(entry: Arc<Entry>, start: u64, end: u64) -> tokio::sync::mpsc::Receiver<std::io::Result<Bytes>> {
    let (tx, rx) = tokio::sync::mpsc::channel(QUEUE);
    tokio::spawn(async move {
        let mut at = start;
        let mut to = end.min(at + FIRST_CHUNK - 1);
        while at <= end {
            let url = entry.upstream.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let sent =
                entry.client.get(url).header(reqwest::header::RANGE, format!("bytes={at}-{to}")).send().await;
            let mut resp = match sent {
                Ok(r) if r.status() == reqwest::StatusCode::PARTIAL_CONTENT => r,
                Ok(r) => {
                    let why = format!("upstream answered {} for bytes {at}-{to}", r.status().as_u16());
                    return failed(&tx, why).await;
                }
                Err(e) => return failed(&tx, format!("upstream bytes {at}-{to}: {}", e.without_url())).await,
            };
            let mut left = to - at + 1;
            loop {
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        let n = (chunk.len() as u64).min(left);
                        left -= n;
                        at += n;
                        if tx.send(Ok(chunk.slice(..n as usize))).await.is_err() {
                            // ffmpeg went elsewhere. Read out a short remainder so the connection goes back warm.
                            if left <= DRAIN_MAX {
                                while let Ok(Some(_)) = resp.chunk().await {}
                            }
                            return;
                        }
                        if left == 0 {
                            break;
                        }
                    }
                    Ok(None) => {
                        return failed(&tx, format!("upstream ended {left} bytes short of {to}")).await
                    }
                    Err(e) => {
                        let why = format!("upstream ended {left} bytes short of {to}: {}", e.without_url());
                        return failed(&tx, why).await;
                    }
                }
            }
            to = end.min(to + CHUNK);
        }
    });
    rx
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
}
