# den-remux

Den's playback for browsers — and, through the same URLs, for AirPlay and Cast receivers. A phone or a
laptop cannot play what den-scout returns: Matroska, and Dolby/DTS/TrueHD audio. den-remux copies the
video, re-encodes the audio to AAC stereo, and serves HLS (fMP4) cut on the file's own keyframes.

```
browser ──POST /remux/login {key}─────────────►  cookie (HttpOnly, SameSite=Strict, Path=/remux)
browser ──POST /remux/session {imdb, scout}───►  scout (the library's install URL) → cached H.264/HEVC release
        ◄──{ playlist: /remux/s/<sid>/<sig>/master.m3u8 }       probe: duration, tracks, keyframe index
<video> ──GET /remux/s/<sid>/<sig>/seg<N>.m4s─►  ffmpeg: -c:v copy -c:a aac, one fMP4 file per GOP
```

This is the MVP ("phase 2") of oxyc/den#11: movies and episodes, cached releases only, AAC stereo, two sessions.

## Routes

```
POST   /remux/login                     {key} → 204 + Set-Cookie; 401 bad_key
POST   /remux/session                   {imdb, season?, episode?, filename?, scout?, audio?, audioTrack?,
                                         subtitles?, subtitleLanguages?, videoCodecs?} (cookie) → 201
                                        {sid, playlist, release:{label,filename,size}, duration, expiresAt,
                                         video:{codec,transcoded}, audioTrack, audioTracks, subtitles}
                                        401 not_logged_in · 400 bad_request/bad_scout/bad_subtitles/bad_audio_track
                                        404 no_release/no_playable_release · 429 too_many_sessions
                                        502 scout_unavailable · 503 scout_unconfigured/transcode_unavailable
GET    /remux/s/<sid>/<sig>/master.m3u8 one variant: CODECS "<avc1…|hvc1…>,mp4a.40.2", BANDWIDTH from size/duration
GET    /remux/s/<sid>/<sig>/media.m3u8  VOD, #EXT-X-MAP init.mp4, segments on real keyframes, #EXT-X-ENDLIST
GET    /remux/s/<sid>/<sig>/init.mp4
GET    /remux/s/<sid>/<sig>/seg<N>.m4s  200 when made (waits up to 20 s), else 503 + Retry-After: 2
GET    /remux/s/<sid>/<sig>/sub<N>.m3u8 a subtitle rendition: one WebVTT segment spanning the film
GET    /remux/s/<sid>/<sig>/sub<N>.vtt  text/vtt; an empty document when nothing in that language was found
DELETE /remux/s/<sid>/<sig>             204; the session's URLs answer 410 from then on
GET    /health                          200 {status} — ok, or degraded with a reason (Maintenance)
GET    /metrics                         Prometheus text (bearer METRICS_TOKEN; 404 without it)
anything else                           404 {"error":"not_found"}
```

`scout` is the scout install URL the web app reads from the library's `set:plugins` group
(`http://<scout>:8080/<config>`); without it the server's `SCOUT_INSTALL_URL` is used. `filename` prefers that release when it is playable here.

- **Audio.** `audio` is the player's languages, most wanted first, in any spelling a release or a browser
  uses (`en-US`, `eng`, `fin`). The first language a track is in wins — the default-flagged track among
  several — and a commentary never does (Matroska's FlagCommentary, or "commentary" in the track title).
  No match: the first default track that is not a commentary. `audioTrack` picks one by index from an
  earlier session's `audioTracks` (send that session's `filename` with it). The track is re-encoded to AAC
  stereo as before; one per session, so switching language is a new session.
