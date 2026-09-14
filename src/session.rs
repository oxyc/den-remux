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

use futures_util::stream::{FuturesOrdered, StreamExt};
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
/// A resume's first job starts at the segment this far before `startAt`, not the one `startAt` falls in: a
/// player may ask for the segment before when the point is near a boundary (hls.js allows a quarter second),
/// and a job already behind that request is kept, where one ahead of it would be restarted.
const START_SLACK: f64 = 1.0;
/// ffmpeg runs in a row that produced nothing before the session gives up on its source.
const MAX_FAILURES: u32 = 3;
/// How many releases to resolve and probe before giving up on a title. Past the first few, each is one debrid
/// link and a handful of ranged reads, so a title whose top releases won't open still finds one further down.
const MAX_TRIES: usize = 12;
/// Releases opened at once while picking. The pick still goes by rank: one that answers sooner waits for those
/// ranked above it.
const PARALLEL_OPENS: usize = 3;
/// No new release is opened once picking has taken this long — about where a person gives up on a play
/// button; those already opening finish.
const PICK_BUDGET: Duration = Duration::from_secs(30);
/// One release's resolve and probe. A debrid slower than this to hand over the head of a file plays badly anyway.
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);
/// A session lives for the film plus this, and never longer than `SESSION_MAX_SECS`.
const SESSION_GRACE_SECS: u64 = 60 * 60;
const SESSION_MAX_SECS: u64 = 6 * 60 * 60;

pub fn seg_index(file: &str) -> Option<usize> {
    let n = file.strip_prefix("seg")?.strip_suffix(".m4s")?;
    (!n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()) && n.len() < 8).then(|| n.parse().ok())?
}

/// `sub<N>.m3u8` or `sub<N>.vtt`: the rendition and whether it is the document.
pub fn sub_file(file: &str) -> Option<(usize, bool)> {
    let rest = file.strip_prefix("sub")?;
    let (n, vtt) = match rest.strip_suffix(".vtt") {
        Some(n) => (n, true),
        None => (rest.strip_suffix(".m3u8")?, false),
    };
    let n = (n.len() == 1).then(|| n.parse::<usize>().ok())??;
    (n < crate::subs::MAX_LANGUAGES).then_some((n, vtt))
}

pub fn is_session_file(file: &str) -> bool {
    matches!(file, "master.m3u8" | "media.m3u8" | "init.mp4" | "report")
        || seg_index(file).is_some()
        || sub_file(file).is_some()
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
    /// This session's hold on the GPU; given back when it ends.
    transcode: Option<crate::state::TranscodeSlot>,
}

