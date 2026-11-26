use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::CString;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::io::Read;
use std::os::fd::RawFd;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use image::imageops::FilterType;
use rusttype::{Font, Scale, point};
use time::OffsetDateTime;
use x11rb::CURRENT_TIME;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::ReplyError;
use x11rb::image::{BitsPerPixel, Image, ImageOrder as XrbImageOrder, ScanlinePad};
use x11rb::protocol::composite::{self, ConnectionExt as CompositeConnectionExt};
use x11rb::protocol::screensaver::ConnectionExt as ScreenSaverConnectionExt;
use x11rb::protocol::shape::{self, ConnectionExt as ShapeConnectionExt};
use x11rb::protocol::xfixes::{self, ConnectionExt as XFixesConnectionExt};
use x11rb::protocol::xproto::ConnectionExt as XprotoConnectionExt;
use x11rb::protocol::xproto::*;
use x11rb::protocol::{ErrorKind, Event};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;

type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;
use crate::*;
use crate::wm_extras::*;
use crate::canvas::*;
use crate::model::*;
use crate::wm_core::*;
use crate::events::*;
use crate::clients::*;
use crate::draw_chrome::*;
use crate::draw_settings::*;
use crate::workspaces::*;
use crate::clipboard_ui::*;
use crate::wifi_ui::*;
use crate::settings_events::*;
use crate::keys::*;
use crate::dock_menus::*;
use crate::folder_ui::*;
use crate::screenshot::*;
use crate::terminal_ui::*;
use crate::folder_actions::*;
use crate::media_ui::*;
use crate::system_apply::*;
use crate::layout::*;
use crate::draw_helpers::*;
use crate::pixels::*;
use crate::textutil::*;
use crate::procutil::*;
use crate::files::*;

pub(crate) fn read_display_modes(display: &str, current_width: u16, current_height: u16) -> Vec<DisplayMode> {
    let mut modes = Vec::new();
    if let Ok(output) = Command::new("xrandr")
        .env("DISPLAY", display)
        .arg("--query")
        .output()
    {
        let text = String::from_utf8_lossy(&output.stdout);
        let mut current_output: Option<String> = None;
        for line in text.lines() {
            if !line.chars().next().is_some_and(char::is_whitespace) {
                let mut parts = line.split_whitespace();
                let Some(name) = parts.next() else {
                    continue;
                };
                current_output = parts
                    .next()
                    .is_some_and(|state| state == "connected")
                    .then(|| name.to_string());
                continue;
            }
            let trimmed = line.trim_start();
            let Some(first) = trimmed.split_whitespace().next() else {
                continue;
            };
            let Some((w, h)) = first.split_once('x') else {
                continue;
            };
            let (Ok(width), Ok(height)) = (w.parse::<u16>(), h.parse::<u16>()) else {
                continue;
            };
            let tokens = trimmed.split_whitespace().collect::<Vec<_>>();
            let current = tokens.iter().any(|token| token.contains('*'));
            let refresh = tokens
                .iter()
                .skip(1)
                .find_map(|token| token.trim_end_matches(['*', '+']).parse::<f32>().ok())
                .filter(|rate| *rate >= 1.0);
            let output_name = current_output.clone();
            if !modes.iter().any(|m: &DisplayMode| {
                m.output == output_name && m.width == width && m.height == height
            }) {
                modes.push(DisplayMode {
                    output: output_name,
                    width,
                    height,
                    refresh,
                    current,
                });
            }
        }
    }
    if !modes.iter().any(|mode| mode.current) {
        for mode in &mut modes {
            mode.current = mode.width == current_width && mode.height == current_height;
        }
    }
    modes.sort_by(|a, b| {
        b.current.cmp(&a.current).then_with(|| {
            (u32::from(b.width) * u32::from(b.height))
                .cmp(&(u32::from(a.width) * u32::from(a.height)))
        })
    });
    if modes.is_empty() {
        modes.push(DisplayMode {
            output: None,
            width: current_width,
            height: current_height,
            refresh: Some(60.0),
            current: true,
        });
        modes.push(DisplayMode {
            output: None,
            width: 1366,
            height: 768,
            refresh: Some(60.0),
            current: false,
        });
        modes.push(DisplayMode {
            output: None,
            width: 1600,
            height: 900,
            refresh: Some(60.0),
            current: false,
        });
        modes.push(DisplayMode {
            output: None,
            width: 1920,
            height: 1080,
            refresh: Some(60.0),
            current: false,
        });
    }
    modes
}

pub(crate) fn apply_xrandr_mode(display: &str, mode: &DisplayMode) -> Result<(), String> {
    let size = format!("{}x{}", mode.width, mode.height);
    if let Some(output) = mode.output.as_deref() {
        let mut cmd = Command::new("xrandr");
        cmd.env("DISPLAY", display)
            .args(["--output", output, "--mode", &size]);
        if let Some(rate) = mode.refresh {
            cmd.args(["--rate", &format!("{rate:.2}")]);
        }
        if command_status_success(&mut cmd) {
            return Ok(());
        }

        let mut without_rate = Command::new("xrandr");
        without_rate
            .env("DISPLAY", display)
            .args(["--output", output, "--mode", &size]);
        if command_status_success(&mut without_rate) {
            return Ok(());
        }
    }

    let mut by_size = Command::new("xrandr");
    by_size.env("DISPLAY", display).args(["-s", &size]);
    if command_status_success(&mut by_size) {
        return Ok(());
    }

    Err(format!("Could not switch to {size} with xrandr"))
}

pub(crate) fn apply_xrandr_brightness(
    display: &str,
    output: Option<&str>,
    brightness_percent: u8,
) -> Result<(), String> {
    let brightness = f32::from(brightness_percent.clamp(10, 100)) / 100.0;
    if let Some(output) = output {
        let mut cmd = Command::new("xrandr");
        cmd.env("DISPLAY", display).args([
            "--output",
            output,
            "--brightness",
            &format!("{brightness:.2}"),
        ]);
        if command_status_success(&mut cmd) {
            return Ok(());
        }
        return Err(format!("Could not set brightness for {output}"));
    }

    Err("Could not find an active display output for brightness".to_string())
}

pub(crate) fn read_cpu_model() -> String {
    let Ok(text) = fs::read_to_string("/proc/cpuinfo") else {
        return "Unknown CPU".to_string();
    };
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("model name") {
            if let Some((_, model)) = value.split_once(':') {
                return model.trim().to_string();
            }
        }
    }
    "Unknown CPU".to_string()
}

pub(crate) fn read_cpu_times() -> Option<CpuTimes> {
    let text = fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().next()?;
    let nums: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|p| p.parse().ok())
        .collect();
    if nums.len() < 5 {
        return None;
    }
    let idle = nums[3] + nums.get(4).copied().unwrap_or(0);
    let total = nums.iter().sum();
    Some(CpuTimes { idle, total })
}

pub(crate) fn read_cpu_status(cpu_usage: f32) -> String {
    let temp = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp")
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .map(|v| format!("{}%, {:.0} C", cpu_usage.round(), v / 1000.0));
    temp.unwrap_or_else(|| format!("{}% load", cpu_usage.round()))
}

