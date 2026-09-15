//! One ffmpeg run: a release, from one keyframe onward, remuxed into one fMP4 file per GOP.
//!
//! # Why one file per GOP
//!
//! The playlist is fixed before any segment exists, with boundaries on the file's real keyframes. With
//! the video copied, ffmpeg's hls muxer cuts at the first keyframe at least `-hls_time` past the
//! START OF THE RUN — so a run started at a seek point cuts at different keyframes than one started at
//! zero, and the playlist would describe segments no restarted run produces. `-hls_time 0` makes it cut
//! at every keyframe instead, which no start point can shift. The session joins consecutive GOP files
//! into the playlist's segments (several moof/mdat pairs in one segment is valid fMP4 HLS).
//!
//! # Why these flags
//!
//! Each was settled by running ffmpeg 9 against the fixtures (README, "Segment alignment"):
//! - `-copyts -start_at_zero`: timestamps stay on the source's timeline, so a GOP's position does not
//!   depend on where its run started.
//! - `-hls_segment_options movflags=+frag_discont+skip_sidx`, `-avoid_negative_ts disabled`: without
//!   them the mp4 muxer rebases each run to zero and records the offset in that run's `init.mp4` edit
//!   list, so GOPs from two runs cannot share one init. With them `tfdt` is absolute and every run
//!   writes a byte-identical init. `skip_sidx` also keeps a per-GOP `sidx` out of the joined segment.
//! - `-ss K+0.135 -noaccurate_seek`: see [`seek_for`].
//! - `-hls_flags temp_file`: a GOP file appears under its final name only once it is complete.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

/// How far past a keyframe to aim `-ss`. ffmpeg backs an input seek off by `3/23` s (0.1304) for
/// formats that seek by decode time when a stream has B-frames — Matroska does, MP4 does not — so an
/// exact `-ss K` lands on the keyframe BEFORE K. That is not a harmless extra GOP: a first fragment
/// with no audio in it makes the mp4 muxer rebase the audio to zero, desyncing it by K seconds.
/// Aiming 0.135 s past K lands on K whether or not ffmpeg backs off.
pub const SEEK_PAST: f64 = 0.135;
/// A keyframe with another one closer than this cannot be landed on reliably by the aim above; the
/// session restarts from an earlier one instead.
pub const MIN_SEEK_GAP: f64 = 0.15;

/// `-ss` for a run that must start exactly on the keyframe at `k`, or `None` for the start of the file.
pub fn seek_for(k: f64) -> Option<f64> {
    (k > 0.0).then_some(k + SEEK_PAST)
}

const CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
const STDERR_TAIL: usize = 2048;

/// What happens to the video.
pub enum Video {
    Copy {
        /// The sample entry to write: `hvc1` for HEVC (Safari plays HEVC in fMP4 only under it, not `hev1`), `dvh1`
        /// for Dolby Vision profile 5 kept as it is; `None` leaves the muxer's own: `avc1` for H.264, `av01` for AV1,
        /// `vp09` for VP9.
        tag: Option<&'static str>,
        dovi: Dovi,
    },
    /// Decoded, scaled (and tone-mapped, for HDR) on the GPU and encoded to H.264 there, over VAAPI — for
    /// a player that cannot take the release's HEVC. `-force_key_frames source` puts an output keyframe on
    /// every source keyframe, so the GOPs, and the playlist cut on them, are the ones a copy would make.
    Transcode {
        device: String,
        /// The output size, inside the preset's; 0 × 0 when the source's is unknown, which leaves ffmpeg to scale it
        /// to the preset's lines.
        width: u32,
        height: u32,
        tonemap: bool,
        preset: Preset,
    },
}

/// What a copy does with the video's Dolby Vision.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Dovi {
    /// The video carries none.
    Absent,
    /// Drop it — its RPU and enhancement-layer units, and its configuration record, so no `dvcC` is written —
    /// leaving the base layer: HDR10 (or SDR, HLG), which a browser plays where a profile 7 or 8 stream it would
    /// refuse is not.
    Strip,
    /// Keep it, RPU and configuration record, for a player that shows it. The mp4 muxer writes the `dvcC`/`dvvC`
    /// box only below its default strictness, hence `-strict unofficial`.
    Keep,
}

/// A transcode's H.264: High, level 4.1 — what every H.264 player takes, up to 1920 × 1080.
pub const TRANSCODE_CODECS: &str = "avc1.640029";

