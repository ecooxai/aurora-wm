//! Press-and-hold menus on the topbar and dock.
//!
//! A tap on a workspace or running-app icon does what it always did (on
//! release). Holding for HOLD_DELAY opens a menu while the finger is still
//! down: a workspace menu (new workspace, close workspace with confirmation)
//! or a running-app menu (force close, restart, CPU priority).

use std::process::Command;
use std::time::{Duration, Instant};

use x11rb::CURRENT_TIME;
use x11rb::protocol::xproto::ConnectionExt as XprotoConnectionExt;
use x11rb::protocol::xproto::*;

use crate::*;
use crate::app_control::{self, Priority};
use crate::canvas::*;
use crate::model::*;
use crate::procutil::*;
use crate::textutil::*;

pub(crate) const HOLD_DELAY: Duration = Duration::from_millis(600);
/// Pointer travel (px) that turns a press into a drag and cancels the hold.
const HOLD_SLOP: i32 = 24;
pub(crate) const MENU_WIDTH: u16 = 232;
const MENU_W: i32 = MENU_WIDTH as i32;
const ROW_H: i32 = 36;
const PAD: i32 = 8;
const DANGER: Color = Color::rgb(196, 64, 74);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HoldTarget {
    Workspace(usize),
    Task(Window),
    /// Topbar clipboard icon: a tap toggles the menu, a long press pastes.
    Clipboard,
}

pub(crate) struct PendingHold {
    pub(crate) target: HoldTarget,
    pub(crate) pressed_at: Instant,
    pub(crate) root_x: i32,
    pub(crate) root_y: i32,
    /// On the topbar (menu opens below it) rather than the dock (menu opens above it).
    pub(crate) topbar: bool,
    /// The hold already opened a menu or was cancelled by moving away; the release does nothing.
    pub(crate) done: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct OpenHoldMenu {
    pub(crate) target: HoldTarget,
    pub(crate) anchor_x: i32,
    pub(crate) topbar: bool,
}

#[derive(Clone, Copy)]
enum HoldAction {
    NewWorkspace,
    CloseWorkspace(usize),
    ForceClose(Window),
    Restart(Window),
    SetPriority(Window, Priority),
}

struct MenuRow {
    label: String,
    color: Color,
    check: Option<bool>,
    action: Option<HoldAction>,
    rule_above: bool,
    header: bool,
}

impl MenuRow {
    fn action(label: &str, color: Color, action: HoldAction) -> MenuRow {
        MenuRow {
            label: label.to_string(),
            color,
            check: None,
            action: Some(action),
            rule_above: false,
            header: false,
        }
    }
}

impl Aurora {
    /// Topbar press: starts a hold if it lands on a workspace or running-app icon.
    pub(crate) fn begin_topbar_hold(&mut self, root_x: i32) -> AnyResult<bool> {
        let controls = self.topbar_controls();
        if topbar_icon_shown(controls.clipboard_x)
            && (root_x - controls.clipboard_x).abs() <= TOPBAR_ICON_HIT_RADIUS
        {
            self.start_hold(HoldTarget::Clipboard, root_x, 20, true)?;
            return Ok(true);
        }
        let workspace = (0..self.workspace_count).find(|&index| {
            self.workspace_x(index) + WORKSPACE_SIZE < controls.tray_left - TOPBAR_ICON_HIT_RADIUS
                && (self.workspace_x(index)..=self.workspace_x(index) + WORKSPACE_SIZE)
                    .contains(&root_x)
        });
        let target = match workspace {
            Some(index) => HoldTarget::Workspace(index),
            None if self.dock_is_merged() => match self.topbar_task_at(root_x) {
                Some(client) => HoldTarget::Task(client),
                None => return Ok(false),
            },
            None => return Ok(false),
        };
        self.start_hold(target, root_x, 20, true)?;
        Ok(true)
    }

    /// Dock press (dock-local coordinates): starts a hold on a running-app icon.
    pub(crate) fn begin_dock_hold(&mut self, x: i32, y: i32) -> AnyResult<bool> {
        let Some(client) = self.dock_task_at(x, y) else {
            return Ok(false);
        };
        let (dock_x, dock_y, _, _) = self.dock_geometry();
        self.start_hold(
            HoldTarget::Task(client),
            x + i32::from(dock_x),
            y + i32::from(dock_y),
            false,
        )?;
        Ok(true)
    }

