//! A player's report on its session (`POST …/report`): why it couldn't play (`{code, message}`), how playing went
//! (`{stats}`), or both. Every field of `stats` is optional and read leniently — a field missing or of another type is
//! left out of the line, never a refusal — since the web app's player decides what it can measure. The report is
//! the browser's text: nothing of it reaches the log but numbers and short words with URLs scrubbed.

use serde_json::Value;

/// The largest report taken: stats with their stall and error lists, far past what a session's worth makes.
pub const MAX_BODY: usize = 64 * 1024;
/// Stalls and errors named on the line; the rest are counted.
const MAX_STALLS: usize = 5;
const MAX_ERRORS: usize = 5;
/// The longest word — a browser, an engine, an error's name — the line carries.
const MAX_WORD: usize = 40;

pub struct Report {
    /// The old shape: the player's error code and message.
    pub error: Option<(u16, String)>,
    /// How playing went, as the player measured it.
    pub stats: Option<Value>,
}

/// A report, when the body is one: a JSON object with `code` and `message`, or `stats`, or both.
pub fn parse(body: &[u8]) -> Option<Report> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let obj = v.as_object()?;
    let error = match (obj.get("code").and_then(Value::as_u64), obj.get("message").and_then(Value::as_str)) {
        (Some(code), Some(message)) => Some((code.min(u16::MAX.into()) as u16, message.to_string())),
        _ => None,
    };
    let stats = obj.get("stats").filter(|s| s.is_object()).cloned();
    (error.is_some() || stats.is_some()).then_some(Report { error, stats })
}

/// One of the browser's words as the log may show it: printable, URLs scrubbed, no spaces, short.
fn word(raw: &str) -> String {
    crate::redact::player_message(raw).replace(' ', "_").chars().take(MAX_WORD).collect()
}

fn num(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|n| n.is_finite())
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(word).filter(|w| !w.is_empty())
}

/// One stall: `at=512s 4200ms v=0.0 a=6.1`, what it says of itself.
fn stall(s: &Value) -> String {
    let parts = [
        num(s, "at").map(|n| format!("at={n:.0}s")),
        num(s, "ms").map(|n| format!("{n:.0}ms")),
        num(s, "videoAhead").map(|n| format!("v={n:.1}")),
        num(s, "audioAhead").map(|n| format!("a={n:.1}")),
    ];
    let out: Vec<String> = parts.into_iter().flatten().collect();
    if out.is_empty() {
        "?".into()
    } else {
        out.join(" ")
    }
}

