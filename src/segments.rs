//! Intro and credits detection for series episodes (oxyc/den#227): Jellyfin Intro Skipper's audio-fingerprint
//! method, on the box, for the shows SkipDB has no times for.
//!
//!   POST /remux/segments/scan {imdb, season, episode, scout?}   X-Den-Segments-Key, or a valid `scout` install
//!        → 202 {status: queued|running|done|none} | 503 {status: busy} | 422 {status: no_install}
//!   GET  /remux/segments/<tt>/<season>/<episode>
//!        → 200 {duration, intro, credits, method, scanned_at} | 202 {status: pending} | 404 {status: unknown}
//!
//! A scan resolves the episode through the scout install the request brought (held in memory, never stored or
//! logged), else this service's own (`SCOUT_INSTALL_URL`), takes the smallest
//! cached release, and has ffmpeg decode only the two windows it needs — the intro window from the start and the
//! credits window at the end — to 11025 Hz mono PCM on a pipe. An input seek (`-ss` before `-i`) reads just those
//! byte ranges, through the session door (`source`), so the link never reaches ffmpeg's command line. The PCM is
//! fingerprinted as it arrives (rusty-chromaprint, the same algorithm as `fpcalc`), never held whole and never
//! written to disk.
//!
//! Two episodes of a season share their intro and their end credits, so the longest stretch of matching
//! fingerprint points between their windows is the segment in both. Scanning episode N therefore also scans N+1
//! (N−1 where there is no N+1) and stores both. Once a season has a match, the matched points are kept as the
//! season's fingerprint, and every later episode is matched against that alone: one episode, one scan.
//!
//! One scan runs at a time, never while a transcode is running, on a bounded queue with a daily budget.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use hyper::{Response, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::httputil::{self, Body};
use crate::state::{unix_now, AppState};

// ---- matching -------------------------------------------------------------------------------------------------
//
// Thresholds are Intro Skipper's (github.com/intro-skipper/intro-skipper, IntroSkipper/Analyzers/
// ChromaprintAnalyzer.cs, Configuration/PluginConfiguration.cs and Data/ChromaprintConstants.cs, read 2026-10):
//   MaximumFingerprintPointDifferences = 6   two points match when at most 6 of their 32 bits differ
//   MaximumTimeSkip = 3.5 s                   a run survives a gap of non-matching points up to this long
//   InvertedIndexShift = 2                    candidate offsets come from points equal to another's ±2
//   MinimumIntroDuration = 15, MaximumIntroDuration = 120
//   MinimumCreditsDuration = 15; credits at most the window less 1 s ("don't allow perfect matches")
//   a region starting within 5 s of the start snaps to 0 (SelectSharedRegion); an end within EndSnapThreshold
//   (2 s) of the file's end snaps to it (TimeAdjustmentHelper)
//   SampleDuration = 4096 / 11025 / 3 s       one point per a third of a 4096-sample frame at 11025 Hz
// The windows differ by request: the intro window is 25 % of the runtime capped at 6 min (Intro Skipper's cap is
// 10), and the credits window is the last 8 min (its default is 450 s).

/// Seconds of audio per fingerprint point.
pub const POINT_SECS: f64 = 4096.0 / 11025.0 / 3.0;
/// The sample rate the fingerprint is computed at; ffmpeg resamples to it so the fingerprinter does not.
const SAMPLE_RATE: u32 = 11025;
const MAX_BIT_DIFF: u32 = 6;
const MAX_GAP_SECS: f64 = 3.5;
const INDEX_SHIFT: i32 = 2;
const INTRO_MIN_SECS: f64 = 15.0;
const INTRO_MAX_SECS: f64 = 120.0;
/// Once a strict intro match establishes an alignment, follow a differently mixed or fading copy of the same
/// music a little farther. Random 32-bit fingerprints differ in 16 bits on average; an average of 12 across a
/// one-second block is deliberately too loose to discover a match, but selective enough to refine its tail.
const INTRO_TAIL_MAX_SECS: f64 = 12.0;
const INTRO_TAIL_BLOCK_SECS: f64 = 1.0;
const INTRO_TAIL_MAX_AVG_BIT_DIFF: u32 = 12;
const FINGERPRINT_METHOD: &str = "fingerprint-v3";
const CREDITS_MIN_SECS: f64 = 15.0;
const CREDITS_MAX_SECS: f64 = 480.0;
const INTRO_WINDOW_SHARE: f64 = 0.25;
const INTRO_WINDOW_MAX_SECS: f64 = 360.0;
const CREDITS_WINDOW_SECS: f64 = 480.0;
const START_SNAP_SECS: f64 = 5.0;
/// Chromaprint's filters need roughly this much following audio before they emit a point. The final matching
/// point consequently describes audio through this far beyond its timestamp; without restoring that coverage,
/// every detected intro ends a little early (Springfloden S2E6 measured 276.8–297.1 and resumed in its tail).
const FINGERPRINT_END_LAG_SECS: f64 = 2.6;
/// Intro Skipper's 2 s, plus the chromaprint filter lag above: the last points of a window are absent, so a
/// stretch that runs to the end of the file is seen to end that much early.
const END_SNAP_SECS: f64 = 5.0;

/// The longest stretch two fingerprints share: inclusive point indices into each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shared {
    pub a: (usize, usize),
    pub b: (usize, usize),
}

impl Shared {
    pub fn secs(&self) -> f64 {
        (self.a.1 - self.a.0) as f64 * POINT_SECS
    }
}

/// Each point's last position: Intro Skipper's inverted index.
fn inverted(fp: &[u32]) -> HashMap<u32, usize> {
    let mut index = HashMap::with_capacity(fp.len());
    for (i, &p) in fp.iter().enumerate() {
        index.insert(p, i);
    }
    index
}

/// The longest run of matching points between `a` and `b` at least `min_secs` long, over every alignment an
/// (almost) identical point suggests — an inverted index on the hashes, not every offset.
pub fn longest_shared(a: &[u32], b: &[u32], min_secs: f64) -> Option<Shared> {
    let (ia, ib) = (inverted(a), inverted(b));
    let mut shifts: Vec<isize> = Vec::new();
    for (&point, &pa) in &ia {
        for d in -INDEX_SHIFT..=INDEX_SHIFT {
            if let Some(&pb) = ib.get(&point.wrapping_add_signed(d)) {
                shifts.push(pb as isize - pa as isize);
            }
        }
    }
    shifts.sort_unstable();
    shifts.dedup();
    let mut best: Option<Shared> = None;
    for shift in shifts {
        if let Some((s, e)) = contiguous(a, b, shift, min_secs) {
            if best.is_none_or(|b| e - s > b.a.1 - b.a.0) {
                let at = |i: usize| (i as isize + shift) as usize;
                best = Some(Shared { a: (s, e), b: (at(s), at(e)) });
            }
        }
    }
    best
}

