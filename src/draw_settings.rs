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
use crate::textutil::*;
use crate::procutil::*;
use crate::files::*;

impl Aurora {
    pub(crate) fn draw_settings_sidebar(&self, c: &mut Canvas) {
        let items = [
            SettingsTab::Display,
            SettingsTab::Power,
            SettingsTab::Wallpaper,
            SettingsTab::Audio,
            SettingsTab::Network,
            SettingsTab::Bluetooth,
            SettingsTab::Startup,
            SettingsTab::Apps,
            SettingsTab::Shortcuts,
            SettingsTab::About,
        ];
        for (idx, tab) in items.iter().enumerate() {
            let y = SETTINGS_SIDEBAR_TOP + idx as i32 * 48;
            let active = *tab == self.settings.tab;
            if active {
                c.draw_round_rect(
                    13,
                    y - 4,
                    SIDEBAR_WIDTH - 26,
                    35,
                    10,
                    Color::rgba(119, 215, 198, 92),
                );
            }
            draw_sidebar_icon(c, idx, 28, y + 12, if active { MINT_DARK } else { MUTED });
        }
    }

    pub(crate) fn draw_display_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        c.draw_text(&self.bold, "Display", sx, 22, 24.0, INK);
        c.draw_text(
            &self.regular,
            "Resolution, refresh rate, and idle sleep.",
            sx,
            54,
            13.0,
            MUTED,
        );

        draw_card(c, sx, 86, i32::from(c.width) - sx - 24, 156);
        c.draw_text(&self.bold, "Resolution", sx + 16, 104, 15.0, INK);
        let modes = self.display_modes.iter().take(4).collect::<Vec<_>>();
        for (idx, mode) in modes.iter().enumerate() {
            let y = 132 + idx as i32 * 25;
            let selected = idx == self.settings.selected_mode;
            c.draw_round_rect(
                sx + 14,
                y - 4,
                i32::from(c.width) - sx - 52,
                22,
                8,
                if selected {
                    Color::rgba(116, 213, 198, 90)
                } else {
                    Color::rgba(255, 255, 255, 120)
                },
            );
            c.draw_text(
                &self.regular,
                &mode.label(),
                sx + 24,
                y,
                12.0,
                if selected { MINT_DARK } else { INK },
            );
            if mode.current {
                c.draw_text_right(
                    &self.regular,
                    "current",
                    i32::from(c.width) - 38,
                    y,
                    11.0,
                    MINT_DARK,
                );
            }
        }

        draw_card(c, sx, 258, i32::from(c.width) - sx - 24, 86);
        c.draw_text(&self.bold, "Refresh rate", sx + 16, 276, 15.0, INK);
        let refresh = self
            .display_modes
            .get(self.settings.selected_mode)
            .and_then(|m| m.refresh)
            .unwrap_or(60.0);
        c.draw_text(
            &self.regular,
            &format!("{refresh:.0} Hz"),
            sx + 16,
            306,
            20.0,
            MINT_DARK,
        );
        c.draw_text(&self.regular, "xrandr mode list", sx + 92, 310, 11.0, MUTED);
        let compositor_switch_x = i32::from(c.width) - 78;
        let compositor_switch_y = 276;
        let compositor_label_x = (i32::from(c.width) - 180).max(sx + 150);
        c.draw_text(&self.bold, "Compositor", compositor_label_x, 276, 15.0, INK);
        if self.settings.compositor_enabled {
            c.draw_round_rect(
                compositor_switch_x,
                compositor_switch_y,
                40,
                24,
                12,
                Color::rgba(160, 238, 220, 210),
            );
            c.draw_circle(
                compositor_switch_x + 28,
                compositor_switch_y + 12,
                8,
                Color::rgb(255, 255, 255),
            );
        } else {
            c.draw_round_rect(
                compositor_switch_x,
                compositor_switch_y,
                40,
                24,
                12,
                Color::rgba(200, 200, 200, 180),
            );
            c.draw_circle(
                compositor_switch_x + 12,
                compositor_switch_y + 12,
                8,
                Color::rgb(255, 255, 255),
            );
        }
        c.draw_text(
            &self.regular,
            if self.compositor_active {
                "active"
            } else if self.settings.compositor_enabled {
                "saved"
            } else {
                "off"
            },
            compositor_label_x,
            306,
            11.0,
            if self.settings.compositor_enabled {
                MINT_DARK
            } else {
                MUTED
            },
        );
        if let Some(status) = self.settings.display_status.as_deref() {
            c.draw_text(
                &self.regular,
                &compact(status, 54),
                sx + 16,
                328,
                11.0,
                BLUE,
            );
        }