/// The report's stats as one bounded log line for session `short`:
/// `session AbCdEf: report chrome/macos hls.js frags=120 med=4100k p10=900k slowest=9800ms est=3800k dropped=0/5400
/// stalls=3 [at=512s 4200ms v=0.0 a=6.1; …] errors=bufferStalledError x2,fragLoadError(fatal)`.
pub fn stats_line(short: &str, s: &Value) -> String {
    let mut line = format!(
        "session {short}: report {} {}",
        text(s, "browser").unwrap_or_else(|| "?".into()),
        text(s, "engine").unwrap_or_else(|| "?".into())
    );
    if let Some(f) = s.get("fragments").filter(|f| f.is_object()) {
        let parts = [
            num(f, "count").map(|n| format!("frags={n:.0}")),
            num(f, "bytes").map(|n| format!("bytes={:.0}M", n / 1e6)),
            num(f, "loadMs").map(|n| format!("load={n:.0}ms")),
            num(f, "kbpsMedian").map(|n| format!("med={n:.0}k")),
            num(f, "kbpsP10").map(|n| format!("p10={n:.0}k")),
            num(f, "slowestMs").map(|n| format!("slowest={n:.0}ms")),
        ];
        for p in parts.into_iter().flatten() {
            line.push(' ');
            line.push_str(&p);
        }
    }
    if let Some(n) = num(s, "bandwidthEstimateKbps") {
        line.push_str(&format!(" est={n:.0}k"));
    }
    match (num(s, "droppedFrames"), num(s, "totalFrames")) {
        (Some(d), Some(t)) => line.push_str(&format!(" dropped={d:.0}/{t:.0}")),
        (Some(d), None) => line.push_str(&format!(" dropped={d:.0}")),
        _ => {}
    }
    if let Some(stalls) = s.get("stalls").and_then(Value::as_array) {
        line.push_str(&format!(" stalls={}", stalls.len()));
        if !stalls.is_empty() {
            let mut shown: Vec<String> = stalls.iter().take(MAX_STALLS).map(stall).collect();
            if stalls.len() > MAX_STALLS {
                shown.push(format!("+{} more", stalls.len() - MAX_STALLS));
            }
            line.push_str(&format!(" [{}]", shown.join("; ")));
        }
    }
    if let Some(errors) = s.get("errors").and_then(Value::as_array).filter(|e| !e.is_empty()) {
        // Each kind once, in the order first seen, with how often it came and whether it was ever fatal.
        let mut kinds: Vec<(String, usize, bool)> = Vec::new();
        for e in errors {
            let name = text(e, "details").unwrap_or_else(|| "?".into());
            let fatal = e.get("fatal").and_then(Value::as_bool).unwrap_or(false);
            match kinds.iter_mut().find(|(n, _, _)| *n == name) {
                Some(k) => {
                    k.1 += 1;
                    k.2 |= fatal;
                }
                None => kinds.push((name, 1, fatal)),
            }
        }
        let mut shown: Vec<String> = kinds
            .iter()
            .take(MAX_ERRORS)
            .map(|(name, count, fatal)| {
                let count = if *count > 1 { format!(" x{count}") } else { String::new() };
                format!("{name}{count}{}", if *fatal { "(fatal)" } else { "" })
            })
            .collect();
        if kinds.len() > MAX_ERRORS {
            shown.push(format!("+{} more", kinds.len() - MAX_ERRORS));
        }
        line.push_str(&format!(" errors={}", shown.join(",")));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_shape_still_parses() {
        let r = parse(br#"{"code":3,"message":"DECODE"}"#).expect("a report");
        assert_eq!(r.error, Some((3, "DECODE".to_string())));
        assert!(r.stats.is_none());
    }

    #[test]
    fn the_stats_shape_parses_with_or_without_an_error() {
        let r = parse(br#"{"stats":{"engine":"native"},"unknown":[1,2]}"#).expect("stats alone");
        assert!(r.error.is_none() && r.stats.is_some());
        let r = parse(br#"{"code":2,"message":"x","stats":{}}"#).expect("both");
        assert!(r.error.is_some() && r.stats.is_some());
    }

    #[test]
    fn what_is_not_a_report_is_refused() {
        for garbage in
            [&b"{}"[..], b"[]", b"null", b"not json", br#"{"code":3}"#, br#"{"stats":"fast"}"#, b""]
        {
            assert!(parse(garbage).is_none(), "{}", String::from_utf8_lossy(garbage));
        }
    }

    #[test]
    fn the_stats_line_says_it_all_compactly() {
        let body = br#"{"stats":{"browser":"chrome/macos","engine":"hls.js",
            "fragments":{"count":120,"bytes":512000000,"loadMs":61200,"slowestMs":9800,"kbpsP10":900.4,"kbpsMedian":4100},
            "bandwidthEstimateKbps":3800,"droppedFrames":0,"totalFrames":5400,
            "stalls":[{"at":512.3,"ms":4200,"videoAhead":0,"audioAhead":6.1}],
            "errors":[{"details":"bufferStalledError","fatal":false},{"details":"bufferStalledError"},
                      {"details":"fragLoadError","fatal":true}],
            "future":{"field":1}}}"#;
        let stats = parse(body).unwrap().stats.unwrap();
        assert_eq!(
            stats_line("AbCdEf", &stats),
            "session AbCdEf: report chrome/macos hls.js frags=120 bytes=512M load=61200ms med=4100k p10=900k \
             slowest=9800ms est=3800k dropped=0/5400 stalls=1 [at=512s 4200ms v=0.0 a=6.1] \
             errors=bufferStalledError x2,fragLoadError(fatal)"
        );
    }

    #[test]
    fn a_sparse_or_mistyped_report_leaves_out_what_it_lacks() {
        let stats = serde_json::json!({"engine": 7, "droppedFrames": "many", "stalls": [], "fragments": {"count": "x"}});
        assert_eq!(stats_line("AbCdEf", &stats), "session AbCdEf: report ? ? stalls=0");
    }

    #[test]
    fn the_line_stays_bounded_and_carries_no_url() {
        let stalls: Vec<Value> = (0..500).map(|i| serde_json::json!({"at": i, "ms": 1000})).collect();
        let errors: Vec<Value> = (0..500)
            .map(|i| serde_json::json!({"details": format!("e{i} https://cdn.example/t/SECRET {}", "y".repeat(300))}))
            .collect();
        let stats = serde_json::json!({
            "browser": "x\nsession ZZZZZZ: ended (forged)",
            "engine": "z".repeat(10_000),
            "stalls": stalls,
            "errors": errors,
        });
        let line = stats_line("AbCdEf", &stats);
        assert!(line.len() < 1000, "{} bytes: {line}", line.len());
        assert!(!line.contains('\n') && !line.contains("SECRET") && !line.contains("cdn.example"), "{line}");
        assert!(line.contains("stalls=500 [") && line.contains("; +495 more]"), "{line}");
        assert!(line.contains(",+495 more"), "errors past the first five are counted: {line}");
    }
}
