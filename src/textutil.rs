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
use crate::procutil::*;
use crate::files::*;

pub(crate) fn compact(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        let mut out = value
            .chars()
            .take(max_chars.saturating_sub(3))
            .collect::<String>();
        out.push_str("...");
        out
    }
}

pub(crate) fn clipboard_text_preview_lines(text: &str) -> (String, Option<String>) {
    let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let total_chars = cleaned.chars().count();
    if total_chars > 60 {
        let first = cleaned.chars().take(40).collect::<String>();
        let mut second = String::from("...");
        second.extend(cleaned.chars().skip(total_chars - 20));
        return (first, Some(second));
    }
    if total_chars > 40 {
        let first = cleaned.chars().take(40).collect::<String>();
        let second = cleaned.chars().skip(40).collect::<String>();
        return (first, Some(second));
    }
    (cleaned, None)
}

pub(crate) fn clipboard_entry_row_height(entry: &ClipboardEntry) -> i32 {
    match entry.item {
        ClipboardItem::Text(_) => CLIPBOARD_MENU_TEXT_ROW_HEIGHT,
        ClipboardItem::Image(_) => CLIPBOARD_MENU_IMAGE_ROW_HEIGHT,
    }
}

pub(crate) fn clipboard_image_type_label(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_uppercase())
        .filter(|ext| !ext.is_empty())
        .unwrap_or_else(|| "IMAGE".to_string())
}

pub(crate) fn format_size_mb(bytes: u64) -> String {
    format!("{:.2} MB", bytes as f64 / 1024.0 / 1024.0)
}

pub(crate) fn folder_entry_info(entry: &FolderEntry) -> String {
    let size = fs::metadata(&entry.path).map(|m| m.len()).unwrap_or(0);
    let mut parts = vec![
        entry.name.clone(),
        file_kind_label(entry.kind).to_string(),
        format_size_mb(size),
    ];
    if entry.kind == FileKind::Image {
        if let Some((w, h)) = image_dimensions(&entry.path) {
            parts.push(format!("{w}x{h}"));
        }
    }
    parts.join("  ")
}

pub(crate) fn image_info_line(path: &Path, cached_resolution: Option<(u32, u32)>) -> String {
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut parts = vec![format_size_mb(size)];
    if let Some((w, h)) = cached_resolution.or_else(|| image_dimensions(path)) {
        parts.push(format!("{w}x{h}"));
    }
    parts.push(
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Image")
            .to_string(),
    );
    parts.join("  ")
}

pub(crate) fn viewer_status(media: &MediaState) -> String {
    let size = fs::metadata(&media.entry.path)
        .map(|m| m.len())
        .unwrap_or(0);
    if media.entry.kind == FileKind::Text {
        let lines = media.text_lines.len();
        let words = media
            .text_lines
            .iter()
            .map(|line| line.split_whitespace().count())
            .sum::<usize>();
        format!("{lines} lines  {words} words  {}", format_size_mb(size))
    } else {
        format_size_mb(size)
    }
}

pub(crate) fn unknown_file_info_line(media: &MediaState, local_y: i32) -> Option<String> {
    let idx = (local_y - 112) / 24;
    if !(0..4).contains(&idx) {
        return None;
    }
    let meta = fs::metadata(&media.entry.path).ok();
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let modified = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| format!("modified {}s", d.as_secs()))
        .unwrap_or_else(|| "modified unknown".to_string());
    let kind = media
        .file_info
        .as_deref()
        .unwrap_or("Unknown file type")
        .to_string();
    match idx {
        0 => Some(media.entry.name.clone()),
        1 => Some(format_size_mb(size)),
        2 => Some(modified),
        3 => Some(kind),
        _ => None,
    }
}

pub(crate) fn normalized_media_selection(selection: &MediaTextSelection) -> ((usize, usize), (usize, usize)) {
    let start = (selection.start_line, selection.start_col);
    let end = (selection.end_line, selection.end_col);
    if start <= end {
        (start, end)
    } else {
        (end, start)
    }
}

