//! A playback session: one release, its keyframe-cut playlist, and the ffmpeg run producing it.
//!
//! The playlist is fixed at creation. Segments are made on demand: a request for segment N joins the
//! GOP files that cover it, waiting briefly if the running job is about to reach it, or restarting the
//! job at N's keyframe if N is behind the job or far ahead of it. A job that has produced enough ahead
//! of the player is paused with SIGSTOP, and the GOPs behind the player are deleted, so a session costs
//! one idle process and a window of a few segments on disk.
//!
//! There are no timers while nothing is playing: each session has one supervising task, which ticks
//! only while its ffmpeg is actually running and otherwise sleeps until a request or its idle deadline.

use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use hyper::{Response, StatusCode};
use tokio::sync::Notify;

use crate::httputil::{self, Body};
use crate::job::{self, Job, Spec};
use crate::playlist::{self, Segment};
use crate::probe::{MediaInfo, Source, VideoCodec};
use crate::scout;
use crate::state::{unix_now, AppState};

/// How far ahead of the newest request a job may produce before it is paused, in segments.
const AHEAD_SEGMENTS: usize = 4;
/// A request further than this past what the job has produced restarts the job there rather than
/// waiting for it to arrive. Copying runs far faster than real time, so a few segments is a wait of
/// seconds; more is a seek.
const RESTART_GAP_SEGMENTS: usize = 3;
/// How long a segment request waits for ffmpeg before answering 503 with Retry-After.
const SEGMENT_WAIT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(100);
/// How often the supervisor looks at a RUNNING job. A paused one is not looked at at all.
const TICK: Duration = Duration::from_millis(500);
/// Timestamps within this of each other name the same keyframe. Keyframes are at least a few frames
/// apart; ffmpeg's playlist rounds durations to the microsecond.
const SNAP: f64 = 0.05;
/// ffmpeg runs in a row that produced nothing before the session gives up on its source.
const MAX_FAILURES: u32 = 3;
/// How many releases to resolve and probe before giving up on a title.
const MAX_TRIES: usize = 3;
/// A session lives for the film plus this, and never longer than `SESSION_MAX_SECS`.
const SESSION_GRACE_SECS: u64 = 60 * 60;
const SESSION_MAX_SECS: u64 = 6 * 60 * 60;

pub fn seg_index(file: &str) -> Option<usize> {
    let n = file.strip_prefix("seg")?.strip_suffix(".m4s")?;
    (!n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()) && n.len() < 8).then(|| n.parse().ok())?
}

pub fn is_session_file(file: &str) -> bool {
    matches!(file, "master.m3u8" | "media.m3u8" | "init.mp4") || seg_index(file).is_some()
}

pub struct Release {
    pub label: String,
    pub filename: String,
    pub size: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Gop {
    pub job: u32,
    /// Its position within its job's run.
    pub idx: u32,
    pub start: f64,
    pub dur: f64,
    pub path: PathBuf,
    pub size: u64,
}

pub struct Inner {
    job: Option<Job>,
    next_job: u32,
    gops: Vec<Gop>,
    /// Jobs that ran to the end of the file, so their last GOP ends the last segment.
    finished: Vec<u32>,
    init: Option<PathBuf>,
    last_seen: Instant,
    /// The newest segment asked for: production follows it.
    want: usize,
    ended: bool,
    bytes: u64,
    failures: u32,
    /// The debrid link ffmpeg reads — scout's play URL, resolved — and whether it has been re-fetched.
    input: String,
    reresolved: bool,
}

pub struct Session {
    pub sid: String,
    pub sig: String,
    pub exp: u64,
    pub browser: String,
    pub imdb: String,
    pub dir: PathBuf,
    pub release: Release,
    pub info: MediaInfo,
    pub segments: Vec<Segment>,
    pub master: String,
    pub media: String,
    /// Scout's play URL for the release and the scout it came from, for fetching a fresh debrid link
    /// when the one ffmpeg reads stops working mid-session. Both are secrets.
    play_url: String,
    source: scout::ScoutSource,
    inner: Mutex<Inner>,
    wake: Notify,
}

pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub detail: String,
}

fn api(status: StatusCode, code: &'static str, detail: impl Into<String>) -> ApiError {
    ApiError { status, code, detail: detail.into() }
}

