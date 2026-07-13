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
use crate::system::*;
use crate::textutil::*;
use crate::files::*;

pub(crate) fn command_exists(name: &str) -> bool {
    env::var_os("PATH")
        .and_then(|paths| {
            env::split_paths(&paths)
                .map(|path| path.join(name))
                .find(|path| path.exists())
        })
        .is_some()
}

pub(crate) fn command_status_success(cmd: &mut Command) -> bool {
    cmd.status().is_ok_and(|status| status.success())
}

/// Locate the standalone `aurora-files` app: next to the running aurora-wm
/// binary first (dev builds), then on PATH.
pub(crate) fn aurora_files_binary() -> Option<String> {
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("aurora-files");
            if sibling.exists() {
                return Some(sibling.to_string_lossy().into_owned());
            }
        }
    }
    command_exists("aurora-files").then(|| "aurora-files".to_string())
}

pub(crate) fn shell_quote_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

pub(crate) fn pulse_command_output(program: &str, args: &[&str]) -> Option<std::process::Output> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    apply_pulse_env_defaults(&mut cmd);
    cmd.output().ok()
}

pub(crate) fn command_output_timeout(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Option<std::process::Output> {
    let mut cmd = if command_exists("timeout") {
        let mut timeout_cmd = Command::new("timeout");
        timeout_cmd.arg(format!("{:.3}", timeout.as_secs_f64()));
        timeout_cmd.arg(program);
        timeout_cmd
    } else {
        Command::new(program)
    };
    cmd.args(args).stderr(Stdio::null()).output().ok()
}

pub(crate) fn apply_pulse_env_defaults(cmd: &mut Command) {
    let runtime_dir = env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| {
        let uid = unsafe { libc::geteuid() };
        format!("/run/user/{uid}").into()
    });
    cmd.env("XDG_RUNTIME_DIR", &runtime_dir);

    if env::var_os("PULSE_SERVER").is_none() {
        let native = PathBuf::from(&runtime_dir).join("pulse/native");
        let server = format!("unix:{}", native.to_string_lossy());
        cmd.env("PULSE_SERVER", server);
    }
    if env::var_os("PULSE_RUNTIME_PATH").is_none() {
        cmd.env(
            "PULSE_RUNTIME_PATH",
            PathBuf::from(&runtime_dir).join("pulse"),
        );
    }
    if env::var_os("PULSE_COOKIE").is_none() {
        let cookie = home_dir().join(".config/pulse/cookie");
        if cookie.exists() {
            cmd.env("PULSE_COOKIE", cookie);
        }
    }
}

pub(crate) fn spawn_detached(mut cmd: Command) {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Ok(mut child) = cmd.spawn() {
        thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

pub(crate) fn copy_text_to_clipboard(text: &str) {
    for name in ["xclip", "xsel", "wl-copy"] {
        if !command_exists(name) {
            continue;
        }
        let mut cmd = Command::new(name);
        match name {
            "xclip" => {
                cmd.args(["-selection", "clipboard"]);
            }
            "xsel" => {
                cmd.args(["--clipboard", "--input"]);
            }
            _ => {}
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(mut child) = cmd.spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            append_clipboard_history(&ClipboardItem::Text(text.to_string()));
            break;
        }
    }
}

pub(crate) fn read_text_clipboard() -> Option<String> {
    let commands: [(&str, &[&str]); 4] = [
        (
            "xclip",
            &["-selection", "clipboard", "-o", "-target", "UTF8_STRING"],
        ),
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["--clipboard", "--output"]),
        ("wl-paste", &[]),
    ];
    for (name, args) in commands {
        if !command_exists(name) {
            continue;
        }
        if let Some(output) = command_output_timeout(name, args, CLIPBOARD_COMMAND_TIMEOUT) {
            if output.status.success() {
                return Some(String::from_utf8_lossy(&output.stdout).to_string());
            }
        }
    }
    None
}

pub(crate) fn read_image_clipboard() -> Option<(PathBuf, u64)> {
    let target = clipboard_image_target()?;
    let output = command_output_timeout(
        "xclip",
        &["-selection", "clipboard", "-target", target, "-o"],
        CLIPBOARD_COMMAND_TIMEOUT,
    )?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    let sig = clipboard_image_signature(target, &output.stdout);
    let img = image::load_from_memory(&output.stdout).ok()?.to_rgba8();
    let (width, height) = img.dimensions();
    let dir = clipboard_image_history_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("clipboard-{sig:016x}-{width}x{height}.png"));
    if !path.exists()
        && image::save_buffer_with_format(
            &path,
            img.as_raw(),
            width,
            height,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .is_err()
    {
        return None;
    }
    Some((path, sig))
}

pub(crate) fn clipboard_image_target() -> Option<&'static str> {
    if !command_exists("xclip") {
        return None;
    }
    let output = command_output_timeout(
        "xclip",
        &["-selection", "clipboard", "-target", "TARGETS", "-o"],
        CLIPBOARD_COMMAND_TIMEOUT,
    )?;
    if !output.status.success() {
        return None;
    }
    let targets = String::from_utf8_lossy(&output.stdout);
    [
        "image/png",
        "image/jpeg",
        "image/jpg",
        "image/bmp",
        "image/tiff",
    ]
    .into_iter()
    .find(|target| targets.lines().any(|line| line.trim() == *target))
}