/// The longest run in `a` (inclusive indices) whose points match `b`'s `shift` further on, tolerating gaps up to
/// `MAX_GAP_SECS`, if it is at least `min_secs` long. Intro Skipper's `FindContiguous`.
fn contiguous(a: &[u32], b: &[u32], shift: isize, min_secs: f64) -> Option<(usize, usize)> {
    type Run = Option<(usize, usize)>;
    let lo = (-shift).max(0) as usize;
    let hi = (a.len() as isize).min(b.len() as isize - shift).max(0) as usize;
    if lo >= hi {
        return None;
    }
    let longer = |x: Run, y: Run| match (x, y) {
        (Some(x), Some(y)) if y.1 - y.0 > x.1 - x.0 => Some(y),
        (None, y) => y,
        (x, _) => x,
    };
    let (mut best, mut run): (Run, Run) = (None, None);
    let aligned = &b[(lo as isize + shift) as usize..];
    for (k, (x, y)) in a[lo..hi].iter().zip(aligned).enumerate() {
        let i = lo + k;
        if (x ^ y).count_ones() > MAX_BIT_DIFF {
            continue;
        }
        run = match run {
            Some((s, e)) if (i - e) as f64 * POINT_SECS <= MAX_GAP_SECS => Some((s, i)),
            _ => {
                best = longer(best, run);
                Some((i, i))
            }
        };
    }
    longer(best, run).filter(|(s, e)| (e - s) as f64 * POINT_SECS >= min_secs)
}

/// Extend the end of an already established intro match along exactly the same alignment. This deliberately
/// cannot find a match of its own: it only recovers the tail where a fade, voice-over or mastering difference
/// makes corresponding Chromaprint points slightly less alike than the strict discovery threshold permits.
fn refine_intro_tail(a: &[u32], b: &[u32], mut shared: Shared) -> Shared {
    let block = (INTRO_TAIL_BLOCK_SECS / POINT_SECS).ceil() as usize;
    let max_points = (INTRO_TAIL_MAX_SECS / POINT_SECS).floor() as usize;
    let available = (a.len() - shared.a.1 - 1).min(b.len() - shared.b.1 - 1).min(max_points);
    let mut extended = 0;

    while extended + block <= available {
        let a_start = shared.a.1 + 1 + extended;
        let b_start = shared.b.1 + 1 + extended;
        let difference: u32 = a[a_start..a_start + block]
            .iter()
            .zip(&b[b_start..b_start + block])
            .map(|(x, y)| (x ^ y).count_ones())
            .sum();
        if difference > INTRO_TAIL_MAX_AVG_BIT_DIFF * block as u32 {
            break;
        }
        extended += block;
    }

    shared.a.1 += extended;
    shared.b.1 += extended;
    shared
}

/// A segment in seconds into the file.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub start: f64,
    pub end: f64,
}

/// What one episode's two windows fingerprinted to.
#[derive(Clone, Debug, Default)]
pub struct Prints {
    pub duration: f64,
    pub intro: Vec<u32>,
    pub credits: Vec<u32>,
    /// Where the credits window starts in the file.
    pub credits_start: f64,
}

/// The intro window's length: a quarter of the runtime, at most 6 min.
pub fn intro_window(duration: f64) -> f64 {
    (duration * INTRO_WINDOW_SHARE).min(INTRO_WINDOW_MAX_SECS)
}

/// Where the credits window starts: 8 min before the end, never inside the intro window (a short file).
pub fn credits_start(duration: f64) -> f64 {
    (duration - CREDITS_WINDOW_SECS).max(intro_window(duration))
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn intro_span((s, e): (usize, usize)) -> Span {
    let start = s as f64 * POINT_SECS;
    Span {
        start: if start <= START_SNAP_SECS { 0.0 } else { round3(start) },
        end: round3(e as f64 * POINT_SECS + FINGERPRINT_END_LAG_SECS),
    }
}

fn credits_span(p: &Prints, (s, e): (usize, usize)) -> Span {
    let end = p.credits_start + e as f64 * POINT_SECS + FINGERPRINT_END_LAG_SECS;
    Span {
        start: round3(p.credits_start + s as f64 * POINT_SECS),
        end: if end >= p.duration - END_SNAP_SECS { round3(p.duration) } else { round3(end) },
    }
}

/// The intro two episodes share.
fn pair_intro(a: &Prints, b: &Prints) -> Option<Shared> {
    longest_shared(&a.intro, &b.intro, INTRO_MIN_SECS)
        .map(|s| refine_intro_tail(&a.intro, &b.intro, s))
        .filter(|s| s.secs() <= INTRO_MAX_SECS)
}

/// The longest credits a window of this episode can hold: under the whole window, as Intro Skipper allows.
fn credits_max(p: &Prints) -> f64 {
    CREDITS_MAX_SECS.min(p.duration - p.credits_start - 1.0)
}

/// The credits two episodes share.
fn pair_credits(a: &Prints, b: &Prints) -> Option<Shared> {
    let max = credits_max(a).min(credits_max(b));
    longest_shared(&a.credits, &b.credits, CREDITS_MIN_SECS).filter(|s| s.secs() <= max)
}

/// The intro an episode shares with its season's kept fingerprint.
fn intro_alone(p: &Prints, kept: &[u32]) -> Option<Span> {
    longest_shared(&p.intro, kept, INTRO_MIN_SECS)
        .map(|s| refine_intro_tail(&p.intro, kept, s))
        .filter(|s| s.secs() <= INTRO_MAX_SECS)
        .map(|s| intro_span(s.a))
}

/// The credits an episode shares with its season's kept fingerprint.
fn credits_alone(p: &Prints, kept: &[u32]) -> Option<Span> {
    longest_shared(&p.credits, kept, CREDITS_MIN_SECS)
        .filter(|s| s.secs() <= credits_max(p))
        .map(|s| credits_span(p, s.a))
}

/// What a scan found for one episode.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Found {
    pub intro: Option<Span>,
    pub credits: Option<Span>,
}

