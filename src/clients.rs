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
use crate::layout::*;
use crate::draw_helpers::*;
use crate::pixels::*;
use crate::system::*;
use crate::textutil::*;
use crate::procutil::*;
use crate::files::*;

impl Aurora {
    pub(crate) fn configure_managed_client(
        &self,
        info: &ClientInfo,
        stack: Option<StackMode>,
    ) -> AnyResult<()> {
        let title_h = self.titlebar_height(info);
        let mut frame_aux = ConfigureWindowAux::new()
            .x(i32::from(info.x))
            .y(i32::from(info.y))
            .width(u32::from(info.width))
            .height(u32::from(info.height + title_h));
        if let Some(stack) = stack {
            frame_aux = frame_aux.stack_mode(stack);
        }
        self.conn.configure_window(info.frame, &frame_aux)?;
        self.conn.configure_window(
            info.window,
            &ConfigureWindowAux::new()
                .x(0)
                .y(i32::from(title_h))
                .width(u32::from(info.width))
                .height(u32::from(info.height))
                .border_width(0),
        )?;
        self.apply_frame_shape(info)?;
        self.send_synthetic_configure(info)
    }

    pub(crate) fn send_synthetic_configure(&self, info: &ClientInfo) -> AnyResult<()> {
        let title_h = self.titlebar_height(info);
        let client_y = (i32::from(info.y) + i32::from(title_h))
            .clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        let event = ConfigureNotifyEvent {
            response_type: CONFIGURE_NOTIFY_EVENT,
            sequence: 0,
            event: info.window,
            window: info.window,
            above_sibling: x11rb::NONE,
            x: info.x,
            y: client_y,
            width: info.width,
            height: info.height,
            border_width: 0,
            override_redirect: false,
        };
        self.conn
            .send_event(false, info.window, EventMask::STRUCTURE_NOTIFY, event)?;
        Ok(())
    }

    pub(crate) fn ffplay_geometry(&self) -> (i16, i16, u16, u16) {
        let folder = self.folder_geometry();
        let preferred_x = i32::from(folder.0) + i32::from(folder.2) + 8;
        let preferred_w = (self.screen_width / 2).max(300.min(self.screen_width));
        let max_w_at_preferred = i32::from(self.screen_width)
            .saturating_sub(preferred_x + 16)
            .max(240) as u16;
        let width = preferred_w.min(max_w_at_preferred);
        let x = preferred_x
            .min(i32::from(self.screen_width.saturating_sub(width + 16)))
            .max(16) as i16;
        let height = folder
            .3
            .min(
                self.screen_height
                    .saturating_sub(TOPBAR_HEIGHT + DOCK_HEIGHT + 48),
            )
            .max(240.min(self.screen_height));
        (x, folder.1, width, height)
    }

    pub(crate) fn schedule_window_nudge(&mut self, client: Window, width: u16, height: u16) {
        self.pending_window_nudges
            .retain(|pending| pending.client != client);
        self.pending_window_nudges.push(PendingWindowNudge {
            client,
            base_width: width,
            base_height: height,
            step: 0,
            at: Instant::now() + Duration::from_millis(850),
        });
    }

    pub(crate) fn process_pending_window_nudges(&mut self) -> AnyResult<bool> {
        let now = Instant::now();
        let mut idx = 0;
        let mut changed = false;
        while idx < self.pending_window_nudges.len() {
            if now < self.pending_window_nudges[idx].at {
                idx += 1;
                continue;
            }
            let mut pending = self.pending_window_nudges[idx];
            let Some(mut info) = self.clients.get(&pending.client).copied() else {
                self.pending_window_nudges.swap_remove(idx);
                continue;
            };
            info.width = if pending.step == 0 {
                pending.base_width.saturating_add(MEDIA_WINDOW_NUDGE_WIDTH)
            } else {
                pending.base_width
            };
            info.height = pending.base_height;
            self.configure_managed_client(&info, Some(StackMode::ABOVE))?;
            self.clients.insert(pending.client, info);
            self.redraw_frame_titlebar(pending.client)?;
            changed = true;

            if pending.step == 0 {
                pending.step = 1;
                pending.at = now + Duration::from_millis(90);
                self.pending_window_nudges[idx] = pending;
                idx += 1;
            } else {
                self.pending_window_nudges.swap_remove(idx);
            }
        }
        Ok(changed)
    }

