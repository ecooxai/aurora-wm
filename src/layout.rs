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
use crate::media_ui::*;
use crate::system_apply::*;
use crate::draw_helpers::*;
use crate::pixels::*;
use crate::system::*;
use crate::textutil::*;
use crate::procutil::*;
use crate::files::*;

impl Aurora {
    pub(crate) fn resize_to_root(&mut self) -> AnyResult<()> {
        let geom = self.conn.get_geometry(self.root)?.reply()?;
        if geom.width == self.screen_width && geom.height == self.screen_height {
            return Ok(());
        }
        self.screen_width = geom.width;
        self.screen_height = geom.height;
        self.display_modes =
            read_display_modes(&self.display, self.screen_width, self.screen_height);
        if let Some(current) = self.display_modes.iter().position(|mode| mode.current) {
            self.settings.selected_mode = current;
        }
        self.wallpaper_cache = vec![None; WALLPAPERS.len()];
        self.wallpaper_pixels = render_wallpaper_pixels(
            WALLPAPERS[self.wallpaper_index].bytes,
            self.screen_width,
            self.screen_height,
        )?;
        self.wallpaper_cache[self.wallpaper_index] = Some(self.wallpaper_pixels.clone());
        self.hide_dock_more_menu()?;
        let dock = self.dock_geometry();
        let settings = self.settings_geometry();
        let folder = self.folder_geometry();
        let terminal = self.folder_terminal_geometry();
        let menu = self.app_menu_geometry();
        let clipboard_menu = self.clipboard_menu_geometry();
        self.conn.configure_window(
            self.ui.topbar,
            &ConfigureWindowAux::new()
                .x(0)
                .y(0)
                .width(u32::from(self.screen_width))
                .height(u32::from(TOPBAR_HEIGHT)),
        )?;
        self.conn.configure_window(
            self.ui.dock,
            &ConfigureWindowAux::new()
                .x(i32::from(dock.0))
                .y(i32::from(dock.1))
                .width(u32::from(dock.2))
                .height(u32::from(dock.3)),
        )?;
        self.conn.configure_window(
            self.ui.settings,
            &ConfigureWindowAux::new()
                .x(i32::from(settings.0))
                .y(i32::from(settings.1))
                .width(u32::from(settings.2))
                .height(u32::from(settings.3)),
        )?;
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
        self.conn.configure_window(
            self.ui.app_menu,
            &ConfigureWindowAux::new()
                .x(i32::from(menu.0))
                .y(i32::from(menu.1))
                .width(u32::from(menu.2))
                .height(u32::from(menu.3)),
        )?;
        let aurora_menu = self.aurora_menu_geometry();
        self.conn.configure_window(
            self.ui.aurora_menu,
            &ConfigureWindowAux::new()
                .x(i32::from(aurora_menu.0))
                .y(i32::from(aurora_menu.1))
                .width(u32::from(aurora_menu.2))
                .height(u32::from(aurora_menu.3)),
        )?;
        self.conn.configure_window(
            self.ui.clipboard_menu,
            &ConfigureWindowAux::new()
                .x(i32::from(clipboard_menu.0))
                .y(i32::from(clipboard_menu.1))
                .width(u32::from(clipboard_menu.2))
                .height(u32::from(clipboard_menu.3)),
        )?;
        self.conn.configure_window(
            self.ui.screenshot_overlay,
            &ConfigureWindowAux::new()
                .x(0)
                .y(0)
                .width(u32::from(self.screen_width))
                .height(u32::from(self.screen_height)),
        )?;
        for (idx, window) in self.ui.media.iter().copied().enumerate() {
            let media = self.media_geometry(idx);
            self.conn.configure_window(
                window,
                &ConfigureWindowAux::new()
                    .x(i32::from(media.0))
                    .y(i32::from(media.1))
                    .width(u32::from(media.2))
                    .height(u32::from(media.3)),
            )?;
        }
        self.redraw_everything()?;
        Ok(())
    }

