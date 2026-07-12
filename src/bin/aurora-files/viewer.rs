//! File viewers/editors: text (editable), images, PDF pages (pdftoppm),
//! office documents (text extraction), 3D wireframes (OBJ/STL), and
//! audio/video playback embedded via mpv (ffplay fallback).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::fsmodel::FileKind;

pub enum Viewer {
    Text(TextView),
    Image(ImageView),
    Pdf(PdfView),
    Model(ModelView),
    Media(MediaView),
    Info(String),
}

// ------------------------------------------------------------------ text

pub struct TextView {
    pub path: PathBuf,
    pub lines: Vec<String>,
    pub cursor: (usize, usize), // (line, col in chars)
    pub scroll: usize,
    pub dirty: bool,
    pub editable: bool,
    pub status: String,
}

impl TextView {
    pub fn open(path: &Path) -> Self {
        let content = fs::read_to_string(path).unwrap_or_default();
        let mut lines: Vec<String> = content.split('\n').map(str::to_string).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Self {
            path: path.to_path_buf(),
            lines,
            cursor: (0, 0),
            scroll: 0,
            dirty: false,
            editable: true,
            status: String::new(),
        }
    }

    pub fn from_text(path: &Path, text: String) -> Self {
        let mut view = Self::open(Path::new("/dev/null"));
        view.path = path.to_path_buf();
        view.lines = text.split('\n').map(str::to_string).collect();
        view.editable = false;
        view
    }

    pub fn save(&mut self) {
        if !self.editable {
            return;
        }
        match fs::write(&self.path, self.lines.join("\n")) {
            Ok(()) => {
                self.dirty = false;
                self.status = "Saved".to_string();
            }
            Err(err) => self.status = format!("Save failed: {err}"),
        }
    }

    pub fn insert_char(&mut self, ch: char) {
        if !self.editable {
            return;
        }
        let (line, col) = self.cursor;
        let line_text = &mut self.lines[line];
        let byte = char_to_byte(line_text, col);
        line_text.insert(byte, ch);
        self.cursor.1 += 1;
        self.dirty = true;
    }

    pub fn newline(&mut self) {
        if !self.editable {
            return;
        }
        let (line, col) = self.cursor;
        let byte = char_to_byte(&self.lines[line], col);
        let rest = self.lines[line].split_off(byte);
        self.lines.insert(line + 1, rest);
        self.cursor = (line + 1, 0);
        self.dirty = true;
    }

    pub fn backspace(&mut self) {
        if !self.editable {
            return;
        }
        let (line, col) = self.cursor;
        if col > 0 {
            let byte = char_to_byte(&self.lines[line], col - 1);
            self.lines[line].remove(byte);
            self.cursor.1 -= 1;
            self.dirty = true;
        } else if line > 0 {
            let prev_len = self.lines[line - 1].chars().count();
            let current = self.lines.remove(line);
            self.lines[line - 1].push_str(&current);
            self.cursor = (line - 1, prev_len);
            self.dirty = true;
        }
    }

    pub fn move_cursor(&mut self, dx: i32, dy: i32) {
        let (mut line, mut col) = self.cursor;
        if dy < 0 {
            line = line.saturating_sub((-dy) as usize);
        } else {
            line = (line + dy as usize).min(self.lines.len() - 1);
        }
        let max_col = self.lines[line].chars().count();
        if dx < 0 {
            col = col.saturating_sub((-dx) as usize);
        } else if dx > 0 {
            col += dx as usize;
        }
        col = col.min(max_col);
        self.cursor = (line, col);
    }
}

fn char_to_byte(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map(|(i, _)| i)
        .unwrap_or(line.len())
}

// ------------------------------------------------------------------ image

pub struct ImageView {
    pub path: PathBuf,
    pub pixels: Vec<u8>, // RGBA
    pub width: u32,
    pub height: u32,
    /// Native resolution of the image on disk (before fit-to-view scaling).
    pub orig_width: u32,
    pub orig_height: u32,
    /// User zoom factor applied on top of the fit-to-view size.
    pub zoom: f32,
    pub error: Option<String>,
}

impl ImageView {
    pub fn open(path: &Path, max_w: u32, max_h: u32) -> Self {
        Self::open_zoomed(path, max_w, max_h, 1.0)
    }

