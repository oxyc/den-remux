# Architecture

den-remux turns one cached remote movie file into signed fMP4 HLS. The Rust process authenticates and selects a
release, probes it in process, coordinates bounded producers, and serves completed media. FFmpeg does the media work.

## Request and byte path

1. `POST /remux/session` validates the browser, scout install, or den-edge grant; asks scout for cached releases; and
   resolves and probes candidates. It creates a lightweight logical session and starts no process for hls.js.
2. A native-player prepare or a request for `init.mp4`/`seg<N>.m4s` starts a producer when the requested bytes are not
   already complete. Producers acquire FIFO `MAX_ACTIVE_REMUXES`; transcodes also acquire `MAX_TRANSCODES`.
3. FFmpeg reads a random per-session URL on `127.0.0.1`. The door serves the probed head from memory and upstream data
   over the Rust client's pooled TLS connections. One sequential read requests 1, 16, 64, then 128 MiB ranges; a seek
   begins again at 1 MiB. Only eight received body frames may queue, so slow readers apply upstream backpressure.
4. FFmpeg writes one temporary fMP4 file per source GOP and renames it only when complete. A segment response opens all
   required GOPs before pruning and streams their file handles with Range and strong ETag semantics.

The first segment at a new or resumed playback point is split at nearby source keyframes for quick startup. Following
segments retain the ordinary target duration.

## Lifetimes and bounds

`MAX_SESSIONS` limits logical sessions. A producer pauses when its bounded ahead window is full; it is killed and
reaped after `PRODUCER_IDLE_SECS`, or promptly when another session queues for its slot. The session, playlists,
completed GOPs, and signed URL remain; a later segment request resumes at the nearest safe keyframe. The default
logical cap stays at two for rollout compatibility and can be raised independently after this scheduler is verified.
A session ends after `SESSION_IDLE_SECS`, expiry, replacement, deletion, or shutdown.

Each producer has one process, one bounded upstream queue, and a bounded stderr tail. Scratch is pruned around demand
and capped globally. Cancellation drops queued semaphore acquisitions and process permits; process groups are killed
and reaped on replacement, parking, session end, and shutdown.

## Durable and ephemeral state

Live links and tickets stay in memory and expire quickly. Full probe metadata is credential-free and persists in
`SCRATCH_DIR/probe-metadata.json`, atomically replaced and bounded by entry count, file bytes, entry bytes, track and
index counts, and string lengths. Restore validates numeric ranges and ordering before accepting the file. Its
identity is the exact file size plus SHA-256 of the probed head, and the file has an explicit parser schema. Session
scratch is disposable and swept at startup.

## Operations

The Tokio runtime remains single-threaded: routing is asynchronous, file opens and persistence use blocking workers,
and FFmpeg owns media CPU/GPU work. `/metrics` exposes logical sessions, active/waiting/max producers, active/max GPU
transcodes, source bytes requested/consumed/abandoned, scratch, and run totals.
