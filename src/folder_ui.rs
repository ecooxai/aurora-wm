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
    pub(crate) fn show_folder(&mut self, mode: FolderMode, front: bool) -> AnyResult<()> {
        let path = folder_path_for(mode);
        if self.launch_file_manager(&path) {
            return Ok(());
        }

        // Keep the former built-in folder available as a fallback when the
        // standalone binary is missing.
        self.folder_mode = mode;
        self.folder_path = path;
        self.folder_entries = folder_entries_for(mode, self.folder_sort);
        self.folder_selected = None;
        self.folder_scroll = 0;
        self.folder_front = front;
        self.folder_more_open = false;
        self.folder_sort_open = false;
        self.sync_folder_terminal_cwd();
        if front {
            self.settings_front = false;
            self.media_front = false;
        }
        let folder = self.folder_geometry();
        let terminal = self.folder_terminal_geometry();
        self.conn.configure_window(
            self.ui.folder,
            &ConfigureWindowAux::new()
                .x(i32::from(folder.0))
                .y(i32::from(folder.1))
                .width(u32::from(folder.2))
                .height(u32::from(folder.3))
                .stack_mode(if front {
                    StackMode::ABOVE
                } else {
                    StackMode::BELOW
                }),
        )?;
        self.conn.configure_window(
            self.ui.folder_terminal,
            &ConfigureWindowAux::new()
                .x(i32::from(terminal.0))
                .y(i32::from(terminal.1))
                .width(u32::from(terminal.2))
                .height(u32::from(terminal.3)),
        )?;
        self.conn.map_window(self.ui.folder)?;
        self.redraw_folder()?;
        if self.folder_terminal.visible {
            self.redraw_folder_terminal()?;
        }
        self.raise_ui()?;
        Ok(())
    }

    pub(crate) fn handle_folder_click(&mut self, ev: ButtonPressEvent) -> AnyResult<()> {
        let x = i32::from(ev.event_x);
        let y = i32::from(ev.event_y);
        let (_, _, w, h) = self.folder_geometry();
        self.folder_press = None;
        if self.choose_file_mode {
            let cancel_x = i32::from(w) - 190;
            let choose_x = i32::from(w) - 100;
            let btn_y = i32::from(h) - 46;
            if y >= btn_y && y <= btn_y + 32 {
                if x >= cancel_x && x <= cancel_x + 80 {
                    self.cancel_choose_file()?;
                    return Ok(());
                }
                if x >= choose_x && x <= choose_x + 80 {
                    self.submit_choose_file()?;
                    return Ok(());
                }
            }
        }
        if self.folder_sort_open {
            if let Some(sort) = self.folder_sort_at(x, y) {
                self.folder_sort = sort;
                self.folder_sort_open = false;
                self.refresh_folder_entries();
                self.folder_info = Some(format!("Sorted by {}", sort.label().to_lowercase()));
                self.redraw_folder()?;
                return Ok(());
            }
            self.folder_sort_open = false;
        }
        if self.folder_context_open {
            if let Some(action) = self.folder_context_action_at(x, y) {
                self.run_folder_context_action(action)?;
                self.folder_context_open = false;
                self.redraw_folder()?;
                return Ok(());
            }
            self.folder_context_open = false;
        }
        if (18..=48).contains(&x) && (18..=48).contains(&y) {
            self.folder_mode = FolderMode::Home;
            self.folder_path = folder_path_for(FolderMode::Home);
            self.folder_entries = folder_entries_for(FolderMode::Home, self.folder_sort);
            self.folder_selected = None;
            self.folder_scroll = 0;
            self.folder_more_open = false;
            self.folder_sort_open = false;
            self.folder_info = None;
            self.sync_folder_terminal_cwd();
            self.redraw_folder()?;
            if self.folder_terminal.visible {
                self.redraw_folder_terminal()?;
            }
            return Ok(());
        }
        if (56..=86).contains(&x) && (18..=48).contains(&y) {
            self.toggle_folder_terminal()?;
            return Ok(());
        }
        if (94..=124).contains(&x) && (18..=48).contains(&y) {
            self.folder_sort_open = !self.folder_sort_open;
            self.folder_more_open = false;
            self.redraw_folder()?;
            return Ok(());
        }
        if x >= 58 && x <= i32::from(w) - 58 && (36..=60).contains(&y) {
            copy_text_to_clipboard(&self.folder_path.to_string_lossy());
            self.folder_info = Some("Path copied to clipboard".to_string());
            self.redraw_folder()?;
            return Ok(());
        }
        if x >= i32::from(w) - 50 && x <= i32::from(w) - 20 && (18..=48).contains(&y) {
            self.folder_more_open = !self.folder_more_open;
            self.redraw_folder()?;
            return Ok(());
        }
        if self.folder_more_open {
            let menu_x = i32::from(w) - 214;
            for (idx, place) in self.folder_places.iter().take(6).enumerate() {
                let row_y = 94 + idx as i32 * 28;
                if x >= menu_x + 8 && x <= menu_x + 186 && y >= row_y - 5 && y <= row_y + 18 {
                    self.folder_mode = FolderMode::Home;
                    self.folder_path = place.path.clone();
                    self.folder_entries = folder_entries_in(place.path.clone(), self.folder_sort);
                    self.folder_selected = None;
                    self.folder_scroll = 0;
                    self.folder_more_open = false;
                    self.sync_folder_terminal_cwd();
                    self.redraw_folder()?;
                    if self.folder_terminal.visible {
                        self.redraw_folder_terminal()?;
                    }
                    return Ok(());
                }
            }
        }
        self.folder_more_open = false;
        if y < 86 {
            self.folder_info = None;
            self.redraw_folder()?;
            return Ok(());
        }
        let idx = (y - 86) / 42;
        if idx < 0 || idx as usize >= self.folder_visible_row_count() {
            self.folder_info = None;
            self.redraw_folder()?;
            return Ok(());
        }
        let Some(entry) = self
            .folder_entries
            .get(self.folder_scroll + idx as usize)
            .cloned()
        else {
            self.redraw_folder()?;
            return Ok(());
        };
        self.folder_drag = Some(entry.path.clone());
        self.folder_press = Some(FolderPress {
            entry: entry.clone(),
            root_x: ev.root_x,
            root_y: ev.root_y,
        });
        match entry.kind {
            FileKind::Directory => {
                self.folder_selected = Some(entry.path.clone());
                self.folder_info = Some(folder_entry_info(&entry));
                self.redraw_folder()?;
            }
            FileKind::Text
            | FileKind::Image
            | FileKind::Audio
            | FileKind::Video
            | FileKind::Other => {
                if self.folder_selected.as_ref() == Some(&entry.path) {
                    if self.choose_file_mode {
                        self.submit_choose_file()?;
                    } else {
                        self.open_media(entry)?;
                    }
                } else {
                    self.folder_selected = Some(entry.path.clone());
                    self.folder_info = Some(folder_entry_info(&entry));
                    self.redraw_folder()?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn handle_folder_release(&mut self, ev: ButtonReleaseEvent) -> AnyResult<()> {
        let press = self.folder_press.take();
        let Some(path) = self.folder_drag.take() else {
            return Ok(());
        };
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        let mut target = pointer.child;
        let moved = press.as_ref().is_some_and(|press| {
            (i32::from(ev.root_x) - i32::from(press.root_x)).abs() > 6
                || (i32::from(ev.root_y) - i32::from(press.root_y)).abs() > 6
        });
        if target == self.ui.folder && !moved {
            if let Some(press) = press.filter(|press| {
                press.entry.kind == FileKind::Directory
                    || self.folder_selected.as_ref() != Some(&press.entry.path)
            }) {
                self.activate_folder_entry(press.entry)?;
            }
            return Ok(());
        }
        if target == self.ui.folder_terminal {
            self.ensure_folder_terminal_pty();
            self.folder_terminal.focused = true;
            self.conn.set_input_focus(
                InputFocus::POINTER_ROOT,
                self.ui.folder_terminal,
                CURRENT_TIME,
            )?;
            self.write_folder_terminal(shell_quote(&path).as_bytes());
            self.redraw_folder_terminal()?;
            return Ok(());
        }
        if target == x11rb::NONE || target == self.ui.folder || self.is_ui_window(target) {
            return Ok(());
        }
        if let Some(client) = self.client_key_for(target) {
            if let Some(info) = self.clients.get(&client) {
                target = info.window;
            }
        }
        self.folder_drag = Some(path);
        let selection = self.atom(b"XdndSelection")?;
        self.conn
            .set_selection_owner(self.ui.folder, selection, CURRENT_TIME)?;
        let xdnd_enter = self.atom(b"XdndEnter")?;
        let xdnd_position = self.atom(b"XdndPosition")?;
        let xdnd_drop = self.atom(b"XdndDrop")?;
        let uri = self.atom(b"text/uri-list")?;
        let action_copy = self.atom(b"XdndActionCopy")?;
        let packed_xy =
            ((u32::from(pointer.root_x as u16)) << 16) | u32::from(pointer.root_y as u16);
        self.conn.send_event(
            false,
            target,
            EventMask::NO_EVENT,
            ClientMessageEvent::new(32, target, xdnd_enter, [self.ui.folder, 5 << 24, uri, 0, 0]),
        )?;
        self.conn.send_event(
            false,
            target,
            EventMask::NO_EVENT,
            ClientMessageEvent::new(
                32,
                target,
                xdnd_position,
                [self.ui.folder, 0, packed_xy, CURRENT_TIME, action_copy],
            ),
        )?;
        self.conn.send_event(
            false,
            target,
            EventMask::NO_EVENT,
            ClientMessageEvent::new(
                32,
                target,
                xdnd_drop,
                [self.ui.folder, 0, CURRENT_TIME, 0, 0],
            ),
        )?;
        Ok(())
    }

    pub(crate) fn activate_folder_entry(&mut self, entry: FolderEntry) -> AnyResult<()> {
        match entry.kind {
            FileKind::Directory => {
                self.folder_path = entry.path.clone();
                self.folder_entries = folder_entries_in(entry.path, self.folder_sort);
                self.folder_selected = None;
                self.folder_info = None;
                self.folder_scroll = 0;
                self.sync_folder_terminal_cwd();
                self.redraw_folder()?;
                if self.folder_terminal.visible {
                    self.redraw_folder_terminal()?;
                }
            }
            FileKind::Text
            | FileKind::Image
            | FileKind::Audio
            | FileKind::Video
            | FileKind::Other => {
                if self.folder_selected.as_ref() == Some(&entry.path) {
                    if self.choose_file_mode {
                        self.submit_choose_file()?;
                    } else {
                        self.open_media(entry)?;
                    }
                } else {
                    self.folder_selected = Some(entry.path.clone());
                    self.folder_info = Some(folder_entry_info(&entry));
                    self.redraw_folder()?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn handle_folder_context(&mut self, x: i32, y: i32) -> AnyResult<()> {
        if y >= 86 {
            let idx = (y - 86) / 42;
            if idx >= 0 && (idx as usize) < self.folder_visible_row_count() {
                if let Some(entry) = self.folder_entries.get(self.folder_scroll + idx as usize) {
                    self.folder_selected = Some(entry.path.clone());
                }
            }
        }
        self.folder_context_open = true;
        self.folder_context_pos = (x, y);
        self.folder_more_open = false;
        self.folder_sort_open = false;
        self.redraw_folder()?;
        Ok(())
    }

    pub(crate) fn handle_folder_scroll(&mut self, button: u8) -> AnyResult<()> {
        let max_scroll = self
            .folder_entries
            .len()
            .saturating_sub(self.folder_visible_row_count());
        let old_scroll = self.folder_scroll;
        if button == 4 {
            self.folder_scroll = self.folder_scroll.saturating_sub(3);
        } else {
            self.folder_scroll = (self.folder_scroll + 3).min(max_scroll);
        }
        if self.folder_scroll == old_scroll {
            return Ok(());
        }
        self.redraw_folder()?;
        Ok(())
    }

    pub(crate) fn folder_sort_at(&self, x: i32, y: i32) -> Option<FolderSort> {
        let menu_x = 94;
        let menu_y = 54;
        if x < menu_x || x > menu_x + 122 || y < menu_y || y > menu_y + 96 {
            return None;
        }
        let idx = (y - menu_y - 8) / 28;
        match idx {
            0 => Some(FolderSort::Name),
            1 => Some(FolderSort::Date),
            2 => Some(FolderSort::Size),
            _ => None,
        }
    }

    pub(crate) fn refresh_folder_entries(&mut self) -> bool {
        let previous_entries = self.folder_entries.clone();
        let previous_scroll = self.folder_scroll;
        let previous_selected = self.folder_selected.clone();
        let anchor = self
            .folder_entries
            .get(self.folder_scroll)
            .map(|entry| entry.path.clone());

        let new_entries = self.current_folder_entries();
        if new_entries == self.folder_entries {
            self.clamp_folder_scroll();
            return self.folder_scroll != previous_scroll
                || self.folder_selected != previous_selected;
        }

        self.folder_entries = new_entries;
        if let Some(anchor) = anchor {
            if let Some(idx) = self
                .folder_entries
                .iter()
                .position(|entry| entry.path == anchor)
            {
                self.folder_scroll = idx;
            }
        }
        self.clamp_folder_scroll();
        self.folder_selected = self
            .folder_selected
            .take()
            .filter(|path| self.folder_entries.iter().any(|entry| &entry.path == path));

        self.folder_entries != previous_entries
            || self.folder_scroll != previous_scroll
            || self.folder_selected != previous_selected
    }

    pub(crate) fn current_folder_entries(&self) -> Vec<FolderEntry> {
        if self.folder_path == folder_path_for(self.folder_mode) {
            folder_entries_for(self.folder_mode, self.folder_sort)
        } else {
            folder_entries_in(self.folder_path.clone(), self.folder_sort)
        }
    }

    pub(crate) fn clamp_folder_scroll(&mut self) {
        self.folder_scroll = self.folder_scroll.min(
            self.folder_entries
                .len()
                .saturating_sub(self.folder_visible_row_count()),
        );
    }

    pub(crate) fn folder_visible_row_count(&self) -> usize {
        if self.choose_file_mode { 7 } else { 9 }
    }

    pub(crate) fn sync_folder_terminal_cwd(&mut self) {
        self.folder_terminal.cwd = self.folder_path.clone();
        if self.folder_terminal.master_fd.is_some() {
            let command = format!("cd {}\n", shell_quote(&self.folder_path));
            self.write_folder_terminal(command.as_bytes());
        }
    }

    pub(crate) fn sync_folder_to_terminal_cwd(&mut self) -> AnyResult<bool> {
        let Some(pid) = self.folder_terminal.child_pid else {
            return Ok(false);
        };
        let Ok(cwd) = fs::read_link(format!("/proc/{pid}/cwd")) else {
            return Ok(false);
        };
        if cwd == self.folder_terminal.cwd && cwd == self.folder_path {
            return Ok(false);
        }
        self.folder_terminal.cwd = cwd.clone();
        if cwd == self.folder_path || !cwd.is_dir() {
            if self.folder_terminal.visible {
                self.redraw_folder_terminal()?;
            }
            return Ok(false);
        }
        self.folder_mode = FolderMode::Home;
        self.folder_path = cwd;
        self.folder_entries = folder_entries_in(self.folder_path.clone(), self.folder_sort);
        self.folder_selected = None;
        self.folder_scroll = 0;
        self.folder_more_open = false;
        self.folder_sort_open = false;
        self.redraw_folder()?;
        if self.folder_terminal.visible {
            self.redraw_folder_terminal()?;
        }
        Ok(true)
    }

    pub(crate) fn set_topbar_notice(&mut self, message: &str, duration: Duration) -> AnyResult<()> {
        self.topbar_notice = Some((message.to_string(), Instant::now() + duration));
        self.redraw_topbar()?;
        Ok(())
    }

    pub(crate) fn handle_topbar_release(&mut self, _ev: ButtonReleaseEvent) -> AnyResult<()> {
        self.pending_screenshot_button = None;
        Ok(())
    }

    pub(crate) fn toggle_folder_terminal(&mut self) -> AnyResult<()> {
        self.folder_terminal.visible = !self.folder_terminal.visible;
        self.folder_terminal.focused = self.folder_terminal.visible;
        if self.folder_terminal.visible {
            self.ensure_folder_terminal_pty();
            self.sync_folder_terminal_cwd();
            let terminal = self.folder_terminal_geometry();
            self.conn.configure_window(
                self.ui.folder_terminal,
                &ConfigureWindowAux::new()
                    .x(i32::from(terminal.0))
                    .y(i32::from(terminal.1))
                    .width(u32::from(terminal.2))
                    .height(u32::from(terminal.3))
                    .stack_mode(StackMode::ABOVE),
            )?;
            self.conn.map_window(self.ui.folder_terminal)?;
            self.conn.set_input_focus(
                InputFocus::POINTER_ROOT,
                self.ui.folder_terminal,
                CURRENT_TIME,
            )?;
            self.redraw_folder_terminal()?;
        } else {
            self.conn.unmap_window(self.ui.folder_terminal)?;
            self.conn
                .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
        }
        self.redraw_folder()?;
        self.raise_ui()?;
        Ok(())
    }

}