pub(crate) fn read_cpu_frequencies() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/devices/system/cpu") {
        let mut cpus = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                let idx = name.strip_prefix("cpu")?.parse::<usize>().ok()?;
                Some((idx, entry.path()))
            })
            .collect::<Vec<_>>();
        cpus.sort_by_key(|(idx, _)| *idx);
        for (idx, path) in cpus {
            let freq_path = path.join("cpufreq/scaling_cur_freq");
            let fallback_path = path.join("cpufreq/cpuinfo_cur_freq");
            let freq = fs::read_to_string(&freq_path)
                .or_else(|_| fs::read_to_string(&fallback_path))
                .ok()
                .and_then(|text| text.trim().parse::<u64>().ok());
            if let Some(khz) = freq {
                out.push(format!("c{idx}: {:.2}GHz", khz as f64 / 1_000_000.0));
            }
        }
    }
    if !out.is_empty() {
        return out;
    }
    let Ok(text) = fs::read_to_string("/proc/cpuinfo") else {
        return out;
    };
    for (idx, mhz) in text
        .lines()
        .filter_map(|line| line.strip_prefix("cpu MHz").and_then(|v| v.split_once(':')))
        .filter_map(|(_, v)| v.trim().parse::<f64>().ok())
        .enumerate()
    {
        out.push(format!("c{idx}: {:.2}GHz", mhz / 1000.0));
    }
    out
}

pub(crate) fn cpu_frequency_lines(freqs: &[String], max_chars: usize) -> Vec<String> {
    if freqs.is_empty() {
        return vec!["No CPU frequency data".to_string()];
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    for freq in freqs {
        let sep = if line.is_empty() { "" } else { "  " };
        if !line.is_empty() && line.len() + sep.len() + freq.len() > max_chars {
            lines.push(line);
            line = String::new();
        }
        if !line.is_empty() {
            line.push_str(sep);
        }
        line.push_str(freq);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

pub(crate) fn read_memory() -> (u64, u64, u64, u64) {
    let Ok(text) = fs::read_to_string("/proc/meminfo") else {
        return (0, 0, 0, 0);
    };
    let mut total = 0;
    let mut available = 0;
    let mut swap_total = 0;
    let mut swap_free = 0;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let key = parts.next().unwrap_or("");
        let value = parts
            .next()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        match key {
            "MemTotal:" => total = value,
            "MemAvailable:" => available = value,
            "SwapTotal:" => swap_total = value,
            "SwapFree:" => swap_free = value,
            _ => {}
        }
    }
    (
        total,
        total.saturating_sub(available),
        swap_total,
        swap_total.saturating_sub(swap_free),
    )
}

fn user_config_dir() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".config"))
}

/// Screen rotation choice (Settings > Display > Rotation), applied by
/// /usr/local/libexec/aurora-rotate (started from /etc/aurora/xinitrc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RotationMode {
    /// Follow the accelerometer.
    Auto,
    /// Locked upright (0 degrees, "Off").
    Normal,
    /// Locked at 90 degrees (xrandr "left").
    Left,
    /// Locked at 180 degrees.
    Inverted,
    /// Locked at 270 degrees (xrandr "right").
    Right,
}

/// Selector order and labels.
pub(crate) const ROTATION_CHOICES: [(RotationMode, &str); 5] = [
    (RotationMode::Auto, "Auto"),
    (RotationMode::Normal, "Off"),
    (RotationMode::Left, "90°"),
    (RotationMode::Inverted, "180°"),
    (RotationMode::Right, "270°"),
];

impl RotationMode {
    /// The word stored in the mode file (also the xrandr rotation).
    pub(crate) fn key(self) -> &'static str {
        match self {
            RotationMode::Auto => "auto",
            RotationMode::Normal => "normal",
            RotationMode::Left => "left",
            RotationMode::Inverted => "inverted",
            RotationMode::Right => "right",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Some(match key.trim().to_ascii_lowercase().as_str() {
            "auto" => RotationMode::Auto,
            "normal" | "off" | "0" | "locked" => RotationMode::Normal,
            "left" | "90" => RotationMode::Left,
            "inverted" | "180" => RotationMode::Inverted,
            "right" | "270" => RotationMode::Right,
            _ => return None,
        })
    }
}

/// One word per user (auto/normal/left/inverted/right), read by aurora-rotate.
pub(crate) fn rotation_mode_path() -> PathBuf {
    user_config_dir().join("aurora-wm").join("rotation")
}

/// Pre-selector lock file (auto rotate off); still honoured when no mode
/// file exists, as "Off".
fn legacy_rotation_lock_path() -> PathBuf {
    user_config_dir().join("omarchy").join("rotation-lock")
}

pub(crate) fn rotation_mode() -> RotationMode {
    match fs::read_to_string(rotation_mode_path()) {
        Ok(text) => RotationMode::from_key(&text).unwrap_or(RotationMode::Auto),
        Err(_) if legacy_rotation_lock_path().exists() => RotationMode::Normal,
        Err(_) => RotationMode::Auto,
    }
}

pub(crate) fn set_rotation_mode(mode: RotationMode) -> Result<(), String> {
    let path = rotation_mode_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|err| format!("Rotation: {err}"))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, format!("{}\n", mode.key()))
        .and_then(|()| fs::rename(&tmp, &path))
        .map_err(|err| format!("Rotation: {err}"))?;
    let _ = fs::remove_file(legacy_rotation_lock_path());
    wake_rotate_helper();
    Ok(())
}

/// Nudge aurora-rotate to re-read the mode now instead of at its next poll:
/// it sleeps reading this FIFO. Non-blocking; no reader (helper not
/// running) is not an error.
fn wake_rotate_helper() {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let fifo = dir.join("aurora-rotate.wake");
    if let Ok(mut f) = fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
    {
        let _ = f.write_all(b"\n");
    }
}

/// Whether an IIO accelerometer exists now. On some boots the AOP (motion
/// coprocessor) firmware crashes during init and there is none until reboot.
pub(crate) fn accelerometer_available() -> bool {
    fs::read_dir("/sys/bus/iio/devices")
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.path().join("in_accel_x_raw").exists())
        })
        .unwrap_or(false)
}

/// One active swap area from /proc/swaps; zram devices carry their
/// compressor and the RAM their compressed pages occupy.
#[derive(Clone, Default)]
pub(crate) struct SwapArea {
    pub(crate) path: String,
    pub(crate) zram: bool,
    pub(crate) size_kb: u64,
    pub(crate) used_kb: u64,
    pub(crate) zram_algorithm: Option<String>,
    pub(crate) zram_ram_kb: Option<u64>,
}

pub(crate) fn read_swaps() -> Vec<SwapArea> {
    let Ok(text) = fs::read_to_string("/proc/swaps") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        // /proc/swaps escapes spaces in paths as \040.
        let path = fields[0].replace("\\040", " ");
        let size_kb = fields[2].parse().unwrap_or(0);
        let used_kb = fields[3].parse().unwrap_or(0);
        let zram_name = path
            .strip_prefix("/dev/")
            .filter(|name| name.starts_with("zram"))
            .map(str::to_string);
        let mut area = SwapArea { path, zram: zram_name.is_some(), size_kb, used_kb, ..Default::default() };
        if let Some(name) = zram_name {
            let sys = format!("/sys/block/{name}");
            // "lzo [lz4] zstd": the selected algorithm is bracketed.
            area.zram_algorithm = fs::read_to_string(format!("{sys}/comp_algorithm"))
                .ok()
                .and_then(|algos| {
                    algos
                        .split_whitespace()
                        .find_map(|a| a.strip_prefix('[').and_then(|a| a.strip_suffix(']')))
                        .map(str::to_string)
                });
            // mm_stat: orig_data_size compr_data_size mem_used_total ... (bytes)
            area.zram_ram_kb = fs::read_to_string(format!("{sys}/mm_stat"))
                .ok()
                .and_then(|stat| stat.split_whitespace().nth(2)?.parse::<u64>().ok())
                .map(|bytes| bytes / 1024);
        }
        out.push(area);
    }
    out
}