- **Subtitles.** `subtitles` is den-subtitles' install URL from the library, on `SUBTITLE_ORIGINS`;
  `subtitleLanguages` (up to 4) become WebVTT renditions in the master playlist — what AirPlay and Cast
  receivers show, which a page's own `<track>` never reaches. Nothing is fetched until a player opens one:
  then den-remux asks den-subtitles for the title with the release's OpenSubtitles hash, size and
  filename (the Apple TV's hints, so an exact-encode match ranks first) and serves the first subtitle in
  that language. A subtitle URL off `SUBTITLE_ORIGINS` is skipped.
- **Video codecs.** `videoCodecs` is what the player takes (`["h264"]` for Firefox or an old
  Chromecast); empty is H.264 and HEVC. Without HEVC, H.264 releases are tried first, and an HEVC-only
  title is transcoded to H.264 on the GPU (Hardware transcode, below) — or refused with
  `transcode_unavailable` while transcoding is off or in use.

`/remux/s/…` responses carry `Access-Control-Allow-Origin: *`, allow `Range` and expose
`Content-Range`/`Content-Length` — a Cast receiver's page is on Google's origin. Playlists are
`application/vnd.apple.mpegurl` and `no-store`; `init.mp4` and segments are `video/mp4`. Login and session
creation get no CORS at all: they are for the Den web app on the same origin.

A browser that creates a session while it already has one ends the old one (it has moved on to another
title), so `MAX_SESSIONS` counts browsers watching, not titles clicked.

## Security model

- **The browser names its own scout install.** Every device in a Den library holds the library's plugin
  list, scout's full install URL included (one trust level, oxyc/den#12), and the web app sends that URL
  with each session request. A scope=availability URL works too: den-scout lets it list and play only
  when the caller presents `X-Den-Remux-Key` (`REMUX_SCOUT_KEY`), so that URL on its own plays nothing.
  den-remux accepts either only on an origin in `SCOUT_ORIGINS` with exactly one base64url config segment
  and no credentials, query or fragment — the SSRF guard, without which a logged-in browser could make
  this service fetch anything on the LAN.
- **The key reaches scout and nothing else.** An HTTP client forwards custom headers across a
  cross-origin redirect, which would hand the key to the debrid's CDN on scout's 302. den-remux talks to
  scout with a client that follows no redirect and follows play URLs by hand, sending the key only to
  scout's origin; the integration tests put the file host on another origin and fail if it ever sees
  the key.
- **Tickets and debrid links stay here.** The browser names a title and gets back one release's session.
  Scout's play URL and the debrid link it leads to are used only by this process and its ffmpeg, so every
  byte leaves from the homelab's IP. `SCOUT_INSTALL_URL`, this service's own install, is a fallback for
  driving it by hand.
- **The cookie only creates sessions.** A browser posts its key once; the server holds only SHA-256 hashes
  (`BROWSER_KEY_HASHES`). The cookie is `HttpOnly; Secure; SameSite=Strict; Path=/remux`, HMAC-signed with
  an expiry (30 days), and stops working when its key's hash is removed or `REMUX_URL_KEY` rotates. It is a
  cookie, not a header, because native HLS in Safari cannot add headers. Script on the page cannot read it;
  at worst an XSS starts sessions, and there are two of those.
- **Session URLs are signed bearer URLs.** `/remux/s/<sid>/<sig>/…`: `sid` is 128 random bits, `sig` is
  `HMAC-SHA256(REMUX_URL_KEY, sid‖exp)` truncated to 128 bits, compared in constant time. No cookie is
  asked for there, which is what will let a Cast or AirPlay receiver play. A session lives for the film's
  runtime plus an hour (six hours at most), ends after `SESSION_IDLE_SECS` without a request, and
  `DELETE` ends it at once. Whoever holds the URL can watch that one title through the homelab until then —
  and nothing else: no scout, no ticket, no debrid link. A signature that does not match is a 404, so a
  guess learns nothing about which sessions exist.
- **The logs never carry a secret.** The request log shortens the session id and drops the signature;
  anything unrouted is written as `/<unrouted>`; request bodies (where the scout URL arrives) are never
  logged. Every line that quotes an upstream or ffmpeg error goes through a scrubber that removes the
  scout URL and every URL.

## Segment alignment

The playlist is fixed before any segment exists, so every segment ffmpeg makes has to start exactly where
the playlist says — including after a seek restarts ffmpeg mid-file. With the video copied, a segment can
only start on a keyframe, so the playlist is cut on "the first keyframe at or after n × 6 s" (Jellyfin's
approach), from the Matroska Cues (one ranged read via the SeekHead, as Jellyfin's
`MatroskaKeyframeExtractor` does) or the MP4 `stss`/`stts`/`ctts`.

The hard part is making ffmpeg cut there. What was tried, on ffmpeg 9.0.1 against the fixtures:

1. **`-hls_time 6` cuts relative to where the run started.** A run from 0 cuts at 8, 13, 19.5 …; a run
   restarted at 13 cuts at the first keyframe ≥ 19, ≥ 25 … — a different set of boundaries. Fixed-duration
   cutting cannot match a fixed playlist across restarts.
2. **`-hls_time 0` cuts at every keyframe**, whatever the start: one fMP4 file per GOP, and no restart can
   shift that. den-remux joins consecutive GOP files into the playlist's segments (a segment may hold
   several `moof`/`mdat` pairs; the extra `styp` boxes are skipped when joining).