/// A season's kept fingerprints: the points of the intro and the credits a pair of its episodes shared.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Season {
    pub intro: Option<Vec<u32>>,
    pub credits: Option<Vec<u32>>,
    /// Missing on caches written before tail refinement; those truncated intro fingerprints must be rebuilt.
    #[serde(default)]
    pub intro_method: String,
}

/// Match `a` (and `b`, the sibling scanned beside it, if any) against the season's fingerprints, or against each
/// other where the season has none yet; a new pair match becomes the season's. Returns what each episode got.
pub fn match_episodes(season: &mut Season, a: &Prints, b: Option<&Prints>) -> (Found, Found) {
    let (mut fa, mut fb) = (Found::default(), Found::default());
    match season.intro.as_deref() {
        Some(kept) => {
            fa.intro = intro_alone(a, kept);
            fb.intro = b.and_then(|b| intro_alone(b, kept));
        }
        None => {
            if let Some(s) = b.and_then(|b| pair_intro(a, b)) {
                fa.intro = Some(intro_span(s.a));
                fb.intro = Some(intro_span(s.b));
                season.intro = Some(a.intro[s.a.0..=s.a.1].to_vec());
                season.intro_method = FINGERPRINT_METHOD.into();
            }
        }
    }
    match season.credits.as_deref() {
        Some(kept) => {
            fa.credits = credits_alone(a, kept);
            fb.credits = b.and_then(|b| credits_alone(b, kept));
        }
        None => {
            if let Some((b, s)) = b.and_then(|b| pair_credits(a, b).map(|s| (b, s))) {
                fa.credits = Some(credits_span(a, s.a));
                fb.credits = Some(credits_span(b, s.b));
                season.credits = Some(a.credits[s.a.0..=s.a.1].to_vec());
            }
        }
    }
    (fa, fb)
}

// ---- storage --------------------------------------------------------------------------------------------------

/// One episode's stored result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stored {
    pub imdb: String,
    pub season: u32,
    pub episode: u32,
    pub duration: f64,
    pub intro: Option<Span>,
    pub credits: Option<Span>,
    pub method: String,
    pub scanned_at: u64,
}

fn upgrade_stored(stored: &mut Stored) {
    if stored.method != "fingerprint" {
        return;
    }
    let duration = stored.duration;
    let restore = |span: &mut Option<Span>| {
        if let Some(span) = span {
            span.end = round3((span.end + FINGERPRINT_END_LAG_SECS).min(duration));
        }
    };
    restore(&mut stored.intro);
    restore(&mut stored.credits);
    stored.method = "fingerprint-v2".into();
}

