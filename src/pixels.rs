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
use crate::system::*;
use crate::textutil::*;
use crate::procutil::*;
use crate::files::*;

pub(crate) fn render_wallpaper_pixels(
    bytes: &[u8],
    screen_width: u16,
    screen_height: u16,
) -> AnyResult<Vec<u8>> {
    render_cover_pixels_nearest(bytes, screen_width, screen_height)
}

pub(crate) fn render_asset_preview_pixels(bytes: &[u8], w: u16, h: u16) -> AnyResult<Vec<u8>> {
    render_cover_pixels_nearest(bytes, w, h)
}

pub(crate) fn render_cover_pixels_nearest(bytes: &[u8], w: u16, h: u16) -> AnyResult<Vec<u8>> {
    let img = image::load_from_memory(bytes)?.to_rgba8();
    let (iw, ih) = img.dimensions();
    if iw == 0 || ih == 0 || w == 0 || h == 0 {
        return Ok(Vec::new());
    }

    let scale = (f32::from(w) / iw as f32).max(f32::from(h) / ih as f32);
    let view_w = f32::from(w) / scale;
    let view_h = f32::from(h) / scale;
    let src_x0 = ((iw as f32 - view_w) * 0.5).max(0.0);
    let src_y0 = ((ih as f32 - view_h) * 0.5).max(0.0);
    let raw = img.as_raw();
    let x_map = (0..u32::from(w))
        .map(|x| {
            (src_x0 + (x as f32 + 0.5) * view_w / f32::from(w))
                .floor()
                .clamp(0.0, iw.saturating_sub(1) as f32) as usize
        })
        .collect::<Vec<_>>();
    let y_map = (0..u32::from(h))
        .map(|y| {
            (src_y0 + (y as f32 + 0.5) * view_h / f32::from(h))
                .floor()
                .clamp(0.0, ih.saturating_sub(1) as f32) as usize
        })
        .collect::<Vec<_>>();
    let mut out = vec![0; usize::from(w) * usize::from(h) * 4];
    let src_stride = iw as usize * 4;
    for (dy, sy) in y_map.iter().copied().enumerate() {
        let src_row = sy * src_stride;
        let dst_row = dy * usize::from(w) * 4;
        for (dx, sx) in x_map.iter().copied().enumerate() {
            let src = src_row + sx * 4;
            let dst = dst_row + dx * 4;
            out[dst] = raw[src + 2];
            out[dst + 1] = raw[src + 1];
            out[dst + 2] = raw[src];
            out[dst + 3] = 0;
        }
    }
    Ok(out)
}

pub(crate) fn paint_bgr_pixels(c: &mut Canvas, pixels: &[u8], x: i32, y: i32, w: i32, h: i32) {
    if w <= 0 || h <= 0 {
        return;
    }
    for yy in 0..h {
        for xx in 0..w {
            let idx = ((yy * w + xx) * 4) as usize;
            if idx + 2 < pixels.len() {
                c.blend_pixel(
                    x + xx,
                    y + yy,
                    Color::rgba(pixels[idx + 2], pixels[idx + 1], pixels[idx], 255),
                );
            }
        }
    }
}

pub(crate) fn paint_rgba_pixels(c: &mut Canvas, pixels: &[u8], x: i32, y: i32, w: i32, h: i32) {
    if w <= 0 || h <= 0 {
        return;
    }
    for yy in 0..h {
        for xx in 0..w {
            let idx = ((yy * w + xx) * 4) as usize;
            if idx + 3 < pixels.len() {
                c.blend_pixel(
                    x + xx,
                    y + yy,
                    Color::rgba(
                        pixels[idx],
                        pixels[idx + 1],
                        pixels[idx + 2],
                        pixels[idx + 3],
                    ),
                );
            }
        }
    }
}

pub(crate) fn decode_icon_pixels(bytes: &[u8], size: i32) -> AnyResult<Vec<u8>> {
    let img = image::load_from_memory(bytes)?.to_rgba8();
    let resized = image::imageops::resize(
        &img,
        size.max(1) as u32,
        size.max(1) as u32,
        FilterType::Triangle,
    );
    Ok(resized.into_raw())
}

pub(crate) fn resolve_window_icon(class: &str, title: &str) -> Option<PathBuf> {
    let terms = window_match_terms(class, title);
    let icon_name = find_desktop_icon_name(&terms)?;
    resolve_icon_path(&icon_name)
}