    fn topbar_task_at(&self, x: i32) -> Option<Window> {
        let relative = x - self.topbar_tasks_x();
        if relative < 0 {
            return None;
        }
        let slot = (relative / TOPBAR_TASK_STRIDE) as usize;
        // Slot 0 is the Files launcher; slot n maps to task n-1.
        if slot == 0
            || slot - 1 >= self.dock_task_limit()
            || relative % TOPBAR_TASK_STRIDE >= TOPBAR_TASK_SIZE
        {
            return None;
        }
        self.task_client_windows().get(slot - 1).copied()
    }

    fn dock_task_at(&self, x: i32, y: i32) -> Option<Window> {
        if x < 0 || y < 0 || y >= DOCK_ICON_SIZE || x % DOCK_STRIDE >= DOCK_ICON_SIZE {
            return None;
        }
        let slot = (x / DOCK_STRIDE) as usize;
        let pinned = self.dock_pinned_count();
        if slot < pinned || slot - pinned >= self.dock_task_limit() {
            return None;
        }
        self.task_client_windows().get(slot - pinned).copied()
    }

    fn start_hold(&mut self, target: HoldTarget, root_x: i32, root_y: i32, topbar: bool) -> AnyResult<()> {
        self.hide_aurora_menu()?;
        // The clipboard tap toggles the menu on release, so it must stay open until then.
        if target != HoldTarget::Clipboard {
            self.hide_clipboard_menu()?;
        }
        self.hide_app_menu()?;
        self.pending_hold = Some(PendingHold {
            target,
            pressed_at: Instant::now(),
            root_x,
            root_y,
            topbar,
            done: false,
        });
        self.conn
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
            .reply()?;
        Ok(())
    }

    /// Called from the main loop: opens the menu once the hold is long enough.
    pub(crate) fn poll_hold(&mut self) -> AnyResult<bool> {
        let Some(pending) = self.pending_hold.as_ref() else {
            return Ok(false);
        };
        if pending.done || pending.pressed_at.elapsed() < HOLD_DELAY {
            return Ok(false);
        }
        let (root_x, root_y, target, topbar) = (pending.root_x, pending.root_y, pending.target, pending.topbar);
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        let (px, py) = (i32::from(pointer.root_x), i32::from(pointer.root_y));
        if let Some(pending) = self.pending_hold.as_mut() {
            pending.done = true;
        }
        if (px - root_x).abs() > HOLD_SLOP || (py - root_y).abs() > HOLD_SLOP {
            return Ok(false);
        }
        if target == HoldTarget::Clipboard {
            self.paste_from_clipboard_icon()?;
            return Ok(true);
        }
        self.show_hold_menu(OpenHoldMenu { target, anchor_x: root_x, topbar })?;
        Ok(true)
    }

    /// Button release: a tap runs the original action; a finished hold or a drag does nothing.
    pub(crate) fn release_hold(&mut self, ev: ButtonReleaseEvent) -> AnyResult<()> {
        if ev.detail != 1 {
            return Ok(());
        }
        let Some(pending) = self.pending_hold.take() else {
            return Ok(());
        };
        self.conn.ungrab_pointer(CURRENT_TIME)?;
        let moved = (i32::from(ev.root_x) - pending.root_x).abs() > HOLD_SLOP
            || (i32::from(ev.root_y) - pending.root_y).abs() > HOLD_SLOP;
        if pending.done || moved {
            return Ok(());
        }
        match pending.target {
            HoldTarget::Workspace(index) => self.switch_workspace(index)?,
            HoldTarget::Task(client) => {
                self.hide_app_menu()?;
                self.hide_dock_more_menu()?;
                self.handle_task_icon_click(client)?;
            }
            HoldTarget::Clipboard => self.toggle_clipboard_menu()?,
        }
        Ok(())
    }

