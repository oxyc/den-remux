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

impl Report {
    /// The error the player couldn't play past, when there was one. Den Web sends its stats with
    /// `{code: 0, message: "playback stats (<event>)"}`, which is no failure: the stats line says what sent them.
    pub fn failure(&self) -> Option<(u16, &str)> {
        let (code, message) = self.error.as_ref()?;
        (*code != 0 || self.stats.is_none()).then_some((*code, message.as_str()))
    }
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

/// One stall: `at=512s frozen 4200ms v=12.0 a=11.8`, what it says of itself. Its kind is `wait` (the player waited
/// for data) or `frozen` (the clock ran with no new frame decoded); a null `ms` is a stall not yet over, `ms=?`.
fn stall(s: &Value) -> String {
    let ms = match s.get("ms") {
        Some(Value::Null) => Some("ms=?".to_string()),
        _ => num(s, "ms").map(|n| format!("{n:.0}ms")),
    };
    let parts = [
        num(s, "at").map(|n| format!("at={n:.0}s")),
        text(s, "kind"),
        ms,
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
/// `session AbCdEf: report end chrome/windows hls.js buffers=separate frags=120 med=4100k p10=900k slowest=9800ms
/// est=3800k dropped=0/5400 stalls=7 (4 frozen) 18400ms [at=512s frozen 4200ms v=12.0 a=11.8; …]
/// errors=bufferStalledError x2,fragLoadError(fatal)`. What sent it (`event`: stall, hidden, end) leads, when named.
pub fn stats_line(short: &str, s: &Value) -> String {
    let mut line = format!("session {short}: report");
    if let Some(event) = text(s, "event") {
        line.push(' ');
        line.push_str(&event);
    }
    line.push_str(&format!(
        " {} {}",
        text(s, "browser").unwrap_or_else(|| "?".into()),
        text(s, "engine").unwrap_or_else(|| "?".into())
    ));
    // Whether a stall's videoAhead and audioAhead are each track's own buffer or one combined.
    if let Some(buffers) = text(s, "buffers") {
        line.push_str(&format!(" buffers={buffers}"));
    }
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
    // `stallCount` and `stalledMs` are totals over every stall; the `stalls` list the player sends is capped.
    let stalls = s.get("stalls").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let count = num(s, "stallCount").map(|n| n.max(0.0) as usize);
    if count.is_some() || s.get("stalls").is_some_and(Value::is_array) {
        let total = count.unwrap_or(0).max(stalls.len());
        line.push_str(&format!(" stalls={total}"));
        if stalls.iter().any(|st| st.get("kind").is_some()) {
            let frozen = stalls.iter().filter(|st| st.get("kind").and_then(Value::as_str) == Some("frozen"));
            line.push_str(&format!(" ({} frozen)", frozen.count()));
        }
        if let Some(ms) = num(s, "stalledMs") {
            line.push_str(&format!(" {ms:.0}ms"));
        }
        if !stalls.is_empty() {
            let mut shown: Vec<String> = stalls.iter().take(MAX_STALLS).map(stall).collect();
            if total > shown.len() {
                shown.push(format!("+{} more", total - shown.len()));
            }
            line.push_str(&format!(" [{}]", shown.join("; ")));
        }
    }
    if let Some(errors) = s.get("errors").and_then(Value::as_array).filter(|e| !e.is_empty()) {
        // Each kind once, in the order first seen, with how often it came (an entry's own `count`, else once) and
        // whether it was ever fatal.
        let mut kinds: Vec<(String, u64, bool)> = Vec::new();
        for e in errors {
            let name = text(e, "details").unwrap_or_else(|| "?".into());
            let fatal = e.get("fatal").and_then(Value::as_bool).unwrap_or(false);
            let times = num(e, "count").map_or(1, |n| (n.max(1.0)) as u64);
            match kinds.iter_mut().find(|(n, _, _)| *n == name) {
                Some(k) => {
                    k.1 = k.1.saturating_add(times);
                    k.2 |= fatal;
                }
                None => kinds.push((name, times, fatal)),
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
    fn den_webs_final_stats_name_the_event_buffers_frozen_stalls_and_error_counts() {
        let body = br#"{"code":0,"message":"playback stats (end)","stats":{
            "event":"end","browser":"chrome/windows","engine":"hls.js","buffers":"separate",
            "fragments":{"count":80,"kbpsMedian":4100,"kbpsP10":900,"slowestMs":9800},
            "bandwidthEstimateKbps":3800,"droppedFrames":3,"totalFrames":5400,
            "stallCount":7,"stalledMs":18400,
            "stalls":[{"at":512,"kind":"frozen","ms":4200,"videoAhead":12,"audioAhead":11.8},
                      {"at":900.4,"kind":"wait","ms":null,"videoAhead":0,"audioAhead":0.2}],
            "errors":[{"details":"bufferStalledError","fatal":false,"count":2},
                      {"details":"bufferNudgeOnStall","count":1},{"details":"bufferStalledError","count":3}]}}"#;
        let r = parse(body).expect("a report");
        assert!(r.failure().is_none(), "code 0 alongside stats is no failure");
        assert_eq!(
            stats_line("AbCdEf", r.stats.as_ref().unwrap()),
            "session AbCdEf: report end chrome/windows hls.js buffers=separate frags=80 med=4100k p10=900k \
             slowest=9800ms est=3800k dropped=3/5400 stalls=7 (1 frozen) 18400ms \
             [at=512s frozen 4200ms v=12.0 a=11.8; at=900s wait ms=? v=0.0 a=0.2; +5 more] \
             errors=bufferStalledError x5,bufferNudgeOnStall"
        );
    }

    #[test]
    fn only_a_nonzero_code_or_one_without_stats_is_a_failure() {
        let failure = |body: &[u8]| parse(body).unwrap().failure().map(|(c, m)| (c, m.to_string()));
        assert_eq!(failure(br#"{"code":3,"message":"DECODE","stats":{}}"#), Some((3, "DECODE".into())));
        assert_eq!(failure(br#"{"code":0,"message":"x"}"#), Some((0, "x".into())), "the old shape as it was");
        assert_eq!(failure(br#"{"code":0,"message":"playback stats (stall)","stats":{}}"#), None);
    }

    #[test]
    fn stall_totals_stand_without_a_list() {
        let stats = serde_json::json!({"event": "hidden", "stallCount": 2, "stalledMs": 3100});
        assert_eq!(stats_line("AbCdEf", &stats), "session AbCdEf: report hidden ? ? stalls=2 3100ms");
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
