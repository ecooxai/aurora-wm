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
use crate::files::*;

impl Aurora {
    pub(crate) fn take_workspace_ui_state(&mut self) -> WorkspaceUiState {
        WorkspaceUiState {
            folder_mode: self.folder_mode,
            folder_entries: std::mem::take(&mut self.folder_entries),
            folder_path: std::mem::take(&mut self.folder_path),
            folder_selected: self.folder_selected.take(),
            folder_scroll: self.folder_scroll,
            folder_front: self.folder_front,
            folder_more_open: self.folder_more_open,
            folder_sort_open: self.folder_sort_open,
            folder_sort: self.folder_sort,
            folder_width: self.folder_width,
            folder_height: self.folder_height,
            folder_terminal_width: self.folder_terminal_width,
            folder_terminal_height: self.folder_terminal_height,
            folder_terminal: std::mem::replace(
                &mut self.folder_terminal,
                FolderTerminal::new(folder_path_for(FolderMode::Home)),
            ),
            media: self.media.take(),
            media_slots: std::mem::take(&mut self.media_slots),
            media_front: self.media_front,
            media_front_slot: self.media_front_slot,
            media_text_selection: self.media_text_selection.take(),
            media_text_selecting: self.media_text_selecting,
            media_text_selection_redraw_at: self.media_text_selection_redraw_at.take(),
            media_text_live_rects: std::mem::take(&mut self.media_text_live_rects),
            media_context_open: self.media_context_open,
            media_trash_prompt: self.media_trash_prompt,
            folder_context_open: self.folder_context_open,
            folder_context_pos: self.folder_context_pos,
            folder_clipboard: self.folder_clipboard.take(),
            folder_info: self.folder_info.take(),
            folder_terminal_selection: self.folder_terminal_selection.take(),
            folder_terminal_selecting: self.folder_terminal_selecting,
            folder_terminal_live_rects: std::mem::take(&mut self.folder_terminal_live_rects),
            folder_drag: self.folder_drag.take(),
            folder_press: self.folder_press.take(),
        }
    }

    pub(crate) fn apply_workspace_ui_state(&mut self, state: WorkspaceUiState) {
        self.folder_mode = state.folder_mode;
        self.folder_entries = state.folder_entries;
        self.folder_path = state.folder_path;
        self.folder_selected = state.folder_selected;
        self.folder_scroll = state.folder_scroll;
        self.folder_front = state.folder_front;
        self.folder_more_open = state.folder_more_open;
        self.folder_sort_open = state.folder_sort_open;
        self.folder_sort = state.folder_sort;
        self.folder_width = state.folder_width;
        self.folder_height = state.folder_height;
        self.folder_terminal_width = state.folder_terminal_width;
        self.folder_terminal_height = state.folder_terminal_height;
        self.folder_terminal = state.folder_terminal;
        self.media = state.media;
        self.media_slots = state.media_slots;
        self.media_front = state.media_front;
        self.media_front_slot = state.media_front_slot;
        self.media_text_selection = state.media_text_selection;
        self.media_text_selecting = state.media_text_selecting;
        self.media_text_selection_redraw_at = state.media_text_selection_redraw_at;
        self.media_text_live_rects = state.media_text_live_rects;
        self.media_context_open = state.media_context_open;
        self.media_trash_prompt = state.media_trash_prompt;
        self.folder_context_open = state.folder_context_open;
        self.folder_context_pos = state.folder_context_pos;
        self.folder_clipboard = state.folder_clipboard;
        self.folder_info = state.folder_info;
        self.folder_terminal_selection = state.folder_terminal_selection;
        self.folder_terminal_selecting = state.folder_terminal_selecting;
        self.folder_terminal_live_rects = state.folder_terminal_live_rects;
        self.folder_drag = state.folder_drag;
        self.folder_press = state.folder_press;
    }