pub struct Session {
    pub sid: String,
    pub sig: String,
    pub exp: u64,
    /// Whom it counts against: a logged-in browser's id, or the install it was started with
    /// (`auth::install_id`).
    pub owner: String,
    /// When it was set up, so an install past its share ends its oldest.
    pub started: Instant,
    pub imdb: String,
    pub dir: PathBuf,
    pub release: Release,
    pub info: MediaInfo,
    /// The audio track played, counting audio tracks only.
    pub audio: usize,
    /// The track is copied as it is, as this codec (`ec-3`, `ac-3`); `None` re-encodes it to AAC stereo.
    pub audio_copy: Option<&'static str>,
    /// Subtitle renditions, when the browser asked for any.
    pub subs: Option<crate::subs::Subs>,
    /// What a copy does with the video's Dolby Vision: kept for a player that shows it, else stripped.
    pub dovi: job::Dovi,
    /// The HEVC is transcoded to H.264 on the GPU, for a player that cannot take it.
    pub transcoded: bool,
    pub segments: Vec<Segment>,
    pub master: String,
    pub media: String,
    /// Scout's play URL for the release and the scout it came from, for fetching a fresh debrid link
    /// when the one ffmpeg reads stops working mid-session. Both are secrets.
    play_url: String,
    source: scout::ScoutSource,
    /// What the release's link and probe are remembered under (`opened_key`), so a link that stops working is
    /// forgotten there too.
    opened_key: String,
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

/// The segment a session's first job heads for: the one a resume at `start_at` plays, less `START_SLACK`. The
/// init request starts that job, so a resume runs ffmpeg once, from there, instead of from zero and again
/// after the player's seek.
pub(crate) fn start_segment(segments: &[Segment], start_at: f64) -> usize {
    seg_at(segments, (start_at - START_SLACK).max(0.0))
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

    pub fn short(&self) -> &str {
        &self.sid[..6]
    }

    /// The ffmpeg this session is running, if any. Tests use it to prove the process is gone.
    pub fn job_pid(&self) -> Option<u32> {
        self.lock().job.as_ref().filter(|j| j.exit.is_none()).map(|j| j.pid)
    }

    pub fn touch(&self) {
        self.lock().last_seen = Instant::now();
    }

    /// The segment production follows. Tests use it to see where a resume's first job will start.
    #[cfg(test)]
    pub fn wanted(&self) -> usize {
        self.lock().want
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

    /// What a job does with the video: a copy, or this session's transcode.
    fn video(&self, st: &AppState) -> job::Video {
        match self.transcoded {
            true => {
                let (width, height) = transcode_size(self.info.width, self.info.height);
                job::Video::Transcode {
                    device: st.cfg.vaapi_device.to_string_lossy().into_owned(),
                    width,
                    height,
                    tonemap: tonemaps(&self.info),
                }
            }
            false => {
                // Profile 5 kept as it is goes under its own sample entry; its HEVC has no other picture.
                let p5 =
                    self.dovi == job::Dovi::Keep && self.info.dolby_vision.is_some_and(|dv| dv.profile == 5);
                let tag = match self.info.video {
                    VideoCodec::Hevc if p5 => Some("dvh1"),
                    VideoCodec::Hevc => Some("hvc1"),
                    _ => None,
                };
                job::Video::Copy { tag, dovi: self.dovi }
            }
        }
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
        let spec = Spec {
            input: &input,
            seek,
            video: self.video(st),
            audio: self.audio,
            audio_copy: self.audio_copy.is_some(),
            dir: &dir,
        };
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
        // The next session for this release must not be handed the link that just failed.
        st.opened().forget(&self.opened_key);
        let play_url = crate::config::local(&self.play_url, &st.cfg.origin_aliases);
        match scout::resolve(&st.scout_http, &play_url, &self.source).await {
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

    /// Rendition `n`'s playlist.
    pub fn subtitle_playlist(&self, n: usize) -> String {
        playlist::subtitle_media(self.info.duration, n)
    }

    /// Rendition `n`'s WebVTT: the first subtitle in its language that den-subtitles offers and serves,
    /// made once and kept; an empty document when there is none.
    pub async fn subtitle(&self, st: &AppState, n: usize) -> String {
        use crate::subs;
        let Some(sb) = self.subs.as_ref().filter(|s| n < s.langs.len()) else { return subs::EMPTY.into() };
        let mut cache = sb.cache.lock().await;
        if let Some(Some(doc)) = cache.docs.get(n) {
            return doc.clone();
        }
        if cache.list.is_none() {
            cache.list = Some(self.list_subtitles(st, sb).await);
        }
        let want = &sb.langs[n];
        let offered: Vec<String> = cache
            .list
            .iter()
            .flatten()
            .filter(|e| crate::lang::canonical(&e.lang) == *want)
            .map(|e| e.url.clone())
            .take(3)
            .collect();
        let mut doc = None;
        for url in offered {
            if let Some(d) = self.fetch_subtitle(st, sb, &url).await {
                doc = Some(d);
                break;
            }
        }
        let doc = doc.unwrap_or_else(|| subs::EMPTY.into());
        if cache.docs.len() <= n {
            cache.docs.resize(n + 1, None);
        }
        cache.docs[n] = Some(doc.clone());
        doc
    }

    /// den-subtitles' list for this title, with the release's hash, size and filename as its hints.
    async fn list_subtitles(&self, st: &AppState, sb: &crate::subs::Subs) -> Vec<crate::subs::Entry> {
        use crate::subs;
        let size = self.release.size;
        // The hash's second half is the file's last 64 KiB: one ranged read, only now that it is wanted.
        let hash = match (size, sb.head_sum) {
            (Some(size), Some(head)) if size >= subs::HASH_CHUNK => {
                let input = self.lock().input.clone();
                crate::probe::read_range(&st.http, &input, size - subs::HASH_CHUNK, subs::HASH_CHUNK)
                    .await
                    .ok()
                    .filter(|t| t.len() as u64 == subs::HASH_CHUNK)
                    .map(|tail| subs::movie_hash(size, head, subs::chunk_sum(&tail)))
            }
            _ => None,
        };
        let Some(url) = subs::list_url(&sb.base, &self.imdb, hash.as_deref(), size, &self.release.filename)
        else {
            return Vec::new();
        };
        let secrets = [sb.base.as_str()];
        let listed = async {
            let resp = st.scout_http.get(&url).send().await.map_err(|e| e.without_url().to_string())?;
            if !resp.status().is_success() {
                return Err(format!("den-subtitles answered {}", resp.status().as_u16()));
            }
            crate::probe::read_capped(resp, subs::MAX_LIST).await.map(|b| subs::parse_list(&b))
        };
        listed.await.unwrap_or_else(|e| {
            crate::log_limited("subtitles_list", || {
                format!("session {}: subtitles: {}", self.short(), crate::redact::scrub(&e, &secrets))
            });
            Vec::new()
        })
    }

    /// One subtitle as HLS WebVTT, if it is on an allowed origin and is WebVTT.
    async fn fetch_subtitle(&self, st: &AppState, sb: &crate::subs::Subs, url: &str) -> Option<String> {
        use crate::subs;
        let vtt = subs::vtt_url(url);
        if !subs::on_origin(&vtt, &st.cfg.subtitle_origins) {
            crate::log_limited("subtitle_origin", || {
                format!("session {}: skipped a subtitle not on SUBTITLE_ORIGINS", self.short())
            });
            return None;
        }
        let vtt = crate::config::local(&vtt, &st.cfg.origin_aliases);
        let fetched = async {
            let resp = st.scout_http.get(&vtt).send().await.map_err(|e| e.without_url().to_string())?;
            if !resp.status().is_success() {
                return Err(format!("a subtitle answered {}", resp.status().as_u16()));
            }
            crate::probe::read_capped(resp, subs::MAX_SUBTITLE).await
        };
        match fetched.await {
            Ok(body) => subs::for_hls(&body),
            Err(e) => {
                crate::log_limited("subtitle_fetch", || {
                    format!("session {}: {}", self.short(), crate::redact::scrub(&e, &[sb.base.as_str()]))
                });
                None
            }
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
            // Now rather than when the last handle on the session drops: the next session may want it.
            i.transcode = None;
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

/// What an opened release is remembered under: the install that listed it, by its hash as a session's owner is,
/// and the release by its size and name. Not its play URL: scout issues a new ticket with every listing.
pub(crate) fn opened_key(base: &str, s: &scout::Stream) -> String {
    format!("{}/{}/{}", crate::auth::install_id(base), s.attributes.size_bytes.unwrap_or(0), s.filename())
}

/// Try one candidate: follow its play URL, read the head, and probe it — or take all of that from a recent
/// open of the same release by the same install.
async fn open(
    st: &AppState,
    src: &scout::ScoutSource,
    s: &scout::Stream,
) -> Result<(scout::Resolved, MediaInfo), String> {
    let key = opened_key(&src.base, s);
    if let Some(hit) = st.opened().get(&key, Instant::now()) {
        return Ok(hit);
    }
    // A ticket on scout's public name (d-play) is fetched at its LAN address, like the install it came from.
    let r =
        scout::resolve(&st.scout_http, &crate::config::local(&s.url, &st.cfg.origin_aliases), src).await?;
    let info = crate::probe::probe(&Source::Http { client: &st.http, url: &r.url }, &r.head)
        .await
        .map_err(|e| e.to_string())?;
    match &info.video {
        VideoCodec::Other(c) => return Err(format!("video is {c}, which needs a re-encode")),
        VideoCodec::Av1 => return Err("video is AV1, which needs a re-encode".into()),
        _ => {}
    }
    if info.audio.is_empty() {
        return Err("no audio track".into());
    }
    st.opened().put(key, &r, &info, Instant::now());
    Ok((r, info))
}

/// What a browser asked for in `POST /remux/session`.
pub struct Want<'a> {
    /// Scout's title id: `tt…`, or `tt…:<season>:<episode>` for an episode.
    pub id: &'a str,
    /// The release to prefer, when it is playable here.
    pub filename: Option<&'a str>,
    /// The scout install from the web app's library, accepted only on a `SCOUT_ORIGINS` origin: a full
    /// one, or — for a logged-in browser, presented with `REMUX_SCOUT_KEY` — a scope=availability one.
    /// Without it this service's own `SCOUT_INSTALL_URL` is used, for a logged-in browser only, which is
    /// how it can be driven by hand.
    pub scout: Option<&'a str>,
    /// Audio languages, most wanted first.
    pub audio: &'a [String],
    /// One audio track by index, overriding `audio` — from an earlier session's `audioTracks`, with that
    /// session's `filename`.
    pub audio_track: Option<usize>,
    /// den-subtitles' install from the web app's library, accepted only on a `SUBTITLE_ORIGINS` origin.
    pub subtitles: Option<&'a str>,
    /// Subtitle languages to offer as renditions, most wanted first.
    pub subtitle_languages: &'a [String],
    /// The video codecs the player takes (`h264`, `hevc`); empty is both. Without HEVC, H.264 releases
    /// are tried first and an HEVC one is transcoded on the GPU.
    pub video_codecs: &'a [String],
    /// What the player decodes, level by level; when given, it decides over `video_codecs`.
    pub playable: Option<&'a Playable>,
    /// Seconds into the title the player starts at: a resume. 0 from the start.
    pub start_at: f64,
}

/// What the player decodes, as its own tests found (`playable` in `POST /remux/session`): the highest level it
/// takes of 8-bit H.264 and of H.264 High 10 (`level_idc`), 8-bit and 10-bit HEVC (`general_level_idc`, level ×
/// 30) and HEVC's High tier, 0 for none, and whether it decodes PQ HDR. An HEVC release beyond it is transcoded on
/// the GPU; an H.264 one is passed over.
#[derive(serde::Deserialize, Clone, Copy, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Playable {
    pub h264: u16,
    /// 10-bit H.264 (High 10, profile 110) — almost only anime Hi10P. Safari decodes none; Chrome decodes it in
    /// software. Not transcoded when it's 0: the box's GPU has no High 10 decoder.
    pub h264_high10: u16,
    pub hevc_main: u16,
    pub hevc_main10: u16,
    /// A UHD Blu-ray remux is often High tier. Apple's decoders refuse it, whatever their tests say, so the web app
    /// reports 0 there; a player that doesn't send it gets it converted.
    pub hevc_high_tier: u16,
    pub hdr: bool,
    /// Plays E-AC-3 and AC-3 in fMP4 HLS — Safari and Apple's receivers: such a track is copied, not converted.
    pub eac3: bool,
    /// Which Dolby Vision the player shows as Dolby Vision rather than as its base layer. Absent is neither.
    pub dolby_vision: DolbyVisionPlay,
}

/// `playable.dolbyVision`: profile 5 (no base layer at all; Safari shows it, Chrome can't) and profile 8.x shown as
/// Dolby Vision.
#[derive(serde::Deserialize, Clone, Copy, Debug, Default)]
#[serde(default)]
pub struct DolbyVisionPlay {
    pub p5: bool,
    pub p8: bool,
}

impl std::fmt::Display for Playable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hdr = match (self.hdr, self.eac3) {
            (true, true) => ", HDR, E-AC-3",
            (true, false) => ", HDR",
            (false, true) => ", E-AC-3",
            (false, false) => "",
        };
        let (h264, high10) = (self.h264, self.h264_high10);
        let (main, main10, high) = (self.hevc_main, self.hevc_main10, self.hevc_high_tier);
        write!(f, "H.264 L{h264}, High 10 L{high10}, HEVC L{main}, 10-bit L{main10}, High tier L{high}")?;
        let dv = match (self.dolby_vision.p5, self.dolby_vision.p8) {
            (true, true) => ", Dolby Vision 5 and 8",
            (true, false) => ", Dolby Vision 5",
            (false, true) => ", Dolby Vision 8",
            (false, false) => "",
        };
        write!(f, "{hdr}{dv}")
    }
}

impl Playable {
    /// Whether the player takes this video as it is. A level the file doesn't name is taken: refusing it would
    /// refuse every release without a codec record.
    pub fn takes(&self, info: &crate::probe::MediaInfo) -> bool {
        let named = info.codecs.as_deref().and_then(crate::probe::profile_level);
        let fits = |max: u16| max > 0 && named.is_none_or(|(_, level, _)| level <= max);
        match info.video {
            // High 10 is its own decoder, reported at its own level; every other H.264 profile is 8-bit here.
            VideoCodec::H264 => match named {
                Some((110, _, _)) => fits(self.h264_high10),
                _ => fits(self.h264),
            },
            VideoCodec::Hevc => {
                let max = match named {
                    Some((_, _, true)) => self.hevc_high_tier,
                    Some((1, _, _)) | None => self.hevc_main.max(self.hevc_main10),
                    Some((2, _, _)) => self.hevc_main10,
                    Some(_) => 0,
                };
                fits(max) && (!info.hdr || self.hdr)
            }
            VideoCodec::Av1 | VideoCodec::Other(_) => false,
        }
    }

    fn takes_hevc(&self) -> bool {
        self.hevc_main > 0 || self.hevc_main10 > 0
    }
}

/// Whether a copy keeps the release's Dolby Vision: profile 5 for a player that shows profile 5, profile 8 with an
/// HDR10, SDR or HLG base layer (8.1, 8.2, 8.4) for one that shows profile 8. Profile 7's enhancement layer is
/// always dropped, and so is anything HLS has no name for.
pub(crate) fn keeps_dolby_vision(dv: crate::probe::DolbyVision, playable: Option<&Playable>) -> bool {
    let Some(p) = playable.filter(|_| dolby_vision_signal(dv).is_some()) else { return false };
    match dv.profile {
        5 => p.dolby_vision.p5,
        8 => p.dolby_vision.p8,
        _ => false,
    }
}

/// How kept Dolby Vision is named in the master playlist: CODECS to use in place of the base layer's (profile 5,
/// `dvh1.05.LL`), SUPPLEMENTAL-CODECS (profile 8, `dvh1.08.LL/` and the brand of what its base layer is — `db1p`
/// HDR10, `db2g` SDR, `db4h` HLG) and VIDEO-RANGE. `None` for a profile HLS does not name.
pub(crate) fn dolby_vision_signal(
    dv: crate::probe::DolbyVision,
) -> Option<(Option<String>, Option<String>, &'static str)> {
    let (brand, range) = match (dv.profile, dv.compat) {
        (5, _) => return Some((Some(format!("dvh1.05.{:02}", dv.level)), None, "PQ")),
        (8, 1) => ("db1p", "PQ"),
        (8, 2) => ("db2g", "SDR"),
        (8, 4) => ("db4h", "HLG"),
        _ => return None,
    };
    Some((None, Some(format!("dvh1.08.{:02}/{brand}", dv.level)), range))
}

/// Whether a release would play with no picture: Dolby Vision profile 5, which has no base layer, so stripped or
/// transcoded it comes out green and purple — unless the player shows profile 5 and takes the release as it is.
fn no_picture(want: &Want<'_>, takes_hevc: bool, info: &MediaInfo) -> bool {
    info.dolby_vision.is_some_and(|dv| {
        if dv.has_fallback() {
            return false;
        }
        !(keeps_dolby_vision(dv, want.playable) && plays(want, takes_hevc, info))
    })
}

/// The HLS codec a Dolby track is copied as, from the container's name for it — Matroska's `A_EAC3`/`A_AC3`
/// (and its `A_AC3/BSID…` variants), MP4's `ec-3`/`ac-3` — or `None` for anything else.
pub(crate) fn dolby_codec(container_codec: &str) -> Option<&'static str> {
    match container_codec {
        "A_EAC3" | "ec-3" => Some("ec-3"),
        c if c.starts_with("A_AC3") || c == "ac-3" => Some("ac-3"),
        _ => None,
    }
}

/// Whether a transcode of this release has to tone-map it to SDR. The container's transfer says so where it is
/// written down — a UHD Blu-ray remux often leaves Matroska's Colour element out — and Dolby Vision says so too:
/// its base layer is HDR10 or HLG, except profile 8.2's, which is already SDR.
pub(crate) fn tonemaps(info: &crate::probe::MediaInfo) -> bool {
    info.hdr || info.dolby_vision.is_some_and(|dv| dv.compat != 2)
}

/// Whether the player takes the release's video as it is: by `playable`, else by `videoCodecs`, which can only say
/// that it takes no HEVC.
fn plays(want: &Want<'_>, takes_hevc: bool, info: &crate::probe::MediaInfo) -> bool {
    match want.playable {
        Some(p) => p.takes(info),
        None => info.video != VideoCodec::Hevc || takes_hevc,
    }
}

/// How a release will play for this player, by what scout says of it before anything is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Fit {
    /// As it is — or nothing says otherwise, and the probe decides.
    Copy,
    /// Only converted on the GPU.
    Convert,
    /// Not at all, so it isn't opened.
    Never,
}

/// A release's `Fit` from scout's attributes, only where they say so. The probe still has the last word on
/// whatever is tried; this decides the order, and which not to try.
pub(crate) fn fit(a: &scout::Attributes, playable: Option<&Playable>, takes_hevc: bool) -> Fit {
    // Dolby Vision profile 5 has no base layer: stripped or converted, its picture is green and purple — unless the
    // player shows profile 5 itself. Only scout's probe reads the profile, so it is the file's.
    if a.dv_profile == 5 && !playable.is_some_and(|p| p.dolby_vision.p5) {
        return Fit::Never;
    }
    let uhd =
        a.resolution.as_deref().is_some_and(|r| matches!(r.to_ascii_lowercase().as_str(), "2160p" | "4k"));
    let ten_bit = a.bit_depth >= 10;
    match (a.codec.as_deref().map(str::to_ascii_lowercase).as_deref(), playable) {
        (Some("h264"), Some(p)) => {
            // A 10 the name inferred from "HDR" is no evidence of High 10: H.264 releases are not HDR.
            let max = if ten_bit && (a.probed || !a.hdr) { p.h264_high10 } else { p.h264 };
            // Nothing here makes H.264 smaller; 3840 × 2160 needs level 5.1.
            if max == 0 || (uhd && max < 51) {
                Fit::Never
            } else {
                Fit::Copy
            }
        }
        (Some("hevc"), Some(p)) => {
            let max = if ten_bit { p.hevc_main10 } else { p.hevc_main.max(p.hevc_main10) };
            // 3840 × 2160 needs level 5.0.
            if max == 0 || (uhd && max < 150) || (a.hdr && !p.hdr) {
                Fit::Convert
            } else {
                Fit::Copy
            }
        }
        (Some("hevc"), None) if !takes_hevc => Fit::Convert,
        _ => Fit::Copy,
    }
}

/// A transcode's output size: the source's, fitted inside `TRANSCODE_WIDTH` × `TRANSCODE_HEIGHT` with its
/// aspect kept (a 2.4:1 4K film becomes 1920 × 800 — 1080 lines of it would be wider than level 4.1
/// allows), never scaled up, both even. 0 × 0 when the source's is unknown.
pub(crate) fn transcode_size(w: u32, h: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (0, 0);
    }
    let scale =
        (job::TRANSCODE_WIDTH as f64 / w as f64).min(job::TRANSCODE_HEIGHT as f64 / h as f64).min(1.0);
    let even = |x: u32| ((x as f64 * scale).round() as u32 & !1).max(2);
    (even(w), even(h))
}