    pub fn open_zoomed(path: &Path, max_w: u32, max_h: u32, zoom: f32) -> Self {
        match fs::read(path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| image::load_from_memory(&bytes).map_err(|e| e.to_string()))
        {
            Ok(img) => {
                let img = img.to_rgba8();
                let (iw, ih) = img.dimensions();
                let fit = (max_w as f32 / iw as f32)
                    .min(max_h as f32 / ih as f32)
                    .min(1.0);
                let scale = (fit * zoom.max(0.05)).max(0.001);
                let nw = ((iw as f32 * scale) as u32).max(1);
                let nh = ((ih as f32 * scale) as u32).max(1);
                // Lanczos3 gives noticeably sharper downscaling than Triangle.
                let resized =
                    image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3);
                Self {
                    path: path.to_path_buf(),
                    pixels: resized.into_raw(),
                    width: nw,
                    height: nh,
                    orig_width: iw,
                    orig_height: ih,
                    zoom,
                    error: None,
                }
            }
            Err(err) => Self {
                path: path.to_path_buf(),
                pixels: Vec::new(),
                width: 0,
                height: 0,
                orig_width: 0,
                orig_height: 0,
                zoom,
                error: Some(err),
            },
        }
    }
}

// ------------------------------------------------------------------ pdf

pub struct PdfView {
    pub path: PathBuf,
    pub page: u32,
    pub pages: u32,
    pub image: Option<ImageView>,
    pub error: Option<String>,
}

impl PdfView {
    pub fn open(path: &Path, max_w: u32, max_h: u32) -> Self {
        let pages = Command::new("pdfinfo")
            .arg(path)
            .output()
            .ok()
            .and_then(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .find(|l| l.starts_with("Pages:"))
                    .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
            })
            .unwrap_or(1);
        let mut view = Self {
            path: path.to_path_buf(),
            page: 1,
            pages,
            image: None,
            error: None,
        };
        view.render(max_w, max_h);
        view
    }

    pub fn render(&mut self, max_w: u32, max_h: u32) {
        let out_base = format!("/tmp/aurora-files-pdf-{}", std::process::id());
        let status = Command::new("pdftoppm")
            .args(["-png", "-r", "110", "-f"])
            .arg(self.page.to_string())
            .arg("-l")
            .arg(self.page.to_string())
            .arg(&self.path)
            .arg(&out_base)
            .stderr(Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => {
                // pdftoppm names output base-N.png with zero padding variants.
                let mut found = None;
                for candidate in [
                    format!("{out_base}-{}.png", self.page),
                    format!("{out_base}-{:02}.png", self.page),
                    format!("{out_base}-{:03}.png", self.page),
                ] {
                    if Path::new(&candidate).exists() {
                        found = Some(candidate);
                        break;
                    }
                }
                match found {
                    Some(png) => {
                        self.image = Some(ImageView::open(Path::new(&png), max_w, max_h));
                        let _ = fs::remove_file(&png);
                        self.error = None;
                    }
                    None => self.error = Some("pdftoppm produced no page image".into()),
                }
            }
            Ok(_) => self.error = Some("pdftoppm failed to render this PDF".into()),
            Err(_) => {
                self.error =
                    Some("PDF preview needs 'pdftoppm' (poppler-utils); not installed".into())
            }
        }
    }
}

// ------------------------------------------------------------------ office docs

pub fn extract_doc_text(path: &Path) -> Result<String, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let unzip_entry = |entry: &str| -> Result<String, String> {
        let out = Command::new("unzip")
            .args(["-p"])
            .arg(path)
            .arg(entry)
            .stderr(Stdio::null())
            .output()
            .map_err(|e| format!("unzip unavailable: {e}"))?;
        if !out.status.success() {
            return Err("could not read document archive".into());
        }
        Ok(strip_xml_tags(&String::from_utf8_lossy(&out.stdout)))
    };
    match ext.as_str() {
        "docx" => unzip_entry("word/document.xml"),
        "odt" => unzip_entry("content.xml"),
        "rtf" => fs::read_to_string(path)
            .map(|text| strip_rtf(&text))
            .map_err(|e| e.to_string()),
        "doc" => {
            for tool in ["antiword", "catdoc"] {
                if let Ok(out) = Command::new(tool).arg(path).stderr(Stdio::null()).output() {
                    if out.status.success() {
                        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
                    }
                }
            }
            Err("Legacy .doc preview needs 'antiword' or 'catdoc' installed".into())
        }
        _ => Err("Unsupported document format".into()),
    }
}

