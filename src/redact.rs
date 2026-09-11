//! What the log may say. The scout install URL lists and plays anything; a ticket or a debrid link
//! plays one title; a session signature plays it from anywhere. None of them may reach a log line,
//! and every line that carries upstream or ffmpeg text goes through [`scrub`] first.

use std::borrow::Cow;

/// `text` with every configured secret and every URL replaced. Secrets first, so a secret that
/// appears without its scheme (a host and path quoted by some error message) goes too.
pub fn scrub(text: &str, secrets: &[&str]) -> String {
    let mut s = text.to_string();
    for secret in secrets.iter().filter(|x| !x.is_empty()) {
        s = s.replace(secret, "<scout>");
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(i) = [rest.find("http://"), rest.find("https://")].into_iter().flatten().min() {
        out.push_str(&rest[..i]);
        out.push_str("<url>");
        let tail = &rest[i..];
        let mut end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '>' | ')' | ','))
            .unwrap_or(tail.len());
        // ffmpeg writes `<url>: Server returned 403`; the colon is the message's, not the URL's.
        while end > 0 && tail[..end].ends_with([':', '.', ';']) {
            end -= 1;
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// The request path as the request log may show it: our own routes verbatim, a session path with the
/// id shortened and the signature removed, and anything else as `/<unrouted>` — a stray path could be
/// someone pasting a URL that carries a secret.
pub fn path(path: &str) -> Cow<'_, str> {
    if matches!(path, "/health" | "/remux/health" | "/metrics" | "/remux/login" | "/remux/session") {
        return path.into();
    }
    let Some(rest) = path.strip_prefix("/remux/s/") else { return "/<unrouted>".into() };
    let mut it = rest.splitn(3, '/');
    let sid = it.next().unwrap_or("");
    let sid = if crate::auth::is_id(sid) { &sid[..6] } else { "<bad>" };
    let _sig = it.next();
    let file = match it.next() {
        Some(f) if crate::session::is_session_file(f) => format!("/{f}"),
        Some(_) => "/<file>".to_string(),
        None => String::new(),
    };
    format!("/remux/s/{sid}…/<sig>{file}").into()
}

/// The caller's `X-Request-Id`, reduced to `[A-Za-z0-9_-]` and 32 characters: it is written into the
/// log verbatim, so nothing that could forge a line or carry a secret gets through.
pub fn request_id(headers: &hyper::HeaderMap) -> Option<String> {
    let raw = headers.get("x-request-id")?.to_str().ok()?;
    let id: String =
        raw.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(32).collect();
    (!id.is_empty()).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSTALL: &str = "http://192.168.86.193:8080/eyJzZWFsZWQiOiJ4In0";

    #[test]
    fn the_install_url_never_survives() {
        let line =
            format!("scout list failed: error sending request for url ({INSTALL}/stream/movie/tt1.json)");
        let s = scrub(&line, &[INSTALL]);
        assert!(!s.contains("eyJzZWFsZWQ"), "{s}");
        assert!(!s.contains("192.168.86.193"), "{s}");
        // The config segment alone — no scheme, no host — is still a secret.
        let bare = scrub("bad config eyJzZWFsZWQiOiJ4In0 rejected", &["eyJzZWFsZWQiOiJ4In0"]);
        assert_eq!(bare, "bad config <scout> rejected");
    }

    #[test]
    fn debrid_and_ticket_urls_are_scrubbed() {
        let s = scrub(
            "[https @ 0x1] HTTP error 403 Forbidden\nhttps://cdn.real-debrid.com/d/ABCDEF/movie.mkv: Server returned 403",
            &[],
        );
        assert!(!s.contains("ABCDEF") && !s.contains("real-debrid"), "{s}");
        assert!(s.contains("<url>: Server returned 403"), "the diagnostic must survive: {s}");
        assert_eq!(scrub("open http://h/p/TICKET failed", &[]), "open <url> failed");
    }

    #[test]
    fn a_session_path_drops_its_signature() {
        let sid = "AAAAAAAAAAAAAAAAAAAAAA";
        let p = format!("/remux/s/{sid}/SIGSIGSIGSIGSIGSIGSIGS/seg3.m4s");
        assert_eq!(path(&p), "/remux/s/AAAAAA…/<sig>/seg3.m4s");
        assert_eq!(path(&format!("/remux/s/{sid}/SIG/whatever")), "/remux/s/AAAAAA…/<sig>/<file>");
        assert_eq!(path("/remux/s/not-an-id/x"), "/remux/s/<bad>…/<sig>");
        assert_eq!(path("/remux/session"), "/remux/session");
        assert_eq!(path(&format!("{INSTALL}/manifest.json")), "/<unrouted>");
    }
}
