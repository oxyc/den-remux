# den-remux

Den's playback for browsers — and, through the same URLs, for AirPlay and Cast receivers. A phone or a
laptop cannot play what den-scout returns: Matroska, and Dolby/DTS/TrueHD audio. den-remux copies the
video, copies the audio where the player plays it as it is or re-encodes it to AAC (stereo, or 5.1 or 7.1 where the
player plays it), and serves HLS (fMP4) cut on the file's
own keyframes.

```
browser ──POST /remux/session {imdb, scout}───►  scout (the library's install URL, its credential) → cached H.264/HEVC/AV1/VP9 release
        ◄──{ playlist: /remux/s/<sid>/<sig>/master.m3u8 }       probe: duration, tracks, keyframe index
<video> ──GET /remux/s/<sid>/<sig>/seg<N>.m4s─►  ffmpeg: -c:v copy -c:a aac, one fMP4 file per GOP
```

This is the MVP ("phase 2") of oxyc/den#11: movies and episodes, cached releases only, AAC stereo, two sessions.

## Routes

```
POST   /remux/login                     {key} → 204 + Set-Cookie + X-Den-Browser-Token; 401 bad_key · 429 rate_limited
POST   /remux/session                   {imdb, season?, episode?, filename?, scout?, audio?, audioTrack?,
                                         subtitles?, subtitleLanguages?, videoCodecs?, playable?, startAt?, maxBitrate?, player?} (a full scout install,
                                         or browser cookie/bearer) → 201
                                        {sid, playlist, release:{label,filename,size}, duration, expiresAt,
                                         video:{codec: h264|hevc|av1|vp9, transcoded}, audioTrack, audioChannels,
                                         audioTracks:[{language,name,channels,commentary}], subtitles}
                                        401 not_logged_in · 403 scout_refused
                                        400 bad_request/bad_scout/bad_subtitles/bad_audio_track
                                        404 no_release/no_playable_release · 429 too_many_sessions/rate_limited
                                        502 scout_unavailable · 503 scout_unconfigured/transcode_unavailable
POST   /remux/releases                  {imdb, season?, episode?, scout?, videoCodecs?, playable?} (a full scout install, or browser cookie/bearer)
                                        → 200 {releases:[{label,filename,size,plays,why}]}: `plays` is "yes" | "convert" |
                                        "no" for the player that sent `playable` (as /remux/session parses it) or
                                        `videoCodecs`, by scout's attributes and by what opening a release has shown of
                                        it (remembered in memory); `why` is a short reason, null for a yes. Without a
                                        report every release is "yes". A session's `release.requested {filename, why}`
                                        names the release it was asked for when it played another. What a session could play, in the
                                        order it would try them, for a player to name one as `filename`. No URLs
GET    /remux/speed?bytes=<n>           200 application/octet-stream: n random bytes (2 MiB unnamed, 8 MiB at most),
                                        no-store, no credential; 400 for a count that isn't one · 429 rate_limited
GET    /remux/s/<sid>/<sig>/master.m3u8 one variant: CODECS "<avc1…|hvc1…|av01…|vp09…>,mp4a.40.2", BANDWIDTH from size/duration;
                                        copied Dolby or FLAC audio: CODECS "…,ec-3|ac-3|fLaC" and an AUDIO rendition with CHANNELS;
                                        5.1 or 7.1 AAC: an AUDIO rendition with CHANNELS="6"|"8";
                                        HDR AV1 or VP9: VIDEO-RANGE=PQ|HLG
GET    /remux/s/<sid>/<sig>/media.m3u8  VOD, #EXT-X-MAP init.mp4, segments on real keyframes, #EXT-X-ENDLIST;
                                        #EXT-X-START:TIME-OFFSET=<startAt>,PRECISE=YES for a resume
GET    /remux/s/<sid>/<sig>/init.mp4
GET    /remux/s/<sid>/<sig>/seg<N>.m4s  200 when made (waits up to 20 s), else 503 + Retry-After: 2 (no-store)
GET    /remux/s/<sid>/<sig>/sub<N>.m3u8 a subtitle rendition: den-subtitles' one WebVTT segment spanning the film, or the
                                        release's own track as a WebVTT segment per video segment
GET    /remux/s/<sid>/<sig>/sub<N>.vtt  text/vtt, kept for the session; an empty document, no-store, when nothing in
                                        that language was found or den-subtitles failed (asked again next time)
GET    /remux/s/<sid>/<sig>/sub<N>_<W>.vtt
                                        the release's own track over video segment W; waits up to 20 s for the job to read
                                        past it, else 503 + Retry-After: 2 (no-store)
GET    /remux/s/<sid>/<sig>/speed?bytes=<n>
                                        the same bounded speed probe, authorized by this session URL; two attempts
POST   /remux/s/<sid>/<sig>/report      {code, message}: the player couldn't play it — logged against the session, 204;
                                        at most three attempts per session, including malformed reports
DELETE /remux/s/<sid>/<sig>             204; the session's URLs answer 410 from then on
GET    /health, /remux/health           200 {status} — ok, or degraded with a reason (Maintenance); the second is
                                        what a client probes under the /remux mount (den-spec routes-v1)
GET    /metrics                         Prometheus text (bearer METRICS_TOKEN; 404 without it)
POST   /remux/admin/kill                {owner: "grant:<gid>"} → {ended: n}; only with x-den-edge-secret (EDGE_SECRET),
                                        else the same 404 as any unknown route
anything else                           404 {"error":"not_found"}
```

