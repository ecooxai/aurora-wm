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
    /// Kick off background loading of the slow data a settings tab needs.
    /// The tab draws immediately from `settings_cache` (showing a loading
    /// hint when empty) and is redrawn when the fresh data arrives.
    pub(crate) fn request_settings_data(&mut self, tab: SettingsTab) {
        match tab {
            SettingsTab::Audio => {
                if self.settings_data_pending & settings_pending::AUDIO == 0 {
                    self.settings_data_pending |= settings_pending::AUDIO;
                    let tx = self.settings_data_tx.clone();
                    thread::spawn(move || {
                        let _ = tx.send(SettingsData::Audio {
                            volume: read_audio_volume_percent(),
                            outputs: read_audio_devices(AudioDeviceKind::Output),
                            inputs: read_audio_devices(AudioDeviceKind::Input),
                        });
                    });
                }
            }
            SettingsTab::Network => {
                if self.settings_data_pending & settings_pending::NETWORK == 0 {
                    self.settings_data_pending |= settings_pending::NETWORK;
                    let tx = self.settings_data_tx.clone();
                    thread::spawn(move || {
                        let _ = tx.send(SettingsData::Network(read_network_details()));
                    });
                }
            }
            SettingsTab::Bluetooth => {
                if self.settings_data_pending & settings_pending::BLUETOOTH == 0 {
                    self.settings_data_pending |= settings_pending::BLUETOOTH;
                    let tx = self.settings_data_tx.clone();
                    thread::spawn(move || {
                        let _ = tx.send(SettingsData::Bluetooth(read_bluetooth_devices()));
                    });
                }
            }
            SettingsTab::Startup => {
                if self.settings_data_pending & settings_pending::AUTOSTART == 0 {
                    self.settings_data_pending |= settings_pending::AUTOSTART;
                    let tx = self.settings_data_tx.clone();
                    thread::spawn(move || {
                        let _ = tx.send(SettingsData::Autostart(read_autostart_apps()));
                    });
                }
            }
            SettingsTab::Power | SettingsTab::About => {
                if self.settings_data_pending & settings_pending::GPU == 0 {
                    self.settings_data_pending |= settings_pending::GPU;
                    let tx = self.settings_data_tx.clone();
                    thread::spawn(move || {
                        let _ = tx.send(SettingsData::GpuUsage(read_gpu_usage()));
                    });
                }
            }
            _ => {}
        }
    }

    /// Apply results from settings background loaders. Returns true when the
    /// visible settings tab was redrawn.
    pub(crate) fn poll_settings_data(&mut self) -> AnyResult<bool> {
        let mut redraw_tabs: [bool; 4] = [false; 4]; // audio, network, bt, startup
        let mut gpu_updated = false;
        loop {
            match self.settings_data_rx.try_recv() {
                Ok(SettingsData::Audio {
                    volume,
                    outputs,
                    inputs,
                }) => {
                    self.settings_data_pending &= !settings_pending::AUDIO;
                    self.settings_cache.audio_volume = Some(volume);
                    self.settings_cache.audio_outputs = Some(outputs);
                    self.settings_cache.audio_inputs = Some(inputs);
                    redraw_tabs[0] = true;
                }
                Ok(SettingsData::Network(details)) => {
                    self.settings_data_pending &= !settings_pending::NETWORK;
                    self.settings_cache.network_details = Some(details);
                    redraw_tabs[1] = true;
                }
                Ok(SettingsData::Bluetooth(devices)) => {
                    self.settings_data_pending &= !settings_pending::BLUETOOTH;
                    self.settings_cache.bluetooth_devices = Some(devices);
                    redraw_tabs[2] = true;
                }
                Ok(SettingsData::Autostart(apps)) => {
                    self.settings_data_pending &= !settings_pending::AUTOSTART;
                    self.settings_cache.autostart_apps = Some(apps);
                    redraw_tabs[3] = true;
                }
                Ok(SettingsData::GpuUsage(usage)) => {
                    self.settings_data_pending &= !settings_pending::GPU;
                    self.metrics.gpu_usage = usage.clone();
                    self.settings_cache.gpu_usage = Some(usage);
                    gpu_updated = true;
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        if !self.settings_visible {
            return Ok(false);
        }
        let should_redraw = match self.settings.tab {
            SettingsTab::Audio => redraw_tabs[0],
            SettingsTab::Network => redraw_tabs[1],
            SettingsTab::Bluetooth => redraw_tabs[2],
            SettingsTab::Startup => redraw_tabs[3],
            SettingsTab::Power | SettingsTab::About => gpu_updated,
            _ => false,
        };
        if should_redraw {
            self.redraw_settings()?;
        }
        Ok(should_redraw)
    }

    pub(crate) fn handle_settings_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        if self.settings.tab == SettingsTab::Power && self.settings.auto_power_saver_editing {
            let sx = SIDEBAR_WIDTH + 24;
            let input_x = sx + 16;
            let inside_input = y >= 132 && y <= 162 && x >= input_x && x <= input_x + 118;
            if !inside_input {
                self.settings.auto_power_saver_editing = false;
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
            }
        }
        if x < SIDEBAR_WIDTH {
            if y < SETTINGS_SIDEBAR_TOP - 4 {
                return Ok(());
            }
            let tab = match (y - (SETTINGS_SIDEBAR_TOP - 4)) / 48 {
                0 => Some(SettingsTab::Display),
                1 => Some(SettingsTab::Power),
                2 => Some(SettingsTab::Wallpaper),
                3 => Some(SettingsTab::Audio),
                4 => Some(SettingsTab::Network),
                5 => Some(SettingsTab::Bluetooth),
                6 => Some(SettingsTab::Startup),
                7 => Some(SettingsTab::Apps),
                8 => Some(SettingsTab::Shortcuts),
                9 => Some(SettingsTab::About),
                _ => None,
            };
            if let Some(tab) = tab {
                self.settings.tab = tab;
                self.settings.scroll = 0;
                if tab == SettingsTab::Network {
                    self.ensure_wifi_refresh_started(false);
                }
                // Draw the tab shell immediately from cached data; heavy
                // content loads in the background and fills in when ready.
                self.request_settings_data(tab);
                self.redraw_settings()?;
                self.redraw_topbar()?;
            }
            return Ok(());
        }

        match self.settings.tab {
            SettingsTab::Display => self.handle_display_click(x, y)?,
            SettingsTab::Power => self.handle_power_click(x, y)?,
            SettingsTab::Wallpaper => self.handle_wallpaper_click(y)?,
            SettingsTab::Audio => self.handle_audio_click(x, y)?,
            SettingsTab::Bluetooth if y >= 224 && y <= 300 => {
                self.spawn_first_available(&["blueman-manager", "bluetoothctl"], &[]);
            }
            SettingsTab::Apps => self.handle_apps_click(x, y)?,
            SettingsTab::Shortcuts => self.handle_shortcuts_click(x, y)?,
            SettingsTab::Network => self.handle_network_click(x, y)?,
            SettingsTab::Bluetooth | SettingsTab::Startup => {}
            SettingsTab::About => {}
        }
        Ok(())
    }

    pub(crate) fn handle_settings_scroll(&mut self, button: u8, x: i32, y: i32) -> AnyResult<()> {
        if x <= SIDEBAR_WIDTH {
            return Ok(());
        }
        if self.settings.tab == SettingsTab::Network
            && self.handle_wifi_list_scroll(button, x, y)?
        {
            return Ok(());
        }
        let max_scroll = match self.settings.tab {
            SettingsTab::Network => {
                let lines = self
                    .settings_cache
                    .network_details
                    .as_ref()
                    .map(Vec::len)
                    .unwrap_or(0);
                (lines.saturating_sub(4) * 24) as i32
            }
            SettingsTab::Startup | SettingsTab::About => 180,
            SettingsTab::Shortcuts => 0,
            SettingsTab::Audio | SettingsTab::Wallpaper => 80,
            SettingsTab::Apps => self
                .available_apps(self.settings.app_kind)
                .len()
                .saturating_sub(6)
                .saturating_mul(29) as i32,
            SettingsTab::Display => 120,
            SettingsTab::Power | SettingsTab::Bluetooth => 40,
        };
        let old_scroll = self.settings.scroll;
        let step = if self.settings.tab == SettingsTab::Apps {
            29
        } else if self.settings.tab == SettingsTab::Network {
            24
        } else {
            36
        };
        if button == 4 {
            self.settings.scroll = self.settings.scroll.saturating_sub(step);
        } else {
            self.settings.scroll = (self.settings.scroll + step).min(max_scroll);
        }
        if self.settings.scroll == old_scroll {
            return Ok(());
        }
        self.redraw_settings()?;
        Ok(())
    }

    pub(crate) fn handle_wifi_list_scroll(&mut self, button: u8, x: i32, y: i32) -> AnyResult<bool> {
        if button != 4 && button != 5 {
            return Ok(false);
        }
        let sx = SIDEBAR_WIDTH + 24;
        let card_w = i32::from(self.settings_geometry().2) - sx - 24;
        let list_start_y = if self
            .settings
            .wifi_connected
            .as_ref()
            .is_some_and(Option::is_some)
        {
            376
        } else {
            344
        };
        let list_end_y = list_start_y + 5 * 24;
        if x < sx + 12 || x > sx + card_w - 12 || y < list_start_y || y >= list_end_y {
            return Ok(false);
        }
        let max_scroll = self.settings.wifi_networks.len().saturating_sub(5);
        let old_scroll = self.settings.wifi_scroll;
        if button == 4 {
            self.settings.wifi_scroll = self.settings.wifi_scroll.saturating_sub(1);
        } else {
            self.settings.wifi_scroll = (self.settings.wifi_scroll + 1).min(max_scroll);
        }
        if self.settings.wifi_scroll != old_scroll {
            self.redraw_settings()?;
        }
        Ok(true)
    }

    pub(crate) fn handle_display_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        let sx = SIDEBAR_WIDTH + 24;
        if x >= sx + 14 && x <= i32::from(self.settings_geometry().2) - 24 {
            for idx in 0..self.display_modes.len().min(4) {
                let row_y = 132 + idx as i32 * 25;
                if y >= row_y - 6 && y <= row_y + 18 {
                    self.settings.selected_mode = idx;
                    self.apply_display_mode(idx);
                    self.redraw_settings()?;
                    return Ok(());
                }
            }
        }
        let compositor_switch_x = i32::from(self.settings_geometry().2) - 78;
        if y >= 274 && y <= 302 && x >= compositor_switch_x && x <= compositor_switch_x + 40 {
            self.set_compositor_enabled(!self.settings.compositor_enabled)?;
            self.redraw_settings()?;
            return Ok(());
        }
        let bar_x = sx + 16;
        let bar_w = 230;
        if x >= bar_x && x <= bar_x + bar_w && (404..=432).contains(&y) {
            let percent = (10 + ((x - bar_x) * 90) / bar_w).clamp(10, 100) as u8;
            self.settings.brightness_percent = percent;
            self.settings.display_status = match self.apply_brightness_all(percent) {
                Ok(()) => Some(format!("Brightness set to {percent}%")),
                Err(err) => Some(err),
            };
            save_app_commands(&self.settings)?;
            self.redraw_settings()?;
            return Ok(());
        }
        if y >= 509 && y <= 542 {
            if x >= sx + 16 && x <= sx + 50 {
                self.settings.sleep_after_secs =
                    self.settings.sleep_after_secs.saturating_sub(60).max(0);
                self.apply_sleep_timeout();
                save_app_commands(&self.settings)?;
                self.redraw_settings()?;
            } else if x >= sx + 174 && x <= sx + 212 {
                self.settings.sleep_after_secs = (self.settings.sleep_after_secs + 60).min(7200);
                self.apply_sleep_timeout();
                save_app_commands(&self.settings)?;
                self.redraw_settings()?;
            }
        }
        Ok(())
    }

    pub(crate) fn handle_audio_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        let sx = SIDEBAR_WIDTH + 24;
        let bar_x = sx + 16;
        let bar_w = 230;
        if x >= bar_x && x <= bar_x + bar_w && (132..=162).contains(&y) {
            let percent = (((x - bar_x) * 100) / bar_w).clamp(0, 100) as u8;
            self.settings.audio_status = match set_audio_volume_percent(percent) {
                Ok(()) => {
                    self.settings_cache.audio_volume = Some(Some(percent));
                    Some(format!("Volume set to {percent}%"))
                }
                Err(err) => Some(err),
            };
            self.redraw_settings()?;
        }
        let card_w = i32::from(self.settings_geometry().2) - sx - 24;
        if x >= sx + 12 && x <= sx + card_w - 12 {
            let outputs = self
                .settings_cache
                .audio_outputs
                .clone()
                .unwrap_or_default();
            let inputs = self
                .settings_cache
                .audio_inputs
                .clone()
                .unwrap_or_default();
            for (idx, dev) in outputs.iter().take(3).enumerate() {
                let row_y = 260 + idx as i32 * 30;
                if y >= row_y - 3 && y <= row_y + 21 {
                    self.settings.audio_status =
                        match set_default_audio_device(AudioDeviceKind::Output, dev) {
                            Ok(()) => Some(format!("Output set to {}", dev.label)),
                            Err(err) => Some(err),
                        };
                    self.settings_data_pending &= !settings_pending::AUDIO;
                    self.request_settings_data(SettingsTab::Audio);
                    self.redraw_settings()?;
                    return Ok(());
                }
            }
            for (idx, dev) in inputs.iter().take(2).enumerate() {
                let row_y = 432 + idx as i32 * 30;
                if y >= row_y - 3 && y <= row_y + 21 {
                    self.settings.audio_status =
                        match set_default_audio_device(AudioDeviceKind::Input, dev) {
                            Ok(()) => Some(format!("Input set to {}", dev.label)),
                            Err(err) => Some(err),
                        };
                    self.settings_data_pending &= !settings_pending::AUDIO;
                    self.request_settings_data(SettingsTab::Audio);
                    self.redraw_settings()?;
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    pub(crate) fn handle_network_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        let sx = SIDEBAR_WIDTH + 24;
        let card_w = i32::from(self.settings_geometry().2) - sx - 24;

        // Clicks on the Refresh text button next to Wi-Fi title
        if y >= 273 && y <= 297 && x >= sx + 75 && x <= sx + 133 {
            self.settings.wifi_disconnect_confirm = false;
            self.start_wifi_refresh(true);
            self.redraw_settings()?;
            return Ok(());
        }

        // Clicks on the Disconnect text button next to Wi-Fi title
        if y >= 273 && y <= 297 && x >= sx + 141 && x <= sx + 219 {
            if self
                .settings
                .wifi_connected
                .as_ref()
                .is_some_and(Option::is_some)
            {
                self.settings.wifi_disconnect_confirm = true;
                self.settings.wifi_password_editing = false;
                self.redraw_settings()?;
                return Ok(());
            }
        }

        // Clicks on the new On/Off toggle switch next to Wi-Fi title
        if y >= 273 && y <= 297 && x >= sx + 227 && x <= sx + 267 {
            let current_enabled = self.settings.wifi_radio_enabled.unwrap_or(true);
            if let Err(e) = set_wifi_radio_enabled(!current_enabled) {
                self.settings.wifi_status = Some(format!("Error setting Wi-Fi radio: {e}"));
            } else {
                self.settings.wifi_status = Some(format!(
                    "Wi-Fi turned {}",
                    if !current_enabled { "on" } else { "off" }
                ));
                self.settings.wifi_radio_enabled = Some(!current_enabled);
                if !current_enabled {
                    self.start_wifi_refresh(true);
                } else {
                    self.wifi_refresh_rx = None;
                    self.settings.wifi_networks.clear();
                    self.settings.wifi_scroll = 0;
                    self.settings.wifi_selected = None;
                    self.settings.wifi_connected = Some(None);
                }
            }
            self.redraw_settings()?;
            return Ok(());
        }

        if self.settings.wifi_disconnect_confirm {
            let list_start_y = if self
                .settings
                .wifi_connected
                .as_ref()
                .is_some_and(Option::is_some)
            {
                376
            } else {
                344
            };
            if y >= list_start_y + 8
                && y <= list_start_y + 36
                && x >= sx + card_w - 194
                && x <= sx + card_w - 118
            {
                self.settings.wifi_disconnect_confirm = false;
                self.redraw_settings()?;
                return Ok(());
            }
            if y >= list_start_y + 8
                && y <= list_start_y + 36
                && x >= sx + card_w - 108
                && x <= sx + card_w - 20
            {
                self.disconnect_wifi()?;
                return Ok(());
            }
            self.settings.wifi_disconnect_confirm = false;
            self.redraw_settings()?;
            return Ok(());
        }

        // Dynamic Wi-Fi list coordinates
        let list_start_y = if self
            .settings
            .wifi_connected
            .as_ref()
            .is_some_and(Option::is_some)
        {
            376
        } else {
            344
        };
        let list_end_y = list_start_y + 5 * 24;
        if y >= list_start_y && y < list_end_y {
            let idx = self.settings.wifi_scroll + ((y - list_start_y) / 24) as usize;
            if let Some(network) = self.settings.wifi_networks.get(idx) {
                self.settings.wifi_selected = Some(network.ssid.clone());
                self.settings.wifi_password.clear();
                self.settings.wifi_password_editing = true;
                self.settings.wifi_disconnect_confirm = false;
                self.settings.wifi_status = Some(format!("Selected {}", network.ssid));
                self.conn.set_input_focus(
                    InputFocus::POINTER_ROOT,
                    self.ui.settings,
                    CURRENT_TIME,
                )?;
                self.redraw_settings()?;
                return Ok(());
            }
        }

        if self.settings.wifi_selected.is_some() {
            let input_x = sx + 16;
            let button_w = 34; // Updated button_w to 34
            let gap = 12;
            let input_w = (card_w - 32 - button_w - gap).max(132);
            let input_y = 508;
            let inside_input =
                y >= input_y && y <= input_y + 34 && x >= input_x && x <= input_x + input_w;
            let button_x = input_x + input_w + gap;
            let inside_button =
                y >= input_y && y <= input_y + 34 && x >= button_x && x <= button_x + button_w;

            if inside_input {
                self.settings.wifi_password_editing = true;
                self.conn.set_input_focus(
                    InputFocus::POINTER_ROOT,
                    self.ui.settings,
                    CURRENT_TIME,
                )?;
                self.redraw_settings()?;
            } else if inside_button {
                self.connect_selected_wifi()?;
            } else if self.settings.wifi_password_editing {
                self.settings.wifi_password_editing = false;
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
                self.redraw_settings()?;
            }
        }
        Ok(())
    }

    pub(crate) fn handle_power_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        let width = i32::from(self.settings_geometry().2);
        let switch_x = width - 78;
        if y >= 98 && y <= 122 && x >= switch_x && x <= switch_x + 40 {
            self.settings.auto_power_saver_enabled = !self.settings.auto_power_saver_enabled;
            self.settings.auto_power_saver_editing = false;
            self.pending_auto_power_saver_apply = None;
            self.last_pointer_activity = Instant::now();
            self.last_pointer_pos = None;
            if self.settings.auto_power_saver_enabled && self.settings.auto_power_saver_minutes == 0
            {
                self.settings.auto_power_saver_minutes = 50;
                self.settings.auto_power_saver_input = "50".to_string();
            }
            if self.settings.auto_power_saver_enabled && self.settings.auto_power_saver_minutes > 0
            {
                touch_notidle_marker()?;
                self.set_power_mode(PowerMode::Performance)?;
            }
            save_app_commands(&self.settings)?;
            self.redraw_settings()?;
            return Ok(());
        }
        let sx = SIDEBAR_WIDTH + 24;
        let input_x = sx + 16;
        if y >= 132 && y <= 162 && x >= input_x && x <= input_x + 118 {
            self.settings.auto_power_saver_editing = true;
            self.settings.auto_power_saver_input.clear();
            self.conn
                .set_input_focus(InputFocus::POINTER_ROOT, self.ui.settings, CURRENT_TIME)?;
            self.redraw_settings()?;
            return Ok(());
        }
        let slider_left = input_x;
        let slider_right = width - 40;
        if y >= 168 && y <= 196 && x >= slider_left - 8 && x <= slider_right + 8 {
            self.settings.auto_power_saver_slider_dragging = true;
            self.settings.auto_power_saver_editing = false;
            self.set_auto_power_saver_from_slider(x)?;
            return Ok(());
        }
        let modes = [
            PowerMode::Saver,
            PowerMode::Balanced,
            PowerMode::Performance,
        ];
        for (idx, mode) in modes.iter().enumerate() {
            let row_y = 228 + idx as i32 * 22;
            if y >= row_y - 7 && y <= row_y + 18 {
                self.settings.auto_power_saver_editing = false;
                self.pending_auto_power_saver_apply = None;
                self.set_power_mode(*mode)?;
                self.redraw_settings()?;
                return Ok(());
            }
        }
        Ok(())
    }

    pub(crate) fn set_auto_power_saver_from_slider(&mut self, x: i32) -> AnyResult<()> {
        let width = i32::from(self.settings_geometry().2);
        let slider_left = SIDEBAR_WIDTH + 40;
        let slider_width = width - 40 - slider_left;
        let minutes = auto_power_saver_minutes_from_slider(x, slider_left, slider_width);
        if minutes != self.settings.auto_power_saver_minutes {
            self.settings.auto_power_saver_minutes = minutes;
            self.settings.auto_power_saver_input = minutes.to_string();
            self.pending_auto_power_saver_apply = Some(Instant::now() + Duration::from_secs(3));
        }
        self.redraw_settings()?;
        Ok(())
    }

    pub(crate) fn handle_wallpaper_click(&mut self, y: i32) -> AnyResult<()> {
        for idx in 0..WALLPAPERS.len() {
            let row_y = 88 + idx as i32 * 116;
            if y >= row_y && y <= row_y + 94 {
                if idx == self.wallpaper_index {
                    return Ok(());
                }
                self.wallpaper_index = idx;
                if self.wallpaper_cache[idx].is_none() {
                    self.wallpaper_cache[idx] = Some(render_wallpaper_pixels(
                        WALLPAPERS[idx].bytes,
                        self.screen_width,
                        self.screen_height,
                    )?);
                }
                if let Some(pixels) = self.wallpaper_cache[idx].as_ref() {
                    self.wallpaper_pixels.clone_from(pixels);
                }
                self.redraw_everything()?;
                return Ok(());
            }
        }
        Ok(())
    }

    pub(crate) fn handle_apps_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        let sx = SIDEBAR_WIDTH + 24;
        let card_w = i32::from(self.settings_geometry().2) - sx - 24;
        if x < sx || x > sx + card_w {
            return Ok(());
        }
        let kinds = [
            DefaultAppKind::Terminal,
            DefaultAppKind::Browser,
            DefaultAppKind::Photo,
            DefaultAppKind::Video,
        ];
        if (84..=118).contains(&y) {
            let item_w = (card_w - 6) / 4;
            let idx = ((x - sx) / item_w).clamp(0, 3) as usize;
            if let Some(kind) = kinds.get(idx) {
                self.settings.app_kind = *kind;
                self.settings.scroll = 0;
                self.settings.terminal_editing = false;
                self.settings.app_status = None;
                self.redraw_settings()?;
            }
            return Ok(());
        }
        let apps = self.available_apps(self.settings.app_kind).to_vec();
        let start = (self.settings.scroll / 29).max(0) as usize;
        for (idx, app) in apps.iter().skip(start).take(6).enumerate() {
            let row_y = 180 + idx as i32 * 29;
            if y >= row_y - 5 && y <= row_y + 19 {
                self.set_selected_app_command(self.settings.app_kind, app.command.clone());
                save_app_commands(&self.settings)?;
                if self.settings.app_kind == DefaultAppKind::Terminal {
                    self.test_terminal_launch(&app.command, &app.name);
                } else {
                    self.settings.app_status = Some(format!("{} set as default.", app.name));
                }
                self.settings.terminal_editing = false;
                self.redraw_settings()?;
                return Ok(());
            }
        }
        if self.settings.app_kind == DefaultAppKind::Terminal && (426..=458).contains(&y) {
            self.settings.terminal_command.clear();
            self.settings.terminal_editing = true;
            self.settings.app_status = None;
            self.conn
                .set_input_focus(InputFocus::POINTER_ROOT, self.ui.settings, CURRENT_TIME)?;
            self.redraw_settings()?;
        }
        Ok(())
    }

}
