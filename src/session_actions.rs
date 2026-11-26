//! Session actions (log out, soft reboot, reboot, suspend, shut down) and the
//! full network restart, each behind a confirmation that shows the exact
//! command it runs.

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


/// Root helper for the network restart; tablet may run it via sudo (see
/// /etc/sudoers.d/aurora-net-restart on the device).
pub(crate) const NET_RESTART_HELPER: &str = "/usr/local/libexec/aurora-net-restart";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionAction {
    Logout,
    SoftReboot,
    Reboot,
    Suspend,
    Shutdown,
    NetworkRestart,
}

pub(crate) const SESSION_MENU: [SessionAction; 5] = [
    SessionAction::Logout,
    SessionAction::SoftReboot,
    SessionAction::Reboot,
    SessionAction::Suspend,
    SessionAction::Shutdown,
];

impl SessionAction {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Logout => "Log out",
            Self::SoftReboot => "Soft reboot",
            Self::Reboot => "Reboot",
            Self::Suspend => "Suspend",
            Self::Shutdown => "Shut down",
            Self::NetworkRestart => "Restart network",
        }
    }

    pub(crate) fn hint(self) -> &'static str {
        match self {
            Self::Logout => "End this session",
            Self::SoftReboot => "Restart userspace, keep kernel",
            Self::Reboot => "Restart the iPad",
            Self::Suspend => "Sleep until a button press",
            Self::Shutdown => "Power off",
            Self::NetworkRestart => "",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Logout => "Log out?",
            Self::SoftReboot => "Soft reboot?",
            Self::Reboot => "Reboot the iPad?",
            Self::Suspend => "Suspend now?",
            Self::Shutdown => "Shut down the iPad?",
            Self::NetworkRestart => "Restart all networking?",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            Self::Logout => "All apps close; the desktop then starts again, fresh.",
            Self::SoftReboot => "Every service and app restarts; the kernel keeps running.",
            Self::Reboot => "All apps close and the iPad restarts.",
            Self::Suspend => "Press Power, Home or a Volume key to wake.",
            Self::Shutdown => "All apps close and the iPad powers off.",
            Self::NetworkRestart => "Wi-Fi and Ethernet drop for a few seconds: all interfaces \
                go down, the Wi-Fi driver is reloaded and the network services restart.",
        }
    }

    fn button(self) -> &'static str {
        match self {
            Self::Logout => "Log out",
            Self::SoftReboot => "Soft reboot",
            Self::Reboot => "Reboot",
            Self::Suspend => "Suspend",
            Self::Shutdown => "Shut down",
            Self::NetworkRestart => "Restart",
        }
    }

    /// The exact shell command that runs.
    pub(crate) fn command(self) -> String {
        match self {
            // Ending X ends this desktop; aurora-x11-session then starts a
            // fresh one. (loginctl terminate-session would end that loop too
            // and leave greetd on its "session failed" screen.)
            Self::Logout => "pkill -TERM -x -u \"$(id -u)\" Xorg".to_string(),
            Self::SoftReboot => "systemctl soft-reboot".to_string(),
            Self::Reboot => "systemctl reboot".to_string(),
            Self::Suspend => "systemctl suspend".to_string(),
            Self::Shutdown => "systemctl poweroff".to_string(),
            Self::NetworkRestart => format!("sudo -n {NET_RESTART_HELPER}"),
        }
    }

    /// What a helper runs, for commands that are a helper (shown below it).
    fn steps(self) -> &'static [&'static str] {
        match self {
            Self::NetworkRestart => &[
                "nmcli networking off",
                "ip link set <each interface> down",
                "modprobe -r brcmfmac_wcc brcmfmac; modprobe brcmfmac",
                "systemctl restart NetworkManager (+ iwd/wpa_supplicant if used)",
                "nmcli networking on; nmcli radio wifi on",
            ],
            _ => &[],
        }
    }

    fn danger(self) -> bool {
        !matches!(self, Self::Suspend | Self::NetworkRestart)
    }
}

