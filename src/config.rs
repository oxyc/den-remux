//! Runtime configuration, all from the environment.
//!
//! Env: PORT, SCOUT_ORIGINS, REMUX_SCOUT_KEY, SCOUT_INSTALL_URL, SUBTITLE_ORIGINS, ORIGIN_ALIASES, BROWSER_KEY_HASHES,
//!      REMUX_URL_KEY,
//!      MAX_SESSIONS, MAX_SESSIONS_PER_INSTALL, SESSION_IDLE_SECS, SCRATCH_DIR, SCRATCH_MAX_BYTES, FFMPEG_PATH,
//!      MAX_TRANSCODES, VAAPI_DEVICE, TRUSTED_PROXIES, METRICS_TOKEN, LOG_REQUESTS.

use std::env;
use std::path::PathBuf;
use std::time::Duration;

pub struct Config {
    pub port: u16,
    /// `SCOUT_ORIGINS` — the only origins (`scheme://host[:port]`) a browser's scoped scout URL may point
    /// at. This list is the SSRF guard on `POST /remux/session {scout}`; empty refuses every such URL.
    pub scout_origins: Vec<String>,
    /// `REMUX_SCOUT_KEY` — the service key sent to scout as `X-Den-Remux-Key`. A scope=availability
    /// scout config lists and plays only for a caller presenting it.
    pub scout_key: Option<String>,
    /// `SCOUT_INSTALL_URL` — the fallback when a request names no scout: a den-scout install of this
    /// service's own, sealed config included, with no trailing slash. A secret: it lists and plays
    /// anything, so it is never logged and never leaves the process.
    pub scout_install_url: Option<String>,
    /// `SUBTITLE_ORIGINS` — the only origins a request's den-subtitles install, and the subtitle URLs it
    /// answers with, may be on. Empty turns subtitles off.
    pub subtitle_origins: Vec<String>,
    /// `ORIGIN_ALIASES` — public origin → LAN origin (`https://d-scout.oxy.fi=http://192.168.86.193:8080,…`).
    /// scout and den-subtitles are on this box, so an install URL or play URL on a public name is fetched at
    /// the LAN address: two services on one box must not need the WAN, Cloudflare and the tunnel between
    /// them, and this service never holds the Access token the public names ask for (oxyc/den#15).
    pub origin_aliases: Vec<(String, String)>,
    /// `BROWSER_KEY_HASHES` — SHA-256 of each browser's key. Only hashes live in the env file, so the
    /// file on the box cannot be replayed as a key.
    pub browser_key_hashes: Vec<[u8; 32]>,
    /// MAC key for the cookie and the session URLs, derived from `REMUX_URL_KEY`. Rotating it logs
    /// every browser out and kills every session URL at once.
    pub url_key: Vec<u8>,
    /// No `REMUX_URL_KEY` was set, so `url_key` is random for this process: every restart logs the
    /// browsers out. Reported by /health.
    pub url_key_ephemeral: bool,
    pub max_sessions: usize,
    /// `MAX_SESSIONS_PER_INSTALL` — sessions one scout install may play at once without a login (default
    /// 2), under `MAX_SESSIONS`: one household, or one leaked install URL, cannot take every slot.
    pub max_sessions_per_install: usize,
    pub session_idle: Duration,
    pub scratch_dir: PathBuf,
    pub scratch_max_bytes: u64,
    pub ffmpeg: String,
    /// `MAX_TRANSCODES` — sessions transcoding on the GPU at once (default 1; 0 turns transcoding off).
    /// Separate from `MAX_SESSIONS`: a copy costs no GPU.
    pub max_transcodes: usize,
    /// `VAAPI_DEVICE` — the GPU's render node.
    pub vaapi_device: PathBuf,
    /// `TRUSTED_PROXIES` — proxies (comma-separated IPs) whose `X-Forwarded-For` names the visitor, for the
    /// per-visitor limit on logins and new sessions. `tailscale serve` connects from its host's address.
    pub trusted_proxies: Vec<std::net::IpAddr>,
    /// `METRICS_TOKEN` — the bearer token `/metrics` requires. `None` turns the endpoint off (404).
    pub metrics_token: Option<String>,
    /// `LOG_REQUESTS` — one stderr line per response when set (anything but empty or `0`).
    pub log_requests: bool,
}