`scout` is the scout install URL the web app reads from the library's `set:plugins` group
(`http://<scout>:8080/<config>`); without it — for a logged-in browser — the server's `SCOUT_INSTALL_URL` is used. `filename` prefers that release when it is playable here.
`player` (`native` or `hls.js`) is the HLS player the page chose. A `native` session is answered only once its
`init.mp4` is written (up to 15 s): Apple's players give up on the map after about five seconds ("No response for map",
which Safari shows as "Media failed to decode"), and a job seeking into a remote file takes longer than that. It also
ends the session's log line, `client: Chrome 151 · macOS · hls.js` — browser and OS read from the User-Agent, which
itself is never logged. Brave on iOS sends Safari's User-Agent, so it reads as Safari.

- **Audio.** `audio` is the player's languages, most wanted first, in any spelling a release or a browser
  uses (`en-US`, `eng`, `fin`). The first language a track is in wins — the default-flagged track among
  several — and a commentary never does (Matroska's FlagCommentary, or "commentary" in the track title).
  No match: the first default track that is not a commentary. `audioTrack` picks one by index from an
  earlier session's `audioTracks` (send that session's `filename` with it). One track per session, so
  switching language is a new session. For a player whose `playable.eac3` says it plays E-AC-3 and AC-3 in
  fMP4 HLS (Safari, Apple's receivers), such a track is copied as it is, channels and all, and the master names
  it (`ec-3`/`ac-3`, an AUDIO rendition with `CHANNELS`); so is a FLAC track (`fLaC`) for a player whose
  `playable.flac` says it plays FLAC in fMP4 HLS. Every other track is converted to AAC-LC: 7.1 at 512 kbit/s from a
  track of eight channels or more for a player whose `playable.aac71` says it plays 8-channel AAC (`CHANNELS="8"`);
  5.1 at 384 kbit/s from a track of six channels or more (7.1 folds down to 5.1) for a player whose
  `playable.aacMultichannel` says it plays multichannel AAC — named in the master by an AUDIO rendition with
  `CHANNELS="6"` — and stereo at 192 kbit/s otherwise, byte for byte as before. `audioChannels` in the answer is what the session carries, beside the track's
  own `channels` in `audioTracks`: fewer means the track plays downmixed to stereo.
- **Subtitles.** `subtitles` is den-subtitles' install URL from the library, on `SUBTITLE_ORIGINS`;
  `subtitleLanguages` (up to 8, most wanted first) become WebVTT renditions in the master playlist — what AirPlay
  and Cast receivers show, which a page's own `<track>` never reaches. Every one is `DEFAULT=NO,AUTOSELECT=YES`,
  so an unset preference remains off and the player or receiver can select one. Nothing is fetched until a player opens one:
  then den-remux asks den-subtitles for the title with the release's OpenSubtitles hash, size and
  filename (the Apple TV's hints, so an exact-encode match ranks first) and serves the first subtitle in
  that language. A subtitle URL off `SUBTITLE_ORIGINS` is skipped.
  **The release's own text tracks** — SRT, ASS/SSA, WebVTT, `mov_text` — are the fallback where den-subtitles lists
  nothing in a language, and add the release's other languages (up to eight renditions in all, the languages asked for
  first), with or without a den-subtitles install. Matroska interleaves them with the video, so a separate pass would
  read the whole file; instead each ffmpeg run writes them as one more WebVTT output beside the video it copies, and the
  session cuts what the runs have written into a segment per video segment (`sub<N>_<W>.vtt`), made once a run has read a
  second past it. A seek starts another run, which writes from its own start; what earlier runs wrote stays. A cue
  that began before a run's start point, and is still showing there, is not in it. ASS/SSA lose their styling. A forced
  track (foreign-language parts only), a bitmap track (PGS, VOBSUB) and a track that names no language are not offered.
- **Video codecs.** `videoCodecs` is what the player takes (`["h264"]` for Firefox or an old
  Chromecast); empty is H.264 and HEVC. Without HEVC, H.264 releases are tried first, and an HEVC-only
  title is transcoded to H.264 on the GPU (Hardware transcode, below) — or refused with
  `transcode_unavailable` while transcoding is off or in use.
- **What the player decodes.** `playable` — `{h264, h264High10, hevcMain, hevcMain10, hevcHighTier, hdr, eac3,
  aacMultichannel, dolbyVision: {p5, p8}, av1, av1Main10, av1Hdr, flac, aac71, vp9, vp9Profile2}`, the
  highest level it takes of 8-bit H.264 and of H.264 High 10 (`level_idc`, 0x33 is 5.1), of 8-bit and 10-bit HEVC
  (level × 30, 153 is 5.1) and of HEVC's High tier, 0 for none, and whether it decodes PQ HDR — decides over
  `videoCodecs` when given. A High 10 release (profile 110) is passed over unless `h264High10` reaches its level:
  the box's GPU has no High 10 decoder to convert it. `dolbyVision` says which Dolby Vision it shows as such. With
  `p8`, a copied profile 8.1/8.2/8.4 keeps its RPU and configuration record and is named beside its base layer's
  `hvc1…`: `SUPPLEMENTAL-CODECS="dvh1.08.LL/db1p|db2g|db4h"`, `VIDEO-RANGE=PQ|SDR|HLG`. With `p5`, profile 5 plays
  as a copy tagged `dvh1` (`CODECS="dvh1.05.LL"`, `VIDEO-RANGE=PQ`) instead of being skipped. Profile 7, and every
  transcode, plays its base layer alone. A UHD Blu-ray
  remux is often High tier, which Apple's decoders refuse whatever their tests say, so the web app reports 0 on
  them; a player that sends no `hevcHighTier` has it converted. The session's log line says what the player
  reported. A release is probed first and its own codec string
  compared: an HEVC one beyond the player (10-bit, 4K, or HDR it can't decode) is transcoded, an H.264 one passed
  over.
- **AV1.** `playable.av1` and `av1Main10` are the highest `seq_level_idx` the player decodes at Main profile, 8-bit
  and 10-bit (8 is level 4.0, 13 is 5.1), and `av1Hdr` whether it decodes 10-bit AV1 in PQ; absent is 0 and false.
  An AV1 release is only ever copied — the box's UHD 630 has no AV1 decoder — so for a player that doesn't report
  it at the release's level, depth and HDR it is passed over, never converted: scout's `codec: "av1"` keeps it
  unopened, and the probe refuses one scout didn't name. A player sending no `playable` gets no AV1 at all, and
  neither does `/remux/releases` without one. A release at another profile or at High tier plays
  nowhere: the web app asks about Main profile and Main tier only. The copy keeps its `av01` sample entry and
  `av1C`; the master names the AV1-ISOBMFF codec string — with its colour fields whenever the stream describes its
  colours, so HDR10 is `av01.0.13M.10.0.110.09.16.09.0` — and `VIDEO-RANGE=PQ` (or `HLG`) for HDR. The colours come
  from Matroska's Colour element or MP4's `colr`, and where those are silent from the sequence header in `av1C`.
- **VP9.** `playable.vp9` and `vp9Profile2` say the player decodes VP9 profile 0 (8-bit) and profile 2 (10-bit) in
  fMP4 HLS; absent is false. Safari's own HLS player plays profile 0 at 1080p30 and 1080p60 on iPhone and Mac (codec
  lab, 2026-09-15), but profile 2 there is unmeasured, so `vp9Profile2` counts only for a session that names
  `player: "hls.js"`: for a native session, or one that names no player, it is cleared — in the report sent to scout
  too. Like AV1, a VP9 release is only ever copied:
  for a player that doesn't take its profile it is passed over, unopened where scout's `codec: "vp9"` and `bitDepth`
  say so. 12-bit, and profiles 1 and 3, play nowhere. The copy keeps its `vp09` sample entry; the master names
  `vp09.PP.LL.DD`, with the colour fields whenever the stream describes its colours, from MP4's `vpcC` or Matroska's
  CodecPrivate features and Colour element. What they don't record is assumed: 10-bit profile 2 where the transfer is
  PQ or HLG and 8-bit profile 0 otherwise, and the lowest level whose picture size and sample rate cover the release at
  its frame rate (60 fps where that isn't known). Profile 2 with a PQ or HLG transfer is HDR: `VIDEO-RANGE=PQ|HLG`.
- **A remote player's link.** Away from home every byte crosses the home upload, so a player that reached den-remux
  off the LAN times its link with `GET /remux/speed` and sends `maxBitrate`, in bits a second (the web app sends 70 %
  of what it measured). A release that plays as it is but whose average bitrate — its size × 8 over its duration —
  is above it is kept aside while one that fits is looked for, in rank order as before. With none that fits, a
  transcode is taken next: of a release that plays only converted, else of an HEVC one that plays as it is, where the
  preset comes in under it. The preset is 1080p at 8 Mbit/s where `maxBitrate` carries that and 5.1 audio, else 720p
  at 3 Mbit/s (4.5 max) — two and no more, since a transcode holds the box's one GPU slot for the film, and below 720p
  a remote player does better with the smallest release as it is. With the GPU busy or nothing to convert, that
  smallest copy plays: a stall now and then beats nothing. A named `filename` is weighed alone, the same way. Without
  `maxBitrate` nothing changes. The session's log line names the link.
- **Resume.** `startAt` is the second the player starts at. The media playlist names it (`EXT-X-START`, so
  Safari's native player starts there), and the first job starts a segment before it rather than at zero, so a
  resume runs ffmpeg once instead of twice. Negative is a 400; at or past the end starts from zero.
- **Which release.** The session passes the browser's `playable` on to scout in `X-Den-Playable`, and scout ranks
  the list for that browser: releases it plays as they are first, then those this service converts, then those
  it can't play (den-scout's README, "Ranked for a browser"). A player that sends no report gets scout's TV
  ranking. Before any is opened, scout's
  attributes (`codec`, `resolution`, `hdr`, `bitDepth`, `dvProfile`, `probed`) sort them for this player: those
  that play as they are, then those that play only converted; one they rule out (Dolby Vision profile 5 without `dolbyVision.p5`, H.264
  beyond the player, 10-bit H.264 without `h264High10`, AV1 or VP9 beyond the player) is never opened, and anything they don't say is left to
  the probe. Three are opened at a time and taken in rank order; the first that plays as it is wins, one that
  plays only converted is the last resort, and the search goes on — up to 12 releases, starting none after 30 s,
  20 s each — while a release that may play as it is remains. An opened release — its debrid link and probe — is remembered
  per install for 10 minutes (8 releases at most), so another of its audio tracks, or the title again, starts
  without scout or a probe; a link that stops working mid-session is fetched again and forgotten. A named `filename` (another
  audio track of the release playing) goes first and is kept, converted if need be.

Only a valid `/remux/s/…` bearer URL gets public CORS. Those responses carry
`Access-Control-Allow-Origin: *`, allow `Range` and expose
`Content-Range`/`Content-Length` — a Cast receiver's page is on Google's origin. Playlists are
`application/vnd.apple.mpegurl`; `init.mp4` and segments are `video/mp4`, always whole (`Accept-Ranges: none`; a
`Range` is ignored). What a session's URL serves is fixed for its life and signed for it alone, so playlists, found
subtitles, `init.mp4` and segments are `private, max-age=<seconds until the session expires>` (`immutable` on the
media): a seek back to a segment already pruned from scratch comes from the player's cache instead of restarting
ffmpeg. Errors, a 503 not-ready and an empty subtitle are `no-store`. A retry is a new session with its own URLs, so
nothing cached reaches it. Login and session
creation answer CORS only for `WEB_ORIGINS`: the Den web app on its public name, whose player plays from this
service's tailnet address because video never goes through the Cloudflare tunnel (oxyc/den#15).

A logged-in browser that creates a session while it already has one ends the old one (it has moved on to
another title), and an install past `MAX_SESSIONS_PER_INSTALL` ends its oldest, so `MAX_SESSIONS` counts
people watching, not titles clicked.

## Security model

- **The scout install is the credential.** Every device in a Den library holds the library's plugin
  list, scout's full install URL included (one trust level, oxyc/den#12), and the web app sends that URL
  with each session request. That URL is what every Den addon's credential is, so it admits a session on
  its own: no login, and nothing per browser or per household to configure. Scout alone decides —
  den-remux sends no key of its own for such a request — so an install scout revokes (`REVOKED_INSTALLS`,
  `CONFIG_EPOCH`) stops playing here too (`403 scout_refused`), and a scope=availability URL, which
  den-scout lets list and play only for `X-Den-Remux-Key` (`REMUX_SCOUT_KEY`), plays nothing unless a
  logged-in browser sends it (`401 not_logged_in`). A scout URL is accepted only on an origin in
  `SCOUT_ORIGINS` with exactly one base64url config segment and no credentials, query or fragment — the
  SSRF guard, without which a request could make this service fetch anything on the LAN.
- **Shares and limits.** An install plays `MAX_SESSIONS_PER_INSTALL` sessions at once and its oldest gives
  way to a new one, so one household — or one leaked install URL — cannot hold every slot; `MAX_SESSIONS`
  and `MAX_TRANSCODES` cap the box. Logins, new sessions and speed tests are limited to 10 a minute per visitor,
  counted together: the address a `TRUSTED_PROXIES` proxy forwarded, else the connection's. A speed test is up to
  8 MiB of the home upload, and all it tells anyone is how fast that is.
- **The key reaches scout and nothing else.** An HTTP client forwards custom headers across a
  cross-origin redirect, which would hand the key to the debrid's CDN on scout's 302. den-remux talks to
  scout with a client that follows no redirect and follows play URLs by hand, sending the key only to
  scout's origin; the integration tests put the file host on another origin and fail if it ever sees
  the key.
- **Tickets and debrid links stay here.** The browser names a title and gets back one release's session.
  Scout's play URL and the debrid link it leads to are used only by this process and its ffmpeg, so every
  byte leaves from the homelab's IP. `SCOUT_INSTALL_URL`, this service's own install, is a fallback for
  driving it by hand.
- **A browser key is for what needs den-remux to vouch.** An availability-only scout install, or the
  `SCOUT_INSTALL_URL` fallback, plays only for a browser that posted its key once; the server holds only
  SHA-256 hashes (`BROWSER_KEY_HASHES`, optional). The cookie it gets is `HttpOnly; Secure;
  SameSite=Strict; Path=/remux`, HMAC-signed with an expiry (30 days), and stops working when its key's
  hash is removed or `REMUX_URL_KEY` rotates. A web page on another site uses the separately signed
  `X-Den-Browser-Token` returned by the same login: send `Authorization: Bearer <token>` on
  `/remux/session` and `/remux/releases`. It expires after eight hours and has the same revocation checks;
  it cannot stand in for the cookie, a session URL signature, or the metrics token. The web app keeps
  it only in page memory, never the raw browser key, and asks for login again after a reload when no
  same-origin cookie is available. `WEB_ORIGINS` exposes the response header and permits the authorization
  preflight; third-party cookies and credentialed CORS are unnecessary. An explicit bearer takes
  precedence over the cookie. Both browser credentials admit release listing and session creation.
- **Session URLs are signed bearer URLs.** `/remux/s/<sid>/<sig>/…`: `sid` is 128 random bits, `sig` is
  `HMAC-SHA256(REMUX_URL_KEY, sid‖exp)` truncated to 128 bits, compared in constant time. No cookie is
  asked for there, which is what will let a Cast or AirPlay receiver play. A session lives for the film's
  runtime plus an hour (six hours at most), ends after `SESSION_IDLE_SECS` without a request, and
  `DELETE` ends it at once. Whoever holds the URL can watch that one title through the homelab until then —
  and nothing else: no scout, no ticket, no debrid link. A signature that does not match is a 404, so a
  guess learns nothing about which sessions exist. Forged or unknown-session URLs return an empty 404 without CORS,
  and each session accepts at most three player reports, bounding both log volume and disk writes.
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
   replacing by guess`, dts past pts). The image enables the h264/hevc decoders for probing only. AV1 needs no
   decoder — its packets are never reordered — but it does need the `av1` parser: without it, a Matroska file with
   no Colour element came out of the minimal build with no `colr` box in its init, its PQ and BT.2020 lost; with it
   they are read from the sequence header.

The integration tests hold all of it: they fetch segments out of order (3, then 1 — behind that run — then
0) so the job restarts twice, and check with ffprobe that every segment starts with a keyframe at its
playlist time (±1 frame), ends before the next, carries audio, and that the segments joined back-to-back
hold every source frame exactly once over the full duration, with no hole in the audio at any join — for H.264,
HEVC and AV1 Matroska, for MP4, and for 5.1 and 7.1 tracks converted to AAC 5.1. Where a restarted run joins the run
before it, one audio frame may overlap (a restart reads audio from a little before its keyframe, see 3 above), which a
player drops; the tests allow that one frame and no more.

## Resource use

Low resource use comes first, and the design follows from it:

- **Image** — a static musl `den-remux` and an ffmpeg configured with `--disable-everything` plus only what
  a session uses, on Alpine: **67.3 MB**. Until the GPU transcode it was 13.4 MB on `distroless/static`;
  libva has to load Intel's driver at run time, which a fully static binary cannot, and that driver is
  most of the image now. Disk only: nothing loads it until a transcode starts. den-reel's image on the box is 693 MB
  (Debian ffmpeg, yt-dlp, deno, MP4Box).
- **Idle is zero work.** No timers, sweeps or polling without sessions: scratch is swept at start and a
  session's directory is removed when it ends. Each session has one task, which ticks every 500 ms only
  while its ffmpeg is actually running and otherwise sleeps until a request or its idle deadline.
- **One ffmpeg per session**, `-threads 1` (the video is copied; AAC needs one thread — a 7.1 track decoded and
  encoded to 5.1 took 0.90 CPU-s for 30 s against stereo's 0.60, 34× real time on one Apple Silicon core), paused with
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
| `ORIGIN_ALIASES` | — | `<public origin>=<LAN origin>` pairs (`https://d-scout.oxy.fi=http://192.168.86.193:8080,…`). An install, play or subtitle URL on a public name is fetched at the LAN address: scout and den-subtitles are on this box, so the WAN and the tunnel never sit between them, and this service never needs the Access token the public names ask for. The public name must still be in `SCOUT_ORIGINS`/`SUBTITLE_ORIGINS`. |
| `BROWSER_KEY_HASHES` | — | Optional. Comma-separated hex SHA-256 of each browser key, for an availability-only scout install or `SCOUT_INSTALL_URL`. Removing one logs that browser out. |
| `REMUX_URL_KEY` | random | **Secret.** Signs cookies and session URLs. **Set it**: unset, every restart logs the browsers out (`/health` says `url_key_ephemeral`). Rotating it kills every cookie and session URL. |
| `MAX_SESSIONS` | `2` | Sessions at once; the next gets 429 `too_many_sessions`. |
| `MAX_SESSIONS_PER_INSTALL` | `2` | Sessions one scout install plays at once without a login; past it, its oldest ends. |
| `SESSION_IDLE_SECS` | `600` | A session with no request for this long is ended (min 30). |
| `SCRATCH_DIR` | `/cache` | Where GOP files go. den-remux's alone: every `s-*` directory in it is deleted at start. |
| `SCRATCH_MAX_BYTES` | `1073741824` | Cap across sessions; past it a job pauses once the requested segment is done (min 64 MiB). |
| `FFMPEG_PATH` | `ffmpeg` | The image sets `/usr/local/bin/ffmpeg`. (There is no `FFPROBE_PATH`: probing is done in-process.) |
| `MAX_TRANSCODES` | `1` | Sessions transcoding on the GPU at once; `0` turns transcoding off. Copies do not count. |
| `VAAPI_DEVICE` | `/dev/dri/renderD128` | The GPU's render node. Transcoding is on only when it exists and ffmpeg has the VAAPI encoder and filters (the startup line says `transcode=vaapi(max N)` or `off`). |
| `TRUSTED_PROXIES` | — | Proxy IPs (comma-separated) whose `X-Forwarded-For` names the visitor, for the limit on logins and new sessions: `tailscale serve`'s host. |
| `WEB_ORIGINS` | — | Pages on another origin that may log in and start sessions (comma-separated): the Den web app on its public name. Session files are readable from anywhere already. |
| `METRICS_TOKEN` | — | Turns on `/metrics` behind `Authorization: Bearer <token>`; otherwise it is a 404. |
| `EDGE_SECRET` | — | **Secret**, shared with den-edge (`REMUX_EDGE_SECRET`). With it, a request to `/remux/session` or `/remux/releases` carrying `x-den-edge-secret` and `x-den-owner: grant:<8 hex>` plays as that guest grant: its own cap (`GUEST_MAX_SESSIONS`), never a transcode, never counted against a host. It also turns on `POST /remux/admin/kill {"owner":"grant:<gid>"}` → `{"ended": n}`, which ends that grant's sessions. Unset, the headers are ignored and the admin route is a 404. |
| `GUEST_MAX_SESSIONS` | `2` | Sessions one guest grant plays at once (1–8); past it, its oldest ends. |
| `LOG_REQUESTS` | off | `1` writes `<METHOD> <path> <status> <ms>ms[ rid=<X-Request-Id>]` per response. |
| `PORT` | `8095` | |

A browser key and its hash: `key=$(head -c 24 /dev/urandom | base64 | tr '+/' '-_'); printf %s "$key" | sha256sum`.

## Maintenance

`/health` is 200 with `status: ok`, or `degraded` with the first reason sessions cannot work:
`ffmpeg_unavailable` (checked at start: matroska and mov demuxers, hls muxer, aac encoder, https),
`scratch_unwritable`, `scout_unconfigured` (neither `SCOUT_ORIGINS` nor `SCOUT_INSTALL_URL`),
`scout_key_missing`, `url_key_ephemeral`.

`/metrics` (gauges and counters prefixed `remux_`): sessions and the cap, ffmpeg processes alive, scratch
bytes and the cap, transcodes and their cap (0 when off), sessions and ffmpeg runs started. The log is state changes: the startup line, one line
per session start and end (with the reason: `idle`, `expired`, `deleted`, `replaced`, `shutdown`), a
failed ffmpeg run's last stderr line (scrubbed), and rate-limited upstream failures.

ffmpeg is built from a checksummed source tarball (`FFMPEG_VERSION`/`FFMPEG_SHA256` in the Dockerfile);
dependabot cannot bump it, so bump both lines by hand.

## Limits (MVP)

- **Cached releases only** (an uncached one would start a debrid download).
- **H.264, HEVC, AV1 and VP9 sources only.** AV1 and VP9 play only as a copy, for a player whose `playable` says it
  decodes them (10-bit VP9 through hls.js alone); there is no conversion to fall back on. MPEG-4 Part 2/XviD and VC-1 are
  skipped. Files without a keyframe index (Matroska with no Cues) are skipped.
- **Audio is AAC**, one track per session: 7.1 for a player that reports `aac71`, 5.1 for one that reports
  `aacMultichannel` (a 7.1 track folds down to it), stereo for every other — or, for a player that plays them,
  E-AC-3/AC-3 or FLAC copied with no stereo alternate beside it (Apple's authoring spec asks for AC-3 beside E-AC-3 for
  devices without it). A 5.1 or 7.1 variant carries no stereo alternate either: the player downmixes it for stereo
  output.
- **Text subtitles only**: den-subtitles', and the release's own SRT, ASS/SSA, WebVTT and `mov_text` tracks. Bitmap
  tracks (PGS, VOBSUB) have no WebVTT form and are not carried.
- **Dolby Vision profile 5** has no HDR10/SDR base layer: Safari shows it, Chrome cannot, and stripped or
  transcoded its colours come out green and purple. A session skips it — the probe reads the profile — unless
  `playable.dolbyVision.p5` says the player shows profile 5 and it takes the release as it is: then it is copied,
  tagged `dvh1`.
  **A profile 5 with no record.** Some releases (an Amazon WEB-DL, say) are profile 5 in the bitstream — every
  frame carries its RPU — but the Matroska has no `dvcC`/`dvvC` and the HEVC VUI names no colours. For 10-bit HEVC
  that names no transfer, the probe reads the first cued Cluster's first RPU (libdovi's profile guess: full-range base
  layer, no residual is profile 5) and sets `dolby_vision_recordless`. For a player that shows profile 5 it is kept
  like any other: the RPUs stay in, and ffmpeg, which writes a `dvcC` only from the source's side data, is
  followed by a patch of the served `init.mp4` (`job::add_dovi_record`): `hvc1` becomes `dvh1` and a 24-byte `dvcC`
  (profile 5, level 6, RPU present, no enhancement layer, compatibility 0) joins the `hvcC`, the sizes above it fixed;
  a layout that is not a fragmented init is refused rather than patched. Everywhere else it is skipped, as a record
  would be, and never converted.
- **Bandwidth**: at home this is fine. Away from home every byte crosses the home **upload** link, so a
  remote session is bounded by it — a 4K remux will not fit. A player that sends `maxBitrate` gets a release that
  fits, or a 1080p or 720p transcode, or failing both the smallest copy; its measure is taken once, before the
  session, and nothing adapts to a link that changes mid-film (one variant, no ABR ladder).
- **One listener.** The public `/remux/s/` listener for Cast/AirPlay (#11 §C, #31) is Caddy in front of this service,
  deployed by oxyc/den's `deploy/`; it forwards only strict signed-session paths.

### Hardware transcode

For a player without HEVC, or without the HEVC a release needs (`playable`), an HEVC release is decoded, scaled
and — HDR10, HLG or Dolby Vision — tone-mapped on the box's UHD 630, and encoded to H.264 High 4.1 there (VAAPI,
the `h264_vaapi` encoder), fitted inside 1920 × 1080 at 8 Mbit/s (12 max) — or, for a `maxBitrate` below that,
inside 1280 × 720 at 3 Mbit/s (4.5 max). The tone-mapper marks every frame it
makes BT.709, which is what the output carries: H.264 tagged BT.2020 and PQ is HDR H.264, which Apple's decoders
refuse. (Naming those colours on the command line as well breaks the filter graph, so it isn't done.) A UHD Blu-ray remux often leaves Matroska's Colour element
out, so where the container is silent the colours come from the VUI of the SPS in the `hvcC` record, and Dolby Vision
counts as HDR too — its base layer is HDR10 or HLG, except profile 8.2's, already SDR. Taking such a release for SDR
fails twice over on Apple's player: the copy's variant names no `VIDEO-RANGE=PQ` for PQ in its SPS, and the
transcode skips the tone-mapper and carries the source's PQ tags, and either is refused at `init.mp4` as error 3,
before a segment is asked for. A copied HDR variant also names `FRAME-RATE` (Matroska's DefaultDuration, an MP4's
sample timing): Safari passes over a `VIDEO-RANGE=PQ` variant without one, with no error. `-force_key_frames source` puts an output
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
key. The `#[ignore]`d tests drive the whole path with real ffmpeg: five end-to-end remuxes (H.264, HEVC and AV1
Matroska, MP4, and a 5.1 and a 7.1 track converted to AAC 5.1), the session cap, the idle kill (ffmpeg killed and reaped), and `DELETE` → 410. They
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