/// Compact size for swap rows: MB below 1 GB, otherwise GB.
pub(crate) fn format_swap_size(kib: u64) -> String {
    if kib < 1024 * 1024 {
        format!("{} MB", (kib + 512) / 1024)
    } else {
        format_gb(kib)
    }
}

pub(crate) fn read_gpus() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("card") || name.contains('-') {
                continue;
            }
            let vendor = fs::read_to_string(entry.path().join("device/vendor"))
                .unwrap_or_default()
                .trim()
                .to_string();
            let device = fs::read_to_string(entry.path().join("device/device"))
                .unwrap_or_default()
                .trim()
                .to_string();
            if vendor.is_empty() && device.is_empty() {
                out.push(name);
            } else {
                out.push(format!("{name} {vendor}:{device}"));
            }
        }
    }
    out
}

pub(crate) fn read_nics() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name != "lo" {
                out.push(name);
            }
        }
    }
    out.sort();
    out
}

pub(crate) fn read_audio_devices(kind: AudioDeviceKind) -> Vec<AudioDevice> {
    let default_name = read_pactl_default_audio_device(kind);
    let mut devices = read_pactl_audio_devices(kind, default_name.as_deref());
    if devices.is_empty() {
        devices = read_wpctl_audio_devices(kind);
    }
    if kind == AudioDeviceKind::Input {
        let filtered = devices
            .iter()
            .filter(|device| !device.name.ends_with(".monitor"))
            .cloned()
            .collect::<Vec<_>>();
        if !filtered.is_empty() {
            devices = filtered;
        }
    }
    devices.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.label.cmp(&b.label)));
    devices
}

pub(crate) fn read_pactl_default_audio_device(kind: AudioDeviceKind) -> Option<String> {
    let output = pulse_command_output("pactl", &["info"])?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix(kind.pactl_default_key())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

pub(crate) fn read_pactl_audio_devices(kind: AudioDeviceKind, default_name: Option<&str>) -> Vec<AudioDevice> {
    let Some(output) = pulse_command_output("pactl", &["list", kind.pactl_list_arg()]) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut devices = Vec::new();
    let mut id = String::new();
    let mut name = String::new();
    let mut label = String::new();
    let header = match kind {
        AudioDeviceKind::Output => "Sink #",
        AudioDeviceKind::Input => "Source #",
    };
    let push_device =
        |devices: &mut Vec<AudioDevice>, id: &mut String, name: &mut String, label: &mut String| {
            if name.is_empty() {
                id.clear();
                label.clear();
                return;
            }
            let display = if label.is_empty() {
                prettify_audio_name(name)
            } else {
                label.clone()
            };
            let is_default = default_name.is_some_and(|default| default == name || default == id);
            devices.push(AudioDevice {
                id: id.clone(),
                name: name.clone(),
                label: display,
                is_default,
            });
            id.clear();
            name.clear();
            label.clear();
        };

    for line in text.lines() {
        if let Some(value) = line.trim().strip_prefix(header) {
            push_device(&mut devices, &mut id, &mut name, &mut label);
            id = value.trim().to_string();
        } else if let Some(value) = line.trim().strip_prefix("Name:") {
            name = value.trim().to_string();
        } else if let Some(value) = line.trim().strip_prefix("Description:") {
            label = value.trim().to_string();
        }
    }
    push_device(&mut devices, &mut id, &mut name, &mut label);
    if devices.is_empty() {
        read_pactl_short_audio_devices(kind, default_name)
    } else {
        devices
    }
}

pub(crate) fn read_pactl_short_audio_devices(
    kind: AudioDeviceKind,
    default_name: Option<&str>,
) -> Vec<AudioDevice> {
    let Some(output) = pulse_command_output("pactl", &["list", "short", kind.pactl_list_arg()])
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let id = parts.next()?.to_string();
            let name = parts.next()?.to_string();
            let is_default = default_name.is_some_and(|default| default == name || default == id);
            Some(AudioDevice {
                id,
                label: prettify_audio_name(&name),
                name,
                is_default,
            })
        })
        .collect()
}

pub(crate) fn read_wpctl_audio_devices(kind: AudioDeviceKind) -> Vec<AudioDevice> {
    let Some(output) = pulse_command_output("wpctl", &["status"]) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let wanted = match kind {
        AudioDeviceKind::Output => "Sinks:",
        AudioDeviceKind::Input => "Sources:",
    };
    let mut in_audio = false;
    let mut in_section = false;
    let mut devices = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "Audio" {
            in_audio = true;
            in_section = false;
            continue;
        }
        if matches!(trimmed, "Video" | "Settings") {
            in_audio = false;
            in_section = false;
        }
        if !in_audio {
            continue;
        }
        if trimmed.contains(wanted) {
            in_section = true;
            continue;
        }
        if in_section && (trimmed.starts_with("├─") || trimmed.starts_with("└─")) {
            break;
        }
        if !in_section {
            continue;
        }
        let Some(dot) = trimmed.find('.') else {
            continue;
        };
        let prefix = trimmed[..dot].replace(['│', '*'], " ");
        let Some(id) = prefix.split_whitespace().last() else {
            continue;
        };
        if !id.chars().all(|ch| ch.is_ascii_digit()) {
            continue;
        }
        let rest = trimmed[dot + 1..].trim();
        let label = rest
            .split_once("  [")
            .map(|(label, _)| label)
            .or_else(|| rest.split_once(" [").map(|(label, _)| label))
            .unwrap_or(rest)
            .trim();
        if label.is_empty() {
            continue;
        }
        devices.push(AudioDevice {
            id: id.to_string(),
            name: id.to_string(),
            label: label.to_string(),
            is_default: trimmed.contains('*'),
        });
    }
    devices
}

pub(crate) fn prettify_audio_name(name: &str) -> String {
    name.replace("alsa_output.", "")
        .replace("alsa_input.", "")
        .replace("pci-", "PCI ")
        .replace("usb-", "USB ")
        .replace(['_', '.'], " ")
}

pub(crate) fn read_audio_volume_percent() -> Option<u8> {
    if let Some(output) = pulse_command_output("pactl", &["get-sink-volume", "@DEFAULT_SINK@"]) {
        if output.status.success() {
            if let Some(percent) = parse_first_percent(&String::from_utf8_lossy(&output.stdout)) {
                return Some(percent);
            }
        }
    }
    if let Some(output) = pulse_command_output("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"]) {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Some(value) = text
                .split_whitespace()
                .find_map(|token| token.parse::<f32>().ok())
            {
                return Some((value * 100.0).round().clamp(0.0, 100.0) as u8);
            }
        }
    }
    None
}

pub(crate) fn parse_first_percent(text: &str) -> Option<u8> {
    text.split_whitespace().find_map(|token| {
        token
            .trim_end_matches(',')
            .strip_suffix('%')?
            .parse::<u16>()
            .ok()
            .map(|value| value.min(100) as u8)
    })
}

