//! Nonblocking screen recorder. Discovery and file validation run off the WM thread.
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    Arc, Mutex,
    mpsc::{self, Receiver, TryRecvError},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt, StackMode};
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;

use crate::canvas::{Canvas, Color};
use crate::files::home_dir;
use crate::procutil::{apply_pulse_env_defaults, command_exists};
use crate::{AnyResult, Aurora};

pub(crate) struct RecordingPlan {
    path: PathBuf,
    monitor: String,
    microphone: String,
    reserved: bool,
    encoder: VideoEncoder,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum VideoEncoder {
    Vaapi(PathBuf),
    Nvenc,
    Qsv,
    Software,
}

impl VideoEncoder {
    fn name(&self) -> &'static str {
        match self {
            Self::Vaapi(_) => "h264_vaapi",
            Self::Nvenc => "h264_nvenc",
            Self::Qsv => "h264_qsv",
            Self::Software => "libx264",
        }
    }
}

struct RecordingRetry {
    plan: RecordingPlan,
    display: String,
    width: u16,
    height: u16,
}

impl Drop for RecordingPlan {
    fn drop(&mut self) {
        if self.reserved {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) enum RecordingState {
    Preparing {
        result: Receiver<Result<RecordingPlan, String>>,
    },
    Countdown {
        plan: RecordingPlan,
        started: Instant,
        shown: u8,
    },
    Running(RecordingProcess),
    Stopping {
        process: RecordingProcess,
        requested: Instant,
        interrupted: bool,
    },
    Finalizing {
        result: Receiver<Result<PathBuf, String>>,
    },
}

impl RecordingState {
    pub(crate) fn is_recording(&self) -> bool {
        matches!(self, Self::Running(_) | Self::Stopping { .. })
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Preparing { .. } => "Preparing recording",
            Self::Countdown { .. } => "Cancel recording",
            Self::Running(_) => "Stop recording",
            Self::Stopping { .. } | Self::Finalizing { .. } => "Saving recording",
        }
    }

    pub(crate) fn next_poll_delay(&self) -> Duration {
        Duration::from_millis(if matches!(self, Self::Running(_)) {
            250
        } else {
            50
        })
    }
}

pub(crate) struct RecordingProcess {
    child: Option<Child>,
    path: PathBuf,
    errors: Arc<Mutex<VecDeque<u8>>>,
    started: Instant,
    retry: Option<RecordingRetry>,
}

impl RecordingProcess {
    fn stop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            if let Some(mut stdin) = child.stdin.take() {
                // FFmpeg's interactive quit writes the MP4 trailer before exiting.
                let _ = stdin.write_all(b"q\n");
            }
        }
    }

    fn interrupt(&self) {
        if let Some(child) = self.child.as_ref() {
            // Target only our recorder, never other applications or the X server.
            unsafe {
                libc::kill(child.id() as libc::pid_t, libc::SIGINT);
            }
        }
    }

    fn error_text(&self) -> String {
        self.errors
            .lock()
            .ok()
            .map(|bytes| {
                String::from_utf8_lossy(&bytes.iter().copied().collect::<Vec<_>>())
                    .trim()
                    .to_string()
            })
            .unwrap_or_default()
    }
}

impl Drop for RecordingProcess {
    fn drop(&mut self) {
        // A WM restart must not leave an invisible recorder capturing indefinitely.
        self.stop();
        self.interrupt();
        if let Some(mut child) = self.child.take() {
            thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => return,
                        Err(_) => break,
                        Ok(None) if Instant::now() >= deadline => break,
                        Ok(None) => thread::sleep(Duration::from_millis(100)),
                    }
                }
                unsafe {
                    libc::kill(child.id() as libc::pid_t, libc::SIGINT);
                }
                let _ = child.wait();
            });
        }
    }
}

fn pactl(args: &[&str]) -> Result<String, String> {
    // Bound discovery even when the external timeout utility is absent.
    let mut command = Command::new("pactl");
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_pulse_env_defaults(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("Cannot access audio: {error}"))?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let output = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let errors = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break Err("Audio discovery timed out; check your audio service".to_string());
            }
        }
    };
    let stdout = output.join().unwrap_or_default();
    let stderr = errors.join().unwrap_or_default();
    if !status?.success() {
        return Err(format!(
            "Cannot access system audio and microphone: {}",
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&stdout).trim().to_string())
}