pub(crate) fn selected_text_from_lines(lines: &[String], selection: &MediaTextSelection) -> String {
    let (start, end) = normalized_media_selection(selection);
    if start == end {
        return String::new();
    }
    let mut out = String::new();
    for line_no in start.0..=end.0.min(lines.len().saturating_sub(1)) {
        let Some(line) = lines.get(line_no) else {
            continue;
        };
        let line_len = line.chars().count();
        let start_col = if line_no == start.0 {
            start.1.min(line_len)
        } else {
            0
        };
        let end_col = if line_no == end.0 {
            end.1.min(line_len)
        } else {
            line_len
        };
        if end_col > start_col {
            out.push_str(
                &line
                    .chars()
                    .skip(start_col)
                    .take(end_col - start_col)
                    .collect::<String>(),
            );
        }
        if line_no != end.0 {
            out.push('\n');
        }
    }
    out
}

pub(crate) fn push_text_undo(media: &mut MediaState) {
    media.text_undo.push(media.text_lines.clone());
    if media.text_undo.len() > 64 {
        media.text_undo.remove(0);
    }
}

pub(crate) fn delete_text_selection(media: &mut MediaState, selection: &MediaTextSelection) {
    if media.text_lines.is_empty() {
        media.text_lines.push(String::new());
        media.text_cursor_line = 0;
        media.text_cursor_col = 0;
        return;
    }
    let (start, end) = normalized_media_selection(selection);
    if start == end {
        media.text_cursor_line = start.0.min(media.text_lines.len().saturating_sub(1));
        media.text_cursor_col = start.1;
        return;
    }
    let start_line = start.0.min(media.text_lines.len().saturating_sub(1));
    let end_line = end.0.min(media.text_lines.len().saturating_sub(1));
    let start_col = start.1.min(media.text_lines[start_line].chars().count());
    let end_col = end.1.min(media.text_lines[end_line].chars().count());
    let start_byte = nth_char_byte(&media.text_lines[start_line], start_col);
    let end_byte = nth_char_byte(&media.text_lines[end_line], end_col);
    if start_line == end_line {
        media.text_lines[start_line].replace_range(start_byte..end_byte, "");
    } else {
        let prefix = media.text_lines[start_line][..start_byte].to_string();
        let suffix = media.text_lines[end_line][end_byte..].to_string();
        media
            .text_lines
            .splice(start_line..=end_line, [format!("{prefix}{suffix}")]);
    }
    if media.text_lines.is_empty() {
        media.text_lines.push(String::new());
    }
    media.text_cursor_line = start_line.min(media.text_lines.len().saturating_sub(1));
    media.text_cursor_col = start_col.min(media.text_lines[media.text_cursor_line].chars().count());
}

pub(crate) fn insert_text_at_cursor(media: &mut MediaState, text: &str) {
    if media.text_lines.is_empty() {
        media.text_lines.push(String::new());
    }
    let line_idx = media
        .text_cursor_line
        .min(media.text_lines.len().saturating_sub(1));
    let col = media
        .text_cursor_col
        .min(media.text_lines[line_idx].chars().count());
    let byte_idx = nth_char_byte(&media.text_lines[line_idx], col);
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let parts = normalized.split('\n').collect::<Vec<_>>();
    if parts.len() == 1 {
        media.text_lines[line_idx].insert_str(byte_idx, &normalized);
        media.text_cursor_line = line_idx;
        media.text_cursor_col = col + normalized.chars().count();
        return;
    }
    let tail = media.text_lines[line_idx].split_off(byte_idx);
    media.text_lines[line_idx].push_str(parts[0]);
    let mut insert_at = line_idx + 1;
    for part in parts.iter().skip(1).take(parts.len().saturating_sub(2)) {
        media.text_lines.insert(insert_at, (*part).to_string());
        insert_at += 1;
    }
    let last = parts.last().copied().unwrap_or("");
    media.text_lines.insert(insert_at, format!("{last}{tail}"));
    media.text_cursor_line = insert_at;
    media.text_cursor_col = last.chars().count();
}

pub(crate) fn media_text_copy_button_rect(x: i32, y: i32, w: i32, h: i32) -> (i32, i32, i32, i32) {
    (x + w - 48, y + h - 42, 32, 30)
}

