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
use crate::textutil::*;
use crate::procutil::*;
use crate::files::*;

impl Aurora {
    pub(crate) fn redraw_everything(&mut self) -> AnyResult<()> {
        self.redraw_wallpaper()?;
        self.redraw_folder()?;
        if self.folder_terminal.visible {
            self.redraw_folder_terminal()?;
        }
        self.redraw_topbar()?;
        self.redraw_dock()?;
        if self.dock_more_visible {
            self.redraw_dock_more_menu()?;
        }
        self.redraw_settings()?;
        if self.app_menu_visible {
            self.redraw_app_menu()?;
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
        for client in self.clients.keys().copied().collect::<Vec<_>>() {
            self.redraw_frame_titlebar(client)?;
        }
        self.raise_ui()?;
        self.conn.flush()?;
        Ok(())
    }

    pub(crate) fn redraw_wallpaper(&mut self) -> AnyResult<()> {
        let pixmap = self.conn.generate_id()?;
        self.conn.create_pixmap(
            self.depth,
            pixmap,
            self.root,
            self.screen_width,
            self.screen_height,
        )?;
        let canvas = Canvas {
            width: self.screen_width,
            height: self.screen_height,
            data: self.wallpaper_pixels.clone(),
        };
        self.upload_canvas(pixmap, &canvas)?;
        self.conn.change_window_attributes(
            self.root,
            &ChangeWindowAttributesAux::new().background_pixmap(pixmap),
        )?;
        self.conn.clear_area(
            false,
            self.root,
            0,
            0,
            self.screen_width,
            self.screen_height,
        )?;
        self.install_pointer_cursor()?;
        if let Some(old) = self.wallpaper_pixmap.replace(pixmap) {
            let _ = self.conn.free_pixmap(old);
        }
        Ok(())
    }

    pub(crate) fn clear_root_region(&self, x: i32, y: i32, width: u32, height: u32) -> AnyResult<()> {
        let x0 = x.clamp(0, i32::from(self.screen_width));
        let y0 = y.clamp(0, i32::from(self.screen_height));
        let x1 = (x.saturating_add(width.min(i32::MAX as u32) as i32))
            .clamp(0, i32::from(self.screen_width));
        let y1 = (y.saturating_add(height.min(i32::MAX as u32) as i32))
            .clamp(0, i32::from(self.screen_height));
        let clear_w = x1.saturating_sub(x0);
        let clear_h = y1.saturating_sub(y0);
        if clear_w <= 0 || clear_h <= 0 {
            return Ok(());
        }
        let x = x0 as i16;
        let y = y0 as i16;
        let w = clear_w.min(i32::from(u16::MAX)) as u16;
        let h = clear_h.min(i32::from(u16::MAX)) as u16;
        if let Some(pixmap) = self.wallpaper_pixmap {
            self.conn
                .copy_area(pixmap, self.root, self.gc, x, y, x, y, w, h)?;
        } else {
            self.conn.clear_area(false, self.root, x, y, w, h)?;
        }
        Ok(())
    }

    pub(crate) fn topbar_controls(&self) -> TopbarControls {
        let battery = self.metrics.battery.as_deref().unwrap_or("100%");
        let battery_right = i32::from(self.screen_width) - 16;
        let battery_left = battery_right - measure_text(&self.bold, battery, 19.0) - 44;
        let network_x = battery_left - 22;
        let audio_x = network_x - TOPBAR_ICON_SPACING;
        let display_x = audio_x - TOPBAR_ICON_SPACING;
        let screenshot_x = display_x - TOPBAR_ICON_SPACING;
        let clipboard_x = screenshot_x - TOPBAR_ICON_SPACING;
        TopbarControls {
            clipboard_x,
            screenshot_x,
            display_x,
            audio_x,
            network_x,
            battery_left,
            battery_right,
        }
    }

    pub(crate) fn workspace_x(&self, index: usize) -> i32 {
        let brand_x = 24;
        let aurora_width = measure_text(&self.bold, "Aurora", 16.0);
        let start_x = brand_x + 23 + aurora_width + 24;
        start_x + index as i32 * WORKSPACE_STRIDE
    }

    pub(crate) fn add_workspace_x(&self) -> i32 {
        self.workspace_x(self.workspace_count)
    }

    pub(crate) fn redraw_topbar(&self) -> AnyResult<()> {
        let mut c = Canvas::from_wallpaper_crop(
            &self.wallpaper_pixels,
            self.screen_width,
            0,
            0,
            self.screen_width,
            TOPBAR_HEIGHT,
        );
        c.draw_rect(
            0,
            0,
            i32::from(c.width),
            i32::from(c.height),
            Color::rgba(23, 34, 42, 178),
        );

        // Draw Brand on the far left
        let brand_x = 24;
        c.draw_circle(brand_x, 20, 10, Color::rgba(160, 238, 220, 38));
        c.draw_circle(brand_x, 20, 7, MINT_LIGHT);
        c.draw_circle(brand_x - 2, 18, 2, Color::rgb(248, 255, 254));
        c.draw_text(
            &self.bold,
            "Aurora",
            brand_x + 23,
            11,
            16.0,
            Color::rgb(239, 252, 250),
        );

        // Draw Workspace icons to the right of the Brand
        for index in 0..self.workspace_count {
            draw_workspace_icon(
                &mut c,
                self.workspace_x(index),
                11,
                index == self.active_workspace,
            );
        }
        let add_x = self.add_workspace_x();
        draw_add_workspace_icon(&mut c, add_x, 20);

        let clock = self
            .topbar_notice
            .as_ref()
            .map(|(message, _)| message.clone())
            .unwrap_or_else(format_clock);
        c.draw_text_center(
            &self.regular,
            &clock,
            i32::from(self.screen_width) / 2,
            10,
            16.0,
            Color::rgb(239, 252, 250),
        );

        let controls = self.topbar_controls();
        draw_clipboard_icon(&mut c, controls.clipboard_x, 20, MINT_LIGHT);
        draw_screenshot_icon(&mut c, controls.screenshot_x, 20, MINT_LIGHT);
        draw_sidebar_display_icon(&mut c, controls.display_x, 20, MINT_LIGHT);
        draw_sidebar_audio_icon(&mut c, controls.audio_x, 20, MINT_LIGHT);
        draw_sidebar_network_icon(&mut c, controls.network_x, 20, MINT_LIGHT);
        let battery = self.metrics.battery.as_deref().unwrap_or("100%");
        c.draw_round_rect(
            controls.battery_left,
            7,
            controls.battery_right - controls.battery_left,
            26,
            9,
            if self.settings_visible && self.settings.tab == SettingsTab::Power {
                Color::rgba(116, 213, 198, 118)
            } else {
                Color::rgba(255, 255, 255, 42)
            },
        );
        draw_power_icon(&mut c, controls.battery_left + 14, 20, MINT_LIGHT);
        c.draw_text(
            &self.bold,
            battery,
            controls.battery_left + 30,
            9,
            19.0,
            Color::rgb(239, 252, 250),
        );
        self.upload_canvas(self.ui.topbar, &c)
    }

    pub(crate) fn redraw_clipboard_menu(&mut self) -> AnyResult<()> {
        let (_, _, w, h) = self.clipboard_menu_geometry();
        let mut c = Canvas::new(w, h, Color::rgb(247, 252, 255));
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            14,
            Color::rgb(247, 252, 255),
        );
        c.draw_text(&self.bold, "Clipboard", 18, 16, 15.0, INK);
        let page_count = self.clipboard_page_count();
        let page = self.clamped_clipboard_page();
        let has_prev = page > 0;
        let has_next = page + 1 < page_count;
        self.draw_clipboard_nav_button(&mut c, CLIPBOARD_MENU_PREV_X, "<", has_prev);
        self.draw_clipboard_nav_button(&mut c, CLIPBOARD_MENU_NEXT_X, ">", has_next);
        let page_label = if page_count == 0 {
            "0/0".to_string()
        } else {
            format!("{}/{}", page + 1, page_count)
        };
        c.draw_text_right(
            &self.bold,
            &page_label,
            i32::from(w) - 16,
            16,
            12.0,
            SOFT_INK,
        );
        if self.clipboard_history.is_empty() {
            draw_clipboard_icon(&mut c, i32::from(w) / 2, 78, MUTED);
            c.draw_text_center(
                &self.regular,
                "No clipboard history yet",
                i32::from(w) / 2,
                112,
                13.0,
                MUTED,
            );
            return self.upload_canvas(self.ui.clipboard_menu, &c);
        }

        let (start, end) = self.clipboard_page_range();
        let visible_entries = self.clipboard_history[start..end].to_vec();
        let mut row_y = 46;
        for entry in visible_entries.iter() {
            let row_h = clipboard_entry_row_height(entry);
            c.draw_round_rect(
                12,
                row_y - 8,
                i32::from(w) - 24,
                row_h - 8,
                9,
                Color::rgba(255, 255, 255, 150),
            );
            match &entry.item {
                ClipboardItem::Text(text) => {
                    draw_text_file_icon(&mut c, 34, row_y + 16, SOFT_INK);
                    let (line_one, line_two) = clipboard_text_preview_lines(text);
                    c.draw_text(
                        &self.regular,
                        &line_one,
                        58,
                        if line_two.is_some() {
                            row_y + 2
                        } else {
                            row_y + 10
                        },
                        13.0,
                        INK,
                    );
                    if let Some(line_two) = line_two {
                        c.draw_text(&self.regular, &line_two, 58, row_y + 21, 13.0, MUTED);
                    }
                }
                ClipboardItem::Image(path) => {
                    self.ensure_clipboard_image_preview(path);
                    draw_picture_icon(&mut c, 34, row_y + 18, MINT_DARK);
                    let info_x = 58;
                    let preview_x = i32::from(w) / 2;
                    let preview_y = row_y - 1;
                    let preview_w = (i32::from(w) - preview_x - 20)
                        .max(80)
                        .min(CLIPBOARD_MENU_IMAGE_PREVIEW_W);
                    let preview_h = (row_h - 14).min(CLIPBOARD_MENU_IMAGE_PREVIEW_H);
                    c.draw_round_rect(
                        preview_x,
                        preview_y,
                        preview_w,
                        preview_h,
                        8,
                        Color::rgba(255, 255, 255, 220),
                    );
                    if let Some(Some(preview)) = self.clipboard_image_previews.get(path) {
                        paint_cached_image_preview_left(
                            &mut c, preview, preview_x, preview_y, preview_w, preview_h,
                        );
                    }
                    c.draw_text(&self.bold, "Image", info_x, row_y, 13.0, INK);
                    let label = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("image");
                    c.draw_text(
                        &self.regular,
                        &compact(label, 22),
                        info_x,
                        row_y + 15,
                        10.5,
                        MUTED,
                    );
                    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                    c.draw_text(
                        &self.regular,
                        &format!("Size {}", format_size_mb(size)),
                        info_x,
                        row_y + 30,
                        10.5,
                        SOFT_INK,
                    );
                    let kind = clipboard_image_type_label(path);
                    let resolution = self
                        .clipboard_image_previews
                        .get(path)
                        .and_then(|preview| preview.as_ref())
                        .and_then(|preview| preview.resolution)
                        .map(|(iw, ih)| format!("{iw}x{ih}"))
                        .unwrap_or_else(|| "...".to_string());
                    c.draw_text(
                        &self.regular,
                        &format!("{kind}  {resolution}"),
                        info_x,
                        row_y + 45,
                        10.5,
                        SOFT_INK,
                    );
                }
            }
            row_y += row_h;
        }
        self.upload_canvas(self.ui.clipboard_menu, &c)
    }

