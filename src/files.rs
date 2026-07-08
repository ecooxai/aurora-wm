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
use crate::procutil::*;

pub(crate) fn app_menu_items() -> Vec<AppMenuItem> {
    vec![
        AppMenuItem {
            label: "Terminal",
            hint: "Open shell",
            action: AppAction::Terminal,
        },
        AppMenuItem {
            label: "Browser",
            hint: "Launch web browser",
            action: AppAction::Browser,
        },
        AppMenuItem {
            label: "Pictures",
            hint: "Browse images",
            action: AppAction::Pictures,
        },
        AppMenuItem {
            label: "Music",
            hint: "Browse audio",
            action: AppAction::Music,
        },
        AppMenuItem {
            label: "Videos",
            hint: "Browse movies",
            action: AppAction::Videos,
        },
        AppMenuItem {
            label: "Settings",
            hint: "Display and power",
            action: AppAction::Settings,
        },
        AppMenuItem {
            label: "More",
            hint: "All desktop apps",
            action: AppAction::More,
        },
    ]
}

pub(crate) fn folder_entries_for(mode: FolderMode, sort: FolderSort) -> Vec<FolderEntry> {
    let home = home_dir();
    let path = folder_path_for(mode);
    let mut entries = folder_entries_in(path, sort);
    if entries.is_empty() && mode == FolderMode::Home {
        for (name, mode) in [
            ("Pictures", FolderMode::Pictures),
            ("Music", FolderMode::Music),
            ("Videos", FolderMode::Videos),
        ] {
            entries.push(FolderEntry {
                name: name.to_string(),
                path: home.join(mode.title()),
                kind: FileKind::Directory,
            });
        }
        sort_folder_entries(&mut entries, sort);
    }
    entries
}

pub(crate) fn folder_path_for(mode: FolderMode) -> PathBuf {
    let home = home_dir();
    match mode {
        FolderMode::Home => home,
        FolderMode::Pictures => home.join("Pictures"),
        FolderMode::Music => home.join("Music"),
        FolderMode::Videos => home.join("Videos"),
    }
}

pub(crate) fn folder_entries_in(path: PathBuf, sort: FolderSort) -> Vec<FolderEntry> {
    let mut entries = Vec::new();
    let mut other_count = 0usize;
    let Ok(read_dir) = fs::read_dir(&path) else {
        return entries;
    };
    for entry in read_dir.flatten() {
        let entry_path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let kind = if entry.file_type().is_ok_and(|ty| ty.is_dir()) {
            FileKind::Directory
        } else {
            file_kind_for(&entry_path)
        };
        if kind == FileKind::Other {
            if other_count >= FOLDER_OTHER_ENTRY_LIMIT {
                continue;
            }
            other_count += 1;
        }
        entries.push(FolderEntry {
            name,
            path: entry_path,
            kind,
        });
        if entries.len() >= FOLDER_ENTRY_LIMIT {
            break;
        }
    }
    sort_folder_entries(&mut entries, sort);
    entries
}

pub(crate) fn sort_folder_entries(entries: &mut [FolderEntry], sort: FolderSort) {
    entries.sort_by(|a, b| {
        let base = (a.kind != FileKind::Directory)
            .cmp(&(b.kind != FileKind::Directory))
            .then((a.kind == FileKind::Other).cmp(&(b.kind == FileKind::Other)));
        if base != std::cmp::Ordering::Equal {
            return base;
        }
        match sort {
            FolderSort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            FolderSort::Date => entry_modified_secs(b)
                .cmp(&entry_modified_secs(a))
                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase())),
            FolderSort::Size => entry_size(b)
                .cmp(&entry_size(a))
                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase())),
        }
    });
}