    pub(crate) fn raise_ui(&self) -> AnyResult<()> {
        if self.settings_visible {
            self.conn.configure_window(
                self.ui.settings,
                &ConfigureWindowAux::new().stack_mode(if self.settings_front {
                    StackMode::ABOVE
                } else {
                    StackMode::BELOW
                }),
            )?;
        }
        self.conn.configure_window(
            self.ui.folder,
            &ConfigureWindowAux::new().stack_mode(if self.folder_front {
                StackMode::ABOVE
            } else {
                StackMode::BELOW
            }),
        )?;
        if self.folder_terminal.visible {
            self.conn.configure_window(
                self.ui.folder_terminal,
                &ConfigureWindowAux::new().stack_mode(if self.folder_front {
                    StackMode::ABOVE
                } else {
                    StackMode::BELOW
                }),
            )?;
        }
        for (idx, window) in self.ui.media.iter().copied().enumerate() {
            if self.media_slots.get(idx).and_then(|m| m.as_ref()).is_some() {
                self.conn.configure_window(
                    window,
                    &ConfigureWindowAux::new().stack_mode(if self.media_front_slot == Some(idx) {
                        StackMode::ABOVE
                    } else {
                        StackMode::BELOW
                    }),
                )?;
            }
        }
        if self.screenshot_mode {
            self.conn.configure_window(
                self.ui.screenshot_overlay,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
            self.conn.configure_window(
                self.ui.topbar,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        self.raise_chrome()?;
        if self.app_menu_visible {
            self.conn.configure_window(
                self.ui.app_menu,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        if self.aurora_menu_visible {
            self.conn.configure_window(
                self.ui.aurora_menu,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        if self.clipboard_menu_visible {
            self.conn.configure_window(
                self.ui.clipboard_menu,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        Ok(())
    }

    pub(crate) fn raise_chrome(&self) -> AnyResult<()> {
        // Keep the dock above app windows at all times so nothing can cover it (a covering
        // window would otherwise leave the dock area unpainted / black without a compositor).
        self.conn.configure_window(
            self.ui.dock,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?;
        self.conn.configure_window(
            self.ui.topbar,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?;
        if self.app_menu_visible {
            self.conn.configure_window(
                self.ui.app_menu,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        if self.aurora_menu_visible {
            self.conn.configure_window(
                self.ui.aurora_menu,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        if self.clipboard_menu_visible {
            self.conn.configure_window(
                self.ui.clipboard_menu,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        if self.dock_more_visible {
            self.conn.configure_window(
                self.ui.dock_more_menu,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        Ok(())
    }

    pub(crate) fn raise_media(&self) -> AnyResult<()> {
        if let Some(slot) = self.media_front_slot {
            self.conn.configure_window(
                self.ui.media[slot],
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        self.raise_chrome()
    }

    pub(crate) fn upload_canvas(&self, drawable: Drawable, canvas: &Canvas) -> AnyResult<()> {
        self.upload_canvas_at(drawable, canvas, 0, 0)
    }

    pub(crate) fn upload_canvas_at(
        &self,
        drawable: Drawable,
        canvas: &Canvas,
        x: i32,
        y: i32,
    ) -> AnyResult<()> {
        let img = Image::new(
            canvas.width,
            canvas.height,
            ScanlinePad::Pad32,
            self.depth,
            BitsPerPixel::B32,
            XrbImageOrder::LsbFirst,
            Cow::Borrowed(&canvas.data),
        )?;
        img.put(&self.conn, drawable, self.gc, x as i16, y as i16)?;
        Ok(())
    }

    pub(crate) fn draw_xor_rects(&self, drawable: Drawable, rects: &[Rectangle]) -> AnyResult<()> {
        if rects.is_empty() {
            return Ok(());
        }
        self.conn.change_gc(
            self.gc,
            &ChangeGCAux::new()
                .function(GX::XOR)
                .foreground(0x00af_e5f5)
                .line_width(2),
        )?;
        self.conn.poly_fill_rectangle(drawable, self.gc, rects)?;
        self.conn.change_gc(
            self.gc,
            &ChangeGCAux::new()
                .function(GX::COPY)
                .foreground(0)
                .line_width(1),
        )?;
        Ok(())
    }

    pub(crate) fn atom(&self, name: &[u8]) -> AnyResult<Atom> {
        Ok(self.conn.intern_atom(false, name)?.reply()?.atom)
    }

    pub(crate) fn paint_window_icon(
        &self,
        canvas: &mut Canvas,
        window: Window,
        x: i32,
        y: i32,
        size: i32,
    ) -> bool {
        let Ok(cookie) = self.conn.intern_atom(false, b"_NET_WM_ICON") else {
            return false;
        };
        let Ok(atom) = cookie.reply() else {
            return false;
        };
        let Ok(cookie) =
            self.conn
                .get_property(false, window, atom.atom, AtomEnum::CARDINAL, 0, 262_144)
        else {
            return false;
        };
        let Ok(reply) = cookie.reply() else {
            return false;
        };
        let Some(values) = reply.value32() else {
            return false;
        };
        let data = values.collect::<Vec<_>>();
        let mut pos = 0usize;
        let mut best: Option<(usize, usize, usize)> = None;
        while pos + 2 <= data.len() {
            let w = data[pos] as usize;
            let h = data[pos + 1] as usize;
            pos += 2;
            let count = w.saturating_mul(h);
            if w == 0 || h == 0 || pos + count > data.len() {
                break;
            }
            let score = (w as i32 - size).abs() + (h as i32 - size).abs();
            let replace = best
                .map(|(_, bw, bh)| score < (bw as i32 - size).abs() + (bh as i32 - size).abs())
                .unwrap_or(true);
            if replace {
                best = Some((pos, w, h));
            }
            pos += count;
        }
        let Some((start, w, h)) = best else {
            return false;
        };
        for yy in 0..size {
            for xx in 0..size {
                let sx = (xx as usize * w / size as usize).min(w - 1);
                let sy = (yy as usize * h / size as usize).min(h - 1);
                let argb = data[start + sy * w + sx];
                let a = ((argb >> 24) & 0xff) as u8;
                if a == 0 {
                    continue;
                }
                let r = ((argb >> 16) & 0xff) as u8;
                let g = ((argb >> 8) & 0xff) as u8;
                let b = (argb & 0xff) as u8;
                canvas.blend_pixel(x + xx, y + yy, Color::rgba(r, g, b, a));
            }
        }
        true
    }

    pub(crate) fn paint_desktop_icon(
        &mut self,
        canvas: &mut Canvas,
        window: Window,
        x: i32,
        y: i32,
        size: i32,
    ) -> bool {
        let class = self.window_class(window);
        let title = self.window_title(window).to_ascii_lowercase();
        let key = format!("{class}|{title}");
        if !self.icon_cache.contains_key(&key) {
            let icon = resolve_window_icon(&class, &title)
                .and_then(|path| fs::read(path).ok())
                .and_then(|bytes| decode_icon_pixels(&bytes, size).ok());
            self.icon_cache.insert(key.clone(), icon);
        }
        let Some(Some(pixels)) = self.icon_cache.get(&key) else {
            return false;
        };
        paint_rgba_pixels(canvas, pixels, x, y, size, size);
        true
    }

    pub(crate) fn dock_geometry(&self) -> (i16, i16, u16, u16) {
        let buttons = self.dock_button_count().max(5);
        let width = (buttons as i32 * DOCK_STRIDE - (DOCK_STRIDE - DOCK_ICON_SIZE))
            .max(DOCK_ICON_SIZE) as u16;
        let width = width.min(self.screen_width);
        let x = ((self.screen_width.saturating_sub(width)) / 2) as i16;
        let y = self
            .screen_height
            .saturating_sub(DOCK_HEIGHT + DOCK_BOTTOM_MARGIN) as i16;
        (x, y, width, DOCK_HEIGHT)
    }

    pub(crate) fn settings_geometry(&self) -> (i16, i16, u16, u16) {
        let width = SETTINGS_TARGET_WIDTH
            .min(self.screen_width.saturating_sub(SETTINGS_MARGIN * 2))
            .max(SETTINGS_MIN_WIDTH.min(self.screen_width));
        let height = 578u16
            .min(
                self.screen_height
                    .saturating_sub(TOPBAR_HEIGHT + SETTINGS_MARGIN * 2),
            )
            .max(440.min(self.screen_height));
        let x = self.screen_width.saturating_sub(width + SETTINGS_MARGIN) as i16;
        let y = (TOPBAR_HEIGHT + SETTINGS_MARGIN) as i16;
        (x, y, width, height)
    }

    pub(crate) fn folder_geometry(&self) -> (i16, i16, u16, u16) {
        let width = self
            .folder_width
            .min(self.screen_width.saturating_sub(48))
            .max(FOLDER_MIN_WIDTH.min(self.screen_width));
        let height = self
            .folder_height
            .min(
                self.screen_height
                    .saturating_sub(TOPBAR_HEIGHT + DOCK_HEIGHT + 48),
            )
            .max(FOLDER_MIN_HEIGHT.min(self.screen_height));
        (24, (TOPBAR_HEIGHT + 26) as i16, width, height)
    }

    pub(crate) fn folder_terminal_geometry(&self) -> (i16, i16, u16, u16) {
        let folder = self.folder_geometry();
        let y = i32::from(folder.1) + i32::from(folder.3) + 8;
        let available = i32::from(self.screen_height).saturating_sub(y + 50);
        let width = self
            .folder_terminal_width
            .min(self.screen_width.saturating_sub(48))
            .max(TERMINAL_MIN_WIDTH.min(self.screen_width));
        let height = self
            .folder_terminal_height
            .min(available.max(i32::from(TERMINAL_MIN_HEIGHT)) as u16)
            .max(TERMINAL_MIN_HEIGHT.min(self.screen_height));
        (folder.0, y as i16, width, height)
    }

    pub(crate) fn ui_bottom_right_resize_hit(&self, target: UiResizeTarget, x: i16, y: i16) -> bool {
        let (_, _, width, height) = match target {
            UiResizeTarget::Folder => self.folder_geometry(),
            UiResizeTarget::FolderTerminal => self.folder_terminal_geometry(),
        };
        let width = i16::try_from(width).unwrap_or(i16::MAX);
        let height = i16::try_from(height).unwrap_or(i16::MAX);
        x >= width - RESIZE_CORNER && y >= height - RESIZE_CORNER
    }

    pub(crate) fn app_menu_geometry(&self) -> (i16, i16, u16, u16) {
        let width = if self.app_menu_more { 700u16 } else { 420u16 }
            .min(self.screen_width.saturating_sub(36));
        let height = if self.app_menu_more { 540u16 } else { 350u16 }
            .min(self.screen_height.saturating_sub(TOPBAR_HEIGHT + DOCK_HEIGHT + 24));
        let dock = self.dock_geometry();
        let x = dock.0.max(18);
        let y = dock.1.saturating_sub(height as i16 + 10);
        (x, y, width, height)
    }

    pub(crate) fn aurora_menu_geometry(&self) -> (i16, i16, u16, u16) {
        let width = 390u16.min(self.screen_width.saturating_sub(24)).max(260);
        let height = if self.aurora_menu_about {
            276
        } else if self.aurora_menu_restart_confirm {
            220
        } else {
            168
        };
        (12, TOPBAR_HEIGHT as i16 + 8, width, height)
    }

    pub(crate) fn clipboard_menu_geometry(&self) -> (i16, i16, u16, u16) {
        let width = CLIPBOARD_MENU_WIDTH
            .min(self.screen_width.saturating_sub(24))
            .max(260.min(self.screen_width));
        let height = if self.clipboard_history.is_empty() {
            150
        } else {
            (56 + self.clipboard_page_content_height() + 10) as u16
        };
        let controls = self.topbar_controls();
        let mut x = controls.clipboard_x - i32::from(width) / 2;
        let max_x = i32::from(self.screen_width.saturating_sub(width)).saturating_sub(12);
        x = x.max(12).min(max_x.max(12));
        (x as i16, TOPBAR_HEIGHT as i16 + 4, width, height)
    }

    pub(crate) fn media_geometry(&self, slot: usize) -> (i16, i16, u16, u16) {
        let folder = self.folder_geometry();
        let width = MEDIA_WIDTH
            .min(self.screen_width.saturating_sub(48))
            .max(320.min(self.screen_width));
        let height = folder
            .3
            .min(self.screen_height.saturating_sub(TOPBAR_HEIGHT + 56))
            .max(300.min(self.screen_height));
        let desired_x = i32::from(folder.0) + i32::from(folder.2);
        let max_x = i32::from(self.screen_width.saturating_sub(width));
        let x = desired_x.min(max_x).max(0) as i16;
        let y = i32::from(folder.1) + (slot.min(4) as i32 * 10);
        (x, y as i16, width, height)
    }

    pub(crate) fn dock_button_count(&self) -> usize {
        let task_windows = self.task_client_windows();
        if task_windows.len() <= 10 {
            5 + task_windows.len()
        } else {
            5 + 10 + 1
        }
    }

    pub(crate) fn task_client_windows(&self) -> Vec<Window> {
        let mut windows = self
            .clients
            .iter()
            .filter_map(|(window, info)| {
                self.client_on_active_workspace(info).then_some(*window)
            })
            .collect::<Vec<_>>();
        windows.sort_unstable();
        windows
    }

    pub(crate) fn dock_more_menu_geometry(&self) -> (i16, i16, u16, u16) {
        let (dx, dy, dw, _dh) = self.dock_geometry();
        let mut icon_x = 15 * DOCK_STRIDE;
        icon_x = icon_x.min(i32::from(dw).saturating_sub(DOCK_ICON_SIZE));
        let center_x = dx + icon_x as i16 + (DOCK_ICON_SIZE / 2) as i16;

        let task_windows = self.task_client_windows();
        let hidden_count = task_windows.len().saturating_sub(10);
        let width = 240u16;
        let height = (hidden_count as u16 * 40 + 16).max(40);

        let mut x = center_x - (width as i16 / 2);
        if x + width as i16 > self.screen_width as i16 - 12 {
            x = self.screen_width as i16 - width as i16 - 12;
        }
        if x < 12 {
            x = 12;
        }
        let y = dy - height as i16 - 8;
        (x, y, width, height)
    }

    pub(crate) fn client_key_for(&self, window: Window) -> Option<Window> {
        if self.clients.contains_key(&window) {
            return Some(window);
        }
        self.clients
            .iter()
            .find_map(|(client, info)| (info.frame == window).then_some(*client))
    }

    pub(crate) fn client_or_ancestor_key_for(&self, window: Window) -> Option<Window> {
        if let Some(client) = self.client_key_for(window) {
            return Some(client);
        }
        let mut current = window;
        for _ in 0..8 {
            let Ok(cookie) = self.conn.query_tree(current) else {
                return None;
            };
            let Ok(reply) = cookie.reply() else {
                return None;
            };
            if reply.parent == self.root || reply.parent == x11rb::NONE {
                return self.client_key_for(reply.parent);
            }
            if let Some(client) = self.client_key_for(reply.parent) {
                return Some(client);
            }
            current = reply.parent;
        }
        None
    }

    pub(crate) fn is_ui_window(&self, window: Window) -> bool {
        window == self.ui.topbar
            || window == self.ui.dock
            || window == self.ui.settings
            || window == self.ui.folder
            || window == self.ui.folder_terminal
            || window == self.ui.screenshot_overlay
            || window == self.ui.app_menu
            || window == self.ui.aurora_menu
            || window == self.ui.clipboard_menu
            || window == self.ui.dock_more_menu
            || window == self.ui.title_menu
            || window == self.ui.confirm_dialog
            || window == self.ui.tooltip
            || self.ui.media.contains(&window)
    }

    pub(crate) fn media_slot_for_window(&self, window: Window) -> Option<usize> {
        self.ui.media.iter().position(|&media| media == window)
    }
}