    pub(crate) fn clipboard_page_count(&self) -> usize {
        self.clipboard_history
            .len()
            .div_ceil(CLIPBOARD_MENU_VISIBLE_ROWS)
    }

    pub(crate) fn clamped_clipboard_page(&self) -> usize {
        self.clipboard_history_page
            .min(self.clipboard_page_count().saturating_sub(1))
    }

    pub(crate) fn clipboard_page_range(&self) -> (usize, usize) {
        let start = self.clamped_clipboard_page() * CLIPBOARD_MENU_VISIBLE_ROWS;
        let end = (start + CLIPBOARD_MENU_VISIBLE_ROWS).min(self.clipboard_history.len());
        (start, end)
    }

    pub(crate) fn clipboard_page_content_height(&self) -> i32 {
        if self.clipboard_history.is_empty() {
            return 0;
        }
        let (start, end) = self.clipboard_page_range();
        self.clipboard_history[start..end]
            .iter()
            .map(clipboard_entry_row_height)
            .sum()
    }

    pub(crate) fn configure_clipboard_menu(&self) -> AnyResult<()> {
        let menu = self.clipboard_menu_geometry();
        self.conn.configure_window(
            self.ui.clipboard_menu,
            &ConfigureWindowAux::new()
                .x(i32::from(menu.0))
                .y(i32::from(menu.1))
                .width(u32::from(menu.2))
                .height(u32::from(menu.3)),
        )?;
        Ok(())
    }

    pub(crate) fn ensure_clipboard_image_preview(&mut self, path: &Path) {
        if self.clipboard_image_previews.contains_key(path)
            || self.clipboard_image_preview_pending.contains(path)
        {
            return;
        }
        let path = path.to_path_buf();
        self.clipboard_image_preview_pending.insert(path.clone());
        let tx = self.clipboard_image_preview_tx.clone();
        thread::spawn(move || {
            let preview = render_image_preview(
                &path,
                CLIPBOARD_MENU_IMAGE_PREVIEW_W,
                CLIPBOARD_MENU_IMAGE_PREVIEW_H,
            );
            let _ = tx.send(ClipboardImagePreviewResult { path, preview });
        });
    }