3. **`-ss 13` does not start at 13.** ffmpeg backs an input seek off by 3/23 s for formats that seek by
   decode time when a stream has B-frames (Matroska, not MP4), and landed on the keyframe at 10.5. Worse,
   that first GOP held no audio, so the mp4 muxer rebased the audio to 0 — a 13-second desync. Aiming
   `-ss K+0.135` lands on K whether or not the back-off applies (`job.rs`, `SEEK_PAST`); a keyframe whose
   successor is closer than 0.15 s restarts from an earlier segment instead. `-noaccurate_seek` keeps the
   audio from the demuxer's position (12.907) instead of trimming it to the seek point, which left a
   120 ms hole.
4. **Restarted runs wrote different inits.** By default the mp4 muxer rebases every run to zero (`tfdt=0`
   for the GOP at 13 s) and records the offset in that run's `init.mp4` edit list (1353 vs 1305 bytes), so
   GOPs from two runs cannot share one init. `-copyts -start_at_zero -avoid_negative_ts disabled` and
   `-hls_segment_options movflags=+frag_discont+skip_sidx` (Jellyfin passes the same) make `tfdt` absolute
   (`208000/16000 = 13.0`) and the inits **byte-identical** across runs; one run's init played another's GOP
   with its keyframe at pts 13.000.
5. **A copy still needs the video decoders in the build.** Without them stream probing cannot learn the
   B-frame reorder delay, and the minimal ffmpeg guessed every copied packet's DTS (`Invalid DTS …
   replacing by guess`, dts past pts). The image enables the h264/hevc decoders for probing only.

The integration tests hold all of it: they fetch segments out of order (3, then 1 — behind that run — then
0) so the job restarts twice, and check with ffprobe that every segment starts with a keyframe at its
playlist time (±1 frame), ends before the next, carries audio, and that the segments joined back-to-back
hold every source frame exactly once over the full duration — for H.264 and HEVC Matroska and for MP4.

## Resource use

Low resource use comes first, and the design follows from it:

- **Image** — a static musl `den-remux` and an ffmpeg configured with `--disable-everything` plus only what
  a session uses, on Alpine. Until the GPU transcode it was 13.4 MB on `distroless/static`; libva has to
  load Intel's driver at run time, which a fully static binary cannot, and that driver is most of the
  image now. Disk only: nothing loads it until a transcode starts. den-reel's image on the box is 693 MB
  (Debian ffmpeg, yt-dlp, deno, MP4Box).
- **Idle is zero work.** No timers, sweeps or polling without sessions: scratch is swept at start and a
  session's directory is removed when it ends. Each session has one task, which ticks every 500 ms only
  while its ffmpeg is actually running and otherwise sleeps until a request or its idle deadline.
- **One ffmpeg per session**, `-threads 1` (the video is copied; stereo AAC needs one thread), paused with
  `SIGSTOP` once it is four segments ahead of the newest request and resumed when the player catches up.
  Killed (process group) and reaped on end, idle, seek-restart and shutdown.
- **Scratch is a window**: GOPs behind the previous segment are deleted when a segment is served, and a
  job pauses when `SCRATCH_MAX_BYTES` is reached. Segments stream from disk; nothing is buffered whole.

Measured with the image (arm64 build under OrbStack on Apple Silicon — indicative for the box's i5-8500T),
a 300 s 720p H.264 file with 5.1 AC-3 served from a local origin:

| | |
|---|---|
| den-remux RSS | 1.8 MB idle, 2.3 MB peak during a session |
| ffmpeg per session | 10 MB peak RSS; 16.9 CPU-s (11.6 user + 5.3 sys) for the whole 300 s — about **5.6 % of one core** at real-time playback |
| Latency (local origin) | session created in 0.10–0.13 s; first segment 0.01–0.16 s; a seek (restart at a keyframe) 0.26–1.7 s |

Over a real debrid add its first-byte time to session creation and to a seek.

## Configuration

Every variable is unprefixed; `.env.example` lists them with their defaults.

| Variable | Default | Purpose |
|---|---|---|
| `SCOUT_ORIGINS` | — | Origins (`scheme://host[:port]`, comma-separated) a request's `scout` URL may point at. The SSRF guard; empty refuses every `scout` URL. |
| `REMUX_SCOUT_KEY` | — | **Secret.** Sent to scout (and only to scout's origin) as `X-Den-Remux-Key`; a scope=availability config lists and plays only with it (a full install URL doesn't need it). `/health` says `scout_key_missing` when origins are set without it. |
| `SCOUT_INSTALL_URL` | — | Fallback when a request names no scout: an install of this service's own, sealed config included (`http://<scout>:8080/<config>`). **Secret**; never logged. |
| `SUBTITLE_ORIGINS` | — | Origins a request's den-subtitles install, and the subtitle URLs it answers with, may be on. Empty turns subtitles off. |
| `BROWSER_KEY_HASHES` | — | Comma-separated hex SHA-256 of each browser's key. Removing one logs that browser out. |
| `REMUX_URL_KEY` | random | **Secret.** Signs cookies and session URLs. **Set it**: unset, every restart logs the browsers out (`/health` says `url_key_ephemeral`). Rotating it kills every cookie and session URL. |
| `MAX_SESSIONS` | `2` | Sessions at once; the next gets 429 `too_many_sessions`. |
| `SESSION_IDLE_SECS` | `600` | A session with no request for this long is ended (min 30). |
| `SCRATCH_DIR` | `/cache` | Where GOP files go. den-remux's alone: every `s-*` directory in it is deleted at start. |
| `SCRATCH_MAX_BYTES` | `1073741824` | Cap across sessions; past it a job pauses once the requested segment is done (min 64 MiB). |
| `FFMPEG_PATH` | `ffmpeg` | The image sets `/usr/local/bin/ffmpeg`. (There is no `FFPROBE_PATH`: probing is done in-process.) |
| `MAX_TRANSCODES` | `1` | Sessions transcoding on the GPU at once; `0` turns transcoding off. Copies do not count. |
| `VAAPI_DEVICE` | `/dev/dri/renderD128` | The GPU's render node. Transcoding is on only when it exists and ffmpeg has the VAAPI encoder and filters (the startup line says `transcode=vaapi(max N)` or `off`). |
| `METRICS_TOKEN` | — | Turns on `/metrics` behind `Authorization: Bearer <token>`; otherwise it is a 404. |
| `LOG_REQUESTS` | off | `1` writes `<METHOD> <path> <status> <ms>ms[ rid=<X-Request-Id>]` per response. |
| `PORT` | `8095` | |