pub(crate) fn window_match_terms(class: &str, title: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for raw in class.split('\0').chain(title.split([' ', '-', '_', '.'])) {
        let term = raw.trim().trim_matches(char::from(0)).to_ascii_lowercase();
        if term.len() >= 2 && !terms.contains(&term) {
            terms.push(term);
        }
    }
    terms
}

pub(crate) fn find_desktop_icon_name(terms: &[String]) -> Option<String> {
    for dir in desktop_search_dirs() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("desktop") {
                continue;
            }
            if let Some(icon) = desktop_icon_if_matches(&path, terms) {
                return Some(icon);
            }
        }
    }
    None
}

pub(crate) fn desktop_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(home) = env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }
    if let Ok(data_home) = env::var("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(data_home).join("applications"));
    }
    if let Ok(data_dirs) = env::var("XDG_DATA_DIRS") {
        dirs.extend(
            data_dirs
                .split(':')
                .filter(|dir| !dir.is_empty())
                .map(|dir| PathBuf::from(dir).join("applications")),
        );
    }
    dirs.push(PathBuf::from("/usr/share/applications"));
    dirs
}

pub(crate) fn desktop_icon_if_matches(path: &Path, terms: &[String]) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let mut name = String::new();
    let mut startup_class = String::new();
    let mut icon = String::new();
    let mut no_display = false;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "Name" => name = value.to_ascii_lowercase(),
            "StartupWMClass" => startup_class = value.to_ascii_lowercase(),
            "Icon" => icon = value.trim().to_string(),
            "NoDisplay" => no_display = value.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }
    if icon.is_empty() || no_display {
        return None;
    }
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let matched = terms.iter().any(|term| {
        startup_class == *term
            || stem == *term
            || stem.ends_with(&format!(".{term}"))
            || name == *term
            || (!name.is_empty() && name.contains(term))
    });
    matched.then_some(icon)
}

pub(crate) fn resolve_icon_path(icon_name: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(icon_name);
    if direct.is_absolute() && direct.exists() {
        return Some(direct);
    }
    let candidates = icon_candidate_paths(icon_name);
    candidates.into_iter().find(|path| path.exists())
}

pub(crate) fn icon_candidate_paths(icon_name: &str) -> Vec<PathBuf> {
    let mut bases = Vec::new();
    if let Ok(home) = env::var("HOME") {
        bases.push(PathBuf::from(home).join(".local/share/icons"));
    }
    bases.push(PathBuf::from("/usr/share/icons/hicolor"));
    bases.push(PathBuf::from("/usr/share/icons"));
    bases.push(PathBuf::from("/usr/share/pixmaps"));

    let sizes = [
        "64x64", "48x48", "32x32", "128x128", "256x256", "scalable", "symbolic",
    ];
    let contexts = ["apps", "categories", "places", "mimetypes"];
    let exts = ["png", "webp", "jpg", "jpeg", "gif"];
    let mut paths = Vec::new();
    for base in bases {
        for size in sizes {
            for context in contexts {
                for ext in exts {
                    paths.push(
                        base.join(size)
                            .join(context)
                            .join(format!("{icon_name}.{ext}")),
                    );
                }
            }
        }
        for ext in exts {
            paths.push(base.join(format!("{icon_name}.{ext}")));
        }
    }
    paths
}

pub(crate) fn paint_file_preview(c: &mut Canvas, path: &std::path::Path, x: i32, y: i32, w: i32, h: i32) {
    if w <= 0 || h <= 0 {
        return;
    }
    let Ok(bytes) = fs::read(path) else {
        return;
    };
    let Ok(img) = image::load_from_memory(&bytes).map(|img| img.to_rgba8()) else {
        return;
    };
    let (iw, ih) = img.dimensions();
    let scale = (w as f32 / iw as f32).min(h as f32 / ih as f32);
    let nw = (iw as f32 * scale).round().max(1.0) as u32;
    let nh = (ih as f32 * scale).round().max(1.0) as u32;
    let resized = image::imageops::resize(&img, nw, nh, FilterType::Triangle);
    let dx = x + (w - nw as i32) / 2;
    let dy = y + (h - nh as i32) / 2;
    for yy in 0..nh as i32 {
        for xx in 0..nw as i32 {
            let p = resized.get_pixel(xx as u32, yy as u32);
            c.blend_pixel(dx + xx, dy + yy, Color::rgba(p[0], p[1], p[2], 255));
        }
    }
}