/// The nearest keyframe to `t` when there is one within `SNAP`, else `t`. Keeps a job's running sum of
/// GOP durations pinned to the real keyframes, so rounding cannot drift across a long film.
fn snap(keyframes: &[f64], t: f64) -> f64 {
    let i = keyframes.partition_point(|k| *k < t);
    [i.checked_sub(1), Some(i)]
        .into_iter()
        .flatten()
        .filter_map(|j| keyframes.get(j))
        .copied()
        .filter(|k| (k - t).abs() < SNAP)
        .min_by(|a, b| (a - t).abs().total_cmp(&(b - t).abs()))
        .unwrap_or(t)
}

/// The segment playing at `t`.
fn seg_at(segments: &[Segment], t: f64) -> usize {
    segments.partition_point(|s| s.start <= t + SNAP).saturating_sub(1)
}

/// The GOPs that make up `seg`, in order, when they are all on disk: a GOP starting on the segment's
/// first keyframe, then its job's following GOPs until the segment's end is covered — or until the
/// job's last GOP, when that job ran to the end of the file.
pub fn ready_chain(gops: &[Gop], finished: &[u32], seg: Segment, first: bool) -> Option<Vec<usize>> {
    'candidates: for (i, g) in gops.iter().enumerate() {
        if (g.start - seg.start).abs() >= SNAP && !(first && g.start < seg.end) {
            continue;
        }
        let mut chain = vec![i];
        let mut cur = g;
        loop {
            if cur.start + cur.dur >= seg.end - SNAP {
                return Some(chain);
            }
            match gops.iter().position(|n| n.job == cur.job && n.idx == cur.idx + 1) {
                Some(j) => {
                    chain.push(j);
                    cur = &gops[j];
                }
                None if finished.contains(&cur.job) && seg.end - cur.start < 1e9 && is_last(gops, cur) => {
                    return Some(chain);
                }
                None => continue 'candidates,
            }
        }
    }
    None
}

/// Is `g` the last GOP its job will ever write? Only asked of finished jobs, whose GOPs are all read.
fn is_last(gops: &[Gop], g: &Gop) -> bool {
    !gops.iter().any(|o| o.job == g.job && o.idx > g.idx)
}

impl Session {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn short(&self) -> &str {
        &self.sid[..6]
    }

    /// The ffmpeg this session is running, if any. Tests use it to prove the process is gone.
    pub fn job_pid(&self) -> Option<u32> {
        self.lock().job.as_ref().filter(|j| j.exit.is_none()).map(|j| j.pid)
    }

    pub fn touch(&self) {
        self.lock().last_seen = Instant::now();
    }

    /// Take what the job has finished since the last look: its exit, and the GOPs in its playlist.
    fn refresh(&self, i: &mut Inner, st: &AppState) {
        let Inner { job, gops, finished, init, bytes, failures, input, .. } = i;
        let Some(job) = job.as_mut() else { return };
        let newly_exited = job.exit.is_none() && job.poll_exit().is_some();
        // After the exit check, so everything written before the exit is read.
        let text = std::fs::read_to_string(job.dir.join("gops.m3u8")).unwrap_or_default();
        for (name, dur) in job::parse_gops(&text).into_iter().skip(job.read) {
            let start = snap(&self.info.keyframes, job.next_start);
            job.next_start = start + dur;
            job.read += 1;
            let path = job.dir.join(&name);
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            gops.push(Gop { job: job.id, idx: (job.read - 1) as u32, start, dur, path, size });
            *bytes += size;
            st.scratch_bytes.fetch_add(size, Relaxed);
            *failures = 0;
            if init.is_none() {
                // Every run writes a byte-identical init (see job.rs), so the first one serves all.
                let dst = self.dir.join("init.mp4");
                if std::fs::copy(job.dir.join("init.mp4"), &dst).is_ok() {
                    *init = Some(dst);
                }
            }
        }
        gops.sort_by(|a, b| a.start.total_cmp(&b.start));
        if newly_exited {
            if job.exit == Some(true) {
                finished.push(job.id);
            } else {
                if job.read == 0 {
                    *failures += 1;
                }
                if let Some(tail) = job.take_stderr() {
                    let (sid, start) = (self.short().to_string(), job.start);
                    let secrets = [input.clone(), self.play_url.clone(), self.source.base.clone()];
                    tokio::spawn(async move {
                        let tail = tail.await.unwrap_or_default();
                        let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
                        let tail = crate::redact::scrub(tail.trim(), &refs);
                        eprintln!(
                            "session {sid}: ffmpeg from {start:.3}s failed: {}",
                            tail.lines().last().unwrap_or("(no output)")
                        );
                    });
                }
            }
        }
    }