fn env_opt(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

/// `LOG_REQUESTS`: off when unset, empty, or exactly `0`; on for any other value — the rule every Den
/// addon reads it by.
pub(crate) fn log_requests_on(v: Option<&str>) -> bool {
    v.is_some_and(|v| !v.is_empty() && v != "0")
}

/// `BROWSER_KEY_HASHES`: comma-separated 64-character hex digests. A malformed entry is said once and
/// skipped rather than failing the boot — the other browsers should keep working.
pub(crate) fn parse_key_hashes(raw: &str) -> Vec<[u8; 32]> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let parsed = crate::auth::parse_hex32(s);
            if parsed.is_none() {
                eprintln!(
                    "warning: BROWSER_KEY_HASHES has an entry that is not 64 hex characters; skipping it"
                );
            }
            parsed
        })
        .collect()
}

/// `SCOUT_ORIGINS`: comma-separated `scheme://host[:port]`, normalised to lower case without a trailing
/// slash. An entry with a path, query or credentials is said once and skipped: an origin list that
/// quietly admitted more than it names would not be a guard.
pub(crate) fn parse_origins(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let o = s.trim_end_matches('/').to_ascii_lowercase();
            let ok = o.split_once("://").is_some_and(|(scheme, host)| {
                matches!(scheme, "http" | "https") && !host.is_empty() && !host.contains(['/', '?', '#', '@'])
            });
            if !ok {
                eprintln!("warning: SCOUT_ORIGINS entry {s:?} is not scheme://host[:port]; skipping it");
            }
            ok.then_some(o)
        })
        .collect()
}

/// `TRUSTED_PROXIES`: comma-separated IP addresses. A malformed entry is said once and skipped.
pub(crate) fn parse_proxies(raw: &str) -> Vec<std::net::IpAddr> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let parsed = s.parse().ok();
            if parsed.is_none() {
                eprintln!("warning: TRUSTED_PROXIES entry {s:?} is not an IP address; skipping it");
            }
            parsed
        })
        .collect()
}

/// `ORIGIN_ALIASES`: comma-separated `<public origin>=<LAN origin>` pairs, each side as `parse_origins` reads
/// it. A malformed pair is said once and skipped.
pub(crate) fn parse_aliases(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|pair| {
            let parsed = pair.split_once('=').and_then(|(public, lan)| {
                let one = |o: &str| parse_origins(o).into_iter().next();
                Some((one(public)?, one(lan)?))
            });
            if parsed.is_none() {
                eprintln!(
                    "warning: ORIGIN_ALIASES entry {pair:?} is not <public origin>=<LAN origin>; skipping it"
                );
            }
            parsed
        })
        .collect()
}

/// `url` at its LAN address when it is on a public origin in `aliases`; anything else as it is. The path
/// and query are kept, so a sealed config segment or a ticket goes along unchanged.
pub fn local(url: &str, aliases: &[(String, String)]) -> String {
    for (public, lan) in aliases {
        if let Some(rest) = url.strip_prefix(public.as_str()) {
            if rest.is_empty() || rest.starts_with(['/', '?']) {
                return format!("{lan}{rest}");
            }
        }
    }
    url.to_string()
}

/// Below this the cap cannot hold one window of a 4K session, and every job would sit paused.
const MIN_SCRATCH_BYTES: u64 = 64 * 1024 * 1024;

