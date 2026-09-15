# Fixtures

Real container files, deliberately small (320×180 test pattern, under 1 MB each), so the parsers and the
remux path are tested against what a muxer actually writes. Every clip has B-frames and keyframes forced
at irregular 2–4 s intervals: the irregularity is what makes "first keyframe at or after n × 6 s" cut
unevenly, and the B-frames are what trigger ffmpeg's seek back-off (see `src/job.rs`, `SEEK_PAST`).

| File | What it pins | Keyframes (ffprobe) | Duration |
|---|---|---|---|
| `h264.mkv` | Matroska Cues via the SeekHead; two audio tracks (AC-3 eng, AAC mono swe) | 0, 2.5, 5, 8, 10.5, 13, 16, 19.5, 22, 24, 27.5 | 30.021 |
| `hevc.mkv` | HEVC → `hvc1` codec string and `-tag:v hvc1`; E-AC-3 audio | 0, 3, 6.5, 9, 12, 15.5, 18, 21, 24.5, 27 | 30.005 |
| `hdr10.mkv` | HDR10: Main 10 HEVC, PQ and BT.2020 with mastering metadata — the transfer a conversion tone-maps | 0, 3, 6.5, 9, 12, 15.5, 18, 21, 24.5, 27 | 30.000 |
| `h264.mp4` | MP4 `stss`/`stts`/`ctts` with an edit list; faststart | 0, 3.5, 6, 9, 11.5, 15, 18, 20.5, 24, 27 | 30.000 |
| `moov-at-end.mp4` | A `moov` after the `mdat`, found by walking box headers | 0, 2, 5 | 8.0 |
| `av1.mkv` | AV1: SVT-AV1's 10-bit HDR10 (PQ, BT.2020), `V_AV1` with its `av1C` CodecPrivate, Colour element and sequence header; Opus audio | 0, 2.5, 6, 8.5, 12, 14, 18.5, 21, 24, 27.5 | 30.008 |
| `av1.mp4` | `av1.mkv`'s video copied into MP4: the `av01` sample entry's `av1C` and `colr` | as `av1.mkv` | 29.999 |
| `surround.mkv` | `h264.mkv`'s video copied, with a 5.1 AAC track (eng) and a 7.1 one (swe): converted to AAC 5.1 | as `h264.mkv` | 30.021 |
| `scout-streams.json` | A den-scout stream list: uncached, AV1, VP9, XviD/AVI, 3D and cache-unknown releases to skip | — | — |

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

ffmpeg -y -f lavfi -i testsrc2=size=320x180:rate=24:duration=30 -f lavfi -i sine=frequency=550:duration=30:sample_rate=48000 \
  -map 0:v -map 1:a -c:v libx265 -preset fast -crf 36 -forced-idr 1 \
  -x265-params "keyint=1000:min-keyint=1000:scenecut=0:bframes=3:open-gop=0:log-level=error:hdr-opt=1:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(40000000,50):max-cll=801,188" \
  -force_key_frames 0,3,6.5,9,12,15.5,18,21,24.5,27 -pix_fmt yuv420p10le \
  -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc \
  -c:a eac3 -b:a 64k -ac 2 -metadata:s:a:0 language=eng hdr10.mkv

ffmpeg -y -f lavfi -i testsrc2=size=320x180:rate=24:duration=30 -f lavfi -i sine=frequency=440:duration=30:sample_rate=48000 \
  -map 0:v -map 1:a -c:v libx264 -preset veryfast -crf 36 -bf 3 -g 1000 -keyint_min 1000 -sc_threshold 0 \
  -force_key_frames 0,3.5,6,9,11.5,15,18,20.5,24,27 -pix_fmt yuv420p \
  -c:a aac -b:a 64k -ac 2 -metadata:s:a:0 language=fra -movflags +faststart h264.mp4

ffmpeg -y -f lavfi -i testsrc2=size=160x90:rate=24:duration=8 -f lavfi -i sine=frequency=440:duration=8:sample_rate=48000 \
  -map 0:v -map 1:a -c:v libx264 -preset veryfast -crf 36 -bf 3 -g 1000 -keyint_min 1000 -sc_threshold 0 \
  -force_key_frames 0,2,5 -pix_fmt yuv420p -c:a aac -b:a 32k -ac 2 moov-at-end.mp4

# AV1 is SVT-AV1 (libsvtav1, 4.1.0 in nixpkgs): what AV1 releases are mostly encoded with, and its forced keyframes
# are one key packet each — libaom's keyframe filtering adds a second right behind every one. `setparams` tags the
# frames, which is where the encoder takes HDR10's colours from: with `-color_*` alone a libaom encode's transfer and
# primaries came out "unknown".
ffmpeg -y -f lavfi -i testsrc2=size=320x180:rate=24:duration=30 -f lavfi -i sine=frequency=500:duration=30:sample_rate=48000 \
  -map 0:v -map 1:a -vf setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc,format=yuv420p10le \
  -c:v libsvtav1 -preset 10 -crf 60 -g 1000 -force_key_frames 0,2.5,6,8.5,12,14,18.5,21,24,27.5 \
  -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc \
  -c:a libopus -b:a 32k -ac 2 -metadata:s:a:0 language=eng av1.mkv

ffmpeg -y -i av1.mkv -map 0:v -c copy -movflags +faststart av1.mp4

# A tone on each channel, the LFE's low and quieter, so a listen tells the channels apart. 40 kbit/s keeps the file under
# 1 MB; what a session does with the tracks is convert them, so their own quality doesn't matter.
ffmpeg -y -i h264.mkv \
  -f lavfi -i "aevalsrc=sin(440*2*PI*t)|sin(550*2*PI*t)|sin(660*2*PI*t)|0.3*sin(55*2*PI*t)|sin(770*2*PI*t)|sin(880*2*PI*t):c=5.1:s=48000:d=30" \
  -f lavfi -i "aevalsrc=sin(440*2*PI*t)|sin(495*2*PI*t)|sin(550*2*PI*t)|0.3*sin(55*2*PI*t)|sin(660*2*PI*t)|sin(770*2*PI*t)|sin(880*2*PI*t)|sin(990*2*PI*t):c=7.1:s=48000:d=30" \
  -map 0:v -map 1:a -map 2:a -c:v copy -c:a aac -b:a:0 40k -b:a:1 40k \
  -metadata:s:a:0 language=eng -metadata:s:a:1 language=swe surround.mkv
```

AV1 has no frame reordering, so its keyframes are its key packets as well:
`ffprobe -v error -select_streams v:0 -show_entries packet=pts_time,flags -of csv=p=0 av1.mkv` lists the same ten.

The expected keyframe lists come from:

```sh
ffprobe -v error -select_streams V:0 -skip_frame nokey -show_entries frame=pts_time -of csv=p=0 <file>
```

A regenerated file must reproduce those lists exactly; `src/tests.rs` asserts them.