fn select_audio_sources(
    sources: &str,
    sink: &str,
    default_source: &str,
) -> Result<(String, String), String> {
    let names: Vec<&str> = sources
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .collect();
    let monitor = format!("{sink}.monitor");
    if !names.iter().any(|name| *name == monitor) {
        return Err("The current audio output has no recording monitor".into());
    }
    let microphone = names
        .iter()
        .copied()
        .find(|name| *name == default_source && !name.ends_with(".monitor"))
        .or_else(|| {
            names
                .iter()
                .copied()
                .find(|name| !name.ends_with(".monitor"))
        })
        .ok_or_else(|| {
            "No microphone is available; connect a microphone and try again".to_string()
        })?;
    Ok((monitor, microphone.to_string()))
}

fn append_encoder_args(command: &mut Command, encoder: &VideoEncoder, width: u16, height: u16) {
    command.args([
        "-c:v",
        encoder.name(),
        "-r",
        "24",
        "-b:v",
        "3M",
        "-maxrate",
        "3M",
        "-bufsize",
        "6M",
        "-g",
        "48",
    ]);
    match encoder {
        VideoEncoder::Vaapi(_) => {
            command.args(["-vf", "format=nv12,hwupload", "-bf", "0"]);
        }
        VideoEncoder::Nvenc => {
            command.args([
                "-preset",
                "p1",
                "-tune",
                "ll",
                "-rc",
                "vbr",
                "-bf",
                "0",
                "-pix_fmt",
                if width % 2 == 0 && height % 2 == 0 {
                    "yuv420p"
                } else {
                    "yuv444p"
                },
            ]);
        }
        VideoEncoder::Qsv => {
            command.args(["-preset", "veryfast", "-bf", "0", "-pix_fmt", "nv12"]);
        }
        VideoEncoder::Software => {
            command.args([
                "-preset",
                "ultrafast",
                "-threads",
                "2",
                "-pix_fmt",
                if width % 2 == 0 && height % 2 == 0 {
                    "yuv420p"
                } else {
                    "yuv444p"
                },
            ]);
        }
    }
}

fn append_hardware_device(command: &mut Command, encoder: &VideoEncoder) {
    if let VideoEncoder::Vaapi(device) = encoder {
        command.arg("-vaapi_device").arg(device);
    }
}

fn encoder_probe_command(encoder: &VideoEncoder, width: u16, height: u16) -> Command {
    let mut command = Command::new("ffmpeg");
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-filter_threads",
        "1",
        "-filter_complex_threads",
        "1",
    ]);
    append_hardware_device(&mut command, encoder);
    // Test actual native dimensions and RGB conversion, not just advertised codecs.
    command
        .args(["-f", "lavfi", "-i"])
        .arg(format!("testsrc=size={width}x{height}:rate=24,format=bgr0"))
        .args(["-frames:v", "2"]);
    append_encoder_args(&mut command, encoder, width, height);
    command
        .args(["-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn encoder_works(encoder: &VideoEncoder, width: u16, height: u16) -> bool {
    let Ok(mut child) = encoder_probe_command(encoder, width, height).spawn() else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn choose_video_encoder(width: u16, height: u16) -> VideoEncoder {
    let even = width % 2 == 0 && height % 2 == 0;
    if even {
        let mut devices = fs::read_dir("/dev/dri")
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("renderD"))
            })
            .collect::<Vec<_>>();
        devices.sort();
        for device in devices.into_iter().take(4) {
            let candidate = VideoEncoder::Vaapi(device);
            if encoder_works(&candidate, width, height) {
                return candidate;
            }
        }
    }
    if encoder_works(&VideoEncoder::Nvenc, width, height) {
        return VideoEncoder::Nvenc;
    }
    if even && encoder_works(&VideoEncoder::Qsv, width, height) {
        return VideoEncoder::Qsv;
    }
    VideoEncoder::Software
}