fn current(stored: &Stored) -> bool {
    stored.method == FINGERPRINT_METHOD
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Episode {
    pub imdb: String,
    pub season: u32,
    pub episode: u32,
}

impl Episode {
    fn id(&self) -> String {
        crate::scout::title_id(&self.imdb, Some((self.season, self.episode)))
    }

    fn next(&self, episode: u32) -> Episode {
        Episode { episode, ..self.clone() }
    }
}

/// `SCRATCH_DIR/segments`: a JSON file per episode and one per season. Not an `s-*` directory, so the start-up
/// sweep leaves it alone.
fn dir(scratch: &Path) -> PathBuf {
    scratch.join("segments")
}

fn episode_path(scratch: &Path, ep: &Episode) -> PathBuf {
    dir(scratch).join(format!("{}-{}-{}.json", ep.imdb, ep.season, ep.episode))
}

fn season_path(scratch: &Path, ep: &Episode) -> PathBuf {
    dir(scratch).join(format!("{}-{}.season.json", ep.imdb, ep.season))
}

async fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Written whole through a rename, so a reader never sees half a file.
async fn write_json<T: Serialize>(path: &Path, value: &T) {
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec(value).expect("a result serialises");
    let written = async {
        tokio::fs::create_dir_all(path.parent().expect("a file in the segments directory")).await?;
        tokio::fs::write(&tmp, json).await?;
        tokio::fs::rename(&tmp, path).await
    };
    if let Err(e) = written.await {
        crate::log_limited("segments_write", || {
            format!("segments: {} could not be written: {e}", path.display())
        });
    }
}

pub async fn stored(scratch: &Path, ep: &Episode) -> Option<Stored> {
    let mut stored: Stored = read_json(&episode_path(scratch, ep)).await?;
    // v1 wrote the timestamp of Chromaprint's final emitted point as the end. Its filters need another ~2.6 s
    // of audio to emit that point, so every boundary was correspondingly early. Upgrade in memory so existing
    // scans improve immediately after rollout; v2 results already include the restored coverage.
    upgrade_stored(&mut stored);
    Some(stored)
}

// ---- queue ----------------------------------------------------------------------------------------------------

/// Episodes waiting at most; past it a request is answered `busy`.
const QUEUE_MAX: usize = 32;
/// One episode's fingerprinting (list, resolve, probe, both windows) at most.
const SCAN_TIMEOUT: Duration = Duration::from_secs(180);
/// How often a scan waiting for a transcode to finish looks again.
const TRANSCODE_WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Inner {
    queue: VecDeque<Episode>,
    /// The scout install a queued episode was asked with, where the request named one. Memory only: an install is
    /// a credential, so it is never written to a result or a log.
    installs: HashMap<Episode, String>,
    running: HashSet<Episode>,
    /// The UTC day `used` counts, and the episodes fingerprinted on it.
    day: u64,
    used: u32,
}

/// The scan queue: one worker takes from it, requests add to it.
#[derive(Default)]
pub struct Segments {
    inner: Mutex<Inner>,
    wake: tokio::sync::Notify,
}

/// Where an episode stands in memory.
#[derive(Debug, PartialEq, Eq)]
pub enum Pending {
    Queued,
    Running,
}

impl Segments {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn pending(&self, ep: &Episode) -> Option<Pending> {
        let inner = self.lock();
        if inner.running.contains(ep) {
            Some(Pending::Running)
        } else if inner.queue.contains(ep) {
            Some(Pending::Queued)
        } else {
            None
        }
    }

    /// Queue `ep` unless it is already waiting or running; `None` when the queue is full or today's budget spent.
    /// `install` is the scout install to resolve it through; an episode already waiting keeps the one it has.
    pub fn enqueue(&self, ep: Episode, cap: u32, install: Option<String>) -> Option<Pending> {
        let mut inner = self.lock();
        if inner.running.contains(&ep) {
            return Some(Pending::Running);
        }
        if inner.queue.contains(&ep) {
            return Some(Pending::Queued);
        }
        let today = unix_now() / 86_400;
        let spent = inner.day == today && inner.used >= cap;
        if inner.queue.len() >= QUEUE_MAX || spent {
            return None;
        }
        if let Some(install) = install {
            inner.installs.insert(ep.clone(), install);
        }
        inner.queue.push_back(ep);
        drop(inner);
        self.wake.notify_one();
        Some(Pending::Queued)
    }

    /// One unit of today's budget, if any is left.
    fn spend(&self, cap: u32) -> bool {
        let mut inner = self.lock();
        let today = unix_now() / 86_400;
        if inner.day != today {
            inner.day = today;
            inner.used = 0;
        }
        if inner.used >= cap {
            // Nothing queued can run today: let those requests come again tomorrow.
            inner.queue.clear();
            inner.installs.clear();
            return false;
        }
        inner.used += 1;
        true
    }

    async fn next(&self) -> (Episode, Option<String>) {
        loop {
            {
                let mut inner = self.lock();
                if let Some(ep) = inner.queue.pop_front() {
                    inner.running.insert(ep.clone());
                    let install = inner.installs.remove(&ep);
                    return (ep, install);
                }
            }
            self.wake.notified().await;
        }
    }

    #[cfg(test)]
    pub async fn take_for_test(&self) -> (Episode, Option<String>) {
        self.next().await
    }

    fn hold(&self, ep: &Episode) {
        self.lock().running.insert(ep.clone());
    }

    fn release(&self, ep: &Episode) {
        self.lock().running.remove(ep);
    }
}

/// Runs scans one at a time, for the life of the process. Idle it waits on the queue and does nothing.
pub async fn worker(st: Arc<AppState>) {
    loop {
        let (ep, install) = st.segments.next().await;
        // A transcode is the box's heaviest work and a viewer is waiting on it; a scan can wait.
        while st.transcodes.load(Relaxed) > 0 {
            tokio::time::sleep(TRANSCODE_WAIT).await;
        }
        scan(&st, &ep, install.or_else(|| st.cfg.scout_install_url.clone())).await;
        st.segments.release(&ep);
    }
}

// ---- a scan ---------------------------------------------------------------------------------------------------

/// Why an episode was not fingerprinted.
enum Failure {
    /// Scout lists no cached release of it: maybe there is no such episode.
    NoRelease,
    /// Today's budget is spent.
    Budget,
    Failed(String),
}

/// What one episode's fingerprinting cost, for its log line.
struct Cost {
    release: String,
    bytes: u64,
    wall: Duration,
}

/// Scan `ep` and its sibling through the scout install `base`, which is never logged.
async fn scan(st: &Arc<AppState>, ep: &Episode, base: Option<String>) {
    let scratch = &st.cfg.scratch_dir;
    if stored(scratch, ep).await.as_ref().is_some_and(current) {
        return;
    }
    let Some(base) = base else {
        eprintln!("segments: {} result=failed (no scout install)", who(st, ep));
        return;
    };
    let base = base.as_str();
    let mut season: Season = read_json(&season_path(scratch, ep)).await.unwrap_or_default();
    if season.intro_method != FINGERPRINT_METHOD {
        season.intro = None;
        season.intro_method.clear();
    }
    let Some((a, cost_a)) = attempt(st, ep, base).await else { return };
    // A sibling is needed only for what the season has no fingerprint of yet.
    let mut b: Option<(Episode, Prints, Cost)> = None;
    if season.intro.is_none() || season.credits.is_none() {
        for n in [ep.episode + 1, ep.episode - 1] {
            if n == 0 {
                continue;
            }
            let sib = ep.next(n);
            st.segments.hold(&sib);
            let got = fingerprint_logged(st, &sib, base).await;
            if !matches!(got, Err(Failure::NoRelease)) || n < ep.episode {
                if let Ok((p, c)) = got {
                    b = Some((sib.clone(), p, c));
                }
                st.segments.release(&sib);
                break;
            }
            st.segments.release(&sib);
        }
    }
    let before = season.clone();
    let (fa, fb) = match_episodes(&mut season, &a, b.as_ref().map(|(_, p, _)| p));
    if season != before {
        write_json(&season_path(scratch, ep), &season).await;
    }
    save(st, ep, &a, fa, &cost_a).await;
    if let Some((sib, p, c)) = &b {
        st.segments.hold(sib);
        save(st, sib, p, fb, c).await;
        st.segments.release(sib);
    }
}

/// Fingerprint `ep` for the episode a scan was asked for; a failure is logged and ends the scan.
async fn attempt(st: &Arc<AppState>, ep: &Episode, base: &str) -> Option<(Prints, Cost)> {
    fingerprint_logged(st, ep, base).await.ok()
}

async fn fingerprint_logged(st: &Arc<AppState>, ep: &Episode, base: &str) -> Result<(Prints, Cost), Failure> {
    let started = Instant::now();
    let got = match tokio::time::timeout(SCAN_TIMEOUT, fingerprint(st, ep, base)).await {
        Ok(got) => got,
        Err(_) => Err(Failure::Failed(format!("timed out after {}s", SCAN_TIMEOUT.as_secs()))),
    };
    if let Err(f) = &got {
        let why = match f {
            Failure::NoRelease => "no cached release".to_string(),
            Failure::Budget => format!("today's budget of {} is spent", st.cfg.segments_daily_cap),
            Failure::Failed(why) => why.clone(),
        };
        eprintln!(
            "segments: {} wall={:.1}s result=failed ({why})",
            who(st, ep),
            started.elapsed().as_secs_f64()
        );
    }
    got
}

/// The episode as the log may name it: its id, unless `LOG_IDENTITY` is off.
fn who(st: &AppState, ep: &Episode) -> String {
    match st.cfg.log_identity {
        true => ep.id(),
        false => "<episode>".into(),
    }
}

/// Store what was found for `ep` — merged into an earlier result, a found segment never replaced by none — and log
/// its line.
async fn save(st: &AppState, ep: &Episode, p: &Prints, found: Found, cost: &Cost) {
    let scratch = &st.cfg.scratch_dir;
    let old = stored(scratch, ep).await;
    let old_current = old.as_ref().filter(|o| current(o));
    let result = Stored {
        imdb: ep.imdb.clone(),
        season: ep.season,
        episode: ep.episode,
        duration: round3(p.duration),
        intro: found.intro.or(old_current.and_then(|o| o.intro)),
        credits: found.credits.or(old_current.and_then(|o| o.credits)),
        method: FINGERPRINT_METHOD.into(),
        scanned_at: unix_now(),
    };
    if old
        .as_ref()
        .is_none_or(|o| !current(o) || o.intro != result.intro || o.credits != result.credits)
    {
        write_json(&episode_path(scratch, ep), &result).await;
    }
    let span = |s: Option<Span>| s.map_or("none".to_string(), |s| format!("{:.1}-{:.1}", s.start, s.end));
    eprintln!(
        "segments: {} release={} read={:.1}MB wall={:.1}s result=intro:{} credits:{}",
        who(st, ep),
        cost.release,
        cost.bytes as f64 / 1e6,
        cost.wall.as_secs_f64(),
        span(result.intro),
        span(result.credits),
    );
}

/// The releases to read, cheapest first: cached, in a container the probe opens, not 3D; anything up to 1080p before
/// a 4K copy, then by size.
fn cheapest(list: &[crate::scout::Stream]) -> Vec<&crate::scout::Stream> {
    let mut out: Vec<_> = list
        .iter()
        .filter(|s| {
            s.attributes.cached == Some(true)
                && crate::scout::left_out_by_name(s).is_none()
                && !s.attributes.three_d
        })
        .collect();
    let uhd = |s: &crate::scout::Stream| {
        s.attributes.resolution.as_deref().is_some_and(|r| {
            let r = r.to_ascii_lowercase();
            r.contains("2160") || r.contains("4k")
        })
    };
    out.sort_by_key(|s| (uhd(s), s.attributes.size_bytes.unwrap_or(u64::MAX)));
    out
}

/// How many releases one episode's fingerprinting may open before giving up.
const OPEN_MAX: usize = 3;

async fn fingerprint(st: &Arc<AppState>, ep: &Episode, base: &str) -> Result<(Prints, Cost), Failure> {
    let started = Instant::now();
    let base = base.to_string();
    let secrets = [base.clone()];
    let scrub = |text: &str| crate::redact::scrub(text, &[secrets[0].as_str()]);
    let src = crate::scout::ScoutSource { base, key: st.cfg.scout_key.clone() };
    let list = crate::scout::list(&st.scout_http, &src, &ep.id(), None)
        .await
        .map_err(|e| Failure::Failed(format!("scout: {}", scrub(&e.detail))))?;
    let picks = cheapest(&list);
    if picks.is_empty() {
        return Err(Failure::NoRelease);
    }
    if !st.segments.spend(st.cfg.segments_daily_cap) {
        return Err(Failure::Budget);
    }
    let mut why = String::new();
    for s in picks.into_iter().take(OPEN_MAX) {
        let opened = open(st, &src, s).await;
        let (resolved, duration, size) = match opened {
            Ok(o) => o,
            Err(e) => {
                why = scrub(&e);
                continue;
            }
        };
        let door = st.doors.open(st.source_http.clone(), &resolved.url, &resolved.head, size);
        let Some(input) = st.doors.url(&door) else {
            return Err(Failure::Failed("no loopback door".into()));
        };
        let start = credits_start(duration);
        let windows = async {
            let intro = decode(&st.cfg.ffmpeg, &input, 0.0, intro_window(duration)).await?;
            Ok::<_, String>((intro, decode(&st.cfg.ffmpeg, &input, start, duration - start).await?))
        };
        let (intro, credits) = match windows.await {
            Ok(w) => w,
            Err(e) => {
                why = scrub(&e);
                continue;
            }
        };
        let res = s.attributes.resolution.as_deref().unwrap_or("?");
        let cost = Cost {
            release: format!("{res}/{:.2}GB", size as f64 / 1e9),
            bytes: resolved.head.len() as u64 + door.bytes_read(),
            wall: started.elapsed(),
        };
        return Ok((Prints { duration, intro, credits, credits_start: start }, cost));
    }
    Err(Failure::Failed(why))
}

/// Follow a release's play URL and learn its duration, from the probe cache when a session already probed it.
async fn open(
    st: &Arc<AppState>,
    src: &crate::scout::ScoutSource,
    s: &crate::scout::Stream,
) -> Result<(crate::scout::Resolved, f64, u64), String> {
    let play = crate::config::local(&s.url, &st.cfg.origin_aliases);
    let r = crate::scout::resolve(&st.scout_http, &play, src).await?;
    let size = r.size.ok_or("the release has no size")?;
    let key = crate::state::probe_key(r.size, &r.head);
    let info = match key.as_deref().and_then(|k| st.probe(k)) {
        Some(info) => info,
        None => {
            let source = crate::probe::Source::Http { client: &st.source_http, url: &r.url };
            let info = crate::probe::probe(&source, &r.head).await.map_err(|e| e.to_string())?;
            if let Some(k) = key {
                st.remember_probe(k, &info);
            }
            info
        }
    };
    if info.audio.is_empty() {
        return Err("no audio track".into());
    }
    if !(info.duration.is_finite() && info.duration > 0.0) {
        return Err("no duration".into());
    }
    Ok((r, info.duration, size))
}

/// Most of ffmpeg's stderr kept, for the reason a failed run gives.
const STDERR_TAIL: usize = 2048;
/// PCM read from ffmpeg at a time: 16 k samples, about 1.5 s of audio.
const READ_CHUNK: usize = 32 * 1024;

/// The ffmpeg arguments for one window: an input seek to `start` (only the byte ranges from there are read), the
/// first audio stream alone, `len` seconds of it as 11025 Hz mono s16le on stdout. One decoder thread.
pub fn decode_args(input: &str, start: f64, len: f64) -> Vec<String> {
    let mut args: Vec<String> =
        ["-nostdin", "-hide_banner", "-loglevel", "error", "-threads", "1"].map(String::from).to_vec();
    if start > 0.0 {
        args.extend(["-ss".into(), format!("{start:.3}")]);
    }
    args.extend(["-i".into(), input.into(), "-t".into(), format!("{len:.3}")]);
    args.extend(
        [
            "-map",
            "0:a:0",
            "-vn",
            "-sn",
            "-dn",
            "-ac",
            "1",
            "-ar",
            "11025",
            "-c:a",
            "pcm_s16le",
            "-f",
            "s16le",
        ]
        .map(String::from),
    );
    args.push("pipe:1".into());
    args
}

/// Decode one window and fingerprint it as the PCM arrives. The process runs at the lowest CPU priority in its own
/// group, and is killed if this future is dropped (a timeout).
async fn decode(ffmpeg: &str, input: &str, start: f64, len: f64) -> Result<Vec<u32>, String> {
    use rusty_chromaprint::{Configuration, Fingerprinter};
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(decode_args(input, start, len))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        cmd.process_group(0);
        // SAFETY: setpriority is async-signal-safe, and nothing else runs between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::setpriority(libc::PRIO_PROCESS, 0, 19);
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg: {e}"))?;
    let pid = child.id().unwrap_or(0);
    crate::job::register_group(pid);
    struct Registered(u32);
    impl Drop for Registered {
        fn drop(&mut self) {
            crate::job::unregister_group(self.0);
        }
    }
    let _registered = Registered(pid);
    let stderr = child.stderr.take().map(|mut pipe| {
        tokio::spawn(async move {
            let mut tail = Vec::new();
            let mut buf = [0u8; 1024];
            while let Ok(n) = pipe.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > STDERR_TAIL {
                    tail.drain(..tail.len() - STDERR_TAIL);
                }
            }
            String::from_utf8_lossy(&tail).into_owned()
        })
    });
    // The fingerprinter lives on a blocking thread, off the one runtime thread, fed a few chunks at a time: a slow
    // fingerprint holds ffmpeg back through the pipe rather than PCM piling up in memory.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<i16>>(4);
    let printer = tokio::task::spawn_blocking(move || {
        let mut fp = Fingerprinter::new(&Configuration::preset_test2());
        fp.start(SAMPLE_RATE, 1).map_err(|e| format!("fingerprinter: {e}"))?;
        while let Some(samples) = rx.blocking_recv() {
            fp.consume(&samples);
        }
        fp.finish();
        Ok::<_, String>(fp.fingerprint().to_vec())
    });
    let mut out = child.stdout.take().ok_or("ffmpeg has no stdout")?;
    let mut buf = vec![0u8; READ_CHUNK];
    let mut carry: Option<u8> = None;
    loop {
        let n = out.read(&mut buf).await.map_err(|e| format!("reading ffmpeg: {e}"))?;
        if n == 0 {
            break;
        }
        let mut bytes = &buf[..n];
        let mut samples: Vec<i16> = Vec::with_capacity(n / 2 + 1);
        if let Some(lo) = carry.take() {
            samples.push(i16::from_le_bytes([lo, bytes[0]]));
            bytes = &bytes[1..];
        }
        let (pairs, rest) = bytes.as_chunks::<2>();
        carry = rest.first().copied();
        samples.extend(pairs.iter().map(|p| i16::from_le_bytes(*p)));
        if tx.send(samples).await.is_err() {
            break;
        }
    }
    drop(tx);
    let status = child.wait().await.map_err(|e| format!("ffmpeg: {e}"))?;
    if !status.success() {
        let tail = match stderr {
            Some(h) => h.await.unwrap_or_default(),
            None => String::new(),
        };
        let last = tail.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no message");
        return Err(format!("ffmpeg failed: {last}"));
    }
    printer.await.map_err(|e| format!("fingerprinter: {e}"))?
}