impl Config {
    pub fn from_env() -> Config {
        let (url_key, url_key_ephemeral) = match env_opt("REMUX_URL_KEY") {
            // Hashed, so any length the operator typed is a valid key of a fixed size.
            Some(k) => (crate::auth::sha256(k.as_bytes()).to_vec(), false),
            None => {
                eprintln!(
                    "warning: REMUX_URL_KEY is unset — using a random key, so every restart logs the browsers out"
                );
                (crate::auth::random_bytes::<32>().to_vec(), true)
            }
        };
        Config {
            port: env_opt("PORT").and_then(|v| v.parse().ok()).unwrap_or(8095),
            scout_origins: parse_origins(&env_opt("SCOUT_ORIGINS").unwrap_or_default()),
            scout_key: env_opt("REMUX_SCOUT_KEY"),
            scout_install_url: env_opt("SCOUT_INSTALL_URL").map(|u| u.trim_end_matches('/').to_string()),
            subtitle_origins: parse_origins(&env_opt("SUBTITLE_ORIGINS").unwrap_or_default()),
            origin_aliases: parse_aliases(&env_opt("ORIGIN_ALIASES").unwrap_or_default()),
            browser_key_hashes: parse_key_hashes(&env_opt("BROWSER_KEY_HASHES").unwrap_or_default()),
            url_key,
            url_key_ephemeral,
            max_sessions: env_opt("MAX_SESSIONS")
                .and_then(|v| v.parse().ok())
                .filter(|n| *n >= 1)
                .unwrap_or(2),
            max_sessions_per_install: env_opt("MAX_SESSIONS_PER_INSTALL")
                .and_then(|v| v.parse().ok())
                .filter(|n| *n >= 1)
                .unwrap_or(2),
            // Floored: an idle window shorter than a player's pause-and-resume would kill sessions
            // that are merely paused.
            session_idle: Duration::from_secs(
                env_opt("SESSION_IDLE_SECS").and_then(|v| v.parse().ok()).filter(|s| *s >= 30).unwrap_or(600),
            ),
            scratch_dir: env_opt("SCRATCH_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/cache")),
            scratch_max_bytes: env_opt("SCRATCH_MAX_BYTES")
                .and_then(|v| v.parse().ok())
                .filter(|b| *b >= MIN_SCRATCH_BYTES)
                .unwrap_or(1024 * 1024 * 1024),
            ffmpeg: env_opt("FFMPEG_PATH").unwrap_or_else(|| "ffmpeg".to_string()),
            max_transcodes: env_opt("MAX_TRANSCODES").and_then(|v| v.parse().ok()).unwrap_or(1),
            vaapi_device: env_opt("VAAPI_DEVICE")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/dev/dri/renderD128")),
            trusted_proxies: parse_proxies(&env_opt("TRUSTED_PROXIES").unwrap_or_default()),
            metrics_token: env_opt("METRICS_TOKEN"),
            log_requests: log_requests_on(env::var("LOG_REQUESTS").ok().as_deref()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scout_origins_admit_only_bare_origins() {
        let o = parse_origins(
            "http://192.168.86.193:8080/, HTTPS://Scout.lan ,http://x/path,ftp://y,http://u@z,",
        );
        assert_eq!(o, ["http://192.168.86.193:8080", "https://scout.lan"]);
    }

    #[test]
    fn a_public_name_is_fetched_at_its_lan_address() {
        let a = parse_aliases(
            "https://d-scout.oxy.fi=http://192.168.86.193:8080, HTTPS://D-Subs.oxy.fi/=http://192.168.86.193:8093,junk",
        );
        assert_eq!(a.len(), 2, "{a:?}");
        assert_eq!(local("https://d-scout.oxy.fi/c2VhbGVk", &a), "http://192.168.86.193:8080/c2VhbGVk");
        assert_eq!(
            local("https://d-subs.oxy.fi/c/subtitle/9.vtt?lang=fin", &a),
            "http://192.168.86.193:8093/c/subtitle/9.vtt?lang=fin"
        );
        assert_eq!(
            local("https://d-scout.oxy.fi.evil/x", &a),
            "https://d-scout.oxy.fi.evil/x",
            "a longer host is not the name"
        );
        assert_eq!(local("https://cdn.debrid/f.mkv", &a), "https://cdn.debrid/f.mkv");
    }

    #[test]
    fn trusted_proxies_are_addresses() {
        let p = parse_proxies("192.168.86.149, ::1,pve,,10.0.0.1/8");
        assert_eq!(p, ["192.168.86.149".parse::<std::net::IpAddr>().unwrap(), "::1".parse().unwrap()]);
    }

    #[test]
    fn log_requests_follows_the_fleet_rule() {
        assert!(!log_requests_on(None));
        assert!(!log_requests_on(Some("")));
        assert!(!log_requests_on(Some("0")));
        assert!(log_requests_on(Some("1")));
        assert!(log_requests_on(Some("true")));
    }

    #[test]
    fn a_bad_key_hash_is_skipped_not_fatal() {
        let good = "a".repeat(64);
        let parsed = parse_key_hashes(&format!("{good}, nothex, ,{}", "B".repeat(64)));
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], [0xaa; 32]);
        assert_eq!(parsed[1], [0xbb; 32]);
    }
}
