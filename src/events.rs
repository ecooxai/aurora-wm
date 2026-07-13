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
use crate::layout::*;
use crate::draw_helpers::*;
use crate::pixels::*;
use crate::system::*;
use crate::textutil::*;
use crate::procutil::*;
use crate::files::*;

impl Aurora {
    pub(crate) fn handle_event(&mut self, event: Event) -> AnyResult<()> {
        match event {
            Event::Expose(ev) => self.handle_expose(ev)?,
            Event::KeyPress(ev) => self.handle_key_press(ev)?,
            Event::KeyRelease(ev) => self.handle_key_release(ev)?,
            Event::ButtonPress(ev) => self.handle_button_press(ev)?,
            Event::ButtonRelease(ev) => {
                if self.settings.auto_power_saver_slider_dragging {
                    self.settings.auto_power_saver_slider_dragging = false;
                    self.pending_auto_power_saver_apply = None;
                    self.apply_auto_power_saver_setting()?;
                    self.redraw_settings()?;
                } else if self.pending_screenshot_button.is_some() {
                    self.handle_topbar_release(ev)?;
                } else if self.screenshot_selection.is_some() {
                    self.finish_screenshot_selection(ev.root_x, ev.root_y)?;
                } else if self.pending_ui_resize.is_some() || self.ui_resize.is_some() {
                    self.end_drag()?;
                } else if ev.event == self.ui.folder_terminal {
                    self.handle_folder_terminal_release()?;
                } else if ev.event == self.ui.folder {
                    self.handle_folder_release(ev)?;
                } else if let Some(slot) = self.media_slot_for_window(ev.event) {
                    self.handle_media_release(slot)?;
                } else {
                    self.pending_client_drag = None;
                    if self.drag.is_some() {
                        let _ = self.update_drag_position_inner(ev.root_x, ev.root_y, true)?;
                    }
                    if self.ui_resize.is_some() {
                        let _ = self.update_ui_resize(ev.root_x, ev.root_y)?;
                    }
                    self.end_drag()?;
                }
            }
            Event::MotionNotify(ev) => {
                self.handle_motion_notify(ev)?;
            }
            Event::LeaveNotify(ev) => self.handle_leave_notify(ev)?,
            Event::EnterNotify(ev) => self.handle_enter_notify(ev)?,
            Event::ClientMessage(ev) => self.handle_client_message(ev)?,
            Event::SelectionRequest(ev) => self.handle_selection_request(ev)?,
            Event::SelectionNotify(ev) => self.handle_selection_notify(ev)?,
            Event::XfixesSelectionNotify(ev) => self.handle_xfixes_selection_notify(ev)?,
            Event::SelectionClear(ev) => {
                if ev.selection == self.wm_s_atom {
                    std::process::exit(0);
                }
            }
            Event::CreateNotify(ev) => {
                if ev.parent == self.root && ev.override_redirect {
                    // ARGB overlays normally set WM_NAME between CreateWindow
                    // and MapWindow. Subscribe immediately so an unsupported
                    // transparent overlay can be shaped before it ever covers
                    // the desktop.
                    let _ = self.conn.change_window_attributes(
                        ev.window,
                        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                    );
                    let _ = self.suppress_uncomposited_cursor_overlay(ev.window);
                }
            }
            Event::MapRequest(ev) => self.manage_window(ev.window)?,
            Event::MapNotify(ev) => {
                if ev.event == self.root {
                    if ev.override_redirect {
                        // Watch for a title assigned just after mapping as well as
                        // the usual set-title-before-map sequence.
                        let _ = self.conn.change_window_attributes(
                            ev.window,
                            &ChangeWindowAttributesAux::new()
                                .event_mask(EventMask::PROPERTY_CHANGE),
                        );
                        if self
                            .suppress_uncomposited_cursor_overlay(ev.window)
                            .unwrap_or(false)
                        {
                            // The overlay can cover the screen briefly before its
                            // empty shape is applied. Repaint regions invalidated
                            // during that mapping so no black remnants remain.
                            self.redraw_everything()?;
                        }
                    } else {
                        // Save-set restoration can map a surviving client after startup scanning.
                        let _ = self.adopt_mapped_root_window(ev.window);
                    }
                }
            }
            Event::ConfigureRequest(ev) => self.handle_configure_request(ev)?,
            Event::DestroyNotify(ev) => self.remove_client(ev.window)?,
            Event::UnmapNotify(ev) => {
                if let Some(pos) = self.ignored_unmaps.iter().position(|&win| win == ev.window) {
                    self.ignored_unmaps.swap_remove(pos);
                } else {
                    self.remove_client(ev.window)?;
                }
            }
            Event::PropertyNotify(ev) => self.handle_property_notify(ev)?,
            Event::ConfigureNotify(ev) => {
                if ev.window == self.root {
                    self.resize_to_root()?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn handle_xfixes_selection_notify(
        &mut self,
        ev: xfixes::SelectionNotifyEvent,
    ) -> AnyResult<()> {
        if ev.selection == self.atom(b"CLIPBOARD")? {
            self.clipboard_dirty = true;
        }
        Ok(())
    }

    pub(crate) fn handle_property_notify(&mut self, ev: PropertyNotifyEvent) -> AnyResult<()> {
        if ev.atom == AtomEnum::WM_NAME.into()
            && self.suppress_uncomposited_cursor_overlay(ev.window)?
        {
            self.redraw_everything()?;
            return Ok(());
        }
        if !self.clients.contains_key(&ev.window) {
            return Ok(());
        }
        let relevant = ev.atom == AtomEnum::WM_NAME.into()
            || ev.atom == AtomEnum::WM_CLASS.into()
            || ev.atom == AtomEnum::WM_HINTS.into();
        if !relevant {
            return Ok(());
        }

        let had_titlebar = self
            .clients
            .get(&ev.window)
            .is_some_and(|info| info.titlebar);
        self.update_client_chrome(ev.window)?;
        if self
            .clients
            .get(&ev.window)
            .is_some_and(|info| info.titlebar)
        {
            self.redraw_frame_titlebar(ev.window)?;
        }
        if had_titlebar || ev.atom == AtomEnum::WM_HINTS.into() {
            self.redraw_dock()?;
        }
        Ok(())
    }

    pub(crate) fn handle_expose(&mut self, ev: ExposeEvent) -> AnyResult<()> {
        if ev.window == self.root {
            self.clear_root_region(
                i32::from(ev.x),
                i32::from(ev.y),
                u32::from(ev.width),
                u32::from(ev.height),
            )?;
            self.conn.flush()?;
            return Ok(());
        }
        if ev.count != 0 {
            return Ok(());
        }
        if ev.window == self.ui.topbar {
            self.redraw_topbar()?;
        } else if ev.window == self.ui.dock {
            self.redraw_dock()?;
        } else if ev.window == self.ui.settings && self.settings_visible {
            self.redraw_settings()?;
        } else if ev.window == self.ui.folder {
            self.redraw_folder()?;
        } else if ev.window == self.ui.folder_terminal && self.folder_terminal.visible {
            self.redraw_folder_terminal()?;
        } else if ev.window == self.ui.screenshot_overlay && self.screenshot_mode {
            self.redraw_screenshot_overlay()?;
        } else if ev.window == self.ui.app_menu && self.app_menu_visible {
            self.redraw_app_menu()?;
        } else if ev.window == self.ui.title_menu && self.title_menu_open.is_some() {
            self.redraw_title_menu()?;
        } else if ev.window == self.ui.confirm_dialog && self.confirm_close.is_some() {
            self.redraw_close_confirm()?;
        } else if ev.window == self.ui.tooltip {
            if let Some((_, text)) = self.tooltip_shown.clone() {
                let tw = (measure_text(&self.regular, &text, 12.0) + 22) as u16;
                self.redraw_tooltip(&text, tw)?;
            }
        } else if ev.window == self.ui.aurora_menu && self.aurora_menu_visible {
            self.redraw_aurora_menu()?;
        } else if ev.window == self.ui.clipboard_menu && self.clipboard_menu_visible {
            self.redraw_clipboard_menu()?;
        } else if ev.window == self.ui.dock_more_menu && self.dock_more_visible {
            self.redraw_dock_more_menu()?;
        } else if let Some(slot) = self.media_slot_for_window(ev.window) {
            if self
                .media_slots
                .get(slot)
                .and_then(|m| m.as_ref())
                .is_some()
            {
                self.redraw_media_slot(slot)?;
            }
        } else if let Some(client) = self.client_key_for(ev.window) {
            if self
                .clients
                .get(&client)
                .is_some_and(|info| info.frame == ev.window)
            {
                self.redraw_frame_titlebar(client)?;
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    pub(crate) fn handle_button_press(&mut self, ev: ButtonPressEvent) -> AnyResult<()> {
        self.last_pointer_activity = Instant::now();
        // Close-confirmation dialog: route by root coordinates because the
        // synchronous root button grab reports these presses on the root
        // window first; the replayed event then reaches the dialog itself.
        if self.confirm_close.is_some() {
            let (cx, cy, cw, ch) = self.confirm_geometry();
            let rx = i32::from(ev.root_x);
            let ry = i32::from(ev.root_y);
            let inside = rx >= i32::from(cx)
                && rx <= i32::from(cx) + i32::from(cw)
                && ry >= i32::from(cy)
                && ry <= i32::from(cy) + i32::from(ch);
            if inside {
                if ev.event == self.ui.confirm_dialog {
                    self.handle_confirm_click(rx - i32::from(cx), ry - i32::from(cy))?;
                    self.conn.flush()?;
                    return Ok(());
                }
                if ev.event == self.root {
                    self.conn.allow_events(Allow::REPLAY_POINTER, ev.time)?;
                    self.conn.flush()?;
                    return Ok(());
                }
            } else if ev.event != self.ui.confirm_dialog {
                self.hide_close_confirm()?;
            }
        }
        // Title dropdown menu: same replay-aware routing.
        if let Some(menu_client) = self.title_menu_open {
            let (mx, my, mw, mh) = self.title_menu_geometry(menu_client);
            let rx = i32::from(ev.root_x);
            let ry = i32::from(ev.root_y);
            let inside = rx >= i32::from(mx)
                && rx <= i32::from(mx) + i32::from(mw)
                && ry >= i32::from(my)
                && ry <= i32::from(my) + i32::from(mh);
            if inside {
                if ev.event == self.ui.title_menu || ev.event == self.root {
                    // The synchronous root grab reports the press on the root window; handle
                    // it directly (rather than replaying) so the click reaches the menu.
                    if ev.event == self.root {
                        self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
                    }
                    self.handle_title_menu_click(ry - i32::from(my))?;
                    self.conn.flush()?;
                    return Ok(());
                }
            } else {
                self.hide_title_menu()?;
            }
        }
        if self.dock_more_visible && ev.event != self.ui.dock_more_menu && ev.event != self.ui.dock
        {
            self.hide_dock_more_menu()?;
        }
        if self.clipboard_menu_visible && ev.event != self.ui.clipboard_menu {
            let (mx, my, mw, mh) = self.clipboard_menu_geometry();
            let rx = i32::from(ev.root_x);
            let ry = i32::from(ev.root_y);
            if rx >= i32::from(mx)
                && rx <= i32::from(mx) + i32::from(mw)
                && ry >= i32::from(my)
                && ry <= i32::from(my) + i32::from(mh)
            {
                self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
                self.handle_clipboard_menu_press(
                    ev.detail,
                    rx - i32::from(mx),
                    ry - i32::from(my),
                )?;
                self.conn.flush()?;
                return Ok(());
            }
        }
        if self.aurora_menu_visible && ev.event != self.ui.topbar && ev.event != self.ui.aurora_menu
        {
            let (mx, my, mw, mh) = self.aurora_menu_geometry();
            let rx = i32::from(ev.root_x);
            let ry = i32::from(ev.root_y);
            if rx >= i32::from(mx)
                && rx <= i32::from(mx) + i32::from(mw)
                && ry >= i32::from(my)
                && ry <= i32::from(my) + i32::from(mh)
            {
                self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
                self.handle_aurora_menu_click(rx - i32::from(mx), ry - i32::from(my))?;
                return Ok(());
            }
            self.hide_aurora_menu()?;
        }
        let topbar_root_click = ev.root_y >= 0 && ev.root_y < TOPBAR_HEIGHT as i16;
        if self.clipboard_menu_visible
            && ev.event != self.ui.topbar
            && ev.event != self.ui.clipboard_menu
            && !topbar_root_click
        {
            self.hide_clipboard_menu()?;
        }
        let pointer_target = if ev.event == self.root && ev.detail == 1 {
            self.conn.query_pointer(self.root)?.reply()?.child
        } else {
            ev.event
        };
        if self.app_menu_visible
            && ev.event != self.ui.app_menu
            && pointer_target != self.ui.app_menu
            && ev.event != self.ui.dock
            && pointer_target != self.ui.dock
        {
            self.hide_app_menu()?;
        }
        if self.screenshot_mode && ev.detail == 1 {
            self.start_screenshot_selection(ev.root_x, ev.root_y)?;
            if ev.event == self.root {
                self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
            }
        } else if ev.event == self.ui.settings || pointer_target == self.ui.settings {
            let (settings_x, settings_y, _, _) = self.settings_geometry();
            let (event_x, event_y) = if ev.event == self.ui.settings {
                (i32::from(ev.event_x), i32::from(ev.event_y))
            } else {
                (
                    i32::from(ev.root_x) - i32::from(settings_x),
                    i32::from(ev.root_y) - i32::from(settings_y),
                )
            };
            if ev.detail == 1 {
                self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
                self.conn.set_input_focus(
                    InputFocus::POINTER_ROOT,
                    self.ui.settings,
                    CURRENT_TIME,
                )?;
            }
            if ev.detail == 4 || ev.detail == 5 {
                self.handle_settings_scroll(ev.detail, event_x, event_y)?;
                self.conn.flush()?;
                return Ok(());
            }
            self.settings_front = true;
            self.folder_front = false;
            self.media_front = false;
            self.raise_ui()?;
            self.handle_settings_click(event_x, event_y)?;
        } else if ev.event == self.ui.dock || pointer_target == self.ui.dock {
            let (dock_x, dock_y, _, _) = self.dock_geometry();
            let (event_x, event_y) = if ev.event == self.ui.dock {
                (i32::from(ev.event_x), i32::from(ev.event_y))
            } else {
                (
                    i32::from(ev.root_x) - i32::from(dock_x),
                    i32::from(ev.root_y) - i32::from(dock_y),
                )
            };
            if ev.detail == 1 {
                self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
            }
            self.handle_dock_click(event_x, event_y)?;
        } else if ev.event == self.ui.folder {
            if ev.detail == 4 || ev.detail == 5 {
                self.handle_folder_scroll(ev.detail)?;
                self.conn.flush()?;
                return Ok(());
            }
            self.folder_front = true;
            self.settings_front = false;
            self.media_front = false;
            self.raise_ui()?;
            if ev.detail == 1
                && self.ui_bottom_right_resize_hit(UiResizeTarget::Folder, ev.event_x, ev.event_y)
            {
                self.pending_ui_resize = Some(PendingUiResize {
                    target: UiResizeTarget::Folder,
                    root_x: ev.root_x,
                    root_y: ev.root_y,
                    pressed_at: Instant::now(),
                });
                return Ok(());
            }
            if ev.detail == 3 {
                self.handle_folder_context(i32::from(ev.event_x), i32::from(ev.event_y))?;
            } else {
                self.handle_folder_click(ev)?;
            }
        } else if ev.event == self.ui.folder_terminal {
            if ev.detail == 4 || ev.detail == 5 {
                self.handle_folder_terminal_scroll(ev.detail)?;
                self.conn.flush()?;
                return Ok(());
            }
            self.folder_front = true;
            self.settings_front = false;
            self.media_front = false;
            self.folder_terminal.focused = true;
            self.conn.set_input_focus(
                InputFocus::POINTER_ROOT,
                self.ui.folder_terminal,
                CURRENT_TIME,
            )?;
            self.raise_ui()?;
            if ev.detail == 1
                && self.ui_bottom_right_resize_hit(
                    UiResizeTarget::FolderTerminal,
                    ev.event_x,
                    ev.event_y,
                )
            {
                self.pending_ui_resize = Some(PendingUiResize {
                    target: UiResizeTarget::FolderTerminal,
                    root_x: ev.root_x,
                    root_y: ev.root_y,
                    pressed_at: Instant::now(),
                });
                return Ok(());
            }
            self.handle_folder_terminal_click(i32::from(ev.event_x), i32::from(ev.event_y))?;
            self.redraw_folder_terminal()?;
        } else if ev.event == self.ui.app_menu || pointer_target == self.ui.app_menu {
            let (menu_x, menu_y, _, _) = self.app_menu_geometry();
            let (event_x, event_y) = if ev.event == self.ui.app_menu {
                (i32::from(ev.event_x), i32::from(ev.event_y))
            } else {
                (
                    i32::from(ev.root_x) - i32::from(menu_x),
                    i32::from(ev.root_y) - i32::from(menu_y),
                )
            };
            if ev.detail == 1 {
                self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
            }
            self.handle_app_menu_click(ev.detail, event_x, event_y)?;
        } else if ev.event == self.ui.aurora_menu {
            self.handle_aurora_menu_click(i32::from(ev.event_x), i32::from(ev.event_y))?;
        } else if ev.event == self.ui.clipboard_menu {
            self.handle_clipboard_menu_press(
                ev.detail,
                i32::from(ev.event_x),
                i32::from(ev.event_y),
            )?;
        } else if ev.event == self.ui.dock_more_menu {
            self.handle_dock_more_menu_click(i32::from(ev.event_x), i32::from(ev.event_y))?;
        } else if let Some(slot) = self.media_slot_for_window(ev.event) {
            if ev.detail == 4 || ev.detail == 5 {
                self.handle_media_scroll(slot, ev.detail)?;
                self.conn.flush()?;
                return Ok(());
            }
            self.media_front = true;
            self.media_front_slot = Some(slot);
            self.settings_front = false;
            self.folder_front = false;
            self.conn
                .set_input_focus(InputFocus::POINTER_ROOT, ev.event, CURRENT_TIME)?;
            self.handle_media_click(
                slot,
                ev.detail,
                i32::from(ev.event_x),
                i32::from(ev.event_y),
            )?;
        } else if ev.event == self.ui.topbar {
            let x = i32::from(ev.event_x);
            let _ = self.handle_topbar_press_x(x)?;
        } else if ev.event == self.root {
            self.hide_aurora_menu()?;
            self.handle_root_button_press(ev)?;
        } else if let Some(client) = self.client_or_ancestor_key_for(ev.event) {
            if self
                .clients
                .get(&client)
                .is_some_and(|info| info.frame == ev.event)
            {
                self.handle_frame_click(client, ev)?;
            } else {
                self.handle_client_click(client, ev)?;
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    pub(crate) fn handle_motion_notify(&mut self, ev: MotionNotifyEvent) -> AnyResult<bool> {
        self.last_pointer_activity = Instant::now();
        if self.settings.auto_power_saver_slider_dragging {
            let button_down = u16::from(ev.state) & u16::from(KeyButMask::BUTTON1) != 0;
            if button_down {
                let (settings_x, _, _, _) = self.settings_geometry();
                self.set_auto_power_saver_from_slider(
                    i32::from(ev.root_x) - i32::from(settings_x),
                )?;
            } else {
                self.settings.auto_power_saver_slider_dragging = false;
                self.pending_auto_power_saver_apply = None;
                self.apply_auto_power_saver_setting()?;
                self.redraw_settings()?;
            }
            return Ok(true);
        }
        if ev.event == self.ui.topbar && self.drag.is_none() {
            self.update_topbar_tooltip(i32::from(ev.event_x))?;
        }
        if self.drag.is_none() {
            if self.screenshot_mode {
                if let Some(selection) = self.screenshot_selection.as_mut() {
                    selection.current_x = ev.root_x;
                    selection.current_y = ev.root_y;
                    self.update_screenshot_live_rect()?;
                    return Ok(true);
                }
                return Ok(false);
            }
            if let Some(pending) = self.pending_client_drag {
                let moved = (i32::from(ev.root_x) - i32::from(pending.root_x)).abs() > 4
                    || (i32::from(ev.root_y) - i32::from(pending.root_y)).abs() > 4;
                if moved && self.drag.is_none() {
                    self.pending_client_drag = None;
                    self.start_drag(pending.client, ev.root_x, ev.root_y)?;
                    return Ok(true);
                }
            }
            let mut changed = false;
            if let Some(slot) = self.media_slot_for_window(ev.event) {
                let button_down = u16::from(ev.state) & u16::from(KeyButMask::BUTTON1) != 0;
                self.handle_media_motion(
                    slot,
                    i32::from(ev.event_x),
                    i32::from(ev.event_y),
                    button_down,
                )?;
                changed |= button_down;
            }
            if ev.event == self.ui.folder_terminal {
                let button_down = u16::from(ev.state) & u16::from(KeyButMask::BUTTON1) != 0;
                self.handle_folder_terminal_motion(
                    i32::from(ev.event_x),
                    i32::from(ev.event_y),
                    button_down,
                )?;
                changed |= button_down;
            }
            if let Some(ref mut pending) = self.pending_resize {
                pending.root_x = ev.root_x;
                pending.root_y = ev.root_y;
            }
            if let Some(ref mut pending) = self.pending_ui_resize {
                pending.root_x = ev.root_x;
                pending.root_y = ev.root_y;
            }
            if let Some(client) = self.client_key_for(ev.event) {
                let next = self
                    .clients
                    .get(&client)
                    .filter(|info| info.frame == ev.event && info.titlebar)
                    .and_then(|_| {
                        hover_title_button(ev.event_x, ev.event_y).map(|button| (client, button))
                    });
                if next != self.title_hover {
                    let old = self.title_hover.take().map(|(client, _)| client);
                    self.title_hover = next;
                    if let Some(old) = old {
                        self.redraw_frame_titlebar(old)?;
                    }
                    if let Some((client, _)) = self.title_hover {
                        self.redraw_frame_titlebar(client)?;
                    }
                    changed = true;
                }
            }
            return Ok(changed);
        }
        if self.ui_resize.is_some() {
            return self.update_ui_resize(ev.root_x, ev.root_y);
        }
        self.update_drag_position(ev.root_x, ev.root_y)
    }

    pub(crate) fn update_drag_position(&mut self, root_x: i16, root_y: i16) -> AnyResult<bool> {
        self.update_drag_position_inner(root_x, root_y, false)
    }

    pub(crate) fn update_drag_position_inner(
        &mut self,
        root_x: i16,
        root_y: i16,
        force: bool,
    ) -> AnyResult<bool> {
        let Some(mut drag) = self.drag else {
            return Ok(false);
        };
        let now = Instant::now();
        let min_interval = match drag.kind {
            DragKind::Move if self.compositor_active => COMPOSITED_MOVE_INTERVAL,
            DragKind::Move => NON_COMPOSITED_MOVE_INTERVAL,
            DragKind::Resize => Duration::from_millis(33),
        };
        if !force && now.duration_since(drag.last_update_at) < min_interval {
            return Ok(false);
        }
        let Some(mut info) = self.clients.get(&drag.client).copied() else {
            self.drag = None;
            return Ok(true);
        };
        let old_info = info;
        match drag.kind {
            DragKind::Move => {
                info.x = root_x.saturating_sub(drag.offset_x);
                info.y = root_y.saturating_sub(drag.offset_y);
                if self
                    .clients
                    .get(&drag.client)
                    .is_some_and(|old| old.x == info.x && old.y == info.y)
                {
                    return Ok(false);
                }
                self.conn.configure_window(
                    info.frame,
                    &ConfigureWindowAux::new()
                        .x(i32::from(info.x))
                        .y(i32::from(info.y)),
                )?;
                if !self.compositor_active {
                    let old_h = old_info.height + self.titlebar_height(&old_info);
                    self.clear_root_region(
                        i32::from(old_info.x),
                        i32::from(old_info.y),
                        u32::from(old_info.width),
                        u32::from(old_h),
                    )?;
                    self.conn.flush()?;
                }
            }
            DragKind::Resize => {
                let dx = i32::from(root_x) - i32::from(drag.start_root_x);
                let dy = i32::from(root_y) - i32::from(drag.start_root_y);
                let min_w = 180;
                let min_h = 120;
                let mut new_x = i32::from(drag.start_x);
                let mut new_y = i32::from(drag.start_y);
                let mut new_w = i32::from(drag.start_w);
                let mut new_h = i32::from(drag.start_h);
                if drag.resize_edges.right {
                    new_w = (i32::from(drag.start_w) + dx).max(min_w);
                }
                if drag.resize_edges.left {
                    new_w = (i32::from(drag.start_w) - dx).max(min_w);
                    new_x = i32::from(drag.start_x) + i32::from(drag.start_w) - new_w;
                }
                if drag.resize_edges.bottom {
                    new_h = (i32::from(drag.start_h) + dy).max(min_h);
                }
                if drag.resize_edges.top {
                    new_h = (i32::from(drag.start_h) - dy).max(min_h);
                    new_y = i32::from(drag.start_y) + i32::from(drag.start_h) - new_h;
                }
                info.x = new_x.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
                info.y = new_y.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
                info.width = new_w as u16;
                info.height = new_h as u16;
                if self.clients.get(&drag.client).is_some_and(|old| {
                    old.x == info.x
                        && old.y == info.y
                        && old.width == info.width
                        && old.height == info.height
                }) {
                    return Ok(false);
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
                        .height(u32::from(info.height)),
                )?;
                self.apply_frame_shape(&info)?;
                self.redraw_frame_titlebar(drag.client)?;
            }
        }
        drag.last_update_at = now;
        self.drag = Some(drag);
        self.clients.insert(drag.client, info);
        if force || matches!(drag.kind, DragKind::Resize) {
            self.send_synthetic_configure(&info)?;
        }
        Ok(true)
    }

    pub(crate) fn handle_leave_notify(&mut self, ev: LeaveNotifyEvent) -> AnyResult<()> {
        if ev.event == self.ui.topbar {
            self.hide_tooltip()?;
        }
        let Some((client, _)) = self.title_hover else {
            return Ok(());
        };
        if self
            .clients
            .get(&client)
            .is_some_and(|info| info.frame == ev.event)
        {
            self.title_hover = None;
            self.redraw_frame_titlebar(client)?;
        }
        Ok(())
    }

    pub(crate) fn handle_enter_notify(&mut self, _ev: EnterNotifyEvent) -> AnyResult<()> {
        Ok(())
    }

    pub(crate) fn handle_client_message(&mut self, ev: ClientMessageEvent) -> AnyResult<()> {
        let active_atom = self.atom(b"_NET_ACTIVE_WINDOW")?;
        if ev.type_ == active_atom {
            if let Some(client) = self.client_or_ancestor_key_for(ev.window) {
                if let Some(info) = self.clients.get(&client) {
                    if info.workspace != self.active_workspace {
                        self.switch_workspace(info.workspace)?;
                    }
                }
                self.focus_window(client)?;
            }
            return Ok(());
        }
        let open_folder_atom = self.atom(b"_AURORA_OPEN_FOLDER")?;
        if ev.type_ == open_folder_atom {
            let path_atom = self
                .conn
                .intern_atom(false, b"_AURORA_OPEN_FOLDER_PATH")?
                .reply()?
                .atom;
            let string_atom = self.conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
            if let Ok(prop) = self
                .conn
                .get_property(false, self.root, path_atom, string_atom, 0, 65535)?
                .reply()
            {
                if !prop.value.is_empty() {
                    let path_str = String::from_utf8_lossy(&prop.value).into_owned();
                    let path = PathBuf::from(path_str);
                    if path.exists() {
                        if !self.launch_file_manager(&path) {
                            self.folder_path = path.clone();
                            self.folder_entries = folder_entries_in(path, self.folder_sort);
                            self.folder_selected = None;
                            self.folder_scroll = 0;
                            self.folder_front = true;
                            self.choose_file_mode = false;
                            self.conn.map_window(self.ui.folder)?;
                            self.redraw_folder()?;
                            self.raise_ui()?;
                        }
                    }
                }
            }
            return Ok(());
        }

        let choose_file_atom = self.atom(b"_AURORA_CHOOSE_FILE")?;
        if ev.type_ == choose_file_atom {
            self.choose_file_mode = true;
            self.folder_path = folder_path_for(FolderMode::Home);
            self.folder_entries = folder_entries_for(FolderMode::Home, self.folder_sort);
            self.folder_selected = None;
            self.folder_scroll = 0;
            self.folder_front = true;
            self.conn.map_window(self.ui.folder)?;
            self.redraw_folder()?;
            self.raise_ui()?;
            return Ok(());
        }

        if self.handle_net_wm_state_message(&ev)? {
            return Ok(());
        }
        let Ok(cookie) = self.conn.intern_atom(false, b"_NET_WM_MOVERESIZE") else {
            return Ok(());
        };
        let Ok(atom) = cookie.reply() else {
            return Ok(());
        };
        if ev.type_ != atom.atom {
            self.handle_xdnd_message(ev)?;
            return Ok(());
        }
        let data = ev.data.as_data32();
        let Some(client) = self.client_or_ancestor_key_for(ev.window) else {
            return Ok(());
        };
        let root_x = data[0].min(i16::MAX as u32) as i16;
        let root_y = data[1].min(i16::MAX as u32) as i16;
        match data[2] {
            8 => {
                // The app is driving the move itself (CSD titlebar). Drop our own pending
                // titlebar-drag fallback so the two don't both call start_drag with
                // different offsets — that double-drag makes the frame jump and exposes the
                // black frame background inside the window during non-composited moves.
                self.pending_client_drag = None;
                if self.drag.is_none() {
                    self.start_drag(client, root_x, root_y)?;
                }
            }
            0 => self.start_resize(
                client,
                root_x,
                root_y,
                ResizeEdges {
                    top: true,
                    left: true,
                    ..ResizeEdges::default()
                },
            )?,
            1 => self.start_resize(
                client,
                root_x,
                root_y,
                ResizeEdges {
                    top: true,
                    ..ResizeEdges::default()
                },
            )?,
            2 => self.start_resize(
                client,
                root_x,
                root_y,
                ResizeEdges {
                    top: true,
                    right: true,
                    ..ResizeEdges::default()
                },
            )?,
            3 => self.start_resize(
                client,
                root_x,
                root_y,
                ResizeEdges {
                    right: true,
                    ..ResizeEdges::default()
                },
            )?,
            4 => self.start_resize(
                client,
                root_x,
                root_y,
                ResizeEdges {
                    right: true,
                    bottom: true,
                    ..ResizeEdges::default()
                },
            )?,
            5 => self.start_resize(
                client,
                root_x,
                root_y,
                ResizeEdges {
                    bottom: true,
                    ..ResizeEdges::default()
                },
            )?,
            6 => self.start_resize(
                client,
                root_x,
                root_y,
                ResizeEdges {
                    bottom: true,
                    left: true,
                    ..ResizeEdges::default()
                },
            )?,
            7 => self.start_resize(
                client,
                root_x,
                root_y,
                ResizeEdges {
                    left: true,
                    ..ResizeEdges::default()
                },
            )?,
            11 => self.end_drag()?,
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn handle_xdnd_message(&mut self, ev: ClientMessageEvent) -> AnyResult<()> {
        let xdnd_enter = self.atom(b"XdndEnter")?;
        let xdnd_position = self.atom(b"XdndPosition")?;
        let xdnd_drop = self.atom(b"XdndDrop")?;
        if ev.type_ == xdnd_enter {
            self.xdnd_source = Some(ev.data.as_data32()[0]);
        } else if ev.type_ == xdnd_position {
            let data = ev.data.as_data32();
            let source = data[0];
            self.xdnd_source = Some(source);
            let status = self.atom(b"XdndStatus")?;
            let action_copy = self.atom(b"XdndActionCopy")?;
            let msg =
                ClientMessageEvent::new(32, source, status, [self.ui.folder, 1, 0, 0, action_copy]);
            self.conn
                .send_event(false, source, EventMask::NO_EVENT, msg)?;
        } else if ev.type_ == xdnd_drop {
            let source = ev.data.as_data32()[0];
            self.xdnd_source = Some(source);
            let selection = self.atom(b"XdndSelection")?;
            let uri = self.atom(b"text/uri-list")?;
            self.conn
                .convert_selection(self.ui.folder, selection, uri, selection, CURRENT_TIME)?;
        }
        Ok(())
    }

    pub(crate) fn handle_selection_request(&self, ev: SelectionRequestEvent) -> AnyResult<()> {
        let selection = self.atom(b"XdndSelection")?;
        let uri = self.atom(b"text/uri-list")?;
        let mut property = x11rb::NONE;
        if ev.selection == selection && ev.target == uri {
            if let Some(path) = self.folder_drag.as_ref() {
                property = if ev.property == x11rb::NONE {
                    ev.target
                } else {
                    ev.property
                };
                let data = format!("{}\r\n", file_uri(path));
                self.conn.change_property8(
                    PropMode::REPLACE,
                    ev.requestor,
                    property,
                    uri,
                    data.as_bytes(),
                )?;
            }
        }
        let reply = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: ev.time,
            requestor: ev.requestor,
            selection: ev.selection,
            target: ev.target,
            property,
        };
        self.conn
            .send_event(false, ev.requestor, EventMask::NO_EVENT, reply)?;
        Ok(())
    }

    pub(crate) fn handle_selection_notify(&mut self, ev: SelectionNotifyEvent) -> AnyResult<()> {
        let selection = self.atom(b"XdndSelection")?;
        if ev.selection != selection || ev.property == x11rb::NONE {
            return Ok(());
        }
        let uri = self.atom(b"text/uri-list")?;
        let reply = self
            .conn
            .get_property(false, self.ui.folder, ev.property, uri, 0, 65535)?
            .reply()?;
        let text = String::from_utf8_lossy(&reply.value);
        let mut copied = 0usize;
        for line in text.lines() {
            if let Some(path) = path_from_file_uri(line.trim()) {
                if path.is_file() {
                    let dst = self.folder_path.join(path.file_name().unwrap_or_default());
                    if fs::copy(&path, dst).is_ok() {
                        copied += 1;
                    }
                }
            }
        }
        if copied > 0 {
            self.refresh_folder_entries();
            self.folder_info = Some(format!("Dropped {copied} file(s)"));
            self.redraw_folder()?;
        }
        if let Some(source) = self.xdnd_source {
            let finished = self.atom(b"XdndFinished")?;
            let action_copy = self.atom(b"XdndActionCopy")?;
            let msg = ClientMessageEvent::new(
                32,
                source,
                finished,
                [self.ui.folder, 1, action_copy, 0, 0],
            );
            self.conn
                .send_event(false, source, EventMask::NO_EVENT, msg)?;
        }
        Ok(())
    }

    pub(crate) fn handle_frame_click(&mut self, client: Window, ev: ButtonPressEvent) -> AnyResult<()> {
        let Some(info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        self.focus_window_at(client, ev.time)?;
        if let Some(edges) = resize_corner_edges_for_frame(
            &info,
            self.titlebar_height(&info),
            ev.event_x,
            ev.event_y,
        ) {
            self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
            self.pending_resize = Some(PendingResize {
                client,
                root_x: ev.root_x,
                root_y: ev.root_y,
                edges,
                pressed_at: Instant::now(),
            });
            return Ok(());
        }
        if resize_side_hint_for_frame(&info, ev.event_x) {
            self.set_topbar_notice(
                "Resize from the bottom-left or bottom-right corner: hold 2s, then drag",
                Duration::from_secs(3),
            )?;
        }
        let title_h = self.titlebar_height(&info);
        if title_h == 0 || ev.event_y >= i16::try_from(title_h).unwrap_or(i16::MAX) {
            self.conn.allow_events(Allow::REPLAY_POINTER, ev.time)?;
            return Ok(());
        }
        let x = ev.event_x;
        if (12..=26).contains(&x) {
            self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
            self.close_client(client)?;
        } else if (34..=50).contains(&x) {
            self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
            self.minimize_client(client)?;
        } else if (57..=73).contains(&x) {
            self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
            self.toggle_maximize_client(client)?;
        } else {
            let title = compact(
                &self.window_title(info.window),
                ((info.width / 9).max(8)) as usize,
            );
            let title_w = measure_text(&self.bold, &title, 13.0);
            self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
            if i32::from(x) >= 88 && i32::from(x) <= 96 + title_w {
                self.toggle_title_menu(client)?;
            } else {
                self.start_drag(client, ev.root_x, ev.root_y)?;
            }
        }
        Ok(())
    }

    pub(crate) fn handle_client_click(&mut self, client: Window, ev: ButtonPressEvent) -> AnyResult<()> {
        let Some(info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        let title_h = self.titlebar_height(&info) as i16;
        let client_x = ev.root_x.saturating_sub(info.x);
        let client_y = ev.root_y.saturating_sub(info.y).saturating_sub(title_h);
        if let Some(edges) = resize_corner_edges_for_client(&info, client_x, client_y) {
            self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
            self.pending_resize = Some(PendingResize {
                client,
                root_x: ev.root_x,
                root_y: ev.root_y,
                edges,
                pressed_at: Instant::now(),
            });
        } else {
            if resize_side_hint_for_client(&info, client_x) {
                self.set_topbar_notice(
                    "Resize from the bottom-left or bottom-right corner: hold 2s, then drag",
                    Duration::from_secs(3),
                )?;
            }
            // A client-side decorated window owns its titlebar interaction. GTK,
            // Chromium and Firefox send _NET_WM_MOVERESIZE when a native drag starts;
            // arming a second WM drag here races that request and can move the client
            // inside its frame instead of moving the whole window.
            self.focus_window_at(client, ev.time)?;
            self.conn.flush()?;
            self.conn.allow_events(Allow::REPLAY_POINTER, ev.time)?;
        }
        Ok(())
    }

    pub(crate) fn handle_root_button_press(&mut self, ev: ButtonPressEvent) -> AnyResult<()> {
        if ev.detail != 1 {
            self.conn.allow_events(Allow::REPLAY_POINTER, ev.time)?;
            return Ok(());
        }
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        if pointer.root_y >= 0
            && pointer.root_y < TOPBAR_HEIGHT as i16
            && self.handle_topbar_press_x(i32::from(pointer.root_x))?
        {
            self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
            return Ok(());
        }
        let target = pointer.child;
        if let Some(client) = self.client_or_ancestor_key_for(target) {
            self.focus_window_at(client, ev.time)?;
            if let Some(info) = self.clients.get(&client).copied() {
                let title_h = self.titlebar_height(&info) as i16;
                let frame_x = pointer.root_x.saturating_sub(info.x);
                let frame_y = pointer.root_y.saturating_sub(info.y);
                if let Some(edges) =
                    resize_corner_edges_for_frame(&info, title_h as u16, frame_x, frame_y)
                {
                    self.pending_resize = Some(PendingResize {
                        client,
                        root_x: pointer.root_x,
                        root_y: pointer.root_y,
                        edges,
                        pressed_at: Instant::now(),
                    });
                    self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
                    return Ok(());
                }
                if resize_side_hint_for_frame(&info, frame_x) {
                    self.set_topbar_notice(
                        "Resize from the bottom-left or bottom-right corner: hold 2s, then drag",
                        Duration::from_secs(3),
                    )?;
                }
                // Do not synthesize a top-strip drag for CSD windows. Their own
                // titlebar sends _NET_WM_MOVERESIZE and remains fully interactive.
            }
        }
        self.conn.allow_events(Allow::REPLAY_POINTER, ev.time)?;
        Ok(())
    }

    pub(crate) fn handle_topbar_press_x(&mut self, x: i32) -> AnyResult<bool> {
        let controls = self.topbar_controls();
        let brand_x = 24;
        let aurora_width = measure_text(&self.bold, "Aurora", 16.0);
        let aurora_end = brand_x + 23 + aurora_width;
        if (0..=aurora_end).contains(&x) {
            self.hide_clipboard_menu()?;
            self.toggle_aurora_menu()?;
            return Ok(true);
        }
        let workspace = (0..self.workspace_count).find(|&index| {
            (self.workspace_x(index)..=self.workspace_x(index) + WORKSPACE_SIZE).contains(&x)
        });
        if let Some(workspace) = workspace {
            self.hide_aurora_menu()?;
            self.hide_clipboard_menu()?;
            self.switch_workspace(workspace)?;
        } else if (self.add_workspace_x()..=self.add_workspace_x() + WORKSPACE_SIZE).contains(&x) {
            self.hide_aurora_menu()?;
            self.hide_clipboard_menu()?;
            self.add_workspace()?;
        } else if (controls.clipboard_x - TOPBAR_ICON_HIT_RADIUS
            ..=controls.clipboard_x + TOPBAR_ICON_HIT_RADIUS)
            .contains(&x)
        {
            self.hide_aurora_menu()?;
            self.toggle_clipboard_menu()?;
        } else if (controls.screenshot_x - TOPBAR_ICON_HIT_RADIUS
            ..=controls.screenshot_x + TOPBAR_ICON_HIT_RADIUS)
            .contains(&x)
        {
            self.hide_aurora_menu()?;
            self.hide_clipboard_menu()?;
            if self.screenshot_mode {
                self.capture_screenshot(None)?;
            } else {
                self.pending_screenshot_button = Some(PendingScreenshotButton {
                    pressed_at: Instant::now(),
                });
                self.toggle_screenshot_mode()?;
            }
        } else if (controls.display_x - TOPBAR_ICON_HIT_RADIUS
            ..=controls.display_x + TOPBAR_ICON_HIT_RADIUS)
            .contains(&x)
        {
            self.hide_aurora_menu()?;
            self.hide_clipboard_menu()?;
            self.open_settings_tab(SettingsTab::Display)?;
        } else if (controls.audio_x - TOPBAR_ICON_HIT_RADIUS
            ..=controls.audio_x + TOPBAR_ICON_HIT_RADIUS)
            .contains(&x)
        {
            self.hide_aurora_menu()?;
            self.hide_clipboard_menu()?;
            self.open_settings_tab(SettingsTab::Audio)?;
        } else if (controls.network_x - TOPBAR_ICON_HIT_RADIUS
            ..=controls.network_x + TOPBAR_ICON_HIT_RADIUS)
            .contains(&x)
        {
            self.hide_aurora_menu()?;
            self.hide_clipboard_menu()?;
            self.open_settings_tab(SettingsTab::Network)?;
        } else if (controls.battery_left..=controls.battery_right).contains(&x) {
            self.hide_aurora_menu()?;
            self.hide_clipboard_menu()?;
            self.open_settings_tab(SettingsTab::Power)?;
        } else {
            self.hide_aurora_menu()?;
            self.hide_clipboard_menu()?;
            return Ok(false);
        }
        Ok(true)
    }

    pub(crate) fn grab_root_button1(&self) -> AnyResult<()> {
        let res = self
            .conn
            .grab_button(
                false,
                self.root,
                EventMask::BUTTON_PRESS,
                GrabMode::SYNC,
                GrabMode::ASYNC,
                x11rb::NONE,
                x11rb::NONE,
                ButtonIndex::M1,
                ModMask::ANY,
            )?
            .check();
        if let Err(ReplyError::X11Error(ref err)) = res {
            if err.error_kind == ErrorKind::Access {
                return Ok(());
            }
        }
        res?;
        Ok(())
    }

    pub(crate) fn grab_alt_tab(&self) -> AnyResult<()> {
        let Some(tab_keycode) = self.keycode_for_keysym(0xff09)? else {
            return Ok(());
        };
        let lock = ModMask::LOCK;
        let num_lock = ModMask::M2;

        // Grab Alt + Tab and Alt + Shift + Tab
        for modifiers in [
            ModMask::M1,
            ModMask::M1 | ModMask::SHIFT,
            ModMask::M1 | lock,
            ModMask::M1 | ModMask::SHIFT | lock,
            ModMask::M1 | num_lock,
            ModMask::M1 | ModMask::SHIFT | num_lock,
            ModMask::M1 | lock | num_lock,
            ModMask::M1 | ModMask::SHIFT | lock | num_lock,
        ] {
            let _ = self.conn.grab_key(
                false,
                self.root,
                modifiers,
                tab_keycode,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            );
        }

        Ok(())
    }

    pub(crate) fn grab_workspace_keys(&self) -> AnyResult<()> {
        let Some(left_keycode) = self.keycode_for_keysym(0xff51)? else {
            return Ok(());
        };
        let Some(right_keycode) = self.keycode_for_keysym(0xff53)? else {
            return Ok(());
        };
        let lock = ModMask::LOCK;
        let num_lock = ModMask::M2;
        let super_mod = ModMask::M4; // Mod4 is standard for Super/Win

        for keycode in [left_keycode, right_keycode] {
            for modifiers in [
                super_mod,
                super_mod | lock,
                super_mod | num_lock,
                super_mod | lock | num_lock,
            ] {
                let _ = self.conn.grab_key(
                    false,
                    self.root,
                    modifiers,
                    keycode,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                );
            }
        }
        Ok(())
    }

    pub(crate) fn grab_command_paste(&self) -> AnyResult<()> {
        let Some(v_keycode) = self.keycode_for_keysym(0x76)? else {
            return Ok(());
        };
        for modifiers in [
            ModMask::M4,
            ModMask::M4 | ModMask::LOCK,
            ModMask::M4 | ModMask::M2,
            ModMask::M4 | ModMask::LOCK | ModMask::M2,
        ] {
            let _ = self.conn.grab_key(
                false,
                self.root,
                modifiers,
                v_keycode,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            );
        }
        Ok(())
    }

    pub(crate) fn keycode_for_keysym(&self, target: u32) -> AnyResult<Option<u8>> {
        let setup = self.conn.setup();
        let min = setup.min_keycode;
        let max = setup.max_keycode;
        let count = max.saturating_sub(min).saturating_add(1);
        let mapping = self.conn.get_keyboard_mapping(min, count)?.reply()?;
        for (idx, keysyms) in mapping
            .keysyms
            .chunks(mapping.keysyms_per_keycode as usize)
            .enumerate()
        {
            if keysyms.contains(&target) {
                return Ok(Some(min.saturating_add(idx as u8)));
            }
        }
        Ok(None)
    }

}
