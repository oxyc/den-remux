# Fixtures

Real container files, deliberately small (320×180 test pattern, under 1 MB each), so the parsers and the
remux path are tested against what a muxer actually writes. Every clip has B-frames and keyframes forced
at irregular 2–4 s intervals: the irregularity is what makes "first keyframe at or after n × 6 s" cut
unevenly, and the B-frames are what trigger ffmpeg's seek back-off (see `src/job.rs`, `SEEK_PAST`).

| File | What it pins | Keyframes (ffprobe) | Duration |
|---|---|---|---|
| `h264.mkv` | Matroska Cues via the SeekHead; two audio tracks (AC-3 eng, AAC mono swe) | 0, 2.5, 5, 8, 10.5, 13, 16, 19.5, 22, 24, 27.5 | 30.021 |
| `hevc.mkv` | HEVC → `hvc1` codec string and `-tag:v hvc1`; E-AC-3 audio | 0, 3, 6.5, 9, 12, 15.5, 18, 21, 24.5, 27 | 30.005 |
| `h264.mp4` | MP4 `stss`/`stts`/`ctts` with an edit list; faststart | 0, 3.5, 6, 9, 11.5, 15, 18, 20.5, 24, 27 | 30.000 |
| `moov-at-end.mp4` | A `moov` after the `mdat`, found by walking box headers | 0, 2, 5 | 8.0 |
| `scout-streams.json` | A den-scout stream list: uncached, AV1, XviD/AVI, 3D and cache-unknown releases to skip | — | — |

## Regenerating

ffmpeg 9.0.1 (nixpkgs) made these; any recent build with libx264 and libx265 will do.

```sh
cd testdata
ffmpeg -y -f lavfi -i testsrc2=size=320x180:rate=24:duration=30 \
  -f lavfi -i sine=frequency=440:duration=30:sample_rate=48000 -f lavfi -i sine=frequency=660:duration=30:sample_rate=48000 \
  -map 0:v -map 1:a -map 2:a -c:v libx264 -preset veryfast -crf 36 -bf 3 -g 1000 -keyint_min 1000 -sc_threshold 0 \
  -force_key_frames 0,2.5,5,8,10.5,13,16,19.5,22,24,27.5 -pix_fmt yuv420p \
  -c:a:0 ac3 -b:a:0 64k -ac:a:0 2 -c:a:1 aac -b:a:1 32k -ac:a:1 1 \
  -metadata:s:a:0 language=eng -metadata:s:a:1 language=swe h264.mkv

ffmpeg -y -f lavfi -i testsrc2=size=320x180:rate=24:duration=30 -f lavfi -i sine=frequency=550:duration=30:sample_rate=48000 \
  -map 0:v -map 1:a -c:v libx265 -preset fast -crf 36 -forced-idr 1 \
  -x265-params keyint=1000:min-keyint=1000:scenecut=0:bframes=3:open-gop=0:log-level=error \
  -force_key_frames 0,3,6.5,9,12,15.5,18,21,24.5,27 -pix_fmt yuv420p \
  -c:a eac3 -b:a 64k -ac 2 -metadata:s:a:0 language=eng hevc.mkv

ffmpeg -y -f lavfi -i testsrc2=size=320x180:rate=24:duration=30 -f lavfi -i sine=frequency=440:duration=30:sample_rate=48000 \
  -map 0:v -map 1:a -c:v libx264 -preset veryfast -crf 36 -bf 3 -g 1000 -keyint_min 1000 -sc_threshold 0 \
  -force_key_frames 0,3.5,6,9,11.5,15,18,20.5,24,27 -pix_fmt yuv420p \
  -c:a aac -b:a 64k -ac 2 -metadata:s:a:0 language=fra -movflags +faststart h264.mp4

ffmpeg -y -f lavfi -i testsrc2=size=160x90:rate=24:duration=8 -f lavfi -i sine=frequency=440:duration=8:sample_rate=48000 \
  -map 0:v -map 1:a -c:v libx264 -preset veryfast -crf 36 -bf 3 -g 1000 -keyint_min 1000 -sc_threshold 0 \
  -force_key_frames 0,2,5 -pix_fmt yuv420p -c:a aac -b:a 32k -ac 2 moov-at-end.mp4
```

The expected keyframe lists come from:

```sh
ffprobe -v error -select_streams V:0 -skip_frame nokey -show_entries frame=pts_time -of csv=p=0 <file>
```

A regenerated file must reproduce those lists exactly; `src/tests.rs` asserts them.