    fn hold_menu_rows(&self, open: OpenHoldMenu) -> Vec<MenuRow> {
        match open.target {
            // The clipboard hold never opens a menu; its press is a tap or a paste.
            HoldTarget::Clipboard => Vec::new(),
            HoldTarget::Workspace(index) => {
                let can_close = self.workspace_count > 1;
                vec![
                    MenuRow::action("New workspace", INK, HoldAction::NewWorkspace),
                    MenuRow {
                        label: format!("Close workspace {}", index + 1),
                        color: if can_close { DANGER } else { MUTED },
                        check: None,
                        action: can_close.then_some(HoldAction::CloseWorkspace(index)),
                        rule_above: false,
                        header: false,
                    },
                ]
            }
            HoldTarget::Task(client) => {
                let class = self.class_key(client);
                let current = self
                    .app_priorities
                    .get(&class)
                    .copied()
                    .unwrap_or(Priority::Normal);
                let mut rows = vec![MenuRow {
                    label: compact(&self.window_title(client), 28),
                    color: MUTED,
                    check: None,
                    action: None,
                    rule_above: false,
                    header: true,
                }];
                rows.push(MenuRow::action("Force close", DANGER, HoldAction::ForceClose(client)));
                rows.push(MenuRow::action("Restart", INK, HoldAction::Restart(client)));
                for (index, level) in Priority::ALL.into_iter().enumerate() {
                    rows.push(MenuRow {
                        label: format!("Priority: {}", level.label()),
                        color: INK,
                        check: Some(current == level),
                        action: (!class.is_empty()).then_some(HoldAction::SetPriority(client, level)),
                        rule_above: index == 0,
                        header: false,
                    });
                }
                rows
            }
        }
    }

    pub(crate) fn hold_menu_geometry(&self, open: OpenHoldMenu) -> (i32, i32, i32, i32) {
        let height = PAD * 2 + self.hold_menu_rows(open).len() as i32 * ROW_H;
        let x = (open.anchor_x - MENU_W / 2).clamp(4, i32::from(self.screen_width) - MENU_W - 4);
        let y = if open.topbar {
            i32::from(TOPBAR_HEIGHT) + 4
        } else {
            let (_, dock_y, _, _) = self.dock_geometry();
            i32::from(dock_y) - height - 8
        };
        (x, y, MENU_W, height)
    }

    pub(crate) fn show_hold_menu(&mut self, open: OpenHoldMenu) -> AnyResult<()> {
        self.hide_hold_menu()?;
        self.hold_menu_open = Some(open);
        let (x, y, w, h) = self.hold_menu_geometry(open);
        self.conn.configure_window(
            self.ui.hold_menu,
            &ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .width(w as u32)
                .height(h as u32)
                .stack_mode(StackMode::ABOVE),
        )?;
        self.conn.map_window(self.ui.hold_menu)?;
        // The window must be viewable before drawing into it.
        self.conn.sync()?;
        self.redraw_hold_menu()
    }

    pub(crate) fn hide_hold_menu(&mut self) -> AnyResult<()> {
        if self.hold_menu_open.take().is_some() {
            self.conn.unmap_window(self.ui.hold_menu)?;
        }
        Ok(())
    }

    pub(crate) fn redraw_hold_menu(&self) -> AnyResult<()> {
        let Some(open) = self.hold_menu_open else {
            return Ok(());
        };
        let (_, _, w, h) = self.hold_menu_geometry(open);
        let mut c = Canvas::new(w as u16, h as u16, Color::rgb(247, 252, 255));
        c.draw_round_rect(0, 0, w, h, 12, Color::rgb(250, 254, 255));
        c.draw_round_rect(0, 0, w, h, 12, Color::rgba(176, 198, 210, 60));
        for (index, row) in self.hold_menu_rows(open).iter().enumerate() {
            let y = PAD + index as i32 * ROW_H;
            if row.rule_above {
                c.draw_rect(10, y - 2, w - 20, 1, Color::rgba(176, 198, 210, 90));
            }
            let font = if row.header { &self.bold } else { &self.regular };
            let text_x = if row.check.is_some() { 40 } else { 16 };
            c.draw_text(font, &row.label, text_x, y + 8, 13.0, row.color);
            match row.check {
                Some(true) => {
                    c.draw_round_rect(12, y + 8, 16, 16, 5, Color::rgba(29, 145, 137, 210));
                    c.draw_line(15, y + 16, 19, y + 20, 2, Color::rgb(255, 255, 255));
                    c.draw_line(19, y + 20, 25, y + 11, 2, Color::rgb(255, 255, 255));
                }
                Some(false) => {
                    c.draw_round_rect(12, y + 8, 16, 16, 5, Color::rgba(176, 198, 210, 140));
                }
                None => {}
            }
        }
        self.upload_canvas(self.ui.hold_menu, &c)
    }

