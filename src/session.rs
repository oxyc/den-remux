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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};
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
/// How long creating a session for a native player waits for its `init.mp4` (`Session::prepare`).
const INIT_WAIT: Duration = Duration::from_secs(15);
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

/// A subtitle rendition's file.
#[derive(Debug, PartialEq)]
pub enum SubFile {
    /// `sub<N>.m3u8`
    Playlist(usize),
    /// `sub<N>.vtt`: den-subtitles' one document spanning the film.
    Document(usize),
    /// `sub<N>_<W>.vtt`: the release's own track over video segment `W`.
    Window(usize, usize),
}

pub fn sub_file(file: &str) -> Option<SubFile> {
    let rest = file.strip_prefix("sub")?;
    let digits = |s: &str, max: usize| {
        (!s.is_empty() && s.len() <= max && s.bytes().all(|c| c.is_ascii_digit()))
            .then(|| s.parse::<usize>().ok())?
    };
    let rendition = |s: &str| digits(s, 1).filter(|n| *n < crate::subs::MAX_LANGUAGES);
    if let Some(n) = rest.strip_suffix(".m3u8") {
        return rendition(n).map(SubFile::Playlist);
    }
    let body = rest.strip_suffix(".vtt")?;
    match body.split_once('_') {
        Some((n, w)) => Some(SubFile::Window(rendition(n)?, digits(w, 7)?)),
        None => rendition(body).map(SubFile::Document),
    }
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
    /// The release the player named when this one was played instead, and why it was passed over — where that is
    /// known.
    pub requested: Option<Requested>,
}

pub struct Requested {
    pub filename: String,
    pub why: String,
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
    /// Created through den-edge's authenticated public origin. Used only to release the public firewall gate;
    /// signed media authorization remains the session URL itself.
    pub public: bool,
    /// When it was set up, so an install past its share ends its oldest.
    pub started: Instant,
    pub imdb: String,
    pub dir: PathBuf,
    pub release: Release,
    pub info: MediaInfo,
    /// The audio track played, counting audio tracks only.
    pub audio: usize,
    /// The track is copied as it is, as this codec (`ec-3`, `ac-3`, `fLaC`); `None` re-encodes it to AAC.
    pub audio_copy: Option<&'static str>,
    /// What happens to the track: copied, or AAC stereo, 5.1 or 7.1.
    pub audio_out: job::AudioOut,
    /// The channels the session's audio carries: the track's own when copied, else 2, 6 or 8.
    pub audio_channels: u32,
    /// den-subtitles, when the browser named an install and languages.
    pub subs: Option<crate::subs::Subs>,
    /// What a player can pick: the languages asked for, then the release's own others, in rendition order.
    pub renditions: Vec<crate::subs::Rendition>,
    /// The release's own tracks the runs write, as ffmpeg numbers its subtitle streams.
    text_tracks: Vec<usize>,
    /// What the runs have written of them.
    own: Mutex<crate::subs::OwnCues>,
    /// What a copy does with the video's Dolby Vision: kept for a player that shows it, else stripped.
    pub dovi: job::Dovi,
    /// The HEVC is transcoded to H.264 on the GPU, for a player that cannot take it.
    pub transcoded: bool,
    /// The size and rate a transcode comes down to: 720p for a player whose link can't carry 1080p.
    pub preset: job::Preset,
    pub segments: Vec<Segment>,
    /// How long a player on the link it named waits before a copy plays through without running dry (`prebuffer`):
    /// its pre-buffer target. `None` for a transcode, or without a link or a byte index.
    pub prebuffer: Option<f64>,
    /// What a copy asks of a link (`need`); `None` for a transcode, whose rate is its preset's.
    pub need: Option<Need>,
    /// Each segment's start and bytes as the session sends them (`playlist::segment_bytes`), for a copy with a byte
    /// index: what a player needs to work out, as the film plays, whether its link keeps up.
    pub demand: Option<Vec<(f64, u64)>>,
    pub master: String,
    pub media: String,
    reports: AtomicU8,
    speed_probes: AtomicU8,
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

    /// Make the last request `ago` old and let the supervisor look again: a silence, in a test's time.
    #[cfg(test)]
    pub fn silent_for(&self, ago: Duration) {
        self.lock().last_seen = Instant::now() - ago;
        self.wake.notify_one();
    }

    /// A player's diagnostic is useful once and bounded to three attempts. The signed URL is a bearer
    /// credential; it must not also be an unbounded log-writing endpoint.
    pub fn take_report_slot(&self) -> bool {
        self.reports.fetch_update(Relaxed, Relaxed, |count| (count < 3).then_some(count + 1)).is_ok()
    }

    /// A receiver may retry one interrupted measurement. Beyond that, the signed bearer must not become an
    /// unbounded random-byte and home-upload generator for the rest of the session lifetime.
    pub fn take_speed_slot(&self) -> bool {
        self.speed_probes.fetch_update(Relaxed, Relaxed, |count| (count < 2).then_some(count + 1)).is_ok()
    }

    /// `Cache-Control` for what the session's URL serves the same for its whole life: its playlists, a found
    /// subtitle. The URL names the session and is signed for it, so nothing is kept past it or shared with another.
    pub fn cache_control(&self) -> String {
        format!("private, max-age={}", self.exp.saturating_sub(unix_now()))
    }

    /// The segment production follows. Tests use it to see where a resume's first job will start.
    #[cfg(test)]
    pub fn wanted(&self) -> usize {
        self.lock().want
    }