    fn ready(&self, i: &Inner, n: usize) -> Option<Vec<PathBuf>> {
        let chain = ready_chain(&i.gops, &i.finished, self.segments[n], n == 0)?;
        Some(chain.into_iter().map(|k| i.gops[k].path.clone()).collect())
    }

    /// Where a job has to start to produce segment `n`: `n`'s own keyframe, unless the next keyframe
    /// is too close behind it to land on reliably (see `job::MIN_SEEK_GAP`), in which case an earlier
    /// segment's. Returns the keyframe and the `-ss` for it.
    fn restart_point(&self, n: usize) -> (f64, Option<f64>) {
        let kfs = &self.info.keyframes;
        for m in (1..=n).rev() {
            let k = self.segments[m].start;
            let next = kfs.iter().copied().find(|x| *x > k + 1e-6).unwrap_or(self.info.duration);
            if next - k >= job::MIN_SEEK_GAP {
                return (k, job::seek_for(k));
            }
        }
        (kfs.first().copied().unwrap_or(0.0), None)
    }

    /// Make sure a job is heading for segment `n`: leave a running job that will reach it soon, start
    /// one at `n` otherwise. `Err` when the source has failed too often to try again.
    fn ensure_job(&self, i: &mut Inner, n: usize, st: &AppState) -> Result<(), ()> {
        let seg = self.segments[n];
        let keep = i.job.as_ref().is_some_and(|j| {
            j.exit.is_none()
                && j.start <= seg.start + SNAP
                && n <= seg_at(&self.segments, j.next_start) + RESTART_GAP_SEGMENTS
        });
        if keep {
            return Ok(());
        }
        // A run failed without output: the next one waits for `reresolve_if_needed` to fetch a fresh link.
        if i.failures > 0 && !i.reresolved {
            return Ok(());
        }
        if i.failures >= MAX_FAILURES {
            return Err(());
        }
        let (start, seek) = self.restart_point(n);
        let id = i.next_job;
        i.next_job += 1;
        let dir = self.dir.join(format!("j{id}"));
        let input = i.input.clone();
        let video = job::Video::Copy { hevc: self.info.video == VideoCodec::Hevc };
        let spec = Spec { input: &input, seek, video, audio: 0, dir: &dir };
        match Job::spawn(&st.cfg.ffmpeg, id, start, &spec) {
            Ok(new) => {
                if let Some(old) = i.job.replace(new) {
                    tokio::spawn(old.stop());
                }
                st.jobs_started.fetch_add(1, Relaxed);
                Ok(())
            }
            Err(e) => {
                crate::log_limited("ffmpeg_spawn", || {
                    format!("session {}: ffmpeg would not start: {e}", self.short())
                });
                i.failures += 1;
                Err(())
            }
        }
    }

    /// Pause the job once it is `AHEAD_SEGMENTS` past the newest request — or, once that request's
    /// segment is done, while scratch is over its cap — and resume it when the player catches up.
    fn gate(&self, i: &mut Inner, st: &AppState) {
        let last = self.segments.len() - 1;
        let limit = self.segments[(i.want + AHEAD_SEGMENTS).min(last)].end;
        let wanted_done = self.segments[i.want.min(last)].end;
        let over_cap = st.scratch_bytes.load(Relaxed) > st.cfg.scratch_max_bytes;
        if let Some(j) = i.job.as_mut().filter(|j| j.exit.is_none()) {
            let ahead = j.next_start >= limit - SNAP;
            if ahead || (over_cap && j.next_start >= wanted_done - SNAP) {
                j.pause();
            } else {
                j.resume();
            }
        }
    }