pub(crate) fn set_audio_volume_percent(percent: u8) -> Result<(), String> {
    let percent = percent.min(100);
    let pactl_percent = format!("{percent}%");
    let mut pactl = Command::new("pactl");
    pactl.args(["set-sink-volume", "@DEFAULT_SINK@", &pactl_percent]);
    apply_pulse_env_defaults(&mut pactl);
    if command_status_success(&mut pactl) {
        let mut unmute = Command::new("pactl");
        unmute.args(["set-sink-mute", "@DEFAULT_SINK@", "0"]);
        apply_pulse_env_defaults(&mut unmute);
        let _ = command_status_success(&mut unmute);
        return Ok(());
    }

    let wpctl_value = format!("{:.2}", f32::from(percent) / 100.0);
    let mut wpctl = Command::new("wpctl");
    wpctl.args(["set-volume", "@DEFAULT_AUDIO_SINK@", &wpctl_value]);
    apply_pulse_env_defaults(&mut wpctl);
    if command_status_success(&mut wpctl) {
        let mut unmute = Command::new("wpctl");
        unmute.args(["set-mute", "@DEFAULT_AUDIO_SINK@", "0"]);
        apply_pulse_env_defaults(&mut unmute);
        let _ = command_status_success(&mut unmute);
        return Ok(());
    }

    let mut amixer = Command::new("amixer");
    amixer.args(["-D", "pulse", "sset", "Master", &pactl_percent]);
    if command_status_success(&mut amixer) {
        return Ok(());
    }

    Err("Could not set audio volume".to_string())
}

pub(crate) fn set_default_audio_device(kind: AudioDeviceKind, device: &AudioDevice) -> Result<(), String> {
    let mut pactl = Command::new("pactl");
    pactl.args([kind.pactl_set_default_command(), &device.name]);
    apply_pulse_env_defaults(&mut pactl);
    if command_status_success(&mut pactl) {
        move_current_audio_streams(kind, &device.name);
        return Ok(());
    }

    if !device.id.is_empty() {
        let mut wpctl = Command::new("wpctl");
        wpctl.args(["set-default", &device.id]);
        apply_pulse_env_defaults(&mut wpctl);
        if command_status_success(&mut wpctl) {
            return Ok(());
        }
    }

    Err(format!("Could not set {} as default", device.label))
}

pub(crate) fn move_current_audio_streams(kind: AudioDeviceKind, device_name: &str) {
    let (list_arg, move_arg) = match kind {
        AudioDeviceKind::Output => ("sink-inputs", "move-sink-input"),
        AudioDeviceKind::Input => ("source-outputs", "move-source-output"),
    };
    let Some(output) = pulse_command_output("pactl", &["list", "short", list_arg]) else {
        return;
    };
    if !output.status.success() {
        return;
    }
    for stream_id in String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
    {
        let mut cmd = Command::new("pactl");
        cmd.args([move_arg, stream_id, device_name]);
        apply_pulse_env_defaults(&mut cmd);
        let _ = command_status_success(&mut cmd);
    }
}

pub(crate) fn read_network_details() -> Vec<String> {
    // One line per real interface ("wld0  up  192.168.1.7/24"), live ones
    // first: the card only has room for four. Tunnels and other non-Ethernet
    // links (sit0, ip6tnl0) are skipped unless they carry an address.
    let mut rows: Vec<(bool, String)> = Vec::new();
    for nic in read_nics() {
        let read = |f: &str| {
            fs::read_to_string(format!("/sys/class/net/{nic}/{f}"))
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        let state = read("operstate");
        let ethernet = read("type") == "1";
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        if let Ok(output) = Command::new("ip")
            .args(["-o", "addr", "show", "dev", &nic, "scope", "global"])
            .output()
        {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let parts = line.split_whitespace().collect::<Vec<_>>();
                if parts.len() > 3 && parts[2] == "inet" {
                    v4.push(parts[3].to_string());
                } else if parts.len() > 3 && parts[2] == "inet6" {
                    v6.push(parts[3].to_string());
                }
            }
        }
        if !ethernet && v4.is_empty() {
            continue;
        }
        let addr = v4.first().or(v6.first()).cloned().unwrap_or_else(|| "no address".to_string());
        let live = state == "up" && !v4.is_empty();
        rows.push((live, format!("{nic}  {state}  {addr}")));
    }
    rows.sort_by_key(|(live, _)| !*live);
    let mut out = rows.into_iter().map(|(_, line)| line).collect::<Vec<_>>();
    if out.is_empty() {
        out.push("No network devices found".to_string());
    }
    out
}

pub(crate) fn scan_wifi_networks(rescan: bool) -> Result<Vec<WifiNetwork>, String> {
    let rescan_val = if rescan { "yes" } else { "no" };
    let output = Command::new("nmcli")
        .args([
            "-t", "-f", "SSID", "dev", "wifi", "list", "--rescan", rescan_val,
        ])
        .output()
        .map_err(|err| format!("Could not run nmcli Wi-Fi scan: {err}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(compact(&format!("Wi-Fi scan failed: {}", err.trim()), 70));
    }

    let mut networks = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let ssid = unescape_nmcli_field(line.trim());
        if ssid.is_empty()
            || networks
                .iter()
                .any(|network: &WifiNetwork| network.ssid == ssid)
        {
            continue;
        }
        networks.push(WifiNetwork { ssid });
    }
    Ok(networks)
}

pub(crate) fn connect_wifi_network(ssid: &str, password: &str) -> Result<(), String> {
    let mut cmd = Command::new("nmcli");
    cmd.args(["dev", "wifi", "connect", ssid]);
    if !password.is_empty() {
        cmd.args(["password", password]);
    }
    let output = cmd
        .output()
        .map_err(|err| format!("Could not run nmcli connect: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        Err(compact(&format!("Wi-Fi connect failed: {message}"), 70))
    }
}

pub(crate) fn disconnect_current_wifi() -> Result<(), String> {
    let Some(wifi) = read_connected_wifi() else {
        return Err("No connected Wi-Fi to disconnect".to_string());
    };
    let output = Command::new("nmcli")
        .args(["dev", "disconnect", &wifi.device])
        .output()
        .map_err(|err| format!("Could not run nmcli disconnect: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        Err(compact(&format!("Wi-Fi disconnect failed: {message}"), 70))
    }
}

pub(crate) fn read_connected_wifi() -> Option<WifiConnection> {
    let output = Command::new("nmcli")
        .args(["-t", "-f", "DEVICE,TYPE,STATE", "dev", "status"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let device = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let parts = split_nmcli_line(line);
            (parts.len() >= 3 && parts[1] == "wifi" && parts[2] == "connected")
                .then(|| parts[0].clone())
        })?;

    let ssid = Command::new("nmcli")
        .args(["-t", "-f", "ACTIVE,SSID", "dev", "wifi"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| {
                    let parts = split_nmcli_line(line);
                    (parts.len() >= 2 && parts[0] == "yes").then(|| parts[1].clone())
                })
        })
        .unwrap_or_else(|| device.clone());
    let ip = Command::new("ip")
        .args(["-o", "-4", "addr", "show", "dev", &device])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .collect::<Vec<_>>()
                .windows(2)
                .find_map(|parts| (parts[0] == "inet").then(|| parts[1].to_string()))
        });
    Some(WifiConnection { ssid, device, ip })
}

