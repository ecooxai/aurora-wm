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

impl Aurora {
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
        if y < 38 {
            if point_in_rect(
                x,
                y,
                CLIPBOARD_MENU_PREV_X,
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
                CLIPBOARD_MENU_NEXT_X,
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
        let (start, end) = self.clipboard_page_range();
        let mut row_y = 46;
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
