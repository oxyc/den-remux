//! Shared application state: the configuration, the one HTTP client, the live sessions and the
//! counters `/health` and `/metrics` read.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::probe::MediaInfo;
use crate::scout::Resolved;
use crate::session::Session;

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// How many ended sessions are remembered, so their URLs answer 410 rather than 404.
const TOMBSTONES_MAX: usize = 1024;

/// Logins and new sessions one visitor may start a minute: far above a person trying titles, far below
/// what it takes to keep every slot churning or to guess at a browser key.
pub const STARTS_PER_MINUTE: u32 = 10;

/// Releases remembered as opened. Each is a link, a subtitle-hash's worth of head and a keyframe index: a few
/// hundred KB at most.
const OPENED_MAX: usize = 8;
/// How long an opened release is remembered: well inside a debrid link's life, which is hours. A link that
/// stops working anyway is fetched again by the session's expired-link refetch, which also forgets it here.
const OPENED_TTL: Duration = Duration::from_secs(10 * 60);

/// Releases opened lately, so opening one again — another of its audio tracks, or the same title a second
/// time — neither fetches a fresh debrid link through scout nor probes the file again. Keyed by
/// `session::opened_key`: the install that listed it, and the release.
#[derive(Default)]
pub struct OpenedCache {
    entries: Vec<Opened>,
}

struct Opened {
    key: String,
    at: Instant,
    resolved: Resolved,
    info: MediaInfo,
}

impl OpenedCache {
    /// The release opened under `key`, if it was within `OPENED_TTL` of `now`.
    pub fn get(&mut self, key: &str, now: Instant) -> Option<(Resolved, MediaInfo)> {
        self.entries.retain(|e| now.duration_since(e.at) < OPENED_TTL);
        self.entries.iter().find(|e| e.key == key).map(|e| (e.resolved.clone(), e.info.clone()))
    }

    /// Remember an opened release, the oldest giving way past `OPENED_MAX`. Of the head only what a subtitle's
    /// hash reads is kept; the probe that needed the rest is done.
    pub fn put(&mut self, key: String, resolved: &Resolved, info: &MediaInfo, now: Instant) {
        self.entries.retain(|e| e.key != key && now.duration_since(e.at) < OPENED_TTL);
        if self.entries.len() >= OPENED_MAX {
            self.entries.remove(0);
        }
        let head = resolved.head[..resolved.head.len().min(crate::subs::HASH_CHUNK as usize)].to_vec();
        let resolved = Resolved { url: resolved.url.clone(), head, size: resolved.size };
        self.entries.push(Opened { key, at: now, resolved, info: info.clone() });
    }

    /// Drop a release whose link stopped working, so the next session fetches a fresh one.
    pub fn forget(&mut self, key: &str) {
        self.entries.retain(|e| e.key != key);
    }
}

pub struct AppState {
    pub cfg: Config,
    pub http: reqwest::Client,
    /// For requests to scout: follows no redirect, so the service key cannot be carried off scout's
    /// origin (`scout::resolve` follows them by hand).
    pub scout_http: reqwest::Client,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Ended sessions and the expiry their URLs were signed with.
    tombstones: Mutex<HashMap<String, u64>>,
    /// Sessions being set up: they hold a slot against `MAX_SESSIONS` while their release is probed.
    creating: Mutex<HashMap<String, usize>>,
    /// Starts per visitor in the current minute: `(minute, count)`.
    starts: Mutex<HashMap<IpAddr, (u64, u32)>>,
    /// Releases opened lately: their debrid links and probes.
    opened: Mutex<OpenedCache>,
    pub scratch_bytes: AtomicU64,
    pub sessions_started: AtomicU64,
    pub jobs_started: AtomicU64,
    pub ffmpeg_ok: AtomicBool,
    pub scratch_ok: AtomicBool,
    /// Can this ffmpeg transcode on the GPU (`check_transcode`), with `MAX_TRANSCODES` above 0?
    pub transcode_ok: AtomicBool,
    /// Sessions transcoding now, against `MAX_TRANSCODES`.
    pub transcodes: Arc<AtomicUsize>,
}

/// A held slot against `MAX_SESSIONS`; released when dropped.
pub struct Slot<'a> {
    creating: &'a Mutex<HashMap<String, usize>>,
    owner: String,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut creating = self.creating.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = creating.get_mut(&self.owner) {
            *count -= 1;
            if *count == 0 {
                creating.remove(&self.owner);
            }
        }
    }
}

/// A held transcode against `MAX_TRANSCODES`, owned by its session; released when the session is dropped.
pub struct TranscodeSlot(Arc<AtomicUsize>);

impl Drop for TranscodeSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Relaxed);
    }
}