    /// Delete the GOPs outside the window around segment `n`: everything before the previous segment,
    /// and anything a dead job left far ahead.
    fn prune(&self, i: &mut Inner, n: usize, st: &AppState) {
        let last = self.segments.len() - 1;
        let lo = self.segments[n.saturating_sub(1)].start - SNAP;
        let hi = self.segments[(n + AHEAD_SEGMENTS + 1).min(last)].end + SNAP;
        let current = i.job.as_ref().map(|j| j.id);
        let mut freed = 0;
        i.gops.retain(|g| {
            let keep = g.start + g.dur > lo && (g.start < hi || Some(g.job) == current);
            if !keep {
                let _ = std::fs::remove_file(&g.path);
                freed += g.size;
            }
            keep
        });
        i.bytes -= freed;
        st.scratch_bytes.fetch_sub(freed, Relaxed);
    }

    /// After a run that produced nothing, fetch a fresh link through scout, once: a debrid link can
    /// expire inside a long session. Done here rather than by handing ffmpeg scout's play URL, because
    /// following scout's redirect is where the service key has to be kept away from the debrid.
    async fn reresolve_if_needed(&self, st: &AppState) {
        {
            let mut i = self.lock();
            if i.ended || i.failures == 0 || i.reresolved {
                return;
            }
            i.reresolved = true;
        }
        match scout::resolve(&st.scout_http, &self.play_url, &self.source).await {
            Ok(r) => self.lock().input = r.url,
            Err(e) => eprintln!(
                "session {}: re-resolving the release failed: {}",
                self.short(),
                crate::redact::scrub(&e, &[&self.source.base])
            ),
        }
    }

    pub async fn serve_segment(&self, st: &AppState, n: usize, head: bool) -> Response<Body> {
        let deadline = Instant::now() + SEGMENT_WAIT;
        loop {
            self.reresolve_if_needed(st).await;
            let paths = {
                let mut i = self.lock();
                if i.ended {
                    return gone();
                }
                i.last_seen = Instant::now();
                i.want = n;
                self.refresh(&mut i, st);
                let ready = self.ready(&i, n);
                if ready.is_none() && self.ensure_job(&mut i, n, st).is_err() {
                    return source_failed();
                }
                self.gate(&mut i, st);
                ready
            };
            self.wake.notify_one();
            if let Some(paths) = paths {
                // Opened before pruning, so a GOP deleted from the window keeps serving this body.
                if let Ok(Ok(parts)) = tokio::task::spawn_blocking(move || open_parts(&paths)).await {
                    {
                        let mut i = self.lock();
                        self.prune(&mut i, n, st);
                        self.gate(&mut i, st);
                    }
                    return media_response("video/mp4", parts, head);
                }
            }
            if Instant::now() >= deadline {
                return busy();
            }
            tokio::time::sleep(POLL).await;
        }
    }

    pub async fn serve_init(&self, st: &AppState, head: bool) -> Response<Body> {
        let deadline = Instant::now() + SEGMENT_WAIT;
        loop {
            self.reresolve_if_needed(st).await;
            let init = {
                let mut i = self.lock();
                if i.ended {
                    return gone();
                }
                i.last_seen = Instant::now();
                self.refresh(&mut i, st);
                if i.init.is_none() {
                    let want = i.want;
                    if self.ensure_job(&mut i, want, st).is_err() {
                        return source_failed();
                    }
                }
                self.gate(&mut i, st);
                i.init.clone()
            };
            self.wake.notify_one();
            if let Some(path) = init {
                if let Ok(Ok(parts)) = tokio::task::spawn_blocking(move || open_parts(&[path])).await {
                    return media_response("video/mp4", parts, head);
                }
            }
            if Instant::now() >= deadline {
                return busy();
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Stop the job, reap it, and delete the session's scratch. `false` if it had already ended.
    pub async fn end(&self, st: &AppState) -> bool {
        let job = {
            let mut i = self.lock();
            if i.ended {
                return false;
            }
            i.ended = true;
            st.scratch_bytes.fetch_sub(i.bytes, Relaxed);
            i.bytes = 0;
            i.gops.clear();
            i.job.take()
        };
        if let Some(j) = job {
            j.stop().await;
        }
        let dir = self.dir.clone();
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir)).await;
        self.wake.notify_one();
        true
    }
}

/// Open each GOP file, positioned past the `styp` box of every file but the first: the joined segment
/// is one segment, and a segment type box belongs only at its start.
fn open_parts(paths: &[PathBuf]) -> std::io::Result<Vec<(std::fs::File, u64)>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut out = Vec::with_capacity(paths.len());
    for (k, p) in paths.iter().enumerate() {
        let mut f = std::fs::File::open(p)?;
        let mut len = f.metadata()?.len();
        if k > 0 {
            let mut hdr = [0u8; 8];
            f.read_exact(&mut hdr)?;
            let size = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
            if &hdr[4..8] == b"styp" && size >= 8 && size <= len {
                len -= size;
                f.seek(SeekFrom::Start(size))?;
            } else {
                f.seek(SeekFrom::Start(0))?;
            }
        }
        out.push((f, len));
    }
    Ok(out)
}