fn prepare_recording(width: u16, height: u16) -> Result<RecordingPlan, String> {
    if !command_exists("ffmpeg") {
        return Err("Install FFmpeg to record your screen".into());
    }
    if !command_exists("ffprobe") {
        return Err(
            "Screen recording requires ffprobe; install the complete FFmpeg package".into(),
        );
    }
    if !command_exists("pactl") {
        return Err("Screen recording requires pactl for system audio and microphone".into());
    }
    let sink = pactl(&["get-default-sink"])?;
    let microphone = pactl(&["get-default-source"])?;
    let sources = pactl(&["list", "sources", "short"])?;
    let (monitor, microphone) = select_audio_sources(&sources, &sink, &microphone)?;
    let encoder = choose_video_encoder(width, height);
    eprintln!(
        "Screen recording: selected {} at {width}x{height}",
        encoder.name()
    );
    let directory = home_dir().join("Desktop/screenrecording");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("Cannot create recording folder: {error}"))?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    for suffix in 0..1000 {
        let path = directory.join(format!("screenrecording-{timestamp}-{suffix}.mp4"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => {
                return Ok(RecordingPlan {
                    path,
                    monitor,
                    microphone,
                    reserved: true,
                    encoder,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("Cannot save recording: {error}")),
        }
    }
    Err("Cannot reserve a unique recording filename".into())
}

fn recorder_command(plan: &RecordingPlan, display: &str, width: u16, height: u16) -> Command {
    let mut command = Command::new("ffmpeg");
    append_hardware_device(&mut command, &plan.encoder);
    command
        .args([
            "-hide_banner",
            "-loglevel",
            "warning",
            "-filter_threads",
            "1",
            "-filter_complex_threads",
            "1",
            "-y",
            "-thread_queue_size",
            "512",
            "-f",
            "x11grab",
            "-framerate",
            "24",
            "-video_size",
        ])
        .arg(format!("{width}x{height}"))
        .args(["-draw_mouse", "1", "-i"])
        .arg(format!("{display}+0,0"));
    for source in [&plan.monitor, &plan.microphone] {
        command
            .args([
                "-thread_queue_size",
                "512",
                "-f",
                "pulse",
                "-sample_rate",
                "48000",
                "-channels",
                "2",
                "-i",
            ])
            .arg(source);
    }
    command.args([
        "-filter_complex", "[1:a]aresample=async=1:first_pts=0[system];[2:a]aresample=async=1:first_pts=0[mic];[system][mic]amix=inputs=2:duration=longest:dropout_transition=0:normalize=1[audio]",
        "-map", "0:v:0", "-map", "[audio]",
    ]);
    append_encoder_args(&mut command, &plan.encoder, width, height);
    command
        .args([
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-movflags",
            "+faststart",
        ])
        .arg(&plan.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    apply_pulse_env_defaults(&mut command);
    // process::exit and external WM termination skip Rust destructors. Linux
    // delivers SIGINT to FFmpeg so it can finalize the MP4 even in those cases.
    let parent = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGINT) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Close the race if the WM exited between fork and prctl.
            if libc::getppid() != parent {
                libc::raise(libc::SIGINT);
            }
            Ok(())
        });
    }
    command
}

fn start_recording(
    mut plan: RecordingPlan,
    display: &str,
    width: u16,
    height: u16,
) -> Result<RecordingProcess, String> {
    let mut child = match recorder_command(&plan, display, width, height).spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = fs::remove_file(&plan.path);
            return Err(format!("Cannot start screen recording: {error}"));
        }
    };
    let errors = Arc::new(Mutex::new(VecDeque::new()));
    if let Some(mut stderr) = child.stderr.take() {
        let errors = errors.clone();
        thread::spawn(move || {
            let mut buffer = [0; 1024];
            while let Ok(length) = stderr.read(&mut buffer) {
                if length == 0 {
                    break;
                }
                if let Ok(mut bytes) = errors.lock() {
                    bytes.extend(&buffer[..length]);
                    while bytes.len() > 8192 {
                        bytes.pop_front();
                    }
                }
            }
        });
    }
    plan.reserved = false;
    let retry = (plan.encoder != VideoEncoder::Software).then(|| RecordingRetry {
        plan: RecordingPlan {
            path: plan.path.clone(),
            monitor: plan.monitor.clone(),
            microphone: plan.microphone.clone(),
            reserved: false,
            encoder: VideoEncoder::Software,
        },
        display: display.to_string(),
        width,
        height,
    });
    Ok(RecordingProcess {
        child: Some(child),
        path: plan.path.clone(),
        errors,
        started: Instant::now(),
        retry,
    })
}