pub(crate) fn clipboard_image_signature(target: &str, bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    target.hash(&mut hasher);
    bytes.hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn clipboard_file_image_signature(path: &Path) -> Option<u64> {
    let bytes = fs::read(path).ok()?;
    Some(clipboard_image_signature("image/png", &bytes))
}

pub(crate) fn clipboard_image_history_dir() -> PathBuf {
    if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("aurora-clipboard-images");
    }
    PathBuf::from(format!("/tmp/aurora-clipboard-images-{}", unsafe {
        libc::geteuid()
    }))
}

pub(crate) fn copy_image_to_clipboard(path: &Path) {
    if command_exists("xclip") {
        let copied = Command::new("xclip")
            .args(["-selection", "clipboard", "-target", "image/png", "-i"])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if copied {
            append_clipboard_history(&ClipboardItem::Image(path.to_path_buf()));
        }
    } else {
        copy_text_to_clipboard(&path.to_string_lossy());
    }
}

pub(crate) fn terminal_uses_native_paste_shortcut(window_class: &str) -> bool {
    if window_class.contains("aurora-files") {
        // Aurora Files owns an embedded terminal under the same top-level X11
        // class as its folder view. Its terminal handles Ctrl+Shift+V natively.
        return true;
    }
    window_class
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .any(|part| {
            matches!(
                part,
                "alacritty"
                    | "blackbox"
                    | "console"
                    | "contour"
                    | "coolretroterm"
                    | "foot"
                    | "ghostty"
                    | "gnometerminal"
                    | "hyper"
                    | "kitty"
                    | "konsole"
                    | "lxterminal"
                    | "mateterminal"
                    | "qterminal"
                    | "rio"
                    | "rxvt"
                    | "st"
                    | "tabby"
                    | "terminator"
                    | "terminal"
                    | "tilix"
                    | "urxvt"
                    | "uxterm"
                    | "wezterm"
                    | "xfce4terminal"
                    | "xterm"
            )
        })
}

pub(crate) fn clipboard_history_path() -> PathBuf {
    if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("aurora-clipboard-history");
    }
    PathBuf::from(format!("/tmp/aurora-clipboard-history-{}", unsafe {
        libc::geteuid()
    }))
}

pub(crate) fn append_clipboard_history(item: &ClipboardItem) {
    let path = clipboard_history_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let line = match item {
        ClipboardItem::Text(text) if text.is_empty() || text.len() > 1_000_000 => return,
        ClipboardItem::Text(text) => format!("T\t{}\n", escape_history_field(text)),
        ClipboardItem::Image(path) => {
            format!("I\t{}\n", escape_history_field(&path.to_string_lossy()))
        }
    };
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write;
        let _ = file.write_all(line.as_bytes());
    }
    compact_clipboard_history_store(&path);
}

pub(crate) fn compact_clipboard_history_store(path: &Path) {
    let entries = read_clipboard_history_store_from(path);
    let mut out = String::new();
    for entry in entries.iter().rev() {
        match &entry.item {
            ClipboardItem::Text(text) => {
                out.push_str("T\t");
                out.push_str(&escape_history_field(text));
                out.push('\n');
            }
            ClipboardItem::Image(path) => {
                out.push_str("I\t");
                out.push_str(&escape_history_field(&path.to_string_lossy()));
                out.push('\n');
            }
        }
    }
    let _ = fs::write(path, out);
}

pub(crate) fn read_clipboard_history_store() -> Vec<ClipboardEntry> {
    read_clipboard_history_store_from(&clipboard_history_path())
}

