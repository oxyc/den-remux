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

/// Releases whose probe is remembered for a verdict, oldest out first.
const KNOWN_MAX: usize = 512;

/// What opening a release showed of its video, kept without a link or a TTL — a release's file does not change — so
/// `POST /remux/releases` can say a release once opened and ruled out for a player is `no` before anything is opened
/// again. The facts, not a verdict: whether a profile 5 is refused, or an HEVC converted, depends on the player asking.
/// Keyed by `session::known_key`: the title and the release.
#[derive(Default)]
pub struct KnownReleases {
    entries: std::collections::VecDeque<(String, MediaInfo)>,
}

impl KnownReleases {
    pub fn put(&mut self, key: String, info: &MediaInfo) {
        self.entries.retain(|(k, _)| *k != key);
        if self.entries.len() >= KNOWN_MAX {
            self.entries.pop_front();
        }
        // Only what a verdict reads: the keyframe and byte indexes, the audio and the subtitles are the bulk.
        self.entries.push_back((
            key,
            MediaInfo {
                keyframes: Vec::new(),
                byte_index: Vec::new(),
                audio: Vec::new(),
                subtitles: Vec::new(),
                ..info.clone()
            },
        ));
    }

    pub fn get(&self, key: &str) -> Option<MediaInfo> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, i)| i.clone())
    }
}

/// Releases remembered as playing nowhere.
const UNPLAYABLE_MAX: usize = 10_000;
/// How long such a verdict holds. The file does not change; this only lets a wrong verdict — a probe fixed since, a
/// debrid that served something else under a release's name — heal on its own.
pub const UNPLAYABLE_TTL_SECS: u64 = 7 * 24 * 60 * 60;
/// Where the verdicts are kept across restarts, in `SCRATCH_DIR`.
pub const UNPLAYABLE_FILE: &str = "unplayable.json";

#[derive(serde::Serialize, serde::Deserialize)]
struct UnplayableEntry {
    key: String,
    /// Unix seconds when it was found.
    at: u64,
    why: String,
}

/// Releases whose opening showed they play in no browser — their container isn't Matroska or MP4, their video is
/// something nothing here copies or converts, they have no audio — so a session skips them without opening them
/// again. Only what the file itself decides: whatever depends on the moment or the player is never kept. Keyed by
/// `session::unplayable_key`: the title, the release's size and its name.
#[derive(Default)]
pub struct Unplayable {
    entries: HashMap<String, (u64, String)>,
    /// Counts changes, so a write of an older state never replaces a newer one on disk.
    generation: u64,
}

impl Unplayable {
    /// Why the release under `key` plays nowhere, when that was found within `UNPLAYABLE_TTL_SECS` of `now`.
    pub fn get(&self, key: &str, now: u64) -> Option<String> {
        self.entries
            .get(key)
            .filter(|(at, _)| now.saturating_sub(*at) < UNPLAYABLE_TTL_SECS)
            .map(|(_, why)| why.clone())
    }

    /// Remember a release that plays nowhere; past `UNPLAYABLE_MAX` the oldest verdict gives way.
    pub fn put(&mut self, key: String, why: String, now: u64) {
        if !self.entries.contains_key(&key) && self.entries.len() >= UNPLAYABLE_MAX {
            self.entries.retain(|_, (at, _)| now.saturating_sub(*at) < UNPLAYABLE_TTL_SECS);
        }
        if !self.entries.contains_key(&key) && self.entries.len() >= UNPLAYABLE_MAX {
            let oldest = self.entries.iter().min_by_key(|(_, (at, _))| *at).map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                self.entries.remove(&k);
            }
        }
        self.entries.insert(key, (now, why));
        self.generation += 1;
    }

    fn to_json(&self) -> Vec<u8> {
        let list: Vec<UnplayableEntry> = self
            .entries
            .iter()
            .map(|(key, (at, why))| UnplayableEntry { key: key.clone(), at: *at, why: why.clone() })
            .collect();
        serde_json::to_vec(&list).unwrap_or_default()
    }

    /// What `to_json` wrote, less what has expired by `now`, the newest `UNPLAYABLE_MAX` of it.
    fn from_json(bytes: &[u8], now: u64) -> Result<Unplayable, String> {
        let mut list: Vec<UnplayableEntry> = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        list.retain(|e| now.saturating_sub(e.at) < UNPLAYABLE_TTL_SECS);
        list.sort_by_key(|e| std::cmp::Reverse(e.at));
        list.truncate(UNPLAYABLE_MAX);
        Ok(Unplayable { entries: list.into_iter().map(|e| (e.key, (e.at, e.why))).collect(), generation: 0 })
    }
}