/// A transcode's size and rate: the picture fitted inside `width` × `height`, `bitrate` on average, `peak` at most —
/// as the master playlist's BANDWIDTH counts it and as ffmpeg is told it — over a `buffer` of rate control.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Preset {
    pub width: u32,
    pub height: u32,
    pub bitrate: u64,
    pub peak: u64,
    pub buffer: u64,
}

/// What every transcode was before a player could name its link: 1080p at 8 Mbit/s.
pub const HD1080: Preset =
    Preset { width: 1920, height: 1080, bitrate: 8_000_000, peak: 12_000_000, buffer: 16_000_000 };
/// For a link that can't carry that: 720p at 3 Mbit/s, about what streaming services give 720p.
pub const HD720: Preset =
    Preset { width: 1280, height: 720, bitrate: 3_000_000, peak: 4_500_000, buffer: 6_000_000 };

/// The most audio a session adds to its video: AAC 5.1.
const AUDIO_MAX: u64 = 384_000;

/// The preset for a player's `maxBitrate` (bits a second): 1080p where it carries 1080p with its audio, or nothing is
/// said, else 720p. Two and no more: a transcode takes the box's one GPU slot for the whole film, and below 720p at
/// 3 Mbit/s a remote player is better served by the smallest release as it is.
pub fn preset_for(max_bitrate: Option<u64>) -> Preset {
    match max_bitrate {
        Some(max) if max < HD1080.bitrate + AUDIO_MAX => HD720,
        _ => HD1080,
    }
}

/// What happens to the audio track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioOut {
    /// Copied as it is — E-AC-3, AC-3 or FLAC, for a player that plays them in fMP4 HLS.
    Copy,
    /// Re-encoded to AAC-LC stereo, which every player takes.
    Stereo,
    /// Re-encoded to AAC-LC 5.1, for a player that plays multichannel AAC, from a track of six channels or more.
    /// `-ac 6` hands the encoder 5.1 whatever the source's layout: swresample folds 7.1 down to it.
    Surround,
    /// Re-encoded to AAC-LC 7.1, for a player that plays 8-channel AAC, from a track of eight channels or more.
    Surround71,
}

impl AudioOut {
    /// The channels the session's audio carries, from the source track's.
    pub fn channels(self, source: u32) -> u32 {
        match self {
            AudioOut::Copy => source,
            AudioOut::Stereo => 2,
            AudioOut::Surround => 6,
            AudioOut::Surround71 => 8,
        }
    }
}

pub struct Spec<'a> {
    pub input: &'a str,
    pub seek: Option<f64>,
    pub video: Video,
    /// Which audio track, counting audio tracks only.
    pub audio: usize,
    pub audio_out: AudioOut,
    pub dir: &'a Path,
}

