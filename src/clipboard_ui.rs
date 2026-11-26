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
use crate::files::*;

/// Clipboard menu shortcut row: copy, select all, undo, redo (sent to the focused window).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClipboardShortcut {
    Copy,
    SelectAll,
    Undo,
    Redo,
}

const CLIPBOARD_SHORTCUTS: [ClipboardShortcut; 4] = [
    ClipboardShortcut::Copy,
    ClipboardShortcut::SelectAll,
    ClipboardShortcut::Undo,
    ClipboardShortcut::Redo,
];
const CLIPBOARD_SHORTCUT_X: i32 = 14;
const CLIPBOARD_SHORTCUT_Y: i32 = 8;
const CLIPBOARD_SHORTCUT_STRIDE: i32 = 38;
const CLIPBOARD_SHORTCUT_SIZE: i32 = 32;
/// How often the menu re-reads the focused window's selection.
const SELECTION_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// X keysyms used by the clipboard shortcuts and the long-press paste.
const KEYSYM_SHIFT_L: u32 = 0xffe1;
const KEYSYM_CONTROL_L: u32 = 0xffe3;
const KEYSYM_A: u32 = 0x61;
const KEYSYM_C: u32 = 0x63;
const KEYSYM_V: u32 = 0x76;
const KEYSYM_Z: u32 = 0x7a;

/// Page nav button x positions, anchored to the menu's right edge.
pub(crate) fn clipboard_nav_xs(width: i32) -> (i32, i32) {
    let next = width - 12 - CLIPBOARD_MENU_NAV_W;
    (next - CLIPBOARD_MENU_NAV_W - 6, next)
}

fn clipboard_shortcut_at(x: i32, y: i32) -> Option<ClipboardShortcut> {
    if !(CLIPBOARD_SHORTCUT_Y..CLIPBOARD_SHORTCUT_Y + CLIPBOARD_SHORTCUT_SIZE).contains(&y) {
        return None;
    }
    let relative = x - CLIPBOARD_SHORTCUT_X;
    if relative < 0 {
        return None;
    }
    let index = (relative / CLIPBOARD_SHORTCUT_STRIDE) as usize;
    if index >= CLIPBOARD_SHORTCUTS.len()
        || relative % CLIPBOARD_SHORTCUT_STRIDE >= CLIPBOARD_SHORTCUT_SIZE
    {
        return None;
    }
    Some(CLIPBOARD_SHORTCUTS[index])
}

impl Aurora {
    pub(crate) fn draw_clipboard_shortcut_row(&self, c: &mut Canvas) {
        for (index, shortcut) in CLIPBOARD_SHORTCUTS.iter().enumerate() {
            let x = CLIPBOARD_SHORTCUT_X + index as i32 * CLIPBOARD_SHORTCUT_STRIDE;
            c.draw_round_rect(
                x,
                CLIPBOARD_SHORTCUT_Y,
                CLIPBOARD_SHORTCUT_SIZE,
                CLIPBOARD_SHORTCUT_SIZE,
                9,
                Color::rgba(255, 255, 255, 190),
            );
            let cx = x + CLIPBOARD_SHORTCUT_SIZE / 2;
            let cy = CLIPBOARD_SHORTCUT_Y + CLIPBOARD_SHORTCUT_SIZE / 2;
            match shortcut {
                ClipboardShortcut::Copy => draw_copy_icon(c, cx, cy, INK),
                ClipboardShortcut::SelectAll => draw_select_all_icon(c, cx, cy, INK),
                ClipboardShortcut::Undo => draw_undo_icon(c, cx, cy, false, INK),
                ClipboardShortcut::Redo => draw_undo_icon(c, cx, cy, true, INK),
            }
        }
    }