    pub(crate) fn poll_clipboard_image_previews(&mut self) -> AnyResult<bool> {
        let mut changed = false;
        loop {
            match self.clipboard_image_preview_rx.try_recv() {
                Ok(result) => {
                    self.clipboard_image_preview_pending.remove(&result.path);
                    self.clipboard_image_previews
                        .insert(result.path, result.preview);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if changed && self.clipboard_menu_visible {
            self.redraw_clipboard_menu()?;
        }
        Ok(changed)
    }

    pub(crate) fn draw_clipboard_nav_button(&self, c: &mut Canvas, x: i32, label: &str, enabled: bool) {
        c.draw_round_rect(
            x,
            CLIPBOARD_MENU_NAV_Y,
            CLIPBOARD_MENU_NAV_W,
            CLIPBOARD_MENU_NAV_H,
            8,
            if enabled {
                Color::rgba(225, 246, 241, 210)
            } else {
                Color::rgba(226, 234, 239, 130)
            },
        );
        c.draw_text_center(
            &self.bold,
            label,
            x + CLIPBOARD_MENU_NAV_W / 2,
            CLIPBOARD_MENU_NAV_Y + 5,
            18.0,
            if enabled { MINT_DARK } else { MUTED },
        );
    }

    pub(crate) fn redraw_dock(&mut self) -> AnyResult<()> {
        let task_windows = self.task_client_windows();
        if task_windows.len() <= 10 {
            let _ = self.hide_dock_more_menu();
        }

        let (x, y, w, h) = self.dock_geometry();
        self.conn.configure_window(
            self.ui.dock,
            &ConfigureWindowAux::new()
                .x(i32::from(x))
                .y(i32::from(y))
                .width(u32::from(w))
                .height(u32::from(h)),
        )?;
        let mut c = Canvas::from_wallpaper_crop(
            &self.wallpaper_pixels,
            self.screen_width,
            i32::from(x),
            i32::from(y),
            w,
            h,
        );

        let buttons = self.dock_button_count();
        let cy = i32::from(h) / 2;
        for i in 0..buttons {
            let icon_x = i as i32 * DOCK_STRIDE;
            let icon_y = cy - DOCK_ICON_SIZE / 2;
            if i < 5 {
                c.draw_round_rect(
                    icon_x,
                    icon_y,
                    DOCK_ICON_SIZE,
                    DOCK_ICON_SIZE,
                    DOCK_ICON_RADIUS,
                    Color::rgba(255, 255, 255, 215),
                );
                c.draw_round_rect(
                    icon_x + 1,
                    icon_y + 1,
                    42,
                    42,
                    11,
                    Color::rgba(196, 219, 229, 95),
                );
                draw_dock_icon(&mut c, i, icon_x + 22, icon_y + 22);
            } else if i == 15 && task_windows.len() > 10 {
                c.draw_round_rect(icon_x, icon_y, 44, 44, 12, Color::rgba(255, 255, 255, 215));
                c.draw_round_rect(
                    icon_x + 1,
                    icon_y + 1,
                    42,
                    42,
                    11,
                    Color::rgba(196, 219, 229, 95),
                );
                let dot_color = Color::rgba(44, 77, 91, 220);
                c.draw_circle(icon_x + 14, icon_y + 22, 3, dot_color);
                c.draw_circle(icon_x + 22, icon_y + 22, 3, dot_color);
                c.draw_circle(icon_x + 30, icon_y + 22, 3, dot_color);
            } else if let Some(client) = task_windows
                .get(i - 5)
                .and_then(|window| self.clients.get(window))
                .copied()
            {
                let active = self.active_client == Some(client.window);
                c.draw_round_rect(
                    icon_x,
                    icon_y,
                    44,
                    44,
                    12,
                    if active {
                        Color::rgba(28, 67, 111, 242)
                    } else {
                        Color::rgba(255, 255, 255, 235)
                    },
                );
                let title = self.window_title(client.window);
                if !self.paint_window_icon(&mut c, client.window, icon_x + 8, icon_y + 8, 28)
                    && !self.paint_desktop_icon(&mut c, client.window, icon_x + 8, icon_y + 8, 28)
                {
                    draw_client_task_icon(
                        &mut c,
                        &self.bold,
                        icon_x + 22,
                        icon_y + 22,
                        client.mapped,
                        &title,
                    );
                }
            }
        }

        if self.shape_supported {
            let rects = (0..buttons)
                .flat_map(|i| {
                    rounded_rect_shape_rects(
                        (i as i32 * DOCK_STRIDE) as i16,
                        0,
                        DOCK_ICON_SIZE as u16,
                        DOCK_ICON_SIZE as u16,
                        DOCK_ICON_RADIUS,
                    )
                })
                .collect::<Vec<_>>();
            self.conn.shape_rectangles(
                shape::SO::SET,
                shape::SK::BOUNDING,
                ClipOrdering::UNSORTED,
                self.ui.dock,
                0,
                0,
                &rects,
            )?;
        }
        self.upload_canvas(self.ui.dock, &c)
    }

    pub(crate) fn redraw_settings(&mut self) -> AnyResult<()> {
        if self.settings.tab == SettingsTab::Wallpaper {
            self.ensure_wallpaper_previews();
        }
        let (x, y, w, h) = self.settings_geometry();
        let mut c = Canvas::from_wallpaper_crop(
            &self.wallpaper_pixels,
            self.screen_width,
            i32::from(x),
            i32::from(y),
            w,
            h,
        );
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            18,
            Color::rgba(247, 252, 255, 226),
        );
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            18,
            Color::rgba(214, 229, 237, 70),
        );
        c.draw_rect(
            SIDEBAR_WIDTH,
            20,
            1,
            i32::from(h) - 40,
            Color::rgba(176, 198, 210, 100),
        );
        self.draw_settings_sidebar(&mut c);

        match self.settings.tab {
            SettingsTab::Display => self.draw_display_tab(&mut c),
            SettingsTab::Power => self.draw_power_tab(&mut c),
            SettingsTab::Wallpaper => self.draw_wallpaper_tab(&mut c),
            SettingsTab::Audio => self.draw_audio_tab(&mut c),
            SettingsTab::Network => self.draw_network_tab(&mut c),
            SettingsTab::Bluetooth => self.draw_bluetooth_tab(&mut c),
            SettingsTab::Startup => self.draw_startup_tab(&mut c),
            SettingsTab::Apps => self.draw_apps_tab(&mut c),
            SettingsTab::Shortcuts => self.draw_shortcuts_tab(&mut c),
            SettingsTab::About => self.draw_about_tab(&mut c),
        }
        self.upload_canvas(self.ui.settings, &c)
    }

    pub(crate) fn redraw_folder(&self) -> AnyResult<()> {
        let (x, y, w, h) = self.folder_geometry();
        let mut c = Canvas::from_wallpaper_crop(
            &self.wallpaper_pixels,
            self.screen_width,
            i32::from(x),
            i32::from(y),
            w,
            h,
        );
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            18,
            Color::rgba(247, 252, 255, 212),
        );
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            18,
            Color::rgba(214, 229, 237, 70),
        );
        c.draw_round_rect(18, 18, 30, 30, 10, Color::rgba(255, 255, 255, 155));
        draw_home_icon(&mut c, 33, 33, MINT_DARK);
        c.draw_round_rect(
            56,
            18,
            FOLDER_HEADER_ICON,
            FOLDER_HEADER_ICON,
            10,
            Color::rgba(255, 255, 255, 155),
        );
        draw_terminal_icon(
            &mut c,
            71,
            33,
            if self.folder_terminal.visible {
                MINT_DARK
            } else {
                SOFT_INK
            },
        );
        c.draw_round_rect(
            94,
            18,
            FOLDER_HEADER_ICON,
            FOLDER_HEADER_ICON,
            10,
            Color::rgba(255, 255, 255, 155),
        );
        draw_sort_icon(&mut c, 109, 33, MINT_DARK);
        c.draw_text(
            &self.bold,
            &compact_path(&self.folder_path, 28),
            18,
            54,
            14.0,
            MINT_DARK,
        );
        c.draw_round_rect(
            i32::from(w) - 50,
            18,
            30,
            30,
            10,
            Color::rgba(255, 255, 255, 155),
        );
        draw_more_icon(&mut c, i32::from(w) - 35, 33, MINT_DARK);
        c.draw_rect(
            18,
            72,
            i32::from(w) - 36,
            1,
            Color::rgba(178, 202, 214, 110),
        );

