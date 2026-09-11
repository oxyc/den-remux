//! Who may do what.
//!
//! Creating a session takes one of two credentials. Usually it is the full scout install URL the request
//! names — the credential every Den addon's install URL is — and scout alone decides whether it plays.
//! A browser may also prove itself once, by posting its key to `/remux/login` (its SHA-256 has to be one
//! of `BROWSER_KEY_HASHES`), for what needs den-remux to vouch: an availability-only scout install, or
//! this service's own. What it gets back is a cookie — `HttpOnly`, so script on the page cannot read it —
//! that authorises one thing: **creating** a session.
//!
//! Everything a session serves is under a signed path, `/remux/s/<sid>/<sig>/…`, and the cookie is not
//! asked for there. A Cast or AirPlay receiver has no cookie, and the signed path is what lets it play
//! anyway: whoever holds that URL can watch that one title until the session ends, and nothing else.
//!
//! Both MACs are HMAC-SHA256 under the key derived from `REMUX_URL_KEY`, with a domain string so a
//! cookie can never be presented as a URL signature or the other way round.

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

pub const COOKIE_NAME: &str = "den_remux";
/// A browser logs in about once a month.
pub const COOKIE_TTL_SECS: u64 = 30 * 24 * 60 * 60;
/// 16 bytes of MAC, base64url: 22 characters. 128 bits is past guessing, and it keeps the URL short.
pub const SIG_LEN: usize = 22;

fn mac(key: &[u8], domain: &str, parts: &[&str]) -> [u8; 32] {
    let mut m = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC takes a key of any length");
    m.update(domain.as_bytes());
    for p in parts {
        // A separator that cannot occur in any part, so ("ab","c") and ("a","bc") sign differently.
        m.update(&[0]);
        m.update(p.as_bytes());
    }
    m.finalize().into_bytes().into()
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the OS random source is unavailable");
    b
}

/// A fresh 128-bit session id, base64url.
pub fn random_id() -> String {
    b64(&random_bytes::<16>())
}