pub(crate) fn selection_border_rects(x: i16, y: i16, w: u16, h: u16) -> [Rectangle; 4] {
    let bw = 2u16;
    let right_x = x.saturating_add(w.saturating_sub(bw) as i16);
    let bottom_y = y.saturating_add(h.saturating_sub(bw) as i16);
    [
        Rectangle {
            x,
            y,
            width: w,
            height: bw,
        },
        Rectangle {
            x,
            y: bottom_y,
            width: w,
            height: bw,
        },
        Rectangle {
            x,
            y,
            width: bw,
            height: h,
        },
        Rectangle {
            x: right_x,
            y,
            width: bw,
            height: h,
        },
    ]
}

pub(crate) fn same_rects(a: &[Rectangle], b: &[Rectangle]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(left, right)| {
            left.x == right.x
                && left.y == right.y
                && left.width == right.width
                && left.height == right.height
        })
}

pub(crate) fn terminal_point_to_cell(
    x: i32,
    y: i32,
    cell_w: i32,
    cell_h: i32,
    cols: usize,
    rows: usize,
) -> (usize, usize) {
    let row = ((y - 52).max(0) / cell_h).clamp(0, rows as i32 - 1) as usize;
    let col = ((x - 18).max(0) / cell_w).clamp(0, cols as i32 - 1) as usize;
    (row, col)
}

pub(crate) fn terminal_selection_rects(
    selection: TerminalSelection,
    rows: &[String],
    cell_w: i32,
    cell_h: i32,
) -> Vec<Rectangle> {
    let start = (selection.start_row, selection.start_col);
    let end = (selection.end_row, selection.end_col);
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    let mut rects = Vec::new();
    for row in start.0..=end.0.min(rows.len().saturating_sub(1)) {
        let line_len = rows.get(row).map(|line| line.chars().count()).unwrap_or(0);
        let start_col = if row == start.0 {
            start
                .1
                .min(rows.get(row).map(|line| line.chars().count()).unwrap_or(0))
        } else {
            0
        };
        let end_col = if row == end.0 {
            end.1
                .min(rows.get(row).map(|line| line.chars().count()).unwrap_or(0))
        } else {
            line_len.max(start_col)
        };
        if end_col <= start_col {
            continue;
        }
        rects.push(Rectangle {
            x: (18 + start_col as i32 * cell_w) as i16,
            y: (53 + row as i32 * cell_h) as i16,
            width: ((end_col - start_col) as i32 * cell_w).max(4) as u16,
            height: (cell_h - 3).max(10) as u16,
        });
    }
    rects
}