    pub(crate) fn restore_workspace_ui_windows(&mut self) -> AnyResult<()> {
        let folder = self.folder_geometry();
        let terminal = self.folder_terminal_geometry();
        self.conn.configure_window(
            self.ui.folder,
            &ConfigureWindowAux::new()
                .x(i32::from(folder.0))
                .y(i32::from(folder.1))
                .width(u32::from(folder.2))
                .height(u32::from(folder.3)),
        )?;
        self.conn.configure_window(
            self.ui.folder_terminal,
            &ConfigureWindowAux::new()
                .x(i32::from(terminal.0))
                .y(i32::from(terminal.1))
                .width(u32::from(terminal.2))
                .height(u32::from(terminal.3)),
        )?;
        self.sync_folder_terminal_size();
        if self.choose_file_mode {
            self.conn.map_window(self.ui.folder)?;
        } else {
            self.conn.unmap_window(self.ui.folder)?;
        }
        if self.choose_file_mode && self.folder_terminal.visible {
            self.conn.map_window(self.ui.folder_terminal)?;
        } else {
            self.conn.unmap_window(self.ui.folder_terminal)?;
        }
        for (idx, window) in self.ui.media.iter().copied().enumerate() {
            if self.media_slots.get(idx).and_then(|m| m.as_ref()).is_some() {
                self.conn.map_window(window)?;
            } else {
                self.conn.unmap_window(window)?;
            }
        }
        if self.choose_file_mode {
            self.redraw_folder()?;
        }
        if self.choose_file_mode && self.folder_terminal.visible {
            self.redraw_folder_terminal()?;
        }
        for slot in 0..MEDIA_SLOT_COUNT {
            if self
                .media_slots
                .get(slot)
                .and_then(|m| m.as_ref())
                .is_some()
            {
                self.redraw_media_slot(slot)?;
            }
        }
        Ok(())
    }

    pub(crate) fn add_workspace(&mut self) -> AnyResult<()> {
        if self.workspace_count >= MAX_WORKSPACE_COUNT {
            return Ok(());
        }
        let workspace = self.workspace_count;
        self.workspace_count += 1;
        self.workspace_ui
            .push(WorkspaceUiState::new(self.screen_height));

        // Update EWMH _NET_NUMBER_OF_DESKTOPS
        if let Ok(num_atom) = self.atom(b"_NET_NUMBER_OF_DESKTOPS") {
            if let Ok(cardinal_atom) = self.atom(b"CARDINAL") {
                let _ = self.conn.change_property32(
                    PropMode::REPLACE,
                    self.root,
                    num_atom,
                    cardinal_atom,
                    &[self.workspace_count as u32],
                );
            }
        }

        self.switch_workspace(workspace)
    }

    pub(crate) fn switch_workspace(&mut self, workspace: usize) -> AnyResult<()> {
        if workspace >= self.workspace_count || workspace == self.active_workspace {
            return Ok(());
        }
        if self.choose_file_mode {
            let _ = self.cancel_choose_file();
        }
        self.end_drag()?;
        let previous = self.active_workspace;
        let hidden_frames = self
            .clients
            .values()
            .filter(|info| info.workspace == previous && info.mapped && !info.sticky)
            .map(|info| info.frame)
            .collect::<Vec<_>>();
        let shown_frames = self
            .clients
            .values()
            .filter(|info| info.workspace == workspace && info.mapped && !info.sticky)
            .map(|info| info.frame)
            .collect::<Vec<_>>();
        for frame in hidden_frames {
            self.ignored_unmaps.push(frame);
            self.conn.unmap_window(frame)?;
        }
        while self.workspace_ui.len() <= workspace {
            self.workspace_ui
                .push(WorkspaceUiState::new(self.screen_height));
        }
        let previous_ui = self.take_workspace_ui_state();
        if let Some(slot) = self.workspace_ui.get_mut(previous) {
            *slot = previous_ui;
        }
        let next_ui = std::mem::replace(
            &mut self.workspace_ui[workspace],
            WorkspaceUiState::new(self.screen_height),
        );
        self.apply_workspace_ui_state(next_ui);
        self.active_workspace = workspace;

        // Update EWMH _NET_CURRENT_DESKTOP
        if let Ok(cur_atom) = self.atom(b"_NET_CURRENT_DESKTOP") {
            if let Ok(cardinal_atom) = self.atom(b"CARDINAL") {
                let _ = self.conn.change_property32(
                    PropMode::REPLACE,
                    self.root,
                    cur_atom,
                    cardinal_atom,
                    &[self.active_workspace as u32],
                );
            }
        }

        for frame in shown_frames {
            self.conn.map_window(frame)?;
        }
        self.hide_dock_more_menu()?;
        self.dock_last_click = None;
        self.active_client = None;
        self.update_active_window_property()?;
        self.conn
            .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
        self.restore_workspace_ui_windows()?;
        self.redraw_topbar()?;
        self.redraw_dock()?;
        self.raise_ui()
    }