fn media_response(content_type: &str, parts: Vec<(std::fs::File, u64)>, head: bool) -> Response<Body> {
    let len: u64 = parts.iter().map(|(_, l)| l).sum();
    let body = if head { httputil::full("") } else { httputil::files_body(parts) };
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", content_type)
        .header("content-length", len)
        // Only ever this session's bytes, and only for as long as the session lives.
        .header("cache-control", "private, max-age=600")
        .body(body)
        .unwrap()
}

pub fn gone() -> Response<Body> {
    httputil::error(StatusCode::GONE, "session_ended", "This session has ended; start a new one.")
}

fn busy() -> Response<Body> {
    httputil::json(
        StatusCode::SERVICE_UNAVAILABLE,
        &serde_json::json!({"error": "not_ready", "detail": "The segment is still being made."}),
        &[("retry-after", "2"), ("cache-control", "no-store")],
    )
}

fn source_failed() -> Response<Body> {
    httputil::error(
        StatusCode::BAD_GATEWAY,
        "source_failed",
        "The release stopped answering; start a new session.",
    )
}

/// Try one candidate: follow its play URL, read the head, and probe it.
async fn open(
    st: &AppState,
    src: &scout::ScoutSource,
    s: &scout::Stream,
) -> Result<(scout::Resolved, MediaInfo), String> {
    let r = scout::resolve(&st.scout_http, &s.url, src).await?;
    let info = crate::probe::probe(&Source::Http { client: &st.http, url: &r.url }, &r.head)
        .await
        .map_err(|e| e.to_string())?;
    if let VideoCodec::Other(c) = &info.video {
        return Err(format!("video is {c}, which needs a re-encode"));
    }
    if info.audio.is_empty() {
        return Err("no audio track".into());
    }
    Ok((r, info))
}