pub(crate) fn render_image_preview(path: &std::path::Path, w: i32, h: i32) -> Option<ImagePreview> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let resolution = image_dimensions(path);
    let bytes = fs::read(path).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (iw, ih) = img.dimensions();
    let scale = (w as f32 / iw as f32).min(h as f32 / ih as f32);
    let nw = (iw as f32 * scale).round().max(1.0) as u32;
    let nh = (ih as f32 * scale).round().max(1.0) as u32;
    // Lanczos3 keeps downscaled photos crisp in the viewer.
    let resized = image::imageops::resize(&img, nw, nh, FilterType::Lanczos3);
    Some(ImagePreview {
        pixels: resized.into_raw(),
        width: nw.min(u16::MAX as u32) as u16,
        height: nh.min(u16::MAX as u32) as u16,
        resolution,
    })
}

pub(crate) fn capture_screen_preview(conn: &RustConnection, root: Window) -> Option<ImagePreview> {
    let (pixels, iw, ih) = capture_root_rgba(conn, root, 0, 0, u16::MAX, u16::MAX).ok()?;
    Some(ImagePreview {
        pixels,
        width: iw.min(u16::MAX as u32) as u16,
        height: ih.min(u16::MAX as u32) as u16,
        resolution: Some((iw, ih)),
    })
}

pub(crate) fn capture_root_rgba(
    conn: &RustConnection,
    root: Window,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
) -> AnyResult<(Vec<u8>, u32, u32)> {
    let geom = conn.get_geometry(root)?.reply()?;
    let capture_x = x.max(0);
    let capture_y = y.max(0);
    let max_w = i32::from(geom.width).saturating_sub(i32::from(capture_x));
    let max_h = i32::from(geom.height).saturating_sub(i32::from(capture_y));
    let capture_w = u16::try_from(i32::from(width).min(max_w).max(1))?;
    let capture_h = u16::try_from(i32::from(height).min(max_h).max(1))?;
    let reply = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            root,
            capture_x,
            capture_y,
            capture_w,
            capture_h,
            u32::MAX,
        )?
        .reply()?;
    let setup = conn.setup();
    let format = setup
        .pixmap_formats
        .iter()
        .find(|format| format.depth == reply.depth)
        .ok_or("missing X11 pixmap format for screenshot depth")?;
    let visual = find_visual(setup, reply.visual).ok_or("missing X11 visual for screenshot")?;
    let bits_per_pixel = usize::from(format.bits_per_pixel);
    let bytes_per_pixel = bits_per_pixel.div_ceil(8);
    if bytes_per_pixel == 0 || bits_per_pixel > 32 {
        return Err("unsupported X11 screenshot pixel format".into());
    }
    let stride_bits = usize::from(capture_w)
        .checked_mul(bits_per_pixel)
        .ok_or("screenshot row is too wide")?;
    let pad = usize::from(format.scanline_pad).max(8);
    let stride = stride_bits.div_ceil(pad) * (pad / 8);
    let mut rgba = vec![0; usize::from(capture_w) * usize::from(capture_h) * 4];
    for row in 0..usize::from(capture_h) {
        let row_start = row * stride;
        for col in 0..usize::from(capture_w) {
            let src = row_start + col * bytes_per_pixel;
            if src + bytes_per_pixel > reply.data.len() {
                return Err("short X11 screenshot data".into());
            }
            let pixel = read_x11_pixel(
                &reply.data[src..src + bytes_per_pixel],
                setup.image_byte_order,
            );
            let dst = (row * usize::from(capture_w) + col) * 4;
            rgba[dst] = scale_masked_channel(pixel, visual.red_mask);
            rgba[dst + 1] = scale_masked_channel(pixel, visual.green_mask);
            rgba[dst + 2] = scale_masked_channel(pixel, visual.blue_mask);
            rgba[dst + 3] = 255;
        }
    }
    Ok((rgba, u32::from(capture_w), u32::from(capture_h)))
}

pub(crate) fn find_visual(setup: &Setup, visual_id: Visualid) -> Option<Visualtype> {
    setup.roots.iter().find_map(|screen| {
        screen.allowed_depths.iter().find_map(|depth| {
            depth
                .visuals
                .iter()
                .find(|visual| visual.visual_id == visual_id)
                .copied()
        })
    })
}

pub(crate) fn read_x11_pixel(bytes: &[u8], order: ImageOrder) -> u32 {
    let mut pixel = 0u32;
    match order {
        ImageOrder::LSB_FIRST => {
            for (shift, byte) in bytes.iter().enumerate() {
                pixel |= u32::from(*byte) << (shift * 8);
            }
        }
        ImageOrder::MSB_FIRST => {
            for byte in bytes {
                pixel = (pixel << 8) | u32::from(*byte);
            }
        }
        _ => {}
    }
    pixel
}