fn validate_recording(path: &Path) -> Result<(), String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("Cannot check saved recording: {error}"))?;
    let streams = String::from_utf8_lossy(&output.stdout);
    if !output.status.success()
        || !streams.lines().any(|line| line.trim() == "video")
        || !streams.lines().any(|line| line.trim() == "audio")
    {
        return Err(format!(
            "Recording did not produce a complete video with audio: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

impl Aurora {
    pub(crate) fn shutdown_screen_recording(&mut self) {
        // Drop requests graceful quit and SIGINT synchronously; waiting is async.
        self.recording.take();
    }

    fn recording_error(&mut self, message: String) {
        eprintln!("Screen recording: {message}");
        let compact = message.split_whitespace().collect::<Vec<_>>().join(" ");
        let notice = if compact.chars().count() > 90 {
            format!("{}…", compact.chars().take(87).collect::<String>())
        } else {
            compact
        };
        self.topbar_notice = Some((notice, Instant::now() + Duration::from_secs(10)));
    }

    pub(crate) fn toggle_screen_recording(&mut self) -> AnyResult<()> {
        self.recording_error_notice = None;
        match self.recording.take() {
            None if !command_exists("ffmpeg") => {
                self.recording_error_notice = Some((
                    "Install FFmpeg to record your screen".into(),
                    Instant::now() + Duration::from_secs(10),
                ));
            }
            None => {
                let (width, height) = (self.screen_width, self.screen_height);
                let (tx, result) = mpsc::channel();
                thread::spawn(move || {
                    let prepared = prepare_recording(width, height);
                    if let Err(unsent) = tx.send(prepared) {
                        if let Ok(plan) = unsent.0 {
                            drop(plan);
                        }
                    }
                });
                self.recording = Some(RecordingState::Preparing { result });
            }
            Some(RecordingState::Preparing { result }) => {
                // A queued successful preparation owns an empty reservation.
                if let Ok(Ok(plan)) = result.try_recv() {
                    drop(plan);
                }
            }
            Some(RecordingState::Countdown { plan, .. }) => {
                drop(plan);
            }
            Some(RecordingState::Running(mut process)) => {
                process.stop();
                self.recording = Some(RecordingState::Stopping {
                    process,
                    requested: Instant::now(),
                    interrupted: false,
                });
            }
            Some(other) => {
                self.recording = Some(other);
            }
        }
        self.redraw_recording_notice()?;
        self.redraw_topbar()?;
        self.conn.flush()?;
        Ok(())
    }

    pub(crate) fn poll_screen_recording(&mut self) -> AnyResult<bool> {
        let expired = self
            .recording_error_notice
            .as_ref()
            .is_some_and(|(_, until)| Instant::now() >= *until);
        if expired {
            self.recording_error_notice = None;
            self.redraw_recording_notice()?;
        }
        let Some(state) = self.recording.take() else {
            return Ok(expired);
        };
        let mut changed = false;
        self.recording = match state {
            RecordingState::Preparing { result } => match result.try_recv() {
                Ok(Ok(plan)) => {
                    changed = true;
                    Some(RecordingState::Countdown {
                        plan,
                        started: Instant::now(),
                        shown: 3,
                    })
                }
                Ok(Err(error)) => {
                    changed = true;
                    self.recording_error(error);
                    None
                }
                Err(TryRecvError::Empty) => Some(RecordingState::Preparing { result }),
                Err(TryRecvError::Disconnected) => {
                    changed = true;
                    self.recording_error("Recording preparation failed".into());
                    None
                }
            },
            RecordingState::Countdown {
                plan,
                started,
                shown,
            } => {
                let elapsed = started.elapsed().as_secs();
                if elapsed >= 3 {
                    // Synchronize unmapping so the notice never appears in capture frames.
                    self.conn.unmap_window(self.ui.recording_notice)?;
                    self.conn.sync()?;
                    changed = true;
                    match start_recording(
                        plan,
                        &self.display,
                        self.screen_width,
                        self.screen_height,
                    ) {
                        Ok(process) => Some(RecordingState::Running(process)),
                        Err(error) => {
                            self.recording_error(error);
                            None
                        }
                    }
                } else {
                    let count = 3 - elapsed as u8;
                    changed = count != shown;
                    Some(RecordingState::Countdown {
                        plan,
                        started,
                        shown: count,
                    })
                }
            }
            RecordingState::Running(mut process) => {
                match process.child.as_mut().unwrap().try_wait() {
                    Ok(Some(status)) => {
                        changed = true;
                        self.finish_recording(process, status, false)
                    }
                    Ok(None) => Some(RecordingState::Running(process)),
                    Err(error) => {
                        changed = true;
                        self.recording_error(format!("Cannot check recorder: {error}"));
                        None
                    }
                }
            }
            RecordingState::Stopping {
                mut process,
                requested,
                mut interrupted,
            } => match process.child.as_mut().unwrap().try_wait() {
                Ok(Some(status)) => {
                    changed = true;
                    self.finish_recording(process, status, true)
                }
                Ok(None) => {
                    if !interrupted && requested.elapsed() >= Duration::from_secs(4) {
                        process.interrupt();
                        interrupted = true;
                    }
                    Some(RecordingState::Stopping {
                        process,
                        requested,
                        interrupted,
                    })
                }
                Err(error) => {
                    changed = true;
                    self.recording_error(format!("Cannot finalize recording: {error}"));
                    None
                }
            },
            RecordingState::Finalizing { result } => match result.try_recv() {
                Ok(Ok(path)) => {
                    changed = true;
                    if let Some(parent) = path.parent() {
                        self.open_file_manager_tab(parent);
                    }
                    self.topbar_notice = Some((
                        format!(
                            "Saved {}",
                            path.file_name().unwrap_or_default().to_string_lossy()
                        ),
                        Instant::now() + Duration::from_secs(6),
                    ));
                    None
                }
                Ok(Err(error)) => {
                    changed = true;
                    self.recording_error(error);
                    None
                }
                Err(TryRecvError::Empty) => Some(RecordingState::Finalizing { result }),
                Err(TryRecvError::Disconnected) => {
                    changed = true;
                    self.recording_error("Recording validation failed".into());
                    None
                }
            },
        };
        if changed {
            self.redraw_recording_notice()?;
            self.redraw_topbar()?;
        }
        Ok(changed || expired)
    }

    fn finish_recording(
        &mut self,
        mut process: RecordingProcess,
        status: ExitStatus,
        requested: bool,
    ) -> Option<RecordingState> {
        // The child has already been reaped. Prevent its Drop from sending signals.
        process.child.take();
        let path = process.path.clone();
        if !status.success() && !(requested && status.code() == Some(255)) {
            let errors = process.error_text();
            if !requested && process.started.elapsed() < Duration::from_secs(4) {
                if let Some(retry) = process.retry.take() {
                    eprintln!(
                        "Screen recording: GPU startup failed; retrying software encoder: {errors}"
                    );
                    match start_recording(retry.plan, &retry.display, retry.width, retry.height) {
                        Ok(restarted) => {
                            self.topbar_notice = Some((
                                "GPU unavailable; recording with CPU".into(),
                                Instant::now() + Duration::from_secs(6),
                            ));
                            return Some(RecordingState::Running(restarted));
                        }
                        Err(error) => {
                            self.recording_error(error);
                            return None;
                        }
                    }
                }
            }
            self.recording_error(format!("Recording failed ({status}): {errors}"));
            if fs::metadata(&path).is_ok_and(|metadata| metadata.len() == 0) {
                let _ = fs::remove_file(path);
            }
            return None;
        }
        let (tx, result) = mpsc::channel();
        thread::spawn(move || {
            let completed = validate_recording(&path).map(|()| path);
            let _ = tx.send(completed);
        });
        Some(RecordingState::Finalizing { result })
    }

    pub(crate) fn redraw_recording_notice(&self) -> AnyResult<()> {
        if let Some((message, _)) = self
            .recording_error_notice
            .as_ref()
            .filter(|(_, until)| Instant::now() < *until)
        {
            let width = self.screen_width.min(330);
            let height = 52u16;
            let x = (self.topbar_controls().recording_x - i32::from(width) / 2)
                .clamp(0, i32::from(self.screen_width.saturating_sub(width)));
            self.conn.configure_window(
                self.ui.recording_notice,
                &ConfigureWindowAux::new()
                    .x(x)
                    .y(i32::from(crate::TOPBAR_HEIGHT) + 6)
                    .width(u32::from(width))
                    .height(u32::from(height))
                    .stack_mode(StackMode::ABOVE),
            )?;
            let mut canvas = Canvas::new(width, height, Color::rgb(247, 252, 255));
            canvas.draw_rect(0, 0, i32::from(width), 3, Color::rgb(216, 60, 72));
            canvas.draw_text_center(
                &self.regular,
                message,
                i32::from(width / 2),
                18,
                14.0,
                Color::rgb(25, 42, 51),
            );
            self.conn.map_window(self.ui.recording_notice)?;
            return self.upload_canvas(self.ui.recording_notice, &canvas);
        }
        let title = match self.recording.as_ref() {
            Some(RecordingState::Preparing { .. }) => "Preparing screen recording".to_string(),
            Some(RecordingState::Countdown { shown, .. }) => format!("Start recording in {shown}"),
            _ => {
                self.conn.unmap_window(self.ui.recording_notice)?;
                return Ok(());
            }
        };
        let width = self.screen_width.min(460);
        let height = self.screen_height.min(130);
        self.conn.configure_window(
            self.ui.recording_notice,
            &ConfigureWindowAux::new()
                .x(i32::from(self.screen_width.saturating_sub(width) / 2))
                .y(i32::from(self.screen_height.saturating_sub(height) / 2))
                .width(u32::from(width))
                .height(u32::from(height))
                .stack_mode(StackMode::ABOVE),
        )?;
        let mut canvas = Canvas::new(width, height, Color::rgb(247, 252, 255));
        canvas.draw_rect(0, 0, i32::from(width), 3, Color::rgb(216, 60, 72));
        canvas.draw_text_center(
            &self.bold,
            &title,
            i32::from(width / 2),
            32,
            25.0,
            Color::rgb(25, 42, 51),
        );
        canvas.draw_text_center(
            &self.regular,
            "System audio + microphone",
            i32::from(width / 2),
            74,
            15.0,
            Color::rgb(77, 99, 111),
        );
        canvas.draw_text_center(
            &self.regular,
            "Click the recording icon again to cancel",
            i32::from(width / 2),
            98,
            12.0,
            Color::rgb(77, 99, 111),
        );
        self.conn.map_window(self.ui.recording_notice)?;
        self.upload_canvas(self.ui.recording_notice, &canvas)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_sources_prefer_microphone_and_require_both_inputs() {
        let sources = "1 speakers.monitor PipeWire s16le 2ch 48000Hz RUNNING\n2 microphone PipeWire s16le 2ch 48000Hz RUNNING\n3 usb_microphone PipeWire s16le 2ch 48000Hz RUNNING";
        assert_eq!(
            select_audio_sources(sources, "speakers", "usb_microphone").unwrap(),
            ("speakers.monitor".into(), "usb_microphone".into())
        );
        assert_eq!(
            select_audio_sources(sources, "speakers", "speakers.monitor")
                .unwrap()
                .1,
            "microphone"
        );
        assert!(
            select_audio_sources(
                "1 speakers.monitor PipeWire",
                "speakers",
                "speakers.monitor"
            )
            .is_err()
        );
        assert!(select_audio_sources(sources, "missing", "microphone").is_err());
    }

    #[test]
    fn command_preserves_native_dimensions_and_both_audio_sources() {
        let plan = RecordingPlan {
            path: PathBuf::from("/tmp/test recording.mp4"),
            monitor: "speakers.monitor".into(),
            microphone: "microphone".into(),
            reserved: false,
            encoder: VideoEncoder::Software,
        };
        let command = recorder_command(&plan, ":11", 1921, 1081);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-video_size", "1921x1081"])
        );
        assert!(args.windows(2).any(|pair| pair == ["-framerate", "24"]));
        assert!(args.windows(2).any(|pair| pair == ["-b:v", "3M"]));
        assert!(args.windows(2).any(|pair| pair == ["-pix_fmt", "yuv444p"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-i", "speakers.monitor"])
        );
        assert!(args.windows(2).any(|pair| pair == ["-i", "microphone"]));
        assert_eq!(args.last().unwrap(), "/tmp/test recording.mp4");
    }
    #[test]
    fn hardware_probe_matches_capture_geometry_and_encode_settings() {
        let encoder = VideoEncoder::Vaapi(PathBuf::from("/dev/dri/renderD128"));
        let probe = encoder_probe_command(&encoder, 1920, 1080);
        let args = probe
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-i", "testsrc=size=1920x1080:rate=24,format=bgr0"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-vaapi_device", "/dev/dri/renderD128"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-vf", "format=nv12,hwupload"])
        );
        assert!(args.windows(2).any(|pair| pair == ["-c:v", "h264_vaapi"]));
        assert!(args.windows(2).any(|pair| pair == ["-b:v", "3M"]));
        assert!(args.windows(2).any(|pair| pair == ["-frames:v", "2"]));
    }
}
