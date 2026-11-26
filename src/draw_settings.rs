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
            SettingsTab::Multitask,
            SettingsTab::About,
        ];
        for (idx, tab) in items.iter().enumerate() {
            let y = SETTINGS_SIDEBAR_TOP + idx as i32 * SETTINGS_SIDEBAR_STRIDE;
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

    pub(crate) fn draw_multitask_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        let card_w = i32::from(c.width) - sx - 24;
        c.draw_text(&self.bold, "Multitask", sx, 22, 24.0, INK);
        c.draw_text(&self.regular, "Dock placement and the icons on the topbar.", sx, 54, 13.0, MUTED);

        // Dock placement. The switch row (86..132) is hit-tested in settings_events.
        draw_card(c, sx, 86, card_w, 132);
        c.draw_text(&self.bold, "Merge dock into topbar", sx + 16, 105, 16.0, INK);
        c.draw_text(&self.regular, "Keep Files and running apps beside workspaces.", sx + 16, 144, 12.0, MUTED);
        c.draw_text(&self.regular, "The clock moves beside the clipboard button.", sx + 16, 171, 12.0, MUTED);
        draw_switch(c, i32::from(c.width) - 80, 103, self.settings.dock_in_topbar);

        // Topbar icons: a two-column grid, filled row by row in MULTITASK_ICONS order.
        draw_card(c, sx, MULTITASK_ICONS_CARD_Y, card_w, 0);
        c.draw_text(&self.bold, "Topbar icons", sx + 16, MULTITASK_ICONS_CARD_Y + 14, 15.0, INK);
        c.draw_text(
            &self.regular,
            "Hidden icons take no space. Recording stays visible while it runs.",
            sx + 16,
            MULTITASK_ICONS_CARD_Y + 36,
            12.0,
            MUTED,
        );
        let col_w = (card_w - 32) / 2;
        for (index, (label, on)) in multitask_icon_items(self.settings.topbar_icons).iter().enumerate() {
            let col = (index % 2) as i32;
            let row = (index / 2) as i32;
            let x = sx + 16 + col * col_w;
            let y = MULTITASK_ROW_Y + row * MULTITASK_ROW_H;
            c.draw_text(&self.regular, label, x, y + 6, 13.0, INK);
            draw_switch(c, x + col_w - 64, y, *on);
        }
        c.draw_text(
            &self.regular,
            "Click Aurora to open the start menu.",
            sx,
            MULTITASK_ROW_Y + 4 * MULTITASK_ROW_H + 18,
            13.0,
            MUTED,
        );
    }

    pub(crate) fn draw_display_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        let l = DisplayLayout::new(i32::from(c.width));
        c.draw_text(&self.bold, "Display", sx, 22, 24.0, INK);
        c.draw_text(
            &self.regular,
            "Resolution, rotation, brightness, and idle sleep.",
            sx,
            54,
            13.0,
            MUTED,
        );

        // Resolution: the panel's real modes (xrandr). Scale below it is GTK's
        // GDK_SCALE (1, 2 or 3); scale does not change the panel mode.
        draw_card(c, sx, 86, l.card_w, 0);
        c.draw_text(&self.bold, "Resolution", sx + 16, l.dd_y + 6, 15.0, INK);
        let (native_w, native_h) = self.native_screen_size();
        let open = self.settings.display_dropdown_open;
        c.draw_round_rect(
            l.dd_x,
            l.dd_y,
            l.dd_w,
            l.dd_h,
            9,
            if open { Color::rgba(188, 224, 255, 245) } else { Color::rgba(255, 255, 255, 205) },
        );
        let selected_mode = self.display_modes.get(self.settings.selected_mode);
        let shown = selected_mode
            .map(|m| format!("{} × {}", m.width, m.height))
            .unwrap_or_else(|| format!("{native_w} × {native_h}"));
        c.draw_text(&self.bold, &shown, l.dd_x + 12, l.dd_y + 7, 13.0, INK);
        if let Some(rate) = selected_mode.and_then(|m| m.refresh) {
            c.draw_text_right(&self.regular, &format!("{rate:.0} Hz"), l.dd_x + l.dd_w - 30, l.dd_y + 8, 12.0, MUTED);
        }
        draw_chevron(c, l.dd_x + l.dd_w - 16, l.dd_y + l.dd_h / 2, open, SOFT_INK);

        // Scale: 1× / 2× / 3× (GDK_SCALE), right-aligned like the rotation row.
        let scale = crate::scale::gdk_scale();
        c.draw_text(&self.bold, "Scale", sx + 16, l.scale_y, 15.0, INK);
        c.draw_round_rect(l.seg_x, l.scale_seg_y, l.seg_w, l.seg_h, 9, Color::rgba(234, 244, 248, 220));
        for (idx, &choice) in GDK_SCALE_CHOICES.iter().enumerate() {
            let (x, w) = l.scale_seg_rect(idx);
            let selected = choice == scale;
            if selected {
                c.draw_round_rect(x + 2, l.scale_seg_y + 2, w - 4, l.seg_h - 4, 7, Color::rgba(160, 238, 220, 235));
            } else if idx > 0 && GDK_SCALE_CHOICES[idx - 1] != scale {
                c.draw_rect(x, l.scale_seg_y + 7, 1, l.seg_h - 14, Color::rgba(180, 196, 206, 200));
            }
            c.draw_text_center(
                if selected { &self.bold } else { &self.regular },
                &format!("{choice}×"),
                x + w / 2,
                l.scale_seg_y + 6,
                12.0,
                if selected { INK } else { SOFT_INK },
            );
        }
        let (desk_w, desk_h) = (native_w / scale as u16, native_h / scale as u16);
        c.draw_text(
            &self.regular,
            &format!("Desktop {desk_w} × {desk_h} at {scale}×. Open apps keep their scale."),
            sx + 16,
            l.scale_hint_y,
            11.0,
            MUTED,
        );

        // Rotation: Auto / Off / 90° / 180° / 270° selector.
        c.draw_text(&self.bold, "Rotation", sx + 16, l.rotate_y, 15.0, INK);
        let mode = rotation_mode();
        let sensor = accelerometer_available();
        c.draw_round_rect(l.seg_x, l.seg_y, l.seg_w, l.seg_h, 9, Color::rgba(234, 244, 248, 220));
        for (idx, &(choice, label)) in ROTATION_CHOICES.iter().enumerate() {
            let (x, w) = l.seg_rect(idx);
            let selected = choice == mode;
            if selected {
                c.draw_round_rect(
                    x + 2,
                    l.seg_y + 2,
                    w - 4,
                    l.seg_h - 4,
                    7,
                    if choice == RotationMode::Auto && !sensor {
                        Color::rgba(255, 214, 160, 235)
                    } else {
                        Color::rgba(160, 238, 220, 235)
                    },
                );
            } else if idx > 0 && ROTATION_CHOICES[idx - 1].0 != mode {
                c.draw_rect(x, l.seg_y + 7, 1, l.seg_h - 14, Color::rgba(180, 196, 206, 200));
            }
            let color = if selected {
                INK
            } else if choice == RotationMode::Auto && !sensor {
                MUTED
            } else {
                SOFT_INK
            };
            c.draw_text_center(
                if selected { &self.bold } else { &self.regular },
                label,
                x + w / 2,
                l.seg_y + 6,
                12.0,
                color,
            );
        }
        let (hint, hint_color): (Cow<str>, Color) = match (mode, sensor) {
            (RotationMode::Auto, true) => ("Turns the screen when you rotate the iPad".into(), MUTED),
            (RotationMode::Auto, false) => (
                "Auto: sensor unavailable this boot - stays upright; pick an angle to lock".into(),
                Color::rgb(196, 110, 24),
            ),
            (fixed, sensor) => {
                let angle = match fixed {
                    RotationMode::Left => "90°",
                    RotationMode::Inverted => "180°",
                    RotationMode::Right => "270°",
                    _ => "0° (upright)",
                };
                let mut text = format!("Locked at {angle}");
                if !sensor {
                    text.push_str(" · auto: sensor unavailable this boot");
                }
                (text.into(), MUTED)
            }
        };
        c.draw_text(&self.regular, &hint, sx + 16, l.rotate_y + 28, 11.0, hint_color);

        // Refresh rate + compositor.
        draw_card(c, sx, l.refresh_card_y, l.card_w, 0);
        c.draw_text(&self.bold, "Refresh rate", sx + 16, l.refresh_y, 15.0, INK);
        let refresh = self
            .display_modes
            .get(self.settings.selected_mode)
            .and_then(|m| m.refresh)
            .unwrap_or(60.0);
        c.draw_text(&self.bold, &format!("{refresh:.0} Hz"), sx + 16, l.refresh_y + 22, 12.0, MINT_DARK);
        c.draw_text(
            &self.regular,
            &format!("native {native_w}×{native_h}"),
            sx + 60,
            l.refresh_y + 23,
            11.0,
            MUTED,
        );
        let compositor_label_x = (l.switch_x - 108).max(sx + 170);
        c.draw_text(&self.bold, "Compositor", compositor_label_x, l.refresh_y, 15.0, INK);
        draw_switch(c, l.switch_x, l.refresh_y - 2, self.settings.compositor_enabled);
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
            l.refresh_y + 23,
            11.0,
            if self.settings.compositor_enabled { MINT_DARK } else { MUTED },
        );

        // Brightness: current level on the left, slider, "+" step button on
        // the right (no end labels).
        draw_card(c, sx, l.bright_card_y, l.card_w, 0);
        c.draw_text(&self.bold, "Brightness", sx + 16, l.bright_y, 15.0, INK);
        c.draw_text(
            &self.bold,
            &format!("{}%", self.settings.brightness_percent),
            sx + 16,
            l.bar_y - 2,
            13.0,
            MINT_DARK,
        );
        c.draw_round_rect(l.bar_x, l.bar_y, l.bar_w, 12, 6, Color::rgba(225, 235, 238, 235));
        let fill_w = ((i32::from(self.settings.brightness_percent) - 10) * l.bar_w / 90).max(6);
        c.draw_round_rect(l.bar_x, l.bar_y, fill_w, 12, 6, Color::rgba(116, 213, 198, 220));
        let at_max = self.settings.brightness_percent >= 100;
        c.draw_round_rect(l.bright_plus_x, l.bar_y - 8, 28, 28, 9, Color::rgba(234, 244, 248, 220));
        c.draw_text_center(
            &self.bold,
            "+",
            l.bright_plus_x + 14,
            l.bar_y - 6,
            18.0,
            if at_max { MUTED } else { MINT_DARK },
        );

        // Sleep after: label left, stepper right.
        draw_card(c, sx, l.sleep_card_y, l.card_w, 0);
        c.draw_text(&self.bold, "Sleep after", sx + 16, l.sleep_y + 5, 15.0, INK);
        c.draw_round_rect(l.minus_x, l.sleep_y, 28, 28, 9, Color::rgba(234, 244, 248, 220));
        c.draw_text_center(&self.bold, "-", l.minus_x + 14, l.sleep_y + 2, 18.0, MINT_DARK);
        let sleep = if self.settings.sleep_after_secs == 0 {
            "never".to_string()
        } else if self.settings.sleep_after_secs % 60 == 0 {
            format!("{} min", self.settings.sleep_after_secs / 60)
        } else {
            format!("{} s", self.settings.sleep_after_secs)
        };
        c.draw_text_center(&self.bold, &sleep, (l.minus_x + 28 + l.plus_x) / 2, l.sleep_y + 5, 14.0, INK);
        c.draw_round_rect(l.plus_x, l.sleep_y, 28, 28, 9, Color::rgba(234, 244, 248, 220));
        c.draw_text_center(&self.bold, "+", l.plus_x + 14, l.sleep_y + 2, 18.0, MINT_DARK);

        if let Some(status) = self.settings.display_status.as_deref() {
            c.draw_text(&self.regular, &compact(status, 60), sx + 16, l.status_y, 11.0, BLUE);
        }

        // The open list floats over the cards below the button: the panel's modes.
        if open {
            let (lx, ly, lw, lh) = l.list_rect(self.display_modes.len());
            c.draw_round_rect(lx - 1, ly - 1, lw + 2, lh + 4, 11, Color::rgba(40, 60, 80, 40));
            c.draw_round_rect(lx, ly, lw, lh, 10, Color::rgb(250, 253, 255));
            for (idx, mode) in self.display_modes.iter().enumerate() {
                let ry = l.list_row_y(idx);
                let selected = idx == self.settings.selected_mode;
                if selected {
                    c.draw_round_rect(lx + 4, ry, lw - 8, DISPLAY_LIST_ROW_H - 2, 7, Color::rgba(116, 213, 198, 90));
                }
                let color = if selected { MINT_DARK } else { INK };
                c.draw_text(&self.regular, &format!("{} × {}", mode.width, mode.height), lx + 12, ry + 5, 12.5, color);
                if mode.width == native_w && mode.height == native_h {
                    c.draw_text(&self.regular, "panel native", lx + 104, ry + 6, 11.0, MUTED);
                }
                if let Some(rate) = mode.refresh {
                    c.draw_text_right(&self.regular, &format!("{rate:.0} Hz"), lx + lw - 12, ry + 6, 11.0, if selected { MINT_DARK } else { MUTED });
                }
            }
        }
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
        draw_card(c, sx, 86, i32::from(c.width) - sx - 12, 206);
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

        draw_card(c, sx, 304, i32::from(c.width) - sx - 12, 166);
        c.draw_text(&self.bold, "System", sx + 16, 324, 15.0, INK);
        // Plain text rows with the usage percent pinned to the card's right
        // edge: the clock says more than a bar on a two-core tablet, and
        // memory reads best as used / total.
        let label_x = sx + 16;
        let value_x = sx + 64;
        let pct_right = i32::from(c.width) - 14;
        let pct_w = measure_text(&self.bold, "100%", 13.0);
        let max_w = pct_right - pct_w - 12 - value_x;
        let fit_in = |text: &str, size: f32, max_w: i32| {
            let mut t = text.to_string();
            if measure_text(&self.regular, &t, size) <= max_w {
                return t;
            }
            while measure_text(&self.regular, &format!("{t}…"), size) > max_w && t.chars().count() > 2 {
                t.pop();
            }
            format!("{}…", t.trim_end())
        };
        let fit = |text: &str, size: f32| fit_in(text, size, max_w);
        let pct_text = |v: f32| format!("{:.0}%", v.clamp(0.0, 100.0));
        let pct = |c: &mut Canvas, value: Option<f32>, y: i32| {
            if let Some(v) = value {
                let color = if v >= 85.0 { Color::rgb(196, 72, 72) } else { MINT_DARK };
                c.draw_text_right(&self.bold, &pct_text(v), pct_right, y, 13.0, color);
            }
        };
        let ratio = |used: u64, total: u64| (total > 0).then(|| used as f32 * 100.0 / total as f32);
        const ROW: i32 = 24;

        // CPU: the clock sits right next to the usage percent.
        let mut row_y = 350;
        c.draw_text(&self.regular, "CPU", label_x, row_y, 12.0, MUTED);
        pct(c, Some(self.metrics.cpu_usage), row_y);
        let clock = format_cpu_clock(self.metrics.cpu_mhz);
        let clock_right = pct_right - measure_text(&self.bold, &pct_text(self.metrics.cpu_usage), 13.0) - 10;
        c.draw_text_right(&self.bold, &clock, clock_right, row_y, 13.0, INK);
        let model_w = clock_right - measure_text(&self.bold, &clock, 13.0) - 12 - value_x;
        if !self.metrics.cpu_model.is_empty() && model_w > 40 {
            c.draw_text(&self.regular, &fit_in(&self.metrics.cpu_model, 12.0, model_w), value_x, row_y + 1, 12.0, MUTED);
        }
        row_y += ROW;
        let gpu_rows: Vec<(String, Option<f32>)> = if self.metrics.gpu_info.is_empty() {
            vec![("No GPU exposed".to_string(), None)]
        } else {
            self.metrics.gpu_info.iter().take(2).cloned().collect()
        };
        for (line, load) in gpu_rows.iter() {
            c.draw_text(&self.regular, "GPU", label_x, row_y, 12.0, MUTED);
            c.draw_text(&self.regular, &fit(line, 12.5), value_x, row_y, 12.5, INK);
            pct(c, *load, row_y);
            row_y += ROW;
        }
        let ram = if self.metrics.ram_total_kb > 0 {
            format!(
                "{} / {}",
                format_gb(self.metrics.ram_used_kb),
                format_gb(self.metrics.ram_total_kb)
            )
        } else {
            "unknown".to_string()
        };
        c.draw_text(&self.regular, "RAM", label_x, row_y, 12.0, MUTED);
        c.draw_text(&self.regular, &ram, value_x, row_y, 12.5, INK);
        pct(c, ratio(self.metrics.ram_used_kb, self.metrics.ram_total_kb), row_y);
        row_y += ROW;

        // Swap: zram and disk swap each get their own used / total rows.
        let used_total = |area: &SwapArea| {
            format!("{} / {}", format_swap_size(area.used_kb), format_swap_size(area.size_kb))
        };
        let zram: Vec<&SwapArea> = self.metrics.swaps.iter().filter(|a| a.zram).collect();
        let disk: Vec<&SwapArea> = self.metrics.swaps.iter().filter(|a| !a.zram).collect();
        if zram.is_empty() {
            c.draw_text(&self.regular, "Zram", label_x, row_y, 12.0, MUTED);
            c.draw_text(&self.regular, "off", value_x, row_y, 12.5, MUTED);
            row_y += ROW;
        }
        for area in zram.iter().take(2) {
            let name = area.path.trim_start_matches("/dev/");
            let mut parts = vec![name.to_string()];
            if let Some(algo) = area.zram_algorithm.as_deref() {
                parts.push(algo.to_string());
            }
            parts.push(used_total(area));
            let mut text = parts.join("  ·  ");
            if let Some(ram) = area.zram_ram_kb {
                let with_ram = format!("{text}  ·  {} in RAM", format_swap_size(ram));
                if measure_text(&self.regular, &with_ram, 12.5) <= max_w {
                    text = with_ram;
                }
            }
            c.draw_text(&self.regular, "Zram", label_x, row_y, 12.0, MUTED);
            c.draw_text(&self.regular, &fit(&text, 12.5), value_x, row_y, 12.5, INK);
            pct(c, ratio(area.used_kb, area.size_kb), row_y);
            row_y += ROW;
        }
        if disk.is_empty() {
            c.draw_text(&self.regular, "Swap", label_x, row_y, 12.0, MUTED);
            c.draw_text(&self.regular, "no swap file", value_x, row_y, 12.5, MUTED);
            row_y += ROW;
        }
        for area in disk.iter().take(2) {
            let text = format!("{}  ·  {}", area.path, used_total(area));
            c.draw_text(&self.regular, "Swap", label_x, row_y, 12.0, MUTED);
            c.draw_text(&self.regular, &fit(&text, 12.5), value_x, row_y, 12.5, INK);
            pct(c, ratio(area.used_kb, area.size_kb), row_y);
            row_y += ROW;
        }

        let battery_y = row_y + 10;
        draw_card(c, sx, battery_y, i32::from(c.width) - sx - 12, 64);
        c.draw_text(&self.bold, "Battery", sx + 16, battery_y + 14, 15.0, INK);
        c.draw_text(
            &self.regular,
            self.metrics
                .battery
                .as_deref()
                .unwrap_or("No battery exposed"),
            sx + 16,
            battery_y + 38,
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
        self.draw_network_restart_button(c);
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
        // Total throughput sits in the header: sparklines, then the totals on the right.
        let spark_span = card_w - 150 - 190 - 12;
        if spark_span >= 80 {
            let w = spark_span / 2;
            draw_sparkline(c, sx + 150, 96, w, 20, self.metrics.net_rx_bps, BLUE);
            draw_sparkline(c, sx + 150 + w + 12, 96, w, 20, self.metrics.net_tx_bps, MINT_DARK);
        }
        c.draw_text_right(
            &self.regular,
            &format!(
                "Down {}  Up {}",
                format_bps(self.metrics.net_rx_bps),
                format_bps(self.metrics.net_tx_bps)
            ),
            sx + card_w - 16,
            108,
            12.0,
            INK,
        );

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
            let y = 134 + idx as i32 * 24;
            c.draw_text(
                &self.regular,
                &compact(line, 44),
                sx + 16,
                y,
                13.0,
                if line.contains("  up  ") { INK } else { MUTED },
            );
            // Live speed of this interface, right-aligned on its row.
            let nic = line.split_whitespace().next().unwrap_or("");
            if let Some((_, rx, tx)) = self.metrics.nic_rates.iter().find(|(name, _, _)| name == nic) {
                c.draw_text_right(
                    &self.regular,
                    &format!("Down {}  Up {}", format_bps(*rx), format_bps(*tx)),
                    sx + card_w - 16,
                    y,
                    12.0,
                    MUTED,
                );
            }
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
    }

}