/// Who a session is for, which decides what den-remux vouches for.
pub enum Admission {
    /// A browser logged in with its key. For it den-remux presents its own scout key, so an
    /// availability-only install — or the `SCOUT_INSTALL_URL` fallback — can play.
    Browser(String),
    /// No login: the scout install the request names is the credential, as every addon's install URL is.
    /// den-remux's key is not sent, so scout alone decides: a full install lists and plays, and an
    /// availability-only, revoked or foreign one does not.
    Install,
}

/// The scout install a request names, or this service's own. Validated as sent, then fetched at its LAN address
/// when that is a public name: scout is on this box. The LAN form is also what names the install, whichever name
/// the browser used.
fn scout_base(st: &AppState, scout: Option<&str>) -> Result<String, ApiError> {
    match scout {
        Some(url) => Ok(crate::config::local(
            &scout::validate_scoped(url, &st.cfg.scout_origins).map_err(|why| {
                api(StatusCode::BAD_REQUEST, "bad_scout", format!("The scout URL was refused: {why}."))
            })?,
            &st.cfg.origin_aliases,
        )),
        None => st.cfg.scout_install_url.clone().ok_or_else(|| {
            api(
                StatusCode::SERVICE_UNAVAILABLE,
                "scout_unconfigured",
                "No scout URL in the request, and no SCOUT_INSTALL_URL.",
            )
        }),
    }
}