/// Greedy word wrap to at most `max` pixels per line.
fn wrap_px(font: &Font<'static>, text: &str, size: f32, max: i32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
        if line.is_empty() || measure_text(font, &candidate, size) <= max {
            line = candidate;
        } else {
            lines.push(std::mem::take(&mut line));
            line = word.to_string();
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

const CMD_CONFIRM_W: u16 = 480;
const CMD_PAD: i32 = 22;
const CMD_LINE: i32 = 18;
const CMD_BUTTON_H: i32 = 34;

struct CommandConfirmLayout {
    detail: Vec<String>,
    command: Vec<String>,
    steps: Vec<String>,
    box_y: i32,
    box_h: i32,
    buttons_y: i32,
    height: i32,
}

impl Aurora {
    fn command_confirm_layout(&self, action: SessionAction) -> CommandConfirmLayout {
        let text_w = i32::from(CMD_CONFIRM_W) - 2 * CMD_PAD;
        let detail = wrap_px(&self.regular, action.detail(), 13.0, text_w);
        let command = wrap_px(&self.bold, &action.command(), 13.0, text_w - 24);
        let steps = action
            .steps()
            .iter()
            .flat_map(|step| wrap_px(&self.regular, &format!("• {step}"), 11.0, text_w - 24))
            .collect::<Vec<_>>();
        let box_y = 50 + detail.len() as i32 * CMD_LINE + 30;
        let mut box_h = 12 + command.len() as i32 * CMD_LINE;
        if !steps.is_empty() {
            box_h += 20 + steps.len() as i32 * 16;
        }
        box_h += 8;
        let buttons_y = box_y + box_h + 18;
        CommandConfirmLayout {
            detail,
            command,
            steps,
            box_y,
            box_h,
            buttons_y,
            height: buttons_y + CMD_BUTTON_H + 18,
        }
    }

    pub(crate) fn command_confirm_geometry(&self, action: SessionAction) -> (i16, i16, u16, u16) {
        let h = self.command_confirm_layout(action).height as u16;
        let w = CMD_CONFIRM_W.min(self.screen_width.saturating_sub(16));
        let x = ((self.screen_width.saturating_sub(w)) / 2) as i16;
        let y = ((self.screen_height.saturating_sub(h)) / 3) as i16;
        (x, y, w, h)
    }

    pub(crate) fn show_command_confirm(&mut self, action: SessionAction) -> AnyResult<()> {
        self.hide_close_confirm()?;
        self.confirm_command = Some(action);
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
        self.redraw_command_confirm()?;
        Ok(())
    }

    pub(crate) fn redraw_command_confirm(&self) -> AnyResult<()> {
        let Some(action) = self.confirm_command else {
            return Ok(());
        };
        let layout = self.command_confirm_layout(action);
        let (_, _, w, h) = self.confirm_geometry();
        let wi = i32::from(w);
        let mut c = Canvas::new(w, h, Color::rgb(247, 252, 255));
        c.draw_round_rect(0, 0, wi, i32::from(h), 14, Color::rgb(250, 254, 255));
        c.draw_round_rect(0, 0, wi, i32::from(h), 14, Color::rgba(176, 198, 210, 70));
        c.draw_text(&self.bold, action.title(), CMD_PAD, 18, 17.0, INK);
        for (i, line) in layout.detail.iter().enumerate() {
            c.draw_text(&self.regular, line, CMD_PAD, 50 + i as i32 * CMD_LINE, 13.0, MUTED);
        }
        c.draw_text(&self.bold, "Command", CMD_PAD, layout.box_y - 20, 11.0, MINT_DARK);
        c.draw_round_rect(
            CMD_PAD - 4,
            layout.box_y,
            wi - 2 * CMD_PAD + 8,
            layout.box_h,
            8,
            Color::rgb(33, 42, 52),
        );
        let mut ty = layout.box_y + 8;
        for line in &layout.command {
            c.draw_text(&self.bold, line, CMD_PAD + 8, ty, 13.0, Color::rgb(176, 240, 214));
            ty += CMD_LINE;
        }
        if !layout.steps.is_empty() {
            ty += 4;
            c.draw_text(&self.regular, "which runs, as root:", CMD_PAD + 8, ty, 11.0, Color::rgb(160, 176, 190));
            ty += 16;
            for line in &layout.steps {
                c.draw_text(&self.regular, line, CMD_PAD + 8, ty, 11.0, Color::rgb(222, 232, 240));
                ty += 16;
            }
        }
        let (cancel, go) = self.command_confirm_buttons(wi, &layout, action);
        c.draw_round_rect(cancel.0, layout.buttons_y, cancel.1, CMD_BUTTON_H, 10, Color::rgba(224, 236, 242, 220));
        c.draw_text_center(&self.bold, "Cancel", cancel.0 + cancel.1 / 2, layout.buttons_y + 8, 13.0, INK);
        let go_color = if action.danger() {
            Color::rgba(226, 92, 101, 235)
        } else {
            Color::rgba(29, 145, 137, 235)
        };
        c.draw_round_rect(go.0, layout.buttons_y, go.1, CMD_BUTTON_H, 10, go_color);
        c.draw_text_center(&self.bold, action.button(), go.0 + go.1 / 2, layout.buttons_y + 8, 13.0, Color::rgb(255, 255, 255));
        self.upload_canvas(self.ui.confirm_dialog, &c)
    }

    /// (x, width) of Cancel and of the action button.
    fn command_confirm_buttons(
        &self,
        w: i32,
        _layout: &CommandConfirmLayout,
        action: SessionAction,
    ) -> ((i32, i32), (i32, i32)) {
        let go_w = (measure_text(&self.bold, action.button(), 13.0) + 36).max(90);
        let go_x = w - CMD_PAD - go_w;
        ((go_x - 12 - 96, 96), (go_x, go_w))
    }

    pub(crate) fn handle_command_confirm_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        let Some(action) = self.confirm_command else {
            return Ok(());
        };
        let layout = self.command_confirm_layout(action);
        let (_, _, w, _) = self.confirm_geometry();
        let (cancel, go) = self.command_confirm_buttons(i32::from(w), &layout, action);
        if !(layout.buttons_y..=layout.buttons_y + CMD_BUTTON_H).contains(&y) {
            return Ok(());
        }
        if (go.0..=go.0 + go.1).contains(&x) {
            self.hide_close_confirm()?;
            self.run_session_action(action)?;
        } else if (cancel.0..=cancel.0 + cancel.1).contains(&x) {
            self.hide_close_confirm()?;
        }
        Ok(())
    }

    fn run_session_action(&mut self, action: SessionAction) -> AnyResult<()> {
        let command = action.command();
        eprintln!("aurora-wm: session action {:?}: {command}", action);
        if action == SessionAction::NetworkRestart {
            self.settings.wifi_status = Some("Restarting network...".to_string());
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let result = Command::new("sh")
                    .arg("-c")
                    .arg(&command)
                    .stdin(Stdio::null())
                    .output();
                let message = match result {
                    Ok(out) if out.status.success() => {
                        // Let NetworkManager re-associate before the page reloads
                        // its status, or it shows "Not connected" mid-handshake.
                        let _ = Command::new("nm-online")
                            .args(["-q", "-t", "25"])
                            .stdin(Stdio::null())
                            .status();
                        "Network restarted".to_string()
                    }
                    Ok(out) => {
                        let err = String::from_utf8_lossy(&out.stderr);
                        let last = err.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("");
                        format!("Network restart failed ({}): {last}", out.status)
                    }
                    Err(err) => format!("Network restart failed: {err}"),
                };
                let _ = tx.send(message);
            });
            self.net_restart_rx = Some(rx);
            if self.settings_visible && self.settings.tab == SettingsTab::Network {
                self.redraw_settings()?;
            }
            return Ok(());
        }
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&command);
        spawn_detached(cmd);
        Ok(())
    }

    pub(crate) fn poll_net_restart(&mut self) -> AnyResult<bool> {
        let Some(rx) = self.net_restart_rx.as_ref() else {
            return Ok(false);
        };
        let message = match rx.try_recv() {
            Ok(message) => message,
            Err(TryRecvError::Empty) => return Ok(false),
            Err(TryRecvError::Disconnected) => "Network restart stopped".to_string(),
        };
        self.net_restart_rx = None;
        eprintln!("aurora-wm: {message}");
        self.settings_cache.network_details = None;
        self.request_settings_data(SettingsTab::Network);
        self.start_wifi_refresh(true);
        self.settings.wifi_status = Some(message);
        if self.settings_visible && self.settings.tab == SettingsTab::Network {
            self.redraw_settings()?;
        }
        Ok(true)
    }

    /// Geometry of the session ("Log out") pill in the app menu header.
    pub(crate) fn app_menu_power_pill(&self) -> (i32, i32, i32, i32) {
        let (_, _, w, _) = self.app_menu_geometry();
        (i32::from(w) - 18 - 96, 11, 96, 34)
    }

    /// Dropdown under the pill: (x, y, w, row height).
    pub(crate) fn app_menu_power_dropdown(&self) -> (i32, i32, i32, i32) {
        let (_, _, w, _) = self.app_menu_geometry();
        let dw = 224;
        (i32::from(w) - 18 - dw, 50, dw, 40)
    }

    pub(crate) fn draw_app_menu_power(&self, c: &mut Canvas) {
        let (px, py, pw, ph) = self.app_menu_power_pill();
        let open = self.app_menu_power_open;
        c.draw_round_rect(
            px,
            py,
            pw,
            ph,
            11,
            if open { Color::rgba(226, 92, 101, 225) } else { Color::rgba(255, 255, 255, 205) },
        );
        let fg = if open { Color::rgb(255, 255, 255) } else { Color::rgb(196, 62, 74) };
        draw_session_power_glyph(c, px + 18, py + ph / 2, fg);
        c.draw_text(&self.bold, "Log out", px + 32, py + 9, 12.0, fg);
        if !open {
            return;
        }
        let (dx, dy, dw, rh) = self.app_menu_power_dropdown();
        let dh = rh * SESSION_MENU.len() as i32 + 12;
        c.draw_round_rect(dx - 1, dy - 1, dw + 2, dh + 2, 13, Color::rgba(120, 140, 155, 90));
        c.draw_round_rect(dx, dy, dw, dh, 12, Color::rgb(250, 254, 255));
        for (i, action) in SESSION_MENU.iter().enumerate() {
            let ry = dy + 6 + i as i32 * rh;
            if i > 0 {
                c.draw_rect(dx + 14, ry, dw - 28, 1, Color::rgba(176, 198, 210, 110));
            }
            let color = if action.danger() { Color::rgb(196, 62, 74) } else { MINT_DARK };
            draw_session_power_glyph(c, dx + 22, ry + rh / 2, color);
            c.draw_text(&self.bold, action.label(), dx + 40, ry + 5, 13.0, INK);
            c.draw_text(&self.regular, action.hint(), dx + 40, ry + 22, 10.0, MUTED);
        }
    }

    /// App-menu click while the session pill/dropdown may be involved.
    /// True: consumed.
    pub(crate) fn handle_app_menu_power_click(&mut self, button: u8, x: i32, y: i32) -> AnyResult<bool> {
        let (px, py, pw, ph) = self.app_menu_power_pill();
        let on_pill = (px..=px + pw).contains(&x) && (py..=py + ph).contains(&y);
        if button == 1 && on_pill {
            self.app_menu_power_open = !self.app_menu_power_open;
            self.redraw_app_menu()?;
            return Ok(true);
        }
        if !self.app_menu_power_open {
            return Ok(false);
        }
        let (dx, dy, dw, rh) = self.app_menu_power_dropdown();
        if button == 1 && (dx..=dx + dw).contains(&x) && y >= dy + 6 {
            let idx = ((y - dy - 6) / rh) as usize;
            if let Some(&action) = SESSION_MENU.get(idx) {
                self.hide_app_menu()?;
                self.show_command_confirm(action)?;
                return Ok(true);
            }
        }
        self.app_menu_power_open = false;
        self.redraw_app_menu()?;
        Ok(true)
    }

    /// Restart button right of the Network page title: (centre x, centre y, radius).
    pub(crate) fn network_restart_button(&self) -> (i32, i32, i32) {
        let sx = SIDEBAR_WIDTH + 24;
        (sx + measure_text(&self.bold, "Network", 24.0) + 24, 37, 15)
    }

    pub(crate) fn draw_network_restart_button(&self, c: &mut Canvas) {
        let (cx, cy, r) = self.network_restart_button();
        let busy = self.net_restart_rx.is_some();
        c.draw_circle(cx, cy, r, Color::rgba(29, 145, 137, if busy { 60 } else { 34 }));
        draw_reload_menu_icon(c, cx, cy, if busy { MUTED } else { MINT_DARK });
    }

    pub(crate) fn network_restart_button_hit(&self, x: i32, y: i32) -> bool {
        let (cx, cy, r) = self.network_restart_button();
        (x - cx).abs() <= r + 6 && (y - cy).abs() <= r + 6
    }
}

/// Power symbol: an open ring with a bar through the gap.
pub(crate) fn draw_session_power_glyph(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_arc(c, cx, cy, 7, 300.0, 240.0, 0, 2, color);
    draw_round_line(c, cx, cy - 9, cx, cy - 2, 2, color);
}