/// `POST /remux/session`: pick and probe a release of `imdb`, and set up its session.
///
/// `scout` is the primary path: the scout install from the web app's library (full, or scope=availability),
/// accepted only on a `SCOUT_ORIGINS` origin and presented with `REMUX_SCOUT_KEY`. Without it this service's own
/// `SCOUT_INSTALL_URL` is used, which is how the MVP can be driven by hand.
pub async fn create(
    st: &Arc<AppState>,
    browser: String,
    imdb: &str,
    filename: Option<&str>,
    scout: Option<&str>,
) -> Result<Arc<Session>, ApiError> {
    let base = match scout {
        Some(url) => scout::validate_scoped(url, &st.cfg.scout_origins).map_err(|why| {
            api(StatusCode::BAD_REQUEST, "bad_scout", format!("The scout URL was refused: {why}."))
        })?,
        None => st.cfg.scout_install_url.clone().ok_or_else(|| {
            api(
                StatusCode::SERVICE_UNAVAILABLE,
                "scout_unconfigured",
                "No scout URL in the request, and no SCOUT_INSTALL_URL.",
            )
        })?,
    };
    let source = scout::ScoutSource { base, key: st.cfg.scout_key.clone() };
    // A browser starting another title is done with the one it was watching; making it wait out the
    // idle timer for its own old session would turn every change of mind into a 429.
    let mine: Vec<String> =
        st.sessions().values().filter(|s| s.browser == browser).map(|s| s.sid.clone()).collect();
    for sid in mine {
        st.end_session(&sid, "replaced").await;
    }
    let Some(_slot) = st.reserve() else {
        return Err(api(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_sessions",
            format!("{} sessions are already playing.", st.cfg.max_sessions),
        ));
    };
    let secrets = [source.base.as_str()];
    let list = scout::list(&st.scout_http, &source, imdb).await.map_err(|e| {
        crate::log_limited("scout_list", || format!("scout: {}", crate::redact::scrub(&e, &secrets)));
        api(StatusCode::BAD_GATEWAY, "scout_unavailable", "Could not list releases.")
    })?;
    let candidates = scout::candidates(&list, filename);
    if candidates.is_empty() {
        return Err(api(
            StatusCode::NOT_FOUND,
            "no_release",
            "No cached release in a codec this service can remux.",
        ));
    }
    let mut chosen = None;
    for c in candidates.iter().take(MAX_TRIES) {
        match open(st, &source, c).await {
            Ok(found) => {
                chosen = Some((c, found));
                break;
            }
            Err(why) => eprintln!(
                "session: {imdb} skipped \"{}\": {}",
                c.attributes.label,
                crate::redact::scrub(&why, &secrets)
            ),
        }
    }
    let Some((c, (resolved, info))) = chosen else {
        return Err(api(StatusCode::NOT_FOUND, "no_playable_release", "No cached release could be opened."));
    };

    let sid = crate::auth::random_id();
    let exp = unix_now() + (info.duration.ceil() as u64 + SESSION_GRACE_SECS).min(SESSION_MAX_SECS);
    let sig = crate::auth::url_sig(&st.cfg.url_key, &sid, exp);
    let dir = st.cfg.scratch_dir.join(format!("s-{sid}"));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        st.scratch_ok.store(false, Relaxed);
        eprintln!("session: scratch {} is unusable: {e}", st.cfg.scratch_dir.display());
        return Err(api(
            StatusCode::SERVICE_UNAVAILABLE,
            "scratch_unavailable",
            "Scratch space is unavailable.",
        ));
    }
    st.scratch_ok.store(true, Relaxed);
    let segments = playlist::segments(&info.keyframes, info.duration, playlist::TARGET_SECS);
    let codecs = info.codecs.clone().unwrap_or_else(|| {
        // A file without a codec configuration record; name the commonest profile rather than none.
        match info.video {
            VideoCodec::Hevc => "hvc1.1.6.L150.90",
            _ => "avc1.640028",
        }
        .to_string()
    });
    let size = resolved.size.or(c.attributes.size_bytes);
    let (peak, avg) = playlist::bandwidth(size, info.duration);
    let session = Arc::new(Session {
        master: playlist::master(&codecs, peak, avg, Some((info.width, info.height))),
        media: playlist::media(&segments),
        sid: sid.clone(),
        sig,
        exp,
        browser,
        imdb: imdb.to_string(),
        dir,
        release: Release { label: c.attributes.label.clone(), filename: c.filename().to_string(), size },
        segments,
        play_url: c.url.clone(),
        source,
        info,
        inner: Mutex::new(Inner {
            job: None,
            next_job: 0,
            gops: Vec::new(),
            finished: Vec::new(),
            init: None,
            last_seen: Instant::now(),
            want: 0,
            ended: false,
            bytes: 0,
            failures: 0,
            input: resolved.url,
            reresolved: false,
        }),
        wake: Notify::new(),
    });
    st.insert_session(session.clone());
    st.sessions_started.fetch_add(1, Relaxed);
    eprintln!(
        "session {}: {imdb} \"{}\" ({}, {:?}, {:.0}s, {} keyframes, {} segments)",
        session.short(),
        session.release.label,
        session.info.container,
        session.info.video,
        session.info.duration,
        session.info.keyframes.len(),
        session.segments.len()
    );
    tokio::spawn(supervise(st.clone(), session.clone()));
    Ok(session)
}