/// The ffmpeg command line. Pure, so the flags the alignment depends on are pinned by a test.
pub fn args(spec: &Spec<'_>, ca_file: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> =
        ["-nostdin", "-hide_banner", "-nostats", "-loglevel", "error"].map(String::from).to_vec();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    if spec.input.starts_with("http://") || spec.input.starts_with("https://") {
        // A debrid CDN drops a connection that has sat idle while the run was paused; reconnect
        // resumes it with a Range request at the offset ffmpeg had reached.
        a.extend(s(&["-reconnect", "1", "-reconnect_on_network_error", "1", "-reconnect_delay_max", "5"]));
        a.extend(s(&[
            "-rw_timeout",
            "30000000",
            "-user_agent",
            concat!("den-remux/", env!("CARGO_PKG_VERSION")),
        ]));
    }
    if spec.input.starts_with("https://") {
        // ffmpeg does not verify TLS peers unless asked.
        a.extend(s(&["-tls_verify", "1"]));
        if let Some(ca) = ca_file {
            a.extend(s(&["-ca_file", ca]));
        }
    }
    // One thread for the audio decoder, the filter graph and the AAC encoder: the video is copied, and
    // AAC — stereo or 5.1 — keeps one core far from busy.
    a.extend(s(&["-threads", "1", "-copyts", "-start_at_zero", "-noaccurate_seek"]));
    if let Video::Transcode { device, .. } = &spec.video {
        // Decoded frames stay on the GPU for the scaler and encoder.
        a.extend(s(&["-hwaccel", "vaapi", "-hwaccel_device", device, "-hwaccel_output_format", "vaapi"]));
    }
    if let Some(ss) = spec.seek {
        a.extend(["-ss".to_string(), format!("{ss:.6}")]);
    }
    a.extend(s(&["-i", spec.input, "-map", "0:V:0"]));
    a.push("-map".into());
    a.push(format!("0:a:{}", spec.audio));
    match &spec.video {
        Video::Copy { tag, dovi } => {
            a.extend(s(&["-c:v", "copy"]));
            if let Some(tag) = tag {
                a.extend(s(&["-tag:v", tag]));
            }
            match dovi {
                // dovi_rpu drops the configuration record (and a frame's last RPU); filter_units every Dolby
                // Vision unit — 62 the RPU, 63 the enhancement layer, several of which a profile 7 frame carries.
                Dovi::Strip => a.extend(s(&["-bsf:v", "dovi_rpu=strip=1,filter_units=remove_types=62|63"])),
                Dovi::Keep => a.extend(s(&["-strict", "unofficial"])),
                Dovi::Absent => {}
            }
        }
        Video::Transcode { width, height, tonemap, preset, .. } => {
            let size = match width {
                0 => format!("w=-2:h={}", preset.height),
                w => format!("w={w}:h={height}"),
            };
            let scale = format!("scale_vaapi={size}:format=nv12");
            let vf = match tonemap {
                true => format!("tonemap_vaapi=format=nv12:t=bt709:m=bt709:p=bt709,{scale}"),
                false => scale,
            };
            a.extend(["-vf".to_string(), vf]);
            // The output says it is SDR because `tonemap_vaapi` says so on every frame it makes. Naming the
            // colours here as well (`-colorspace bt709` and its two) instead breaks the graph — "Error
            // reinitializing filters!", and the encoder never opens.
            a.extend(s(&["-c:v", "h264_vaapi", "-profile:v", "high", "-level:v", "4.1"]));
            let [rate, peak, buffer] = [preset.bitrate, preset.peak, preset.buffer].map(|b| b.to_string());
            a.extend(s(&["-b:v", &rate, "-maxrate", &peak, "-bufsize", &buffer]));
            // Keyframes where the source has them, and no others the encoder would add on its own.
            a.extend(s(&["-force_key_frames", "source", "-g", "1000"]));
        }
    }
    match spec.audio_out {
        // No encoder priming to account for: the packets keep the source's timestamps, as the copied video's do.
        AudioOut::Copy => a.extend(s(&["-c:a", "copy"])),
        AudioOut::Stereo => a.extend(s(&["-c:a", "aac", "-ac", "2", "-b:a", "192k"])),
        // 64 kbit/s a channel, as the stereo track has.
        AudioOut::Surround => a.extend(s(&["-c:a", "aac", "-ac", "6", "-b:a", "384k"])),
        // 64 kbit/s a channel, as 5.1 has.
        AudioOut::Surround71 => a.extend(s(&["-c:a", "aac", "-ac", "8", "-b:a", "512k"])),
    }
    a.extend(s(&["-threads", "1", "-filter_threads", "1"]));
    a.extend(s(&["-max_muxing_queue_size", "1024", "-avoid_negative_ts", "disabled"]));
    a.extend(s(&["-f", "hls", "-hls_time", "0", "-hls_segment_type", "fmp4"]));
    a.extend(s(&["-hls_segment_options", "movflags=+frag_discont+skip_sidx"]));
    a.extend(s(&["-hls_fmp4_init_filename", "init.mp4", "-hls_flags", "temp_file"]));
    a.extend(s(&["-hls_playlist_type", "event", "-hls_list_size", "0", "-start_number", "0"]));
    a.push("-hls_segment_filename".into());
    a.push(spec.dir.join("g%d.m4s").to_string_lossy().into_owned());
    a.push(spec.dir.join("gops.m3u8").to_string_lossy().into_owned());
    a
}

/// The GOP files ffmpeg's own playlist lists as finished, with their durations. It rewrites that file
/// by rename after each GOP, so a read never sees half of it.
pub fn parse_gops(text: &str) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    let mut dur = None;
    for line in text.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("#EXTINF:") {
            dur = v.split(',').next().and_then(|d| d.trim().parse::<f64>().ok());
        } else if !line.is_empty() && !line.starts_with('#') {
            // Our own ffmpeg's names, but they become paths: nothing that could leave the directory.
            if let Some(d) =
                dur.take().filter(|_| line.starts_with('g') && line.ends_with(".m4s") && !line.contains('/'))
            {
                out.push((line.to_string(), d));
            }
        }
    }
    out
}

