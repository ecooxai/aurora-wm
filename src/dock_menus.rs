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
    pub(crate) fn handle_dock_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        let (_, _, _, h) = self.dock_geometry();
        let buttons = self.dock_button_count();
        let task_windows = self.task_client_windows();
        let cy = i32::from(h) / 2;
        for i in 0..buttons {
            let rx = i as i32 * DOCK_STRIDE;
            let ry = cy - DOCK_ICON_SIZE / 2;
            if x >= rx
                && x < rx + DOCK_ICON_SIZE
                && y >= ry
                && y < ry + DOCK_ICON_SIZE
            {
                if i == 0 {
                    self.dock_last_click = None;
                    self.hide_dock_more_menu()?;
                    self.toggle_app_menu()?;
                } else if i >= 5 {
                    self.hide_app_menu()?;
                    if i == 15 && task_windows.len() > 10 {
                        if self.dock_more_visible {
                            self.hide_dock_more_menu()?;
                        } else {
                            self.show_dock_more_menu()?;
                        }
                    } else if let Some(client) = task_windows.get(i - 5).copied() {
                        self.hide_dock_more_menu()?;
                        self.handle_task_icon_click(client)?;
                    }
                } else {
                    self.dock_last_click = None;
                    self.hide_app_menu()?;
                    self.hide_dock_more_menu()?;
                    if i == 1 {
                        if !self.open_file_manager_tab(&folder_path_for(FolderMode::Pictures)) {
                            self.show_folder(FolderMode::Pictures, true)?;
                        }
                    } else if i == 2 {
                        if !self.open_file_manager_tab(&folder_path_for(FolderMode::Music)) {
                            self.show_folder(FolderMode::Music, true)?;
                        }
                    } else if i == 3 {
                        if !self.open_file_manager_tab(&folder_path_for(FolderMode::Videos)) {
                            self.show_folder(FolderMode::Videos, true)?;
                        }
                    } else if i == 4 {
                        self.settings_visible = !self.settings_visible;
                        if self.settings_visible {
                            self.settings_front = true;
                            self.settings_hidden_at = None;
                            self.folder_front = false;
                            self.media_front = false;
                            self.conn.map_window(self.ui.settings)?;
                            self.raise_ui()?;
                            self.request_settings_data(self.settings.tab);
                            self.redraw_settings()?;
                            self.redraw_topbar()?;
                        } else {
                            self.settings_hidden_at = Some(Instant::now());
                            self.conn.unmap_window(self.ui.settings)?;
                            self.redraw_topbar()?;
                        }
                    }
                }
                return Ok(());
            }
        }
        self.hide_dock_more_menu()?;
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
        if task_windows.len() > 10 {
            let hidden_apps = &task_windows[10..];
            for (idx, &window) in hidden_apps.iter().enumerate() {
                let row_y = 8 + idx as i32 * 40;
                let active = self.active_client == Some(window);
                c.draw_round_rect(
                    8,
                    row_y,
                    i32::from(w) - 16,
                    32,
                    8,
                    if active {
                        Color::rgba(28, 67, 111, 225)
                    } else {
                        Color::rgba(255, 255, 255, 120)
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
                c.draw_text(&self.bold, &display_title, text_x, text_y, 12.0, INK);
            }
        }

        self.upload_canvas(self.ui.dock_more_menu, &c)?;
        Ok(())
    }

    pub(crate) fn show_dock_more_menu(&mut self) -> AnyResult<()> {
        self.dock_more_visible = true;
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

    pub(crate) fn handle_dock_more_menu_click(&mut self, _x: i32, y: i32) -> AnyResult<()> {
        let task_windows = self.task_client_windows();
        if task_windows.len() > 10 {
            let hidden_apps = &task_windows[10..];
            let idx = (y - 8) / 40;
            if idx >= 0 && idx < hidden_apps.len() as i32 {
                let client = hidden_apps[idx as usize];
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
        process::exit(0);
    }

}