        draw_card(c, sx, 360, i32::from(c.width) - sx - 24, 86);
        c.draw_text(&self.bold, "Brightness", sx + 16, 379, 15.0, INK);
        c.draw_text_right(
            &self.bold,
            &format!("{}%", self.settings.brightness_percent),
            i32::from(c.width) - 42,
            379,
            15.0,
            MINT_DARK,
        );
        let bar_x = sx + 16;
        let bar_y = 412;
        let bar_w = 230;
        c.draw_round_rect(bar_x, bar_y, bar_w, 12, 6, Color::rgba(225, 235, 238, 235));
        let fill_w = ((i32::from(self.settings.brightness_percent) - 10) * bar_w / 90).max(6);
        c.draw_round_rect(bar_x, bar_y, fill_w, 12, 6, Color::rgba(116, 213, 198, 220));
        c.draw_text(&self.regular, "10%", bar_x, 430, 11.0, MUTED);
        c.draw_text_right(&self.regular, "100%", bar_x + bar_w, 430, 11.0, MUTED);

        draw_card(c, sx, 462, i32::from(c.width) - sx - 24, 94);
        c.draw_text(&self.bold, "Sleep after", sx + 16, 481, 15.0, INK);
        c.draw_round_rect(sx + 18, 511, 28, 28, 9, Color::rgba(234, 244, 248, 220));
        c.draw_text_center(&self.bold, "-", sx + 32, 513, 18.0, MINT_DARK);
        c.draw_text_center(
            &self.bold,
            &format!("{} s", self.settings.sleep_after_secs),
            sx + 112,
            515,
            15.0,
            INK,
        );
        c.draw_round_rect(sx + 178, 511, 28, 28, 9, Color::rgba(234, 244, 248, 220));
        c.draw_text_center(&self.bold, "+", sx + 192, 513, 18.0, MINT_DARK);
    }

    pub(crate) fn draw_power_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        c.draw_text(&self.bold, "Power", sx, 22, 24.0, INK);
        c.draw_text(
            &self.regular,
            "Battery mode and live resource pressure.",
            sx,
            54,
            13.0,
            MUTED,
        );
        draw_card(c, sx, 86, i32::from(c.width) - sx - 24, 206);
        c.draw_text(&self.bold, "Auto power saver", sx + 16, 106, 15.0, INK);
        let switch_x = i32::from(c.width) - 78;
        let switch_y = 98;
        if self.settings.auto_power_saver_enabled {
            c.draw_round_rect(
                switch_x,
                switch_y,
                40,
                24,
                12,
                Color::rgba(160, 238, 220, 210),
            );
            c.draw_circle(switch_x + 28, switch_y + 12, 8, Color::rgb(255, 255, 255));
        } else {
            c.draw_round_rect(
                switch_x,
                switch_y,
                40,
                24,
                12,
                Color::rgba(200, 200, 200, 180),
            );
            c.draw_circle(switch_x + 12, switch_y + 12, 8, Color::rgb(255, 255, 255));
        }
        let input_x = sx + 16;
        c.draw_round_rect(
            input_x,
            132,
            118,
            30,
            9,
            if self.settings.auto_power_saver_editing {
                Color::rgba(188, 224, 255, 245)
            } else if self.settings.auto_power_saver_enabled {
                Color::rgba(255, 255, 255, 190)
            } else {
                Color::rgba(235, 235, 235, 145)
            },
        );
        if self.settings.auto_power_saver_editing {
            c.draw_round_rect(input_x, 132, 118, 30, 9, Color::rgba(73, 156, 231, 45));
        }
        let minutes = if self.settings.auto_power_saver_editing {
            self.settings.auto_power_saver_input.as_str()
        } else {
            self.settings.auto_power_saver_input.as_str()
        };
        c.draw_text(
            &self.regular,
            if minutes.is_empty() { "0" } else { minutes },
            input_x + 14,
            140,
            14.0,
            if self.settings.auto_power_saver_enabled {
                INK
            } else {
                MUTED
            },
        );
        c.draw_text(&self.regular, "min", input_x + 72, 140, 13.0, MUTED);
        c.draw_text(
            &self.regular,
            "idle minutes before battery saver",
            input_x + 136,
            140,
            12.0,
            MUTED,
        );

        let slider_left = input_x;
        let slider_right = i32::from(c.width) - 40;
        let slider_width = slider_right - slider_left;
        let slider_y = 178;
        c.draw_round_rect(
            slider_left,
            slider_y,
            slider_width,
            8,
            4,
            Color::rgba(211, 225, 232, 190),
        );
        let thumb_x = auto_power_saver_slider_x(
            self.settings.auto_power_saver_minutes,
            slider_left,
            slider_width,
        );
        c.draw_round_rect(
            slider_left,
            slider_y,
            (thumb_x - slider_left).max(4),
            8,
            4,
            if self.settings.auto_power_saver_enabled {
                Color::rgba(116, 213, 198, 220)
            } else {
                Color::rgba(170, 190, 195, 180)
            },
        );
        c.draw_circle(
            thumb_x,
            slider_y + 4,
            if self.settings.auto_power_saver_slider_dragging { 7 } else { 6 },
            Color::rgb(255, 255, 255),
        );
        c.draw_text(&self.regular, "1", slider_left, 188, 10.0, MUTED);
        c.draw_text_right(&self.regular, "1000 min", slider_right, 188, 10.0, MUTED);

        c.draw_text(&self.bold, "Power profile", sx + 16, 205, 15.0, INK);
        let modes = [
            PowerMode::Saver,
            PowerMode::Balanced,
            PowerMode::Performance,
        ];
        for (idx, mode) in modes.iter().enumerate() {
            let y = 228 + idx as i32 * 22;
            let active = *mode == self.settings.power_mode;
            c.draw_round_rect(
                sx + 16,
                y - 5,
                i32::from(c.width) - sx - 58,
                21,
                8,
                if active {
                    Color::rgba(116, 213, 198, 95)
                } else {
                    Color::rgba(255, 255, 255, 118)
                },
            );
            c.draw_text(
                &self.regular,
                mode.label(),
                sx + 28,
                y,
                12.0,
                if active { MINT_DARK } else { INK },
            );
        }

        draw_card(c, sx, 304, i32::from(c.width) - sx - 24, 166);
        c.draw_text(&self.bold, "System", sx + 16, 324, 15.0, INK);
        draw_metric_bar(
            c,
            &self.regular,
            sx + 16,
            352,
            "CPU",
            self.metrics.cpu_usage,
            "%",
        );
        // GPU usage right below the CPU bar: supports NVIDIA (nvidia-smi),
        // AMD and Intel (sysfs) and lists each GPU on multi-GPU systems.
        let mut bar_y = 352;
        for gpu in self.metrics.gpu_usage.iter().take(3) {
            bar_y += 30;
            draw_metric_bar(
                c,
                &self.regular,
                sx + 16,
                bar_y,
                &compact(&gpu.name, 9),
                gpu.percent,
                "%",
            );
        }
        if self.metrics.gpu_usage.is_empty() {
            bar_y += 22;
            c.draw_text(
                &self.regular,
                if self.settings_cache.gpu_usage.is_some() {
                    "GPU usage unavailable (no driver metric exposed)"
                } else {
                    "Reading GPU usage..."
                },
                sx + 16,
                bar_y,
                11.0,
                MUTED,
            );
            bar_y += 8;
        }
        if bar_y + 30 <= 452 {
            bar_y += 30;
            let ram_pct = if self.metrics.ram_total_kb > 0 {
                self.metrics.ram_used_kb as f32 * 100.0 / self.metrics.ram_total_kb as f32
            } else {
                0.0
            };
            draw_metric_bar(c, &self.regular, sx + 16, bar_y, "RAM", ram_pct, "%");
        }
        let freq_lines = cpu_frequency_lines(&self.metrics.cpu_frequencies, 46);
        if let Some(line) = freq_lines.first() {
            if bar_y + 26 <= 458 {
                c.draw_text(&self.regular, "CPU frequency", sx + 16, bar_y + 24, 11.0, MUTED);
                c.draw_text(&self.regular, line, sx + 110, bar_y + 24, 11.0, INK);
            }
        }

        draw_card(c, sx, 486, i32::from(c.width) - sx - 24, 70);
        c.draw_text(&self.bold, "Battery", sx + 16, 502, 15.0, INK);
        c.draw_text(
            &self.regular,
            self.metrics
                .battery
                .as_deref()
                .unwrap_or("No battery exposed"),
            sx + 16,
            528,
            14.0,
            MINT_DARK,
        );
    }

    pub(crate) fn draw_wallpaper_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        c.draw_text(&self.bold, "Wallpaper", sx, 22, 24.0, INK);
        c.draw_text(
            &self.regular,
            "Select one of the embedded wallpapers.",
            sx,
            54,
            13.0,
            MUTED,
        );
        for (idx, asset) in WALLPAPERS.iter().enumerate() {
            let y = 88 + idx as i32 * 116;
            draw_card(c, sx, y, i32::from(c.width) - sx - 24, 94);
            let preview_x = sx + 14;
            let preview_y = y + 14;
            if let Some(preview) = self
                .wallpaper_previews
                .get(idx)
                .and_then(|preview| preview.as_ref())
            {
                paint_bgr_pixels(c, preview, preview_x, preview_y, 92, 56);
            }
            c.draw_round_rect(
                preview_x,
                preview_y,
                92,
                56,
                10,
                Color::rgba(255, 255, 255, 28),
            );
            c.draw_text(&self.bold, asset.name, sx + 122, y + 20, 14.0, INK);
            c.draw_text(
                &self.regular,
                if idx == self.wallpaper_index {
                    "Current wallpaper"
                } else {
                    "Click to apply"
                },
                sx + 122,
                y + 46,
                12.0,
                if idx == self.wallpaper_index {
                    MINT_DARK
                } else {
                    MUTED
                },
            );
            if idx == self.wallpaper_index {
                c.draw_circle(
                    i32::from(c.width) - 44,
                    y + 44,
                    12,
                    Color::rgba(116, 213, 198, 180),
                );
                c.draw_line(
                    i32::from(c.width) - 50,
                    y + 44,
                    i32::from(c.width) - 45,
                    y + 49,
                    2,
                    Color::rgb(255, 255, 255),
                );
                c.draw_line(
                    i32::from(c.width) - 45,
                    y + 49,
                    i32::from(c.width) - 37,
                    y + 39,
                    2,
                    Color::rgb(255, 255, 255),
                );
            }
        }
    }

    pub(crate) fn ensure_wallpaper_previews(&mut self) {
        for (idx, asset) in WALLPAPERS.iter().enumerate() {
            if self.wallpaper_previews[idx].is_none() {
                self.wallpaper_previews[idx] =
                    render_asset_preview_pixels(asset.bytes, 92, 56).ok();
            }
        }
    }

    pub(crate) fn draw_audio_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        c.draw_text(&self.bold, "Audio", sx, 22, 24.0, INK);
        draw_card(c, sx, 86, i32::from(c.width) - sx - 24, 112);
        c.draw_text(&self.bold, "Volume", sx + 16, 106, 15.0, INK);
        let loaded = self.settings_cache.audio_volume.is_some();
        let volume = self.settings_cache.audio_volume.flatten();
        let volume_pct = volume.unwrap_or(0);
        let bar_w = 230;
        c.draw_round_rect(sx + 16, 142, bar_w, 10, 5, Color::rgba(211, 225, 232, 170));
        c.draw_round_rect(
            sx + 16,
            142,
            bar_w * i32::from(volume_pct) / 100,
            10,
            5,
            Color::rgba(116, 213, 198, 210),
        );
        c.draw_text(
            &self.regular,
            &volume.map(|pct| format!("{pct}%")).unwrap_or_else(|| {
                if loaded {
                    "unavailable".to_string()
                } else {
                    "loading...".to_string()
                }
            }),
            sx + 262,
            136,
            15.0,
            INK,
        );
        if let Some(status) = self.settings.audio_status.as_deref() {
            c.draw_text(
                &self.regular,
                &compact(status, 52),
                sx + 16,
                166,
                11.0,
                BLUE,
            );
        }
        let card_w = i32::from(c.width) - sx - 24;
        draw_card(c, sx, 220, card_w, 150);
        c.draw_text(&self.bold, "Output device", sx + 16, 240, 15.0, INK);
        let outputs_loaded = self.settings_cache.audio_outputs.is_some();
        let outputs = self
            .settings_cache
            .audio_outputs
            .clone()
            .unwrap_or_default();
        if outputs.is_empty() {
            c.draw_text(
                &self.regular,
                if outputs_loaded {
                    "No output devices found"
                } else {
                    "Loading audio devices..."
                },
                sx + 16,
                272,
                12.0,
                MUTED,
            );
        }
        for (idx, dev) in outputs.iter().take(3).enumerate() {
            let row_y = 260 + idx as i32 * 30;
            if dev.is_default {
                c.draw_round_rect(
                    sx + 12,
                    row_y - 3,
                    card_w - 24,
                    24,
                    6,
                    Color::rgba(116, 213, 198, 120),
                );
            }
            c.draw_text(
                &self.regular,
                &compact(&dev.label, 45),
                sx + 18,
                row_y + 4,
                12.0,
                INK,
            );
            if dev.is_default {
                c.draw_text(
                    &self.bold,
                    "default",
                    sx + card_w - 76,
                    row_y + 4,
                    11.0,
                    BLUE,
                );
            }
        }
        draw_card(c, sx, 392, card_w, 108);
        c.draw_text(&self.bold, "Input device", sx + 16, 412, 15.0, INK);
        let inputs_loaded = self.settings_cache.audio_inputs.is_some();
        let inputs = self
            .settings_cache
            .audio_inputs
            .clone()
            .unwrap_or_default();
        if inputs.is_empty() {
            c.draw_text(
                &self.regular,
                if inputs_loaded {
                    "No input devices found"
                } else {
                    "Loading audio devices..."
                },
                sx + 16,
                444,
                12.0,
                MUTED,
            );
        }
        for (idx, dev) in inputs.iter().take(2).enumerate() {
            let row_y = 432 + idx as i32 * 30;
            if dev.is_default {
                c.draw_round_rect(
                    sx + 12,
                    row_y - 3,
                    card_w - 24,
                    24,
                    6,
                    Color::rgba(116, 213, 198, 120),
                );
            }
            c.draw_text(
                &self.regular,
                &compact(&dev.label, 45),
                sx + 18,
                row_y + 4,
                12.0,
                INK,
            );
            if dev.is_default {
                c.draw_text(
                    &self.bold,
                    "default",
                    sx + card_w - 76,
                    row_y + 4,
                    11.0,
                    BLUE,
                );
            }
        }
    }

    pub(crate) fn draw_network_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        let card_w = i32::from(c.width) - sx - 24;
        c.draw_text(&self.bold, "Network", sx, 22, 24.0, INK);
        c.draw_text(
            &self.regular,
            "Wired and Wi-Fi interfaces.",
            sx,
            54,
            13.0,
            MUTED,
        );
        draw_card(c, sx, 86, card_w, 154);
        c.draw_text(&self.bold, "Current status", sx + 16, 106, 15.0, INK);

        // Scroll exactly with step 24 matching line spacing!
        let start = (self.settings.scroll / 24).max(0) as usize;
        let details_loaded = self.settings_cache.network_details.is_some();
        let details = self
            .settings_cache
            .network_details
            .clone()
            .unwrap_or_default();
        if !details_loaded {
            c.draw_text(
                &self.regular,
                "Loading network status...",
                sx + 16,
                134,
                13.0,
                MUTED,
            );
        }
        for (idx, line) in details.iter().skip(start).take(4).enumerate() {
            c.draw_text(
                &self.regular,
                &compact(line, 62),
                sx + 16,
                134 + idx as i32 * 24,
                13.0,
                if idx % 3 == 0 { INK } else { MUTED },
            );
        }

        draw_card(c, sx, 258, card_w, 288);
        c.draw_text(&self.bold, "Wi-Fi", sx + 16, 278, 15.0, INK);

        let connected_wifi = self.settings.wifi_connected.clone().flatten();
        let wifi_enabled = self.settings.wifi_radio_enabled.unwrap_or(true);
        let disconnect_color = if connected_wifi.is_some() {
            INK
        } else {
            Color::rgba(120, 120, 120, 170)
        };

        c.draw_round_rect(sx + 75, 273, 58, 24, 7, Color::rgba(234, 244, 248, 220));
        c.draw_text_center(&self.bold, "Refresh", sx + 104, 281, 10.0, MINT_DARK);
        c.draw_round_rect(sx + 141, 273, 78, 24, 7, Color::rgba(234, 244, 248, 220));
        c.draw_text_center(
            &self.bold,
            "Disconnect",
            sx + 180,
            281,
            10.0,
            disconnect_color,
        );

        // Beautiful Premium 40x24 Sliding On/Off Switch
        let tx = sx + 227;
        let ty = 273;
        if wifi_enabled {
            c.draw_round_rect(tx, ty, 40, 24, 12, Color::rgba(160, 238, 220, 200));
            c.draw_circle(tx + 28, ty + 12, 8, Color::rgb(255, 255, 255));
        } else {
            c.draw_round_rect(tx, ty, 40, 24, 12, Color::rgba(200, 200, 200, 180));
            c.draw_circle(tx + 12, ty + 12, 8, Color::rgb(255, 255, 255));
        }

        let mut list_start_y = 344;
        if let Some(wifi) = connected_wifi.as_ref() {
            c.draw_text(&self.bold, "Connected", sx + 16, 302, 11.0, MINT_DARK);
            c.draw_text(
                &self.bold,
                &compact(&wifi.ssid, 44),
                sx + 16,
                318,
                13.0,
                INK,
            );
            c.draw_text(
                &self.regular,
                wifi.ip.as_deref().unwrap_or("no ip"),
                sx + 16,
                334,
                12.0,
                MUTED,
            );
            list_start_y = 376;
        } else {
            c.draw_text(&self.regular, "Not connected", sx + 16, 302, 12.0, MUTED);
        }

        let status_y = list_start_y - 22;
        if let Some(status) = self.settings.wifi_status.as_deref() {
            c.draw_text(
                &self.regular,
                &compact(status, 54),
                sx + 16,
                status_y,
                11.0,
                BLUE,
            );
        } else {
            c.draw_text(
                &self.regular,
                "Click Refresh to scan nearby Wi-Fi networks",
                sx + 16,
                status_y,
                11.0,
                MUTED,
            );
        }

        if self.settings.wifi_disconnect_confirm {
            c.draw_round_rect(
                sx + 12,
                list_start_y,
                card_w - 24,
                44,
                8,
                Color::rgba(255, 255, 255, 210),
            );
            c.draw_text(
                &self.bold,
                "Disconnect current Wi-Fi?",
                sx + 24,
                list_start_y + 26,
                13.0,
                INK,
            );
            c.draw_round_rect(
                sx + card_w - 194,
                list_start_y + 8,
                76,
                28,
                7,
                Color::rgba(211, 225, 232, 170),
            );
            c.draw_text_center(
                &self.bold,
                "Cancel",
                sx + card_w - 156,
                list_start_y + 26,
                11.0,
                INK,
            );
            c.draw_round_rect(
                sx + card_w - 108,
                list_start_y + 8,
                88,
                28,
                7,
                Color::rgba(241, 126, 135, 150),
            );
            c.draw_text_center(
                &self.bold,
                "Disconnect",
                sx + card_w - 64,
                list_start_y + 26,
                11.0,
                Color::rgb(160, 58, 68),
            );
            return;
        }

        if self.settings.wifi_networks.is_empty() {
            c.draw_text(
                &self.regular,
                "No Wi-Fi networks found",
                sx + 16,
                list_start_y + 10,
                13.0,
                MUTED,
            );
        } else {
            for (idx, network) in self
                .settings
                .wifi_networks
                .iter()
                .skip(self.settings.wifi_scroll)
                .take(5)
                .enumerate()
            {
                let y = list_start_y + idx as i32 * 24;
                let selected = self
                    .settings
                    .wifi_selected
                    .as_deref()
                    .is_some_and(|ssid| ssid == network.ssid);
                if selected {
                    c.draw_round_rect(
                        sx + 12,
                        y - 4, // Perfectly centers text inside the 22px high overlay
                        card_w - 24,
                        22,
                        7,
                        Color::rgba(160, 238, 220, 92),
                    );
                }
                c.draw_text(
                    &self.regular,
                    &compact(&network.ssid, 48),
                    sx + 18,
                    y,
                    13.0,
                    if selected { MINT_DARK } else { INK },
                );
            }
        }

        if let Some(ssid) = self.settings.wifi_selected.as_deref() {
            c.draw_text(
                &self.bold,
                &compact(&format!("Password for {ssid}"), 42),
                sx + 16,
                492,
                13.0,
                INK,
            );
            let input_x = sx + 16;
            let button_w = 34; // 34x34 icon button
            let gap = 12;
            let input_w = (card_w - 32 - button_w - gap).max(132);
            let input_y = 508;
            c.draw_round_rect(
                input_x,
                input_y,
                input_w,
                34,
                7,
                if self.settings.wifi_password_editing {
                    Color::rgba(255, 255, 255, 220)
                } else {
                    Color::rgba(255, 255, 255, 145)
                },
            );
            c.draw_rect(input_x + 8, input_y + 33, input_w - 16, 1, CARD_LINE);
            let shown = if self.settings.wifi_password.is_empty() {
                "enter password".to_string()
            } else {
                password_mask(self.settings.wifi_password.chars().count())
            };

            // Text y = input_y + 10 is perfectly vertically centered (top of bounding box)
            c.draw_text(
                &self.regular,
                &compact(&shown, 38),
                input_x + 12,
                input_y + 10,
                13.0,
                if self.settings.wifi_password.is_empty() {
                    MUTED
                } else {
                    INK
                },
            );

            let button_x = input_x + input_w + gap;
            c.draw_round_rect(
                button_x,
                input_y,
                button_w,
                34,
                7,
                Color::rgba(160, 238, 220, 170),
            );

            // Draw Connect icon arrow centered vertically and horizontally
            let cx = button_x + 17;
            let cy = input_y + 17;
            c.draw_line(cx - 6, cy, cx + 6, cy, 2, MINT_DARK);
            c.draw_line(cx + 6, cy, cx + 2, cy - 4, 2, MINT_DARK);
            c.draw_line(cx + 6, cy, cx + 2, cy + 4, 2, MINT_DARK);
        }
    }

    pub(crate) fn draw_bluetooth_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        c.draw_text(&self.bold, "Bluetooth", sx, 22, 24.0, INK);
        draw_card(c, sx, 86, i32::from(c.width) - sx - 24, 116);
        c.draw_text(&self.bold, "Connected devices", sx + 16, 106, 15.0, INK);
        let devices_loaded = self.settings_cache.bluetooth_devices.is_some();
        let devices = self
            .settings_cache
            .bluetooth_devices
            .clone()
            .unwrap_or_default();
        if devices.is_empty() {
            c.draw_text(
                &self.regular,
                if devices_loaded {
                    "No connected devices"
                } else {
                    "Loading bluetooth devices..."
                },
                sx + 16,
                140,
                12.0,
                MUTED,
            );
        } else {
            for (idx, dev) in devices.iter().take(3).enumerate() {
                c.draw_text(
                    &self.regular,
                    &compact(dev, 50),
                    sx + 16,
                    140 + idx as i32 * 26,
                    12.0,
                    INK,
                );
            }
        }
        draw_card(c, sx, 224, i32::from(c.width) - sx - 24, 76);
        c.draw_text(&self.bold, "Add device", sx + 16, 246, 15.0, INK);
        c.draw_text(
            &self.regular,
            "Click to open bluetoothctl pairing helper",
            sx + 16,
            274,
            12.0,
            MUTED,
        );
    }

    pub(crate) fn draw_startup_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        c.draw_text(&self.bold, "Startup", sx, 22, 24.0, INK);
        c.draw_text(
            &self.regular,
            "Autostart apps for this desktop.",
            sx,
            54,
            13.0,
            MUTED,
        );
        draw_card(c, sx, 86, i32::from(c.width) - sx - 24, 394);
        let apps_loaded = self.settings_cache.autostart_apps.is_some();
        let apps = self
            .settings_cache
            .autostart_apps
            .clone()
            .unwrap_or_default();
        if apps.is_empty() {
            c.draw_text(
                &self.regular,
                if apps_loaded {
                    "No autostart entries"
                } else {
                    "Loading autostart entries..."
                },
                sx + 16,
                116,
                12.0,
                MUTED,
            );
        } else {
            let start = (self.settings.scroll / 28).max(0) as usize;
            for (idx, app) in apps.iter().skip(start).take(12).enumerate() {
                c.draw_text(
                    &self.regular,
                    &compact(app, 54),
                    sx + 16,
                    116 + idx as i32 * 28,
                    13.0,
                    INK,
                );
            }
        }
    }

    pub(crate) fn draw_apps_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        let card_w = i32::from(c.width) - sx - 24;
        c.draw_text(&self.bold, "Apps", sx, 22, 24.0, INK);
        c.draw_text(
            &self.regular,
            "Choose default applications for this desktop.",
            sx,
            54,
            12.0,
            MUTED,
        );

        let kinds = [
            DefaultAppKind::Terminal,
            DefaultAppKind::Browser,
            DefaultAppKind::Photo,
            DefaultAppKind::Video,
        ];
        for (idx, kind) in kinds.iter().enumerate() {
            let x = sx + idx as i32 * ((card_w - 6) / 4);
            let w = (card_w - 12) / 4;
            c.draw_round_rect(
                x,
                84,
                w,
                34,
                8,
                if *kind == self.settings.app_kind {
                    Color::rgba(116, 213, 198, 95)
                } else {
                    Color::rgba(255, 255, 255, 118)
                },
            );
            c.draw_text_center(
                &self.bold,
                kind.label(),
                x + w / 2,
                94,
                11.0,
                if *kind == self.settings.app_kind {
                    MINT_DARK
                } else {
                    INK
                },
            );
        }

        draw_card(c, sx, 132, card_w, 234);
        c.draw_text(&self.bold, "Default apps", sx + 16, 151, 14.0, INK);
        c.draw_text(
            &self.regular,
            &format!("Choose default {}", self.settings.app_kind.label()),
            sx + 112,
            152,
            11.0,
            MUTED,
        );
        let selected = self.selected_app_command(self.settings.app_kind);
        let apps = self.available_apps(self.settings.app_kind);
        let start = (self.settings.scroll / 29).max(0) as usize;
        if apps.is_empty() {
            c.draw_text(
                &self.regular,
                "No installed applications found.",
                sx + 16,
                185,
                12.0,
                MUTED,
            );
        }
        for (idx, app) in apps.iter().skip(start).take(6).enumerate() {
            let y = 180 + idx as i32 * 29;
            let active = selected == app.command;
            c.draw_round_rect(
                sx + 14,
                y - 5,
                card_w - 28,
                24,
                8,
                if active {
                    Color::rgba(116, 213, 198, 95)
                } else {
                    Color::rgba(255, 255, 255, 118)
                },
            );
            c.draw_text(
                &self.regular,
                &compact(&app.name, 46),
                sx + 25,
                y,
                12.0,
                if active { MINT_DARK } else { INK },
            );
        }
        if apps.len() > 6 {
            c.draw_text(
                &self.regular,
                "Scroll to see more default apps",
                sx + card_w - 192,
                151,
                10.0,
                MUTED,
            );
        }

        if self.settings.app_kind == DefaultAppKind::Terminal {
            draw_card(c, sx, 382, card_w, 104);
            c.draw_text(
                &self.bold,
                "Custom terminal command",
                sx + 16,
                399,
                14.0,
                INK,
            );
            c.draw_round_rect(
                sx + 14,
                426,
                card_w - 28,
                32,
                9,
                if self.settings.terminal_editing {
                    Color::rgba(116, 213, 198, 95)
                } else {
                    Color::rgba(224, 236, 242, 170)
                },
            );
            let shown = if self.settings.terminal_command.is_empty() {
                "Click and type a command; Enter saves and launches"
            } else {
                self.settings.terminal_command.as_str()
            };
            c.draw_text(&self.regular, &compact(shown, 52), sx + 25, 435, 12.0, INK);
        }
        if let Some(status) = self.settings.app_status.as_ref() {
            c.draw_text(
                &self.regular,
                &compact(status, 64),
                sx + 16,
                506,
                12.0,
                MINT_DARK,
            );
        }
    }

    pub(crate) fn draw_about_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        c.draw_text(&self.bold, "About", sx, 22, 24.0, INK);
        c.draw_text(
            &self.regular,
            "Hardware and network telemetry.",
            sx,
            54,
            12.0,
            MUTED,
        );
        draw_card(c, sx, 86, i32::from(c.width) - sx - 24, 248);
        c.draw_text(&self.bold, "Computer", sx + 16, 106, 15.0, INK);
        let cpu = compact(&self.metrics.cpu_model, 34);
        draw_info_row(c, &self.regular, sx + 16, 136, "CPU", &cpu);
        draw_info_row(
            c,
            &self.regular,
            sx + 16,
            164,
            "Status",
            &self.metrics.cpu_status,
        );
        draw_info_row(
            c,
            &self.regular,
            sx + 16,
            192,
            "RAM",
            &format!(
                "{} / {}",
                format_kib(self.metrics.ram_used_kb),
                format_kib(self.metrics.ram_total_kb)
            ),
        );
        draw_info_row(
            c,
            &self.regular,
            sx + 16,
            220,
            "Swap",
            &format!(
                "{} / {}",
                format_kib(self.metrics.swap_used_kb),
                format_kib(self.metrics.swap_total_kb)
            ),
        );
        let gpus = if self.metrics.gpus.is_empty() {
            "No GPU info".to_string()
        } else {
            compact(&self.metrics.gpus.join(", "), 32)
        };
        draw_info_row(c, &self.regular, sx + 16, 248, "GPU", &gpus);
        let nics = if self.metrics.nics.is_empty() {
            "No network card".to_string()
        } else {
            compact(&self.metrics.nics.join(", "), 32)
        };
        draw_info_row(c, &self.regular, sx + 16, 276, "NIC", &nics);
        draw_info_row(c, &self.regular, sx + 16, 304, "Display", &self.display);

        draw_card(c, sx, 354, i32::from(c.width) - sx - 24, 154);
        c.draw_text(&self.bold, "Network speed", sx + 16, 376, 15.0, INK);
        c.draw_text(&self.regular, "Down", sx + 16, 408, 12.0, MUTED);
        c.draw_text(
            &self.bold,
            &format_bps(self.metrics.net_rx_bps),
            sx + 70,
            405,
            17.0,
            INK,
        );
        c.draw_text(&self.regular, "Up", sx + 16, 450, 12.0, MUTED);
        c.draw_text(
            &self.bold,
            &format_bps(self.metrics.net_tx_bps),
            sx + 70,
            447,
            17.0,
            INK,
        );
        draw_sparkline(
            c,
            sx + 152,
            402,
            i32::from(c.width) - sx - 190,
            22,
            self.metrics.net_rx_bps,
            BLUE,
        );
        draw_sparkline(
            c,
            sx + 152,
            444,
            i32::from(c.width) - sx - 190,
            22,
            self.metrics.net_tx_bps,
            MINT_DARK,
        );
    }

}
