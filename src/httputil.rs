//! Tiny HTTP plumbing shared by the handlers, in den-reel's shapes: one body type, JSON and error
//! builders, and a streamed body over files already opened on disk.

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Frame;
use hyper::{Response, StatusCode};
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;

/// The one body type every handler returns: bytes in, `io::Error` out (streamed file bodies can fail
/// mid-flight, full-buffer bodies never do).
pub type Body = BoxBody<Bytes, std::io::Error>;

pub fn full(data: impl Into<Bytes>) -> Body {
    Full::new(data.into()).map_err(|never| match never {}).boxed()
}

/// JSON with an explicit Content-Length and any extra headers. Errors are never cacheable.
pub fn json(status: StatusCode, value: &serde_json::Value, extra: &[(&str, &str)]) -> Response<Body> {
    let s = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    let mut b = Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("content-length", s.len());
    let has_cc = extra.iter().any(|(k, _)| k.eq_ignore_ascii_case("cache-control"));
    for (k, v) in extra {
        b = b.header(*k, *v);
    }
    if !has_cc {
        b = b.header("cache-control", "no-store");
    }
    b.body(full(s)).unwrap()
}

/// The one 404 every Den addon answers, for an unknown path and a refused `/metrics` alike.
pub fn not_found() -> Response<Body> {
    json(StatusCode::NOT_FOUND, &serde_json::json!({"error": "not_found"}), &[])
}

/// `{ "error": <code>, "detail": <text> }` — the shape every Den addon answers with.
pub fn error(status: StatusCode, code: &str, detail: &str) -> Response<Body> {
    json(status, &serde_json::json!({ "error": code, "detail": detail }), &[])
}

/// A text body of a given type, never stored by a cache.
pub fn text(content_type: &str, body: String) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", content_type)
        .header("content-length", body.len())
        .header("cache-control", "no-store")
        .body(full(body))
        .unwrap()
}

/// One read buffer per file being streamed. Segments are a few MB; 64 KiB keeps the syscall count low
/// without holding much per response.
const STREAM_BUF: usize = 64 * 1024;

/// `len` random bytes, made as they go out a buffer at a time: nothing on the way can compress them, so the time they
/// take to arrive is the link's.
pub fn random_body(len: u64) -> Body {
    let stream = futures_util::stream::unfold(len, |left| async move {
        (left > 0).then(|| {
            let n = left.min(STREAM_BUF as u64);
            let mut chunk = vec![0u8; n as usize];
            getrandom::fill(&mut chunk).expect("the OS random source is unavailable");
            (Ok(Frame::data(Bytes::from(chunk))), left - n)
        })
    });
    BodyExt::boxed(StreamBody::new(stream))
}

/// Stream already-opened files back to back, `len` bytes of each from its current position. The
/// files are opened before the response starts, so deleting them from the scratch window while the
/// body is still going out cannot cut it short — an open descriptor keeps its inode.
pub fn files_body(parts: Vec<(std::fs::File, u64)>) -> Body {
    let stream = futures_util::stream::iter(parts)
        .map(|(f, len)| ReaderStream::with_capacity(tokio::fs::File::from_std(f).take(len), STREAM_BUF))
        .flatten()
        .map_ok(Frame::data);
    BodyExt::boxed(StreamBody::new(stream))
}