/// The one task per session: tick while ffmpeg runs, sleep otherwise, and end the session when it
/// goes idle or expires.
async fn supervise(st: Arc<AppState>, s: Arc<Session>) {
    loop {
        let (running, idle_at) = {
            let mut i = s.lock();
            if i.ended {
                return;
            }
            s.refresh(&mut i, &st);
            s.gate(&mut i, &st);
            (
                i.job.as_ref().is_some_and(|j| j.exit.is_none() && !j.stopped),
                i.last_seen + st.cfg.session_idle,
            )
        };
        let now = unix_now();
        if Instant::now() >= idle_at {
            st.end_session(&s.sid, "idle").await;
            return;
        }
        if now >= s.exp {
            st.end_session(&s.sid, "expired").await;
            return;
        }
        let exp_at = Instant::now() + Duration::from_secs(s.exp - now);
        let wait = if running { TICK } else { idle_at.min(exp_at).saturating_duration_since(Instant::now()) };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = s.wake.notified() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gop(job: u32, idx: u32, start: f64, dur: f64) -> Gop {
        Gop { job, idx, start, dur, path: PathBuf::from(format!("j{job}/g{idx}")), size: 1 }
    }

    const SEG: Segment = Segment { start: 8.0, end: 13.0 };

    #[test]
    fn a_segment_is_ready_once_its_gops_cover_it() {
        // 8 → 10.5 → 13, as h264.mkv's keyframes fall.
        let gops = [gop(1, 0, 0.0, 2.5), gop(1, 3, 8.0, 2.5), gop(1, 4, 10.5, 2.5), gop(1, 5, 13.0, 3.0)];
        assert_eq!(ready_chain(&gops, &[], SEG, false), Some(vec![1, 2]));
        // Only the first half written so far.
        assert_eq!(ready_chain(&gops[..2], &[], SEG, false), None);
    }

    #[test]
    fn gops_are_never_joined_across_jobs() {
        // Job 1 made 8 → 10.5 and was replaced; job 2 started at 10.5. Their GOPs are compatible, but a
        // chain has to come from one run to be known to be contiguous.
        let gops = [gop(1, 0, 8.0, 2.5), gop(2, 0, 10.5, 2.5)];
        assert_eq!(ready_chain(&gops, &[], SEG, false), None);
        let gops = [gop(1, 0, 8.0, 2.5), gop(2, 0, 10.5, 2.5), gop(3, 0, 8.0, 5.0)];
        assert_eq!(ready_chain(&gops, &[], SEG, false), Some(vec![2]), "a complete chain from another job");
    }

    #[test]
    fn the_last_segment_ends_with_its_jobs_last_gop() {
        // The container's duration runs a little past the last video frame.
        let last = Segment { start: 24.0, end: 30.021 };
        let gops = [gop(1, 0, 24.0, 3.5), gop(1, 1, 27.5, 2.46)];
        assert_eq!(ready_chain(&gops, &[], last, false), None, "still running: more may come");
        assert_eq!(ready_chain(&gops, &[1], last, false), Some(vec![0, 1]));
    }

    #[test]
    fn snapping_pins_drift_to_the_real_keyframes() {
        let kf = [0.0, 2.5, 5.0];
        assert_eq!(snap(&kf, 2.500_004), 2.5);
        assert_eq!(snap(&kf, 4.99), 5.0);
        assert_eq!(snap(&kf, 3.7), 3.7, "a keyframe the index does not list keeps its own time");
    }

    #[test]
    fn session_file_names_are_a_closed_set() {
        assert_eq!(seg_index("seg0.m4s"), Some(0));
        assert_eq!(seg_index("seg12.m4s"), Some(12));
        assert_eq!(seg_index("seg+1.m4s"), None);
        assert_eq!(seg_index("seg.m4s"), None);
        assert_eq!(seg_index("seg99999999.m4s"), None);
        assert!(is_session_file("init.mp4") && is_session_file("media.m3u8"));
        assert!(!is_session_file("../gops.m3u8"));
    }

    #[test]
    fn seg_at_finds_the_segment_playing() {
        let segs =
            playlist::segments(&[0.0, 2.5, 5.0, 8.0, 10.5, 13.0, 16.0, 19.5, 22.0, 24.0, 27.5], 30.021, 6.0);
        assert_eq!(seg_at(&segs, 0.0), 0);
        assert_eq!(seg_at(&segs, 7.9), 0);
        assert_eq!(seg_at(&segs, 7.99), 1, "within SNAP of a boundary is at the boundary");
        assert_eq!(seg_at(&segs, 8.0), 1);
        assert_eq!(seg_at(&segs, 29.0), 4);
    }
}