    pub(crate) fn handle_hold_menu_click(&mut self, y: i32) -> AnyResult<()> {
        let Some(open) = self.hold_menu_open else {
            return Ok(());
        };
        if y < PAD {
            return Ok(());
        }
        let row = ((y - PAD) / ROW_H) as usize;
        let rows = self.hold_menu_rows(open);
        let Some(action) = rows.get(row).and_then(|item| item.action) else {
            return Ok(());
        };
        self.hide_hold_menu()?;
        match action {
            HoldAction::NewWorkspace => self.add_workspace()?,
            HoldAction::CloseWorkspace(index) => self.show_workspace_close_confirm(index)?,
            HoldAction::ForceClose(client) => self.force_close_client(client)?,
            HoldAction::Restart(client) => self.restart_client(client)?,
            HoldAction::SetPriority(client, level) => self.set_client_priority(client, level)?,
        }
        Ok(())
    }

    /// Confirmation before closing a workspace. Its windows move to a neighbour.
    pub(crate) fn show_workspace_close_confirm(&mut self, index: usize) -> AnyResult<()> {
        self.hide_close_confirm()?;
        self.confirm_workspace = Some(index);
        let (x, y, w, h) = self.confirm_geometry();
        self.conn.configure_window(
            self.ui.confirm_dialog,
            &ConfigureWindowAux::new()
                .x(i32::from(x))
                .y(i32::from(y))
                .width(u32::from(w))
                .height(u32::from(h))
                .stack_mode(StackMode::ABOVE),
        )?;
        self.conn.map_window(self.ui.confirm_dialog)?;
        self.redraw_close_confirm()
    }

    pub(crate) fn redraw_workspace_confirm(&self, index: usize) -> AnyResult<()> {
        let (_, _, w, h) = self.confirm_geometry();
        let neighbour = workspace_neighbour(index, self.workspace_count);
        let moving = self
            .clients
            .values()
            .filter(|info| info.workspace == index && !info.sticky)
            .count();
        let mut c = Canvas::new(w, h, Color::rgb(247, 252, 255));
        c.draw_round_rect(0, 0, i32::from(w), i32::from(h), 14, Color::rgb(250, 254, 255));
        c.draw_round_rect(0, 0, i32::from(w), i32::from(h), 14, Color::rgba(176, 198, 210, 70));
        c.draw_text(&self.bold, &format!("Close workspace {}?", index + 1), 22, 18, 17.0, INK);
        let detail = match moving {
            0 => "It has no windows.".to_string(),
            1 => format!("1 window moves to workspace {}.", neighbour + 1),
            n => format!("{n} windows move to workspace {}.", neighbour + 1),
        };
        c.draw_text(&self.regular, &detail, 22, 48, 13.0, MUTED);
        c.draw_round_rect(i32::from(w) - 210, 96, 92, 34, 10, Color::rgba(224, 236, 242, 220));
        c.draw_text_center(&self.bold, "Cancel", i32::from(w) - 164, 104, 13.0, INK);
        c.draw_round_rect(i32::from(w) - 108, 96, 86, 34, 10, DANGER);
        c.draw_text_center(&self.bold, "Close", i32::from(w) - 65, 104, 13.0, Color::rgb(255, 255, 255));
        self.upload_canvas(self.ui.confirm_dialog, &c)
    }