pub struct Job {
    pub id: u32,
    pub dir: PathBuf,
    /// The keyframe this run started on.
    pub start: f64,
    /// Where the next GOP starts: `start` plus every duration read so far.
    pub next_start: f64,
    /// GOPs already taken from ffmpeg's playlist.
    pub read: usize,
    pub stopped: bool,
    /// `Some(true)` once ffmpeg has exited cleanly (it reached the end), `Some(false)` on a failure.
    pub exit: Option<bool>,
    pub pid: u32,
    child: Child,
    stderr: Option<tokio::task::JoinHandle<String>>,
}

impl Job {
    pub fn spawn(ffmpeg: &str, id: u32, start: f64, spec: &Spec<'_>) -> std::io::Result<Job> {
        std::fs::create_dir_all(spec.dir)?;
        let ca = Path::new(CA_BUNDLE).exists().then_some(CA_BUNDLE);
        let mut cmd = Command::new(ffmpeg);
        cmd.args(args(spec, ca))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own process group, so pausing and killing reach everything it runs.
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn()?;
        let pid = child.id().unwrap_or(0);
        register_group(pid);
        // Drained concurrently so a chatty failure cannot block ffmpeg on a full pipe; only the tail
        // is kept, for the one line logged if the run fails.
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
        Ok(Job {
            id,
            dir: spec.dir.to_path_buf(),
            start,
            next_start: start,
            read: 0,
            stopped: false,
            exit: None,
            pid,
            child,
            stderr,
        })
    }

    /// Has ffmpeg exited? Reaps it if so.
    pub fn poll_exit(&mut self) -> Option<bool> {
        if self.exit.is_none() {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exit = Some(status.success());
                unregister_group(self.pid);
            }
        }
        self.exit
    }

    /// Stop ffmpeg where it is: no CPU, no reads. The window ahead of the player is full.
    pub fn pause(&mut self) {
        if self.exit.is_none() && !self.stopped {
            signal_group(self.pid, libc::SIGSTOP);
            self.stopped = true;
        }
    }

    pub fn resume(&mut self) {
        if self.exit.is_none() && self.stopped {
            signal_group(self.pid, libc::SIGCONT);
            self.stopped = false;
        }
    }

    /// The tail of ffmpeg's stderr, once it has exited.
    pub fn take_stderr(&mut self) -> Option<tokio::task::JoinHandle<String>> {
        self.stderr.take()
    }

    /// Kill the run, reap it, and drop the partial GOP it was writing.
    pub async fn stop(mut self) {
        if self.exit.is_none() {
            // A stopped process dies of SIGKILL as readily as a running one.
            kill_group(self.pid);
            let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
            self.exit = Some(false);
            unregister_group(self.pid);
        }
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().ends_with(".tmp") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // Only while it has not been reaped: once it has, the pid can belong to someone else.
        if self.exit.is_none() {
            kill_group(self.pid);
            unregister_group(self.pid);
        }
    }
}

/// Every live ffmpeg's process group, so shutdown can kill them without depending on drop order —
/// den-reel's registry, for the same reason.
static LIVE_GROUPS: std::sync::Mutex<Option<std::collections::HashSet<u32>>> = std::sync::Mutex::new(None);

fn register_group(pgid: u32) {
    if pgid > 1 && pgid <= i32::MAX as u32 {
        LIVE_GROUPS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(Default::default)
            .insert(pgid);
    }
}

fn unregister_group(pgid: u32) {
    if let Some(set) = LIVE_GROUPS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        set.remove(&pgid);
    }
}

/// SIGKILL every ffmpeg still running. Returns how many groups it signalled.
pub fn kill_live_groups() -> usize {
    let taken = LIVE_GROUPS.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
    for p in &taken {
        kill_group(*p);
    }
    taken.len()
}

pub fn live_groups() -> usize {
    LIVE_GROUPS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(0, |s| s.len())
}

fn kill_group(pgid: u32) {
    signal_group(pgid, libc::SIGKILL);
}