pub(crate) fn read_clipboard_history_store_from(path: &Path) -> Vec<ClipboardEntry> {
    let Ok(data) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut entries: Vec<ClipboardEntry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for line in data.lines().rev() {
        let Some((kind, value)) = line.split_once('\t') else {
            continue;
        };
        let Some(value) = unescape_history_field(value) else {
            continue;
        };
        let item = match kind {
            "T" if !value.is_empty() => ClipboardItem::Text(value),
            "I" => {
                let path = PathBuf::from(value);
                if !path.exists() {
                    continue;
                }
                ClipboardItem::Image(path)
            }
            _ => continue,
        };
        let key = clipboard_item_key(&item);
        if !seen.insert(key) {
            continue;
        }
        entries.push(ClipboardEntry { item });
        if entries.len() >= CLIPBOARD_HISTORY_LIMIT {
            break;
        }
    }
    entries
}

pub(crate) fn clipboard_item_key(item: &ClipboardItem) -> String {
    match item {
        ClipboardItem::Text(text) => {
            let mut key = String::with_capacity(text.len() + 2);
            key.push_str("T\t");
            key.push_str(text);
            key
        }
        ClipboardItem::Image(path) => {
            let value = path.to_string_lossy();
            let mut key = String::with_capacity(value.len() + 2);
            key.push_str("I\t");
            key.push_str(&value);
            key
        }
    }
}

pub(crate) fn clipboard_items_match(a: &ClipboardItem, b: &ClipboardItem) -> bool {
    match (a, b) {
        (ClipboardItem::Text(left), ClipboardItem::Text(right)) => left == right,
        (ClipboardItem::Image(left), ClipboardItem::Image(right)) => left == right,
        _ => false,
    }
}

pub(crate) fn escape_history_field(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'%' | b'\t' | b'\n' | b'\r' => out.push_str(&format!("%{byte:02X}")),
            0x20..=0x7e => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub(crate) fn unescape_history_field(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

pub(crate) fn move_to_trash(path: &Path) -> AnyResult<()> {
    let trash_files = home_dir().join(".local/share/Trash/files");
    let trash_info = home_dir().join(".local/share/Trash/info");
    fs::create_dir_all(&trash_files)?;
    fs::create_dir_all(&trash_info)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("item");
    let mut dst = trash_files.join(name);
    if dst.exists() {
        let stamp = OffsetDateTime::now_utc().unix_timestamp();
        dst = trash_files.join(format!("{stamp}-{name}"));
    }
    fs::rename(path, &dst)?;
    let info_name = dst.file_name().and_then(|n| n.to_str()).unwrap_or(name);
    let deletion = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string());
    let info = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        path.to_string_lossy(),
        deletion
    );
    fs::write(trash_info.join(format!("{info_name}.trashinfo")), info)?;
    Ok(())
}

pub(crate) fn file_uri(path: &std::path::Path) -> String {
    let mut out = String::from("file://");
    for ch in path.to_string_lossy().chars() {
        match ch {
            ' ' => out.push_str("%20"),
            '#' => out.push_str("%23"),
            '%' => out.push_str("%25"),
            '\n' | '\r' => {}
            _ => out.push(ch),
        }
    }
    out
}

pub(crate) fn path_from_file_uri(uri: &str) -> Option<PathBuf> {
    let raw = uri.strip_prefix("file://")?;
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            let a = chars.next()?;
            let b = chars.next()?;
            let hex = format!("{a}{b}");
            if let Ok(v) = u8::from_str_radix(&hex, 16) {
                out.push(v as char);
            }
        } else {
            out.push(ch);
        }
    }
    Some(PathBuf::from(out))
}

#[cfg(test)]
mod paste_tests {
    use super::terminal_uses_native_paste_shortcut;

    #[test]
    fn recognizes_terminal_window_classes() {
        for class in [
            "gnome-terminal-server\0Gnome-terminal",
            "kitty\0kitty",
            "org.gnome.Console\0org.gnome.Console",
            "st\0St",
            "Alacritty\0Alacritty",
            "aurora-files\0Aurora Files",
        ] {
            assert!(
                terminal_uses_native_paste_shortcut(&class.to_ascii_lowercase()),
                "expected terminal class: {class:?}"
            );
        }
    }

    #[test]
    fn keeps_standard_apps_on_standard_paste() {
        for class in [
            "firefox\0Firefox",
            "google-chrome\0Google-chrome",
            "org.gnome.Nautilus\0org.gnome.Nautilus",
            "code\0Code",
        ] {
            assert!(
                !terminal_uses_native_paste_shortcut(&class.to_ascii_lowercase()),
                "expected standard app class: {class:?}"
            );
        }
    }
}