/// Scout's releases for `id`, its refusals put in this API's terms.
async fn scout_list(
    st: &AppState,
    source: &scout::ScoutSource,
    id: &str,
    by_install: bool,
) -> Result<Vec<scout::Stream>, ApiError> {
    let secrets = [source.base.as_str()];
    scout::list(&st.scout_http, source, id).await.map_err(|e| match e.status {
        // Out of scope: an availability-only install, which plays only with den-remux's key — sent for a
        // logged-in browser alone.
        Some(403) if by_install => api(
            StatusCode::UNAUTHORIZED,
            "not_logged_in",
            "This scout install can't play on its own; log in with this browser's key.",
        ),
        // Scout would not open the install: revoked, out of scope, or not one of its own.
        Some(status @ (400 | 403)) => {
            crate::log_limited("scout_refused", || format!("scout: refused the install ({status})"));
            api(
                StatusCode::FORBIDDEN,
                "scout_refused",
                "Scout refused this install: revoked, or one it can't open.",
            )
        }
        _ => {
            crate::log_limited("scout_list", || {
                format!("scout: {}", crate::redact::scrub(&e.detail, &secrets))
            });
            api(StatusCode::BAD_GATEWAY, "scout_unavailable", "Could not list releases.")
        }
    })
}

/// `POST /remux/releases`: the releases a session could play, in the order it would try them, so a player can name
/// one (`filename`). The caller gets their labels, names and sizes — never a URL.
pub async fn releases(
    st: &Arc<AppState>,
    admission: Admission,
    scout: Option<&str>,
    id: &str,
) -> Result<Vec<scout::Stream>, ApiError> {
    let by_install = matches!(admission, Admission::Install);
    let key = match admission {
        Admission::Browser(_) => st.cfg.scout_key.clone(),
        Admission::Install => None,
    };
    let source = scout::ScoutSource { base: scout_base(st, scout)?, key };
    Ok(scout::candidates(&scout_list(st, &source, id, by_install).await?, None))
}