        if self.folder_entries.is_empty() {
            c.draw_text(
                &self.regular,
                "No common media files here.",
                24,
                108,
                13.0,
                MUTED,
            );
        } else {
            let limit = self.folder_visible_row_count();
            for (idx, entry) in self
                .folder_entries
                .iter()
                .skip(self.folder_scroll)
                .take(limit)
                .enumerate()
            {
                let row_y = 90 + idx as i32 * 42;
                let selected = self.folder_selected.as_ref() == Some(&entry.path);
                c.draw_round_rect(
                    16,
                    row_y - 4,
                    i32::from(w) - 32,
                    34,
                    9,
                    if selected {
                        Color::rgba(116, 213, 198, 118)
                    } else {
                        Color::rgba(255, 255, 255, 118)
                    },
                );
                draw_file_kind_icon(&mut c, entry.kind, 35, row_y + 13);
                c.draw_text(&self.bold, &compact(&entry.name, 28), 58, row_y, 13.0, INK);
                c.draw_text(
                    &self.regular,
                    file_kind_label(entry.kind),
                    58,
                    row_y + 18,
                    10.0,
                    MUTED,
                );
            }
        }

        if self.folder_more_open {
            let menu_x = i32::from(w) - 214;
            let menu_y = 54;
            let menu_h = 44 + self.folder_places.len().min(6) as i32 * 28;
            c.draw_round_rect(
                menu_x,
                menu_y,
                194,
                menu_h,
                12,
                Color::rgba(250, 254, 255, 238),
            );
            c.draw_text(&self.bold, "Places", menu_x + 14, menu_y + 12, 14.0, INK);
            for (idx, place) in self.folder_places.iter().take(6).enumerate() {
                let y = menu_y + 40 + idx as i32 * 28;
                c.draw_round_rect(
                    menu_x + 8,
                    y - 5,
                    178,
                    23,
                    7,
                    Color::rgba(234, 246, 249, 130),
                );
                draw_folder_icon(&mut c, menu_x + 22, y + 6, MINT_DARK);
                c.draw_text(
                    &self.regular,
                    &compact(&place.name, 20),
                    menu_x + 42,
                    y,
                    11.0,
                    INK,
                );
            }
        }
        if self.folder_sort_open {
            let menu_x = 94;
            let menu_y = 54;
            c.draw_round_rect(menu_x, menu_y, 122, 96, 12, Color::rgba(250, 254, 255, 242));
            for (idx, sort) in [FolderSort::Name, FolderSort::Date, FolderSort::Size]
                .iter()
                .copied()
                .enumerate()
            {
                let y = menu_y + 16 + idx as i32 * 28;
                if sort == self.folder_sort {
                    c.draw_round_rect(
                        menu_x + 8,
                        y - 5,
                        106,
                        23,
                        7,
                        Color::rgba(116, 213, 198, 92),
                    );
                }
                c.draw_text(&self.regular, sort.label(), menu_x + 18, y, 12.0, INK);
            }
        }
        if self.folder_context_open {
            let menu_x = self.folder_context_pos.0.min(i32::from(w) - 166).max(10);
            let menu_y = self.folder_context_pos.1.min(i32::from(h) - 178).max(78);
            let items = ["Open other app", "Copy", "Cut", "Paste", "Info"];
            c.draw_round_rect(
                menu_x,
                menu_y,
                156,
                164,
                10,
                Color::rgba(250, 254, 255, 242),
            );
            for (idx, item) in items.iter().enumerate() {
                let y = menu_y + 14 + idx as i32 * 29;
                c.draw_text(&self.regular, item, menu_x + 14, y, 12.0, INK);
            }
        }
        if let Some(info) = self.folder_info.as_ref() {
            c.draw_text(
                &self.regular,
                &compact(info, 46),
                28,
                i32::from(h) - 24,
                12.0,
                MUTED,
            );
        }
        let displayed_count = self.folder_visible_row_count();
        if self.folder_entries.len() > displayed_count {
            let track_h = i32::from(h) - 100 - if self.choose_file_mode { 42 } else { 0 };
            let track_x = i32::from(w) - 13;
            c.draw_round_rect(track_x, 84, 5, track_h, 3, Color::rgba(176, 198, 210, 90));
            let thumb_h = ((track_h as f32 * displayed_count as f32
                / self.folder_entries.len() as f32) as i32)
                .max(34)
                .min(track_h);
            let max_scroll = self
                .folder_entries
                .len()
                .saturating_sub(displayed_count)
                .max(1);
            let thumb_y = 84
                + ((track_h - thumb_h) as f32 * self.folder_scroll.min(max_scroll) as f32
                    / max_scroll as f32) as i32;
            c.draw_round_rect(
                track_x,
                thumb_y,
                5,
                thumb_h,
                3,
                Color::rgba(29, 145, 137, 180),
            );
        }

        if self.choose_file_mode {
            let cancel_x = i32::from(w) - 190;
            let choose_x = i32::from(w) - 100;
            let btn_y = i32::from(h) - 46;

            c.draw_round_rect(cancel_x, btn_y, 80, 32, 8, Color::rgba(241, 126, 135, 150));
            c.draw_text_center(
                &self.bold,
                "Cancel",
                cancel_x + 40,
                btn_y + 20,
                12.0,
                Color::rgb(160, 58, 68),
            );

            c.draw_round_rect(choose_x, btn_y, 80, 32, 8, Color::rgba(160, 238, 220, 200));
            c.draw_text_center(
                &self.bold,
                "Open",
                choose_x + 40,
                btn_y + 20,
                12.0,
                MINT_DARK,
            );

            if let Some(selected) = self.folder_selected.as_ref() {
                let name = selected.file_name().unwrap_or_default().to_string_lossy();
                c.draw_text(
                    &self.regular,
                    &compact(&format!("File: {name}"), 24),
                    24,
                    btn_y + 20,
                    12.0,
                    INK,
                );
            } else {
                c.draw_text(&self.regular, "Select a file", 24, btn_y + 20, 12.0, MUTED);
            }
        }