pub(crate) fn entry_modified_secs(entry: &FolderEntry) -> u64 {
    fs::metadata(&entry.path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub(crate) fn entry_size(entry: &FolderEntry) -> u64 {
    fs::metadata(&entry.path)
        .map(|meta| meta.len())
        .unwrap_or(0)
}

pub(crate) fn spawn_terminal_pty(cwd: &Path, cols: usize, rows: usize) -> AnyResult<(RawFd, libc::pid_t)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut winsize = libc::winsize {
        ws_row: rows as u16,
        ws_col: cols as u16,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &mut winsize,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        return Err(std::io::Error::last_os_error().into());
    }
    if pid == 0 {
        unsafe {
            libc::close(master);
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY, 0);
            libc::dup2(slave, libc::STDIN_FILENO);
            libc::dup2(slave, libc::STDOUT_FILENO);
            libc::dup2(slave, libc::STDERR_FILENO);
            if slave > libc::STDERR_FILENO {
                libc::close(slave);
            }
        }
        let _ = env::set_current_dir(cwd);
        unsafe {
            env::set_var("TERM", "xterm-256color");
            env::set_var("COLORTERM", "truecolor");
            env::set_var("LINES", rows.to_string());
            env::set_var("COLUMNS", cols.to_string());
            env::set_var("PS1", "$ ");
            env::set_var("ENV", "/dev/null");
            env::set_var("BASH_ENV", "/dev/null");
        }
        let shell_c = CString::new("/bin/bash").unwrap();
        let arg1 = CString::new("--norc").unwrap();
        let arg2 = CString::new("--noprofile").unwrap();
        unsafe {
            libc::execlp(
                shell_c.as_ptr(),
                shell_c.as_ptr(),
                arg1.as_ptr(),
                arg2.as_ptr(),
                std::ptr::null::<libc::c_char>(),
            );
            libc::_exit(127);
        }
    }
    unsafe {
        libc::close(slave);
        let flags = libc::fcntl(master, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    Ok((master, pid))
}

pub(crate) fn shell_quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    format!("'{}'", text.replace('\'', "'\\''"))
}

pub(crate) fn file_kind_for(path: &std::path::Path) -> FileKind {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "md" | "rs" | "toml" | "json" | "yaml" | "yml" | "log" | "conf" | "ini" | "csv"
        | "html" | "css" | "js" | "ts" | "sh" => FileKind::Text,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => FileKind::Image,
        "mp3" | "flac" | "ogg" | "wav" | "m4a" | "aac" => FileKind::Audio,
        "mp4" | "mkv" | "webm" | "mov" | "avi" => FileKind::Video,
        _ => FileKind::Other,
    }
}

pub(crate) fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub(crate) fn place_entries() -> Vec<PlaceEntry> {
    let home = home_dir();
    let mut places = vec![
        PlaceEntry {
            name: "Home".to_string(),
            path: home.clone(),
        },
        PlaceEntry {
            name: "Downloads".to_string(),
            path: home.join("Downloads"),
        },
        PlaceEntry {
            name: "Documents".to_string(),
            path: home.join("Documents"),
        },
        PlaceEntry {
            name: "Trash".to_string(),
            path: home.join(".local/share/Trash/files"),
        },
        PlaceEntry {
            name: "Root /".to_string(),
            path: PathBuf::from("/"),
        },
    ];
    if let Ok(entries) = fs::read_dir("/mnt") {
        for entry in entries.flatten().take(4) {
            let path = entry.path();
            if path.is_dir() {
                places.push(PlaceEntry {
                    name: format!("/mnt/{}", entry.file_name().to_string_lossy()),
                    path,
                });
            }
        }
    }
    if let Ok(entries) = fs::read_dir("/media") {
        for entry in entries.flatten().take(4) {
            let path = entry.path();
            if path.is_dir() {
                places.push(PlaceEntry {
                    name: format!("/media/{}", entry.file_name().to_string_lossy()),
                    path,
                });
            }
        }
    }
    places
}

pub(crate) fn compact_path(path: &std::path::Path, max_chars: usize) -> String {
    let text = path.to_string_lossy();
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let tail = text
            .chars()
            .rev()
            .take(max_chars.saturating_sub(3))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        format!("...{tail}")
    }
}

pub(crate) fn draw_reload_menu_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_arc(c, cx, cy, 6, 40.0, 320.0, 0, 2, color);
    let tip_x = cx + 4;
    let tip_y = cy + 4;
    draw_round_line(c, tip_x, tip_y, tip_x - 4, tip_y, 2, color);
    draw_round_line(c, tip_x, tip_y, tip_x, tip_y - 4, 2, color);
}

pub(crate) fn draw_info_menu_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_arc(c, cx, cy, 7, 0.0, 359.9, 0, 2, color);
    c.draw_circle(cx, cy - 3, 1, color);
    draw_round_line(c, cx, cy - 1, cx, cy + 3, 2, color);
}