/// Can this ffmpeg write PCM to a pipe? The image's build has exactly what a scan uses; a local one usually has it.
pub async fn check_ffmpeg(ffmpeg: &str) -> bool {
    crate::state::reports_all(
        ffmpeg,
        &[
            (&["-hide_banner", "-h", "muxer=s16le"], "Muxer s16le"),
            (&["-hide_banner", "-h", "encoder=pcm_s16le"], "Encoder pcm_s16le"),
            (&["-hide_banner", "-protocols"], "pipe"),
        ],
    )
    .await
}

// ---- routes ---------------------------------------------------------------------------------------------------

fn status(code: StatusCode, status: &str) -> Response<Body> {
    httputil::json(code, &serde_json::json!({ "status": status }), &[])
}

/// `POST /segments/scan`: queue an episode (and so its sibling) for a scan. Off (404) while ffmpeg can't do PCM.
/// The caller is either keyed (`X-Den-Segments-Key` = `SEGMENTS_KEY`) or brings a scout install of its own in the
/// body, validated as a session's is; the scan resolves its releases through that install.
pub async fn scan_route(
    st: &Arc<AppState>,
    headers: &hyper::HeaderMap,
    body: Option<Bytes>,
) -> Response<Body> {
    use subtle::ConstantTimeEq;
    if !st.segments_ok() {
        return httputil::not_found();
    }
    #[derive(Deserialize)]
    struct Scan {
        imdb: String,
        season: u32,
        episode: u32,
        scout: Option<String>,
    }
    let req = body.and_then(|b| serde_json::from_slice::<Scan>(&b).ok());
    let presented = headers.get("x-den-segments-key").map(|v| v.as_bytes()).unwrap_or_default();
    let keyed = st.cfg.segments_key.as_deref().is_some_and(|k| bool::from(presented.ct_eq(k.as_bytes())));
    let scout = req.as_ref().and_then(|r| r.scout.as_deref());
    if !keyed && scout.is_none() {
        return httputil::error(
            StatusCode::UNAUTHORIZED,
            "bad_key",
            "Send X-Den-Segments-Key, or a scout install URL as \"scout\".",
        );
    }
    let install = match scout {
        Some(url) => match crate::scout::validate_scoped(url, &st.cfg.scout_origins) {
            Ok(valid) => Some(crate::config::local(&valid, &st.cfg.origin_aliases)),
            Err(why) => {
                return httputil::error(
                    StatusCode::BAD_REQUEST,
                    "bad_scout",
                    &format!("The scout URL was refused: {why}."),
                )
            }
        },
        None => None,
    };
    // A keyed caller with no install of its own scans through this service's, if it has one.
    if install.is_none() && st.cfg.scout_install_url.is_none() {
        return status(StatusCode::UNPROCESSABLE_ENTITY, "no_install");
    }
    let Some(req) = req.filter(|r| crate::scout::is_imdb(&r.imdb) && r.episode >= 1) else {
        return httputil::error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "Expected {\"imdb\": \"tt…\", \"season\": n, \"episode\": n (1 or more)}.",
        );
    };
    let ep = Episode { imdb: req.imdb, season: req.season, episode: req.episode };
    let answer = match st.segments.pending(&ep) {
        Some(Pending::Running) => "running",
        Some(Pending::Queued) => "queued",
        None => match stored(&st.cfg.scratch_dir, &ep).await {
            Some(s) if current(&s) && (s.intro.is_some() || s.credits.is_some()) => "done",
            Some(s) if current(&s) => "none",
            _ => match st.segments.enqueue(ep, st.cfg.segments_daily_cap, install) {
                Some(Pending::Running) => "running",
                Some(Pending::Queued) => "queued",
                None => {
                    return httputil::json(
                        StatusCode::SERVICE_UNAVAILABLE,
                        &serde_json::json!({"status": "busy"}),
                        &[("retry-after", "60")],
                    )
                }
            },
        },
    };
    status(StatusCode::ACCEPTED, answer)
}

