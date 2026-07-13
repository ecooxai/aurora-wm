//! Directory listing, places, and file-kind classification.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Directory,
    Text,
    Image,
    Audio,
    Video,
    Pdf,
    Doc,
    Model3d,
    Other,
}

impl FileKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Directory => "Folder",
            Self::Text => "Text",
            Self::Image => "Image",
            Self::Audio => "Audio",
            Self::Video => "Video",
            Self::Pdf => "PDF",
            Self::Doc => "Document",
            Self::Model3d => "3D model",
            Self::Other => "File",
        }
    }
}

#[derive(Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub kind: FileKind,
    pub size: u64,
    pub modified: SystemTime,
}

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn file_kind_for(path: &Path) -> FileKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "md" | "rs" | "toml" | "json" | "yaml" | "yml" | "log" | "conf" | "ini"
        | "csv" | "html" | "css" | "js" | "ts" | "sh" | "py" | "c" | "h" | "cpp" | "xml"
        | "desktop" | "svg" => FileKind::Text,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => FileKind::Image,
        "mp3" | "flac" | "ogg" | "wav" | "m4a" | "aac" | "opus" => FileKind::Audio,
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" => FileKind::Video,
        "pdf" => FileKind::Pdf,
        "doc" | "docx" | "odt" | "rtf" => FileKind::Doc,
        "obj" | "stl" => FileKind::Model3d,
        _ => FileKind::Other,
    }
}

pub fn list_dir(path: &Path, show_hidden: bool) -> Vec<Entry> {
    let mut entries = Vec::new();
    let Ok(read_dir) = fs::read_dir(path) else {
        return entries;
    };
    for item in read_dir.flatten() {
        let name = item.file_name().to_string_lossy().to_string();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let entry_path = item.path();
        let meta = item.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        entries.push(Entry {
            name,
            kind: if is_dir {
                FileKind::Directory
            } else {
                file_kind_for(&entry_path)
            },
            size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
            modified: meta
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH),
            path: entry_path,
        });
        if entries.len() >= 2048 {
            break;
        }
    }
    entries.sort_by(|a, b| {
        (a.kind != FileKind::Directory, a.name.to_lowercase())
            .cmp(&(b.kind != FileKind::Directory, b.name.to_lowercase()))
    });
    entries
}

pub struct Place {
    pub name: String,
    pub path: PathBuf,
}

/// File that stores user-pinned sidebar folders, one absolute path per line.
fn pinned_file() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
        .join("aurora-files/pinned")
}

/// Pin a folder to the sidebar (persisted across restarts).
pub fn add_pinned(path: &Path) {
    let file = pinned_file();
    let mut lines: Vec<String> = fs::read_to_string(&file)
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default();
    let entry = path.to_string_lossy().into_owned();
    if lines.iter().any(|line| line == &entry) {
        return;
    }
    lines.push(entry);
    if let Some(parent) = file.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&file, lines.join("\n") + "\n");
}

pub fn places() -> Vec<Place> {
    let home = home_dir();
    let mut out = vec![
        Place { name: "Home".into(), path: home.clone() },
        Place { name: "Desktop".into(), path: home.join("Desktop") },
        Place { name: "Documents".into(), path: home.join("Documents") },
        Place { name: "Downloads".into(), path: home.join("Downloads") },
        Place { name: "Pictures".into(), path: home.join("Pictures") },
        Place { name: "Music".into(), path: home.join("Music") },
        Place { name: "Videos".into(), path: home.join("Videos") },
        Place { name: "Root /".into(), path: PathBuf::from("/") },
    ];
    for base in ["/mnt", "/media"] {
        if let Ok(entries) = fs::read_dir(base) {
            for entry in entries.flatten().take(4) {
                if entry.path().is_dir() {
                    out.push(Place {
                        name: format!("{base}/{}", entry.file_name().to_string_lossy()),
                        path: entry.path(),
                    });
                }
            }
        }
    }
    // User-pinned folders.
    if let Ok(text) = fs::read_to_string(pinned_file()) {
        for line in text.lines().filter(|line| !line.trim().is_empty()).take(12) {
            let path = PathBuf::from(line);
            if path.is_dir() && !out.iter().any(|place| place.path == path) {
                out.push(Place {
                    name: path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "/".into()),
                    path,
                });
            }
        }
    }
    out
}

pub fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}