    pub(crate) fn start_drag(&mut self, client: Window, root_x: i16, root_y: i16) -> AnyResult<()> {
        let Some(info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        self.drag = Some(DragState {
            client,
            offset_x: root_x.saturating_sub(info.x),
            offset_y: root_y.saturating_sub(info.y),
            start_root_x: root_x,
            start_root_y: root_y,
            start_x: info.x,
            start_y: info.y,
            start_w: info.width,
            start_h: info.height,
            kind: DragKind::Move,
            resize_edges: ResizeEdges::default(),
            last_update_at: Instant::now() - Duration::from_millis(16),
        });
        self.settings_front = false;
        self.folder_front = false;
        self.media_front = false;
        self.focus_window(client)?;
        if let Ok(cookie) = self.conn.grab_pointer(
            false,
            self.root,
            EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            x11rb::NONE,
            self.cursor,
            CURRENT_TIME,
        ) {
            let _ = cookie.reply();
        }
        Ok(())
    }

    pub(crate) fn start_resize(
        &mut self,
        client: Window,
        root_x: i16,
        root_y: i16,
        resize_edges: ResizeEdges,
    ) -> AnyResult<()> {
        let Some(info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        self.drag = Some(DragState {
            client,
            offset_x: 0,
            offset_y: 0,
            start_root_x: root_x,
            start_root_y: root_y,
            start_x: info.x,
            start_y: info.y,
            start_w: info.width,
            start_h: info.height,
            kind: DragKind::Resize,
            resize_edges,
            last_update_at: Instant::now() - Duration::from_millis(33),
        });
        self.focus_window(client)?;
        let _ = self
            .conn
            .grab_pointer(
                false,
                self.root,
                EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                x11rb::NONE,
                self.cursor,
                CURRENT_TIME,
            )?
            .reply();
        Ok(())
    }

    pub(crate) fn start_ui_resize(&mut self, pending: PendingUiResize) -> AnyResult<()> {
        let (start_w, start_h) = match pending.target {
            UiResizeTarget::Folder => (self.folder_width, self.folder_height),
            UiResizeTarget::FolderTerminal => {
                (self.folder_terminal_width, self.folder_terminal_height)
            }
        };
        self.ui_resize = Some(UiResizeState {
            target: pending.target,
            start_root_x: pending.root_x,
            start_root_y: pending.root_y,
            start_w,
            start_h,
        });
        let _ = self
            .conn
            .grab_pointer(
                false,
                self.root,
                EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                x11rb::NONE,
                self.cursor,
                CURRENT_TIME,
            )?
            .reply();
        Ok(())
    }

    pub(crate) fn update_ui_resize(&mut self, root_x: i16, root_y: i16) -> AnyResult<bool> {
        let Some(resize) = self.ui_resize else {
            return Ok(false);
        };
        let dx = i32::from(root_x) - i32::from(resize.start_root_x);
        let dy = i32::from(root_y) - i32::from(resize.start_root_y);
        let old_folder = (self.folder_width, self.folder_height);
        let old_terminal = (self.folder_terminal_width, self.folder_terminal_height);
        match resize.target {
            UiResizeTarget::Folder => {
                let max_w = self.screen_width.saturating_sub(48).max(FOLDER_MIN_WIDTH);
                let max_h = self
                    .screen_height
                    .saturating_sub(TOPBAR_HEIGHT + DOCK_HEIGHT + 48)
                    .max(FOLDER_MIN_HEIGHT);
                self.folder_width = (i32::from(resize.start_w) + dx)
                    .clamp(FOLDER_MIN_WIDTH.into(), max_w.into())
                    as u16;
                self.folder_height = (i32::from(resize.start_h) + dy)
                    .clamp(FOLDER_MIN_HEIGHT.into(), max_h.into())
                    as u16;
                self.folder_terminal_width = self.folder_terminal_width.min(self.folder_width);
            }
            UiResizeTarget::FolderTerminal => {
                let folder = self.folder_geometry();
                let y = i32::from(folder.1) + i32::from(folder.3) + 8;
                let max_h = i32::from(self.screen_height)
                    .saturating_sub(y + 50)
                    .max(i32::from(TERMINAL_MIN_HEIGHT)) as u16;
                let max_w = self.screen_width.saturating_sub(48).max(TERMINAL_MIN_WIDTH);
                self.folder_terminal_width = (i32::from(resize.start_w) + dx)
                    .clamp(TERMINAL_MIN_WIDTH.into(), max_w.into())
                    as u16;
                self.folder_terminal_height = (i32::from(resize.start_h) + dy)
                    .clamp(TERMINAL_MIN_HEIGHT.into(), max_h.into())
                    as u16;
            }
        }
        if old_folder == (self.folder_width, self.folder_height)
            && old_terminal == (self.folder_terminal_width, self.folder_terminal_height)
        {
            return Ok(false);
        }
        let folder = self.folder_geometry();
        let terminal = self.folder_terminal_geometry();
        self.conn.configure_window(
            self.ui.folder,
            &ConfigureWindowAux::new()
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
        self.redraw_folder()?;
        if self.folder_terminal.visible {
            self.redraw_folder_terminal()?;
        }
        Ok(true)
    }

    pub(crate) fn end_drag(&mut self) -> AnyResult<()> {
        self.pending_resize = None;
        self.pending_ui_resize = None;
        self.ui_resize = None;
        if let Some(drag) = self.drag.take() {
            self.conn.ungrab_pointer(CURRENT_TIME)?;
            if matches!(drag.kind, DragKind::Move) {
                if let Some(info) = self.clients.get(&drag.client).copied() {
                    if !self.compositor_active {
                        let frame_h = info.height + self.titlebar_height(&info);
                        self.clear_root_region(
                            i32::from(info.x),
                            i32::from(info.y),
                            u32::from(info.width),
                            u32::from(frame_h),
                        )?;
                    }
                    self.redraw_frame_titlebar(drag.client)?;
                }
            }
        } else {
            let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        }
        Ok(())
    }

    pub(crate) fn handle_configure_request(&mut self, ev: ConfigureRequestEvent) -> AnyResult<()> {
        if self.is_ui_window(ev.window) {
            return Ok(());
        }
        if let Some(client) = self.client_key_for(ev.window) {
            let Some(mut info) = self.clients.get(&client).copied() else {
                return Ok(());
            };
            if mask_has(ev.value_mask, ConfigWindow::X) {
                info.x = ev.x;
            }
            if mask_has(ev.value_mask, ConfigWindow::Y) {
                info.y = ev.y;
            }
            if mask_has(ev.value_mask, ConfigWindow::WIDTH) {
                info.width = ev.width.max(160);
            }
            if mask_has(ev.value_mask, ConfigWindow::HEIGHT) {
                info.height = ev.height.max(120);
            }
            let title_h = self.titlebar_height(&info);
            self.conn.configure_window(
                info.frame,
                &ConfigureWindowAux::new()
                    .x(i32::from(info.x))
                    .y(i32::from(info.y))
                    .width(u32::from(info.width))
                    .height(u32::from(info.height + title_h)),
            )?;
            self.conn.configure_window(
                info.window,
                &ConfigureWindowAux::new()
                    .x(0)
                    .y(i32::from(title_h))
                    .width(u32::from(info.width))
                    .height(u32::from(info.height))
                    .border_width(0),
            )?;
            self.apply_frame_shape(&info)?;
            self.clients.insert(client, info);
            self.send_synthetic_configure(&info)?;
            self.redraw_frame_titlebar(client)?;
            return Ok(());
        }
        let aux = ConfigureWindowAux::from_configure_request(&ev);
        self.conn.configure_window(ev.window, &aux)?;
        Ok(())
    }

    pub(crate) fn manage_window(&mut self, window: Window) -> AnyResult<()> {
        if self.is_ui_window(window) || self.client_key_for(window).is_some() {
            return Ok(());
        }
        let attr = self.conn.get_window_attributes(window)?.reply()?;
        if attr.override_redirect {
            self.conn.map_window(window)?;
            return Ok(());
        }
        let was_mapped = attr.map_state != MapState::UNMAPPED;
        let geom = self.conn.get_geometry(window)?.reply()?;
        let class = self.window_class(window);
        let title = self.window_title(window);
        let is_ffplay = client_is_ffplay(&class, &title);
        let titlebar = !client_uses_own_chrome(&class, &title) && !self.window_wants_csd(window);
        let title_h = if titlebar { TITLEBAR_HEIGHT } else { 0 };
        let max_w = self.screen_width.saturating_sub(80).max(300);
        let max_h = self
            .screen_height
            .saturating_sub(TOPBAR_HEIGHT + DOCK_HEIGHT + title_h + 62)
            .max(240);
        let (x, y, width, height) = if is_ffplay {
            self.ffplay_geometry()
        } else {
            let width = geom.width.min(max_w);
            let height = geom.height.min(max_h);
            let x = if geom.x <= 0 { 42 } else { geom.x.max(16) };
            let y = if geom.y <= 0 {
                i16::try_from(TOPBAR_HEIGHT + 26).unwrap()
            } else {
                geom.y.max(i16::try_from(TOPBAR_HEIGHT + 8).unwrap())
            };
            (x, y, width, height)
        };
        let frame = self.conn.generate_id()?;
        let frame_aux = CreateWindowAux::new()
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::LEAVE_WINDOW
                    | EventMask::SUBSTRUCTURE_NOTIFY
                    | EventMask::SUBSTRUCTURE_REDIRECT,
            )
            .cursor(self.cursor)
            .background_pixel(0)
            .bit_gravity(Gravity::NORTH_WEST)
            .backing_store(BackingStore::WHEN_MAPPED)
            .save_under(1);
        self.conn.create_window(
            self.depth,
            frame,
            self.root,
            x,
            y,
            width,
            height + title_h,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &frame_aux,
        )?;
        self.conn.change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new().event_mask(
                EventMask::STRUCTURE_NOTIFY | EventMask::BUTTON_MOTION | EventMask::BUTTON_RELEASE,
            ),
        )?;
        self.conn.change_save_set(SetMode::INSERT, window)?;
        self.conn.configure_window(
            window,
            &ConfigureWindowAux::new()
                .x(0)
                .y(i32::from(title_h))
                .width(u32::from(width))
                .height(u32::from(height))
                .border_width(0),
        )?;
        if was_mapped {
            // A mapped reparent reports both structure and substructure unmaps.
            self.ignored_unmaps.extend([window, window]);
        }
        self.conn
            .reparent_window(window, frame, 0, title_h as i16)?;
        self.conn.map_window(window)?;
        self.conn.map_window(frame)?;
        // Set EWMH _NET_WM_DESKTOP on the client window and its frame
        if let Ok(desktop_atom) = self.atom(b"_NET_WM_DESKTOP") {
            if let Ok(cardinal_atom) = self.atom(b"CARDINAL") {
                let _ = self.conn.change_property32(
                    PropMode::REPLACE,
                    window,
                    desktop_atom,
                    cardinal_atom,
                    &[self.active_workspace as u32],
                );
                let _ = self.conn.change_property32(
                    PropMode::REPLACE,
                    frame,
                    desktop_atom,
                    cardinal_atom,
                    &[self.active_workspace as u32],
                );
            }
        }

        let info = ClientInfo {
            window,
            frame,
            workspace: self.active_workspace,
            mapped: true,
            x,
            y,
            width,
            height,
            titlebar,
            saved: None,
            sticky: false,
            fullscreen: false,
            fs_saved: None,
        };
        self.apply_frame_shape(&info)?;
        self.clients.insert(window, info);
        self.send_synthetic_configure(&info)?;
        if is_ffplay {
            self.schedule_window_nudge(window, width, height);
        }
        self.redraw_frame_titlebar(window)?;
        self.focus_window(window)?;
        self.redraw_dock()?;
        Ok(())
    }

    pub(crate) fn remove_client(&mut self, window: Window) -> AnyResult<()> {
        let Some(client) = self.client_key_for(window) else {
            return Ok(());
        };
        let Some(info) = self.clients.remove(&client) else {
            return Ok(());
        };
        let _ = self.conn.change_save_set(SetMode::DELETE, info.window);
        let _ = self
            .conn
            .reparent_window(info.window, self.root, info.x, info.y);
        let _ = self.conn.destroy_window(info.frame);
        self.focus_history.retain(|&w| w != client);
        if self.active_client == Some(client) {
            self.active_client = None;
            self.update_active_window_property()?;
        }
        self.redraw_dock()?;
        Ok(())
    }

    pub(crate) fn minimize_client(&mut self, client: Window) -> AnyResult<()> {
        if let Some(info) = self.clients.get_mut(&client) {
            info.mapped = false;
            self.ignored_unmaps.push(info.frame);
            self.conn.unmap_window(info.frame)?;
            self.focus_history.retain(|&w| w != client);
            if self.active_client == Some(client) {
                self.active_client = None;
                self.update_active_window_property()?;
            }
            self.redraw_dock()?;
        }
        Ok(())
    }

    pub(crate) fn toggle_maximize_client(&mut self, client: Window) -> AnyResult<()> {
        let Some(mut info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        if let Some((x, y, w, h)) = info.saved.take() {
            info.x = x;
            info.y = y;
            info.width = w;
            info.height = h;
        } else {
            info.saved = Some((info.x, info.y, info.width, info.height));
            info.x = 8;
            info.y = TOPBAR_HEIGHT as i16 + 6;
            info.width = self.screen_width.saturating_sub(16);
            info.height = self
                .screen_height
                .saturating_sub(TOPBAR_HEIGHT + DOCK_HEIGHT + self.titlebar_height(&info) + 18);
        }
        let title_h = self.titlebar_height(&info);
        self.conn.configure_window(
            info.frame,
            &ConfigureWindowAux::new()
                .x(i32::from(info.x))
                .y(i32::from(info.y))
                .width(u32::from(info.width))
                .height(u32::from(info.height + title_h))
                .stack_mode(StackMode::ABOVE),
        )?;
        self.conn.configure_window(
            info.window,
            &ConfigureWindowAux::new()
                .x(0)
                .y(i32::from(title_h))
                .width(u32::from(info.width))
                .height(u32::from(info.height)),
        )?;
        self.apply_frame_shape(&info)?;
        self.clients.insert(client, info);
        self.send_synthetic_configure(&info)?;
        self.redraw_frame_titlebar(client)?;
        self.focus_window(client)?;
        Ok(())
    }

    pub(crate) fn focus_window(&mut self, window: Window) -> AnyResult<()> {
        self.focus_window_at(window, CURRENT_TIME)
    }

    pub(crate) fn focus_window_at(&mut self, window: Window, time: Timestamp) -> AnyResult<()> {
        self.hide_dock_more_menu()?;
        let Some(client) = self.client_key_for(window) else {
            return Ok(());
        };
        let Some(info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        if !self.client_on_active_workspace(&info) {
            return Ok(());
        }
        let previous_active = self.active_client;
        self.active_client = Some(client);
        self.focus_history.retain(|&w| w != client);
        self.focus_history.push(client);
        if !info.mapped {
            let mut mapped = info;
            mapped.mapped = true;
            self.clients.insert(client, mapped);
            self.conn.map_window(mapped.frame)?;
            self.redraw_dock()?;
        }
        self.conn
            .set_input_focus(InputFocus::POINTER_ROOT, info.window, time)?;
        self.send_take_focus(&info, time)?;
        self.settings_front = false;
        self.folder_front = false;
        self.media_front = false;
        self.conn.configure_window(
            info.frame,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?;
        if previous_active.is_some_and(|old| old != client) {
            if let Some(old) = previous_active {
                let _ = self.redraw_frame_titlebar(old);
            }
        }
        self.redraw_frame_titlebar(client)?;
        if previous_active != Some(client) {
            self.redraw_dock()?;
        }
        // The dock stays above app windows (raised by raise_chrome below).
        self.raise_chrome()?;
        if info.fullscreen {
            self.conn.configure_window(
                info.frame,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        self.update_active_window_property()?;
        Ok(())
    }

    pub(crate) fn update_active_window_property(&self) -> AnyResult<()> {
        let active_atom = self.atom(b"_NET_ACTIVE_WINDOW")?;
        let window_atom = self.atom(b"WINDOW")?;
        let active_val = self.active_client.unwrap_or(0);
        self.conn.change_property32(
            PropMode::REPLACE,
            self.root,
            active_atom,
            window_atom,
            &[active_val],
        )?;
        Ok(())
    }

    pub(crate) fn send_take_focus(&self, info: &ClientInfo, time: Timestamp) -> AnyResult<()> {
        let wm_protocols = self.atom(b"WM_PROTOCOLS")?;
        let wm_take_focus = self.atom(b"WM_TAKE_FOCUS")?;
        let Ok(reply) = self
            .conn
            .get_property(false, info.window, wm_protocols, AtomEnum::ATOM, 0, 32)?
            .reply()
        else {
            return Ok(());
        };
        let supports_take_focus = reply
            .value32()
            .is_some_and(|mut atoms| atoms.any(|atom| atom == wm_take_focus));
        if supports_take_focus {
            let event = ClientMessageEvent::new(
                32,
                info.window,
                wm_protocols,
                [wm_take_focus, time, 0, 0, 0],
            );
            self.conn
                .send_event(false, info.window, EventMask::NO_EVENT, event)?;
        }
        Ok(())
    }

    pub(crate) fn titlebar_height(&self, info: &ClientInfo) -> u16 {
        if info.titlebar && !info.fullscreen {
            TITLEBAR_HEIGHT
        } else {
            0
        }
    }

    pub(crate) fn update_client_chrome(&mut self, client: Window) -> AnyResult<()> {
        let Some(mut info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        if !info.titlebar
            || !(client_uses_own_chrome(&self.window_class(client), &self.window_title(client))
                || self.window_wants_csd(client))
        {
            return Ok(());
        }
        info.titlebar = false;
        self.conn.configure_window(
            info.frame,
            &ConfigureWindowAux::new()
                .width(u32::from(info.width))
                .height(u32::from(info.height)),
        )?;
        self.conn.configure_window(
            info.window,
            &ConfigureWindowAux::new()
                .x(0)
                .y(0)
                .width(u32::from(info.width))
                .height(u32::from(info.height)),
        )?;
        self.apply_frame_shape(&info)?;
        self.clients.insert(client, info);
        self.send_synthetic_configure(&info)?;
        self.redraw_frame_titlebar(client)?;
        Ok(())
    }

    pub(crate) fn close_client(&self, client: Window) -> AnyResult<()> {
        if let Some(info) = self.clients.get(&client) {
            let wm_protocols = self.conn.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
            let wm_delete_window = self
                .conn
                .intern_atom(false, b"WM_DELETE_WINDOW")?
                .reply()?
                .atom;
            let event = ClientMessageEvent::new(
                32,
                info.window,
                wm_protocols,
                [wm_delete_window, CURRENT_TIME, 0, 0, 0],
            );
            self.conn
                .send_event(false, info.window, EventMask::NO_EVENT, event)?;
        }
        Ok(())
    }

    pub(crate) fn redraw_frame_titlebar(&self, client: Window) -> AnyResult<()> {
        let Some(info) = self.clients.get(&client) else {
            return Ok(());
        };
        if self.titlebar_height(info) == 0 {
            return Ok(());
        }
        let mut c = Canvas::from_wallpaper_crop(
            &self.wallpaper_pixels,
            self.screen_width,
            i32::from(info.x),
            i32::from(info.y),
            info.width,
            TITLEBAR_HEIGHT,
        );
        let active = self.active_client == Some(client);
        c.draw_rect(
            0,
            0,
            i32::from(info.width),
            i32::from(TITLEBAR_HEIGHT),
            if active {
                Color::rgba(221, 238, 252, 232)
            } else {
                Color::rgba(250, 254, 255, 225)
            },
        );
        c.draw_circle(19, 17, 8, Color::rgba(241, 96, 105, 235));
        c.draw_circle(42, 17, 8, Color::rgba(246, 190, 82, 235));
        c.draw_circle(65, 17, 8, Color::rgba(76, 197, 178, 235));
        if let Some((hover_client, button)) = self.title_hover {
            if hover_client == client {
                match button {
                    TitleButton::Close => {
                        c.draw_line(15, 13, 23, 21, 2, Color::rgba(80, 20, 25, 230));
                        c.draw_line(23, 13, 15, 21, 2, Color::rgba(80, 20, 25, 230));
                    }
                    TitleButton::Minimize => {
                        c.draw_line(37, 17, 47, 17, 2, Color::rgba(90, 60, 15, 235));
                    }
                    TitleButton::Maximize => {
                        c.draw_round_rect(60, 12, 10, 10, 2, Color::rgba(30, 90, 82, 225));
                        c.draw_round_rect(62, 14, 6, 6, 1, Color::rgba(250, 254, 255, 190));
                    }
                }
            }
        }
        let title = compact(
            &self.window_title(info.window),
            ((info.width / 9).max(8)) as usize,
        );
        c.draw_text(&self.bold, &title, 92, 9, 13.0, INK);
        self.upload_canvas(info.frame, &c)
    }

    pub(crate) fn apply_frame_shape(&self, info: &ClientInfo) -> AnyResult<()> {
        if !self.shape_supported {
            return Ok(());
        }

        let title_h = self.titlebar_height(info);
        let frame_h = info.height + title_h;
        let radius = if title_h > 0 { FRAME_CORNER_RADIUS } else { 0 };
        let rects = rounded_top_shape_rects(info.width, frame_h, radius);
        self.conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::BOUNDING,
            ClipOrdering::YX_BANDED,
            info.frame,
            0,
            0,
            &rects,
        )?;
        Ok(())
    }

    pub(crate) fn suppress_uncomposited_cursor_overlay(&self, window: Window) -> AnyResult<bool> {
        let Ok(reply) = self
            .conn
            .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 96)?
            .reply()
        else {
            return Ok(false);
        };
        let title = String::from_utf8_lossy(&reply.value);
        if !title.starts_with("Cua.AgentCursorOverlay.") {
            return Ok(false);
        }

        if self.shape_supported {
            // cua-driver's cursor is a full-screen ARGB window. Without a real
            // alpha compositor its transparent pixels are rendered as black by
            // Xorg/Xephyr. Keep the window mapped (the driver continually
            // enforces its z-order), but give it an empty visible shape.
            self.conn.shape_rectangles(
                shape::SO::SET,
                shape::SK::BOUNDING,
                ClipOrdering::YX_BANDED,
                window,
                0,
                0,
                &[],
            )?;
        } else {
            self.conn.unmap_window(window)?;
        }
        self.conn.flush()?;
        eprintln!("aurora-wm: suppressed unsupported ARGB cursor overlay {title}");
        Ok(true)
    }

    pub(crate) fn window_title(&self, window: Window) -> String {
        let Ok(cookie) =
            self.conn
                .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 96)
        else {
            return "Window".to_string();
        };
        let Ok(reply) = cookie.reply() else {
            return "Window".to_string();
        };
        let title = String::from_utf8_lossy(&reply.value).trim().to_string();
        if title.is_empty() {
            "Window".to_string()
        } else {
            title
        }
    }

    pub(crate) fn window_class(&self, window: Window) -> String {
        let Ok(cookie) =
            self.conn
                .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 128)
        else {
            return String::new();
        };
        let Ok(reply) = cookie.reply() else {
            return String::new();
        };
        String::from_utf8_lossy(&reply.value).to_ascii_lowercase()
    }

}
