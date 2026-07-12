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
use crate::system_apply::*;
use crate::layout::*;
use crate::draw_helpers::*;
use crate::pixels::*;
use crate::system::*;
use crate::textutil::*;
use crate::procutil::*;
use crate::files::*;

impl Aurora {
    pub(crate) fn open_media(&mut self, entry: FolderEntry) -> AnyResult<()> {
        self.stop_ffplay_process();

        for (idx, state) in self.media_slots.iter_mut().enumerate() {
            if state.is_some() {
                *state = None;
                let _ = self.conn.unmap_window(self.ui.media[idx]);
            }
        }
        let slot = 0;
        let text_lines = if entry.kind == FileKind::Text {
            read_text_lines_limited(&entry.path, 5000)
        } else {
            Vec::new()
        };
        let file_info = (entry.kind == FileKind::Other).then(|| file_command_summary(&entry.path));
        let is_playable = entry.kind == FileKind::Audio || entry.kind == FileKind::Video;

        // For playable media, open ffplay in its own standalone window
        if is_playable {
            let (ffplay_x, ffplay_y, ffplay_w, ffplay_h) = self.ffplay_geometry();
            let path_str = entry.path.to_string_lossy().into_owned();
            let mut cmd = Command::new("ffplay");
            cmd.env("DISPLAY", &self.display)
                .args(["-window_title", "Aurora ffplay"])
                .args(["-x", &ffplay_w.to_string()])
                .args(["-y", &ffplay_h.to_string()])
                .args(["-left", &i32::from(ffplay_x).to_string()])
                .args(["-top", &i32::from(ffplay_y).to_string()])
                .arg(&path_str);
            apply_pulse_env_defaults(&mut cmd);
            match cmd
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    self.ffplay_process = Some(child);
                }
                Err(e) => {
                    eprintln!("aurora-wm: ffplay launch failed: {e}");
                }
            }
            // Do not open the internal media panel for playable files
            return Ok(());
        }

        let media_geom = self.media_geometry(slot);
        let image_preview = if entry.kind == FileKind::Image {
            render_image_preview(
                &entry.path,
                i32::from(media_geom.2) - 64,
                i32::from(media_geom.3) - 146,
            )
        } else {
            None
        };
        let state = MediaState {
            entry,
            playing: false,
            progress: 0.0,
            text_lines,
            text_scroll: 0,
            text_cursor_line: 0,
            text_cursor_col: 0,
            text_undo: Vec::new(),
            editing: false,
            file_info,
            image_preview,
            notice: None,
        };
        self.media = Some(state.clone());
        self.media_slots[slot] = Some(state);
        self.media_front = true;
        self.media_front_slot = Some(slot);
        self.folder_front = false;
        self.settings_front = false;
        self.conn.configure_window(
            self.ui.media[slot],
            &ConfigureWindowAux::new()
                .x(i32::from(media_geom.0))
                .y(i32::from(media_geom.1))
                .width(u32::from(media_geom.2))
                .height(u32::from(media_geom.3))
                .stack_mode(StackMode::ABOVE),
        )?;
        self.conn.map_window(self.ui.media[slot])?;
        self.redraw_media_slot(slot)?;
        self.raise_media()?;

        Ok(())
    }

    pub(crate) fn handle_media_click(&mut self, slot: usize, button: u8, x: i32, y: i32) -> AnyResult<()> {
        let (_, _, w, h) = self.media_geometry(slot);
        if button == 3 {
            self.media_context_open = Some((slot, x, y));
            self.media_trash_prompt = None;
            self.redraw_media_slot(slot)?;
            self.raise_media()?;
            return Ok(());
        }
        if x >= i32::from(w) - 43 && x <= i32::from(w) - 19 && (17..=41).contains(&y) {
            self.media_slots[slot] = None;
            if self.media_front_slot == Some(slot) {
                self.media_front_slot = None;
                self.media_front = false;
            }
            self.media = self.media_slots.iter().rev().find_map(|m| m.clone());
            self.conn.unmap_window(self.ui.media[slot])?;
            self.stop_ffplay_process();
            return Ok(());
        }
        if let Some(action) = self.media_context_action_at(slot, x, y) {
            self.run_media_context_action(slot, action)?;
            return Ok(());
        }
        self.media_context_open = None;
        self.media_trash_prompt = None;
        let active_text_selection = self
            .media_text_selection
            .as_ref()
            .filter(|selection| selection.slot == slot)
            .cloned();
        if let Some(media) = self.media_slots.get_mut(slot).and_then(|m| m.as_mut()) {
            if x >= 24 && x <= i32::from(w) - 92 && (18..=38).contains(&y) {
                copy_text_to_clipboard(&media.entry.path.to_string_lossy());
                media.notice = Some("Copied full path".to_string());
                self.redraw_media_slot(slot)?;
                return Ok(());
            }
            if media.entry.kind == FileKind::Text
                && x >= i32::from(w) - 78
                && x <= i32::from(w) - 50
                && (17..=41).contains(&y)
            {
                if media.editing {
                    let _ = fs::write(&media.entry.path, media.text_lines.join("\n"));
                    media.notice = Some("Saved".to_string());
                } else if media.text_lines.is_empty() {
                    media.text_lines.push(String::new());
                    media.text_cursor_line = 0;
                    media.text_cursor_col = 0;
                }
                media.editing = !media.editing;
                self.redraw_media_slot(slot)?;
                return Ok(());
            }
            if media.entry.kind == FileKind::Text {
                let preview_x = 18;
                let preview_y = 58;
                let preview_w = i32::from(w) - 48;
                let preview_h = i32::from(h) - 130;
                if active_text_selection.is_some() {
                    let (bx, by, bw, bh) =
                        media_text_copy_button_rect(preview_x, preview_y, preview_w, preview_h);
                    if x >= bx && x <= bx + bw && y >= by && y <= by + bh {
                        if let Some(selection) = active_text_selection.as_ref() {
                            let selected = selected_text_from_lines(&media.text_lines, selection);
                            if !selected.is_empty() {
                                copy_text_to_clipboard(&selected);
                                media.notice = Some("Copied selection".to_string());
                            }
                        }
                        self.redraw_media_slot(slot)?;
                        return Ok(());
                    }
                }
                if x >= preview_x
                    && x <= preview_x + preview_w
                    && y >= preview_y
                    && y <= preview_y + preview_h
                {
                    if media.text_lines.is_empty() {
                        media.text_lines.push(String::new());
                    }
                    let line_h = 19;
                    let clicked = ((y - preview_y - 12).max(0) / line_h) as usize;
                    let line_idx =
                        (media.text_scroll + clicked).min(media.text_lines.len().saturating_sub(1));
                    let text_x = preview_x + 42;
                    let line = media
                        .text_lines
                        .get(line_idx)
                        .map(String::as_str)
                        .unwrap_or("");
                    media.text_cursor_line = line_idx;
                    media.text_cursor_col = cursor_col_for_x(&self.regular, line, x - text_x, 13.0);
                    self.media_text_selection = Some(MediaTextSelection {
                        slot,
                        start_line: line_idx,
                        start_col: media.text_cursor_col,
                        end_line: line_idx,
                        end_col: media.text_cursor_col,
                    });
                    self.media_text_selecting = true;
                    self.media_text_selection_redraw_at = Some(Instant::now());
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
            }
            if media.entry.kind == FileKind::Other {
                let preview_y = 58;
                let preview_h = i32::from(h) - 130;
                if let Some(line) = unknown_file_info_line(media, y - preview_y) {
                    copy_text_to_clipboard(&line);
                    media.notice = Some("Copied text".to_string());
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
                if y >= preview_y + preview_h - 56 && y <= preview_y + preview_h - 22 {
                    media.entry.kind = FileKind::Text;
                    media.text_lines = read_text_lines_limited(&media.entry.path, 5000);
                    media.text_scroll = 0;
                    media.text_cursor_line = 0;
                    media.text_cursor_col = 0;
                    media.text_undo.clear();
                    media.notice = None;
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
            }
            let playable = matches!(media.entry.kind, FileKind::Audio | FileKind::Video);
            let controls_y = i32::from(h) - 62;
            if playable
                && x >= 24
                && x <= i32::from(w) - 24
                && y >= controls_y
                && y <= controls_y + 42
            {
                let bar_x = 150;
                let bar_w = i32::from(w) - bar_x - 48;
                if x >= bar_x && x <= bar_x + bar_w {
                    media.progress = ((x - bar_x) as f32 / bar_w.max(1) as f32).clamp(0.0, 1.0);
                    media.playing = true;
                    // Seek: write to named pipe
                    let seek_pct = (media.progress * 100.0) as i32;
                    if let Ok(f) = std::fs::OpenOptions::new()
                        .write(true)
                        .open("/tmp/aurora-player-control")
                    {
                        use std::io::Write;
                        let mut w = f;
                        let _ = w.write_all(format!("seek {}\n", seek_pct).as_bytes());
                    }
                } else {
                    media.playing = !media.playing;
                    // Pause/resume: write to named pipe
                    if let Ok(f) = std::fs::OpenOptions::new()
                        .write(true)
                        .open("/tmp/aurora-player-control")
                    {
                        use std::io::Write;
                        let mut w = f;
                        let _ = w.write_all(b"pause\n");
                    }
                }
                self.media = self.media_slots.iter().rev().find_map(|m| m.clone());
                self.redraw_media_slot(slot)?;
            }
        }
        self.raise_media()?;
        Ok(())
    }

    pub(crate) fn media_context_action_at(&self, slot: usize, x: i32, y: i32) -> Option<MediaContextAction> {
        if self.media_trash_prompt == Some(slot) {
            let (_, _, w, h) = self.media_geometry(slot);
            let box_w = 310;
            let box_h = 126;
            let px = (i32::from(w) - box_w) / 2;
            let py = (i32::from(h) - box_h) / 2;
            if x >= px + 48 && x <= px + 132 && y >= py + 82 && y <= py + 112 {
                return Some(MediaContextAction::ConfirmTrash);
            }
            if x >= px + 174 && x <= px + 258 && y >= py + 82 && y <= py + 112 {
                return Some(MediaContextAction::CancelTrash);
            }
        }
        let (ctx_slot, ctx_x, ctx_y) = self.media_context_open?;
        if ctx_slot != slot {
            return None;
        }
        let (_, _, w, h) = self.media_geometry(slot);
        let menu_x = ctx_x.min(i32::from(w) - 184).max(12);
        let menu_y = ctx_y.min(i32::from(h) - 112).max(50);
        if x < menu_x || x > menu_x + 172 || y < menu_y || y > menu_y + 96 {
            return None;
        }
        match (y - menu_y) / 29 {
            0 => Some(MediaContextAction::Rename),
            1 => Some(MediaContextAction::CopyImage),
            2 => Some(MediaContextAction::MoveTrash),
            _ => None,
        }
    }

    pub(crate) fn run_media_context_action(
        &mut self,
        slot: usize,
        action: MediaContextAction,
    ) -> AnyResult<()> {
        match action {
            MediaContextAction::Rename => {
                if let Some(media) = self.media_slots.get_mut(slot).and_then(|m| m.as_mut()) {
                    media.notice = Some("Rename from the folder context menu".to_string());
                }
                self.media_context_open = None;
            }
            MediaContextAction::CopyImage => {
                let mut copied_image: Option<PathBuf> = None;
                if let Some(media) = self.media_slots.get_mut(slot).and_then(|m| m.as_mut()) {
                    if media.entry.kind == FileKind::Image {
                        copy_image_to_clipboard(&media.entry.path);
                        copied_image = Some(media.entry.path.clone());
                        media.notice = Some("Copied image to clipboard".to_string());
                    } else {
                        copy_text_to_clipboard(&media.entry.path.to_string_lossy());
                        media.notice = Some("Copied path".to_string());
                    }
                }
                if let Some(path) = copied_image {
                    self.last_seen_clipboard_image_sig = clipboard_file_image_signature(&path);
                    self.remember_clipboard_item(ClipboardItem::Image(path));
                    if self.clipboard_menu_visible {
                        self.redraw_clipboard_menu()?;
                    }
                }
                self.media_context_open = None;
            }
            MediaContextAction::MoveTrash => {
                self.media_trash_prompt = Some(slot);
                self.media_context_open = None;
            }
            MediaContextAction::ConfirmTrash => {
                if let Some(media) = self.media_slots.get(slot).and_then(|m| m.as_ref()).cloned() {
                    if move_to_trash(&media.entry.path).is_ok() {
                        self.media_slots[slot] = None;
                        self.media = self.media_slots.iter().rev().find_map(|m| m.clone());
                        self.conn.unmap_window(self.ui.media[slot])?;
                        self.refresh_folder_entries();
                        self.folder_info = Some("Moved to Trash".to_string());
                        self.redraw_folder()?;
                    } else if let Some(media) =
                        self.media_slots.get_mut(slot).and_then(|m| m.as_mut())
                    {
                        media.notice = Some("Could not move to Trash".to_string());
                    }
                }
                self.media_trash_prompt = None;
                self.media_context_open = None;
            }
            MediaContextAction::CancelTrash => {
                self.media_trash_prompt = None;
                self.media_context_open = None;
            }
        }
        if self
            .media_slots
            .get(slot)
            .and_then(|m| m.as_ref())
            .is_some()
        {
            self.redraw_media_slot(slot)?;
        }
        Ok(())
    }

    pub(crate) fn handle_media_motion(
        &mut self,
        slot: usize,
        x: i32,
        y: i32,
        button_down: bool,
    ) -> AnyResult<()> {
        if !button_down {
            if self.media_text_selecting {
                self.media_text_selecting = false;
                self.media_text_selection_redraw_at = None;
                self.erase_media_live_selection()?;
                self.redraw_media_slot(slot)?;
            }
            return Ok(());
        }
        if !self.media_text_selecting {
            return Ok(());
        }
        let Some(selection_slot) = self
            .media_text_selection
            .as_ref()
            .map(|selection| selection.slot)
        else {
            return Ok(());
        };
        if selection_slot != slot {
            return Ok(());
        }
        let (_, _, w, h) = self.media_geometry(slot);
        let Some(media) = self.media_slots.get(slot).and_then(|m| m.as_ref()) else {
            return Ok(());
        };
        if media.entry.kind != FileKind::Text {
            return Ok(());
        }
        let preview_x = 18;
        let preview_y = 58;
        let preview_w = i32::from(w) - 48;
        let preview_h = i32::from(h) - 130;
        if x < preview_x || x > preview_x + preview_w || y < preview_y || y > preview_y + preview_h
        {
            return Ok(());
        }
        let (line, col) = text_position_for_point(media, &self.regular, x, y, preview_x, preview_y);
        let Some(selection) = self.media_text_selection.as_mut() else {
            return Ok(());
        };
        if selection.end_line == line && selection.end_col == col {
            return Ok(());
        }
        selection.end_line = line;
        selection.end_col = col;
        let should_redraw = self
            .media_text_selection_redraw_at
            .is_none_or(|last| last.elapsed() >= Duration::from_millis(16));
        if should_redraw {
            self.media_text_selection_redraw_at = Some(Instant::now());
            self.update_media_live_selection(slot)?;
        }
        Ok(())
    }

    pub(crate) fn handle_media_release(&mut self, slot: usize) -> AnyResult<()> {
        self.media_text_selecting = false;
        self.media_text_selection_redraw_at = None;
        self.erase_media_live_selection()?;
        let Some(selection) = self.media_text_selection.as_ref().cloned() else {
            return Ok(());
        };
        if selection.slot != slot {
            return Ok(());
        }
        self.redraw_media_slot(slot)?;
        Ok(())
    }

    pub(crate) fn erase_media_live_selection(&mut self) -> AnyResult<()> {
        if self.media_text_live_rects.is_empty() {
            return Ok(());
        }
        let slot = self
            .media_text_selection
            .as_ref()
            .map(|s| s.slot)
            .unwrap_or(0);
        let rects = std::mem::take(&mut self.media_text_live_rects);
        self.draw_xor_rects(self.ui.media[slot], &rects)?;
        Ok(())
    }

    pub(crate) fn update_media_live_selection(&mut self, slot: usize) -> AnyResult<()> {
        let rects = self.media_text_selection_rects(slot);
        if same_rects(&rects, &self.media_text_live_rects) {
            return Ok(());
        }
        self.erase_media_live_selection()?;
        if !rects.is_empty() {
            self.draw_xor_rects(self.ui.media[slot], &rects)?;
            self.media_text_live_rects = rects;
        }
        Ok(())
    }

    pub(crate) fn media_text_selection_rects(&self, slot: usize) -> Vec<Rectangle> {
        let Some(media) = self.media_slots.get(slot).and_then(|m| m.as_ref()) else {
            return Vec::new();
        };
        let Some(selection) = self
            .media_text_selection
            .as_ref()
            .filter(|selection| selection.slot == slot)
        else {
            return Vec::new();
        };
        let (_, _, w, h) = self.media_geometry(slot);
        let preview_x = 18;
        let preview_y = 58;
        let preview_w = i32::from(w) - 48;
        let preview_h = i32::from(h) - 130;
        let line_h = 19;
        let max_lines = ((preview_h - 20) / line_h).max(1) as usize;
        let start = media
            .text_scroll
            .min(media.text_lines.len().saturating_sub(1));
        let text_x = preview_x + 42;
        let (sel_start, sel_end) = normalized_media_selection(selection);
        let mut rects = Vec::new();
        for idx in 0..max_lines {
            let line_no = start + idx;
            if line_no > sel_end.0 || line_no >= media.text_lines.len() {
                break;
            }
            if line_no < sel_start.0 {
                continue;
            }
            let Some(line) = media.text_lines.get(line_no) else {
                continue;
            };
            let line_len = line.chars().count();
            let start_col = if line_no == sel_start.0 {
                sel_start.1.min(line_len)
            } else {
                0
            };
            let end_col = if line_no == sel_end.0 {
                sel_end.1.min(line_len)
            } else {
                line_len
            };
            if end_col <= start_col {
                continue;
            }
            let sx = text_x + fast_text_width_cols(&self.regular, line, 0, start_col, 13.0);
            let sw = fast_text_width_cols(&self.regular, line, start_col, end_col, 13.0).max(3);
            let yy = preview_y + 13 + idx as i32 * line_h;
            let x = sx.max(preview_x).min(preview_x + preview_w - 1) as i16;
            let y = yy.max(preview_y).min(preview_y + preview_h - 1) as i16;
            let width = sw.min(preview_x + preview_w - i32::from(x)).max(1) as u16;
            rects.push(Rectangle {
                x,
                y,
                width,
                height: 16,
            });
        }
        rects
    }

    pub(crate) fn handle_media_scroll(&mut self, slot: usize, button: u8) -> AnyResult<()> {
        let Some(media) = self.media_slots.get_mut(slot).and_then(|m| m.as_mut()) else {
            return Ok(());
        };
        if media.entry.kind != FileKind::Text {
            return Ok(());
        }
        let old = media.text_scroll;
        let max_scroll = media.text_lines.len().saturating_sub(1);
        if button == 4 {
            media.text_scroll = media.text_scroll.saturating_sub(4);
        } else {
            media.text_scroll = (media.text_scroll + 4).min(max_scroll);
        }
        if media.text_scroll != old {
            self.redraw_media_slot(slot)?;
        }
        Ok(())
    }

    pub(crate) fn handle_media_key(&mut self, slot: usize, ev: KeyPressEvent) -> AnyResult<()> {
        let (_, _, _, h) = self.media_geometry(slot);
        let visible_lines = ((i32::from(h) - 150) / 19).max(1) as usize;
        let active_selection = self
            .media_text_selection
            .as_ref()
            .filter(|selection| selection.slot == slot)
            .cloned();
        let Some(media) = self.media_slots.get_mut(slot).and_then(|m| m.as_mut()) else {
            return Ok(());
        };
        if media.entry.kind != FileKind::Text || !media.editing {
            return Ok(());
        }
        let mapping = self.conn.get_keyboard_mapping(ev.detail, 1)?.reply()?;
        let shifted = u16::from(ev.state) & u16::from(KeyButMask::SHIFT) != 0;
        let column = if shifted && mapping.keysyms_per_keycode > 1 {
            1
        } else {
            0
        };
        let Some(&keysym) = mapping.keysyms.get(column) else {
            return Ok(());
        };
        if media.text_lines.is_empty() {
            media.text_lines.push(String::new());
        }
        let ctrl = u16::from(ev.state) & u16::from(KeyButMask::CONTROL) != 0;
        if ctrl {
            match keysym {
                0x61 | 0x41 => {
                    let last_line = media.text_lines.len().saturating_sub(1);
                    let last_col = media
                        .text_lines
                        .get(last_line)
                        .map(|line| line.chars().count())
                        .unwrap_or(0);
                    self.media_text_selection = Some(MediaTextSelection {
                        slot,
                        start_line: 0,
                        start_col: 0,
                        end_line: last_line,
                        end_col: last_col,
                    });
                    self.media_text_selecting = false;
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
                0x63 | 0x43 => {
                    if let Some(selection) = active_selection.as_ref() {
                        let selected = selected_text_from_lines(&media.text_lines, selection);
                        if !selected.is_empty() {
                            copy_text_to_clipboard(&selected);
                            media.notice = Some("Copied selection".to_string());
                        }
                    }
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
                0x78 | 0x58 => {
                    if let Some(selection) = active_selection.as_ref() {
                        let selected = selected_text_from_lines(&media.text_lines, selection);
                        if !selected.is_empty() {
                            copy_text_to_clipboard(&selected);
                            push_text_undo(media);
                            delete_text_selection(media, selection);
                            self.media_text_selection = None;
                            media.notice = Some("Cut selection".to_string());
                        }
                    }
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
                0x76 | 0x56 => {
                    if let Some(text) = read_text_clipboard() {
                        if !text.is_empty() {
                            push_text_undo(media);
                            if let Some(selection) = active_selection.as_ref() {
                                delete_text_selection(media, selection);
                                self.media_text_selection = None;
                            }
                            insert_text_at_cursor(media, &text);
                        }
                    }
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
                0x7a | 0x5a => {
                    if let Some(previous) = media.text_undo.pop() {
                        media.text_lines = previous;
                        media.text_cursor_line = media
                            .text_cursor_line
                            .min(media.text_lines.len().saturating_sub(1));
                        media.text_cursor_col = media
                            .text_lines
                            .get(media.text_cursor_line)
                            .map(|line| media.text_cursor_col.min(line.chars().count()))
                            .unwrap_or(0);
                        self.media_text_selection = None;
                    }
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
        match keysym {
            0xff08 => {
                if let Some(selection) = active_selection.as_ref() {
                    push_text_undo(media);
                    delete_text_selection(media, selection);
                    self.media_text_selection = None;
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
                let line_idx = media
                    .text_cursor_line
                    .min(media.text_lines.len().saturating_sub(1));
                let col = media
                    .text_cursor_col
                    .min(media.text_lines[line_idx].chars().count());
                if col > 0 {
                    push_text_undo(media);
                    let byte_idx = nth_char_byte(&media.text_lines[line_idx], col - 1);
                    media.text_lines[line_idx].remove(byte_idx);
                    media.text_cursor_col = col - 1;
                } else if line_idx > 0 {
                    push_text_undo(media);
                    let removed = media.text_lines.remove(line_idx);
                    media.text_cursor_line = line_idx - 1;
                    media.text_cursor_col =
                        media.text_lines[media.text_cursor_line].chars().count();
                    media.text_lines[media.text_cursor_line].push_str(&removed);
                }
            }
            0xff0d => {
                if let Some(selection) = active_selection.as_ref() {
                    push_text_undo(media);
                    delete_text_selection(media, selection);
                    self.media_text_selection = None;
                } else {
                    push_text_undo(media);
                }
                let line_idx = media
                    .text_cursor_line
                    .min(media.text_lines.len().saturating_sub(1));
                let col = media
                    .text_cursor_col
                    .min(media.text_lines[line_idx].chars().count());
                let byte_idx = nth_char_byte(&media.text_lines[line_idx], col);
                let rest = media.text_lines[line_idx].split_off(byte_idx);
                media.text_lines.insert(line_idx + 1, rest);
                media.text_cursor_line = line_idx + 1;
                media.text_cursor_col = 0;
            }
            0xff51 => {
                media.text_cursor_col = media.text_cursor_col.saturating_sub(1);
            }
            0xff53 => {
                let line_idx = media
                    .text_cursor_line
                    .min(media.text_lines.len().saturating_sub(1));
                let len = media.text_lines[line_idx].chars().count();
                media.text_cursor_col = (media.text_cursor_col + 1).min(len);
            }
            0xff52 => {
                media.text_cursor_line = media.text_cursor_line.saturating_sub(1);
                let len = media.text_lines[media.text_cursor_line].chars().count();
                media.text_cursor_col = media.text_cursor_col.min(len);
            }
            0xff54 => {
                media.text_cursor_line =
                    (media.text_cursor_line + 1).min(media.text_lines.len().saturating_sub(1));
                let len = media.text_lines[media.text_cursor_line].chars().count();
                media.text_cursor_col = media.text_cursor_col.min(len);
            }
            0xff50 => media.text_cursor_col = 0,
            0xff57 => {
                let line_idx = media
                    .text_cursor_line
                    .min(media.text_lines.len().saturating_sub(1));
                media.text_cursor_col = media.text_lines[line_idx].chars().count();
            }
            0x20..=0x7e => {
                if let Some(selection) = active_selection.as_ref() {
                    push_text_undo(media);
                    delete_text_selection(media, selection);
                    self.media_text_selection = None;
                } else {
                    push_text_undo(media);
                }
                let ch = char::from_u32(keysym).unwrap();
                let line_idx = media
                    .text_cursor_line
                    .min(media.text_lines.len().saturating_sub(1));
                let col = media
                    .text_cursor_col
                    .min(media.text_lines[line_idx].chars().count());
                let byte_idx = nth_char_byte(&media.text_lines[line_idx], col);
                media.text_lines[line_idx].insert(byte_idx, ch);
                media.text_cursor_col = col + 1;
            }
            _ => return Ok(()),
        }
        media.text_cursor_line = media
            .text_cursor_line
            .min(media.text_lines.len().saturating_sub(1));
        let line_len = media.text_lines[media.text_cursor_line].chars().count();
        media.text_cursor_col = media.text_cursor_col.min(line_len);
        if media.text_cursor_line < media.text_scroll {
            media.text_scroll = media.text_cursor_line;
        } else if media.text_cursor_line >= media.text_scroll + visible_lines {
            media.text_scroll = media.text_cursor_line.saturating_sub(visible_lines - 1);
        }
        self.redraw_media_slot(slot)?;
        Ok(())
    }

    pub(crate) fn advance_internal_media(&mut self) -> AnyResult<bool> {
        let mut changed = false;
        // Read real playback progress from the C player's progress file
        let file_progress: Option<f32> = std::fs::read_to_string("/tmp/aurora-player-progress")
            .ok()
            .and_then(|s| s.trim().parse::<f32>().ok());

        for slot in 0..MEDIA_SLOT_COUNT {
            let Some(media) = self.media_slots.get_mut(slot).and_then(|m| m.as_mut()) else {
                continue;
            };
            if !media.playing || !matches!(media.entry.kind, FileKind::Audio | FileKind::Video) {
                continue;
            }
            if let Some(p) = file_progress {
                let clamped = p.clamp(0.0, 1.0);
                if (clamped - media.progress).abs() > 0.001 {
                    media.progress = clamped;
                    self.redraw_media_slot(slot)?;
                    changed = true;
                }
            }
        }
        if changed {
            self.media = self.media_slots.iter().rev().find_map(|m| m.clone());
        }
        Ok(changed)
    }

    pub(crate) fn handle_app_menu_click(&mut self, button: u8, x: i32, y: i32) -> AnyResult<()> {
        let (_, _, w, h) = self.app_menu_geometry();
        if button == 1 && x >= 92 && x <= i32::from(w) - 18 && (11..=45).contains(&y) {
            self.conn
                .set_input_focus(InputFocus::POINTER_ROOT, self.ui.app_menu, CURRENT_TIME)?;
            return Ok(());
        }
        if self.app_menu_more && x >= 264 && (button == 4 || button == 5) {
            let rows = app_catalog_rows(
                &self.app_menu_query,
                &self.app_menu_expanded_categories,
            );
            let visible = ((i32::from(h) - 100) / 30).max(1) as usize;
            let max_scroll = rows.len().saturating_sub(visible);
            if button == 4 {
                self.app_menu_scroll = self.app_menu_scroll.saturating_sub(3);
            } else {
                self.app_menu_scroll = (self.app_menu_scroll + 3).min(max_scroll);
            }
            self.redraw_app_menu()?;
            return Ok(());
        }
        if self.app_menu_more && x >= 264 && button == 1 && y >= 88 {
            let rows = app_catalog_rows(
                &self.app_menu_query,
                &self.app_menu_expanded_categories,
            );
            let visible = ((i32::from(h) - 100) / 30).max(1) as usize;
            let start = self
                .app_menu_scroll
                .min(rows.len().saturating_sub(visible));
            let row_idx = start + ((y - 88) / 30).max(0) as usize;
            if let Some(row) = rows.get(row_idx) {
                match row {
                    AppCatalogRow::Category { name, .. } if self.app_menu_query.is_empty() => {
                        if !self.app_menu_expanded_categories.remove(name) {
                            self.app_menu_expanded_categories.insert(name.clone());
                        }
                        self.app_menu_scroll = 0;
                        self.redraw_app_menu()?;
                    }
                    AppCatalogRow::Category { .. } => {}
                    AppCatalogRow::App { command, .. } => {
                        if self.spawn_configured_app(command, None) {
                            self.hide_app_menu()?;
                        }
                    }
                }
            }
            return Ok(());
        }
        if !self.app_menu_more && !self.app_menu_query.is_empty() && button == 1 {
            let idx = (y - 59) / 42;
            if idx >= 0 && x >= 14 {
                let matches = app_catalog_rows(
                    &self.app_menu_query,
                    &self.app_menu_expanded_categories,
                )
                .into_iter()
                .filter_map(|row| match row {
                    AppCatalogRow::App { command, .. } => Some(command),
                    _ => None,
                })
                .take(6)
                .collect::<Vec<_>>();
                if let Some(command) = matches.get(idx as usize) {
                    if self.spawn_configured_app(command, None) {
                        self.hide_app_menu()?;
                    }
                }
            }
            return Ok(());
        }
        if button != 1 || x < 14 || (self.app_menu_more && x > 252) {
            return Ok(());
        }
        let idx = (y - 59) / 42;
        let apps = app_menu_items();
        let Some(item) = (idx >= 0).then(|| apps.get(idx as usize)).flatten() else {
            return Ok(());
        };
        match item.action {
            AppAction::Terminal => self.launch_terminal(),
            AppAction::Browser => self.launch_browser(),
            AppAction::Camera => {
                self.launch_desktop_app_matching(&["snapshot", "camera", "cheese"], &["snapshot"]);
            }
            AppAction::Recorder => {
                self.launch_desktop_app_matching(
                    &["recorder", "obs studio", "screencast"],
                    &["obs"],
                );
            }
            AppAction::Settings => {
                self.hide_app_menu()?;
                self.settings_visible = true;
                self.settings_front = true;
                self.settings_hidden_at = None;
                self.folder_front = false;
                self.media_front = false;
                self.conn.map_window(self.ui.settings)?;
                self.raise_ui()?;
                self.request_settings_data(self.settings.tab);
                self.redraw_settings()?;
                self.redraw_topbar()?;
                return Ok(());
            }
            AppAction::More => {
                self.app_menu_more = !self.app_menu_more;
                self.app_menu_scroll = 0;
                if self.app_menu_more {
                    self.app_menu_expanded_categories.clear();
                }
                let menu = self.app_menu_geometry();
                self.conn.configure_window(
                    self.ui.app_menu,
                    &ConfigureWindowAux::new()
                        .x(i32::from(menu.0))
                        .y(i32::from(menu.1))
                        .width(u32::from(menu.2))
                        .height(u32::from(menu.3)),
                )?;
                self.redraw_app_menu()?;
                return Ok(());
            }
        }
        self.hide_app_menu()?;
        Ok(())
    }

}