    /// Put a job's init segment where the session serves it. A kept Dolby Vision profile 5 with no record in its
    /// container has no side data for ffmpeg's muxer to write a `dvcC` from, so it is added here; a layout this
    /// can't patch is not served, and the session fails as any whose init never appears.
    fn write_init(&self, from: &Path, to: &Path) -> bool {
        let recordless = self.dovi == job::Dovi::Keep && self.info.dolby_vision_recordless;
        let Some(dv) = self.info.dolby_vision.filter(|_| recordless) else {
            return std::fs::copy(from, to).is_ok();
        };
        let patched = std::fs::read(from)
            .map_err(|e| e.to_string())
            .and_then(|init| job::add_dovi_record(&init, dv.profile, dv.level));
        match patched.and_then(|init| std::fs::write(to, init).map_err(|e| e.to_string())) {
            Ok(()) => true,
            Err(e) => {
                crate::log_limited("init_dovi_record", || {
                    format!("session {}: init.mp4 could not take a Dolby Vision record: {e}", self.short())
                });
                false
            }
        }
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
            job.bytes += size;
            st.scratch_bytes.fetch_add(size, Relaxed);
            *failures = 0;
            if init.is_none() {
                // Every run writes a byte-identical init (see job.rs), so the first one serves all.
                let dst = self.dir.join("init.mp4");
                if self.write_init(&job.dir.join("init.mp4"), &dst) {
                    *init = Some(dst);
                }
            }
        }
        gops.sort_by(|a, b| a.start.total_cmp(&b.start));
        if !job.text.is_empty() {
            // The run has read the film as far as its last finished GOP (less what the demuxer may still hand over
            // for it), or to the end when it ran that far.
            let upto = match job.exit {
                Some(true) => self.info.duration,
                _ => job.next_start - crate::subs::TRAIL,
            };
            let mut own = self.own.lock().unwrap_or_else(|e| e.into_inner());
            for &t in &job.text {
                own.take(job.id, t, &job::text_file(&job.dir, t), job.start, upto);
            }
        }
        if newly_exited {
            let why = if job.exit == Some(true) { "reached the end" } else { "failed" };
            eprintln!("session {}: {}", self.short(), job.pull_line(why));
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
                let (width, height) = transcode_size(self.info.width, self.info.height, self.preset);
                job::Video::Transcode {
                    device: st.cfg.vaapi_device.to_string_lossy().into_owned(),
                    width,
                    height,
                    tonemap: tonemaps(&self.info),
                    preset: self.preset,
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

    /// Whether a running job — started at `start`, its next GOP at `next_start` — gives segment `n`: it began at or
    /// before the segment, is no more than `RESTART_GAP_SEGMENTS` short of it, and hasn't gone past the segment's
    /// end with none of its GOPs there still kept. A job past it doesn't come back for it: a segment's GOPs are
    /// pruned once later ones are served, so a player asking for it again — a seek back, a player with no cache of
    /// its own — used to wait on that job until its request timed out.
    ///
    /// Segment 0 starts at 0 whatever the file's first keyframe says, and a job for it starts at that keyframe
    /// (`restart_point`), so `first_keyframe` is where "at or before" is counted from there.
    fn heads_for(
        segments: &[Segment],
        n: usize,
        first_keyframe: f64,
        job: u32,
        start: f64,
        next_start: f64,
        gops: &[Gop],
    ) -> bool {
        let seg = segments[n];
        let passed = next_start >= seg.end - SNAP;
        let kept = gops
            .iter()
            .any(|g| g.job == job && g.start < seg.end - SNAP && g.start + g.dur > seg.start + SNAP);
        start <= seg.start.max(first_keyframe) + SNAP
            && n <= seg_at(segments, next_start) + RESTART_GAP_SEGMENTS
            && (!passed || kept)
    }

    /// Make sure a job is heading for segment `n`: leave a running job that will reach it soon, start
    /// one at `n` otherwise. `Err` when the source has failed too often to try again.
    fn ensure_job(&self, i: &mut Inner, n: usize, st: &AppState) -> Result<(), ()> {
        let first_keyframe = self.info.keyframes.first().copied().unwrap_or(0.0);
        let keep = i.job.as_ref().is_some_and(|j| {
            j.exit.is_none()
                && Self::heads_for(&self.segments, n, first_keyframe, j.id, j.start, j.next_start, &i.gops)
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
            audio_out: self.audio_out,
            text: &self.text_tracks,
            dir: &dir,
        };
        match Job::spawn(&st.cfg.ffmpeg, id, start, &spec) {
            Ok(new) => {
                if let Some(old) = i.job.replace(new) {
                    // A run that already exited said so when it did.
                    if old.exit.is_none() {
                        eprintln!(
                            "session {}: {}",
                            self.short(),
                            old.pull_line(&format!("replaced for segment {n}"))
                        );
                    }
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
        // A segment further, with the release's own subtitles: a player asks for a subtitle segment as far ahead as
        // it buffers video, and one is only made once the job has read a little past it.
        let ahead_segments = AHEAD_SEGMENTS + usize::from(!self.text_tracks.is_empty());
        let limit = self.segments[(i.want + ahead_segments).min(last)].end;
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
                    return media_response(parts, head, self.exp);
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
                    return media_response(parts, head, self.exp);
                }
            }
            if Instant::now() >= deadline {
                return busy();
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Start the job for the segment the session opens on and wait, up to `INIT_WAIT`, for its `init.mp4`, so a
    /// native player's first request for it is answered at once.
    ///
    /// Apple's players give up on the map after about five seconds — AVFoundation's "No response for map", which
    /// Safari reports as "Media failed to decode" — and a job that seeks into a remote file, and perhaps starts a
    /// transcode, takes longer than that to write one. hls.js waits as long as it takes. Whatever stops this early
    /// (a run that fails, a job that won't start) is left to the player's own request, which handles it as before.
    pub async fn prepare(&self, st: &AppState) {
        let deadline = Instant::now() + INIT_WAIT;
        loop {
            {
                let mut i = self.lock();
                if i.ended {
                    return;
                }
                self.refresh(&mut i, st);
                if i.init.is_some() || i.failures > 0 {
                    return;
                }
                let want = i.want;
                if self.ensure_job(&mut i, want, st).is_err() {
                    return;
                }
                self.gate(&mut i, st);
            }
            self.wake.notify_one();
            if Instant::now() >= deadline {
                // The session is answered anyway, and Safari will likely give up on the map before it is written.
                eprintln!(
                    "session {}: init.mp4 not written after {}s; answering without it",
                    self.short(),
                    INIT_WAIT.as_secs()
                );
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Whether the session's `init.mp4` has been written. Tests use it to see what `prepare` left behind.
    #[cfg(test)]
    pub fn init_ready(&self) -> bool {
        self.lock().init.is_some()
    }

    /// Rendition `n`'s playlist: one document spanning the film when den-subtitles serves the language — it has first
    /// say, its subtitle usually being timed to the release — else a segment per video segment from the release's own
    /// track, where it has one. A language with neither is the document, which serves as empty.
    pub async fn subtitle_playlist(&self, st: &AppState, n: usize) -> String {
        let Some(r) = self.renditions.get(n) else { return String::new() };
        let own = match (r.own, r.den) {
            (None, _) => false,
            (Some(_), false) => true,
            (Some(_), true) => !self.den_offers(st, n).await,
        };
        match own {
            true => playlist::subtitle_segments(&self.segments, n),
            false => playlist::subtitle_media(self.info.duration, n),
        }
    }

    /// Does den-subtitles list a subtitle in rendition `n`'s language? Not when it could not be asked.
    async fn den_offers(&self, st: &AppState, n: usize) -> bool {
        let (Some(sb), Some(r)) = (self.subs.as_ref(), self.renditions.get(n)) else { return false };
        let mut cache = sb.cache.lock().await;
        if cache.list.is_none() {
            cache.list = self.list_subtitles(st, sb).await;
        }
        cache.list.iter().flatten().any(|e| crate::lang::canonical(&e.lang) == r.lang)
    }

    /// Rendition `n`'s own track over video segment `w`, once the job has read the film past it: the cues showing in
    /// it, as WebVTT. It waits a while for a job that is about to, and answers 503 like a video segment when none
    /// does. A subtitle never starts a job: the video's requests drive that, and a player that wants both asks for
    /// both.
    pub async fn serve_subtitle_window(&self, st: &AppState, n: usize, w: usize) -> Response<Body> {
        let (Some(track), Some(seg)) = (self.renditions.get(n).and_then(|r| r.own), self.segments.get(w))
        else {
            return httputil::not_found();
        };
        // Segment 0 starts at 0 whatever the file's first keyframe says, and a job for it starts at that keyframe.
        let from = seg.start.max(self.info.keyframes.first().copied().unwrap_or(0.0));
        let deadline = Instant::now() + SEGMENT_WAIT;
        loop {
            {
                let mut i = self.lock();
                if i.ended {
                    return gone();
                }
                i.last_seen = Instant::now();
                self.refresh(&mut i, st);
                self.gate(&mut i, st);
            }
            self.wake.notify_one();
            {
                let own = self.own.lock().unwrap_or_else(|e| e.into_inner());
                if own.covers(track, from, seg.end) {
                    let doc = own.window(track, seg.start, seg.end);
                    return httputil::text("text/vtt; charset=utf-8", &self.cache_control(), doc);
                }
            }
            if Instant::now() >= deadline {
                return busy();
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Rendition `n`'s WebVTT: the first subtitle in its language that den-subtitles offers and serves, made once
    /// and kept for the session; `None` when there is none yet. Only what was found is kept: den-subtitles' list is
    /// kept once it answers, and a failed list or subtitle is asked for again on the player's next request, so one
    /// blip doesn't leave the session without subtitles.
    pub async fn subtitle(&self, st: &AppState, n: usize) -> Option<String> {
        let want = &self.renditions.get(n).filter(|r| r.den)?.lang;
        let sb = self.subs.as_ref()?;
        let mut cache = sb.cache.lock().await;
        if let Some(Some(doc)) = cache.docs.get(n) {
            return Some(doc.clone());
        }
        if cache.list.is_none() {
            cache.list = self.list_subtitles(st, sb).await;
        }
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
        let doc = doc?;
        if cache.docs.len() <= n {
            cache.docs.resize(n + 1, None);
        }
        cache.docs[n] = Some(doc.clone());
        Some(doc)
    }

    /// den-subtitles' list for this title, with the release's hash, size and filename as its hints; `None` when it
    /// could not be had.
    async fn list_subtitles(&self, st: &AppState, sb: &crate::subs::Subs) -> Option<Vec<crate::subs::Entry>> {
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
            return Some(Vec::new());
        };
        let secrets = [sb.base.as_str()];
        let listed = async {
            let resp = st.scout_http.get(&url).send().await.map_err(|e| e.without_url().to_string())?;
            if !resp.status().is_success() {
                return Err(format!("den-subtitles answered {}", resp.status().as_u16()));
            }
            crate::probe::read_capped(resp, subs::MAX_LIST).await.map(|b| subs::parse_list(&b))
        };
        listed
            .await
            .inspect_err(|e| {
                crate::log_limited("subtitles_list", || {
                    format!("session {}: subtitles: {}", self.short(), crate::redact::scrub(e, &secrets))
                });
            })
            .ok()
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
            if j.exit.is_none() {
                eprintln!("session {}: {}", self.short(), j.pull_line("session ended"));
            }
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

/// `init.mp4` or a segment. Kept until the session expires at `exp`: its URL is signed for this session alone, and
/// a player that seeks back to a segment already pruned from scratch takes it from its cache rather than restarting
/// ffmpeg for it. Whole bodies only — a `Range` is not honoured, and `Accept-Ranges: none` says so.
fn media_response(parts: Vec<(std::fs::File, u64)>, head: bool, exp: u64) -> Response<Body> {
    let len: u64 = parts.iter().map(|(_, l)| l).sum();
    let body = if head { httputil::full("") } else { httputil::files_body(parts) };
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "video/mp4")
        .header("content-length", len)
        .header("cache-control", format!("private, max-age={}, immutable", exp.saturating_sub(unix_now())))
        .header("accept-ranges", "none")
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

/// What `AppState::known` remembers a release's probe under: the title and the release's name, whoever listed it.
pub(crate) fn known_key(title: &str, s: &scout::Stream) -> String {
    format!("{title}/{}", s.filename())
}

/// What `AppState::unplayable` remembers a release under: the title, the release's size and its name. Scout gives
/// no infohash or file index — its play URL is a ticket minted afresh with every listing — and its
/// `behaviorHints.filename` is what it names as a release's identity across listings. The size tells apart two files
/// under one name (a repack, one episode of a season pack from another), and the title keeps a pack's name for one
/// episode from standing for the next. `None` without a size: a name alone is too little to rule a release out by.
pub(crate) fn unplayable_key(title: &str, s: &scout::Stream) -> Option<String> {
    let size = s.attributes.size_bytes.filter(|n| *n > 0)?;
    Some(format!("{title}/{size}/{}", s.filename()))
}

/// Why opening a release failed. `lasting` when the file itself is why — no browser would play it — so the next
/// session skips it unopened; a timeout, a network error or a debrid's refusal never is, and neither is what depends
/// on the player (`create` decides that on an opened release, which `open` returns as `Ok`).
pub(crate) struct OpenFailure {
    pub why: String,
    pub lasting: bool,
}

impl OpenFailure {
    fn passing(why: String) -> Self {
        OpenFailure { why, lasting: false }
    }
}

/// Whether a head that starts neither container is the file's own first bytes, and not an error page a debrid sent
/// in its place: as long as was asked for (or the whole file), and not text.
fn the_files_own_head(r: &scout::Resolved) -> bool {
    let full = r.head.len() as u64 == scout::HEAD_BYTES.min(r.size.unwrap_or(u64::MAX));
    let text = r.head.iter().find(|b| !b.is_ascii_whitespace()).is_some_and(|b| matches!(b, b'<' | b'{'));
    full && !r.head.is_empty() && !text
}

/// Try one candidate: follow its play URL, read the head, and probe it — or take all of that from a recent
/// open of the same release by the same install. What the probe found is remembered for `releases`' verdicts.
async fn open(
    st: &AppState,
    src: &scout::ScoutSource,
    title: &str,
    s: &scout::Stream,
) -> Result<(scout::Resolved, MediaInfo), OpenFailure> {
    let key = opened_key(&src.base, s);
    if let Some(hit) = st.opened().get(&key, Instant::now()) {
        return Ok(hit);
    }
    // A ticket on scout's public name (d-play) is fetched at its LAN address, like the install it came from.
    let r = scout::resolve(&st.scout_http, &crate::config::local(&s.url, &st.cfg.origin_aliases), src)
        .await
        .map_err(OpenFailure::passing)?;
    let info = match crate::probe::probe(&Source::Http { client: &st.http, url: &r.url }, &r.head).await {
        Ok(info) => info,
        Err(e) => {
            let neither =
                matches!(&e, crate::probe::ProbeError::Unsupported(w) if w == crate::probe::NEITHER);
            return Err(OpenFailure { why: e.to_string(), lasting: neither && the_files_own_head(&r) });
        }
    };
    st.known().put(known_key(title, s), &info);
    if let VideoCodec::Other(c) = &info.video {
        return Err(OpenFailure { why: format!("video is {c}, which needs a re-encode"), lasting: true });
    }
    if info.audio.is_empty() {
        return Err(OpenFailure { why: "no audio track".into(), lasting: true });
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
    /// The bits a second the player's link carries, as it measured them: a remote player's. A copy needing more is
    /// taken only when nothing fits, after a transcode at the preset the link takes. `None` is a player at home.
    pub max_bitrate: Option<u64>,
    /// Which browser asked, for the log (`client::label`): `Chrome 151 · macOS · hls.js`.
    pub client: String,
    /// The HLS player it plays in (`native`, `hls.js`, `cast`): how far ahead it buffers (`buffer_of`).
    pub player: Option<&'a str>,
    /// Releases not to open, by filename: the one a player is switching away from.
    pub exclude: &'a [String],
    /// No transcode for this request: a player replacing a session mid-film, which a conversion never replaces one.
    pub no_transcode: bool,
    /// Only a copy that fits the link will do: no copy that would starve on it, and no transcode. A player switching
    /// away from a release its link can't carry asks this, and keeps playing what it has when nothing fits.
    pub fits_only: bool,
}

/// What the player decodes, as its own tests found (`playable` in `POST /remux/session`): the highest level it
/// takes of 8-bit H.264 and of H.264 High 10 (`level_idc`), 8-bit and 10-bit HEVC (`general_level_idc`, level ×
/// 30) and HEVC's High tier, and of 8-bit and 10-bit AV1 at Main profile (`seq_level_idx`), 0 for none, and whether
/// it decodes PQ HDR in HEVC and in AV1, which of VP9's profiles it decodes, and which audio it plays as it is. An HEVC
/// release beyond it is transcoded on the GPU; an H.264, AV1 or VP9 one is passed over.
#[derive(serde::Deserialize, serde::Serialize, Clone, Copy, Debug, Default)]
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
    /// Plays 6-channel AAC-LC: a converted track of six channels or more stays 5.1 rather than coming down to stereo.
    pub aac_multichannel: bool,
    /// Which Dolby Vision the player shows as Dolby Vision rather than as its base layer. Absent is neither.
    pub dolby_vision: DolbyVisionPlay,
    /// 8-bit AV1's highest `seq_level_idx` at Main profile and tier (8 is level 4.0, 13 is 5.1). Nothing on the box
    /// converts AV1, so where this and `av1_main10` are 0 an AV1 release isn't tried at all.
    pub av1: u16,
    /// 10-bit AV1's.
    pub av1_main10: u16,
    /// Decodes 10-bit AV1 with PQ.
    pub av1_hdr: bool,
    /// Plays FLAC in fMP4 HLS: such a track is copied, not converted.
    pub flac: bool,
    /// Plays 8-channel AAC-LC: a converted track of eight channels or more stays 7.1 rather than folding down to 5.1.
    pub aac71: bool,
    /// Decodes VP9 profile 0 (8-bit) in fMP4 HLS. Nothing on the box converts VP9, so where this and `vp9_profile2` are
    /// false a VP9 release isn't tried at all.
    pub vp9: bool,
    /// Decodes VP9 profile 2 (10-bit) — never, whatever it says, for a session Safari's native player plays (`through`).
    pub vp9_profile2: bool,
}

/// `playable.dolbyVision`: profile 5 (no base layer at all; Safari shows it, Chrome can't) and profile 8.x shown as
/// Dolby Vision.
#[derive(serde::Deserialize, serde::Serialize, Clone, Copy, Debug, Default)]
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
        let av1_hdr = if self.av1_hdr { ", AV1 HDR" } else { "" };
        let aac = if self.aac_multichannel { ", AAC 5.1" } else { "" };
        let more: String = [
            (self.aac71, ", AAC 7.1"),
            (self.flac, ", FLAC"),
            (self.vp9, ", VP9"),
            (self.vp9_profile2, ", VP9 profile 2"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect();
        write!(f, "{hdr}, AV1 L{}, AV1 10-bit L{}{av1_hdr}{dv}{aac}{more}", self.av1, self.av1_main10)
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
            VideoCodec::Av1 => {
                // Main profile at Main tier is all a player is asked about; a Main decoder takes 8-bit and 10-bit.
                let max = match (named, info.codecs.as_deref().and_then(crate::probe::av1_bit_depth)) {
                    (Some((profile, _, high_tier)), _) if profile != 0 || high_tier => 0,
                    (_, Some(10)) => self.av1_main10,
                    (_, Some(8) | None) => self.av1.max(self.av1_main10),
                    _ => 0,
                };
                fits(max) && (!info.hdr || self.av1_hdr)
            }
            // Profile 0 is 8-bit; profile 2 is 10-bit or 12-bit, and a player is asked about 10. No level is asked.
            VideoCodec::Vp9 => match (named, info.codecs.as_deref().and_then(|c| c.split('.').nth(3))) {
                (Some((0, _, _)), _) => self.vp9,
                (Some((2, _, _)), Some("10")) => self.vp9_profile2,
                _ => false,
            },
            VideoCodec::Other(_) => false,
        }
    }

    fn takes_hevc(&self) -> bool {
        self.hevc_main > 0 || self.hevc_main10 > 0
    }

    fn takes_av1(&self) -> bool {
        self.av1 > 0 || self.av1_main10 > 0
    }

    fn takes_vp9(&self) -> bool {
        self.vp9 || self.vp9_profile2
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

fn kept_dolby_vision(
    info: &MediaInfo,
    transcoded: bool,
    playable: Option<&Playable>,
) -> Option<crate::probe::DolbyVision> {
    info.dolby_vision
        .filter(|dv| !transcoded && !info.dolby_vision_record_mismatch && keeps_dolby_vision(*dv, playable))
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

/// The VIDEO-RANGE a copied variant names when it keeps no Dolby Vision. Without one a player takes the stream as
/// SDR, so an HDR copy has to say so: an AV1 by the transfer its codec string names, an HEVC by HLG where the
/// container says so and PQ otherwise, since an HDR release that names only its Rec. 2020 colours is HDR10, and a VP9
/// the same way — though it is HDR only by its transfer (`vp9_track`). `None` for SDR.
fn copied_range(info: &MediaInfo, codecs: &str) -> Option<&'static str> {
    match info.video {
        VideoCodec::Av1 => crate::probe::av1_video_range(codecs),
        VideoCodec::Hevc | VideoCodec::Vp9 if info.hdr => Some(if info.hlg { "HLG" } else { "PQ" }),
        _ => None,
    }
}

/// Whether a release would play with no picture: Dolby Vision profile 5, which has no base layer, so stripped or
/// transcoded it comes out green and purple — unless the player shows profile 5 and takes the release as it is.
///
/// A profile 5 whose container has no Dolby Vision record (`dolby_vision_recordless`) is kept the same way: the
/// init segment is given the record it lacks (`job::add_dovi_record`).
fn no_picture(playable: Option<&Playable>, takes_hevc: bool, info: &MediaInfo) -> bool {
    // A contradictory P5 record sits over a regular BT.2020 PQ/HLG base layer. The record itself must not make us
    // refuse that safe fallback when its RPU was unreadable.
    if info.dolby_vision_record_mismatch {
        return false;
    }
    info.dolby_vision.is_some_and(|dv| {
        if dv.has_fallback() {
            return false;
        }
        !(keeps_dolby_vision(dv, playable) && plays(playable, takes_hevc, info))
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

/// The HLS codec a FLAC track is copied as, from Matroska's `A_FLAC` or MP4's `fLaC`; `None` for anything else.
pub(crate) fn flac_codec(container_codec: &str) -> Option<&'static str> {
    matches!(container_codec, "A_FLAC" | "fLaC").then_some("fLaC")
}

/// How a track of `channels` plays, by its container's codec name: copied as the HLS codec the player takes it as —
/// E-AC-3 or AC-3 (`playable.eac3`), FLAC (`playable.flac`) — or converted to AAC-LC: 7.1 from eight channels or more
/// where the player plays 8-channel AAC, 5.1 from six or more where it plays multichannel AAC, stereo otherwise.
pub(crate) fn audio_plan(
    container_codec: &str,
    channels: u32,
    playable: Option<&Playable>,
) -> (Option<&'static str>, job::AudioOut) {
    let p = playable.copied().unwrap_or_default();
    let copy = dolby_codec(container_codec)
        .filter(|_| p.eac3)
        .or_else(|| flac_codec(container_codec).filter(|_| p.flac));
    let out = match copy {
        Some(_) => job::AudioOut::Copy,
        None if channels >= 8 && p.aac71 => job::AudioOut::Surround71,
        None if channels >= 6 && p.aac_multichannel => job::AudioOut::Surround,
        None => job::AudioOut::Stereo,
    };
    (copy, out)
}

/// Whether a transcode of this release has to tone-map it to SDR. The container's transfer says so where it is
/// written down — a UHD Blu-ray remux often leaves Matroska's Colour element out — and Dolby Vision says so too:
/// its base layer is HDR10 or HLG, except profile 8.2's, which is already SDR.
pub(crate) fn tonemaps(info: &crate::probe::MediaInfo) -> bool {
    info.hdr || info.dolby_vision.is_some_and(|dv| dv.compat != 2)
}

/// Whether the player takes the release's video as it is: by `playable`, else by `videoCodecs`, which can only say
/// that it takes no HEVC — and never that it takes AV1 or VP9, which only `playable` reports.
fn plays(playable: Option<&Playable>, takes_hevc: bool, info: &crate::probe::MediaInfo) -> bool {
    match playable {
        Some(p) => p.takes(info),
        None => match info.video {
            VideoCodec::Hevc => takes_hevc,
            VideoCodec::Av1 | VideoCodec::Vp9 => false,
            _ => true,
        },
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
    // Most Profile 5 has no base layer, but AE #532 established a real class whose container falsely says P5 over a
    // normal HDR base layer. Put these last rather than ruling them out: this probe reads the VUI and first RPU before
    // either refusing genuine P5 or safely stripping a contradictory record.
    if a.dv_profile == 5 && !playable.is_some_and(|p| p.dolby_vision.p5) {
        return Fit::Convert;
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
        (Some("av1"), p) => {
            let max = p.map_or(0, |p| if ten_bit { p.av1_main10 } else { p.av1.max(p.av1_main10) });
            // 3840 × 2160 needs level 5.0, `seq_level_idx` 12. Nothing converts AV1: what won't play isn't opened.
            if max == 0 || (uhd && max < 12) || (a.hdr && !p.is_some_and(|p| p.av1_hdr)) {
                Fit::Never
            } else {
                Fit::Copy
            }
        }
        (Some("vp9"), p) => {
            // Nothing converts VP9 either. A 10 is profile 2's; a depth nobody read is left to the probe.
            let takes = p.is_some_and(|p| match a.bit_depth {
                0 => p.takes_vp9(),
                d if d >= 10 => p.vp9_profile2,
                _ => p.vp9,
            });
            if takes {
                Fit::Copy
            } else {
                Fit::Never
            }
        }
        (Some("hevc"), None) if !takes_hevc => Fit::Convert,
        _ => Fit::Copy,
    }
}

/// A release's average bitrate, from its size and duration; `None` when either isn't known.
fn average_bitrate(size: Option<u64>, duration: f64) -> Option<u64> {
    size.filter(|_| duration > 0.0).map(|s| (s as f64 * 8.0 / duration) as u64)
}

/// The longest a player is asked to wait before a copy plays through: a link that needs more of a wait than this
/// for a release is too slow for it. Ten seconds is under the thirty that hls.js and Safari hold ahead, so a player
/// can build that lead at all, and about what a person waits for a start before giving up on it.
const MAX_PREBUFFER: f64 = 10.0;
/// The hard cap on any head start: a copy that asks at most this ranks next after those within `MAX_PREBUFFER`, and
/// the player shows a countdown while it waits. Longer than this and a viewer is left in front of a spinner, which is
/// never acceptable; such a copy ranks below a transcode.
const LONG_PREBUFFER: f64 = 30.0;
/// What a release with no byte index is taken to need over its average. An encode at a constant quality peaks at
/// three or four times its average for seconds at a time, but over the stretches a buffer has to carry, half again is
/// what the indexed releases measured.
const NO_INDEX_HEADROOM: f64 = 1.5;

/// What a copy needs of the link, in bits a second: from where the player starts, the rate at which it waits at most
/// `MAX_PREBUFFER` and then never runs dry (`playlist::needed_rate`) — or, from a file with no byte index, its average
/// with `NO_INDEX_HEADROOM`, which `indexed` says it is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Need {
    pub bitrate: u64,
    pub indexed: bool,
}

impl std::fmt::Display for Need {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.indexed {
            true => write!(f, "{} kbit/s to start within {MAX_PREBUFFER:.0}s", self.bitrate / 1000),
            false => write!(
                f,
                "{} kbit/s, its average with {:.0}% headroom: the file has no byte index",
                self.bitrate / 1000,
                (NO_INDEX_HEADROOM - 1.0) * 100.0
            ),
        }
    }
}

/// How far ahead a player buffers, by the one it named (`player` in `POST /remux/session`): hls.js as den-edge sets it
/// up, two minutes or 150 MB; the cast page's hls.js, a minute or 50 MB, a receiver having little memory; Safari's
/// own player, and any that didn't say, about thirty seconds.
pub(crate) fn buffer_of(player: Option<&str>) -> playlist::Buffer {
    match player {
        Some("hls.js") => playlist::Buffer { secs: 120.0, bytes: Some(150_000_000) },
        Some("cast") => playlist::Buffer { secs: 60.0, bytes: Some(50_000_000) },
        _ => playlist::Buffer { secs: 30.0, bytes: None },
    }
}

/// What a session would deliver of a release, and to what: where the player starts, how far ahead it buffers, and the
/// bytes it is sent — the video and the one audio track it plays (`delivered_bytes`), else the whole file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Delivery {
    pub start_at: f64,
    pub buffer: playlist::Buffer,
    pub bytes: Option<u64>,
}

impl Delivery {
    /// A session of `want` on a release of `size` bytes: `start_at` as the session will take it, a start at or past
    /// the end being a start from zero.
    pub(crate) fn of(info: &MediaInfo, size: Option<u64>, want: &Want<'_>) -> Delivery {
        Delivery {
            start_at: effective_start(want.start_at, info.duration),
            buffer: buffer_of(want.player),
            bytes: delivered_bytes(info, want).or(size),
        }
    }
}

/// Where a session starts: `start_at`, or 0 for one at or past the end, which starts the title over.
pub(crate) fn effective_start(start_at: f64, duration: f64) -> f64 {
    Some(start_at).filter(|t| *t > 0.0 && *t < duration).unwrap_or(0.0)
}

/// The bytes a session sends of a release: its video, and the audio track `want` gets — as the track is where it is
/// copied, at the AAC encoder's rate where it is converted. `None` where the file doesn't say how big its tracks are.
/// The other audio tracks and the subtitles a Matroska file interleaves with them are never sent.
fn delivered_bytes(info: &MediaInfo, want: &Want<'_>) -> Option<u64> {
    let video = info.video_bytes?;
    let n = match want.audio_track {
        Some(n) if n < info.audio.len() => n,
        _ => crate::lang::pick_audio(&info.audio, want.audio),
    };
    let track = info.audio.get(n)?;
    let (_, out) = audio_plan(&track.codec, track.channels, want.playable);
    let audio = match out.aac_bitrate() {
        Some(rate) => (rate as f64 * info.duration / 8.0) as u64,
        None => track.bytes?,
    };
    Some(video + audio)
}

/// `Need` for a release as `delivery` would send it; `None` when neither its index nor its size and duration say.
pub(crate) fn need(info: &MediaInfo, delivery: &Delivery) -> Option<Need> {
    let segs = playlist::segments(&info.keyframes, info.duration, playlist::TARGET_SECS);
    match playlist::segment_bytes(&segs, &info.byte_index, delivery.bytes) {
        Some(bytes) => {
            let from = start_segment(&segs, delivery.start_at);
            let rate = playlist::needed_rate(&segs, &bytes, from, MAX_PREBUFFER, delivery.buffer);
            Some(Need { bitrate: rate as u64, indexed: true })
        }
        None => average_bitrate(delivery.bytes, info.duration)
            .map(|a| Need { bitrate: (a as f64 * NO_INDEX_HEADROOM) as u64, indexed: false }),
    }
}

/// How long a player on a `max_bitrate` link waits before a copy as `delivery` sends it plays through
/// (`playlist::startup_delay`): what it can take as its pre-buffer target. `None` without a link or a byte index, and
/// where no wait would do.
pub(crate) fn prebuffer(info: &MediaInfo, delivery: &Delivery, max_bitrate: Option<u64>) -> Option<f64> {
    let rate = max_bitrate.filter(|r| *r > 0)? as f64;
    let segs = playlist::segments(&info.keyframes, info.duration, playlist::TARGET_SECS);
    let bytes = playlist::segment_bytes(&segs, &info.byte_index, delivery.bytes)?;
    let from = start_segment(&segs, delivery.start_at);
    Some(playlist::startup_delay(&segs, &bytes, from, rate, delivery.buffer)).filter(|w| w.is_finite())
}

/// Whether a copy of the release needs more than the player's `maxBitrate`. Without one every release fits, and so does
/// one whose need isn't known: nothing says it doesn't.
pub(crate) fn over(max_bitrate: Option<u64>, need: Option<Need>) -> bool {
    max_bitrate.zip(need).is_some_and(|(max, need)| need.bitrate > max)
}

/// A transcode's output size: the source's, fitted inside the preset's with its aspect kept (a 2.4:1 4K film becomes
/// 1920 × 800 at 1080p — 1080 lines of it would be wider than level 4.1 allows), never scaled up, both even. 0 × 0
/// when the source's is unknown.
pub(crate) fn transcode_size(w: u32, h: u32, preset: job::Preset) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (0, 0);
    }
    let scale = (preset.width as f64 / w as f64).min(preset.height as f64 / h as f64).min(1.0);
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
    /// A guest's grant, vouched for by den-edge (`EDGE_SECRET`): the install is as for `Install`, but the sessions
    /// belong to the grant (`grant:<gid>`), are capped by `GUEST_MAX_SESSIONS`, and share the one transcode with every host.
    Guest(String),
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

/// Scout's releases for `id`, ranked for the browser that sent `playable`, its refusals put in this API's terms.
async fn scout_list(
    st: &AppState,
    source: &scout::ScoutSource,
    id: &str,
    by_install: bool,
    playable: Option<&Playable>,
) -> Result<Vec<scout::Stream>, ApiError> {
    let secrets = [source.base.as_str()];
    scout::list(&st.scout_http, source, id, playable).await.map_err(|e| match e.status {
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

/// Whether the player takes HEVC as it is: by `playable`, else by `videoCodecs` (empty is both).
fn player_takes_hevc(playable: Option<&Playable>, video_codecs: &[String]) -> bool {
    let named = video_codecs.is_empty()
        || video_codecs
            .iter()
            .any(|c| matches!(c.to_ascii_lowercase().as_str(), "hevc" | "h265" | "hvc1" | "hev1"));
    playable.map_or(named, Playable::takes_hevc)
}

/// Whether a release plays for the player that asked, as `POST /remux/releases` says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plays {
    /// As it is (or nothing says otherwise).
    Yes,
    /// Only converted on the server's GPU.
    Convert,
    /// Not at all: a session skips it.
    No,
}

impl Plays {
    pub fn as_str(self) -> &'static str {
        match self {
            Plays::Yes => "yes",
            Plays::Convert => "convert",
            Plays::No => "no",
        }
    }
}

/// A listed release and how it plays for the player that asked.
pub struct Verdict {
    pub stream: scout::Stream,
    pub plays: Plays,
    /// A short reason a person can read, where it is not a plain yes.
    pub why: Option<String>,
}

/// What opening a release showed of it, as a verdict for this player: the probe's own answer, by the very tests a
/// session applies to what it opens.
fn probed_verdict(
    info: &MediaInfo,
    playable: Option<&Playable>,
    takes_hevc: bool,
) -> (Plays, Option<String>) {
    if no_picture(playable, takes_hevc, info) {
        let profile = info.dolby_vision.map_or(5, |dv| dv.profile);
        let record = if info.dolby_vision_recordless { " (no record in the file)" } else { "" };
        return (
            Plays::No,
            Some(format!("Dolby Vision profile {profile}{record} — this browser can't show it")),
        );
    }
    if plays(playable, takes_hevc, info) {
        return (Plays::Yes, None);
    }
    let codec = info.codecs.as_deref().unwrap_or("its video");
    match info.video {
        VideoCodec::Hevc => {
            (Plays::Convert, Some(format!("{codec} is converted on the server for this browser")))
        }
        _ => (Plays::No, Some(format!("{codec} is beyond this browser"))),
    }
}

/// A release's verdict before it is opened, from scout's attributes.
fn scouted_verdict(
    a: &scout::Attributes,
    playable: Option<&Playable>,
    takes_hevc: bool,
) -> (Plays, Option<String>) {
    match fit(a, playable, takes_hevc) {
        Fit::Copy => (Plays::Yes, None),
        Fit::Convert if a.dv_profile == 5 => (
            Plays::Convert,
            Some("Dolby Vision profile 5 — this browser can't show it, so it plays only if its picture has a base layer".into()),
        ),
        Fit::Convert => (Plays::Convert, Some("converted on the server for this browser".into())),
        Fit::Never => (Plays::No, Some(format!("{} — this browser can't decode it", a.codec.as_deref().unwrap_or("its video")))),
    }
}

/// `POST /remux/releases`: the releases a session could play, in the order it would try them, so a player can name
/// one (`filename`), each with how it plays for a player that says what it decodes (`playable`, `videoCodecs`):
/// by scout's attributes, or — once a session has opened the release — by what the probe found. The caller gets
/// their labels, names and sizes — never a URL. Without a report every release is `yes`, but one a session found
/// plays in no browser (`AppState::unplayable`).
pub async fn releases(
    st: &Arc<AppState>,
    admission: Admission,
    scout: Option<&str>,
    id: &str,
    playable: Option<&Playable>,
    video_codecs: &[String],
) -> Result<Vec<Verdict>, ApiError> {
    let by_install = !matches!(admission, Admission::Browser(_));
    let key = match admission {
        Admission::Browser(_) => st.cfg.scout_key.clone(),
        Admission::Install | Admission::Guest(_) => None,
    };
    let source = scout::ScoutSource { base: scout_base(st, scout)?, key };
    // Without a capability report no AV1 or VP9: nothing says the player decodes them.
    let list = scout_list(st, &source, id, by_install, playable).await?;
    let candidates = scout::candidates(
        &list,
        None,
        playable.is_some_and(Playable::takes_av1),
        playable.is_some_and(Playable::takes_vp9),
    );
    let reported = playable.is_some() || !video_codecs.is_empty();
    let takes_hevc = player_takes_hevc(playable, video_codecs);
    Ok(candidates
        .into_iter()
        .map(|stream| {
            let nowhere = unplayable_key(id, &stream).and_then(|k| st.unplayable().get(&k, unix_now()));
            let (plays, why) = if let Some(why) = nowhere {
                (Plays::No, Some(why))
            } else if !reported {
                (Plays::Yes, None)
            } else if let Some(info) = st.known().get(&known_key(id, &stream)) {
                probed_verdict(&info, playable, takes_hevc)
            } else {
                scouted_verdict(&stream.attributes, playable, takes_hevc)
            };
            Verdict { stream, plays, why }
        })
        .collect())
}

/// `POST /remux/session`: pick and probe a release, choose its audio track, and set up its session.
pub async fn create(
    st: &Arc<AppState>,
    admission: Admission,
    want: &Want<'_>,
    public: bool,
) -> Result<Arc<Session>, ApiError> {
    let imdb = want.id;
    let base = scout_base(st, want.scout)?;
    let by_install = !matches!(admission, Admission::Browser(_));
    let guest = matches!(admission, Admission::Guest(_));
    let (owner, key, share) = match admission {
        Admission::Browser(b) => (b, st.cfg.scout_key.clone(), 1),
        Admission::Install => (crate::auth::install_id(&base), None, st.cfg.max_sessions_per_install),
        Admission::Guest(o) => (o, None, st.cfg.guest_max_sessions),
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
    let list = scout_list(st, &source, imdb, by_install, want.playable).await?;
    let candidates = scout::candidates(
        &list,
        want.filename,
        want.playable.is_some_and(Playable::takes_av1),
        want.playable.is_some_and(Playable::takes_vp9),
    );
    if candidates.is_empty() {
        return Err(api(
            StatusCode::NOT_FOUND,
            "no_release",
            "No cached release in a codec this service can remux.",
        ));
    }
    let takes_hevc = player_takes_hevc(want.playable, want.video_codecs);
    // Why the release the player named was passed over, where it was: `release.requested` in the answer.
    let mut requested_why: Option<String> = None;
    // Left out of `candidates` by their names, which a person reading the log should see was on purpose.
    for s in list.iter().filter(|s| s.attributes.cached == Some(true) && !s.attributes.three_d) {
        if let Some(ext) = scout::left_out_by_name(s) {
            eprintln!(
                "session: {imdb} skipped \"{}\" unopened: {ext} is not a container this service opens",
                s.attributes.label
            );
            if want.filename == Some(s.filename()) {
                requested_why = Some(format!("{ext} files can't be played here"));
            }
        }
    }
    // What scout says of each release decides, before any is opened, which are tried first — those that play as
    // they are, then those that play only converted, scout's ranking holding within each — and which aren't
    // opened at all. The named release stays first unless it can't play. Scout has already ranked a list for a
    // browser that sent its report in the same order; this is what orders one for a player that sent none.
    let mut ranked: Vec<(Fit, &scout::Stream)> = Vec::new();
    // A transcode is chosen here, before playback, or not at all: never for a player replacing a session mid-film.
    let may_convert = !want.no_transcode && !want.fits_only;
    for c in &candidates {
        if want.exclude.iter().any(|f| f == c.filename()) {
            eprintln!(
                "session: {imdb} skipped \"{}\" unopened: the player is switching away from it",
                c.attributes.label
            );
            continue;
        }
        let remembered = unplayable_key(imdb, c).and_then(|k| st.unplayable().get(&k, unix_now()));
        if let Some(why) = remembered {
            eprintln!("session: {imdb} skipped \"{}\" unopened: remembered: {why}", c.attributes.label);
            if want.filename == Some(c.filename()) {
                requested_why = Some(why);
            }
            continue;
        }
        match fit(&c.attributes, want.playable, takes_hevc) {
            Fit::Never => {
                eprintln!(
                    "session: {imdb} skipped \"{}\" unopened: scout's attributes say it can't play here",
                    c.attributes.label
                );
                if want.filename == Some(c.filename()) {
                    requested_why = scouted_verdict(&c.attributes, want.playable, takes_hevc).1;
                }
            }
            // Only a transcode could play it, and none may be started: not worth opening.
            Fit::Convert if !may_convert => {}
            f => ranked.push((f, c)),
        }
    }
    let named_first = usize::from(ranked.first().is_some_and(|(_, c)| want.filename == Some(c.filename())));
    ranked[named_first..].sort_by_key(|(f, _)| *f);
    let mut no_transcode = false;
    let (mut chosen, fallback, mut too_big) = {
        let source = &source;
        let started = Instant::now();
        let mut queue = ranked.into_iter().peekable();
        let mut opening = FuturesOrdered::new();
        let mut tried = 0;
        let mut chosen = None;
        // The first release that plays only converted: the last resort, taken once nothing tried plays as it is.
        let mut fallback = None;
        // Releases that play as they are but need more than the player's `maxBitrate`, in rank order.
        let mut too_big: Vec<(&scout::Stream, (scout::Resolved, MediaInfo))> = Vec::new();
        loop {
            while opening.len() < PARALLEL_OPENS && tried < MAX_TRIES && started.elapsed() < PICK_BUDGET {
                // With a converted release in hand, one scout says plays only converted can't do better.
                if fallback.is_some() && queue.peek().is_some_and(|(f, _)| *f == Fit::Convert) {
                    break;
                }
                let Some((_, c)) = queue.next() else { break };
                tried += 1;
                opening.push_back(async move {
                    let opened = tokio::time::timeout(OPEN_TIMEOUT, open(st, source, imdb, c)).await;
                    let late = || OpenFailure::passing(format!("not open after {}s", OPEN_TIMEOUT.as_secs()));
                    (c, opened.unwrap_or_else(|_| Err(late())))
                });
            }
            // In the order they were started, which is rank order.
            let Some((c, opened)) = opening.next().await else { break };
            match opened {
                // Dolby Vision profile 5 has no base layer to fall back to: stripped or transcoded, its picture is
                // green and purple. Only a player that shows profile 5 plays it, and only as it is.
                Ok((_, info)) if no_picture(want.playable, takes_hevc, &info) => {
                    let record = if info.dolby_vision_recordless {
                        " (no Dolby Vision record in the container)"
                    } else {
                        ""
                    };
                    eprintln!(
                        "session: {imdb} skipped \"{}\": {}{record} has no picture without Dolby Vision",
                        c.attributes.label,
                        info.dolby_vision.map(|dv| dv.to_string()).unwrap_or_default()
                    );
                    if want.filename == Some(c.filename()) {
                        requested_why = probed_verdict(&info, want.playable, takes_hevc).1;
                    }
                }
                // HEVC the player can't take as it is plays only converted on the GPU: kept as the last resort
                // while the rest are looked through for one that plays untouched — unless the player named this
                // release (another audio track of it), which it keeps.
                Ok((r, info))
                    if info.video == VideoCodec::Hevc && !plays(want.playable, takes_hevc, &info) =>
                {
                    eprintln!(
                        "session: {imdb} \"{}\" ({}) plays here only converted",
                        c.attributes.label,
                        info.codecs.as_deref().unwrap_or("HEVC")
                    );
                    let named = want.filename == Some(c.filename());
                    if named {
                        requested_why = probed_verdict(&info, want.playable, takes_hevc).1;
                    }
                    if fallback.is_none() {
                        fallback = Some((c, (r, info)));
                    }
                    if named {
                        break;
                    }
                }
                // H.264, AV1 or VP9 beyond the player: nothing here makes H.264 smaller, or converts AV1 or VP9 at all.
                Ok((_, info)) if !plays(want.playable, takes_hevc, &info) => {
                    eprintln!(
                        "session: {imdb} skipped \"{}\": {} is beyond this player",
                        c.attributes.label,
                        info.codecs.as_deref().unwrap_or("its video")
                    );
                    if want.filename == Some(c.filename()) {
                        requested_why = probed_verdict(&info, want.playable, takes_hevc).1;
                    }
                }
                // A copy needing more than the player's link carries is kept aside while one that fits is looked for —
                // unless the player named it, which is then decided on below as the only one.
                Ok((r, info))
                    if over(
                        want.max_bitrate,
                        need(&info, &Delivery::of(&info, r.size.or(c.attributes.size_bytes), want)),
                    ) =>
                {
                    eprintln!(
                        "session: {imdb} \"{}\" needs {} — more than the player's {} kbit/s",
                        c.attributes.label,
                        need(&info, &Delivery::of(&info, r.size.or(c.attributes.size_bytes), want))
                            .map_or_else(String::new, |n| n.to_string()),
                        want.max_bitrate.unwrap_or(0) / 1000
                    );
                    let named = want.filename == Some(c.filename());
                    too_big.push((c, (r, info)));
                    if named {
                        break;
                    }
                }
                Ok(found) => {
                    chosen = Some((c, found, None));
                    break;
                }
                Err(failure) => {
                    let why = crate::redact::scrub(&failure.why, &secrets);
                    eprintln!("session: {imdb} skipped \"{}\": {why}", c.attributes.label);
                    if let Some(key) = unplayable_key(imdb, c).filter(|_| failure.lasting) {
                        st.remember_unplayable(key, why);
                    }
                    if want.filename == Some(c.filename()) {
                        requested_why = Some("it could not be opened".into());
                    }
                }
            }
        }
        (chosen, fallback, too_big)
    };
    // Nothing plays as it is with a head start of `MAX_PREBUFFER` or less. What comes next, in order: a copy that asks
    // at most `LONG_PREBUFFER` of it, a transcode, the copy that starves least — and else a refusal, up front, which a
    // retry would not change.
    // The transcode the link carries at its peak; none fits a link below the floor's, and one that would starve ranks
    // below the copy that starves least — taken only where no copy plays at all.
    let fitting = job::preset_for(want.max_bitrate);
    let preset = fitting.unwrap_or(job::SD540);
    let fallback_seen = fallback.is_some();
    let delivery = |t: &(&scout::Stream, (scout::Resolved, MediaInfo))| {
        Delivery::of(&t.1 .1, t.1 .0.size.or(t.0.attributes.size_bytes), want)
    };
    let bitrate =
        |t: &(&scout::Stream, (scout::Resolved, MediaInfo))| need(&t.1 .1, &delivery(t)).map(|n| n.bitrate);
    let head_start = |t: &(&scout::Stream, (scout::Resolved, MediaInfo))| {
        prebuffer(&t.1 .1, &delivery(t), want.max_bitrate).unwrap_or(f64::INFINITY)
    };
    if chosen.is_none() && !want.fits_only {
        let longer = (0..too_big.len())
            .filter(|&i| head_start(&too_big[i]) <= LONG_PREBUFFER)
            .min_by(|&a, &b| head_start(&too_big[a]).total_cmp(&head_start(&too_big[b])));
        if let Some(i) = longer {
            let (c, found) = too_big.swap_remove(i);
            chosen = Some((c, found, None));
        }
    }
    let who = if guest { "a guest" } else { "a member" };
    if chosen.is_none() && may_convert && (fitting.is_some() || too_big.is_empty()) {
        // A conversion at the preset the link takes — of the first release that plays only converted, else of the
        // first HEVC copy too big for the link that the preset comes in under.
        let from_too_big = fallback.is_none();
        let nearest = (0..too_big.len()).min_by_key(|&i| bitrate(&too_big[i]).unwrap_or(u64::MAX));
        let why = match (from_too_big, nearest.map(|i| &too_big[i])) {
            (true, Some(t)) => format!(
                "no copy plays within a {LONG_PREBUFFER:.0}s head start on the {} kbit/s link; the nearest, \"{}\", \
                 needs {}",
                want.max_bitrate.unwrap_or(0) / 1000,
                t.0.attributes.label,
                need(&t.1 .1, &delivery(t)).map_or_else(|| "an unknown rate".to_string(), |n| n.to_string())
            ),
            _ => "no release plays in this player as it is".to_string(),
        };
        let convert = fallback.or_else(|| {
            let hevc = |t: &(&scout::Stream, (scout::Resolved, MediaInfo))| {
                t.1 .1.video == VideoCodec::Hevc && bitrate(t).is_some_and(|b| b > preset.bitrate)
            };
            let i = too_big.iter().position(hevc)?;
            Some(too_big.remove(i))
        });
        if let Some((c, found)) = convert {
            match st.reserve_transcode() {
                Some(slot) => {
                    // Every conversion is the box's GPU for a whole film: the log says why no copy would do.
                    eprintln!(
                        "session: {imdb} converting \"{}\" for {who} to {}p at {} kbit/s (at most {}){}: {why}",
                        c.attributes.label,
                        preset.height,
                        preset.bitrate / 1000,
                        preset.peak / 1000,
                        if fitting.is_none() { ", more than the link carries" } else { "" }
                    );
                    chosen = Some((c, found, Some(slot)));
                }
                // The GPU is off or in use: it still plays as it is, below.
                None if from_too_big => too_big.push((c, found)),
                None => {
                    no_transcode = true;
                    eprintln!(
                        "session: {imdb} can't play \"{}\" for {who}: it needs converting ({why}), and no transcode \
                         is free",
                        c.attributes.label
                    );
                }
            }
        }
    }
    if chosen.is_none() && !want.fits_only {
        // Last, the copy too big for the link that needs the least: a stall now and then beats nothing to play.
        if let Some(i) = (0..too_big.len()).min_by_key(|&i| bitrate(&too_big[i]).unwrap_or(u64::MAX)) {
            let (c, found) = too_big.swap_remove(i);
            chosen = Some((c, found, None));
        }
    }
    let Some((c, (resolved, info), transcode)) = chosen else {
        if want.fits_only {
            return Err(api(
                StatusCode::NOT_FOUND,
                "no_fitting_copy",
                "No other release plays here as it is within what the link carries.",
            ));
        }
        // Only a conversion would have played it: the GPU in use, or none may be started for this request.
        if no_transcode || (fallback_seen && !may_convert) {
            return Err(api(
                StatusCode::NOT_FOUND,
                "no_copy",
                "This can't be played on this device right now: no release plays here as it is, and none can be \
                 converted now.",
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
    let source_channels = info.audio[audio].channels;
    let (audio_copy, audio_out) = audio_plan(&info.audio[audio].codec, source_channels, want.playable);

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
    let start_at = effective_start(want.start_at, info.duration);
    let first_segment = start_segment(&segments, start_at);
    let codecs = info.codecs.clone().unwrap_or_else(|| {
        // A file without a codec configuration record; name the commonest profile rather than none.
        match info.video {
            VideoCodec::Hevc => "hvc1.1.6.L150.90",
            VideoCodec::Av1 => "av01.0.08M.08",
            _ => "avc1.640028",
        }
        .to_string()
    });
    let size = resolved.size.or(c.attributes.size_bytes);
    // A player that named a release and got another is told why, where this knows: what it saw of the named one
    // just now, or of an earlier opening.
    let requested = want.filename.filter(|f| *f != c.filename()).and_then(|f| {
        let why = requested_why.or_else(|| {
            let info = st.known().get(&format!("{imdb}/{f}"))?;
            probed_verdict(&info, want.playable, takes_hevc).1
        })?;
        Some(Requested { filename: f.to_string(), why })
    });
    let (peak, avg) = playlist::bandwidth(size, info.duration);
    let (codecs, resolution, (peak, avg)) = match transcode {
        Some(_) => (
            job::TRANSCODE_CODECS.to_string(),
            transcode_size(info.width, info.height, preset),
            (preset.peak + 192_000, preset.bitrate),
        ),
        None => (codecs, (info.width, info.height), (peak, avg)),
    };
    // Dolby Vision stays in a copy for a player that shows it; everywhere else, and in every transcode, the base
    // layer plays alone.
    let kept_dv = kept_dolby_vision(&info, transcode.is_some(), want.playable);
    let dovi = match (info.dolby_vision, kept_dv) {
        (None, _) => job::Dovi::Absent,
        (Some(_), None) => job::Dovi::Strip,
        (Some(_), Some(_)) => job::Dovi::Keep,
    };
    let (codecs, dv_signal) = match kept_dv.and_then(dolby_vision_signal) {
        Some((own, supplemental, range)) => {
            (own.unwrap_or(codecs), Some(playlist::DolbyVision { supplemental, range }))
        }
        None if transcode.is_none() => {
            let range = copied_range(&info, &codecs);
            (codecs, range.map(|range| playlist::DolbyVision { supplemental: None, range }))
        }
        None => (codecs, None),
    };
    let renditions = crate::subs::plan(&sub_langs, sub_base.is_some(), &info.subtitles);
    let text_tracks: Vec<usize> = renditions.iter().filter_map(|r| r.own).collect();
    let subs = sub_base.map(|base| crate::subs::Subs {
        base,
        head_sum: resolved.head.get(..crate::subs::HASH_CHUNK as usize).map(crate::subs::chunk_sum),
        cache: Default::default(),
    });
    let named: Vec<(String, String)> =
        renditions.iter().map(|r| (r.lang.clone(), crate::lang::name(&r.lang))).collect();
    let opened_key = opened_key(&source.base, c);
    // A transcode's keyframes are its encoder's IDRs, in closed GOPs; a copy's are the release's own.
    let closed_gops = transcode.is_some() || info.closed_gops;
    let (prebuffer, need, demand) = match transcode {
        Some(_) => (want.max_bitrate.map(|rate| preset.head_start(rate)), None, None),
        None => {
            let delivery = Delivery::of(&info, size, want);
            let bytes = playlist::segment_bytes(&segments, &info.byte_index, delivery.bytes);
            (
                prebuffer(&info, &delivery, want.max_bitrate),
                need(&info, &delivery),
                bytes.map(|b| segments.iter().zip(b).map(|(s, b)| (s.start, b.round() as u64)).collect()),
            )
        }
    };
    let session = Arc::new(Session {
        prebuffer,
        need,
        demand,
        master: playlist::master(
            &codecs,
            dv_signal.as_ref(),
            &match (audio_copy, audio_out) {
                (Some(codec), _) => playlist::Audio::Copy {
                    codec,
                    channels: source_channels,
                    language: info.audio[audio].language.as_deref(),
                },
                (None, job::AudioOut::Surround | job::AudioOut::Surround71) => playlist::Audio::AacSurround {
                    channels: audio_out.channels(source_channels),
                    language: info.audio[audio].language.as_deref(),
                },
                (None, _) => playlist::Audio::Aac,
            },
            peak,
            avg,
            Some(resolution),
            info.frame_rate,
            &named,
            closed_gops,
        ),
        media: playlist::media(&segments, start_at, closed_gops),
        reports: AtomicU8::new(0),
        speed_probes: AtomicU8::new(0),
        sid: sid.clone(),
        sig,
        exp,
        owner,
        public,
        started: Instant::now(),
        imdb: imdb.to_string(),
        dir,
        release: Release {
            label: c.attributes.label.clone(),
            filename: c.filename().to_string(),
            size,
            requested,
        },
        segments,
        play_url: c.url.clone(),
        source,
        opened_key,
        info,
        audio,
        audio_copy,
        audio_out,
        audio_channels: audio_out.channels(source_channels),
        dovi,
        subs,
        renditions,
        text_tracks,
        own: Default::default(),
        transcoded: transcode.is_some(),
        preset,
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
    let link = want.max_bitrate.map(|b| format!(", link {} kbit/s", b / 1000)).unwrap_or_default();
    let client = &want.client;
    eprintln!(
        "session {}: {imdb} \"{}\" ({}, {:?} {}{}{}, {:.0}s, {} keyframes, {} segments, audio {} {}{}; player: {player}{link}; client: {client})",
        session.short(),
        session.release.label,
        session.info.container,
        session.info.video,
        codecs,
        dv.unwrap_or_default(),
        match (session.transcoded, tonemaps(&session.info)) {
            (true, true) => format!(" → {}p H.264 SDR on the GPU", session.preset.height),
            (true, false) => format!(" → {}p H.264 on the GPU", session.preset.height),
            (false, true) => ", HDR".to_string(),
            (false, false) => String::new(),
        },
        session.info.duration,
        session.info.keyframes.len(),
        session.segments.len(),
        session.audio,
        session.info.audio[session.audio].language.as_deref().unwrap_or("und"),
        match (session.audio_copy, session.audio_out) {
            (Some(c), _) => format!(" copied as {c}"),
            (None, job::AudioOut::Surround) => " as AAC 5.1".to_string(),
            (None, job::AudioOut::Surround71) => " as AAC 7.1".to_string(),
            (None, _) => String::new(),
        },
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
        }
    }

    #[test]
    fn a_copied_hdr_variant_names_its_range() {
        let hdr10 = info(VideoCodec::Hevc, "hvc1.2.4.L153.B0", true);
        assert_eq!(
            copied_range(&hdr10, "hvc1.2.4.L153.B0"),
            Some("PQ"),
            "HDR HEVC without a transfer is HDR10"
        );
        let hlg = crate::probe::MediaInfo { hlg: true, ..hdr10.clone() };
        assert_eq!(copied_range(&hlg, "hvc1.2.4.L153.B0"), Some("HLG"));
        let sdr = info(VideoCodec::Hevc, "hvc1.1.6.L120.90", false);
        assert_eq!(copied_range(&sdr, "hvc1.1.6.L120.90"), None, "SDR names no range");
        let av1 = info(VideoCodec::Av1, "av01.0.13M.10.0.110.09.16.09.0", true);
        assert_eq!(copied_range(&av1, "av01.0.13M.10.0.110.09.16.09.0"), Some("PQ"));
        assert_eq!(copied_range(&info(VideoCodec::H264, "avc1.640028", false), "avc1.640028"), None);
        let hlg_vp9 = crate::probe::MediaInfo {
            hlg: true,
            ..info(VideoCodec::Vp9, "vp09.02.51.10.01.09.18.09.00", true)
        };
        assert_eq!(copied_range(&hlg_vp9, "vp09.02.51.10.01.09.18.09.00"), Some("HLG"));
        assert_eq!(copied_range(&info(VideoCodec::Vp9, "vp09.00.41.08", false), "vp09.00.41.08"), None);
    }

    #[test]
    fn vp9_plays_as_it_is_in_the_profile_the_player_reports() {
        let chrome = Playable { h264: 0x33, vp9: true, vp9_profile2: true, ..Playable::default() };
        let p0 = info(VideoCodec::Vp9, "vp09.00.41.08", false);
        let p2 = info(VideoCodec::Vp9, "vp09.02.51.10.01.09.16.09.00", true);
        assert!(chrome.takes(&p0) && chrome.takes(&p2));
        assert!(!Playable { vp9: false, ..chrome }.takes(&p0), "profile 2 only");
        assert!(!Playable { vp9_profile2: false, ..chrome }.takes(&p2), "profile 0 only");
        assert!(!chrome.takes(&info(VideoCodec::Vp9, "vp09.02.51.12", false)), "12-bit: never asked about");
        assert!(!chrome.takes(&info(VideoCodec::Vp9, "vp09.01.41.08.03.01.01.01.00", false)), "profile 1");
        // Every player now gets the report as the browser gave it. Profile 2 was cleared for a native session
        // while 10-bit VP9 in Apple's own player was unmeasured; codec lab played it there on an iPhone (842
        // frames of a thirty-second clip) and in every path on macOS Safari (oxyc/den#37).
        let sent = serde_json::to_value(chrome).unwrap();
        assert!(sent["vp9"] == true && sent["vp9Profile2"] == true, "scout is sent it as it holds: {sent}");
        assert!(chrome.to_string().ends_with("AV1 10-bit L0, VP9, VP9 profile 2"), "{chrome}");

        let a = |bit_depth| scout::Attributes { codec: Some("vp9".into()), bit_depth, ..Default::default() };
        assert_eq!(fit(&a(8), Some(&chrome), false), Fit::Copy, "VP9 the player decodes is copied");
        assert_eq!(
            fit(&a(10), Some(&Playable { vp9_profile2: false, ..chrome }), false),
            Fit::Never,
            "8-bit only"
        );
        assert_eq!(fit(&a(8), Some(&Playable { vp9: false, ..chrome }), false), Fit::Never, "10-bit only");
        let ten_bit_only = Playable { vp9: false, ..chrome };
        assert_eq!(
            fit(&a(0), Some(&ten_bit_only), false),
            Fit::Copy,
            "a depth nobody read: the probe decides"
        );
        let no_vp9 = Playable { vp9: false, vp9_profile2: false, ..chrome };
        assert_eq!(fit(&a(0), Some(&no_vp9), false), Fit::Never, "and never converted");
        assert_eq!(fit(&a(0), None, true), Fit::Never, "without a report nothing says it plays");
    }

    #[test]
    fn flac_is_copied_and_7_1_stays_7_1_where_the_player_says_so() {
        use job::AudioOut as Out;
        let chrome =
            Playable { h264: 0x33, aac_multichannel: true, flac: true, aac71: true, ..Playable::default() };
        assert_eq!(audio_plan("A_FLAC", 6, Some(&chrome)), (Some("fLaC"), Out::Copy));
        assert_eq!(audio_plan("fLaC", 2, Some(&chrome)), (Some("fLaC"), Out::Copy), "MP4's name for it");
        assert_eq!(audio_plan("A_FLAC", 6, Some(&Playable { flac: false, ..chrome })), (None, Out::Surround));
        assert_eq!(audio_plan("A_TRUEHD", 8, Some(&chrome)), (None, Out::Surround71));
        let no_71 = Playable { aac71: false, ..chrome };
        assert_eq!(audio_plan("A_DTS", 8, Some(&no_71)), (None, Out::Surround), "7.1 folds down to 5.1");
        assert_eq!(audio_plan("A_DTS", 6, Some(&chrome)), (None, Out::Surround), "5.1 stays 5.1");
        assert_eq!(
            audio_plan("A_EAC3", 8, Some(&chrome)),
            (None, Out::Surround71),
            "no eac3, no E-AC-3 copy"
        );
        assert_eq!(
            audio_plan("A_EAC3", 6, Some(&Playable { eac3: true, ..chrome })),
            (Some("ec-3"), Out::Copy)
        );
        assert_eq!(audio_plan("A_FLAC", 8, None), (None, Out::Stereo), "no report: stereo, as before");
        assert!(chrome.to_string().ends_with("AAC 5.1, AAC 7.1, FLAC"), "{chrome}");
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
        assert_eq!(
            phone.to_string(),
            "H.264 L51, High 10 L0, HEVC L153, 10-bit L153, High tier L0, HDR, AV1 L0, AV1 10-bit L0"
        );
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
    fn av1_plays_as_it_is_at_the_levels_depth_and_hdr_the_player_reports() {
        let chrome = Playable { h264: 0x33, av1: 13, av1_main10: 13, av1_hdr: true, ..Playable::default() };
        let hdr10 = info(VideoCodec::Av1, "av01.0.12M.10.0.110.09.16.09.0", true);
        assert!(chrome.takes(&hdr10));
        assert!(!Playable { av1_hdr: false, ..chrome }.takes(&hdr10), "PQ it can't decode is passed over");
        assert!(!Playable { av1_main10: 0, ..chrome }.takes(&hdr10), "8-bit AV1 only");
        assert!(!Playable { av1_main10: 9, ..chrome }.takes(&hdr10), "1080p at most");
        let sdr = info(VideoCodec::Av1, "av01.0.08M.08", false);
        assert!(Playable { av1: 0, av1_main10: 8, ..chrome }.takes(&sdr), "a 10-bit decoder takes 8-bit");
        assert!(!Playable { av1: 0, av1_main10: 0, ..chrome }.takes(&sdr));
        assert!(
            !chrome.takes(&info(VideoCodec::Av1, "av01.0.13H.10", false)),
            "High tier: never asked about"
        );
        let high = info(VideoCodec::Av1, "av01.1.08M.08.0.000.01.01.01.0", false);
        assert!(!chrome.takes(&high), "High profile, 4:4:4");
        assert!(!chrome.takes(&info(VideoCodec::Av1, "av01.2.08M.12", false)), "12-bit");
        assert!(chrome.to_string().ends_with("AV1 L13, AV1 10-bit L13, AV1 HDR"), "{chrome}");
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
    fn a_contradictory_profile5_record_always_uses_the_hdr_base_layer() {
        let mut release = info(VideoCodec::Hevc, "hvc1.2.4.L153.B0", true);
        release.dolby_vision = Some(crate::probe::DolbyVision { profile: 5, compat: 0, level: 6 });
        release.dolby_vision_record_mismatch = true;
        let player = Playable {
            hevc_main10: 153,
            hdr: true,
            dolby_vision: DolbyVisionPlay { p5: true, p8: true },
            ..Playable::default()
        };
        assert_eq!(
            kept_dolby_vision(&release, false, Some(&player)),
            None,
            "never signal the false P5 record"
        );
        let want = Want {
            id: "tt1",
            filename: None,
            scout: None,
            audio: &[],
            audio_track: None,
            subtitles: None,
            subtitle_languages: &[],
            video_codecs: &[],
            playable: Some(&player),
            start_at: 0.0,
            max_bitrate: None,
            client: "test".into(),
            player: None,
            exclude: &[],
            no_transcode: false,
            fits_only: false,
        };
        assert!(!no_picture(want.playable, true, &release), "the BT.2020 base layer has a picture");
    }

    #[test]
    fn a_recordless_profile5_plays_only_as_it_is_for_a_player_that_shows_profile5() {
        let mut release = info(VideoCodec::Hevc, "hvc1.2.4.L153.B0", false);
        release.dolby_vision = Some(crate::probe::DolbyVision { profile: 5, compat: 0, level: 6 });
        release.dolby_vision_recordless = true;
        let safari = &Playable {
            hevc_main10: 153,
            hdr: true,
            dolby_vision: DolbyVisionPlay { p5: true, p8: true },
            ..Playable::default()
        };
        let chrome = &Playable { hevc_main10: 153, hdr: true, ..Playable::default() };
        let no_hevc =
            &Playable { dolby_vision: DolbyVisionPlay { p5: true, p8: true }, ..Playable::default() };
        // Safari keeps it: its master names `dvh1.05.06` and VIDEO-RANGE=PQ, and the copy keeps its RPUs.
        assert!(!no_picture(Some(safari), true, &release));
        let kept = kept_dolby_vision(&release, false, Some(safari)).expect("kept as it is");
        assert_eq!(dolby_vision_signal(kept), Some((Some("dvh1.05.06".into()), None, "PQ")));
        // Anywhere else it is refused, never stripped and never converted: profile 5 has no picture without its RPU.
        assert!(no_picture(Some(chrome), true, &release));
        assert!(
            no_picture(Some(no_hevc), false, &release),
            "a player without HEVC is not offered a conversion"
        );
        assert_eq!(probed_verdict(&release, Some(safari), true), (Plays::Yes, None));
        let (plays, why) = probed_verdict(&release, Some(chrome), true);
        assert_eq!(plays, Plays::No);
        assert_eq!(
            why.unwrap(),
            "Dolby Vision profile 5 (no record in the file) — this browser can't show it"
        );
        assert_eq!(kept_dolby_vision(&release, true, Some(safari)), None, "a transcode drops Dolby Vision");
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
        let p5 = scout::Attributes { dv_profile: 5, hdr: true, ..a("hevc") };
        assert_eq!(
            fit(&p5, p, true),
            Fit::Convert,
            "opened last so a false P5 record can be corrected by the probe"
        );
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
        let chrome = Playable { h264: 0x33, av1: 13, av1_main10: 13, ..Playable::default() };
        assert_eq!(fit(&a("av1"), Some(&chrome), false), Fit::Copy, "AV1 the player decodes is copied");
        assert_eq!(fit(&a("av1"), Some(&firefox), false), Fit::Never, "and never converted");
        assert_eq!(fit(&a("av1"), None, true), Fit::Never, "without a report nothing says it plays");
        let ten_bit_av1 = scout::Attributes { bit_depth: 10, ..a("av1") };
        assert_eq!(
            fit(&ten_bit_av1, Some(&Playable { av1_main10: 0, ..chrome }), false),
            Fit::Never,
            "8-bit only"
        );
        let uhd_av1 = scout::Attributes { resolution: Some("2160p".into()), ..ten_bit_av1 };
        assert_eq!(
            fit(&uhd_av1, Some(&Playable { av1_main10: 9, ..chrome }), false),
            Fit::Never,
            "1080p at most"
        );
        assert_eq!(fit(&uhd_av1, Some(&chrome), false), Fit::Copy);
        let hdr_av1 = scout::Attributes { hdr: true, ..uhd_av1 };
        assert_eq!(fit(&hdr_av1, Some(&chrome), false), Fit::Never, "HDR without av1Hdr");
        assert_eq!(fit(&hdr_av1, Some(&Playable { av1_hdr: true, ..chrome }), false), Fit::Copy);
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
    fn a_transcode_comes_down_to_its_preset_with_its_aspect() {
        let hd = job::HD1080;
        assert_eq!(transcode_size(3840, 2160, hd), (1920, 1080));
        assert_eq!(transcode_size(3840, 1600, hd), (1920, 800), "a wide film is fitted to the width");
        assert_eq!(transcode_size(4096, 2160, hd), (1920, 1012));
        assert_eq!(transcode_size(1440, 1080, hd), (1440, 1080));
        assert_eq!(transcode_size(1280, 720, hd), (1280, 720), "never scaled up");
        assert_eq!(transcode_size(1920, 803, hd), (1920, 802), "both even");
        assert_eq!(transcode_size(0, 0, hd), (0, 0));
        assert_eq!(transcode_size(3840, 2160, job::HD720), (1280, 720));
        assert_eq!(transcode_size(3840, 1600, job::HD720), (1280, 532));
    }

    /// Twelve minutes with a keyframe every 2 s: two cheap minutes at 1 Mbit/s, two heavy ones at 10, then eight cheap
    /// ones again — the shape of a film that opens on titles and then cuts to its first real scene.
    fn front_loaded() -> (crate::probe::MediaInfo, u64) {
        let keyframes: Vec<f64> = (0..360).map(|i| i as f64 * 2.0).collect();
        let mut at = 0u64;
        let byte_index: Vec<(f64, u64)> = keyframes
            .iter()
            .map(|&t| {
                let here = at;
                at += if (120.0..240.0).contains(&t) { 2_500_000 } else { 250_000 };
                (t, here)
            })
            .collect();
        let info = crate::probe::MediaInfo {
            duration: 720.0,
            keyframes,
            byte_index,
            ..info(VideoCodec::H264, "avc1.640028", false)
        };
        (info, at)
    }

    /// Delivering all of `size` bytes from `start_at` to a player that buffers as `player` does.
    fn delivery(size: u64, start_at: f64, player: Option<&str>) -> Delivery {
        Delivery { start_at, buffer: buffer_of(player), bytes: Some(size) }
    }

    #[test]
    fn a_link_that_covers_the_average_but_not_the_heavy_stretch_is_refused() {
        let (info, size) = front_loaded();
        assert_eq!(average_bitrate(Some(size), info.duration), Some(2_500_000));
        // hls.js holds two minutes and 150 MB: all of the heavy stretch. Everything to its end, 1320 Mbit, must be in by
        // the time its last 6 s segment plays (234 s) plus the 10 s allowed: 1320 / 244.
        let deep = delivery(size, 0.0, Some("hls.js"));
        let from_start = need(&info, &deep).unwrap();
        assert!(from_start.indexed);
        assert!((5_400_000..5_420_000).contains(&from_start.bitrate), "{from_start:?}");
        assert!(over(Some(4_000_000), Some(from_start)), "4 Mbit/s carries the average, not the film");
        assert!(!over(Some(6_000_000), Some(from_start)));
        let wait = prebuffer(&info, &deep, Some(6_000_000)).unwrap();
        assert!((0.0..=MAX_PREBUFFER).contains(&wait), "{wait}");
        // At 4 Mbit/s even two minutes ahead can't carry the 1200 Mbit stretch (it needs 5.1): no wait up front would do.
        assert_eq!(prebuffer(&info, &deep, Some(4_000_000)), None);
        assert!(prebuffer(&info, &deep, Some(5_300_000)).unwrap() > MAX_PREBUFFER);
        // Resumed at the heavy stretch, the cheap opening no longer builds a lead: 6 Mbit/s isn't enough there.
        let resumed = need(&info, &delivery(size, 121.0, Some("hls.js"))).unwrap();
        assert!(resumed.bitrate > 9_000_000, "{resumed:?}");
        assert!(over(Some(6_000_000), Some(resumed)));
    }

    #[test]
    fn a_player_that_holds_thirty_seconds_needs_the_heavy_stretch_carried_as_it_plays() {
        let (info, size) = front_loaded();
        // Safari's thirty seconds ahead: the stretch's 1200 Mbit come in 30 s plus the 114 s between its first and last
        // segment's starts, whatever the cheap opening did.
        let native = delivery(size, 0.0, Some("native"));
        let held = need(&info, &native).unwrap();
        assert!((8_300_000..8_350_000).contains(&held.bitrate), "{held:?}");
        assert!(over(Some(6_000_000), Some(held)), "what carries it for hls.js starves Safari");
        assert_eq!(prebuffer(&info, &native, Some(6_000_000)), None, "and no wait up front would do");
        assert!(prebuffer(&info, &native, Some(9_000_000)).is_some_and(|w| w <= MAX_PREBUFFER));
        assert_eq!(
            buffer_of(None),
            buffer_of(Some("native")),
            "a player that says nothing is taken for the least"
        );
    }

    #[test]
    fn the_demand_is_what_the_session_sends_not_every_track_in_the_file() {
        let (info, size) = front_loaded();
        // Half the file is video; the rest, two 3 Mbit/s audio tracks.
        let track = |bytes| crate::probe::AudioTrack {
            codec: "A_EAC3".into(),
            language: Some("eng".into()),
            channels: 6,
            name: None,
            default: true,
            commentary: false,
            bytes: Some(bytes),
        };
        let info = crate::probe::MediaInfo {
            video_bytes: Some(size / 2),
            audio: vec![track(size / 4), track(size / 4)],
            ..info
        };
        let want = |playable| Want { playable, player: Some("hls.js"), ..test_want() };
        // Copied, the one track played: three quarters of the file.
        let eac3 = Playable { eac3: true, ..Default::default() };
        assert_eq!(Delivery::of(&info, Some(size), &want(Some(&eac3))).bytes, Some(size / 2 + size / 4));
        // Converted to stereo AAC: the video and 192 kbit/s for 720 s.
        assert_eq!(Delivery::of(&info, Some(size), &want(None)).bytes, Some(size / 2 + 17_280_000));
        let whole = need(&info, &delivery(size, 0.0, Some("hls.js"))).unwrap().bitrate;
        let sent = need(&info, &Delivery::of(&info, Some(size), &want(None))).unwrap().bitrate;
        assert!(sent < whole * 6 / 10, "{sent} of {whole}");
        // Where the file doesn't say how big its tracks are, the whole file stands in.
        let untold = crate::probe::MediaInfo { video_bytes: None, ..info.clone() };
        assert_eq!(Delivery::of(&untold, Some(size), &want(None)).bytes, Some(size));
    }

    #[test]
    fn a_resume_past_the_end_is_weighed_from_the_start() {
        let (info, size) = front_loaded();
        let want_at = |start_at| Delivery::of(&info, Some(size), &Want { start_at, ..test_want() }).start_at;
        assert_eq!(want_at(5_000.0), 0.0, "a start the session will make from zero");
        assert_eq!(want_at(720.0), 0.0);
        assert_eq!(want_at(121.0), 121.0);
    }

    fn test_want() -> Want<'static> {
        Want {
            id: "tt1",
            filename: None,
            scout: None,
            audio: &[],
            audio_track: None,
            subtitles: None,
            subtitle_languages: &[],
            video_codecs: &[],
            playable: None,
            start_at: 0.0,
            max_bitrate: None,
            client: "test".into(),
            player: None,
            exclude: &[],
            no_transcode: false,
            fits_only: false,
        }
    }

    #[test]
    fn with_no_byte_index_a_copy_needs_its_average_and_half_again() {
        let plain = info(VideoCodec::H264, "avc1.640028", false);
        let plain = crate::probe::MediaInfo { duration: 30.0, ..plain };
        // 3.75 MB over 30 s is 1 Mbit/s.
        let n = need(&plain, &delivery(3_750_000, 0.0, None)).unwrap();
        assert_eq!(n, Need { bitrate: 1_500_000, indexed: false });
        assert!(n.to_string().contains("no byte index"), "the log says it is a guess: {n}");
        assert!(!over(None, Some(n)), "no maxBitrate: every release fits");
        assert!(!over(Some(1_500_000), Some(n)), "at the limit fits");
        assert!(over(Some(1_499_999), Some(n)));
        let unknown = Delivery { bytes: None, ..delivery(0, 0.0, None) };
        assert_eq!(need(&plain, &unknown), None, "an unknown size says nothing");
        assert!(!over(Some(1), None), "and nothing said fits");
        assert_eq!(
            need(&crate::probe::MediaInfo { duration: 0.0, ..plain.clone() }, &delivery(1, 0.0, None)),
            None
        );
        assert_eq!(
            prebuffer(&plain, &delivery(3_750_000, 0.0, None), Some(1)),
            None,
            "no index, no pre-buffer"
        );
    }

    #[test]
    fn snapping_pins_drift_to_the_real_keyframes() {
        let kf = [0.0, 2.5, 5.0];
        assert_eq!(snap(&kf, 2.500_004), 2.5);
        assert_eq!(snap(&kf, 4.99), 5.0);
        assert_eq!(snap(&kf, 3.7), 3.7, "a keyframe the index does not list keeps its own time");
    }

    #[test]
    fn media_is_kept_until_the_session_expires_and_never_ranged() {
        let path = std::env::temp_dir().join(format!("den-remux-media-{}", std::process::id()));
        std::fs::write(&path, b"12345").unwrap();
        let exp = unix_now() + 3600;
        let r = media_response(open_parts(std::slice::from_ref(&path)).unwrap(), false, exp);
        let _ = std::fs::remove_file(&path);
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()["content-type"], "video/mp4");
        assert_eq!(r.headers()["content-length"], "5");
        assert_eq!(r.headers()["accept-ranges"], "none");
        let cc = r.headers()["cache-control"].to_str().unwrap();
        let age: u64 = cc
            .strip_prefix("private, max-age=")
            .and_then(|v| v.strip_suffix(", immutable"))
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("{cc}"));
        assert!((3598..=3600).contains(&age), "the session's remaining life: {cc}");
        assert_eq!(busy().headers()["cache-control"], "no-store", "not ready is never kept");
        assert_eq!(gone().headers()["cache-control"], "no-store");
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
        assert_eq!(sub_file("sub0.m3u8"), Some(SubFile::Playlist(0)));
        assert_eq!(sub_file("sub7.vtt"), Some(SubFile::Document(7)));
        assert_eq!(sub_file("sub3_12.vtt"), Some(SubFile::Window(3, 12)));
        for not in
            ["sub8.vtt", "sub.vtt", "sub0_.vtt", "sub_1.vtt", "sub0_+1.vtt", "sub0_12345678.vtt", "sub01.vtt"]
        {
            assert_eq!(sub_file(not), None, "{not}");
        }
        assert!(is_session_file("sub2_0.vtt") && !is_session_file("sub2_0.m3u8"));
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

    /// A segment asked for again after the job has gone past it and its GOPs were pruned — a seek back, or a player
    /// with no cache of its own — gets a new job. It used to be left to the running one, which never came back, and
    /// the request answered 503 after its full wait.
    #[test]
    fn a_job_past_a_pruned_segment_is_not_waited_on() {
        let segs: Vec<Segment> =
            (0..6).map(|k| Segment { start: k as f64 * 6.0, end: (k + 1) as f64 * 6.0 }).collect();
        let heads = |n, start, next, gops: &[Gop]| Session::heads_for(&segs, n, 0.0, 1, start, next, gops);
        let later = [gop(1, 7, 18.0, 6.0), gop(1, 8, 24.0, 6.0)];
        assert!(!heads(1, 0.0, 30.0, &later), "past segment 1 with its GOPs pruned");
        assert!(
            heads(1, 0.0, 30.0, &[gop(1, 1, 6.0, 6.0), gop(1, 7, 18.0, 6.0)]),
            "past it, but it is still kept"
        );
        assert!(heads(1, 0.0, 8.0, &[]), "part way through it");
        assert!(heads(3, 0.0, 6.0, &[]), "a little short of it");
        assert!(!heads(5, 0.0, 6.0, &[]), "too far short of it");
        assert!(!heads(1, 12.0, 14.0, &[]), "started after it");
        // The last segment: its end is the job's, and its GOPs stay kept while the job winds up.
        assert!(heads(5, 0.0, 36.0, &[gop(1, 9, 30.0, 6.0)]), "the last segment waits for its job to finish");
        // A file whose first keyframe is late: segment 0 still starts at 0, and a job begun on that keyframe is
        // heading for it rather than restarted on every look.
        assert!(
            Session::heads_for(&segs, 0, 0.5, 1, 0.5, 0.5, &[]),
            "segment 0 behind a late first keyframe"
        );
        assert!(!Session::heads_for(&segs, 0, 0.5, 1, 6.0, 6.0, &[]), "but not a job begun past it");
        assert!(!heads(1, 0.0, 30.0, &[gop(2, 1, 6.0, 6.0)]), "a GOP of another job is not this job's");
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