fn signal_group(pgid: u32, sig: libc::c_int) {
    // 0 is our own group and 1 would make kill(-1) "every process we may signal"; above i32::MAX the
    // negation is a single unrelated pid. None of them is reachable from child.id().
    if pgid > 1 && pgid <= i32::MAX as u32 {
        unsafe { libc::kill(-(pgid as i32), sig) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_alignment_flags_are_all_there() {
        let dir = Path::new("/scratch/s-x/j1");
        let spec = Spec {
            input: "https://cdn.example/f.mkv",
            seek: seek_for(13.0),
            video: Video::Copy { tag: Some("hvc1"), dovi: Dovi::Absent },
            audio: 1,
            audio_out: AudioOut::Stereo,
            dir,
        };
        let a = args(&spec, Some(CA_BUNDLE));
        let joined = a.join(" ");
        assert!(!joined.contains("-bsf"), "{joined}");
        let dovi = Spec { video: Video::Copy { tag: Some("hvc1"), dovi: Dovi::Strip }, ..spec };
        let d = args(&dovi, None).join(" ");
        assert!(
            d.contains("-tag:v hvc1 -bsf:v dovi_rpu=strip=1,filter_units=remove_types=62|63 "),
            "the base layer alone, with no dvcC"
        );
        for want in [
            "-copyts -start_at_zero -noaccurate_seek -ss 13.135000 -i https://cdn.example/f.mkv",
            "-map 0:V:0 -map 0:a:1 -c:v copy -tag:v hvc1 -c:a aac -ac 2 -b:a 192k",
            "-avoid_negative_ts disabled",
            "-f hls -hls_time 0 -hls_segment_type fmp4",
            "-hls_segment_options movflags=+frag_discont+skip_sidx",
            "-hls_flags temp_file",
            "-tls_verify 1 -ca_file /etc/ssl/certs/ca-certificates.crt",
            "-hls_segment_filename /scratch/s-x/j1/g%d.m4s /scratch/s-x/j1/gops.m3u8",
        ] {
            assert!(joined.contains(want), "missing `{want}` in: {joined}");
        }
        assert!(a.iter().position(|x| x == "-ss") < a.iter().position(|x| x == "-i"), "-ss is an input seek");
    }

    #[test]
    fn a_transcode_stays_on_the_gpu_and_keeps_the_source_keyframes() {
        let spec = Spec {
            input: "/f.mkv",
            seek: seek_for(13.0),
            video: Video::Transcode {
                device: "/dev/dri/renderD128".into(),
                width: 1920,
                height: 800,
                tonemap: true,
                preset: HD1080,
            },
            audio: 0,
            audio_out: AudioOut::Stereo,
            dir: Path::new("/d"),
        };
        let a = args(&spec, None);
        let joined = a.join(" ");
        for want in [
            "-hwaccel vaapi -hwaccel_device /dev/dri/renderD128 -hwaccel_output_format vaapi -ss 13.135000 -i /f.mkv",
            "-vf tonemap_vaapi=format=nv12:t=bt709:m=bt709:p=bt709,scale_vaapi=w=1920:h=800:format=nv12",
            "-c:v h264_vaapi -profile:v high -level:v 4.1 -b:v 8000000 -maxrate 12000000 -bufsize 16000000",
            "-force_key_frames source",
            "-f hls -hls_time 0",
        ] {
            assert!(joined.contains(want), "missing `{want}` in: {joined}");
        }
        assert!(!joined.contains("-tag:v") && !joined.contains("-c:v copy"));
        let sdr = Spec {
            video: Video::Transcode {
                device: "/d".into(),
                width: 1280,
                height: 720,
                tonemap: false,
                preset: HD720,
            },
            ..spec
        };
        let sdr_args = args(&sdr, None).join(" ");
        assert!(sdr_args.contains("-vf scale_vaapi=w=1280:h=720:format=nv12 "));
        assert!(sdr_args.contains("-b:v 3000000 -maxrate 4500000 -bufsize 6000000"), "{sdr_args}");
        let unknown = Spec {
            video: Video::Transcode {
                device: "/d".into(),
                width: 0,
                height: 0,
                tonemap: false,
                preset: HD1080,
            },
            ..sdr
        };
        assert!(args(&unknown, None).join(" ").contains("-vf scale_vaapi=w=-2:h=1080:format=nv12 "));
    }

    #[test]
    fn a_link_below_1080p_gets_the_720p_transcode() {
        assert_eq!(preset_for(None), HD1080, "nothing said: as before");
        assert_eq!(preset_for(Some(50_000_000)), HD1080);
        assert_eq!(preset_for(Some(8_384_000)), HD1080, "8 Mbit/s of video and 5.1 AAC");
        assert_eq!(preset_for(Some(8_383_999)), HD720);
        assert_eq!(preset_for(Some(2_000_000)), HD720, "nothing smaller: 720p is the floor");
    }

    #[test]
    fn the_start_of_the_file_is_not_a_seek_and_file_inputs_get_no_http_flags() {
        assert_eq!(seek_for(0.0), None);
        let spec = Spec {
            input: "/tmp/f.mkv",
            seek: None,
            video: Video::Copy { tag: None, dovi: Dovi::Absent },
            audio: 0,
            audio_out: AudioOut::Stereo,
            dir: Path::new("/d"),
        };
        let a = args(&spec, None);
        assert!(!a.iter().any(|x| x == "-ss" || x == "-reconnect" || x == "-tls_verify" || x == "-tag:v"));
    }

    #[test]
    fn kept_dolby_vision_keeps_its_configuration_record() {
        let spec = Spec {
            input: "/f.mkv",
            seek: None,
            video: Video::Copy { tag: Some("dvh1"), dovi: Dovi::Keep },
            audio: 0,
            audio_out: AudioOut::Stereo,
            dir: Path::new("/d"),
        };
        let joined = args(&spec, None).join(" ");
        assert!(joined.contains("-c:v copy -tag:v dvh1 -strict unofficial -c:a aac"), "{joined}");
        assert!(!joined.contains("-bsf"), "the RPU stays: {joined}");
    }

    #[test]
    fn dolby_audio_for_a_player_that_plays_it_is_copied() {
        let spec = Spec {
            input: "/f.mkv",
            seek: seek_for(13.0),
            video: Video::Copy { tag: Some("hvc1"), dovi: Dovi::Absent },
            audio: 2,
            audio_out: AudioOut::Copy,
            dir: Path::new("/d"),
        };
        let joined = args(&spec, None).join(" ");
        assert!(joined.contains("-map 0:a:2 -c:v copy -tag:v hvc1 -c:a copy -threads 1"), "{joined}");
        assert!(!joined.contains("aac") && !joined.contains("-ac 2"), "{joined}");
        let alignment = "-copyts -start_at_zero -noaccurate_seek -ss 13.135000";
        assert!(joined.contains(alignment), "the same alignment: {joined}");
    }

    #[test]
    fn surround_for_a_player_that_plays_it_is_aac_5_1() {
        let spec = Spec {
            input: "/f.mkv",
            seek: seek_for(13.0),
            video: Video::Copy { tag: Some("hvc1"), dovi: Dovi::Absent },
            audio: 1,
            audio_out: AudioOut::Surround,
            dir: Path::new("/d"),
        };
        let joined = args(&spec, None).join(" ");
        assert!(
            joined.contains("-map 0:a:1 -c:v copy -tag:v hvc1 -c:a aac -ac 6 -b:a 384k -threads 1"),
            "{joined}"
        );
        let alignment = "-copyts -start_at_zero -noaccurate_seek -ss 13.135000";
        assert!(joined.contains(alignment), "the same alignment: {joined}");
        assert_eq!(
            [AudioOut::Copy, AudioOut::Stereo, AudioOut::Surround, AudioOut::Surround71]
                .map(|o| o.channels(8)),
            [8, 2, 6, 8],
            "7.1 copied or kept 7.1 keeps its eight, converted otherwise comes down to two or six"
        );
    }

    #[test]
    fn seven_one_for_a_player_that_plays_it_is_aac_7_1() {
        let spec = Spec {
            input: "/f.mkv",
            seek: seek_for(13.0),
            video: Video::Copy { tag: None, dovi: Dovi::Absent },
            audio: 1,
            audio_out: AudioOut::Surround71,
            dir: Path::new("/d"),
        };
        let joined = args(&spec, None).join(" ");
        assert!(joined.contains("-map 0:a:1 -c:v copy -c:a aac -ac 8 -b:a 512k -threads 1"), "{joined}");
    }

    #[test]
    fn ffmpegs_playlist_yields_finished_gops_only() {
        let text = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"init.mp4\"\n\
                    #EXTINF:2.500000,\ng0.m4s\n#EXTINF:3.000000,\ng1.m4s\n#EXTINF:1.0,\n../escape.m4s\n";
        assert_eq!(parse_gops(text), vec![("g0.m4s".to_string(), 2.5), ("g1.m4s".to_string(), 3.0)]);
        assert!(parse_gops("").is_empty());
    }
}