    /// The focused window's current selection, shown without being added to the history.
    pub(crate) fn draw_clipboard_selection_pill(&self, c: &mut Canvas, width: i32) {
        c.draw_round_rect(
            12,
            CLIPBOARD_MENU_PREVIEW_Y,
            width - 24,
            26,
            13,
            Color::rgba(255, 255, 255, 200),
        );
        c.draw_text(&self.regular, "Selection", 26, CLIPBOARD_MENU_PREVIEW_Y + 7, 11.0, SOFT_INK);
        let line = self.clipboard_selection.as_deref().and_then(|text| {
            text.lines().map(str::trim).find(|line| !line.is_empty())
        });
        let (label, color) = match line {
            Some(line) => (compact(line, 44), INK),
            None => ("Nothing selected".to_string(), MUTED),
        };
        c.draw_text(&self.regular, &label, 96, CLIPBOARD_MENU_PREVIEW_Y + 6, 12.5, color);
    }

    pub(crate) fn poll_selection_preview(&mut self) -> AnyResult<()> {
        if !self.clipboard_menu_visible {
            return Ok(());
        }
        if let Some(rx) = self.selection_poll_rx.as_ref() {
            match rx.try_recv() {
                Ok(text) => {
                    self.selection_poll_rx = None;
                    return self.apply_selection_preview(text);
                }
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => {
                    self.selection_poll_rx = None;
                    return Ok(());
                }
            }
        }
        if self.last_selection_poll.elapsed() < SELECTION_POLL_INTERVAL {
            return Ok(());
        }
        self.last_selection_poll = Instant::now();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(read_primary_selection());
        });
        self.selection_poll_rx = Some(rx);
        Ok(())
    }

    fn apply_selection_preview(&mut self, text: Option<String>) -> AnyResult<()> {
        // Preview only: deliberately never passed to remember_clipboard_item.
        let text = text
            .map(|text| text.trim_end_matches('\0').to_string())
            .filter(|text| !text.trim().is_empty() && text.len() <= 100_000);
        if text == self.clipboard_selection {
            return Ok(());
        }
        self.clipboard_selection = text;
        self.redraw_clipboard_menu()
    }

    /// Sends a shortcut to the focused window. Terminals only get copy: Ctrl+A, Ctrl+Z
    /// and Ctrl+Shift+Z mean other things there (Ctrl+Z suspends the running job).
    pub(crate) fn run_clipboard_shortcut(&mut self, shortcut: ClipboardShortcut) -> AnyResult<()> {
        if let Some(client) = self.active_client {
            let _ = self.focus_window(client);
        }
        let terminal = self.focused_app_is_terminal()?;
        let (modifiers, key): (&[u32], u32) = match (shortcut, terminal) {
            (ClipboardShortcut::Copy, false) => (&[KEYSYM_CONTROL_L][..], KEYSYM_C),
            (ClipboardShortcut::Copy, true) => (&[KEYSYM_CONTROL_L, KEYSYM_SHIFT_L][..], KEYSYM_C),
            (ClipboardShortcut::SelectAll, false) => (&[KEYSYM_CONTROL_L][..], KEYSYM_A),
            (ClipboardShortcut::Undo, false) => (&[KEYSYM_CONTROL_L][..], KEYSYM_Z),
            (ClipboardShortcut::Redo, false) => (&[KEYSYM_CONTROL_L, KEYSYM_SHIFT_L][..], KEYSYM_Z),
            _ => return Ok(()),
        };
        self.send_key_chord(modifiers, key)
    }

    /// Long-press on the clipboard icon: paste into the focused window without opening the menu.
    /// Ctrl+Shift+V for text, Ctrl+V when the newest clipboard entry is an image.
    pub(crate) fn paste_from_clipboard_icon(&mut self) -> AnyResult<()> {
        let holds_image = matches!(
            self.clipboard_history.first().map(|entry| &entry.item),
            Some(ClipboardItem::Image(_))
        );
        if let Some(client) = self.active_client {
            let _ = self.focus_window(client);
        }
        if holds_image {
            self.send_key_chord(&[KEYSYM_CONTROL_L][..], KEYSYM_V)
        } else {
            self.send_key_chord(&[KEYSYM_CONTROL_L, KEYSYM_SHIFT_L][..], KEYSYM_V)
        }
    }

    pub(crate) fn remember_clipboard_item(&mut self, item: ClipboardItem) -> bool {
        if matches!(&item, ClipboardItem::Text(text) if text.is_empty()) {
            return false;
        }
        self.clipboard_history
            .retain(|entry| !clipboard_items_match(&entry.item, &item));
        self.clipboard_history.insert(0, ClipboardEntry { item });
        if self.clipboard_history.len() > CLIPBOARD_HISTORY_LIMIT {
            self.clipboard_history.truncate(CLIPBOARD_HISTORY_LIMIT);
        }
        self.clipboard_history_page = self.clamped_clipboard_page();
        true
    }

    pub(crate) fn poll_clipboard_history(&mut self) -> AnyResult<bool> {
        if let Some(rx) = self.clipboard_poll_rx.as_ref() {
            match rx.try_recv() {
                Ok(result) => {
                    self.clipboard_poll_rx = None;
                    return self.apply_clipboard_poll_result(result);
                }
                Err(TryRecvError::Empty) => return Ok(false),
                Err(TryRecvError::Disconnected) => {
                    self.clipboard_poll_rx = None;
                    return Ok(false);
                }
            }
        }

        if self.clipboard_watch_supported {
            if !self.clipboard_dirty {
                return Ok(false);
            }
            self.clipboard_dirty = false;
        } else if self.last_clipboard_poll.elapsed() < IDLE_CHECK_INTERVAL {
            return Ok(false);
        }
        self.last_clipboard_poll = Instant::now();
        self.start_clipboard_poll();
        Ok(false)
    }

    pub(crate) fn start_clipboard_poll(&mut self) {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let item = read_image_clipboard()
                .map(|(path, sig)| ClipboardPollItem::Image(path, sig))
                .or_else(|| read_text_clipboard().map(ClipboardPollItem::Text));
            let _ = tx.send(ClipboardPollResult { item });
        });
        self.clipboard_poll_rx = Some(rx);
    }

    pub(crate) fn apply_clipboard_poll_result(&mut self, result: ClipboardPollResult) -> AnyResult<bool> {
        let Some(item) = result.item else {
            return Ok(false);
        };
        match item {
            ClipboardPollItem::Image(path, sig) => {
                if self.last_seen_clipboard_image_sig == Some(sig) {
                    return Ok(false);
                }
                self.last_seen_clipboard_image_sig = Some(sig);
                let item = ClipboardItem::Image(path);
                append_clipboard_history(&item);
                self.remember_clipboard_item(item);
                if self.clipboard_menu_visible {
                    self.configure_clipboard_menu()?;
                    self.redraw_clipboard_menu()?;
                    return Ok(true);
                }
                return Ok(false);
            }
            ClipboardPollItem::Text(text) => {
                let text = text.trim_end_matches('\0').to_string();
                if text.is_empty() || text.len() > 1_000_000 {
                    return Ok(false);
                }
                if self.last_seen_clipboard_text.as_deref() == Some(text.as_str()) {
                    return Ok(false);
                }
                self.last_seen_clipboard_text = Some(text.clone());
                let item = ClipboardItem::Text(text);
                append_clipboard_history(&item);
                self.remember_clipboard_item(item);
                if self.clipboard_menu_visible {
                    self.configure_clipboard_menu()?;
                    self.redraw_clipboard_menu()?;
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub(crate) fn toggle_clipboard_menu(&mut self) -> AnyResult<()> {
        if self.clipboard_menu_visible {
            return self.hide_clipboard_menu();
        }
        self.clipboard_menu_visible = true;
        self.clipboard_history_page = 0;
        // Refresh the selection preview on the next periodic tick.
        self.last_selection_poll = Instant::now()
            .checked_sub(SELECTION_POLL_INTERVAL)
            .unwrap_or_else(Instant::now);
        self.configure_clipboard_menu()?;
        self.conn.map_window(self.ui.clipboard_menu)?;
        // Without a compositor an override-redirect window has no pixel storage until it is
        // actually viewable, so a put_image issued right after map_window is dropped and the
        // window shows its (black) background. Sync so the map completes before we draw.
        self.conn.sync()?;
        self.redraw_clipboard_menu()?;
        self.raise_ui()
    }

    pub(crate) fn hide_clipboard_menu(&mut self) -> AnyResult<()> {
        if self.clipboard_menu_visible {
            self.clipboard_menu_visible = false;
            self.conn.unmap_window(self.ui.clipboard_menu)?;
        }
        Ok(())
    }

    pub(crate) fn handle_clipboard_menu_press(&mut self, detail: u8, x: i32, y: i32) -> AnyResult<()> {
        if detail == 4 || detail == 5 {
            return self.handle_clipboard_menu_scroll(detail);
        }
        if detail != 1 {
            return Ok(());
        }
        if y < CLIPBOARD_MENU_PREVIEW_Y {
            if let Some(shortcut) = clipboard_shortcut_at(x, y) {
                return self.run_clipboard_shortcut(shortcut);
            }
            let (prev_x, next_x) = clipboard_nav_xs(i32::from(self.clipboard_menu_geometry().2));
            if point_in_rect(
                x,
                y,
                prev_x,
                CLIPBOARD_MENU_NAV_Y,
                CLIPBOARD_MENU_NAV_W,
                CLIPBOARD_MENU_NAV_H,
            ) {
                if self.clamped_clipboard_page() > 0 {
                    self.clipboard_history_page = self.clamped_clipboard_page() - 1;
                    self.configure_clipboard_menu()?;
                    self.redraw_clipboard_menu()?;
                }
            } else if point_in_rect(
                x,
                y,
                next_x,
                CLIPBOARD_MENU_NAV_Y,
                CLIPBOARD_MENU_NAV_W,
                CLIPBOARD_MENU_NAV_H,
            ) {
                let page = self.clamped_clipboard_page();
                if page + 1 < self.clipboard_page_count() {
                    self.clipboard_history_page = page + 1;
                    self.configure_clipboard_menu()?;
                    self.redraw_clipboard_menu()?;
                }
            }
            return Ok(());
        }
        if y < CLIPBOARD_MENU_LIST_TOP {
            // The selection preview: display only.
            return Ok(());
        }
        let (start, end) = self.clipboard_page_range();
        let mut row_y = CLIPBOARD_MENU_LIST_TOP + 8;
        let mut selected_idx = None;
        for (offset, entry) in self.clipboard_history[start..end].iter().enumerate() {
            let row_h = clipboard_entry_row_height(entry);
            if y >= row_y - 8 && y < row_y - 8 + row_h - 8 {
                selected_idx = Some(start + offset);
                break;
            }
            row_y += row_h;
        }
        let Some(idx) = selected_idx else {
            return Ok(());
        };
        let Some(entry) = self.clipboard_history.get(idx).cloned() else {
            return Ok(());
        };
        match &entry.item {
            ClipboardItem::Text(text) => {
                copy_text_to_clipboard(text);
                self.last_seen_clipboard_text = Some(text.clone());
            }
            ClipboardItem::Image(path) => {
                copy_image_to_clipboard(path);
                self.last_seen_clipboard_image_sig = clipboard_file_image_signature(path);
            }
        }
        self.remember_clipboard_item(entry.item);
        self.hide_clipboard_menu()?;
        self.redraw_topbar()?;
        if let Some(client) = self.active_client {
            let _ = self.focus_window(client);
        }
        self.paste_clipboard_into_focused_app()?;
        Ok(())
    }

    pub(crate) fn handle_clipboard_menu_scroll(&mut self, detail: u8) -> AnyResult<()> {
        let page = self.clamped_clipboard_page();
        let next_page = match detail {
            4 => page.saturating_sub(1),
            5 if page + 1 < self.clipboard_page_count() => page + 1,
            _ => page,
        };
        if next_page != page {
            self.clipboard_history_page = next_page;
            self.configure_clipboard_menu()?;
            self.redraw_clipboard_menu()?;
        }
        Ok(())
    }

}