pub(crate) fn selected_terminal_text(selection: TerminalSelection, rows: &[String]) -> String {
    let start = (selection.start_row, selection.start_col);
    let end = (selection.end_row, selection.end_col);
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    if start == end {
        return String::new();
    }
    let mut out = String::new();
    for row in start.0..=end.0.min(rows.len().saturating_sub(1)) {
        let line = rows.get(row).map(String::as_str).unwrap_or("");
        let line_len = line.chars().count();
        let start_col = if row == start.0 {
            start.1.min(line_len)
        } else {
            0
        };
        let end_col = if row == end.0 {
            end.1.min(line_len)
        } else {
            line_len
        };
        if end_col > start_col {
            out.push_str(
                &line
                    .chars()
                    .skip(start_col)
                    .take(end_col - start_col)
                    .collect::<String>(),
            );
        }
        if row != end.0 {
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

pub(crate) fn text_position_for_point(
    media: &MediaState,
    font: &Font<'static>,
    x: i32,
    y: i32,
    preview_x: i32,
    preview_y: i32,
) -> (usize, usize) {
    let line_h = 19;
    let clicked = ((y - preview_y - 12).max(0) / line_h) as usize;
    let line_idx = (media.text_scroll + clicked).min(media.text_lines.len().saturating_sub(1));
    let text_x = preview_x + 42;
    let line = media
        .text_lines
        .get(line_idx)
        .map(String::as_str)
        .unwrap_or("");
    (line_idx, cursor_col_for_x(font, line, x - text_x, 13.0))
}

pub(crate) fn nth_char_byte(value: &str, col: usize) -> usize {
    value
        .char_indices()
        .nth(col)
        .map(|(idx, _)| idx)
        .unwrap_or(value.len())
}

pub(crate) fn fast_text_width_cols(
    font: &Font<'static>,
    line: &str,
    start_col: usize,
    end_col: usize,
    size: f32,
) -> i32 {
    if end_col <= start_col {
        return 0;
    }
    let scale = Scale::uniform(size);
    let space = font
        .glyph(' ')
        .scaled(scale)
        .h_metrics()
        .advance_width
        .max(1.0);
    let width = line
        .chars()
        .skip(start_col)
        .take(end_col - start_col)
        .map(|ch| {
            if ch == '\t' {
                space * 4.0
            } else {
                font.glyph(ch)
                    .scaled(scale)
                    .h_metrics()
                    .advance_width
                    .max(space)
            }
        })
        .sum::<f32>();
    width.ceil() as i32
}

pub(crate) fn cursor_col_for_x(font: &Font<'static>, line: &str, x: i32, size: f32) -> usize {
    if x <= 0 {
        return 0;
    }
    let scale = Scale::uniform(size);
    let space = font
        .glyph(' ')
        .scaled(scale)
        .h_metrics()
        .advance_width
        .max(1.0);
    let mut width = 0.0f32;
    let target = x as f32;
    for (col, ch) in line.chars().enumerate() {
        let advance = if ch == '\t' {
            space * 4.0
        } else {
            font.glyph(ch)
                .scaled(scale)
                .h_metrics()
                .advance_width
                .max(space)
        };
        let next = width + advance;
        if target < next {
            return if target - width < next - target {
                col
            } else {
                col + 1
            };
        }
        width = next;
    }
    line.chars().count()
}

pub(crate) fn terminal_display_char(ch: char, line_drawing: bool) -> char {
    if !line_drawing {
        return ch;
    }
    match ch {
        'q' => '-',
        'x' => '|',
        'l' | 'k' | 'm' | 'j' | 't' | 'u' | 'v' | 'w' | 'n' => '+',
        _ => ch,
    }
}

pub(crate) fn ansi_color(idx: u8) -> Color {
    match idx {
        0 => Color::rgb(40, 40, 40),     // Black
        1 => Color::rgb(205, 0, 0),      // Red
        2 => Color::rgb(0, 205, 0),      // Green
        3 => Color::rgb(205, 205, 0),    // Yellow
        4 => Color::rgb(0, 0, 238),      // Blue
        5 => Color::rgb(205, 0, 205),    // Magenta
        6 => Color::rgb(0, 205, 205),    // Cyan
        7 => Color::rgb(229, 229, 229),  // White
        8 => Color::rgb(127, 127, 127),  // Bright Black
        9 => Color::rgb(255, 0, 0),      // Bright Red
        10 => Color::rgb(0, 255, 0),     // Bright Green
        11 => Color::rgb(255, 255, 0),   // Bright Yellow
        12 => Color::rgb(92, 92, 255),   // Bright Blue
        13 => Color::rgb(255, 0, 255),   // Bright Magenta
        14 => Color::rgb(0, 255, 255),   // Bright Cyan
        15 => Color::rgb(255, 255, 255), // Bright White
        16..=231 => {
            let offset = idx - 16;
            let r = offset / 36;
            let g = (offset % 36) / 6;
            let b = offset % 6;
            let scale = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
            Color::rgb(scale(r), scale(g), scale(b))
        }
        232..=255 => {
            let val = 8 + (idx - 232) * 10;
            Color::rgb(val, val, val)
        }
    }
}

pub(crate) fn csi_values(params: &str) -> Vec<usize> {
    if params.is_empty() {
        return Vec::new();
    }
    params
        .split(';')
        .map(|part| {
            let number = part
                .split(':')
                .next()
                .unwrap_or_default()
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>();
            number.parse::<usize>().unwrap_or(0)
        })
        .collect()
}

pub(crate) fn format_clock() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let month = match u8::from(now.month()) {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        _ => "Dec",
    };
    format!(
        "{} {}   {:02}:{:02}",
        month,
        now.day(),
        now.hour(),
        now.minute()
    )
}