pub(crate) fn unescape_nmcli_field(value: &str) -> String {
    let mut out = String::new();
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    if escaped {
        out.push('\\');
    }
    out
}

pub(crate) fn split_nmcli_line(line: &str) -> Vec<String> {
    line.split(':').map(unescape_nmcli_field).collect()
}

pub(crate) fn read_wifi_radio_enabled() -> bool {
    if let Ok(output) = Command::new("nmcli").args(["radio", "wifi"]).output() {
        if output.status.success() {
            let status = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return status == "enabled";
        }
    }
    true
}

pub(crate) fn set_wifi_radio_enabled(enabled: bool) -> Result<(), String> {
    let arg = if enabled { "on" } else { "off" };
    let output = Command::new("nmcli")
        .args(["radio", "wifi", arg])
        .output()
        .map_err(|err| format!("Could not run nmcli radio wifi: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

pub(crate) fn password_mask(len: usize) -> String {
    "*".repeat(len.min(32))
}

pub(crate) fn read_bluetooth_devices() -> Vec<String> {
    let Ok(output) = Command::new("bluetoothctl")
        .arg("devices")
        .arg("Connected")
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim_start_matches("Device ").to_string())
        .collect()
}

pub(crate) fn read_autostart_apps() -> Vec<String> {
    let mut apps = Vec::new();
    for dir in [
        home_dir().join(".config/autostart"),
        PathBuf::from("/etc/xdg/autostart"),
    ] {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let text = fs::read_to_string(entry.path()).unwrap_or_default();
            let name = text
                .lines()
                .find_map(|line| line.strip_prefix("Name=").map(str::to_string))
                .unwrap_or_else(|| entry.file_name().to_string_lossy().to_string());
            apps.push(name);
        }
    }
    apps.sort();
    apps.dedup();
    apps
}

pub(crate) fn terminal_settings_path() -> PathBuf {
    home_dir().join(".config/aurora-wm/settings.conf")
}

pub(crate) fn read_setting_value(key: &str) -> Option<String> {
    fs::read_to_string(terminal_settings_path())
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let (line_key, value) = line.split_once('=')?;
                (line_key == key).then(|| value.to_string())
            })
        })
}

pub(crate) fn read_u32_setting(key: &str, fallback: u32) -> u32 {
    read_setting_value(key)
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(fallback)
}

/// GTK scale saved in Settings → Display: 1, 2 or 3, or 0 when automatic.
/// Older builds saved `ui_scale_percent`; that is read once and rounded to a
/// whole scale (200 → 2, 125 → 1, 150 → 2).
pub(crate) fn saved_gdk_scale() -> u32 {
    if let Some(value) = read_setting_value("gdk_scale").and_then(|v| v.trim().parse::<u32>().ok()) {
        return if (1..=3).contains(&value) { value } else { 0 };
    }
    match read_u32_setting("ui_scale_percent", 0) {
        pct if pct >= 100 => ((pct + 50) / 100).clamp(1, 3),
        _ => 0,
    }
}

pub(crate) fn read_bool_setting(key: &str, fallback: bool) -> bool {
    read_setting_value(key)
        .and_then(|value| match value.trim() {
            "1" | "true" | "on" | "yes" => Some(true),
            "0" | "false" | "off" | "no" => Some(false),
            _ => None,
        })
        .unwrap_or(fallback)
}

pub(crate) fn read_app_command(kind: DefaultAppKind) -> String {
    read_setting_value(kind.key()).unwrap_or_default()
}

pub(crate) fn save_app_commands(settings: &SettingsState) -> AnyResult<()> {
    let path = terminal_settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let clean = |command: &str| command.replace(['\n', '\r'], "");
    fs::write(
        path,
        format!(
            "terminal={}\nbrowser={}\nphoto={}\nvideo={}\nsleep_after_secs={}\nbrightness_percent={}\ncompositor_enabled={}\ndock_in_topbar={}\nauto_power_saver_enabled={}\nauto_power_saver_minutes={}\nshortcut_folder={}\nshortcut_terminal={}\nshortcut_clipboard={}\nshortcut_screenshot={}\ngdk_scale={}\ntopbar_keyboard={}\ntopbar_clipboard={}\ntopbar_screenshot={}\ntopbar_recording={}\ntopbar_display={}\ntopbar_audio={}\ntopbar_wifi={}\n",
            clean(&settings.terminal_command),
            clean(&settings.browser_command),
            clean(&settings.photo_command),
            clean(&settings.video_command),
            settings.sleep_after_secs.min(7200),
            settings.brightness_percent.clamp(10, 100),
            u8::from(settings.compositor_enabled),
            u8::from(settings.dock_in_topbar),
            u8::from(settings.auto_power_saver_enabled),
            settings
                .auto_power_saver_minutes
                .clamp(AUTO_POWER_SAVER_MIN_MINUTES, AUTO_POWER_SAVER_MAX_MINUTES),
            shortcut_setting_string(settings.shortcuts.folder),
            shortcut_setting_string(settings.shortcuts.terminal),
            shortcut_setting_string(settings.shortcuts.clipboard),
            shortcut_setting_string(settings.shortcuts.screenshot),
            settings.gdk_scale,
            u8::from(settings.topbar_icons.keyboard),
            u8::from(settings.topbar_icons.clipboard),
            u8::from(settings.topbar_icons.screenshot),
            u8::from(settings.topbar_icons.recording),
            u8::from(settings.topbar_icons.display),
            u8::from(settings.topbar_icons.audio),
            u8::from(settings.topbar_icons.wifi),
        ),
    )?;
    Ok(())
}

pub(crate) fn read_current_power_mode() -> Option<PowerMode> {
    let output = Command::new("powerprofilesctl").arg("get").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout);
    PowerMode::from_command_value(value.trim())
}

pub(crate) fn current_power_mode_cached_or_refresh() -> Option<PowerMode> {
    if power_mode_cache_fresh() {
        return read_cached_power_mode();
    }

    if let Some(_lock) = try_tmp_file_lock(
        POWER_PROFILE_LOCK_PATH,
        IDLE_CHECK_INTERVAL + Duration::from_secs(5),
    ) {
        if power_mode_cache_fresh() {
            return read_cached_power_mode();
        }
        if let Some(mode) = read_current_power_mode() {
            let _ = write_power_mode_cache(mode);
            return Some(mode);
        }
    }

    read_cached_power_mode()
}

pub(crate) fn power_mode_cache_fresh() -> bool {
    file_age(POWER_PROFILE_CACHE_PATH).is_some_and(|age| age < IDLE_CHECK_INTERVAL)
}

pub(crate) fn read_cached_power_mode() -> Option<PowerMode> {
    let text = fs::read_to_string(POWER_PROFILE_CACHE_PATH).ok()?;
    PowerMode::from_command_value(text.trim())
}

pub(crate) fn write_power_mode_cache(mode: PowerMode) -> AnyResult<()> {
    fs::write(
        POWER_PROFILE_CACHE_PATH,
        format!("{}\n", mode.command_value()),
    )?;
    Ok(())
}

pub(crate) struct TmpFileLock {
    pub(crate) path: &'static str,
}

impl Drop for TmpFileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path);
    }
}