    pub(crate) fn handle_workspace_confirm_click(&mut self, index: usize, x: i32, y: i32) -> AnyResult<()> {
        let (_, _, w, _) = self.confirm_geometry();
        let w = i32::from(w);
        if (96..=130).contains(&y) {
            if (w - 108..=w - 22).contains(&x) {
                self.hide_close_confirm()?;
                self.close_workspace(index)?;
            } else if (w - 210..=w - 118).contains(&x) {
                self.hide_close_confirm()?;
            }
        }
        Ok(())
    }

    /// Window id of the client's process (_NET_WM_PID), if it has one and it is not the WM.
    fn client_pid(&self, client: Window) -> Option<i32> {
        let atom = self.atom(b"_NET_WM_PID").ok()?;
        let reply = self
            .conn
            .get_property(false, client, atom, AtomEnum::CARDINAL, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        let pid = reply.value32()?.next()? as i32;
        (pid > 1 && pid as u32 != std::process::id()).then_some(pid)
    }

    /// Lowercase WM_CLASS class name (the second NUL-separated field), used as the priority key.
    pub(crate) fn class_key(&self, client: Window) -> String {
        self.window_class(client)
            .split('\0')
            .filter(|part| !part.is_empty())
            .last()
            .unwrap_or("")
            .to_string()
    }

    fn force_close_client(&mut self, client: Window) -> AnyResult<()> {
        match self.client_pid(client) {
            Some(pid) => {
                app_control::kill_tree(pid);
                self.flash_notice("Force closed")?;
            }
            None => {
                // No process to signal: drop the X connection instead.
                self.conn.kill_client(client)?;
                self.flash_notice("Force closed")?;
            }
        }
        Ok(())
    }

    fn restart_client(&mut self, client: Window) -> AnyResult<()> {
        let Some(pid) = self.client_pid(client) else {
            return self.flash_notice("Cannot restart: the app did not report its process");
        };
        let Some((argv, cwd)) = app_control::command_line(pid) else {
            return self.flash_notice("Cannot restart: no command line");
        };
        app_control::kill_tree(pid);
        app_control::wait_for_exit(pid, Duration::from_millis(1500));
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        if cwd.is_dir() {
            cmd.current_dir(&cwd);
        }
        spawn_detached(cmd);
        self.flash_notice("Restarted")
    }

    /// Sets the priority for every window of this application and remembers it by WM_CLASS.
    fn set_client_priority(&mut self, client: Window, level: Priority) -> AnyResult<()> {
        let class = self.class_key(client);
        if class.is_empty() {
            return self.flash_notice("Priority needs a WM_CLASS");
        }
        self.app_priorities.insert(class.clone(), level);
        if let Err(err) = app_control::save_priorities(&self.app_priorities) {
            eprintln!("aurora-wm: saving app priorities: {err}");
        }
        let mut failure = None;
        let windows: Vec<Window> = self.clients.keys().copied().collect();
        for window in windows {
            if self.class_key(window) != class {
                continue;
            }
            if let Some(pid) = self.client_pid(window) {
                let tree = app_control::process_tree(pid);
                if let Err(err) = app_control::apply_nice(&tree, level.nice()) {
                    failure = Some(err);
                }
            }
        }
        match failure {
            None => self.flash_notice(format!("Priority {} for {class}", level.label())),
            Some(err) => self.flash_notice(err),
        }
    }

    /// Applies the remembered priority of this window's application (called on manage).
    pub(crate) fn apply_saved_priority(&self, client: Window) {
        let class = self.class_key(client);
        let Some(level) = self.app_priorities.get(&class).copied() else {
            return;
        };
        if let Some(pid) = self.client_pid(client) {
            let tree = app_control::process_tree(pid);
            if let Err(err) = app_control::apply_nice(&tree, level.nice()) {
                eprintln!("aurora-wm: priority for {class}: {err}");
            }
        }
    }

    fn flash_notice(&mut self, message: impl Into<String>) -> AnyResult<()> {
        self.topbar_notice = Some((message.into(), Instant::now() + Duration::from_secs(3)));
        self.redraw_topbar()
    }
}

/// The workspace that receives the windows of a closed one (the next, or the previous for the last).
pub(crate) fn workspace_neighbour(index: usize, count: usize) -> usize {
    if index + 1 < count { index + 1 } else { index.saturating_sub(1) }
}