fn strip_xml_tags(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len() / 2);
    let mut in_tag = false;
    let mut chars = xml.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '<' => {
                in_tag = true;
                // Paragraph-ish closers become newlines.
                let tag: String = chars.clone().take_while(|&c| c != '>').collect();
                if tag.starts_with("/w:p") || tag.starts_with("/text:p") || tag.starts_with("w:br")
                {
                    out.push('\n');
                }
            }
            '>' => in_tag = false,
            ch if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn strip_rtf(rtf: &str) -> String {
    let mut out = String::new();
    let mut chars = rtf.chars().peekable();
    let mut depth = 0;
    while let Some(ch) = chars.next() {
        match ch {
            '{' => depth += 1,
            '}' => depth -= 1,
            '\\' => {
                let word: String = chars
                    .clone()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                for _ in 0..word.len() {
                    chars.next();
                }
                if word == "par" || word == "line" {
                    out.push('\n');
                }
                if chars.peek() == Some(&' ') {
                    chars.next();
                }
            }
            ch if depth <= 2 && !ch.is_control() => out.push(ch),
            _ => {}
        }
    }
    out
}

// ------------------------------------------------------------------ 3D models

pub struct ModelView {
    pub path: PathBuf,
    pub vertices: Vec<[f32; 3]>,
    pub edges: Vec<(u32, u32)>,
    pub yaw: f32,
    pub pitch: f32,
    pub error: Option<String>,
    pub dragging: Option<(i16, i16)>,
}

impl ModelView {
    pub fn open(path: &Path) -> Self {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let result = match ext.as_str() {
            "obj" => parse_obj(path),
            "stl" => parse_stl(path),
            _ => Err("unsupported model format".into()),
        };
        match result {
            Ok((vertices, edges)) => Self {
                path: path.to_path_buf(),
                vertices,
                edges,
                yaw: 0.6,
                pitch: 0.35,
                error: None,
                dragging: None,
            },
            Err(err) => Self {
                path: path.to_path_buf(),
                vertices: Vec::new(),
                edges: Vec::new(),
                yaw: 0.0,
                pitch: 0.0,
                error: Some(err),
                dragging: None,
            },
        }
    }

    /// Project vertices into screen space for the given viewport.
    pub fn project(&self, w: i32, h: i32) -> Vec<(i32, i32)> {
        if self.vertices.is_empty() {
            return Vec::new();
        }
        // Normalize model into unit cube around origin.
        let (mut min, mut max) = ([f32::MAX; 3], [f32::MIN; 3]);
        for v in &self.vertices {
            for i in 0..3 {
                min[i] = min[i].min(v[i]);
                max[i] = max[i].max(v[i]);
            }
        }
        let center = [
            (min[0] + max[0]) / 2.0,
            (min[1] + max[1]) / 2.0,
            (min[2] + max[2]) / 2.0,
        ];
        let extent = (0..3)
            .map(|i| max[i] - min[i])
            .fold(1e-6f32, f32::max);
        let scale = 0.8 * w.min(h) as f32 / extent;
        let (sy, cy_) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        self.vertices
            .iter()
            .map(|v| {
                let x = v[0] - center[0];
                let y = v[1] - center[1];
                let z = v[2] - center[2];
                // yaw around Y, pitch around X
                let (x1, z1) = (x * cy_ + z * sy, -x * sy + z * cy_);
                let (y2, z2) = (y * cp - z1 * sp, y * sp + z1 * cp);
                let depth = 3.0 * extent + z2;
                let f = scale * (2.5 * extent) / depth.max(0.1);
                (
                    (w / 2) + (x1 * f) as i32,
                    (h / 2) - (y2 * f) as i32,
                )
            })
            .collect()
    }
}

fn parse_obj(path: &Path) -> Result<(Vec<[f32; 3]>, Vec<(u32, u32)>), String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut vertices = Vec::new();
    let mut edges = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("v") => {
                let coords: Vec<f32> = parts.take(3).filter_map(|p| p.parse().ok()).collect();
                if coords.len() == 3 {
                    vertices.push([coords[0], coords[1], coords[2]]);
                }
            }
            Some("f") => {
                let indices: Vec<u32> = parts
                    .filter_map(|p| {
                        p.split('/')
                            .next()?
                            .parse::<i64>()
                            .ok()
                            .map(|i| if i < 0 { (vertices.len() as i64 + i) as u32 } else { (i - 1) as u32 })
                    })
                    .collect();
                for i in 0..indices.len() {
                    let a = indices[i];
                    let b = indices[(i + 1) % indices.len()];
                    if (a as usize) < vertices.len() && (b as usize) < vertices.len() {
                        edges.push((a.min(b), a.max(b)));
                    }
                }
            }
            _ => {}
        }
        if vertices.len() > 200_000 {
            break;
        }
    }
    edges.sort_unstable();
    edges.dedup();
    edges.truncate(120_000);
    if vertices.is_empty() {
        return Err("no vertices found in OBJ file".into());
    }
    Ok((vertices, edges))
}