pub(crate) fn try_tmp_file_lock(path: &'static str, stale_after: Duration) -> Option<TmpFileLock> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(_) => Some(TmpFileLock { path }),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            if file_age(path).is_some_and(|age| age > stale_after) {
                let _ = fs::remove_file(path);
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .ok()
                    .map(|_| TmpFileLock { path })
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

pub(crate) fn touch_notidle_marker() -> AnyResult<()> {
    if file_age(NOT_IDLE_MARKER_PATH).is_some_and(|age| age < Duration::from_secs(1)) {
        return Ok(());
    }
    fs::write(NOT_IDLE_MARKER_PATH, b"notidle\n")?;
    Ok(())
}

pub(crate) fn notidle_marker_age() -> Option<Duration> {
    file_age(NOT_IDLE_MARKER_PATH)
}

pub(crate) fn file_age(path: &str) -> Option<Duration> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()?
        .elapsed()
        .ok()
}

const DESKTOP_ENTRIES_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
type DesktopEntrySources = Vec<(PathBuf, u64, Option<std::time::SystemTime>)>;

struct DesktopEntriesCache {
    entries: std::sync::Arc<[DesktopEntry]>,
    sources: DesktopEntrySources,
    checked_at: Instant,
    refreshing: bool,
}

impl DesktopEntriesCache {
    fn needs_refresh(&self, now: Instant) -> bool {
        !self.refreshing && now.saturating_duration_since(self.checked_at) >= DESKTOP_ENTRIES_REFRESH_INTERVAL
    }
}

fn desktop_entry_dirs() -> Vec<PathBuf> {
    vec![
        home_dir().join(".local/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
        PathBuf::from("/usr/share/applications"),
    ]
}

fn desktop_entry_sources(dirs: &[PathBuf]) -> DesktopEntrySources {
    let mut paths = dirs.to_vec();
    for dir in dirs {
        if let Ok(entries) = fs::read_dir(dir) {
            paths.extend(entries.flatten().map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("desktop")));
        }
    }
    paths.sort_unstable();
    paths.into_iter().map(|path| {
        let metadata = fs::metadata(&path).ok();
        let size = metadata.as_ref().map(|metadata| metadata.len()).unwrap_or(0);
        let modified = metadata.and_then(|metadata| metadata.modified().ok());
        (path, size, modified)
    }).collect()
}

/// Startup discovery prewarms this shared snapshot. Menus never reread desktop
/// files synchronously; installation and file edits are checked off-thread.
pub(crate) fn cached_desktop_entries() -> std::sync::Arc<[DesktopEntry]> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<DesktopEntriesCache>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        let sources = desktop_entry_sources(&desktop_entry_dirs());
        std::sync::Mutex::new(DesktopEntriesCache {
            entries: read_desktop_entries_uncached().into(),
            sources,
            checked_at: Instant::now(),
            refreshing: false,
        })
    });
    let mut state = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let snapshot = std::sync::Arc::clone(&state.entries);
    if state.needs_refresh(Instant::now()) {
        state.refreshing = true;
        state.checked_at = Instant::now();
        drop(state);
        let spawned = thread::Builder::new().name("aurora-app-catalog".into())
            .stack_size(256 * 1024).spawn(move || {
                let sources = desktop_entry_sources(&desktop_entry_dirs());
                let changed = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).sources != sources;
                let replacement = changed.then(|| std::sync::Arc::<[DesktopEntry]>::from(read_desktop_entries_uncached()));
                let mut state = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(entries) = replacement { state.entries = entries; }
                state.sources = sources;
                state.checked_at = Instant::now();
                state.refreshing = false;
            });
        if spawned.is_err() {
            cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).refreshing = false;
        }
    }
    snapshot
}

pub(crate) fn read_desktop_entries() -> Vec<DesktopEntry> {
    cached_desktop_entries().to_vec()
}