A browser key and its hash: `key=$(head -c 24 /dev/urandom | base64 | tr '+/' '-_'); printf %s "$key" | sha256sum`.

## Maintenance

`/health` is 200 with `status: ok`, or `degraded` with the first reason sessions cannot work:
`ffmpeg_unavailable` (checked at start: matroska and mov demuxers, hls muxer, aac encoder, https),
`scratch_unwritable`, `scout_unconfigured` (neither `SCOUT_ORIGINS` nor `SCOUT_INSTALL_URL`),
`scout_key_missing`, `no_browser_keys`, `url_key_ephemeral`.

`/metrics` (gauges and counters prefixed `remux_`): sessions and the cap, ffmpeg processes alive, scratch
bytes and the cap, transcodes and their cap (0 when off), sessions and ffmpeg runs started. The log is state changes: the startup line, one line
per session start and end (with the reason: `idle`, `expired`, `deleted`, `replaced`, `shutdown`), a
failed ffmpeg run's last stderr line (scrubbed), and rate-limited upstream failures.

ffmpeg is built from a checksummed source tarball (`FFMPEG_VERSION`/`FFMPEG_SHA256` in the Dockerfile);
dependabot cannot bump it, so bump both lines by hand.

## Limits (MVP)

- **Cached releases only** (an uncached one would start a debrid download).
- **H.264 and HEVC sources only.** AV1, VP9, MPEG-4 Part 2/XviD and VC-1 are skipped. Files without a
  keyframe index (Matroska with no Cues) are skipped.
- **Audio is AAC stereo**, one track per session.
- **Text subtitles only**, from den-subtitles; the release's own tracks (PGS, ASS) are not carried.
- **Dolby Vision profile 5** has no HDR10/SDR base layer: Safari shows it, Chrome cannot, and a transcode
  gets its colours wrong. It is not excluded yet.
