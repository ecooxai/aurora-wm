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
    pub(crate) fn handle_key_press(&mut self, ev: KeyPressEvent) -> AnyResult<()> {
        self.last_pointer_activity = Instant::now();
        let mapping = self.conn.get_keyboard_mapping(ev.detail, 1)?.reply()?;
        let base_keysym = mapping.keysyms.first().copied().unwrap_or(0);
        let state = u16::from(ev.state);
        let command = state & u16::from(ModMask::M4) != 0;
        let ctrl = state & u16::from(ModMask::CONTROL) != 0;
        let alt = state & u16::from(ModMask::M1) != 0;
        let shift = state & u16::from(ModMask::SHIFT) != 0;
        if command && !ctrl && !alt && !shift && matches!(base_keysym, 0x76 | 0x56) {
            paste_clipboard_now(&self.display);
            return Ok(());
        }
        if self.capture_shortcut_key(&ev)? {
            return Ok(());
        }
        if self.dispatch_shortcut(&ev)? {
            return Ok(());
        }
        if let Some(forward) = self.alt_tab_direction(&ev)? {
            self.switch_running_app(forward)?;
            return Ok(());
        }
        self.reset_alt_tab_sequence();
        if let Some(forward) = self.is_workspace_switch_key(&ev)? {
            let current = self.active_workspace;
            let count = self.workspace_count;
            if forward {
                if current + 1 < count {
                    self.switch_workspace(current + 1)?;
                }
            } else {
                if current > 0 {
                    self.switch_workspace(current - 1)?;
                }
            }
            return Ok(());
        }
        if ev.event == self.ui.folder_terminal && self.folder_terminal.visible {
            self.handle_folder_terminal_key(ev)?;
            return Ok(());
        }
        if let Some(slot) = self.media_slot_for_window(ev.event) {
            self.handle_media_key(slot, ev)?;
            return Ok(());
        }
        if ev.event != self.ui.settings {
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
        if self.settings.tab == SettingsTab::Network && self.settings.wifi_password_editing {
            match keysym {
                0xff08 => {
                    self.settings.wifi_password.pop();
                }
                0xff0d => {
                    self.connect_selected_wifi()?;
                    return Ok(());
                }
                0xff1b => {
                    self.settings.wifi_password.clear();
                    self.settings.wifi_password_editing = false;
                    self.conn
                        .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
                }
                0x20..=0x7e if self.settings.wifi_password.len() < 128 => {
                    self.settings
                        .wifi_password
                        .push(char::from_u32(keysym).unwrap());
                }
                _ => return Ok(()),
            }
            self.redraw_settings()?;
            return Ok(());
        }
        if self.settings.tab == SettingsTab::Power && self.settings.auto_power_saver_editing {
            let mut changed = false;
            match keysym {
                0xff08 => {
                    self.settings.auto_power_saver_input.pop();
                    changed = true;
                }
                0xff0d => {
                    self.settings.auto_power_saver_editing = false;
                    self.conn
                        .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
                }
                0xff1b => {
                    self.settings.auto_power_saver_input =
                        self.settings.auto_power_saver_minutes.to_string();
                    self.settings.auto_power_saver_editing = false;
                    self.pending_auto_power_saver_apply = None;
                    self.conn
                        .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
                }
                0x30..=0x39 if self.settings.auto_power_saver_input.len() < 3 => {
                    let digit = char::from_u32(keysym).unwrap();
                    if self.settings.auto_power_saver_input == "0" {
                        self.settings.auto_power_saver_input.clear();
                    }
                    self.settings.auto_power_saver_input.push(digit);
                    changed = true;
                }
                _ => return Ok(()),
            }
            if changed {
                self.settings.auto_power_saver_minutes = self
                    .settings
                    .auto_power_saver_input
                    .trim()
                    .parse::<u32>()
                    .unwrap_or(0)
                    .min(240);
                if self.settings.auto_power_saver_input.is_empty() {
                    self.settings.auto_power_saver_minutes = 0;
                }
                self.pending_auto_power_saver_apply = Some(Instant::now() + Duration::from_secs(3));
            }
            self.redraw_settings()?;
            return Ok(());
        }
        if self.settings.tab != SettingsTab::Apps
            || self.settings.app_kind != DefaultAppKind::Terminal
            || !self.settings.terminal_editing
        {
            return Ok(());
        }
        match keysym {
            0xff08 => {
                self.settings.terminal_command.pop();
            }
            0xff0d => {
                save_app_commands(&self.settings)?;
                self.settings.terminal_editing = false;
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
                let command = self.settings.terminal_command.clone();
                self.test_terminal_launch(&command, &command);
            }
            0xff1b => {
                self.settings.terminal_command = read_app_command(DefaultAppKind::Terminal);
                self.settings.terminal_editing = false;
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
            }
            0x20..=0x7e if self.settings.terminal_command.len() < 200 => {
                self.settings
                    .terminal_command
                    .push(char::from_u32(keysym).unwrap());
            }
            _ => return Ok(()),
        }
        self.redraw_settings()?;
        Ok(())
    }

    pub(crate) fn handle_key_release(&mut self, ev: KeyReleaseEvent) -> AnyResult<()> {
        let mapping = self.conn.get_keyboard_mapping(ev.detail, 1)?.reply()?;
        if mapping.keysyms.contains(&0xffe9) || mapping.keysyms.contains(&0xffea) {
            self.reset_alt_tab_sequence();
        }
        Ok(())
    }

    pub(crate) fn alt_tab_direction(&self, ev: &KeyPressEvent) -> AnyResult<Option<bool>> {
        let is_alt = u16::from(ev.state) & u16::from(KeyButMask::MOD1) != 0;
        let is_shift = u16::from(ev.state) & u16::from(KeyButMask::SHIFT) != 0;
        if !is_alt {
            return Ok(None);
        }
        let mapping = self.conn.get_keyboard_mapping(ev.detail, 1)?.reply()?;
        if mapping.keysyms.contains(&0xff09) {
            Ok(Some(!is_shift))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn is_workspace_switch_key(&self, ev: &KeyPressEvent) -> AnyResult<Option<bool>> {
        if u16::from(ev.state) & u16::from(KeyButMask::MOD4) == 0 {
            return Ok(None);
        }
        let mapping = self.conn.get_keyboard_mapping(ev.detail, 1)?.reply()?;
        if mapping.keysyms.contains(&0xff51) {
            return Ok(Some(false)); // Left
        } else if mapping.keysyms.contains(&0xff53) {
            return Ok(Some(true)); // Right
        }
        Ok(None)
    }

    pub(crate) fn reset_alt_tab_sequence(&mut self) {
        self.alt_tab_index = 0;
        self.alt_tab_windows.clear();
    }

    pub(crate) fn alt_tab_start_window(
        &self,
        windows: &[Window],
        active: Option<Window>,
        forward: bool,
    ) -> Option<usize> {
        if windows.is_empty() {
            return None;
        }
        if let Some(previous) = self
            .focus_history
            .iter()
            .rev()
            .copied()
            .find(|&window| Some(window) != active && windows.contains(&window))
        {
            return windows.iter().position(|&window| window == previous);
        }
        active
            .and_then(|window| windows.iter().position(|&candidate| candidate == window))
            .map(|pos| {
                if forward {
                    (pos + 1) % windows.len()
                } else {
                    (pos + windows.len() - 1) % windows.len()
                }
            })
            .or(Some(0))
    }

    pub(crate) fn build_alt_tab_sequence(
        &self,
        windows: &[Window],
        active: Option<Window>,
        forward: bool,
    ) -> Vec<Window> {
        let Some(start) = self.alt_tab_start_window(windows, active, forward) else {
            return Vec::new();
        };
        (0..windows.len())
            .map(|offset| windows[(start + offset) % windows.len()])
            .collect()
    }

    pub(crate) fn switch_running_app(&mut self, forward: bool) -> AnyResult<()> {
        let windows = self.task_client_windows();
        if windows.is_empty() {
            self.reset_alt_tab_sequence();
            return Ok(());
        }
        let active = self.active_client;
        let needs_new_sequence = self.alt_tab_windows.is_empty()
            || self.alt_tab_index >= self.alt_tab_windows.len()
            || self.alt_tab_windows.len() != windows.len()
            || active.is_some_and(|client| {
                self.alt_tab_windows.get(self.alt_tab_index) != Some(&client)
            })
            || self
                .alt_tab_windows
                .iter()
                .any(|window| !windows.contains(window));
        if needs_new_sequence {
            self.alt_tab_windows = self.build_alt_tab_sequence(&windows, active, forward);
            self.alt_tab_index = 0;
        } else {
            let len = self.alt_tab_windows.len();
            self.alt_tab_index = if forward {
                (self.alt_tab_index + 1) % len
            } else {
                (self.alt_tab_index + len - 1) % len
            };
        }
        let Some(&next) = self.alt_tab_windows.get(self.alt_tab_index) else {
            return Ok(());
        };
        self.focus_window(next)?;
        self.redraw_dock()?;
        self.conn.flush()?;
        Ok(())
    }

}