/// How long a read from a release's host may sit with no byte arriving, while it is being read.
const SOURCE_READ_IDLE: Duration = Duration::from_secs(30);

/// The client that reads releases: the probe's ranged reads, and a session's ffmpeg through its door (`source`), on
/// one pool, so ffmpeg's reads go over the connection the probe opened. No total timeout: a read of the film runs as
/// long as ffmpeg takes it, paused whenever the player is far enough ahead, so only a connection that won't open and
/// a read that stops arriving — timed only while it is being read — end one.
pub fn source_client(connect: Duration, read_idle: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect)
        .read_timeout(read_idle)
        .user_agent(concat!("den-remux/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("reqwest client")
}

pub struct AppState {
    pub cfg: Config,
    pub http: reqwest::Client,
    /// Reads of the releases themselves (`source_client`).
    pub source_http: reqwest::Client,
    /// For requests to scout: follows no redirect, so the service key cannot be carried off scout's
    /// origin (`scout::resolve` follows them by hand).
    pub scout_http: reqwest::Client,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Ended sessions and the expiry their URLs were signed with.
    tombstones: Mutex<HashMap<String, u64>>,
    /// Sessions being set up: they hold a slot against `MAX_SESSIONS` while their release is probed.
    creating: Mutex<HashMap<String, usize>>,
    /// The replacement each owner has under way, if any (`reserve`): at most one, so an owner holds at most one
    /// session past its share.
    replacements: Mutex<HashMap<String, Replacement>>,
    /// Starts per visitor in the current minute: `(minute, count)`.
    starts: Mutex<HashMap<IpAddr, (u64, u32)>>,
    /// Releases opened lately: their debrid links and probes.
    opened: Mutex<OpenedCache>,
    /// What opened releases showed of their video, for `POST /remux/releases`.
    known: Mutex<KnownReleases>,
    /// Releases that play in no browser, and the generation of them last written to disk.
    unplayable: Mutex<Unplayable>,
    unplayable_written: Mutex<u64>,
    pub scratch_bytes: AtomicU64,
    pub sessions_started: AtomicU64,
    pub jobs_started: AtomicU64,
    pub ffmpeg_ok: AtomicBool,
    pub scratch_ok: AtomicBool,
    /// Can this ffmpeg transcode on the GPU (`check_transcode`), with `MAX_TRANSCODES` above 0?
    pub transcode_ok: AtomicBool,
    /// Sessions transcoding now, against `MAX_TRANSCODES`.
    pub transcodes: Arc<AtomicUsize>,
    /// Each session's loopback door to its release, which its ffmpeg reads through (`source`).
    pub doors: crate::source::Doors,
}

/// A session replacing another of the same owner mid-film, which plays on until this one has started.
struct Replacement {
    /// The session being replaced.
    old: String,
    /// The replacing session, once it is set up.
    new: Option<String>,
}

/// Why `reserve` gave no slot.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The owner's share or the server is full.
    Full,
    /// The session named to be replaced is another owner's.
    NotYours,
    /// The owner already has a replacement under way.
    Pending,
}