fn read_desktop_entries_uncached() -> Vec<DesktopEntry> {
    let mut entries = Vec::new();
    for dir in desktop_entry_dirs() {
        let Ok(read_dir) = fs::read_dir(dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            if entry.path().extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let text = fs::read_to_string(entry.path()).unwrap_or_default();
            if text.lines().any(|line| line == "NoDisplay=true") {
                continue;
            }
            let name = text
                .lines()
                .find_map(|line| line.strip_prefix("Name=").map(str::to_string))
                .unwrap_or_else(|| entry.file_name().to_string_lossy().to_string());
            let cats = text
                .lines()
                .find_map(|line| line.strip_prefix("Categories=").map(str::to_string))
                .unwrap_or_default();
            let mime_types = text
                .lines()
                .find_map(|line| line.strip_prefix("MimeType=").map(str::to_string))
                .unwrap_or_default();
            let keywords = text
                .lines()
                .find_map(|line| line.strip_prefix("Keywords=").map(str::to_string))
                .unwrap_or_default();
            let command = text
                .lines()
                .find_map(|line| line.strip_prefix("Exec=").map(clean_desktop_command))
                .unwrap_or_default();
            let category = if cats.contains("Network") {
                "Internet"
            } else if cats.contains("System") || cats.contains("Settings") {
                "System"
            } else if cats.contains("Utility") || cats.contains("Development") {
                "Program"
            } else if cats.contains("Audio") || cats.contains("Video") || cats.contains("Graphics")
            {
                "Media"
            } else {
                "Other"
            }
            .to_string();
            entries.push(DesktopEntry {
                name,
                category,
                command,
                categories: cats,
                mime_types,
                keywords,
            });
        }
    }
    entries.sort_by(|a, b| {
        let rank = |category: &str| match category {
            "Internet" => 0,
            "System" => 1,
            "Program" => 2,
            "Media" => 3,
            _ => 4,
        };
        rank(&a.category)
            .cmp(&rank(&b.category))
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
}

pub(crate) fn clean_desktop_command(value: &str) -> String {
    value
        .split_whitespace()
        .map(|arg| {
            ["%f", "%F", "%u", "%U", "%i", "%c", "%k"]
                .iter()
                .fold(arg.to_string(), |clean, field| clean.replace(field, ""))
        })
        .filter(|arg| !arg.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn discover_installed_apps() -> (
    Vec<InstalledApp>,
    Vec<InstalledApp>,
    Vec<InstalledApp>,
    Vec<InstalledApp>,
) {
    let entries = read_desktop_entries();
    (
        installed_apps(DefaultAppKind::Terminal, &entries),
        installed_apps(DefaultAppKind::Browser, &entries),
        installed_apps(DefaultAppKind::Photo, &entries),
        installed_apps(DefaultAppKind::Video, &entries),
    )
}

pub(crate) fn installed_apps(kind: DefaultAppKind, entries: &[DesktopEntry]) -> Vec<InstalledApp> {
    let mut apps = Vec::new();
    if kind == DefaultAppKind::Terminal {
        for command in TERMINAL_FALLBACKS {
            if command_exists(command) {
                push_installed_app(&mut apps, command.to_string(), command.to_string());
            }
        }
    }
    for entry in entries {
        if entry.command.is_empty() || !command_can_launch(&entry.command) {
            continue;
        }
        let command_lower = entry.command.to_ascii_lowercase();
        let name_lower = entry.name.to_ascii_lowercase();
        let matches = match kind {
            DefaultAppKind::Terminal => {
                entry.categories.contains("TerminalEmulator")
                    || [
                        "terminal",
                        "konsole",
                        "xterm",
                        "kitty",
                        "alacritty",
                        "wezterm",
                    ]
                    .iter()
                    .any(|term| name_lower.contains(term) || command_lower.contains(term))
            }
            DefaultAppKind::Browser => {
                entry.categories.contains("WebBrowser")
                    || entry.mime_types.contains("x-scheme-handler/http")
            }
            DefaultAppKind::Photo => entry.mime_types.contains("image/"),
            DefaultAppKind::Video => entry.mime_types.contains("video/"),
        };
        if matches {
            push_installed_app(&mut apps, entry.name.clone(), entry.command.clone());
        }
    }
    if kind != DefaultAppKind::Terminal {
        apps.sort_by(|left, right| left.name.cmp(&right.name));
    }
    apps
}

pub(crate) fn push_installed_app(apps: &mut Vec<InstalledApp>, name: String, command: String) {
    if apps.iter().any(|app| app.command == command) {
        return;
    }
    apps.push(InstalledApp { name, command });
}

pub(crate) fn command_can_launch(command: &str) -> bool {
    let executable = command
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(['\'', '"']);
    let path = Path::new(executable);
    if path.is_absolute() {
        path.exists()
    } else {
        command_exists(executable)
    }
}

/// Byte counters per interface (name, rx, tx), excluding loopback.
pub(crate) fn read_nic_totals() -> Vec<(String, u64, u64)> {
    let Ok(text) = fs::read_to_string("/proc/net/dev") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines().skip(2) {
        let Some((iface, data)) = line.split_once(':') else {
            continue;
        };
        let name = iface.trim();
        if name == "lo" {
            continue;
        }
        let nums: Vec<u64> = data
            .split_whitespace()
            .filter_map(|p| p.parse::<u64>().ok())
            .collect();
        if nums.len() >= 16 {
            out.push((name.to_string(), nums[0], nums[8]));
        }
    }
    out
}

pub(crate) fn read_net_totals() -> Option<NetTotals> {
    let text = fs::read_to_string("/proc/net/dev").ok()?;
    let mut rx = 0u64;
    let mut tx = 0u64;
    for line in text.lines().skip(2) {
        let Some((iface, data)) = line.split_once(':') else {
            continue;
        };
        if iface.trim() == "lo" {
            continue;
        }
        let nums: Vec<u64> = data
            .split_whitespace()
            .filter_map(|p| p.parse::<u64>().ok())
            .collect();
        if nums.len() >= 16 {
            rx = rx.saturating_add(nums[0]);
            tx = tx.saturating_add(nums[8]);
        }
    }
    Some(NetTotals {
        rx,
        tx,
        at: Instant::now(),
    })
}

pub(crate) fn read_battery() -> Option<String> {
    let entries = fs::read_dir("/sys/class/power_supply").ok()?;
    for entry in entries.flatten() {
        let ty = fs::read_to_string(entry.path().join("type")).unwrap_or_default();
        if ty.trim() != "Battery" {
            continue;
        }
        let cap = fs::read_to_string(entry.path().join("capacity")).ok()?;
        let status = fs::read_to_string(entry.path().join("status")).unwrap_or_default();
        let mut label = format!("{}% {}", cap.trim(), status.trim());
        // Optional detail: current draw in watts and a rough time estimate.
        let read_u64 = |name: &str| {
            fs::read_to_string(entry.path().join(name))
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        let power_uw = read_u64("power_now")
            .or_else(|| Some(read_u64("current_now")? * read_u64("voltage_now")? / 1_000_000));
        if let Some(power_uw) = power_uw.filter(|&p| p > 0) {
            label.push_str(&format!(" - {:.1} W", power_uw as f64 / 1_000_000.0));
            let energy_uwh = read_u64("energy_now")
                .or_else(|| Some(read_u64("charge_now")? * read_u64("voltage_now")? / 1_000_000));
            if status.trim() == "Discharging" {
                if let Some(energy) = energy_uwh.filter(|&e| e > 0) {
                    let hours = energy as f64 / power_uw as f64;
                    label.push_str(&format!(
                        ", ~{}h {:02}m left",
                        hours as u64,
                        ((hours * 60.0) as u64) % 60
                    ));
                }
            }
        }
        return Some(label);
    }
    None
}

pub(crate) fn format_kib(kib: u64) -> String {
    if kib >= 1024 * 1024 {
        format!("{:.1} GiB", kib as f64 / 1024.0 / 1024.0)
    } else if kib >= 1024 {
        format!("{:.0} MiB", kib as f64 / 1024.0)
    } else {
        format!("{kib} KiB")
    }
}

pub(crate) fn format_bps(value: f64) -> String {
    if value >= 1024.0 * 1024.0 {
        format!("{:.1} MB/s", value / 1024.0 / 1024.0)
    } else if value >= 1024.0 {
        format!("{:.1} KB/s", value / 1024.0)
    } else {
        format!("{value:.0} B/s")
    }
}

#[cfg(test)]
mod desktop_catalog_cache_tests {
    use super::*;

    #[test]
    fn warm_catalog_shares_storage_and_refresh_is_throttled() {
        let first = cached_desktop_entries();
        let second = cached_desktop_entries();
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        let now = Instant::now();
        let mut cache = DesktopEntriesCache {
            entries: first,
            sources: Vec::new(),
            checked_at: now,
            refreshing: false,
        };
        assert!(!cache.needs_refresh(now + DESKTOP_ENTRIES_REFRESH_INTERVAL - Duration::from_millis(1)));
        assert!(cache.needs_refresh(now + DESKTOP_ENTRIES_REFRESH_INTERVAL));
        cache.refreshing = true;
        assert!(!cache.needs_refresh(now + DESKTOP_ENTRIES_REFRESH_INTERVAL * 2));
    }

    #[test]
    fn source_fingerprint_detects_installs_edits_and_removals() {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let directory = env::temp_dir().join(format!("aurora-catalog-cache-{}-{nonce}", process::id()));
        fs::create_dir_all(&directory).unwrap();
        let dirs = [directory.clone()];
        let empty = desktop_entry_sources(&dirs);
        let application = directory.join("example.desktop");
        fs::write(&application, "[Desktop Entry]\nName=Example\nExec=example\n").unwrap();
        let installed = desktop_entry_sources(&dirs);
        assert_ne!(installed, empty);
        fs::write(&application, "[Desktop Entry]\nName=Renamed Example\nExec=example --new\n").unwrap();
        let edited = desktop_entry_sources(&dirs);
        assert_ne!(edited, installed);
        fs::remove_file(&application).unwrap();
        let removed = desktop_entry_sources(&dirs);
        assert_ne!(removed, edited);
        assert!(removed.iter().all(|(path, _, _)| path != &application));
        fs::remove_dir_all(directory).unwrap();
    }
}

pub(crate) fn read_cpu_mhz() -> u32 {
    let mut khz = 0u64;
    if let Ok(entries) = fs::read_dir("/sys/devices/system/cpu/cpufreq") {
        for entry in entries.flatten() {
            if let Some(v) = fs::read_to_string(entry.path().join("scaling_cur_freq"))
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
            {
                khz = khz.max(v);
            }
        }
    }
    (khz / 1000) as u32
}

/// Battery state for the topbar (re-read every idle tick; two tiny sysfs reads).
pub(crate) fn read_topbar_status() -> TopbarStatus {
    let read = |path: &std::path::Path| fs::read_to_string(path).ok().map(|s| s.trim().to_string());
    let mut status = TopbarStatus::default();
    if let Ok(entries) = fs::read_dir("/sys/class/power_supply") {
        for entry in entries.flatten() {
            let path = entry.path();
            match read(&path.join("type")).as_deref() {
                Some("Battery") if status.battery_pct.is_none() => {
                    status.battery_pct = read(&path.join("capacity"))
                        .and_then(|v| v.parse::<u32>().ok())
                        .map(|v| v.min(100) as u8);
                    let state = read(&path.join("status")).unwrap_or_default();
                    status.charging |= matches!(state.as_str(), "Charging" | "Full");
                }
                Some("Mains" | "USB") => {
                    status.charging |= read(&path.join("online")).as_deref() == Some("1");
                }
                _ => {}
            }
        }
    }
    status
}

pub(crate) fn format_gb(kib: u64) -> String {
    let gb = kib as f64 / 1024.0 / 1024.0;
    if gb >= 10.0 { format!("{gb:.0} GB") } else { format!("{gb:.1} GB") }
}

/// GPU memory in use by this user's processes, from DRM fdinfo
/// (drm-resident-*/drm-total-* keys, deduplicated by drm-client-id), in KiB.
fn read_drm_client_memory_kib(driver: &str) -> Option<u64> {
    let mut seen = HashSet::new();
    let mut total = 0u64;
    let mut any = false;
    let procs = fs::read_dir("/proc").ok()?;
    for proc_entry in procs.flatten() {
        let name = proc_entry.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(fds) = fs::read_dir(proc_entry.path().join("fd")) else { continue };
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else { continue };
            if !target.to_string_lossy().starts_with("/dev/dri/") {
                continue;
            }
            let info_path = proc_entry.path().join("fdinfo").join(fd.file_name());
            let Ok(info) = fs::read_to_string(info_path) else { continue };
            let mut driver_ok = false;
            let mut client = None;
            let mut resident = 0u64;
            let mut total_mem = 0u64;
            for line in info.lines() {
                let Some((key, value)) = line.split_once(':') else { continue };
                let value = value.trim();
                let parse = |v: &str| {
                    let mut it = v.split_whitespace();
                    let n = it.next()?.parse::<u64>().ok()?;
                    Some(match it.next() {
                        Some("MiB") => n * 1024,
                        Some("GiB") => n * 1024 * 1024,
                        Some("KiB") => n,
                        _ => n / 1024,
                    })
                };
                match key {
                    "drm-driver" => driver_ok = value == driver,
                    "drm-client-id" => client = value.parse::<u64>().ok(),
                    k if k.starts_with("drm-resident-") => resident += parse(value).unwrap_or(0),
                    k if k.starts_with("drm-total-") => total_mem += parse(value).unwrap_or(0),
                    _ => {}
                }
            }
            if !driver_ok {
                continue;
            }
            if let Some(id) = client {
                if !seen.insert(id) {
                    continue;
                }
            }
            any = true;
            total += if resident > 0 { resident } else { total_mem };
        }
    }
    any.then_some(total)
}

/// Parses a devfreq `trans_stat` table into (frequency Hz, residency ms) rows.
fn read_devfreq_residency(devfreq: &Path) -> Option<Vec<(u64, u64)>> {
    let text = fs::read_to_string(devfreq.join("trans_stat")).ok()?;
    let rows: Vec<(u64, u64)> = text
        .lines()
        .filter_map(|line| {
            let (head, rest) = line.split_once(':')?;
            let hz = head.trim().trim_start_matches('*').trim().parse::<u64>().ok()?;
            let ms = rest.split_whitespace().last()?.parse::<u64>().ok()?;
            Some((hz, ms))
        })
        .collect();
    (!rows.is_empty()).then_some(rows)
}

/// Load estimate from devfreq residency since the previous sample: where the
/// time-weighted clock sat between the lowest and highest p-state. The
/// ondemand governor tracks GPU busy time, so this follows real load without
/// needing driver-specific busy counters (PowerVR exposes none).
fn devfreq_load(prev: Option<&Vec<(u64, u64)>>, now: &[(u64, u64)]) -> Option<f32> {
    let min = now.iter().map(|r| r.0).min()?;
    let max = now.iter().map(|r| r.0).max()?;
    if max <= min {
        return None;
    }
    let prev = prev?;
    let mut weighted = 0f64;
    let mut total = 0f64;
    for &(hz, ms) in now {
        let before = prev.iter().find(|r| r.0 == hz).map_or(0, |r| r.1);
        let dt = ms.saturating_sub(before) as f64;
        weighted += dt * hz as f64;
        total += dt;
    }
    if total <= 0.0 {
        return Some(0.0);
    }
    let avg = weighted / total;
    Some((((avg - min as f64) / (max - min) as f64) * 100.0).clamp(0.0, 100.0) as f32)
}

pub(crate) fn read_gpu_info(
    prev_residency: &mut HashMap<String, Vec<(u64, u64)>>,
) -> Vec<(String, Option<f32>)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/drm") else { return out };
    let mut cards: Vec<_> = entries
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.starts_with("card") && !n.contains('-')
        })
        .map(|e| e.path())
        .collect();
    cards.sort();
    for card in cards {
        let dev = card.join("device");
        let driver = fs::read_link(dev.join("driver"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_default();
        // Display-only controllers are not GPUs.
        if driver.is_empty() || driver.contains("adp") || driver.contains("display") || driver.contains("adbe") {
            continue;
        }
        let mut parts = vec![match driver.as_str() {
            "powervr" => "PowerVR".to_string(),
            "amdgpu" => "AMD".to_string(),
            "i915" | "xe" => "Intel".to_string(),
            "nvidia" | "nouveau" => "NVIDIA".to_string(),
            "asahi" => "Apple AGX".to_string(),
            other => other.to_string(),
        }];
        let devfreq_dir = fs::read_dir(dev.join("devfreq"))
            .ok()
            .and_then(|mut d| d.next())
            .and_then(|e| e.ok())
            .map(|e| e.path());
        let mut load = None;
        if let Some(dir) = devfreq_dir.as_deref() {
            if let Some(hz) = fs::read_to_string(dir.join("cur_freq"))
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
            {
                parts.push(format!("{} MHz", hz / 1_000_000));
            }
            if let Some(now) = read_devfreq_residency(dir) {
                let key = card.to_string_lossy().to_string();
                load = devfreq_load(prev_residency.get(&key), &now);
                prev_residency.insert(key, now);
            }
        }
        let busy = fs::read_to_string(dev.join("gpu_busy_percent"))
            .ok()
            .and_then(|v| v.trim().parse::<f32>().ok());
        if busy.is_some() {
            load = busy;
        }
        let vram = |name: &str| {
            fs::read_to_string(dev.join(name)).ok().and_then(|v| v.trim().parse::<u64>().ok())
        };
        if let (Some(used), Some(total)) = (vram("mem_info_vram_used"), vram("mem_info_vram_total")) {
            parts.push(format!("VRAM {} / {}", format_gb(used / 1024), format_gb(total / 1024)));
        } else if let Some(kib) = read_drm_client_memory_kib(&driver).filter(|&k| k >= 1024) {
            parts.push(format!("{:.0} MB shared", kib as f64 / 1024.0));
        } else {
            parts.push("shared RAM".to_string());
        }
        out.push((parts.join("  ·  "), load));
    }
    out
}

pub(crate) fn format_cpu_clock(mhz: u32) -> String {
    if mhz >= 1000 {
        format!("{:.2} GHz", f64::from(mhz) / 1000.0)
    } else {
        format!("{mhz} MHz")
    }
}