impl AppState {
    pub fn new(cfg: Config) -> Arc<AppState> {
        let client = |redirects: reqwest::redirect::Policy| {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                // Scout may spend its whole scrape timeout on a slow indexer before it answers a list.
                .timeout(Duration::from_secs(60))
                .user_agent(concat!("den-remux/", env!("CARGO_PKG_VERSION")))
                .redirect(redirects)
                .build()
                .expect("reqwest client")
        };
        Arc::new(AppState {
            cfg,
            http: client(reqwest::redirect::Policy::default()),
            scout_http: client(reqwest::redirect::Policy::none()),
            sessions: Mutex::new(HashMap::new()),
            tombstones: Mutex::new(HashMap::new()),
            creating: Mutex::new(HashMap::new()),
            starts: Mutex::new(HashMap::new()),
            opened: Mutex::new(OpenedCache::default()),
            scratch_bytes: AtomicU64::new(0),
            sessions_started: AtomicU64::new(0),
            jobs_started: AtomicU64::new(0),
            ffmpeg_ok: AtomicBool::new(false),
            scratch_ok: AtomicBool::new(false),
            transcode_ok: AtomicBool::new(false),
            transcodes: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// A transcode for a new session, or `None` when transcoding is off or `MAX_TRANSCODES` are running.
    /// The GPU is shared with the camera stack and Incus gives no GPU priority, so this cap is the only one.
    pub fn reserve_transcode(&self) -> Option<TranscodeSlot> {
        if !self.transcode_ok.load(Relaxed) {
            return None;
        }
        let max = self.cfg.max_transcodes;
        self.transcodes.fetch_update(Relaxed, Relaxed, |n| (n < max).then_some(n + 1)).ok()?;
        Some(TranscodeSlot(self.transcodes.clone()))
    }

    pub fn opened(&self) -> MutexGuard<'_, OpenedCache> {
        self.opened.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn sessions(&self) -> MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn session(&self, sid: &str) -> Option<Arc<Session>> {
        self.sessions().get(sid).cloned()
    }

    /// Reserve both the owner's share and the global cap before any upstream work. Published sessions
    /// may be replaced; another request's reservation cannot. Detaching replacements and counting the
    /// reservation happen under the same locks, so concurrent starts cannot spend the same free slot.
    pub async fn reserve(self: &Arc<Self>, owner: &str, share: usize) -> Option<Slot<'_>> {
        let (slot, replaced) = {
            let mut map = self.sessions();
            let mut creating = self.creating.lock().unwrap_or_else(|e| e.into_inner());
            let pending = creating.get(owner).copied().unwrap_or(0);
            if pending >= share {
                return None;
            }
            let mut mine: Vec<_> =
                map.values().filter(|s| s.owner == owner).map(|s| (s.started, s.sid.clone())).collect();
            mine.sort();
            let excess = (mine.len() + pending + 1).saturating_sub(share);
            if map.len() - excess + creating.values().sum::<usize>() >= self.cfg.max_sessions {
                return None;
            }
            let replaced: Vec<_> =
                mine.into_iter().take(excess).filter_map(|(_, sid)| map.remove(&sid)).collect();
            *creating.entry(owner.to_string()).or_default() += 1;
            (Slot { creating: &self.creating, owner: owner.to_string() }, replaced)
        };
        if !replaced.is_empty() {
            let state = self.clone();
            // Cancellation of the new request must not strand a detached old session or its ffmpeg.
            let cleanup = tokio::spawn(async move {
                for old in replaced {
                    state.end_removed(old, "replaced").await;
                }
            });
            let _ = cleanup.await;
        }
        Some(slot)
    }

    /// Count a login or a new session for `visitor`: false once it has had `STARTS_PER_MINUTE` this
    /// minute. No address means no limit — only a test's request comes without one.
    pub fn admit(&self, visitor: Option<IpAddr>) -> bool {
        let Some(ip) = visitor else { return true };
        let minute = unix_now() / 60;
        let mut starts = self.starts.lock().unwrap_or_else(|e| e.into_inner());
        starts.retain(|_, (m, _)| *m == minute);
        let (_, count) = starts.entry(ip).or_insert((minute, 0));
        *count += 1;
        *count <= STARTS_PER_MINUTE
    }

    pub fn insert_session(&self, s: Arc<Session>) {
        self.sessions().insert(s.sid.clone(), s);
    }

    /// The expiry an ended session's URLs were signed with.
    pub fn tombstone(&self, sid: &str) -> Option<u64> {
        self.tombstones.lock().unwrap_or_else(|e| e.into_inner()).get(sid).copied()
    }

    pub async fn end_session(&self, sid: &str, why: &str) {
        let Some(s) = self.sessions().remove(sid) else { return };
        self.end_removed(s, why).await;
    }

    async fn end_removed(&self, s: Arc<Session>, why: &str) {
        let sid = &s.sid;
        {
            let mut t = self.tombstones.lock().unwrap_or_else(|e| e.into_inner());
            let now = unix_now();
            t.retain(|_, exp| *exp > now);
            if t.len() < TOMBSTONES_MAX {
                t.insert(sid.to_string(), s.exp);
            }
        }
        if s.end(self).await {
            eprintln!("session {}: ended ({why})", &sid[..6]);
        }
    }

    pub async fn end_all(&self, why: &str) {
        let sids: Vec<String> = self.sessions().keys().cloned().collect();
        for sid in sids {
            self.end_session(&sid, why).await;
        }
    }
}

/// Remove what a previous process left in scratch. Sessions live in memory only, so every `s-*`
/// directory found at boot belongs to one that no longer exists.
pub fn sweep_scratch(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut removed = 0;
    for e in rd.flatten() {
        if e.file_name().to_string_lossy().starts_with("s-") && std::fs::remove_dir_all(e.path()).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        eprintln!("scratch: removed {removed} session dir(s) left by the previous process");
    }
}

/// Can we write to scratch? Creates it if missing.
pub fn check_scratch(dir: &Path) -> bool {
    let probe = dir.join(".probe");
    std::fs::create_dir_all(dir).is_ok()
        && std::fs::write(&probe, b"ok").is_ok()
        && std::fs::remove_file(&probe).is_ok()
}

/// Does this ffmpeg have what a session needs? The image carries a minimal build, and a component
/// dropped from its configure line would otherwise surface as every session failing.
pub async fn check_ffmpeg(ffmpeg: &str) -> bool {
    reports_all(
        ffmpeg,
        &[
            (&["-hide_banner", "-h", "demuxer=matroska"], "Demuxer matroska"),
            (&["-hide_banner", "-h", "demuxer=mov"], "Demuxer mov"),
            (&["-hide_banner", "-h", "muxer=hls"], "Muxer hls"),
            (&["-hide_banner", "-h", "encoder=aac"], "Encoder aac"),
            (&["-hide_banner", "-protocols"], "https"),
        ],
    )
    .await
}

/// Can sessions transcode: the GPU's device node is here, and this ffmpeg has the VAAPI encoder and
/// filters? Off is normal — a host without the device, or a local ffmpeg.
pub async fn check_transcode(ffmpeg: &str, device: &Path) -> bool {
    if !device.exists() {
        return false;
    }
    reports_all(
        ffmpeg,
        &[
            (&["-hide_banner", "-h", "encoder=h264_vaapi"], "Encoder h264_vaapi"),
            (&["-hide_banner", "-h", "filter=scale_vaapi"], "Filter scale_vaapi"),
            (&["-hide_banner", "-h", "filter=tonemap_vaapi"], "Filter tonemap_vaapi"),
        ],
    )
    .await
}

/// Does `ffmpeg <args>` print `want`, for every pair? Says the first that does not.
async fn reports_all(ffmpeg: &str, checks: &[(&[&str], &str)]) -> bool {
    for (args, want) in checks {
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new(ffmpeg)
                .args(*args)
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await;
        let ok = matches!(&out, Ok(Ok(o)) if String::from_utf8_lossy(&o.stdout).contains(want));
        if !ok {
            eprintln!("ffmpeg check: `{ffmpeg} {}` does not report {want}", args.join(" "));
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opened(url: &str) -> (Resolved, MediaInfo) {
        let info = MediaInfo {
            container: "matroska",
            duration: 1.0,
            video: crate::probe::VideoCodec::H264,
            codecs: None,
            width: 0,
            height: 0,
            hdr: false,
            hlg: false,
            dolby_vision: None,
            audio: Vec::new(),
            keyframes: vec![0.0],
        };
        (Resolved { url: url.into(), head: vec![7; 200_000], size: Some(1) }, info)
    }

    #[test]
    fn an_opened_release_is_remembered_briefly_and_boundedly() {
        let mut cache = OpenedCache::default();
        let t0 = Instant::now();
        let (r, info) = opened("https://debrid/a");
        cache.put("a".into(), &r, &info, t0);
        let (hit, _) = cache.get("a", t0 + Duration::from_secs(60)).expect("remembered");
        assert_eq!(hit.url, "https://debrid/a");
        let hash_part = crate::subs::HASH_CHUNK as usize;
        assert_eq!(hit.head.len(), hash_part, "only the subtitle hash's part of the head");
        assert!(cache.get("b", t0).is_none());
        assert!(cache.get("a", t0 + OPENED_TTL).is_none(), "a link that old is fetched again");

        for i in 0..=OPENED_MAX {
            cache.put(format!("r{i}"), &r, &info, t0);
        }
        assert!(cache.get("r0", t0).is_none(), "the oldest gave way");
        assert!(cache.get(&format!("r{OPENED_MAX}"), t0).is_some());
        cache.forget("r1");
        assert!(cache.get("r1", t0).is_none(), "a dead link is forgotten");
    }
}