/// A held slot against `MAX_SESSIONS`; released when dropped.
pub struct Slot<'a> {
    creating: &'a Mutex<HashMap<String, usize>>,
    replacements: &'a Mutex<HashMap<String, Replacement>>,
    owner: String,
    /// The session the one set up in this slot replaces, where it is a replacement.
    pub replaces: Option<String>,
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
        drop(creating);
        // A replacement that was never set up is no longer under way.
        if let Some(old) = &self.replaces {
            let mut replacements = self.replacements.lock().unwrap_or_else(|e| e.into_inner());
            if replacements.get(&self.owner).is_some_and(|r| &r.old == old && r.new.is_none()) {
                replacements.remove(&self.owner);
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
            source_http: source_client(Duration::from_secs(10), SOURCE_READ_IDLE),
            scout_http: client(reqwest::redirect::Policy::none()),
            sessions: Mutex::new(HashMap::new()),
            tombstones: Mutex::new(HashMap::new()),
            creating: Mutex::new(HashMap::new()),
            replacements: Mutex::new(HashMap::new()),
            starts: Mutex::new(HashMap::new()),
            opened: Mutex::new(OpenedCache::default()),
            known: Mutex::new(KnownReleases::default()),
            unplayable: Mutex::new(Unplayable::default()),
            unplayable_written: Mutex::new(0),
            scratch_bytes: AtomicU64::new(0),
            sessions_started: AtomicU64::new(0),
            jobs_started: AtomicU64::new(0),
            ffmpeg_ok: AtomicBool::new(false),
            scratch_ok: AtomicBool::new(false),
            transcode_ok: AtomicBool::new(false),
            transcodes: Arc::new(AtomicUsize::new(0)),
            doors: Default::default(),
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

    pub fn known(&self) -> MutexGuard<'_, KnownReleases> {
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn opened(&self) -> MutexGuard<'_, OpenedCache> {
        self.opened.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn unplayable(&self) -> MutexGuard<'_, Unplayable> {
        self.unplayable.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take up the verdicts an earlier process kept in scratch. A missing file is a first start; an unreadable one
    /// is said, and replaced by the next verdict.
    pub fn load_unplayable(&self) {
        let path = self.cfg.scratch_dir.join(UNPLAYABLE_FILE);
        let loaded = match std::fs::read(&path) {
            Ok(bytes) => Unplayable::from_json(&bytes, unix_now()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => Err(e.to_string()),
        };
        match loaded {
            Ok(u) => {
                eprintln!("unplayable: {} release(s) remembered from the previous process", u.entries.len());
                *self.unplayable() = u;
            }
            Err(e) => eprintln!("unplayable: {} could not be read: {e}", path.display()),
        }
    }

    /// Remember that the release under `key` plays nowhere, and keep that in scratch, off the runtime's threads.
    pub fn remember_unplayable(self: &Arc<Self>, key: String, why: String) {
        self.unplayable().put(key, why, unix_now());
        let st = self.clone();
        tokio::task::spawn_blocking(move || st.write_unplayable());
    }

    /// Write the verdicts to scratch, whole, through a rename — unless a newer state is already there.
    fn write_unplayable(&self) {
        let mut written = self.unplayable_written.lock().unwrap_or_else(|e| e.into_inner());
        let (generation, json) = {
            let u = self.unplayable();
            (u.generation, u.to_json())
        };
        if generation <= *written {
            return;
        }
        let path = self.cfg.scratch_dir.join(UNPLAYABLE_FILE);
        let tmp = path.with_extension("json.tmp");
        match std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &path)) {
            Ok(()) => *written = generation,
            Err(e) => crate::log_limited("unplayable_write", || {
                format!("unplayable: {} could not be written: {e}", path.display())
            }),
        }
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
    ///
    /// `replaces` names the owner's session a player is replacing mid-film (another track, another release). Past
    /// the share that one would be ended here, before its replacement is even probed — a gap in the picture, and
    /// nothing left to play on when no replacement can be made. So it is kept, one session past the share, until the
    /// replacement has served a segment (`replacement_played`) or `REPLACE_GRACE_SECS` pass without it. One replacement
    /// under way per owner; `MAX_SESSIONS` still counts the kept session, and where it leaves no room for both, the
    /// replaced one gives way at once, as it did before.
    pub async fn reserve(
        self: &Arc<Self>,
        owner: &str,
        share: usize,
        replaces: Option<&str>,
    ) -> Result<Slot<'_>, Refused> {
        let (slot, replaced) = {
            let mut map = self.sessions();
            let mut creating = self.creating.lock().unwrap_or_else(|e| e.into_inner());
            let mut replacements = self.replacements.lock().unwrap_or_else(|e| e.into_inner());
            let pending = creating.get(owner).copied().unwrap_or(0);
            if pending >= share {
                return Err(Refused::Full);
            }
            // A session already gone leaves nothing to keep: an ordinary start.
            let replaces = match replaces.and_then(|sid| map.get(sid)) {
                None => None,
                Some(s) if s.owner != owner => return Err(Refused::NotYours),
                Some(_) if replacements.contains_key(owner) => return Err(Refused::Pending),
                Some(s) => Some(s.sid.clone()),
            };
            let mut mine: Vec<_> =
                map.values().filter(|s| s.owner == owner).map(|s| (s.started, s.sid.clone())).collect();
            mine.sort();
            let others = creating.values().sum::<usize>();
            let excess_at = |share: usize| (mine.len() + pending + 1).saturating_sub(share);
            let fits = |excess: usize| map.len() - excess + others < self.cfg.max_sessions;
            let (excess, replaces) = match replaces {
                Some(old) if fits(excess_at(share + 1)) => (excess_at(share + 1), Some(old)),
                _ => (excess_at(share), None),
            };
            if !fits(excess) {
                return Err(Refused::Full);
            }
            let replaced: Vec<_> =
                mine.into_iter().take(excess).filter_map(|(_, sid)| map.remove(&sid)).collect();
            *creating.entry(owner.to_string()).or_default() += 1;
            if let Some(old) = &replaces {
                replacements.insert(owner.to_string(), Replacement { old: old.clone(), new: None });
            }
            let slot = Slot {
                creating: &self.creating,
                replacements: &self.replacements,
                owner: owner.to_string(),
                replaces,
            };
            (slot, replaced)
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
        Ok(slot)
    }

    /// The replacement set up in `slot` is `new`: it takes over once it serves a segment, and is ended as abandoned if
    /// it has not within `replace_grace`, leaving the session it would have replaced playing.
    pub fn replacement_started(self: &Arc<Self>, slot: &Slot<'_>, new: &str) {
        let Some(old) = &slot.replaces else { return };
        {
            let mut replacements = self.replacements.lock().unwrap_or_else(|e| e.into_inner());
            match replacements.get_mut(&slot.owner) {
                Some(r) if &r.old == old => r.new = Some(new.to_string()),
                // The replaced session ended while this one was set up: nothing is left to take over from.
                _ => return,
            }
        }
        let (st, owner, new) = (self.clone(), slot.owner.clone(), new.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(st.cfg.replace_grace).await;
            let abandoned = {
                let mut replacements = st.replacements.lock().unwrap_or_else(|e| e.into_inner());
                let mine = replacements.get(&owner).is_some_and(|r| r.new.as_deref() == Some(new.as_str()));
                mine && replacements.remove(&owner).is_some()
            };
            if abandoned {
                st.end_session(&new, "replacement never played").await;
            }
        });
    }

    /// `s` served a segment: where it is a replacement under way, the session it replaces ends now.
    pub async fn replacement_played(&self, s: &Session) {
        let old = {
            let mut replacements = self.replacements.lock().unwrap_or_else(|e| e.into_inner());
            match replacements.get(&s.owner) {
                Some(r) if r.new.as_deref() == Some(s.sid.as_str()) => {
                    replacements.remove(&s.owner).map(|r| r.old)
                }
                _ => None,
            }
        };
        if let Some(old) = old {
            self.end_session(&old, "replaced").await;
        }
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
        // Either end of a replacement going means it is no longer under way.
        self.replacements
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, r| r.old != *sid && r.new.as_deref() != Some(sid.as_str()));
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

    /// End every session `owner` has published, as a client's DELETE does; how many there were.
    pub async fn end_owner(&self, owner: &str, why: &str) -> usize {
        let sids: Vec<String> =
            self.sessions().values().filter(|s| s.owner == owner).map(|s| s.sid.clone()).collect();
        let mut ended = 0;
        for sid in sids {
            let Some(s) = self.sessions().remove(&sid) else { continue };
            self.end_removed(s, why).await;
            ended += 1;
        }
        ended
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
            frame_rate: None,
            dolby_vision: None,
            dolby_vision_record_mismatch: false,
            dolby_vision_recordless: false,
            audio: Vec::new(),
            subtitles: Vec::new(),
            keyframes: vec![0.0],
            closed_gops: true,
            byte_index: Vec::new(),
            video_bytes: None,
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

    #[test]
    fn an_unplayable_release_is_remembered_for_a_week_and_boundedly() {
        let mut u = Unplayable::default();
        let t0 = 1_000_000;
        u.put("tt1/11/a.mkv".into(), "video is ap4h".into(), t0);
        assert_eq!(u.get("tt1/11/a.mkv", t0 + UNPLAYABLE_TTL_SECS - 1).as_deref(), Some("video is ap4h"));
        assert!(u.get("tt1/11/a.mkv", t0 + UNPLAYABLE_TTL_SECS).is_none(), "a week on it is tried again");
        assert!(u.get("tt1/12/a.mkv", t0).is_none(), "another size is another release");

        for i in 0..UNPLAYABLE_MAX as u64 {
            u.put(format!("r{i}"), "x".into(), t0 + 1 + i);
        }
        assert_eq!(u.entries.len(), UNPLAYABLE_MAX);
        assert!(u.get("tt1/11/a.mkv", t0 + 2).is_none(), "the oldest gave way");
        assert!(u.get("r0", t0 + 2).is_some());
    }

    #[test]
    fn unplayable_verdicts_survive_a_restart_less_what_expired() {
        let mut u = Unplayable::default();
        u.put("old".into(), "neither".into(), 100);
        u.put("new".into(), "video is ap4h".into(), 100 + UNPLAYABLE_TTL_SECS / 2);
        let back = Unplayable::from_json(&u.to_json(), 100 + UNPLAYABLE_TTL_SECS).expect("reads back");
        assert!(back.get("old", 100 + UNPLAYABLE_TTL_SECS).is_none());
        assert_eq!(back.get("new", 100 + UNPLAYABLE_TTL_SECS).as_deref(), Some("video is ap4h"));
        assert!(Unplayable::from_json(b"not json", 0).is_err());
    }
}
