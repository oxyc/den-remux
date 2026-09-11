//! Shared application state: the configuration, the one HTTP client, the live sessions and the
//! counters `/health` and `/metrics` read.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::session::Session;

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// How many ended sessions are remembered, so their URLs answer 410 rather than 404.
const TOMBSTONES_MAX: usize = 1024;

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
    creating: AtomicUsize,
    pub scratch_bytes: AtomicU64,
    pub sessions_started: AtomicU64,
    pub jobs_started: AtomicU64,
    pub ffmpeg_ok: AtomicBool,
    pub scratch_ok: AtomicBool,
}

/// A held slot against `MAX_SESSIONS`; released when dropped.
pub struct Slot<'a>(&'a AtomicUsize);

impl Drop for Slot<'_> {
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
            creating: AtomicUsize::new(0),
            scratch_bytes: AtomicU64::new(0),
            sessions_started: AtomicU64::new(0),
            jobs_started: AtomicU64::new(0),
            ffmpeg_ok: AtomicBool::new(false),
            scratch_ok: AtomicBool::new(false),
        })
    }

    pub fn sessions(&self) -> MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn session(&self, sid: &str) -> Option<Arc<Session>> {
        self.sessions().get(sid).cloned()
    }

    /// A slot for a new session, or `None` when `MAX_SESSIONS` are playing or being set up.
    pub fn reserve(&self) -> Option<Slot<'_>> {
        let map = self.sessions();
        if map.len() + self.creating.load(Relaxed) >= self.cfg.max_sessions {
            return None;
        }
        self.creating.fetch_add(1, Relaxed);
        Some(Slot(&self.creating))
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
    let checks: [(&[&str], &str); 5] = [
        (&["-hide_banner", "-h", "demuxer=matroska"], "Demuxer matroska"),
        (&["-hide_banner", "-h", "demuxer=mov"], "Demuxer mov"),
        (&["-hide_banner", "-h", "muxer=hls"], "Muxer hls"),
        (&["-hide_banner", "-h", "encoder=aac"], "Encoder aac"),
        (&["-hide_banner", "-protocols"], "https"),
    ];
    for (args, want) in checks {
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new(ffmpeg)
                .args(args)
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