    pub(crate) fn move_client_to_workspace(
        &mut self,
        client: Window,
        workspace: usize,
    ) -> AnyResult<()> {
        if workspace >= self.workspace_count {
            return Ok(());
        }
        let Some(mut info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        if info.workspace == workspace {
            return Ok(());
        }
        info.workspace = workspace;
        self.clients.insert(client, info);

        // Keep EWMH _NET_WM_DESKTOP in sync on both the client and its frame.
        if let Ok(desktop_atom) = self.atom(b"_NET_WM_DESKTOP") {
            if let Ok(cardinal_atom) = self.atom(b"CARDINAL") {
                let _ = self.conn.change_property32(
                    PropMode::REPLACE,
                    info.window,
                    desktop_atom,
                    cardinal_atom,
                    &[workspace as u32],
                );
                let _ = self.conn.change_property32(
                    PropMode::REPLACE,
                    info.frame,
                    desktop_atom,
                    cardinal_atom,
                    &[workspace as u32],
                );
            }
        }

        // If it left the active workspace (and isn't sticky), hide it there.
        if !info.sticky && workspace != self.active_workspace && info.mapped {
            self.ignored_unmaps.push(info.frame);
            self.conn.unmap_window(info.frame)?;
            if self.active_client == Some(client) {
                self.active_client = None;
                self.update_active_window_property()?;
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
            }
        }
        self.redraw_dock()?;
        Ok(())
    }

    pub(crate) fn open_settings_tab(&mut self, tab: SettingsTab) -> AnyResult<()> {
        if self.settings_visible && self.settings.tab == tab {
            if !self.settings_front {
                // Settings is showing but behind another window: bring it to
                // the front and give it focus instead of closing it.
                self.settings_front = true;
                self.folder_front = false;
                self.media_front = false;
                self.raise_ui()?;
                self.conn.set_input_focus(
                    InputFocus::POINTER_ROOT,
                    self.ui.settings,
                    CURRENT_TIME,
                )?;
                self.redraw_topbar()?;
                return Ok(());
            }
            // Already focused: the button acts as a close toggle.
            self.settings_visible = false;
            self.settings_front = false;
            self.settings_hidden_at = Some(Instant::now());
            self.conn.unmap_window(self.ui.settings)?;
            self.redraw_topbar()?;
            return Ok(());
        }
        self.settings.tab = tab;
        self.settings.scroll = 0;
        self.settings_visible = true;
        self.settings_front = true;
        self.settings_hidden_at = None;
        self.folder_front = false;
        self.media_front = false;
        self.conn.map_window(self.ui.settings)?;
        self.raise_ui()?;
        if tab == SettingsTab::Network {
            self.ensure_wifi_refresh_started(false);
        }
        self.request_settings_data(tab);
        self.redraw_settings()?;
        self.redraw_topbar()
    }

}
