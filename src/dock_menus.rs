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
    pub(crate) fn launch_files_from_dock(&mut self) -> AnyResult<()> {
        self.hide_app_menu()?;
        self.hide_dock_more_menu()?;
        if !self.open_file_manager_tab(&home_dir()) {
            self.show_folder(FolderMode::Home, true)?;
        }
        Ok(())
    }

    pub(crate) fn handle_topbar_task_click(&mut self, x: i32) -> AnyResult<bool> {
        let relative = x - self.topbar_tasks_x();
        if relative < 0 { return Ok(false); }
        let slot = (relative / TOPBAR_TASK_STRIDE) as usize;
        if slot >= self.dock_button_count() || relative % TOPBAR_TASK_STRIDE >= TOPBAR_TASK_SIZE { return Ok(false); }
        if slot == 0 { self.launch_files_from_dock()?; }
        else { self.handle_dock_task_slot(slot - 1)?; }
        Ok(true)
    }

    pub(crate) fn handle_dock_task_slot(&mut self, slot: usize) -> AnyResult<()> {
        self.hide_app_menu()?;
        let tasks = self.task_client_windows();
        if slot == self.dock_task_limit() && tasks.len() > slot {
            if self.dock_more_visible { self.hide_dock_more_menu()?; }
            else { self.show_dock_more_menu()?; }
        } else if let Some(&window) = tasks.get(slot) {
            self.hide_dock_more_menu()?;
            self.handle_task_icon_click(window)?;
        }
        Ok(())
    }

    pub(crate) fn handle_dock_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        if x < 0 || y < 0 || y >= DOCK_ICON_SIZE { return Ok(()); }
        let slot = (x / DOCK_STRIDE) as usize;
        if slot >= self.dock_button_count() || x % DOCK_STRIDE >= DOCK_ICON_SIZE { return Ok(()); }
        match slot {
            0 => { self.dock_last_click = None; self.hide_dock_more_menu()?; self.toggle_app_menu()?; }
            1 => { self.dock_last_click = None; self.launch_files_from_dock()?; }
            2 => {
                self.dock_last_click = None;
                self.hide_app_menu()?;
                self.hide_dock_more_menu()?;
                if self.settings_visible {
                    self.settings_visible = false;
                    self.settings_hidden_at = Some(Instant::now());
                    self.conn.unmap_window(self.ui.settings)?;
                    self.redraw_topbar()?;
                } else { self.open_settings_tab(self.settings.tab)?; }
            }
            _ => self.handle_dock_task_slot(slot - self.dock_pinned_count())?,
        }
        Ok(())
    }

    pub(crate) fn handle_task_icon_click(&mut self, client: Window) -> AnyResult<()> {
        let now = Instant::now();
        let double_click = self.dock_last_click.is_some_and(|last| {
            last.client == client && now.duration_since(last.at) <= Duration::from_millis(360)
        });
        self.dock_last_click = Some(DockClickState { client, at: now });
        if double_click {
            self.snap_client_top_center(client)?;
        } else {
            self.focus_window(client)?;
        }
        Ok(())
    }

    pub(crate) fn snap_client_top_center(&mut self, client: Window) -> AnyResult<()> {
        let Some(mut info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        info.x = ((self.screen_width.saturating_sub(info.width)) / 2) as i16;
        info.y = (TOPBAR_HEIGHT + 2) as i16;
        self.conn.configure_window(
            info.frame,
            &ConfigureWindowAux::new()
                .x(i32::from(info.x))
                .y(i32::from(info.y))
                .stack_mode(StackMode::ABOVE),
        )?;
        self.clients.insert(client, info);
        self.send_synthetic_configure(&info)?;
        self.focus_window(client)?;
        Ok(())
    }

    pub(crate) fn toggle_app_menu(&mut self) -> AnyResult<()> {
        self.app_menu_visible = !self.app_menu_visible;
        if self.app_menu_visible {
            self.hide_dock_more_menu()?;
            self.app_menu_more = false;
            self.app_menu_scroll = 0;
            self.app_menu_query.clear();
            self.app_menu_expanded_categories.clear();
            let menu = self.app_menu_geometry();
            self.conn.configure_window(
                self.ui.app_menu,
                &ConfigureWindowAux::new()
                    .x(i32::from(menu.0))
                    .y(i32::from(menu.1))
                    .width(u32::from(menu.2))
                    .height(u32::from(menu.3))
                    .stack_mode(StackMode::ABOVE),
            )?;
            self.conn.map_window(self.ui.app_menu)?;
            let _ = self
                .conn
                .grab_keyboard(
                    false,
                    self.ui.app_menu,
                    CURRENT_TIME,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )?
                .reply();
            self.conn
                .set_input_focus(InputFocus::POINTER_ROOT, self.ui.app_menu, CURRENT_TIME)?;
            self.redraw_app_menu()?;
        } else {
            self.conn.ungrab_keyboard(CURRENT_TIME)?;
            self.conn.unmap_window(self.ui.app_menu)?;
            self.conn
                .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
        }
        self.raise_ui()?;
        Ok(())
    }

    pub(crate) fn hide_app_menu(&mut self) -> AnyResult<()> {
        if self.app_menu_visible {
            self.app_menu_visible = false;
            self.app_menu_more = false;
            self.app_menu_scroll = 0;
            self.app_menu_query.clear();
            self.app_menu_expanded_categories.clear();
            self.conn.ungrab_keyboard(CURRENT_TIME)?;
            self.conn.unmap_window(self.ui.app_menu)?;
            self.conn
                .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
        }
        Ok(())
    }

    pub(crate) fn redraw_dock_more_menu(&mut self) -> AnyResult<()> {
        let (x, y, w, h) = self.dock_more_menu_geometry();
        self.conn.configure_window(
            self.ui.dock_more_menu,
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
            16,
            Color::rgba(248, 253, 255, 232),
        );
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            16,
            Color::rgba(214, 229, 237, 70),
        );

        let task_windows = self.task_client_windows();
        if task_windows.len() > self.dock_task_limit() {
            let hidden_apps = &task_windows[self.dock_task_limit()..];
            let visible = self.dock_more_visible_rows();
            self.dock_more_scroll = self.dock_more_scroll.min(hidden_apps.len().saturating_sub(visible));
            for (idx, &window) in hidden_apps.iter().skip(self.dock_more_scroll).take(visible).enumerate() {
                let row_y = 8 + idx as i32 * 40;
                let active = self.active_client == Some(window);
                c.draw_round_rect(
                    8,
                    row_y,
                    i32::from(w) - 16,
                    32,
                    8,
                    if active {
                        Color::rgb(255,255,255)
                    } else {
                        Color::rgba(27,38,49,245)
                    },
                );

                let icon_x = 16;
                let icon_y = row_y + 2;
                let title = self.window_title(window);
                if !self.paint_window_icon(&mut c, window, icon_x, icon_y, 28)
                    && !self.paint_desktop_icon(&mut c, window, icon_x, icon_y, 28)
                {
                    let mapped = self
                        .clients
                        .get(&window)
                        .map(|info| info.mapped)
                        .unwrap_or(true);
                    draw_client_task_icon(
                        &mut c,
                        &self.bold,
                        icon_x + 14,
                        icon_y + 14,
                        mapped,
                        &title,
                    );
                }

                let text_x = 52;
                let text_y = row_y + 8;
                let display_title = compact(&title, 20);
                c.draw_text(&self.bold, &display_title, text_x, text_y, 12.0, if active { INK } else { MINT_LIGHT });
            }
        }

        let hidden = task_windows.len().saturating_sub(self.dock_task_limit());
        let visible = self.dock_more_visible_rows();
        if hidden > visible {
            let track = i32::from(h).saturating_sub(16).max(1);
            let thumb = (track * visible as i32 / hidden as i32).max(18).min(track);
            let offset = (track - thumb) * self.dock_more_scroll as i32 / (hidden - visible) as i32;
            c.draw_round_rect(i32::from(w) - 6, 8, 3, track, 2, Color::rgba(77,99,111,70));
            c.draw_round_rect(i32::from(w) - 6, 8 + offset, 3, thumb, 2, MINT_DARK);
        }

        self.upload_canvas(self.ui.dock_more_menu, &c)?;
        Ok(())
    }

    pub(crate) fn show_dock_more_menu(&mut self) -> AnyResult<()> {
        self.dock_more_visible = true;
        self.dock_more_scroll = 0;
        let menu = self.dock_more_menu_geometry();
        self.conn.configure_window(
            self.ui.dock_more_menu,
            &ConfigureWindowAux::new()
                .x(i32::from(menu.0))
                .y(i32::from(menu.1))
                .width(u32::from(menu.2))
                .height(u32::from(menu.3))
                .stack_mode(StackMode::ABOVE),
        )?;
        self.conn.map_window(self.ui.dock_more_menu)?;
        self.redraw_dock_more_menu()?;
        self.redraw_dock()?;
        Ok(())
    }

    pub(crate) fn hide_dock_more_menu(&mut self) -> AnyResult<()> {
        if self.dock_more_visible {
            self.dock_more_visible = false;
            self.conn.unmap_window(self.ui.dock_more_menu)?;
            self.redraw_dock()?;
        }
        Ok(())
    }

    pub(crate) fn handle_dock_more_menu_press(&mut self, button: u8, x: i32, y: i32) -> AnyResult<()> {
        let tasks = self.task_client_windows();
        let limit = self.dock_task_limit();
        let hidden = tasks.len().saturating_sub(limit);
        let visible = self.dock_more_visible_rows();
        let max_scroll = hidden.saturating_sub(visible);
        self.dock_more_scroll = self.dock_more_scroll.min(max_scroll);
        if matches!(button, 4 | 5) {
            self.dock_more_scroll = if button == 4 { self.dock_more_scroll.saturating_sub(3) }
                else { self.dock_more_scroll.saturating_add(3).min(max_scroll) };
            self.redraw_dock_more_menu()?;
        } else if button == 1 && x >= 8 && y >= 8 && (y - 8) / 40 < visible as i32 {
            let index = limit + self.dock_more_scroll + ((y - 8) / 40) as usize;
            if let Some(&client) = tasks.get(index) {
                self.handle_task_icon_click(client)?;
                self.hide_dock_more_menu()?;
            }
        }
        Ok(())
    }

    pub(crate) fn toggle_aurora_menu(&mut self) -> AnyResult<()> {
        self.aurora_menu_visible = !self.aurora_menu_visible;
        if self.aurora_menu_visible {
            self.hide_dock_more_menu()?;
            self.app_menu_visible = false;
            self.app_menu_more = false;
            self.app_menu_scroll = 0;
            self.app_menu_query.clear();
            self.app_menu_expanded_categories.clear();
            let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
            let _ = self.conn.unmap_window(self.ui.app_menu);
            self.aurora_menu_about = false;
            self.aurora_menu_restart_confirm = false;
            let menu = self.aurora_menu_geometry();
            self.conn.configure_window(
                self.ui.aurora_menu,
                &ConfigureWindowAux::new()
                    .x(i32::from(menu.0))
                    .y(i32::from(menu.1))
                    .width(u32::from(menu.2))
                    .height(u32::from(menu.3))
                    .stack_mode(StackMode::ABOVE),
            )?;
            self.conn.map_window(self.ui.aurora_menu)?;
            self.redraw_aurora_menu()?;
        } else {
            self.conn.unmap_window(self.ui.aurora_menu)?;
        }
        self.raise_ui()?;
        Ok(())
    }

    pub(crate) fn hide_aurora_menu(&mut self) -> AnyResult<()> {
        if self.aurora_menu_visible {
            self.aurora_menu_visible = false;
            self.aurora_menu_about = false;
            self.aurora_menu_restart_confirm = false;
            self.conn.unmap_window(self.ui.aurora_menu)?;
        }
        Ok(())
    }

    pub(crate) fn handle_aurora_menu_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        if self.aurora_menu_about {
            if (16..=92).contains(&x) && (230..=258).contains(&y) {
                self.aurora_menu_about = false;
                self.redraw_aurora_menu()?;
            }
            return Ok(());
        }

        if self.aurora_menu_restart_confirm {
            if (110..=138).contains(&y) {
                if (160..=250).contains(&x) {
                    self.restart_aurora()?;
                } else if (270..=360).contains(&x) {
                    self.aurora_menu_restart_confirm = false;
                    self.redraw_aurora_menu()?;
                }
            } else if (158..=196).contains(&y) {
                self.aurora_menu_about = true;
                self.aurora_menu_restart_confirm = false;
                self.redraw_aurora_menu()?;
            }
        } else {
            if (56..=94).contains(&y) {
                self.aurora_menu_restart_confirm = true;
                self.redraw_aurora_menu()?;
            } else if (106..=144).contains(&y) {
                self.aurora_menu_about = true;
                self.redraw_aurora_menu()?;
            }
        }
        Ok(())
    }

    pub(crate) fn restart_aurora(&mut self) -> AnyResult<()> {
        save_app_commands(&self.settings)?;
        let exe = env::current_exe()?;
        let display = self.display.clone();
        let display_id = display.trim_start_matches(':').replace(['/', '.'], "_");
        let log_path = format!("/tmp/aurora-wm-display{display_id}.log");
        let script = format!(
            "sleep 0.35; exec {} > {} 2>&1",
            shell_quote(&exe),
            shell_quote_text(&log_path),
        );
        Command::new("setsid")
            .arg("sh")
            .arg("-c")
            .arg(script)
            .env("DISPLAY", display)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        self.shutdown_screen_recording();
        process::exit(0);
    }

}