/// `POST /remux/session`: pick and probe a release, choose its audio track, and set up its session.
pub async fn create(
    st: &Arc<AppState>,
    admission: Admission,
    want: &Want<'_>,
) -> Result<Arc<Session>, ApiError> {
    let imdb = want.id;
    let base = scout_base(st, want.scout)?;
    let by_install = matches!(admission, Admission::Install);
    let (owner, key, share) = match admission {
        Admission::Browser(b) => (b, st.cfg.scout_key.clone(), 1),
        Admission::Install => (crate::auth::install_id(&base), None, st.cfg.max_sessions_per_install),
    };
    let source = scout::ScoutSource { base, key };
    // Checked before any work: a bad install is the browser's mistake, not a reason to probe a release.
    let mut sub_langs: Vec<String> = Vec::new();
    for l in want.subtitle_languages.iter().map(|l| crate::lang::canonical(l)) {
        if !l.is_empty() && !sub_langs.contains(&l) && sub_langs.len() < crate::subs::MAX_LANGUAGES {
            sub_langs.push(l);
        }
    }
    let sub_base = match want.subtitles {
        Some(url) if !sub_langs.is_empty() => Some(crate::config::local(
            &scout::validate_scoped(url, &st.cfg.subtitle_origins).map_err(|why| {
                api(
                    StatusCode::BAD_REQUEST,
                    "bad_subtitles",
                    format!("The subtitles URL was refused: {why}."),
                )
            })?,
            &st.cfg.origin_aliases,
        )),
        _ => None,
    };
    // A browser starting another title is done with the one it was watching; making it wait out the
    // idle timer for its own old session would turn every change of mind into a 429. An install plays
    // `MAX_SESSIONS_PER_INSTALL` at once — a household's people — and past that its oldest gives way.
    let Some(_slot) = st.reserve(&owner, share).await else {
        return Err(api(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_sessions",
            "This browser or install, or the server, has no free session slot.",
        ));
    };
    let secrets = [source.base.as_str()];
    let list = scout_list(st, &source, imdb, by_install).await?;
    let mut candidates = scout::candidates(&list, want.filename);
    if candidates.is_empty() {
        return Err(api(
            StatusCode::NOT_FOUND,
            "no_release",
            "No cached release in a codec this service can remux.",
        ));
    }
    let takes_hevc = want.video_codecs.is_empty()
        || want
            .video_codecs
            .iter()
            .any(|c| matches!(c.to_ascii_lowercase().as_str(), "hevc" | "h265" | "hvc1" | "hev1"));
    let takes_hevc = want.playable.map_or(takes_hevc, Playable::takes_hevc);
    if !takes_hevc {
        // An explicit release (also used when changing its audio track) takes priority over codec
        // preferences. Only rank the alternatives; the named HEVC may need the GPU to stay itself.
        let alternatives =
            usize::from(candidates.first().is_some_and(|c| want.filename == Some(c.filename())));
        scout::h264_first(&mut candidates[alternatives..]);
    }
    // What scout says of each release decides, before any is opened, which are tried first — those that play as
    // they are, then those that play only converted, the ranking above holding within each — and which aren't
    // opened at all. The named release stays first unless it can't play.
    let mut ranked: Vec<(Fit, &scout::Stream)> = Vec::new();
    for c in &candidates {
        match fit(&c.attributes, want.playable, takes_hevc) {
            Fit::Never => eprintln!(
                "session: {imdb} skipped \"{}\" unopened: scout's attributes say it can't play here",
                c.attributes.label
            ),
            f => ranked.push((f, c)),
        }
    }
    let named_first = usize::from(ranked.first().is_some_and(|(_, c)| want.filename == Some(c.filename())));
    ranked[named_first..].sort_by_key(|(f, _)| *f);
    let mut no_transcode = false;
    let (mut chosen, fallback) = {
        let source = &source;
        let started = Instant::now();
        let mut queue = ranked.into_iter().peekable();
        let mut opening = FuturesOrdered::new();
        let mut tried = 0;
        let mut chosen = None;
        // The first release that plays only converted: the last resort, taken once nothing tried plays as it is.
        let mut fallback = None;
        loop {
            while opening.len() < PARALLEL_OPENS && tried < MAX_TRIES && started.elapsed() < PICK_BUDGET {
                // With a converted release in hand, one scout says plays only converted can't do better.
                if fallback.is_some() && queue.peek().is_some_and(|(f, _)| *f == Fit::Convert) {
                    break;
                }
                let Some((_, c)) = queue.next() else { break };
                tried += 1;
                opening.push_back(async move {
                    let opened = tokio::time::timeout(OPEN_TIMEOUT, open(st, source, c)).await;
                    (c, opened.unwrap_or_else(|_| Err(format!("not open after {}s", OPEN_TIMEOUT.as_secs()))))
                });
            }
            // In the order they were started, which is rank order.
            let Some((c, opened)) = opening.next().await else { break };
            match opened {
                // Dolby Vision profile 5 has no base layer to fall back to: stripped or transcoded, its picture is
                // green and purple. Only a player that shows profile 5 plays it, and only as it is.
                Ok((_, info)) if no_picture(want, takes_hevc, &info) => eprintln!(
                    "session: {imdb} skipped \"{}\": {} has no picture without Dolby Vision",
                    c.attributes.label,
                    info.dolby_vision.map(|dv| dv.to_string()).unwrap_or_default()
                ),
                // HEVC the player can't take as it is plays only converted on the GPU: kept as the last resort
                // while the rest are looked through for one that plays untouched — unless the player named this
                // release (another audio track of it), which it keeps.
                Ok((r, info)) if info.video == VideoCodec::Hevc && !plays(want, takes_hevc, &info) => {
                    eprintln!(
                        "session: {imdb} \"{}\" ({}) plays here only converted",
                        c.attributes.label,
                        info.codecs.as_deref().unwrap_or("HEVC")
                    );
                    let named = want.filename == Some(c.filename());
                    if fallback.is_none() {
                        fallback = Some((c, (r, info)));
                    }
                    if named {
                        break;
                    }
                }
                // H.264 beyond the player: nothing here makes it smaller.
                Ok((_, info)) if !plays(want, takes_hevc, &info) => eprintln!(
                    "session: {imdb} skipped \"{}\": {} is beyond this player",
                    c.attributes.label,
                    info.codecs.as_deref().unwrap_or("its video")
                ),
                Ok(found) => {
                    chosen = Some((c, found, None));
                    break;
                }
                Err(why) => eprintln!(
                    "session: {imdb} skipped \"{}\": {}",
                    c.attributes.label,
                    crate::redact::scrub(&why, &secrets)
                ),
            }
        }
        (chosen, fallback)
    };
    if chosen.is_none() {
        if let Some((c, found)) = fallback {
            match st.reserve_transcode() {
                Some(slot) => chosen = Some((c, found, Some(slot))),
                None => {
                    no_transcode = true;
                    eprintln!(
                        "session: {imdb} skipped \"{}\": it needs converting, and no transcode is free",
                        c.attributes.label
                    );
                }
            }
        }
    }
    let Some((c, (resolved, info), transcode)) = chosen else {
        if no_transcode {
            return Err(api(
                StatusCode::SERVICE_UNAVAILABLE,
                "transcode_unavailable",
                "This player takes no HEVC, and the GPU transcode is off or in use; try again when the other session ends.",
            ));
        }
        return Err(api(StatusCode::NOT_FOUND, "no_playable_release", "No cached release could be opened."));
    };
    let audio = match want.audio_track {
        Some(n) if n < info.audio.len() => n,
        Some(_) => {
            return Err(api(
                StatusCode::BAD_REQUEST,
                "bad_audio_track",
                format!("This release has {} audio tracks.", info.audio.len()),
            ))
        }
        None => crate::lang::pick_audio(&info.audio, want.audio),
    };
    // E-AC-3 or AC-3 plays as it is where the player says so; everything else is AAC stereo.
    let audio_copy = dolby_codec(&info.audio[audio].codec).filter(|_| want.playable.is_some_and(|p| p.eac3));

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
    // A resume at or past the end starts the title over.
    let start_at = Some(want.start_at).filter(|t| *t > 0.0 && *t < info.duration).unwrap_or(0.0);
    let first_segment = start_segment(&segments, start_at);
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
    let (codecs, resolution, (peak, avg)) = match transcode {
        Some(_) => (
            job::TRANSCODE_CODECS.to_string(),
            transcode_size(info.width, info.height),
            (job::TRANSCODE_PEAK + 192_000, job::TRANSCODE_BITRATE),
        ),
        None => (codecs, (info.width, info.height), (peak, avg)),
    };
    // Dolby Vision stays in a copy for a player that shows it; everywhere else, and in every transcode, the base
    // layer plays alone.
    let kept_dv =
        info.dolby_vision.filter(|dv| transcode.is_none() && keeps_dolby_vision(*dv, want.playable));
    let dovi = match (info.dolby_vision, kept_dv) {
        (None, _) => job::Dovi::Absent,
        (Some(_), None) => job::Dovi::Strip,
        (Some(_), Some(_)) => job::Dovi::Keep,
    };
    let (codecs, dv_signal) = match kept_dv.and_then(dolby_vision_signal) {
        Some((own, supplemental, range)) => {
            (own.unwrap_or(codecs), Some(playlist::DolbyVision { supplemental, range }))
        }
        None => (codecs, None),
    };
    let subs = sub_base.map(|base| crate::subs::Subs {
        base,
        langs: sub_langs,
        head_sum: resolved.head.get(..crate::subs::HASH_CHUNK as usize).map(crate::subs::chunk_sum),
        cache: Default::default(),
    });
    let renditions: Vec<(String, String)> = subs
        .as_ref()
        .map(|s| s.langs.iter().map(|l| (l.clone(), crate::lang::name(l))).collect())
        .unwrap_or_default();
    let opened_key = opened_key(&source.base, c);
    let session = Arc::new(Session {
        master: playlist::master(
            &codecs,
            dv_signal.as_ref(),
            &match audio_copy {
                Some(codec) => playlist::Audio::Copy {
                    codec,
                    channels: info.audio[audio].channels,
                    language: info.audio[audio].language.as_deref(),
                },
                None => playlist::Audio::Aac,
            },
            peak,
            avg,
            Some(resolution),
            &renditions,
        ),
        media: playlist::media(&segments, start_at),
        sid: sid.clone(),
        sig,
        exp,
        owner,
        started: Instant::now(),
        imdb: imdb.to_string(),
        dir,
        release: Release { label: c.attributes.label.clone(), filename: c.filename().to_string(), size },
        segments,
        play_url: c.url.clone(),
        source,
        opened_key,
        info,
        audio,
        audio_copy,
        dovi,
        subs,
        transcoded: transcode.is_some(),
        inner: Mutex::new(Inner {
            job: None,
            next_job: 0,
            gops: Vec::new(),
            finished: Vec::new(),
            init: None,
            last_seen: Instant::now(),
            want: first_segment,
            ended: false,
            bytes: 0,
            failures: 0,
            input: resolved.url,
            reresolved: false,
            transcode,
        }),
        wake: Notify::new(),
    });
    st.insert_session(session.clone());
    st.sessions_started.fetch_add(1, Relaxed);
    let dv = session.info.dolby_vision.map(|dv| match session.dovi {
        job::Dovi::Keep => format!(", {dv} kept"),
        _ => format!(", {dv} stripped to its base layer"),
    });
    let player = want.playable.map_or_else(|| "no capability report".to_string(), |p| p.to_string());
    eprintln!(
        "session {}: {imdb} \"{}\" ({}, {:?} {}{}{}, {:.0}s, {} keyframes, {} segments, audio {} {}{}; player: {player})",
        session.short(),
        session.release.label,
        session.info.container,
        session.info.video,
        codecs,
        dv.unwrap_or_default(),
        match (session.transcoded, tonemaps(&session.info)) {
            (true, true) => " → H.264 SDR on the GPU",
            (true, false) => " → H.264 on the GPU",
            (false, true) => ", HDR",
            (false, false) => "",
        },
        session.info.duration,
        session.info.keyframes.len(),
        session.segments.len(),
        session.audio,
        session.info.audio[session.audio].language.as_deref().unwrap_or("und"),
        session.audio_copy.map(|c| format!(" copied as {c}")).unwrap_or_default(),
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

    fn info(video: VideoCodec, codecs: &str, hdr: bool) -> crate::probe::MediaInfo {
        crate::probe::MediaInfo {
            container: "matroska",
            duration: 1.0,
            video,
            codecs: Some(codecs.into()),
            width: 3840,
            height: 2160,
            hdr,
            dolby_vision: None,
            audio: Vec::new(),
            keyframes: vec![0.0],
        }
    }

    #[test]
    fn dolby_vision_counts_as_hdr_unless_its_base_layer_is_sdr() {
        let mut release = info(VideoCodec::Hevc, "hvc1.2.4.L153.B0", false);
        assert!(!tonemaps(&release));
        release.dolby_vision = Some(crate::probe::DolbyVision { profile: 8, compat: 1, level: 6 });
        assert!(tonemaps(&release), "a remux often names no transfer in its header");
        release.dolby_vision = Some(crate::probe::DolbyVision { profile: 8, compat: 2, level: 6 });
        assert!(!tonemaps(&release), "profile 8.2's base layer is SDR already");
        assert!(tonemaps(&info(VideoCodec::Hevc, "hvc1.2.4.L153.B0", true)));
    }

    #[test]
    fn a_player_takes_what_its_levels_and_hdr_allow() {
        let phone = Playable {
            h264: 0x33,
            hevc_main: 153,
            hevc_main10: 153,
            hevc_high_tier: 0,
            hdr: true,
            ..Playable::default()
        };
        let hobbit = info(VideoCodec::Hevc, "hvc1.2.4.L153.B0", true);
        assert!(phone.takes(&hobbit));
        let bluray = info(VideoCodec::Hevc, "hvc1.2.4.H153.B0", true);
        assert!(!phone.takes(&bluray), "High tier, which an iPhone doesn't decode");
        assert!(Playable { hevc_high_tier: 153, ..phone }.takes(&bluray), "an Android phone that does");
        assert_eq!(phone.to_string(), "H.264 L51, High 10 L0, HEVC L153, 10-bit L153, High tier L0, HDR");
        assert!(!Playable { hdr: false, ..phone }.takes(&hobbit), "HDR it can't decode is converted");
        assert!(!Playable { hevc_main10: 0, ..phone }.takes(&hobbit), "8-bit HEVC only");
        assert!(!Playable { hevc_main10: 123, ..phone }.takes(&hobbit), "1080p at most");
        let sdr_1080 = info(VideoCodec::Hevc, "hvc1.1.6.L120.90", false);
        assert!(Playable { hevc_main10: 0, ..phone }.takes(&sdr_1080), "8-bit 1080p");
        let firefox = Playable { h264: 0x33, ..Playable::default() };
        assert!(firefox.takes(&info(VideoCodec::H264, "avc1.640029", false)));
        assert!(!firefox.takes(&hobbit) && !firefox.takes_hevc());
        assert!(!Playable { h264: 0x29, ..firefox }.takes(&info(VideoCodec::H264, "avc1.640033", false)));
    }

    #[test]
    fn dolby_vision_is_kept_and_named_where_the_player_shows_it() {
        use crate::probe::DolbyVision as Dv;
        let shows = |p5, p8| Playable { dolby_vision: DolbyVisionPlay { p5, p8 }, ..Playable::default() };
        let p81 = Dv { profile: 8, compat: 1, level: 6 };
        let p5 = Dv { profile: 5, compat: 0, level: 6 };
        let p76 = Dv { profile: 7, compat: 6, level: 6 };
        assert!(keeps_dolby_vision(p81, Some(&shows(false, true))));
        assert!(!keeps_dolby_vision(p81, Some(&shows(true, false))) && !keeps_dolby_vision(p81, None));
        assert!(keeps_dolby_vision(p5, Some(&shows(true, false))));
        assert!(!keeps_dolby_vision(p5, Some(&shows(false, true))));
        assert!(!keeps_dolby_vision(p76, Some(&shows(true, true))), "profile 7 always plays its base layer");
        let p86 = Dv { compat: 6, ..p81 };
        assert!(!keeps_dolby_vision(p86, Some(&shows(true, true))), "8.6 has no HLS brand");
        assert_eq!(dolby_vision_signal(p81), Some((None, Some("dvh1.08.06/db1p".into()), "PQ")));
        let sdr = Dv { compat: 2, level: 9, ..p81 };
        assert_eq!(dolby_vision_signal(sdr), Some((None, Some("dvh1.08.09/db2g".into()), "SDR")));
        assert_eq!(dolby_vision_signal(Dv { compat: 4, ..p81 }).map(|s| s.2), Some("HLG"));
        assert_eq!(dolby_vision_signal(p5), Some((Some("dvh1.05.06".into()), None, "PQ")));
        assert_eq!(dolby_vision_signal(p76), None);
    }

    #[test]
    fn only_dolby_tracks_are_copied() {
        assert_eq!(dolby_codec("A_EAC3"), Some("ec-3"));
        assert_eq!(dolby_codec("ec-3"), Some("ec-3"));
        assert_eq!(dolby_codec("A_AC3"), Some("ac-3"));
        assert_eq!(dolby_codec("A_AC3/BSID9"), Some("ac-3"));
        assert_eq!(dolby_codec("ac-3"), Some("ac-3"));
        for other in ["A_TRUEHD", "A_DTS", "A_AAC", "mp4a", "A_OPUS"] {
            assert_eq!(dolby_codec(other), None, "{other}");
        }
    }

    #[test]
    fn scouts_attributes_rank_releases_before_any_is_opened() {
        let a = |codec: &str| scout::Attributes { codec: Some(codec.into()), ..scout::Attributes::default() };
        let safari =
            Playable { h264: 0x33, hevc_main: 153, hevc_main10: 153, hdr: true, ..Playable::default() };
        let firefox = Playable { h264: 0x33, ..Playable::default() };
        let p = Some(&safari);
        assert_eq!(fit(&a("h264"), p, true), Fit::Copy);
        let hi10p = scout::Attributes { bit_depth: 10, ..a("h264") };
        assert_eq!(fit(&hi10p, p, true), Fit::Never, "10-bit H.264 and no High 10 decoder");
        assert_eq!(fit(&hi10p, Some(&Playable { h264_high10: 0x33, ..safari }), true), Fit::Copy);
        let hdr_word = scout::Attributes { bit_depth: 10, hdr: true, ..a("h264") };
        assert_eq!(fit(&hdr_word, p, true), Fit::Copy, "a 10 inferred from an HDR word is not High 10");
        let uhd_h264 = scout::Attributes { resolution: Some("2160p".into()), ..a("h264") };
        assert_eq!(fit(&uhd_h264, Some(&Playable { h264: 0x29, ..firefox }), false), Fit::Never);
        let p5 = scout::Attributes { dv_profile: 5, dolby_vision: true, hdr: true, ..a("hevc") };
        assert_eq!(fit(&p5, p, true), Fit::Never, "no picture without Dolby Vision");
        let shows_p5 = Playable { dolby_vision: DolbyVisionPlay { p5: true, p8: false }, ..safari };
        assert_eq!(fit(&p5, Some(&shows_p5), true), Fit::Copy, "a player that shows profile 5 opens it");
        let uhd = scout::Attributes { resolution: Some("2160p".into()), ..a("hevc") };
        let hd_only = Playable { hevc_main: 123, hevc_main10: 123, ..safari };
        assert_eq!(fit(&uhd, Some(&hd_only), true), Fit::Convert, "4K past a 1080p decoder");
        let hdr10 = scout::Attributes { hdr: true, bit_depth: 10, ..a("hevc") };
        assert_eq!(fit(&hdr10, Some(&Playable { hdr: false, ..safari }), true), Fit::Convert);
        assert_eq!(fit(&hdr10, p, true), Fit::Copy);
        assert_eq!(fit(&a("hevc"), Some(&firefox), false), Fit::Convert);
        assert_eq!(fit(&a("hevc"), None, false), Fit::Convert, "videoCodecs without HEVC");
        let unknown = scout::Attributes::default();
        assert_eq!(fit(&unknown, Some(&firefox), false), Fit::Copy, "unknown: the probe decides");
    }

    #[test]
    fn an_opened_release_is_remembered_per_install_whatever_its_ticket() {
        let s = scout::Stream {
            title: "f.mkv".into(),
            url: "http://scout/p/t1".into(),
            attributes: scout::Attributes::default(),
            hints: scout::Hints::default(),
        };
        let key = opened_key("http://scout/c2VhbGVk", &s);
        assert!(!key.contains("c2VhbGVk"), "the install is named by its hash, never its URL: {key}");
        assert_ne!(key, opened_key("http://scout/b3RoZXI", &s), "another install opens its own");
        let reticketed = scout::Stream { url: "http://scout/p/t2".into(), ..s.clone() };
        let again = opened_key("http://scout/c2VhbGVk", &reticketed);
        assert_eq!(key, again, "a new listing's ticket, the same release");
    }

    #[test]
    fn ten_bit_h264_plays_only_where_high_10_is_decoded() {
        let hi10p = info(VideoCodec::H264, "avc1.6e0028", false);
        let safari = Playable { h264: 0x33, ..Playable::default() };
        assert!(!safari.takes(&hi10p), "8-bit H.264 at any level is not High 10");
        let chrome = Playable { h264_high10: 0x33, ..safari };
        assert!(chrome.takes(&hi10p));
        assert!(!Playable { h264_high10: 0x1f, ..chrome }.takes(&hi10p), "level 4.0 is past 3.1");
        assert!(chrome.takes(&info(VideoCodec::H264, "avc1.640028", false)), "8-bit High still by `h264`");
    }

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
    fn a_transcode_comes_down_to_1080p_with_its_aspect() {
        assert_eq!(transcode_size(3840, 2160), (1920, 1080));
        assert_eq!(transcode_size(3840, 1600), (1920, 800), "a wide film is fitted to the width");
        assert_eq!(transcode_size(4096, 2160), (1920, 1012));
        assert_eq!(transcode_size(1440, 1080), (1440, 1080));
        assert_eq!(transcode_size(1280, 720), (1280, 720), "never scaled up");
        assert_eq!(transcode_size(1920, 803), (1920, 802), "both even");
        assert_eq!(transcode_size(0, 0), (0, 0));
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
    fn a_resume_starts_its_job_a_little_before_the_point() {
        let segs =
            playlist::segments(&[0.0, 2.5, 5.0, 8.0, 10.5, 13.0, 16.0, 19.5, 22.0, 24.0, 27.5], 30.021, 6.0);
        assert_eq!(start_segment(&segs, 0.0), 0);
        assert_eq!(start_segment(&segs, 17.0), 2, "13 → 19.5 plays 17");
        assert_eq!(start_segment(&segs, 13.5), 1, "near 13 a player may ask for the segment before it");
        assert_eq!(start_segment(&segs, 0.5), 0);
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