/// Row height of the open resolution dropdown.
pub(crate) const DISPLAY_LIST_ROW_H: i32 = 28;

/// Multitask tab: the topbar icon grid (two columns, four rows).
pub(crate) const MULTITASK_ICONS_CARD_Y: i32 = 232;
pub(crate) const MULTITASK_ROW_Y: i32 = MULTITASK_ICONS_CARD_Y + 60;
pub(crate) const MULTITASK_ROW_H: i32 = 40;

/// Labels and current state of the topbar icon switches, in grid order.
pub(crate) fn multitask_icon_items(icons: TopbarIcons) -> [(&'static str, bool); 7] {
    [
        ("Keyboard", icons.keyboard),
        ("Clipboard", icons.clipboard),
        ("Screenshot", icons.screenshot),
        ("Recording", icons.recording),
        ("Display", icons.display),
        ("Audio", icons.audio),
        ("Wi-Fi", icons.wifi),
    ]
}

/// Display tab geometry shared by drawing and hit testing.
pub(crate) struct DisplayLayout {
    pub(crate) card_w: i32,
    pub(crate) right: i32,
    pub(crate) dd_x: i32,
    pub(crate) dd_y: i32,
    pub(crate) dd_w: i32,
    pub(crate) dd_h: i32,
    /// Scale row: label, 1×/2×/3× selector box, and its hint line.
    pub(crate) scale_y: i32,
    pub(crate) scale_seg_y: i32,
    pub(crate) scale_hint_y: i32,
    pub(crate) rotate_y: i32,
    /// Rotation selector box (five equal segments).
    pub(crate) seg_x: i32,
    pub(crate) seg_y: i32,
    pub(crate) seg_w: i32,
    pub(crate) seg_h: i32,
    pub(crate) switch_x: i32,
    pub(crate) refresh_card_y: i32,
    pub(crate) refresh_y: i32,
    pub(crate) bright_card_y: i32,
    pub(crate) bright_y: i32,
    pub(crate) bar_x: i32,
    pub(crate) bar_y: i32,
    pub(crate) bar_w: i32,
    /// Left edge of the brightness "+" button (28x28, centred on the bar).
    pub(crate) bright_plus_x: i32,
    pub(crate) sleep_card_y: i32,
    pub(crate) sleep_y: i32,
    pub(crate) minus_x: i32,
    pub(crate) plus_x: i32,
    pub(crate) status_y: i32,
}

impl DisplayLayout {
    pub(crate) fn new(width: i32) -> Self {
        let sx = SIDEBAR_WIDTH + 24;
        let right = width - 38;
        let dd_x = sx + 130;
        // Rotation selector: right-aligned, clear of the "Rotation" label.
        let seg_w = 280.min(right - (sx + 16 + 96)).max(5 * 34);
        Self {
            card_w: width - sx - 24,
            right,
            dd_x,
            dd_y: 98,
            dd_w: (right - dd_x).max(140),
            dd_h: 30,
            scale_y: 146,
            scale_seg_y: 140,
            scale_hint_y: 174,
            rotate_y: 200,
            seg_x: right - seg_w,
            seg_y: 194,
            seg_w,
            seg_h: 28,
            switch_x: width - 78,
            refresh_card_y: 258,
            refresh_y: 274,
            bright_card_y: 326,
            bright_y: 342,
            // Room on the left for the current level ("100%").
            bar_x: sx + 62,
            bar_y: 370,
            bar_w: (right - 28 - 12 - (sx + 62)).max(100),
            bright_plus_x: right - 28,
            sleep_card_y: 396,
            sleep_y: 408,
            minus_x: right - 150,
            plus_x: right - 28,
            status_y: 452,
        }
    }

    /// The open resolution list: one row per panel mode.
    pub(crate) fn list_rect(&self, rows: usize) -> (i32, i32, i32, i32) {
        let h = rows.max(1) as i32 * DISPLAY_LIST_ROW_H + 8;
        (self.dd_x, self.dd_y + self.dd_h + 4, self.dd_w, h)
    }

    pub(crate) fn list_row_y(&self, idx: usize) -> i32 {
        self.list_rect(0).1 + 4 + idx as i32 * DISPLAY_LIST_ROW_H
    }

    /// Left edge and width of scale segment `idx` (three equal segments).
    pub(crate) fn scale_seg_rect(&self, idx: usize) -> (i32, i32) {
        let n = GDK_SCALE_CHOICES.len() as i32;
        let x0 = self.seg_x + self.seg_w * idx as i32 / n;
        let x1 = self.seg_x + self.seg_w * (idx as i32 + 1) / n;
        (x0, x1 - x0)
    }

    /// Scale segment under (x, y), with a little slack for touch.
    pub(crate) fn scale_seg_at(&self, x: i32, y: i32) -> Option<usize> {
        if y < self.scale_seg_y - 8 || y > self.scale_seg_y + self.seg_h + 8 || x < self.seg_x - 4 || x > self.seg_x + self.seg_w + 4 {
            return None;
        }
        let n = GDK_SCALE_CHOICES.len();
        Some((0..n).find(|&i| x < self.scale_seg_rect(i).0 + self.scale_seg_rect(i).1).unwrap_or(n - 1))
    }

    /// Left edge and width of rotation segment `idx`.
    pub(crate) fn seg_rect(&self, idx: usize) -> (i32, i32) {
        let n = ROTATION_CHOICES.len() as i32;
        let x0 = self.seg_x + self.seg_w * idx as i32 / n;
        let x1 = self.seg_x + self.seg_w * (idx as i32 + 1) / n;
        (x0, x1 - x0)
    }

    /// Rotation segment under (x, y), with a little slack for touch.
    pub(crate) fn seg_at(&self, x: i32, y: i32) -> Option<usize> {
        if y < self.seg_y - 8 || y > self.seg_y + self.seg_h + 8 || x < self.seg_x - 4 || x > self.seg_x + self.seg_w + 4 {
            return None;
        }
        let n = ROTATION_CHOICES.len();
        Some((0..n).find(|&i| x < self.seg_rect(i).0 + self.seg_rect(i).1).unwrap_or(n - 1))
    }

    pub(crate) fn in_dropdown_button(&self, x: i32, y: i32) -> bool {
        point_in_rect(x, y, self.dd_x, self.dd_y, self.dd_w, self.dd_h)
    }

    /// Index of the resolution row under (x, y) in a list of `rows`, if any.
    pub(crate) fn list_row_at(&self, x: i32, y: i32, rows: usize) -> Option<usize> {
        let (lx, ly, lw, lh) = self.list_rect(rows);
        if !point_in_rect(x, y, lx, ly + 4, lw, lh - 8) {
            return None;
        }
        let idx = ((y - ly - 4) / DISPLAY_LIST_ROW_H) as usize;
        (idx < rows).then_some(idx)
    }
}

/// Settings on/off switch (40x24), as used by the compositor toggle.
pub(crate) fn draw_switch(c: &mut Canvas, x: i32, y: i32, on: bool) {
    if on {
        c.draw_round_rect(x, y, 40, 24, 12, Color::rgba(160, 238, 220, 210));
        c.draw_circle(x + 28, y + 12, 8, Color::rgb(255, 255, 255));
    } else {
        c.draw_round_rect(x, y, 40, 24, 12, Color::rgba(200, 200, 200, 180));
        c.draw_circle(x + 12, y + 12, 8, Color::rgb(255, 255, 255));
    }
}

/// Small dropdown chevron centred at (cx, cy); points up when open.
pub(crate) fn draw_chevron(c: &mut Canvas, cx: i32, cy: i32, up: bool, color: Color) {
    for i in 0..5 {
        let half = 4 - i;
        let y = if up { cy + 2 - i } else { cy - 2 + i };
        c.draw_rect(cx - half, y, half * 2 + 1, 1, color);
    }
}