/// `GET /segments/<tt>/<season>/<episode>`: what a scan found. Public: times are no secret, and no install is needed.
pub async fn get_route(st: &AppState, rest: &str) -> Response<Body> {
    let mut it = rest.split('/');
    let parsed =
        match (it.next(), it.next().map(str::parse::<u32>), it.next().map(str::parse::<u32>), it.next()) {
            (Some(imdb), Some(Ok(season)), Some(Ok(episode)), None) if crate::scout::is_imdb(imdb) => {
                Some(Episode { imdb: imdb.to_string(), season, episode })
            }
            _ => None,
        };
    let Some(ep) = parsed else { return status(StatusCode::NOT_FOUND, "unknown") };
    if st.segments.pending(&ep).is_some() {
        return status(StatusCode::ACCEPTED, "pending");
    }
    match stored(&st.cfg.scratch_dir, &ep).await {
        Some(s) if current(&s) => httputil::json(
            StatusCode::OK,
            &serde_json::json!({
                "duration": s.duration,
                "intro": s.intro,
                "credits": s.credits,
                "method": s.method,
                "scanned_at": s.scanned_at,
            }),
            &[],
        ),
        _ => status(StatusCode::NOT_FOUND, "unknown"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic stream of fingerprint points: xorshift.
    fn noise(seed: u32, n: usize) -> Vec<u32> {
        let mut x = seed.max(1);
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x
            })
            .collect()
    }

    fn secs(n: f64) -> usize {
        (n / POINT_SECS).round() as usize
    }

    /// `pre` points of noise, `shared`, then `post` points of noise.
    fn episode(seed: u32, pre: usize, shared: &[u32], post: usize) -> Vec<u32> {
        let mut v = noise(seed, pre);
        v.extend_from_slice(shared);
        v.extend(noise(seed.wrapping_mul(7919), post));
        v
    }

    #[test]
    fn a_shared_run_is_found_at_different_offsets() {
        let intro = noise(42, secs(50.0));
        let a = episode(1, secs(10.0), &intro, secs(100.0));
        let b = episode(2, secs(73.0), &intro, secs(30.0));
        let s = longest_shared(&a, &b, INTRO_MIN_SECS).expect("the shared stretch");
        assert_eq!(s.a, (secs(10.0), secs(10.0) + intro.len() - 1));
        assert_eq!(s.b, (secs(73.0), secs(73.0) + intro.len() - 1));
    }

    #[test]
    fn a_few_flipped_bits_and_short_gaps_still_match() {
        let intro = noise(7, secs(40.0));
        let mut noisy = intro.clone();
        for (i, p) in noisy.iter_mut().enumerate() {
            // Up to 6 bits off on every point, as two encodes of one song differ.
            *p ^= [0b1, 0b11_0000, 0b111111 << 20, 0][i % 4];
        }
        // A 2 s dropout (a different sound mixed over it) in the middle.
        let gap = noise(99, secs(2.0));
        noisy.splice(secs(20.0)..secs(20.0) + gap.len(), gap);
        let a = episode(3, secs(5.0), &intro, secs(60.0));
        let b = episode(4, secs(31.0), &noisy, secs(60.0));
        let s = longest_shared(&a, &b, INTRO_MIN_SECS).expect("one run across the gap");
        assert_eq!(s.a.0, secs(5.0));
        assert_eq!(s.b.0, secs(31.0));
        assert!(s.secs() > 39.0, "{}", s.secs());
    }

    #[test]
    fn seven_differing_bits_are_no_match() {
        let intro = noise(8, secs(40.0));
        let far: Vec<u32> = intro.iter().map(|p| p ^ 0b111_1111).collect();
        let a = episode(5, secs(5.0), &intro, secs(60.0));
        let b = episode(6, secs(5.0), &far, secs(60.0));
        assert_eq!(longest_shared(&a, &b, INTRO_MIN_SECS), None);
    }

    #[test]
    fn a_softer_match_refines_an_established_intro_tail_only() {
        let core = noise(80, secs(35.0));
        let tail = noise(81, secs(8.0));
        // Model the same music under a fade or episode-specific mix: each corresponding point is farther apart
        // than the strict six-bit discovery threshold, but the one-second blocks remain strongly correlated.
        let mixed_tail: Vec<u32> = tail.iter().map(|p| p ^ 0b1_1111_1111).collect();
        let mut shared_a = core.clone();
        shared_a.extend_from_slice(&tail);
        let mut shared_b = core.clone();
        shared_b.extend_from_slice(&mixed_tail);
        let a = episode(82, secs(10.0), &shared_a, secs(40.0));
        let b = episode(83, secs(30.0), &shared_b, secs(40.0));

        let strict = longest_shared(&a, &b, INTRO_MIN_SECS).expect("the strict core");
        assert_eq!(strict.a.1, secs(10.0) + core.len() - 1);
        let refined = refine_intro_tail(&a, &b, strict);

        let recovered = (refined.a.1 - strict.a.1) as f64 * POINT_SECS;
        assert!(recovered >= 7.0, "recovered only {recovered:.3} s");
        assert!(recovered <= 8.0, "extended into unrelated audio by {recovered:.3} s");
    }

    #[test]
    fn unrelated_audio_and_a_short_shared_bit_are_no_match() {
        assert_eq!(longest_shared(&noise(10, 3000), &noise(11, 3000), INTRO_MIN_SECS), None);
        let sting = noise(12, secs(10.0));
        let a = episode(13, secs(30.0), &sting, secs(100.0));
        let b = episode(14, secs(60.0), &sting, secs(100.0));
        assert_eq!(longest_shared(&a, &b, INTRO_MIN_SECS), None, "10 s is under the 15 s minimum");
    }

    fn prints(intro: Vec<u32>, credits: Vec<u32>, duration: f64) -> Prints {
        Prints { duration, intro, credits, credits_start: credits_start(duration) }
    }

    #[test]
    fn a_pair_gives_both_episodes_their_times_and_the_season_its_fingerprint() {
        let duration = 2700.0;
        let theme = noise(20, secs(45.0));
        let end = noise(21, secs(60.0));
        let window = secs(CREDITS_WINDOW_SECS);
        // Episode A's intro starts 3 s in (snapped to 0), B's after a 70 s cold open; both credits run to the end.
        let a = prints(
            episode(22, secs(3.0), &theme, secs(200.0)),
            episode(23, window - end.len(), &end, 0),
            duration,
        );
        let b = prints(
            episode(24, secs(70.0), &theme, secs(200.0)),
            episode(25, window - end.len(), &end, 0),
            duration,
        );
        let mut season = Season::default();
        let (fa, fb) = match_episodes(&mut season, &a, Some(&b));
        let intro_a = fa.intro.unwrap();
        assert_eq!(intro_a.start, 0.0, "a start within 5 s snaps to 0");
        assert!((intro_a.end - 50.6).abs() < 0.3, "{intro_a:?}");
        let intro_b = fb.intro.unwrap();
        assert!((intro_b.start - 70.0).abs() < 0.2 && (intro_b.end - 117.6).abs() < 0.3, "{intro_b:?}");
        let credits_a = fa.credits.unwrap();
        assert!((credits_a.start - 2640.0).abs() < 0.5, "{credits_a:?}");
        assert_eq!(credits_a.end, duration, "an end at the file's end snaps to it");
        assert_eq!(season.intro.as_ref().unwrap().len(), theme.len());
        assert_eq!(season.credits.as_ref().unwrap().len(), end.len());

        // A later episode needs no sibling: the season's fingerprints find it alone.
        let c = prints(
            episode(26, secs(20.0), &theme, secs(200.0)),
            episode(27, window - end.len() - secs(30.0), &end, secs(30.0)),
            duration,
        );
        let kept = season.clone();
        let (fc, none) = match_episodes(&mut season, &c, None);
        assert_eq!(none, Found::default());
        assert_eq!(season, kept, "the season's fingerprints are kept as they were");
        assert!((fc.intro.unwrap().start - 20.0).abs() < 0.2);
        let credits_c = fc.credits.unwrap();
        assert!((credits_c.end - (duration - 30.0 + FINGERPRINT_END_LAG_SECS)).abs() < 0.5, "{credits_c:?}");
    }

    #[test]
    fn an_intro_longer_than_two_minutes_is_not_an_intro() {
        let long = noise(30, secs(150.0));
        let a = prints(episode(31, 0, &long, secs(100.0)), noise(32, 100), 2700.0);
        let b = prints(episode(33, secs(10.0), &long, secs(100.0)), noise(34, 100), 2700.0);
        let mut season = Season::default();
        assert_eq!(match_episodes(&mut season, &a, Some(&b)), (Found::default(), Found::default()));
        assert_eq!(season, Season::default());
    }

    #[test]
    fn the_windows_follow_the_runtime() {
        assert_eq!(intro_window(2700.0), 360.0, "6 min at most");
        assert_eq!(intro_window(1200.0), 300.0, "a quarter of a short episode");
        assert_eq!(credits_start(2700.0), 2220.0, "the last 8 min");
        assert_eq!(credits_start(600.0), 150.0, "never inside the intro window");
    }

    #[test]
    fn a_stored_v1_boundary_gains_the_fingerprint_tail_once() {
        let mut old = Stored {
            imdb: "tt5194410".into(),
            season: 2,
            episode: 6,
            duration: 2700.0,
            intro: Some(Span { start: 276.8, end: 297.1 }),
            credits: Some(Span { start: 2690.0, end: 2699.0 }),
            method: "fingerprint".into(),
            scanned_at: 0,
        };
        upgrade_stored(&mut old);
        assert_eq!(old.intro.unwrap().end, 299.7);
        assert_eq!(old.credits.unwrap().end, 2700.0, "the restored tail is clamped to the file");
        assert_eq!(old.method, "fingerprint-v2");
        upgrade_stored(&mut old);
        assert_eq!(old.intro.unwrap().end, 299.7, "v2 must not be extended again on every read");
    }

    #[test]
    fn the_decode_reads_only_its_window_of_the_first_audio_track() {
        let args = decode_args("http://127.0.0.1:1/door", 2220.0, 480.0).join(" ");
        assert_eq!(
            args,
            "-nostdin -hide_banner -loglevel error -threads 1 -ss 2220.000 -i http://127.0.0.1:1/door -t 480.000 \
             -map 0:a:0 -vn -sn -dn -ac 1 -ar 11025 -c:a pcm_s16le -f s16le pipe:1"
        );
        assert!(!decode_args("x", 0.0, 360.0).contains(&"-ss".to_string()), "the intro window starts at 0");
    }

    /// The cost of one window, for `/usr/bin/time -l`: `SEGMENTS_BENCH_FILE` (a local release) from 0 for
    /// `SEGMENTS_BENCH_SECS` (600).
    #[tokio::test]
    #[ignore = "needs ffmpeg and a SEGMENTS_BENCH_FILE"]
    async fn bench_one_window() {
        let Ok(file) = std::env::var("SEGMENTS_BENCH_FILE") else { return };
        let secs: f64 =
            std::env::var("SEGMENTS_BENCH_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(600.0);
        let ffmpeg = std::env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".into());
        let began = Instant::now();
        let fp = decode(&ffmpeg, &file, 0.0, secs).await.unwrap();
        eprintln!("{} points for {secs} s in {:?}", fp.len(), began.elapsed());
    }

    #[test]
    fn the_cheapest_cached_release_comes_first_and_4k_last() {
        let stream = |name: &str, res: &str, size: u64, cached: bool| crate::scout::Stream {
            title: name.into(),
            url: String::new(),
            attributes: crate::scout::Attributes {
                cached: Some(cached),
                size_bytes: Some(size),
                resolution: Some(res.into()),
                ..Default::default()
            },
            hints: Default::default(),
        };
        let list = [
            stream("a.mkv", "2160p", 1_000_000_000, true),
            stream("b.mkv", "1080p", 3_000_000_000, true),
            stream("c.mkv", "720p", 900_000_000, false),
            stream("d.mkv", "720p", 1_200_000_000, true),
            stream("e.avi", "480p", 100_000_000, true),
        ];
        let order: Vec<&str> = cheapest(&list).iter().map(|s| s.filename()).collect();
        assert_eq!(order, ["d.mkv", "b.mkv", "a.mkv"]);
    }
}