- **Bandwidth**: at home this is fine. Away from home every byte crosses the home **upload** link, so a
  remote session is bounded by it — a 4K remux will not fit. A transcode is 8 Mbit/s at most 1080p, but
  nothing asks for one on bandwidth grounds yet.
- **One listener.** The public `/remux/s/` listener for Cast/AirPlay (#11 §C) is phase 3.

### Hardware transcode

For a player without HEVC (`videoCodecs` without it), an HEVC release is decoded, scaled and — HDR10 or
HLG — tone-mapped on the box's UHD 630, and encoded to H.264 High 4.1 there (VAAPI, the `h264_vaapi`
encoder), fitted inside 1920 × 1080 at 8 Mbit/s (12 max). `-force_key_frames source` puts an output
keyframe on every source keyframe, so the GOPs, the playlist and the joining are exactly a copy's; the
audio and subtitles are unchanged.

- **Image**: `--enable-vaapi`, the h264/hevc VAAPI hwaccels, the `h264_vaapi` encoder and the
  `scale_vaapi`/`tonemap_vaapi` filters, with libva and Intel's `iHD` driver (amd64 only).
- **Deploy**: `/dev/dri/renderD128` is passed into the den container, and `provision-podman.sh` passes it
  on to this one with the node's group (a drop-in written only where the device exists).
- **A cap of its own.** The iGPU is shared with the camera stack (Frigate/Scrypted), and Incus has no GPU
  priority — `/dev/dri` is first come, first served. So transcodes are capped inside den-remux:
  `MAX_TRANSCODES` (default 1), separate from `MAX_SESSIONS`; copies do not count against it, and an ended
  session gives its transcode back at once.
- **Tested** by the flags (`job.rs`) and the slot (`tests.rs`) here, and on the box's GPU by hand: CI has no
  GPU.

## Run

```bash
docker build -t den-remux .
docker run -d --name remux -p 8095:8095 -v remux-scratch:/cache \
  -e SCOUT_ORIGINS=http://<scout>:8080 -e REMUX_SCOUT_KEY=<secret> \
  -e BROWSER_KEY_HASHES=<sha256> -e REMUX_URL_KEY=<secret> den-remux
curl -c jar -H 'content-type: application/json' -d '{"key":"<key>"}' http://localhost:8095/remux/login
curl -b jar -H 'content-type: application/json' \
  -d '{"imdb":"tt0111161","scout":"http://<scout>:8080/<scoped-config>"}' http://localhost:8095/remux/session
```

The cookie is `Secure`: a browser keeps it over HTTPS (tailscale serve) or on `localhost` only.

## Tests

`cargo test` is hermetic: parsers against the fixtures in `testdata/` (see its README), playlists,
signing, cookies, redaction, release picking, the scoped-scout validation, and a session created against a
fake scout that refuses a scoped config without the key and a file host that fails if it ever sees the
key. The `#[ignore]`d tests drive the whole path with real ffmpeg: three end-to-end remuxes (H.264 and HEVC
Matroska, MP4), the session cap, the idle kill (ffmpeg killed and reaped), and `DELETE` → 410. They
run in the Dockerfile's `test` stage, against the ffmpeg the image ships — another version seeks
differently (ffmpeg 8.0 lands a restarted H.264 Matroska run one keyframe early), and the alignment
depends on exactly how it seeks. CI's `e2e` job runs the same:

```bash
docker build --target test .
```

## Deploy

Planned like the other addons: a Podman Quadlet unit from the den repo's `deploy/`, uid 65532, every
capability dropped, read-only rootfs, scratch bind-mounted from `/var/lib/den/remux-scratch` (owned by
65532), updated through the health-gated `den-update`.

**Release images.** `docker-publish` builds on a `v*` tag, and again every Monday from the newest tag with
the bases re-pulled and no cache, so a fix to the distroless base or to the OpenSSL ffmpeg links statically
reaches the box between releases. Trivy scans each image before `:latest` moves (it sees the distroless
base; the static binaries carry no package database, so their OpenSSL and ffmpeg are tracked by their
pins). Every image carries SLSA provenance and an SBOM and is signed keylessly with cosign:

```bash
cosign verify \
  --certificate-identity-regexp '^https://github\.com/oxyc/den-remux/\.github/workflows/docker-publish\.yml@refs/(heads/main|tags/v[0-9]+\.[0-9]+\.[0-9]+)$' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  ghcr.io/oxyc/den-remux@sha256:<digest>
```