pub(crate) fn scale_masked_channel(pixel: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let max = mask >> shift;
    let value = (pixel & mask) >> shift;
    ((value * 255 + max / 2) / max) as u8
}

pub(crate) fn canvas_from_preview(preview: &ImagePreview, width: u16, height: u16) -> Canvas {
    let mut c = Canvas::new(width, height, Color::rgba(0, 0, 0, 255));
    let pw = usize::from(preview.width);
    let ph = usize::from(preview.height);
    let cw = usize::from(width);
    let ch = usize::from(height);
    for yy in 0..ph.min(ch) {
        for xx in 0..pw.min(cw) {
            let src = (yy * pw + xx) * 4;
            let dst = (yy * cw + xx) * 4;
            if src + 3 < preview.pixels.len() && dst + 3 < c.data.len() {
                c.data[dst] = preview.pixels[src + 2];
                c.data[dst + 1] = preview.pixels[src + 1];
                c.data[dst + 2] = preview.pixels[src];
                c.data[dst + 3] = preview.pixels[src + 3];
            }
        }
    }
    c
}

pub(crate) fn paint_preview_region(c: &mut Canvas, preview: &ImagePreview, x: i32, y: i32, w: i32, h: i32) {
    let pw = i32::from(preview.width);
    let ph = i32::from(preview.height);
    for yy in 0..h {
        for xx in 0..w {
            let sx = x + xx;
            let sy = y + yy;
            if sx < 0 || sy < 0 || sx >= pw || sy >= ph {
                continue;
            }
            let idx = ((sy * pw + sx) * 4) as usize;
            if idx + 3 < preview.pixels.len() {
                c.blend_pixel(
                    sx,
                    sy,
                    Color::rgba(
                        preview.pixels[idx],
                        preview.pixels[idx + 1],
                        preview.pixels[idx + 2],
                        preview.pixels[idx + 3],
                    ),
                );
            }
        }
    }
}

pub(crate) fn paint_cached_image_preview(
    c: &mut Canvas,
    preview: &ImagePreview,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    paint_cached_image_preview_aligned(c, preview, x, y, w, h, true);
}

pub(crate) fn paint_cached_image_preview_left(
    c: &mut Canvas,
    preview: &ImagePreview,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    paint_cached_image_preview_aligned(c, preview, x, y, w, h, false);
}

pub(crate) fn paint_cached_image_preview_aligned(
    c: &mut Canvas,
    preview: &ImagePreview,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    center_x: bool,
) {
    let pw = i32::from(preview.width);
    let ph = i32::from(preview.height);
    let dx = if center_x { x + (w - pw) / 2 } else { x };
    let dy = y + (h - ph) / 2;
    for yy in 0..ph {
        for xx in 0..pw {
            let px = dx + xx;
            let py = dy + yy;
            if px < x || py < y || px >= x + w || py >= y + h {
                continue;
            }
            let idx = ((yy * pw + xx) * 4) as usize;
            if idx + 3 < preview.pixels.len() {
                c.blend_pixel(
                    px,
                    py,
                    Color::rgba(
                        preview.pixels[idx],
                        preview.pixels[idx + 1],
                        preview.pixels[idx + 2],
                        preview.pixels[idx + 3],
                    ),
                );
            }
        }
    }
}

pub(crate) fn image_dimensions(path: &std::path::Path) -> Option<(u32, u32)> {
    image::image_dimensions(path).ok()
}

pub(crate) fn paint_video_frame_preview(
    c: &mut Canvas,
    path: &std::path::Path,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Option<()> {
    c.draw_round_rect(x, y, w, h, 10, Color::rgba(23, 34, 42, 54));
    c.draw_text_center(
        &Font::try_from_bytes(FONT_REGULAR)?,
        &compact(
            path.file_name().and_then(|n| n.to_str()).unwrap_or("Video"),
            28,
        ),
        x + w / 2,
        y + h / 2 - 12,
        13.0,
        Color::rgb(255, 255, 255),
    );
    None
}

pub(crate) fn read_text_lines_limited(path: &std::path::Path, max_lines: usize) -> Vec<String> {
    let Ok(mut file) = fs::File::open(path) else {
        return vec!["Could not open text file".to_string()];
    };
    let mut buf = String::new();
    let _ = file.by_ref().take(512 * 1024).read_to_string(&mut buf);
    let mut lines = buf
        .lines()
        .take(max_lines)
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

pub(crate) fn file_command_summary(path: &std::path::Path) -> String {
    Command::new("file")
        .arg("-b")
        .arg(path)
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "Unknown file type".to_string())
}
