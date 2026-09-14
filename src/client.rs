//! A short name for the browser behind a session, for its log line: the browser's family and major version, its OS,
//! and the HLS player the page chose. Enough to answer "which browser was that?" from the log, and no more — the
//! User-Agent itself is never logged, and nothing here decides anything.
//!
//! One blind spot is inherent: every browser on iOS is WebKit, and Brave there sends Safari's User-Agent unchanged, so
//! it reads as Safari.

use hyper::header::HeaderValue;

/// `Chrome 151 · macOS · hls.js`. `player` is the page's own word for its HLS player (`native` or `hls.js`); anything
/// else is left out.
pub fn label(user_agent: Option<&HeaderValue>, player: Option<&str>) -> String {
    let ua = user_agent.and_then(|v| v.to_str().ok()).unwrap_or("");
    let mut parts = vec![browser(ua), os(ua)];
    if let Some(p) = player.filter(|p| matches!(*p, "native" | "hls.js")) {
        parts.push(p.to_string());
    }
    parts.join(" · ")
}

/// The digits right after `token`: `Chrome/151.0.0.0` → `151`.
fn major(ua: &str, token: &str) -> Option<String> {
    let rest = &ua[ua.find(token)? + token.len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    (!digits.is_empty()).then_some(digits)
}

fn browser(ua: &str) -> String {
    // Most specific first: Edge, Opera and the iOS shells all also name Chrome or Safari.
    let named = [
        ("EdgiOS/", "Edge"),
        ("Edg/", "Edge"),
        ("OPR/", "Opera"),
        ("CriOS/", "Chrome"),
        ("FxiOS/", "Firefox"),
        ("Firefox/", "Firefox"),
        ("Chrome/", "Chrome"),
    ];
    for (token, name) in named {
        if let Some(version) = major(ua, token) {
            return format!("{name} {version}");
        }
    }
    if ua.contains("Safari/") {
        return major(ua, "Version/").map_or_else(|| "Safari".to_string(), |v| format!("Safari {v}"));
    }
    if ua.is_empty() { "no User-Agent" } else { "another browser" }.to_string()
}

fn os(ua: &str) -> String {
    // iOS names its version as `iPhone OS 18_5` and iPadOS as `CPU OS 18_5`, both before the `Mac OS X` they also say.
    let apple = |name: &str| major(ua, " OS ").map_or_else(|| name.to_string(), |v| format!("{name} {v}"));
    if ua.contains("iPhone") {
        apple("iOS")
    } else if ua.contains("iPad") {
        apple("iPadOS")
    } else if ua.contains("Android") {
        "Android".to_string()
    } else if ua.contains("CrOS") {
        "ChromeOS".to_string()
    } else if ua.contains("Macintosh") {
        "macOS".to_string()
    } else if ua.contains("Windows") {
        "Windows".to_string()
    } else if ua.contains("Linux") {
        "Linux".to_string()
    } else {
        "unknown OS".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(ua: &str, player: Option<&str>) -> String {
        label(Some(&HeaderValue::from_str(ua).unwrap()), player)
    }

    #[test]
    fn a_session_names_its_browser_os_and_player() {
        let chrome_mac =
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) \
                          Chrome/151.0.0.0 Safari/537.36";
        assert_eq!(named(chrome_mac, Some("hls.js")), "Chrome 151 · macOS · hls.js");
        let safari_iphone =
            "Mozilla/5.0 (iPhone; CPU iPhone OS 18_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like \
                             Gecko) Version/18.5 Mobile/15E148 Safari/604.1";
        assert_eq!(named(safari_iphone, Some("native")), "Safari 18 · iOS 18 · native");
        let chrome_iphone =
            "Mozilla/5.0 (iPhone; CPU iPhone OS 18_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like \
                             Gecko) CriOS/140.0.7339.101 Mobile/15E148 Safari/604.1";
        assert_eq!(named(chrome_iphone, None), "Chrome 140 · iOS 18");
        let ipad = "Mozilla/5.0 (iPad; CPU OS 17_4 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) \
                    Version/17.4 Mobile/15E148 Safari/604.1";
        assert_eq!(named(ipad, None), "Safari 17 · iPadOS 17");
        let firefox = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:143.0) Gecko/20100101 Firefox/143.0";
        assert_eq!(named(firefox, Some("hls.js")), "Firefox 143 · Windows · hls.js");
        let edge = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
                    Chrome/151.0.0.0 Safari/537.36 Edg/151.0.0.0";
        assert_eq!(named(edge, None), "Edge 151 · Windows");
    }

    #[test]
    fn what_it_cannot_read_is_said_plainly_and_a_strange_player_word_is_dropped() {
        assert_eq!(label(None, Some("native")), "no User-Agent · unknown OS · native");
        assert_eq!(named("curl/8.7.1", Some("<script>")), "another browser · unknown OS");
    }
}