        self.upload_canvas(self.ui.folder, &c)
    }

    pub(crate) fn cancel_choose_file(&mut self) -> AnyResult<()> {
        self.choose_file_mode = false;
        let result_atom = self
            .conn
            .intern_atom(false, b"_AURORA_CHOOSE_FILE_RESULT")?
            .reply()?
            .atom;
        let string_atom = self.conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        self.conn.change_property8(
            PropMode::REPLACE,
            self.root,
            result_atom,
            string_atom,
            b"CANCEL",
        )?;
        self.conn.unmap_window(self.ui.folder)?;
        if self.folder_terminal.visible {
            self.conn.unmap_window(self.ui.folder_terminal)?;
        }
        self.redraw_folder()?;
        self.conn.flush()?;
        Ok(())
    }

    pub(crate) fn submit_choose_file(&mut self) -> AnyResult<()> {
        let Some(path) = self.folder_selected.clone() else {
            return Ok(());
        };
        self.choose_file_mode = false;
        let result_atom = self
            .conn
            .intern_atom(false, b"_AURORA_CHOOSE_FILE_RESULT")?
            .reply()?
            .atom;
        let string_atom = self.conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let path_str = path.to_string_lossy();
        self.conn.change_property8(
            PropMode::REPLACE,
            self.root,
            result_atom,
            string_atom,
            path_str.as_bytes(),
        )?;
        self.conn.unmap_window(self.ui.folder)?;
        if self.folder_terminal.visible {
            self.conn.unmap_window(self.ui.folder_terminal)?;
        }
        self.redraw_folder()?;
        self.conn.flush()?;
        Ok(())
    }

    pub(crate) fn redraw_app_menu(&self) -> AnyResult<()> {
        let (x, y, w, h) = self.app_menu_geometry();
        let mut c = Canvas::from_wallpaper_crop(
            &self.wallpaper_pixels,
            self.screen_width,
            i32::from(x),
            i32::from(y),
            w,
            h,
        );
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            16,
            Color::rgba(248, 253, 255, 232),
        );
        c.draw_text(&self.bold, "Apps", 20, 18, 20.0, INK);

        let search_x = 92;
        let search_w = i32::from(w) - search_x - 18;
        c.draw_round_rect(search_x, 11, search_w, 34, 11, Color::rgba(255, 255, 255, 205));
        c.draw_round_rect(
            search_x + 1,
            12,
            search_w - 2,
            32,
            10,
            Color::rgba(176, 198, 210, 75),
        );
        draw_search_icon(&mut c, search_x + 18, 28, MINT_DARK);
        let search_label = if self.app_menu_query.is_empty() {
            "Search all apps (typos are okay)"
        } else {
            &self.app_menu_query
        };
        c.draw_text(
            &self.regular,
            &compact(search_label, if self.app_menu_more { 62 } else { 32 }),
            search_x + 36,
            20,
            12.0,
            if self.app_menu_query.is_empty() { MUTED } else { INK },
        );
        if self.app_menu_visible && !self.app_menu_query.is_empty() {
            let shown_query = compact(&self.app_menu_query, if self.app_menu_more { 62 } else { 32 });
            let cursor_x = search_x
                + 38
                + measure_text(&self.regular, &shown_query, 12.0) as i32;
            c.draw_rect(cursor_x.min(search_x + search_w - 10), 19, 1, 16, MINT_DARK);
        }

        let compact_searching = !self.app_menu_more && !self.app_menu_query.is_empty();
        if compact_searching {
            let matches = app_catalog_rows(
                &self.app_menu_query,
                &self.app_menu_expanded_categories,
            )
            .into_iter()
            .filter_map(|row| match row {
                AppCatalogRow::App { name, command } => Some((name, command)),
                _ => None,
            })
            .take(6)
            .collect::<Vec<_>>();
            if matches.is_empty() {
                c.draw_text(&self.regular, "No close matches", 20, 76, 12.0, MUTED);
            }
            for (idx, (name, _)) in matches.iter().enumerate() {
                let row_y = 64 + idx as i32 * 42;
                c.draw_round_rect(
                    14,
                    row_y - 5,
                    i32::from(w) - 28,
                    34,
                    9,
                    Color::rgba(255, 255, 255, 120),
                );
                c.draw_circle(34, row_y + 12, 8, Color::rgba(75, 142, 177, 55));
                c.draw_circle(34, row_y + 12, 4, Color::rgba(75, 142, 177, 210));
                c.draw_text(&self.bold, &compact(name, 38), 58, row_y + 1, 13.0, INK);
                c.draw_text(&self.regular, "Fuzzy match", 58, row_y + 17, 10.0, MUTED);
            }
        } else {
            let apps = app_menu_items();
            for (idx, app) in apps.iter().enumerate() {
                let row_y = 64 + idx as i32 * 42;
                let row_w = if self.app_menu_more { 238 } else { i32::from(w) - 28 };
                c.draw_round_rect(
                    14,
                    row_y - 5,
                    row_w,
                    34,
                    9,
                    Color::rgba(255, 255, 255, 120),
                );
                draw_launcher_icon(&mut c, idx, 34, row_y + 12);
                c.draw_text(&self.bold, app.label, 58, row_y, 13.0, INK);
                c.draw_text(&self.regular, app.hint, 58, row_y + 17, 10.0, MUTED);
            }
        }

        if self.app_menu_more {
            let x0 = 276;
            c.draw_rect(
                x0 - 12,
                58,
                1,
                i32::from(h) - 76,
                Color::rgba(176, 198, 210, 100),
            );
            c.draw_text(&self.bold, "All apps", x0, 62, 16.0, INK);
            let rows = app_catalog_rows(
                &self.app_menu_query,
                &self.app_menu_expanded_categories,
            );
            let visible = ((i32::from(h) - 100) / 30).max(1) as usize;
            let max_scroll = rows.len().saturating_sub(visible);
            let start = self.app_menu_scroll.min(max_scroll);
            let mut row_y = 92;
            for row in rows.iter().skip(start).take(visible) {
                match row {
                    AppCatalogRow::Category { name, count, expanded } => {
                        c.draw_round_rect(
                            x0,
                            row_y - 4,
                            i32::from(w) - x0 - 26,
                            26,
                            8,
                            Color::rgba(214, 239, 236, 165),
                        );
                        draw_catalog_chevron(&mut c, x0 + 13, row_y + 9, *expanded, MINT_DARK);
                        c.draw_text(&self.bold, name, x0 + 28, row_y + 1, 12.0, MINT_DARK);
                        c.draw_text(
                            &self.regular,
                            &count.to_string(),
                            i32::from(w) - 48,
                            row_y + 1,
                            11.0,
                            MUTED,
                        );
                    }
                    AppCatalogRow::App { name, .. } => {
                        c.draw_round_rect(
                            x0 + 10,
                            row_y - 4,
                            i32::from(w) - x0 - 36,
                            26,
                            8,
                            Color::rgba(255, 255, 255, 105),
                        );
                        c.draw_circle(x0 + 24, row_y + 9, 4, Color::rgba(75, 142, 177, 190));
                        c.draw_text(
                            &self.regular,
                            &compact(name, 37),
                            x0 + 38,
                            row_y + 1,
                            12.0,
                            INK,
                        );
                    }
                }
                row_y += 30;
            }
            if rows.is_empty() {
                c.draw_text(&self.regular, "No close matches", x0, 100, 12.0, MUTED);
            }
            if rows.len() > visible {
                let track_x = i32::from(w) - 18;
                let track_h = i32::from(h) - 104;
                c.draw_round_rect(track_x, 90, 5, track_h, 3, Color::rgba(176, 198, 210, 95));
                let thumb_h = ((track_h as f32 * visible as f32 / rows.len() as f32) as i32)
                    .max(34)
                    .min(track_h);
                let max_scroll = max_scroll.max(1);
                let thumb_y = 90
                    + ((track_h - thumb_h) as f32 * self.app_menu_scroll.min(max_scroll) as f32
                        / max_scroll as f32) as i32;
                c.draw_round_rect(
                    track_x,
                    thumb_y,
                    5,
                    thumb_h,
                    3,
                    Color::rgba(29, 145, 137, 185),
                );
            }
        }
        self.upload_canvas(self.ui.app_menu, &c)
    }

    pub(crate) fn redraw_aurora_menu(&self) -> AnyResult<()> {
        let (x, y, w, h) = self.aurora_menu_geometry();
        self.conn.configure_window(
            self.ui.aurora_menu,
            &ConfigureWindowAux::new()
                .x(i32::from(x))
                .y(i32::from(y))
                .width(u32::from(w))
                .height(u32::from(h)),
        )?;
        let mut c = Canvas::from_wallpaper_crop(
            &self.wallpaper_pixels,
            self.screen_width,
            i32::from(x),
            i32::from(y),
            w,
            h,
        );
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            12,
            Color::rgba(248, 253, 255, 235),
        );
        c.draw_round_rect(
            1,
            1,
            i32::from(w) - 2,
            i32::from(h) - 2,
            11,
            Color::rgba(210, 229, 238, 130),
        );
        c.draw_circle(28, 28, 12, Color::rgba(116, 213, 198, 170));
        c.draw_circle(28, 28, 6, MINT_DARK);
        c.draw_text(&self.bold, "Aurora WM", 50, 17, 17.0, INK);
        c.draw_text(
            &self.regular,
            env!("CARGO_PKG_VERSION"),
            146,
            21,
            11.0,
            MUTED,
        );

        if self.aurora_menu_about {
            c.draw_text(&self.bold, "About", 18, 62, 14.0, INK);
            c.draw_text(
                &self.regular,
                "A small X11 desktop shell with dock, folders, media viewer,",
                18,
                84,
                11.0,
                INK,
            );
            c.draw_text(
                &self.regular,
                "settings, screenshots, and lightweight window controls.",
                18,
                100,
                11.0,
                INK,
            );
            c.draw_text(
                &self.bold,
                &format!("Display: {}", self.display),
                18,
                118,
                11.0,
                MINT_DARK,
            );
            c.draw_text(&self.bold, "Help", 18, 142, 13.0, INK);
            c.draw_text(
                &self.regular,
                "Alt+Tab switches running apps.",
                18,
                162,
                11.0,
                MUTED,
            );
            c.draw_text(
                &self.regular,
                "Use the dock for apps and settings; drag titlebars to move windows.",
                18,
                180,
                11.0,
                MUTED,
            );
            c.draw_text(
                &self.regular,
                "Bottom corners resize windows; settings are saved in ~/.config/aurora-wm.",
                18,
                198,
                11.0,
                MUTED,
            );
            c.draw_round_rect(16, 230, 76, 28, 8, Color::rgba(234, 244, 248, 220));
            c.draw_text_center(&self.bold, "Back", 54, 238, 12.0, MINT_DARK);
        } else {
            // Draw Restart WM
            {
                let row_y = 64;
                c.draw_round_rect(
                    14,
                    row_y - 8,
                    i32::from(w) - 28,
                    38,
                    9,
                    Color::rgba(255, 255, 255, 150),
                );
                draw_reload_menu_icon(&mut c, 32, row_y + 11, MINT_DARK);
                c.draw_text(&self.bold, "Restart WM", 50, row_y, 13.0, INK);
                c.draw_text(
                    &self.regular,
                    "Reload Aurora and keep saved settings",
                    50,
                    row_y + 17,
                    10.0,
                    MUTED,
                );
            }

            let mut next_row_y = 114;

            if self.aurora_menu_restart_confirm {
                c.draw_round_rect(
                    14,
                    102,
                    i32::from(w) - 28,
                    46,
                    9,
                    Color::rgba(238, 245, 248, 220),
                );
                c.draw_text(&self.bold, "Confirm?", 28, 118, 12.0, INK);

                // Yes button
                c.draw_round_rect(160, 110, 90, 28, 6, Color::rgba(232, 74, 95, 210));
                c.draw_text_center(&self.bold, "Yes", 205, 118, 12.0, Color::rgb(255, 255, 255));

                // No button
                c.draw_round_rect(270, 110, 90, 28, 6, Color::rgba(200, 215, 225, 180));
                c.draw_text_center(&self.bold, "No", 315, 118, 12.0, INK);

                next_row_y = 166;
            }

            // Draw About Aurora
            {
                let row_y = next_row_y;
                c.draw_round_rect(
                    14,
                    row_y - 8,
                    i32::from(w) - 28,
                    38,
                    9,
                    Color::rgba(255, 255, 255, 150),
                );
                draw_info_menu_icon(&mut c, 32, row_y + 11, MINT_LIGHT);
                c.draw_text(&self.bold, "About Aurora", 50, row_y, 13.0, INK);
                c.draw_text(
                    &self.regular,
                    "Version, description, and quick help",
                    50,
                    row_y + 17,
                    10.0,
                    MUTED,
                );
            }
        }
        self.upload_canvas(self.ui.aurora_menu, &c)
    }

    pub(crate) fn redraw_media_slot(&self, slot: usize) -> AnyResult<()> {
        let Some(media) = self.media_slots.get(slot).and_then(|m| m.as_ref()) else {
            return Ok(());
        };
        let (_, _, w, h) = self.media_geometry(slot);
        // Dark navy border ring so the viewer popup stands out clearly
        // against the wallpaper and light app windows.
        let mut c = Canvas::new(w, h, Color::rgb(21, 39, 66));
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            18,
            Color::rgb(21, 39, 66),
        );
        c.draw_round_rect(
            3,
            3,
            i32::from(w) - 6,
            i32::from(h) - 6,
            15,
            Color::rgb(247, 252, 255),
        );
        c.draw_text(
            &self.bold,
            &compact(&media.entry.name, 30),
            24,
            20,
            16.0,
            INK,
        );
        c.draw_text(
            &self.regular,
            file_kind_label(media.entry.kind),
            24,
            44,
            11.0,
            MUTED,
        );
        if media.entry.kind == FileKind::Text {
            let button_x = i32::from(w) - 78;
            c.draw_round_rect(button_x, 17, 28, 24, 8, Color::rgba(116, 213, 198, 110));
            if media.editing {
                draw_save_icon(&mut c, button_x + 14, 29, MINT_DARK);
            } else {
                draw_edit_icon(&mut c, button_x + 14, 29, MINT_DARK);
            }
        }
        c.draw_round_rect(
            i32::from(w) - 43,
            17,
            24,
            24,
            8,
            Color::rgba(241, 126, 135, 120),
        );
        c.draw_line(
            i32::from(w) - 37,
            23,
            i32::from(w) - 25,
            35,
            2,
            Color::rgb(255, 255, 255),
        );
        c.draw_line(
            i32::from(w) - 25,
            23,
            i32::from(w) - 37,
            35,
            2,
            Color::rgb(255, 255, 255),
        );

        let preview_x = 18;
        let preview_y = 58;
        let preview_w = i32::from(w) - 48;
        let preview_h = i32::from(h) - 130;
        let preview_bg = if media.entry.kind == FileKind::Text {
            Color::rgb(255, 255, 255)
        } else if media.entry.kind == FileKind::Image {
            // Dark navy backdrop improves perceived contrast for photos.
            Color::rgb(28, 48, 78)
        } else {
            Color::rgb(238, 247, 252)
        };
        c.draw_round_rect(preview_x, preview_y, preview_w, preview_h, 15, preview_bg);
        match media.entry.kind {
            FileKind::Text => {
                self.draw_text_viewer(
                    &mut c, slot, media, preview_x, preview_y, preview_w, preview_h,
                );
            }
            FileKind::Image => {
                let image_area_h = (preview_h - 34).max(80);
                if let Some(preview) = media.image_preview.as_ref() {
                    paint_cached_image_preview(
                        &mut c,
                        preview,
                        preview_x + 8,
                        preview_y + 8,
                        preview_w - 16,
                        image_area_h - 10,
                    );
                } else {
                    paint_file_preview(
                        &mut c,
                        &media.entry.path,
                        preview_x + 8,
                        preview_y + 8,
                        preview_w - 16,
                        image_area_h - 10,
                    );
                }
                c.draw_text(
                    &self.regular,
                    &image_info_line(
                        &media.entry.path,
                        media
                            .image_preview
                            .as_ref()
                            .and_then(|preview| preview.resolution),
                    ),
                    preview_x + 12,
                    preview_y + preview_h - 24,
                    11.0,
                    MUTED,
                );
                c.draw_round_rect(
                    preview_x,
                    preview_y,
                    preview_w,
                    preview_h,
                    15,
                    Color::rgba(255, 255, 255, 32),
                );
            }
            FileKind::Audio => {
                draw_music_icon(&mut c, i32::from(w) / 2, preview_y + 94, MINT_DARK);
                draw_sparkline(
                    &mut c,
                    preview_x + 44,
                    preview_y + 152,
                    preview_w - 88,
                    42,
                    4200.0,
                    MINT_DARK,
                );
                c.draw_text_center(
                    &self.bold,
                    if media.playing {
                        "Playing audio"
                    } else {
                        "Audio ready"
                    },
                    i32::from(w) / 2,
                    preview_y + 30,
                    18.0,
                    INK,
                );
            }
            FileKind::Video => {
                let frame_color = if media.playing {
                    Color::rgba(73, 156, 231, 80)
                } else {
                    Color::rgba(23, 34, 42, 38)
                };
                c.draw_round_rect(
                    preview_x + 18,
                    preview_y + 14,
                    preview_w - 36,
                    preview_h - 42,
                    10,
                    frame_color,
                );
                if paint_video_frame_preview(
                    &mut c,
                    &media.entry.path,
                    preview_x + 18,
                    preview_y + 14,
                    preview_w - 36,
                    preview_h - 42,
                )
                .is_none()
                {
                    draw_play_icon(&mut c, i32::from(w) / 2, preview_y + 88, BLUE);
                }
                c.draw_text_center(
                    &self.bold,
                    if media.playing {
                        "Playing video"
                    } else {
                        "Video ready"
                    },
                    i32::from(w) / 2,
                    preview_y + 30,
                    14.0,
                    INK,
                );
                c.draw_round_rect(
                    preview_x + 34,
                    preview_y + 122,
                    preview_w - 68,
                    10,
                    5,
                    Color::rgba(255, 255, 255, 120),
                );
                c.draw_round_rect(
                    preview_x + 34,
                    preview_y + 122,
                    ((preview_w - 68) as f32 * media.progress.clamp(0.0, 1.0)) as i32,
                    10,
                    5,
                    Color::rgba(73, 156, 231, 150),
                );
            }
            FileKind::Directory | FileKind::Other => {
                self.draw_unknown_file_view(
                    &mut c, media, preview_x, preview_y, preview_w, preview_h,
                );
            }
        }

        let controls_y = i32::from(h) - 62;
        c.draw_text(
            &self.regular,
            &compact_path(&media.entry.path, 42),
            24,
            controls_y - 24,
            11.0,
            MUTED,
        );
        let status = media.notice.clone().unwrap_or_else(|| viewer_status(media));
        c.draw_text_right(
            &self.regular,
            &status,
            i32::from(w) - 24,
            controls_y - 24,
            11.0,
            if media.notice.is_some() {
                MINT_DARK
            } else {
                MUTED
            },
        );
        if matches!(media.entry.kind, FileKind::Audio | FileKind::Video) {
            c.draw_round_rect(
                24,
                controls_y,
                i32::from(w) - 48,
                42,
                13,
                Color::rgba(116, 213, 198, 88),
            );
            if media.playing {
                c.draw_rect(44, controls_y + 12, 5, 18, MINT_DARK);
                c.draw_rect(54, controls_y + 12, 5, 18, MINT_DARK);
                c.draw_text(&self.bold, "Pause", 80, controls_y + 11, 13.0, INK);
            } else {
                draw_play_icon(&mut c, 50, controls_y + 21, MINT_DARK);
                c.draw_text(&self.bold, "Play", 80, controls_y + 11, 13.0, INK);
            }
            let bar_x = 150;
            let bar_w = i32::from(w) - bar_x - 48;
            c.draw_round_rect(
                bar_x,
                controls_y + 17,
                bar_w,
                8,
                4,
                Color::rgba(255, 255, 255, 140),
            );
            c.draw_round_rect(
                bar_x,
                controls_y + 17,
                (bar_w as f32 * media.progress.clamp(0.0, 1.0)) as i32,
                8,
                4,
                Color::rgba(29, 145, 137, 190),
            );
        }
        if self
            .media_context_open
            .is_some_and(|(ctx_slot, _, _)| ctx_slot == slot)
        {
            self.draw_media_context_menu(&mut c, slot, media);
        }
        if self.media_trash_prompt == Some(slot) {
            self.draw_media_trash_prompt(&mut c, media, i32::from(w), i32::from(h));
        }
        self.upload_canvas(self.ui.media[slot], &c)
    }

    pub(crate) fn draw_media_context_menu(&self, c: &mut Canvas, slot: usize, media: &MediaState) {
        let Some((_, x, y)) = self
            .media_context_open
            .filter(|(ctx_slot, _, _)| *ctx_slot == slot)
        else {
            return;
        };
        let (_, _, w, h) = self.media_geometry(slot);
        let menu_x = x.min(i32::from(w) - 184).max(12);
        let menu_y = y.min(i32::from(h) - 112).max(50);
        let items = if media.entry.kind == FileKind::Image {
            ["Rename", "Copy image", "Move to Trash"]
        } else {
            ["Rename", "Copy path", "Move to Trash"]
        };
        c.draw_round_rect(menu_x, menu_y, 172, 96, 10, Color::rgba(250, 254, 255, 244));
        for (idx, item) in items.iter().enumerate() {
            c.draw_text(
                &self.regular,
                item,
                menu_x + 14,
                menu_y + 16 + idx as i32 * 29,
                12.0,
                INK,
            );
        }
    }

    pub(crate) fn draw_media_trash_prompt(&self, c: &mut Canvas, media: &MediaState, w: i32, h: i32) {
        let box_w = 310;
        let box_h = 126;
        let x = (w - box_w) / 2;
        let y = (h - box_h) / 2;
        c.draw_round_rect(x, y, box_w, box_h, 14, Color::rgba(250, 254, 255, 246));
        c.draw_text(&self.bold, "Move to Trash?", x + 20, y + 18, 16.0, INK);
        c.draw_text(
            &self.regular,
            &compact(&media.entry.name, 34),
            x + 20,
            y + 46,
            12.0,
            MUTED,
        );
        c.draw_round_rect(x + 48, y + 82, 84, 30, 9, Color::rgba(241, 126, 135, 105));
        c.draw_text_center(&self.bold, "Yes", x + 90, y + 90, 12.0, INK);
        c.draw_round_rect(x + 174, y + 82, 84, 30, 9, Color::rgba(178, 202, 214, 110));
        c.draw_text_center(&self.bold, "No", x + 216, y + 90, 12.0, INK);
    }

    pub(crate) fn draw_text_viewer(
        &self,
        c: &mut Canvas,
        slot: usize,
        media: &MediaState,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) {
        let line_h = 19;
        let max_lines = ((h - 20) / line_h).max(1) as usize;
        let start = media
            .text_scroll
            .min(media.text_lines.len().saturating_sub(1));
        let gutter_w = 34;
        let text_x = x + gutter_w + 8;
        c.draw_rect(
            x + gutter_w,
            y + 8,
            1,
            h - 16,
            Color::rgba(178, 202, 214, 90),
        );
        for (idx, line) in media
            .text_lines
            .iter()
            .skip(start)
            .take(max_lines)
            .enumerate()
        {
            let yy = y + 12 + idx as i32 * line_h;
            c.draw_text_right(
                &self.regular,
                &(start + idx + 1).to_string(),
                x + gutter_w - 8,
                yy,
                11.0,
                MUTED,
            );
            let shown = compact(line, ((w - gutter_w - 18) / 7).max(20) as usize);
            if let Some((sel_start, sel_end)) = self
                .media_text_selection
                .as_ref()
                .filter(|selection| selection.slot == slot)
                .map(normalized_media_selection)
            {
                let line_no = start + idx;
                if line_no >= sel_start.0 && line_no <= sel_end.0 {
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
                    if end_col > start_col {
                        let sx =
                            text_x + fast_text_width_cols(&self.regular, line, 0, start_col, 13.0);
                        let sw =
                            fast_text_width_cols(&self.regular, line, start_col, end_col, 13.0)
                                .max(3);
                        c.draw_round_rect(sx, yy + 1, sw, 16, 4, Color::rgba(73, 156, 231, 70));
                    }
                }
            }
            c.draw_text(&self.regular, &shown, text_x, yy, 13.0, INK);
        }
        if media.editing {
            let cursor_line = media
                .text_cursor_line
                .min(media.text_lines.len().saturating_sub(1));
            if cursor_line >= start && cursor_line < start + max_lines {
                let visible_idx = cursor_line - start;
                let line = media
                    .text_lines
                    .get(cursor_line)
                    .map(String::as_str)
                    .unwrap_or("");
                let cursor_x = text_x
                    + fast_text_width_cols(
                        &self.regular,
                        line,
                        0,
                        media.text_cursor_col.min(line.chars().count()),
                        13.0,
                    );
                let cursor_y = y + 13 + visible_idx as i32 * line_h;
                c.draw_rect(cursor_x, cursor_y, 2, 15, MINT_DARK);
            }
        }
        if self
            .media_text_selection
            .as_ref()
            .filter(|selection| selection.slot == slot)
            .is_some_and(|selection| {
                !selected_text_from_lines(&media.text_lines, selection).is_empty()
            })
        {
            let (bx, by, bw, bh) = media_text_copy_button_rect(x, y, w, h);
            c.draw_round_rect(bx, by, bw, bh, 9, Color::rgba(116, 213, 198, 150));
            draw_copy_icon(c, bx + bw / 2, by + bh / 2, MINT_DARK);
        }
    }

    pub(crate) fn draw_unknown_file_view(
        &self,
        c: &mut Canvas,
        media: &MediaState,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) {
        draw_file_kind_icon(c, media.entry.kind, x + w / 2, y + 58);
        let meta = fs::metadata(&media.entry.path).ok();
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| format!("modified {}s", d.as_secs()))
            .unwrap_or_else(|| "modified unknown".to_string());
        let kind = media.file_info.as_deref().unwrap_or("Unknown file type");
        let size_line = format_size_mb(size);
        let lines = [
            media.entry.name.as_str(),
            size_line.as_str(),
            modified.as_str(),
            kind,
        ];
        for (idx, line) in lines.iter().enumerate() {
            c.draw_text_center(
                &self.regular,
                &compact(line, 54),
                x + w / 2,
                y + 112 + idx as i32 * 24,
                if idx == 0 { 15.0 } else { 12.0 },
                if idx == 0 { INK } else { MUTED },
            );
        }
        c.draw_round_rect(
            x + 40,
            y + h - 56,
            w - 80,
            34,
            10,
            Color::rgba(116, 213, 198, 90),
        );
        c.draw_text_center(
            &self.bold,
            "Open as text",
            x + w / 2,
            y + h - 47,
            13.0,
            MINT_DARK,
        );
    }

}
