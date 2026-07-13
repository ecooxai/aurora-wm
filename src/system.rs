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
    let mut out = Vec::new();
    for nic in read_nics() {
        let state = fs::read_to_string(format!("/sys/class/net/{nic}/operstate"))
            .unwrap_or_default()
            .trim()
            .to_string();
        out.push(format!("{nic}  {state}"));
        if let Ok(output) = Command::new("ip")
            .args(["-o", "addr", "show", "dev", &nic])
            .output()
        {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let parts = line.split_whitespace().collect::<Vec<_>>();
                if parts.len() > 3 && (parts[2] == "inet" || parts[2] == "inet6") {
                    out.push(format!("{} {}", parts[2], parts[3]));
                }
            }
        }
    }
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
            "terminal={}\nbrowser={}\nphoto={}\nvideo={}\nsleep_after_secs={}\nbrightness_percent={}\ncompositor_enabled={}\nauto_power_saver_enabled={}\nauto_power_saver_minutes={}\nshortcut_folder={}\nshortcut_terminal={}\nshortcut_clipboard={}\nshortcut_screenshot={}\n",
            clean(&settings.terminal_command),
            clean(&settings.browser_command),
            clean(&settings.photo_command),
            clean(&settings.video_command),
            settings.sleep_after_secs.min(7200),
            settings.brightness_percent.clamp(10, 100),
            u8::from(settings.compositor_enabled),
            u8::from(settings.auto_power_saver_enabled),
            settings
                .auto_power_saver_minutes
                .clamp(AUTO_POWER_SAVER_MIN_MINUTES, AUTO_POWER_SAVER_MAX_MINUTES),
            shortcut_setting_string(settings.shortcuts.folder),
            shortcut_setting_string(settings.shortcuts.terminal),
            shortcut_setting_string(settings.shortcuts.clipboard),
            shortcut_setting_string(settings.shortcuts.screenshot),
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

pub(crate) fn read_desktop_entries() -> Vec<DesktopEntry> {
    let mut entries = Vec::new();
    let mut dirs = vec![
        home_dir().join(".local/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
        PathBuf::from("/usr/share/applications"),
    ];
    dirs.dedup();
    for dir in dirs {
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
