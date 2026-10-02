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
        let meta = fs::metadata(&entry_path).ok();
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
        Place { name: "Trash".into(), path: trash_dir().join("files") },
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

/// Copies directories recursively without following symlinks or replacing any target.
pub fn copy_entry_no_replace(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        let source = fs::canonicalize(src)?;
        let parent = fs::canonicalize(dst.parent().unwrap_or(Path::new("/")))?;
        if parent.starts_with(&source) { return Err(std::io::Error::other("cannot copy a folder into itself")); }
        fs::create_dir(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_entry_no_replace(&entry.path(), &dst.join(entry.file_name()))?;
        }
        fs::set_permissions(dst, meta.permissions())?;
    } else if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
    } else if meta.is_file() {
        let mut input = fs::File::open(src)?;
        let mut output = fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
        std::io::copy(&mut input, &mut output)?;
        fs::set_permissions(dst, meta.permissions())?;
    } else { return Err(std::io::Error::other("special files cannot be copied")); }
    Ok(())
}

#[cfg(test)]
mod copy_tests {
    use super::*;
    #[test]
    fn recursive_copy_preserves_symlinks_and_refuses_overwrite_and_descendant() {
        let root = std::env::temp_dir().join(format!("aurora-copy-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::write(root.join("src/nested/data.txt"), b"original").unwrap();
        std::os::unix::fs::symlink("nested/data.txt", root.join("src/link")).unwrap();
        assert!(copy_entry_no_replace(&root.join("src"), &root.join("src/inside")).is_err());
        assert!(!root.join("src/inside").exists());
        copy_entry_no_replace(&root.join("src"), &root.join("copy")).unwrap();
        assert_eq!(fs::read(root.join("copy/nested/data.txt")).unwrap(), b"original");
        assert_eq!(fs::read_link(root.join("copy/link")).unwrap(), PathBuf::from("nested/data.txt"));
        assert!(copy_entry_no_replace(&root.join("src/nested/data.txt"), &root.join("copy/nested/data.txt")).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}

/// Use the user's data directory; never remove file data permanently.
pub fn trash_dir() -> PathBuf {
    env::var_os("XDG_DATA_HOME").map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/share")).join("Trash")
}

pub fn move_to_trash(source: &Path) -> std::io::Result<()> {
    trash_into(source, &trash_dir())
}

fn trash_into(source: &Path, trash: &Path) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    let parent = fs::canonicalize(source.parent().ok_or_else(|| std::io::Error::other("cannot trash root"))?)?;
    let name = source.file_name().ok_or_else(|| std::io::Error::other("cannot trash root"))?;
    let absolute = parent.join(name);
    // Canonicalize only the parent so a symlink itself is moved, not its target.
    fs::symlink_metadata(&absolute)?;
    fs::create_dir_all(trash.join("files"))?;
    fs::create_dir_all(trash.join("info"))?;
    let trash_absolute = fs::canonicalize(trash)?;
    if absolute.starts_with(&trash_absolute) || trash_absolute.starts_with(&absolute) {
        return Err(std::io::Error::other("cannot trash the Trash folder or its contents"));
    }
    let mut candidate = name.to_os_string();
    let mut index = 0;
    loop {
        let destination = trash.join("files").join(&candidate);
        let mut info_name = candidate.clone(); info_name.push(".trashinfo");
        let info_path = trash.join("info").join(info_name);
        if fs::symlink_metadata(&destination).is_ok() || fs::symlink_metadata(&info_path).is_ok() {
            index += 1; candidate = name.to_os_string(); candidate.push(format!(".{index}")); continue;
        }
        let mut info = fs::OpenOptions::new().write(true).create_new(true).open(&info_path)?;
        let encoded: String = absolute.as_os_str().as_bytes().iter().map(|&b| {
            if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }
        }).collect();
        let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        let timestamp = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", now.year(), u8::from(now.month()), now.day(), now.hour(), now.minute(), now.second());
        if let Err(err) = write!(info, "[Trash Info]\nPath={encoded}\nDeletionDate={timestamp}\n").and_then(|_| info.flush()).and_then(|_| fs::rename(&absolute, &destination)) {
            let _ = fs::remove_file(&info_path); return Err(err);
        }
        return Ok(());
    }
}

#[cfg(test)]
mod trash_tests {
    use super::*;
    #[test]
    fn trash_keeps_data_and_metadata_and_uniquifies_names() {
        let root = env::temp_dir().join(format!("aurora-trash-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("input")).unwrap();
        let source = root.join("input/a b.txt");
        fs::write(&source, b"one").unwrap();
        trash_into(&source, &root.join("Trash")).unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(root.join("Trash/files/a b.txt")).unwrap(), b"one");
        assert!(fs::read_to_string(root.join("Trash/info/a b.txt.trashinfo")).unwrap().contains("/input/a%20b.txt"));
        fs::write(&source, b"two").unwrap();
        trash_into(&source, &root.join("Trash")).unwrap();
        assert_eq!(fs::read(root.join("Trash/files/a b.txt.1")).unwrap(), b"two");
        assert!(trash_into(&root.join("Trash/files/a b.txt"), &root.join("Trash")).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