fn parse_stl(path: &Path) -> Result<(Vec<[f32; 3]>, Vec<(u32, u32)>), String> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    let mut vertices: Vec<[f32; 3]> = Vec::new();
    let mut edges = Vec::new();
    let mut push_tri = |tri: [[f32; 3]; 3], vertices: &mut Vec<[f32; 3]>| {
        let base = vertices.len() as u32;
        vertices.extend_from_slice(&tri);
        edges.push((base, base + 1));
        edges.push((base + 1, base + 2));
        edges.push((base, base + 2));
    };
    let is_ascii = bytes.starts_with(b"solid")
        && std::str::from_utf8(&bytes[..bytes.len().min(512)])
            .map(|s| s.contains("facet"))
            .unwrap_or(false);
    if is_ascii {
        let text = String::from_utf8_lossy(&bytes);
        let mut tri = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("vertex") {
                let coords: Vec<f32> = rest
                    .split_whitespace()
                    .filter_map(|p| p.parse().ok())
                    .collect();
                if coords.len() == 3 {
                    tri.push([coords[0], coords[1], coords[2]]);
                    if tri.len() == 3 {
                        push_tri([tri[0], tri[1], tri[2]], &mut vertices);
                        tri.clear();
                    }
                }
            }
            if vertices.len() > 150_000 {
                break;
            }
        }
    } else {
        if bytes.len() < 84 {
            return Err("STL file too short".into());
        }
        let count = u32::from_le_bytes(bytes[80..84].try_into().unwrap()) as usize;
        let mut off = 84;
        for _ in 0..count.min(50_000) {
            if off + 50 > bytes.len() {
                break;
            }
            let mut tri = [[0f32; 3]; 3];
            for (v, tri_v) in tri.iter_mut().enumerate() {
                let base = off + 12 + v * 12;
                for i in 0..3 {
                    tri_v[i] = f32::from_le_bytes(
                        bytes[base + i * 4..base + i * 4 + 4].try_into().unwrap(),
                    );
                }
            }
            push_tri(tri, &mut vertices);
            off += 50;
        }
    }
    if vertices.is_empty() {
        return Err("no triangles found in STL file".into());
    }
    Ok((vertices, edges))
}

// ------------------------------------------------------------------ media

pub struct MediaView {
    pub path: PathBuf,
    pub kind: FileKind,
    pub player: Option<Child>,
    pub player_name: String,
    pub error: Option<String>,
    pub embedded: bool,
}

impl MediaView {
    /// Spawn a player. `embed_window` is an X window id for mpv's --wid.
    pub fn open(path: &Path, kind: FileKind, embed_window: u32, display: &str) -> Self {
        let mut view = Self {
            path: path.to_path_buf(),
            kind,
            player: None,
            player_name: String::new(),
            error: None,
            embedded: false,
        };
        let try_spawn = |cmd: &mut Command| -> Option<Child> {
            cmd.env("DISPLAY", display)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()
        };
        if command_exists("mpv") {
            let mut cmd = Command::new("mpv");
            cmd.arg(format!("--wid={embed_window}"))
                .arg("--really-quiet")
                .arg("--keep-open=yes")
                .arg(path);
            if let Some(child) = try_spawn(&mut cmd) {
                view.player = Some(child);
                view.player_name = "mpv (embedded)".into();
                view.embedded = true;
                return view;
            }
        }
        if command_exists("ffplay") {
            let mut cmd = Command::new("ffplay");
            cmd.arg("-loglevel").arg("quiet").arg("-autoexit").arg(path);
            if kind == FileKind::Audio {
                cmd.arg("-nodisp");
            }
            if let Some(child) = try_spawn(&mut cmd) {
                view.player = Some(child);
                view.player_name = "ffplay".into();
                return view;
            }
        }
        view.error = Some("No media player found (install mpv or ffmpeg/ffplay)".into());
        view
    }

    pub fn stop(&mut self) {
        if let Some(child) = self.player.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.player = None;
    }
}

impl Drop for MediaView {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|p| p.join(name).exists())
        })
        .unwrap_or(false)
}