/// Is this shaped like an id or a signature we issue — 22 base64url characters?
pub fn is_id(s: &str) -> bool {
    s.len() == 22 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

pub fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in b.chunks(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The signature in a session's URL: it covers the id and the expiry, so neither can be changed.
pub fn url_sig(key: &[u8], sid: &str, exp: u64) -> String {
    b64(&mac(key, "den-remux/url", &[sid, &exp.to_string()])[..16])
}

/// Does `presented` sign this session? Constant-time, so the signature cannot be learnt a character
/// at a time from how long a refusal took.
pub fn url_sig_ok(key: &[u8], sid: &str, exp: u64, presented: &str) -> bool {
    presented.len() == SIG_LEN && presented.as_bytes().ct_eq(url_sig(key, sid, exp).as_bytes()).into()
}

/// A browser's id: the first 8 bytes of its key hash. Stable across logins, names nothing secret, and
/// stops matching the moment that hash is removed from `BROWSER_KEY_HASHES`.
pub fn browser_id(hash: &[u8; 32]) -> String {
    hex(&hash[..8])
}

/// Whom a session started without a login belongs to: the install it names, as the first 8 bytes of the
/// install URL's hash — stable across requests, and not the URL, which is a secret.
pub fn install_id(base: &str) -> String {
    format!("install:{}", hex(&sha256(base.as_bytes())[..8]))
}

/// The browser `key` belongs to, if any. Every configured hash is compared, with no early exit, so
/// the time taken does not say which one matched.
pub fn browser_for_key(hashes: &[[u8; 32]], key: &str) -> Option<String> {
    let h = sha256(key.as_bytes());
    let mut found = None;
    for known in hashes {
        if bool::from(h.ct_eq(known)) {
            found = Some(browser_id(known));
        }
    }
    found
}

/// `<browser>.<exp>.<mac>`.
pub fn cookie_value(key: &[u8], browser: &str, exp: u64) -> String {
    let m = mac(key, "den-remux/cookie", &[browser, &exp.to_string()]);
    format!("{browser}.{exp}.{}", b64(&m[..16]))
}

/// `Path=/remux` keeps it off every other route on the host; `SameSite=Strict` keeps it off every
/// request another site starts, which is the CSRF defence for `POST /remux/session`.
pub fn set_cookie(value: &str) -> String {
    format!(
        "{COOKIE_NAME}={value}; Path=/remux; Max-Age={COOKIE_TTL_SECS}; HttpOnly; Secure; SameSite=Strict"
    )
}

/// The browser a `Cookie` header speaks for: a cookie we issued, not expired, for a key that is still
/// configured.
pub fn cookie_browser(key: &[u8], hashes: &[[u8; 32]], header: Option<&str>, now: u64) -> Option<String> {
    let value = header?.split(';').map(str::trim).find_map(|kv| kv.strip_prefix("den_remux="))?;
    let mut it = value.splitn(3, '.');
    let (browser, exp) = (it.next()?, it.next()?);
    let exp: u64 = exp.parse().ok()?;
    if now >= exp {
        return None;
    }
    let expected = cookie_value(key, browser, exp);
    if !bool::from(value.as_bytes().ct_eq(expected.as_bytes())) {
        return None;
    }
    hashes.iter().any(|h| browser_id(h) == browser).then(|| browser.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

    #[test]
    fn a_url_signature_verifies_only_for_its_own_session() {
        let sid = random_id();
        assert!(is_id(&sid));
        let sig = url_sig(KEY, &sid, 1_000);
        assert_eq!(sig.len(), SIG_LEN);
        assert!(url_sig_ok(KEY, &sid, 1_000, &sig));
        assert!(!url_sig_ok(KEY, &sid, 1_001, &sig), "the expiry is covered");
        assert!(!url_sig_ok(KEY, &random_id(), 1_000, &sig), "the id is covered");
        assert!(!url_sig_ok(b"another key", &sid, 1_000, &sig), "the key is covered");
        let mut tampered = sig.clone().into_bytes();
        tampered[0] = if tampered[0] == b'A' { b'B' } else { b'A' };
        assert!(!url_sig_ok(KEY, &sid, 1_000, std::str::from_utf8(&tampered).unwrap()));
        assert!(!url_sig_ok(KEY, &sid, 1_000, &sig[..21]), "truncation is not a pass");
    }

    #[test]
    fn a_cookie_can_never_stand_in_for_a_url_signature() {
        let sid = "AAAAAAAAAAAAAAAAAAAAAA";
        let c = cookie_value(KEY, sid, 1_000);
        let cookie_mac = c.rsplit('.').next().unwrap();
        assert!(!url_sig_ok(KEY, sid, 1_000, cookie_mac));
    }

    #[test]
    fn login_matches_only_a_configured_key() {
        let hashes = [sha256(b"phone-key"), sha256(b"laptop-key")];
        assert_eq!(browser_for_key(&hashes, "laptop-key"), Some(browser_id(&hashes[1])));
        assert_eq!(browser_for_key(&hashes, "phone-key"), Some(browser_id(&hashes[0])));
        assert_eq!(browser_for_key(&hashes, "guess"), None);
        assert_eq!(browser_for_key(&[], "phone-key"), None);
    }

    #[test]
    fn a_cookie_is_checked_for_mac_expiry_and_revocation() {
        let hashes = [sha256(b"phone-key")];
        let bid = browser_id(&hashes[0]);
        let v = cookie_value(KEY, &bid, 2_000);
        let header = format!("other=1; {COOKIE_NAME}={v}; x=y");
        assert_eq!(cookie_browser(KEY, &hashes, Some(&header), 1_000), Some(bid.clone()));
        assert_eq!(cookie_browser(KEY, &hashes, Some(&header), 2_000), None, "expired");
        assert_eq!(cookie_browser(b"rotated", &hashes, Some(&header), 1_000), None, "key rotated");
        assert_eq!(cookie_browser(KEY, &[sha256(b"other")], Some(&header), 1_000), None, "hash removed");
        let forged = format!("{COOKIE_NAME}={bid}.9999999999.{}", &v[v.len() - 22..]);
        assert_eq!(cookie_browser(KEY, &hashes, Some(&forged), 1_000), None, "a moved expiry");
        assert_eq!(cookie_browser(KEY, &hashes, None, 1_000), None);
        let set = set_cookie(&v);
        for attr in ["HttpOnly", "Secure", "SameSite=Strict", "Path=/remux"] {
            assert!(set.contains(attr), "{set} lacks {attr}");
        }
    }
}
