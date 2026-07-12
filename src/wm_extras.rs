//! Window-management extras: EWMH fullscreen, sticky windows, the
//! window-title dropdown menu with close confirmation, topbar tooltips,
//! configurable global shortcuts, and GPU usage sampling.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use x11rb::CURRENT_TIME;
use x11rb::protocol::xproto::ConnectionExt as XprotoConnectionExt;
use x11rb::protocol::xproto::*;

use crate::*;
use crate::canvas::*;
use crate::model::*;
use crate::draw_helpers::*;
use crate::procutil::*;
use crate::system::*;
use crate::textutil::*;

pub(crate) const TITLE_MENU_WIDTH: u16 = 252;
pub(crate) const TITLE_MENU_ROW_H: i32 = 36;
pub(crate) const TITLE_MENU_ITEMS: usize = 6;
pub(crate) const CONFIRM_W: u16 = 380;
pub(crate) const CONFIRM_H: u16 = 148;
pub(crate) const TOOLTIP_H: u16 = 30;

// ------------------------------------------------------------------ shortcuts

pub(crate) const SHORTCUT_ACTIONS: [(&str, &str); 4] = [
    ("shortcut_folder", "Open file manager"),
    ("shortcut_terminal", "Open terminal"),
    ("shortcut_clipboard", "Clipboard history"),
    ("shortcut_screenshot", "Take screenshot"),
];

pub(crate) fn default_shortcut(keysym: u32) -> ShortcutSpec {
    ShortcutSpec {
        ctrl: true,
        alt: true,
        shift: false,
        super_key: false,
        keysym,
    }
}

pub(crate) fn parse_shortcut(value: &str) -> Option<ShortcutSpec> {
    let mut spec = ShortcutSpec {
        ctrl: false,
        alt: false,
        shift: false,
        super_key: false,
        keysym: 0,
    };
    for part in value.split('+') {
        match part.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => spec.ctrl = true,
            "alt" => spec.alt = true,
            "shift" => spec.shift = true,
            "super" | "win" | "meta" => spec.super_key = true,
            key if !key.is_empty() => {
                let mut chars = key.chars();
                let first = chars.next()?;
                if chars.next().is_none() && first.is_ascii_graphic() {
                    spec.keysym = first.to_ascii_lowercase() as u32;
                } else {
                    return None;
                }
            }
            _ => {}
        }
    }
    (spec.keysym != 0 && (spec.ctrl || spec.alt || spec.super_key)).then_some(spec)
}

pub(crate) fn format_shortcut(spec: ShortcutSpec) -> String {
    let mut out = String::new();
    if spec.ctrl {
        out.push_str("Ctrl+");
    }
    if spec.alt {
        out.push_str("Alt+");
    }
    if spec.shift {
        out.push_str("Shift+");
    }
    if spec.super_key {
        out.push_str("Super+");
    }
    if let Some(ch) = char::from_u32(spec.keysym) {
        out.push(ch.to_ascii_uppercase());
    }
    out
}

pub(crate) fn shortcut_setting_string(spec: ShortcutSpec) -> String {
    format_shortcut(spec).to_ascii_lowercase()
}

pub(crate) fn read_shortcut_config() -> ShortcutConfig {
    let read = |key: &str, fallback: u32| {
        read_setting_value(key)
            .and_then(|value| parse_shortcut(&value))
            .unwrap_or_else(|| default_shortcut(fallback))
    };
    ShortcutConfig {
        folder: read("shortcut_folder", 'o' as u32),
        terminal: read("shortcut_terminal", 't' as u32),
        clipboard: read("shortcut_clipboard", 'v' as u32),
        screenshot: read("shortcut_screenshot", 's' as u32),
    }
}

pub(crate) fn shortcut_by_index(config: &ShortcutConfig, idx: usize) -> ShortcutSpec {
    match idx {
        0 => config.folder,
        1 => config.terminal,
        2 => config.clipboard,
        _ => config.screenshot,
    }
}

pub(crate) fn set_shortcut_by_index(config: &mut ShortcutConfig, idx: usize, spec: ShortcutSpec) {
    match idx {
        0 => config.folder = spec,
        1 => config.terminal = spec,
        2 => config.clipboard = spec,
        _ => config.screenshot = spec,
    }
}

// ------------------------------------------------------------------ GPU usage

/// Sample GPU utilisation for every GPU in the system. AMD and Intel expose
/// `gpu_busy_percent` through sysfs; NVIDIA is queried via `nvidia-smi`.
pub(crate) fn read_gpu_usage() -> Vec<GpuUsage> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/drm") {
        let mut cards: Vec<_> = entries
            .flatten()
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.starts_with("card") && !name.contains('-')
            })
            .collect();
        cards.sort_by_key(|e| e.file_name());
        for entry in cards {
            let dev = entry.path().join("device");
            let vendor = fs::read_to_string(dev.join("vendor"))
                .unwrap_or_default()
                .trim()
                .to_string();
            if vendor == "0x10de" {
                continue; // NVIDIA: sysfs has no busy percent; use nvidia-smi below.
            }
            let label = match vendor.as_str() {
                "0x1002" => "AMD GPU",
                "0x8086" => "Intel GPU",
                _ => "GPU",
            };
            // AMD (amdgpu) and some drivers expose a direct busy percentage.
            let mut percent = fs::read_to_string(dev.join("gpu_busy_percent"))
                .ok()
                .and_then(|v| v.trim().parse::<f32>().ok());
            // Intel i915/xe expose no busy percent in sysfs; estimate load
            // from actual vs. max GT frequency (idle GPUs clock down).
            if percent.is_none() && vendor == "0x8086" {
                percent = intel_gpu_freq_percent(&entry.path());
            }
            let Some(percent) = percent else {
                continue;
            };
            out.push(GpuUsage {
                name: format!("{label} ({})", entry.file_name().to_string_lossy()),
                percent: percent.clamp(0.0, 100.0),
            });
        }
    }
    if Path::new("/proc/driver/nvidia").exists() && command_exists("nvidia-smi") {
        if let Some(output) = command_output_timeout(
            "nvidia-smi",
            &[
                "--query-gpu=name,utilization.gpu",
                "--format=csv,noheader,nounits",
            ],
            Duration::from_millis(1500),
        ) {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let mut parts = line.rsplitn(2, ',');
                let percent = parts
                    .next()
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .unwrap_or(0.0);
                let name = parts.next().unwrap_or("NVIDIA GPU").trim().to_string();
                out.push(GpuUsage {
                    name,
                    percent: percent.clamp(0.0, 100.0),
                });
            }
        }
    }
    out
}

/// Approximate Intel GPU load from GT frequency scaling: i915 clocks the GPU
/// between RPn (idle) and RP0 (max); actual freq relative to that range is a
/// usable load proxy when no busy counter is exposed.
pub(crate) fn intel_gpu_freq_percent(card: &Path) -> Option<f32> {
    let read_mhz = |name: &str| -> Option<f32> {
        // i915 places these at the card root; newer kernels use gt/gt0/.
        for base in [card.to_path_buf(), card.join("gt/gt0")] {
            if let Some(value) = fs::read_to_string(base.join(name))
                .ok()
                .and_then(|v| v.trim().parse::<f32>().ok())
            {
                return Some(value);
            }
        }
        None
    };
    let act = read_mhz("gt_act_freq_mhz").or_else(|| read_mhz("gt_cur_freq_mhz"))?;
    let max = read_mhz("gt_RP0_freq_mhz").or_else(|| read_mhz("gt_max_freq_mhz"))?;
    let min = read_mhz("gt_RPn_freq_mhz")
        .or_else(|| read_mhz("gt_min_freq_mhz"))
        .unwrap_or(0.0);
    if max <= min {
        return None;
    }
    Some(((act - min) / (max - min) * 100.0).clamp(0.0, 100.0))
}

impl Aurora {
    // -------------------------------------------------------------- EWMH

    /// Publish the EWMH hints this WM understands.
    pub(crate) fn publish_ewmh_support(&self) -> AnyResult<()> {
        let names: [&[u8]; 9] = [
            b"_NET_SUPPORTED",
            b"_NET_WM_STATE",
            b"_NET_WM_STATE_FULLSCREEN",
            b"_NET_WM_STATE_STICKY",
            b"_NET_ACTIVE_WINDOW",
            b"_NET_CURRENT_DESKTOP",
            b"_NET_NUMBER_OF_DESKTOPS",
            b"_NET_WM_DESKTOP",
            b"_NET_WM_MOVERESIZE",
        ];
        let mut atoms = Vec::with_capacity(names.len());
        for name in names {
            atoms.push(self.atom(name)?);
        }
        self.conn.change_property32(
            PropMode::REPLACE,
            self.root,
            atoms[0],
            AtomEnum::ATOM,
            &atoms,
        )?;
        Ok(())
    }

    /// True when the client asked for no WM decorations (`_MOTIF_WM_HINTS`)
    /// or is a window type that must not be framed.
    pub(crate) fn window_wants_csd(&self, window: Window) -> bool {
        if let Ok(motif) = self.atom(b"_MOTIF_WM_HINTS") {
            if let Ok(reply) = self
                .conn
                .get_property(false, window, motif, AtomEnum::ANY, 0, 5)
                .and_then(|c| Ok(c.reply()))
            {
                if let Ok(reply) = reply {
                    if let Some(values) = reply.value32() {
                        let hints: Vec<u32> = values.collect();
                        // flags bit 1 = MWM_HINTS_DECORATIONS; hints[2] == 0 = none
                        if hints.len() >= 3 && hints[0] & 0x2 != 0 && hints[2] == 0 {
                            return true;
                        }
                    }
                }
            }
        }
        if let (Ok(type_atom), Ok(normal), Ok(dialog)) = (
            self.atom(b"_NET_WM_WINDOW_TYPE"),
            self.atom(b"_NET_WM_WINDOW_TYPE_NORMAL"),
            self.atom(b"_NET_WM_WINDOW_TYPE_DIALOG"),
        ) {
            if let Ok(Ok(reply)) = self
                .conn
                .get_property(false, window, type_atom, AtomEnum::ATOM, 0, 8)
                .map(|c| c.reply())
            {
                if let Some(mut values) = reply.value32() {
                    // Any explicit non-normal, non-dialog type manages its own look.
                    if values.any(|atom| atom != normal && atom != dialog) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Set or publish the `_NET_WM_STATE` property for a client.
    pub(crate) fn publish_net_wm_state(&self, info: &ClientInfo) -> AnyResult<()> {
        let state_atom = self.atom(b"_NET_WM_STATE")?;
        let mut states = Vec::new();
        if info.fullscreen {
            states.push(self.atom(b"_NET_WM_STATE_FULLSCREEN")?);
        }
        if info.sticky {
            states.push(self.atom(b"_NET_WM_STATE_STICKY")?);
        }
        self.conn.change_property32(
            PropMode::REPLACE,
            info.window,
            state_atom,
            AtomEnum::ATOM,
            &states,
        )?;
        Ok(())
    }

    /// Handle a `_NET_WM_STATE` client message. Returns true when consumed.
    pub(crate) fn handle_net_wm_state_message(
        &mut self,
        ev: &ClientMessageEvent,
    ) -> AnyResult<bool> {
        let state_atom = self.atom(b"_NET_WM_STATE")?;
        if ev.type_ != state_atom {
            return Ok(false);
        }
        let Some(client) = self.client_or_ancestor_key_for(ev.window) else {
            return Ok(true);
        };
        let data = ev.data.as_data32();
        let action = data[0]; // 0 remove, 1 add, 2 toggle
        let fullscreen_atom = self.atom(b"_NET_WM_STATE_FULLSCREEN")?;
        let sticky_atom = self.atom(b"_NET_WM_STATE_STICKY")?;
        for property in [data[1], data[2]] {
            if property == fullscreen_atom {
                let current = self
                    .clients
                    .get(&client)
                    .is_some_and(|info| info.fullscreen);
                let target = match action {
                    0 => false,
                    1 => true,
                    _ => !current,
                };
                self.set_fullscreen(client, target)?;
            } else if property == sticky_atom {
                let current = self.clients.get(&client).is_some_and(|info| info.sticky);
                let target = match action {
                    0 => false,
                    1 => true,
                    _ => !current,
                };
                self.set_sticky(client, target)?;
            }
        }
        Ok(true)
    }

    // -------------------------------------------------------------- fullscreen

    pub(crate) fn set_fullscreen(&mut self, client: Window, on: bool) -> AnyResult<()> {
        let Some(mut info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        if info.fullscreen == on {
            return Ok(());
        }
        if on {
            info.fs_saved = Some((info.x, info.y, info.width, info.height, info.titlebar));
            info.fullscreen = true;
            info.titlebar = false;
            info.x = 0;
            info.y = 0;
            info.width = self.screen_width;
            info.height = self.screen_height;
        } else {
            info.fullscreen = false;
            if let Some((x, y, w, h, titlebar)) = info.fs_saved.take() {
                info.x = x;
                info.y = y;
                info.width = w;
                info.height = h;
                info.titlebar = titlebar;
            }
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
        self.publish_net_wm_state(&info)?;
        self.send_synthetic_configure(&info)?;
        if info.fullscreen {
            // A fullscreen window covers the WM chrome by design.
            self.conn.configure_window(
                info.frame,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
            self.conn
                .set_input_focus(InputFocus::POINTER_ROOT, info.window, CURRENT_TIME)?;
        } else {
            if title_h > 0 {
                self.redraw_frame_titlebar(client)?;
            }
            self.raise_chrome()?;
        }
        Ok(())
    }

    // -------------------------------------------------------------- sticky

    pub(crate) fn set_sticky(&mut self, client: Window, on: bool) -> AnyResult<()> {
        let Some(mut info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        if info.sticky == on {
            return Ok(());
        }
        info.sticky = on;
        if !on {
            info.workspace = self.active_workspace;
        }
        self.clients.insert(client, info);
        let desktop_atom = self.atom(b"_NET_WM_DESKTOP")?;
        let value: u32 = if on {
            0xFFFF_FFFF
        } else {
            self.active_workspace as u32
        };
        self.conn.change_property32(
            PropMode::REPLACE,
            info.window,
            desktop_atom,
            AtomEnum::CARDINAL,
            &[value],
        )?;
        self.publish_net_wm_state(&info)?;
        self.redraw_dock()?;
        Ok(())
    }

    /// True when this client should be visible on the active workspace.
    pub(crate) fn client_on_active_workspace(&self, info: &ClientInfo) -> bool {
        info.sticky || info.workspace == self.active_workspace
    }

    // -------------------------------------------------------------- title menu

    /// Number of rows in the title menu, depending on which page is showing.
    pub(crate) fn title_menu_row_count(&self) -> usize {
        if self.title_menu_workspaces {
            // A "Back" row followed by one row per workspace.
            self.workspace_count + 1
        } else {
            TITLE_MENU_ITEMS
        }
    }

    pub(crate) fn title_menu_geometry(&self, client: Window) -> (i16, i16, u16, u16) {
        let h = (TITLE_MENU_ROW_H * self.title_menu_row_count() as i32 + 14) as u16;
        let Some(info) = self.clients.get(&client) else {
            return (0, 0, TITLE_MENU_WIDTH, h);
        };
        let x = (info.x + 84)
            .min((self.screen_width.saturating_sub(TITLE_MENU_WIDTH)) as i16)
            .max(0);
        let y = info.y + TITLEBAR_HEIGHT as i16;
        (x, y, TITLE_MENU_WIDTH, h)
    }

    pub(crate) fn configure_title_menu(&self, client: Window) -> AnyResult<()> {
        let (x, y, w, h) = self.title_menu_geometry(client);
        self.conn.configure_window(
            self.ui.title_menu,
            &ConfigureWindowAux::new()
                .x(i32::from(x))
                .y(i32::from(y))
                .width(u32::from(w))
                .height(u32::from(h))
                .stack_mode(StackMode::ABOVE),
        )?;
        Ok(())
    }

    pub(crate) fn toggle_title_menu(&mut self, client: Window) -> AnyResult<()> {
        if self.title_menu_open == Some(client) {
            return self.hide_title_menu();
        }
        self.title_menu_open = Some(client);
        self.title_menu_workspaces = false;
        self.configure_title_menu(client)?;
        self.conn.map_window(self.ui.title_menu)?;
        // Sync so the override-redirect window is viewable before we draw into it
        // (without a compositor an early put_image would otherwise be dropped).
        self.conn.sync()?;
        self.redraw_title_menu()?;
        Ok(())
    }

    pub(crate) fn hide_title_menu(&mut self) -> AnyResult<()> {
        if self.title_menu_open.take().is_some() {
            self.title_menu_workspaces = false;
            self.conn.unmap_window(self.ui.title_menu)?;
        }
        Ok(())
    }

    pub(crate) fn redraw_title_menu(&self) -> AnyResult<()> {
        let Some(client) = self.title_menu_open else {
            return Ok(());
        };
        let Some(info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        let (_, _, w, h) = self.title_menu_geometry(client);
        let mut c = Canvas::new(w, h, Color::rgb(247, 252, 255));
        c.draw_round_rect(0, 0, i32::from(w), i32::from(h), 12, Color::rgb(250, 254, 255));
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            12,
            Color::rgba(176, 198, 210, 60),
        );
        if self.title_menu_workspaces {
            // Workspace picker page: a back row, then one row per workspace.
            c.draw_text(&self.bold, "‹  Move to workspace", 20, 16, 13.0, INK);
            c.draw_rect(
                10,
                TITLE_MENU_ROW_H - 2,
                i32::from(w) - 20,
                1,
                Color::rgba(176, 198, 210, 90),
            );
            for index in 0..self.workspace_count {
                let y = 8 + (index + 1) as i32 * TITLE_MENU_ROW_H;
                let current = index == info.workspace;
                c.draw_text(
                    &self.regular,
                    &format!("Workspace {}", index + 1),
                    40,
                    y + 8,
                    13.0,
                    INK,
                );
                if current {
                    c.draw_round_rect(12, y + 8, 16, 16, 5, Color::rgba(29, 145, 137, 210));
                    c.draw_line(15, y + 16, 19, y + 20, 2, Color::rgb(255, 255, 255));
                    c.draw_line(19, y + 20, 25, y + 11, 2, Color::rgb(255, 255, 255));
                }
            }
            return self.upload_canvas(self.ui.title_menu, &c);
        }
        let items = [
            (
                "Show on all workspaces",
                if info.sticky { Some(true) } else { Some(false) },
                INK,
            ),
            (
                "Fullscreen",
                if info.fullscreen { Some(true) } else { Some(false) },
                INK,
            ),
            ("Maximize / Restore", None, INK),
            ("Minimize", None, INK),
            ("Move to workspace  ›", None, INK),
            ("Close...", None, Color::rgb(196, 64, 74)),
        ];
        for (idx, (label, checked, color)) in items.iter().enumerate() {
            let y = 8 + idx as i32 * TITLE_MENU_ROW_H;
            if idx == items.len() - 1 {
                c.draw_rect(10, y - 2, i32::from(w) - 20, 1, Color::rgba(176, 198, 210, 90));
            }
            c.draw_text(&self.regular, label, 40, y + 8, 13.0, *color);
            match checked {
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
        self.upload_canvas(self.ui.title_menu, &c)
    }

    pub(crate) fn handle_title_menu_click(&mut self, y: i32) -> AnyResult<()> {
        let Some(client) = self.title_menu_open else {
            return Ok(());
        };
        if self.title_menu_workspaces {
            let row = ((y - 8) / TITLE_MENU_ROW_H).max(0) as usize;
            if row == 0 {
                // Back to the main title menu page.
                self.title_menu_workspaces = false;
                self.configure_title_menu(client)?;
                self.conn.sync()?;
                self.redraw_title_menu()?;
                return Ok(());
            }
            let workspace = row - 1;
            self.hide_title_menu()?;
            if workspace < self.workspace_count {
                self.move_client_to_workspace(client, workspace)?;
            }
            return Ok(());
        }
        let idx = ((y - 8) / TITLE_MENU_ROW_H).clamp(0, TITLE_MENU_ITEMS as i32 - 1) as usize;
        match idx {
            0 => {
                self.hide_title_menu()?;
                let sticky = self.clients.get(&client).is_some_and(|info| info.sticky);
                self.set_sticky(client, !sticky)?;
            }
            1 => {
                self.hide_title_menu()?;
                let fullscreen = self
                    .clients
                    .get(&client)
                    .is_some_and(|info| info.fullscreen);
                self.set_fullscreen(client, !fullscreen)?;
            }
            2 => {
                self.hide_title_menu()?;
                self.toggle_maximize_client(client)?;
            }
            3 => {
                self.hide_title_menu()?;
                self.minimize_client(client)?;
            }
            4 => {
                // Open the workspace picker submenu in place (keep the menu open).
                self.title_menu_workspaces = true;
                self.configure_title_menu(client)?;
                self.conn.sync()?;
                self.redraw_title_menu()?;
            }
            _ => {
                self.hide_title_menu()?;
                self.show_close_confirm(client)?;
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------- confirm

    pub(crate) fn confirm_geometry(&self) -> (i16, i16, u16, u16) {
        let x = ((self.screen_width.saturating_sub(CONFIRM_W)) / 2) as i16;
        let y = ((self.screen_height.saturating_sub(CONFIRM_H)) / 3) as i16;
        (x, y, CONFIRM_W, CONFIRM_H)
    }

    pub(crate) fn show_close_confirm(&mut self, client: Window) -> AnyResult<()> {
        self.confirm_close = Some(client);
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
        self.redraw_close_confirm()?;
        Ok(())
    }

    pub(crate) fn hide_close_confirm(&mut self) -> AnyResult<()> {
        if self.confirm_close.take().is_some() {
            self.conn.unmap_window(self.ui.confirm_dialog)?;
        }
        Ok(())
    }

    pub(crate) fn redraw_close_confirm(&self) -> AnyResult<()> {
        let Some(client) = self.confirm_close else {
            return Ok(());
        };
        let (_, _, w, h) = self.confirm_geometry();
        let mut c = Canvas::new(w, h, Color::rgb(247, 252, 255));
        c.draw_round_rect(0, 0, i32::from(w), i32::from(h), 14, Color::rgb(250, 254, 255));
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            14,
            Color::rgba(176, 198, 210, 70),
        );
        let title = self
            .clients
            .get(&client)
            .map(|info| self.window_title(info.window))
            .unwrap_or_else(|| "Window".to_string());
        c.draw_text(&self.bold, "Close window?", 22, 18, 17.0, INK);
        c.draw_text(
            &self.regular,
            &compact(&format!("\"{title}\" will be asked to close."), 46),
            22,
            48,
            13.0,
            MUTED,
        );
        // Cancel button
        c.draw_round_rect(i32::from(w) - 210, 96, 92, 34, 10, Color::rgba(224, 236, 242, 220));
        c.draw_text_center(&self.bold, "Cancel", i32::from(w) - 164, 104, 13.0, INK);
        // Close button
        c.draw_round_rect(i32::from(w) - 108, 96, 86, 34, 10, Color::rgba(226, 92, 101, 235));
        c.draw_text_center(&self.bold, "Close", i32::from(w) - 65, 104, 13.0, Color::rgb(255, 255, 255));
        self.upload_canvas(self.ui.confirm_dialog, &c)
    }

    pub(crate) fn handle_confirm_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        let Some(client) = self.confirm_close else {
            return Ok(());
        };
        let (_, _, w, _) = self.confirm_geometry();
        let w = i32::from(w);
        if (96..=130).contains(&y) {
            if (w - 108..=w - 22).contains(&x) {
                self.hide_close_confirm()?;
                self.close_client(client)?;
                return Ok(());
            }
            if (w - 210..=w - 118).contains(&x) {
                self.hide_close_confirm()?;
                return Ok(());
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------- tooltips

    /// Show or update the tooltip for the topbar icon at pointer x.
    pub(crate) fn update_topbar_tooltip(&mut self, x: i32) -> AnyResult<()> {
        let controls = self.topbar_controls();
        let shortcuts = self.settings.shortcuts;
        let hit = |cx: i32| (cx - TOPBAR_ICON_HIT_RADIUS..=cx + TOPBAR_ICON_HIT_RADIUS).contains(&x);
        let label = if hit(controls.clipboard_x) {
            Some((
                controls.clipboard_x,
                format!("Clipboard  ({})", format_shortcut(shortcuts.clipboard)),
            ))
        } else if hit(controls.screenshot_x) {
            Some((
                controls.screenshot_x,
                format!("Screenshot  ({})", format_shortcut(shortcuts.screenshot)),
            ))
        } else if hit(controls.display_x) {
            Some((controls.display_x, "Display settings  (click toggles)".to_string()))
        } else if hit(controls.audio_x) {
            Some((controls.audio_x, "Audio settings  (click toggles)".to_string()))
        } else if hit(controls.network_x) {
            Some((controls.network_x, "Network settings  (click toggles)".to_string()))
        } else if (controls.battery_left..=controls.battery_right).contains(&x) {
            Some((
                (controls.battery_left + controls.battery_right) / 2,
                "Power settings  (click toggles)".to_string(),
            ))
        } else {
            let workspace = (0..self.workspace_count).find(|&index| {
                (self.workspace_x(index)..=self.workspace_x(index) + WORKSPACE_SIZE).contains(&x)
            });
            if let Some(idx) = workspace {
                Some((
                    self.workspace_x(idx) + WORKSPACE_SIZE / 2,
                    format!("Workspace {}  (Super+Left/Right)", idx + 1),
                ))
            } else if (self.add_workspace_x()..=self.add_workspace_x() + WORKSPACE_SIZE)
                .contains(&x)
            {
                Some((
                    self.add_workspace_x() + WORKSPACE_SIZE / 2,
                    "Add workspace".to_string(),
                ))
            } else {
                None
            }
        };
        match label {
            Some((anchor_x, text)) => {
                if self
                    .tooltip_shown
                    .as_ref()
                    .is_some_and(|(ax, t)| *ax == anchor_x && *t == text)
                {
                    return Ok(());
                }
                self.tooltip_shown = Some((anchor_x, text.clone()));
                let tw = (measure_text(&self.regular, &text, 12.0) + 22) as u16;
                let tx = (anchor_x - i32::from(tw) / 2)
                    .clamp(4, i32::from(self.screen_width.saturating_sub(tw)) - 4);
                self.conn.configure_window(
                    self.ui.tooltip,
                    &ConfigureWindowAux::new()
                        .x(tx)
                        .y(i32::from(TOPBAR_HEIGHT) + 4)
                        .width(u32::from(tw))
                        .height(u32::from(TOOLTIP_H))
                        .stack_mode(StackMode::ABOVE),
                )?;
                self.conn.map_window(self.ui.tooltip)?;
                self.redraw_tooltip(&text, tw)?;
            }
            None => self.hide_tooltip()?,
        }
        Ok(())
    }

    pub(crate) fn hide_tooltip(&mut self) -> AnyResult<()> {
        if self.tooltip_shown.take().is_some() {
            self.conn.unmap_window(self.ui.tooltip)?;
        }
        Ok(())
    }

    pub(crate) fn redraw_tooltip(&self, text: &str, w: u16) -> AnyResult<()> {
        let mut c = Canvas::new(w, TOOLTIP_H, Color::rgb(23, 34, 42));
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(TOOLTIP_H),
            8,
            Color::rgb(30, 44, 54),
        );
        c.draw_text(&self.regular, text, 11, 7, 12.0, Color::rgb(238, 248, 252));
        self.upload_canvas(self.ui.tooltip, &c)
    }

    // -------------------------------------------------------------- shortcuts

    pub(crate) fn grab_configured_shortcuts(&self) -> AnyResult<()> {
        let config = self.settings.shortcuts;
        for spec in [
            config.folder,
            config.terminal,
            config.clipboard,
            config.screenshot,
        ] {
            self.grab_shortcut(spec)?;
        }
        Ok(())
    }

    pub(crate) fn shortcut_modmask(spec: ShortcutSpec) -> ModMask {
        let mut mask = ModMask::from(0u16);
        if spec.ctrl {
            mask = mask | ModMask::CONTROL;
        }
        if spec.alt {
            mask = mask | ModMask::M1;
        }
        if spec.shift {
            mask = mask | ModMask::SHIFT;
        }
        if spec.super_key {
            mask = mask | ModMask::M4;
        }
        mask
    }

    pub(crate) fn grab_shortcut(&self, spec: ShortcutSpec) -> AnyResult<()> {
        let Some(keycode) = self.keycode_for_keysym(spec.keysym)? else {
            return Ok(());
        };
        let base = Self::shortcut_modmask(spec);
        for extra in [
            ModMask::from(0u16),
            ModMask::LOCK,
            ModMask::M2,
            ModMask::LOCK | ModMask::M2,
        ] {
            let _ = self.conn.grab_key(
                false,
                self.root,
                base | extra,
                keycode,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            );
        }
        Ok(())
    }

    pub(crate) fn ungrab_shortcut(&self, spec: ShortcutSpec) -> AnyResult<()> {
        if let Some(keycode) = self.keycode_for_keysym(spec.keysym)? {
            let base = Self::shortcut_modmask(spec);
            for extra in [
                ModMask::from(0u16),
                ModMask::LOCK,
                ModMask::M2,
                ModMask::LOCK | ModMask::M2,
            ] {
                let _ = self.conn.ungrab_key(keycode, self.root, base | extra);
            }
        }
        Ok(())
    }

    /// Try to dispatch a configured global shortcut. Returns true if handled.
    pub(crate) fn dispatch_shortcut(&mut self, ev: &KeyPressEvent) -> AnyResult<bool> {
        let mapping = self.conn.get_keyboard_mapping(ev.detail, 1)?.reply()?;
        let Some(&keysym) = mapping.keysyms.first() else {
            return Ok(false);
        };
        let keysym = if (0x41..=0x5a).contains(&keysym) {
            keysym + 32
        } else {
            keysym
        };
        let state = u16::from(ev.state);
        let ctrl = state & u16::from(ModMask::CONTROL) != 0;
        let alt = state & u16::from(ModMask::M1) != 0;
        let shift = state & u16::from(ModMask::SHIFT) != 0;
        let super_key = state & u16::from(ModMask::M4) != 0;
        if !(ctrl || alt || super_key) {
            return Ok(false);
        }
        let pressed = ShortcutSpec {
            ctrl,
            alt,
            shift,
            super_key,
            keysym,
        };
        let config = self.settings.shortcuts;
        if pressed == config.folder {
            self.shortcut_focus_folder()?;
        } else if pressed == config.terminal {
            self.shortcut_focus_terminal()?;
        } else if pressed == config.clipboard {
            self.toggle_clipboard_menu()?;
        } else if pressed == config.screenshot {
            self.toggle_screenshot_mode()?;
        } else {
            return Ok(false);
        }
        Ok(true)
    }

    pub(crate) fn shortcut_focus_folder(&mut self) -> AnyResult<()> {
        if self.launch_file_manager(&self.folder_path) {
            return Ok(());
        }

        self.folder_front = true;
        self.settings_front = false;
        self.media_front = false;
        self.conn.map_window(self.ui.folder)?;
        self.conn.configure_window(
            self.ui.folder,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?;
        self.conn
            .set_input_focus(InputFocus::POINTER_ROOT, self.ui.folder, CURRENT_TIME)?;
        self.redraw_folder()?;
        self.raise_chrome()?;
        Ok(())
    }

    pub(crate) fn shortcut_focus_terminal(&mut self) -> AnyResult<()> {
        if self.launch_file_manager_terminal(&self.folder_path) {
            return Ok(());
        }

        if !self.folder_terminal.visible {
            self.toggle_folder_terminal()?;
        }
        self.conn.configure_window(
            self.ui.folder_terminal,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?;
        self.conn.set_input_focus(
            InputFocus::POINTER_ROOT,
            self.ui.folder_terminal,
            CURRENT_TIME,
        )?;
        self.raise_chrome()?;
        Ok(())
    }

    /// Record a captured shortcut from the settings UI.
    pub(crate) fn capture_shortcut_key(&mut self, ev: &KeyPressEvent) -> AnyResult<bool> {
        let Some(idx) = self.settings.shortcut_capture else {
            return Ok(false);
        };
        let mapping = self.conn.get_keyboard_mapping(ev.detail, 1)?.reply()?;
        let Some(&keysym) = mapping.keysyms.first() else {
            return Ok(true);
        };
        // Ignore pure modifier presses.
        if (0xffe1..=0xffee).contains(&keysym) {
            return Ok(true);
        }
        if keysym == 0xff1b {
            // Escape cancels capture.
            self.settings.shortcut_capture = None;
            self.redraw_settings()?;
            return Ok(true);
        }
        let keysym = if (0x41..=0x5a).contains(&keysym) {
            keysym + 32
        } else {
            keysym
        };
        if !(0x20..=0x7e).contains(&keysym) {
            return Ok(true);
        }
        let state = u16::from(ev.state);
        let spec = ShortcutSpec {
            ctrl: state & u16::from(ModMask::CONTROL) != 0,
            alt: state & u16::from(ModMask::M1) != 0,
            shift: state & u16::from(ModMask::SHIFT) != 0,
            super_key: state & u16::from(ModMask::M4) != 0,
            keysym,
        };
        if !(spec.ctrl || spec.alt || spec.super_key) {
            return Ok(true);
        }
        let old = shortcut_by_index(&self.settings.shortcuts, idx);
        self.ungrab_shortcut(old)?;
        set_shortcut_by_index(&mut self.settings.shortcuts, idx, spec);
        self.grab_shortcut(spec)?;
        self.settings.shortcut_capture = None;
        save_app_commands(&self.settings)?;
        self.redraw_settings()?;
        Ok(true)
    }

    // -------------------------------------------------------------- UI windows

    /// Create the override-redirect windows introduced by this module.
    pub(crate) fn create_extra_ui_windows(&self) -> AnyResult<()> {
        let menu_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS)
            .cursor(self.cursor)
            .background_pixel(0)
            .backing_store(BackingStore::WHEN_MAPPED);
        for (window, w, h) in [
            (
                self.ui.title_menu,
                TITLE_MENU_WIDTH,
                (TITLE_MENU_ROW_H * TITLE_MENU_ITEMS as i32 + 14) as u16,
            ),
            (self.ui.confirm_dialog, CONFIRM_W, CONFIRM_H),
        ] {
            self.conn.create_window(
                self.depth,
                window,
                self.root,
                0,
                i16::try_from(TOPBAR_HEIGHT).unwrap_or(40),
                w,
                h,
                0,
                WindowClass::INPUT_OUTPUT,
                self.visual,
                &menu_aux,
            )?;
        }
        let tooltip_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE)
            .background_pixel(0)
            .backing_store(BackingStore::WHEN_MAPPED);
        self.conn.create_window(
            self.depth,
            self.ui.tooltip,
            self.root,
            0,
            i16::try_from(TOPBAR_HEIGHT).unwrap_or(40),
            120,
            TOOLTIP_H,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &tooltip_aux,
        )?;
        Ok(())
    }

    // -------------------------------------------------------------- settings tab

    pub(crate) fn draw_shortcuts_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        c.draw_text(&self.bold, "Shortcuts", sx, 22, 24.0, INK);
        c.draw_text(
            &self.regular,
            "Global keyboard shortcuts. Click one, then press the new keys.",
            sx,
            54,
            13.0,
            MUTED,
        );
        let card_w = i32::from(c.width) - sx - 24;
        draw_card(c, sx, 86, card_w, 60 + SHORTCUT_ACTIONS.len() as i32 * 46);
        for (idx, (_, label)) in SHORTCUT_ACTIONS.iter().enumerate() {
            let y = 112 + idx as i32 * 46;
            let capturing = self.settings.shortcut_capture == Some(idx);
            c.draw_round_rect(
                sx + 14,
                y - 6,
                card_w - 28,
                38,
                10,
                if capturing {
                    Color::rgba(188, 224, 255, 235)
                } else {
                    Color::rgba(255, 255, 255, 130)
                },
            );
            c.draw_text(&self.regular, label, sx + 28, y + 2, 13.0, INK);
            let value = if capturing {
                "Press keys...".to_string()
            } else {
                format_shortcut(shortcut_by_index(&self.settings.shortcuts, idx))
            };
            let badge_w = measure_text(&self.bold, &value, 12.0) + 20;
            let badge_x = sx + card_w - 28 - badge_w;
            c.draw_round_rect(
                badge_x,
                y - 1,
                badge_w,
                26,
                8,
                if capturing {
                    Color::rgba(73, 156, 231, 90)
                } else {
                    Color::rgba(224, 236, 242, 220)
                },
            );
            c.draw_text(
                &self.bold,
                &value,
                badge_x + 10,
                y + 4,
                12.0,
                if capturing { BLUE } else { MINT_DARK },
            );
        }
        let hint_y = 118 + SHORTCUT_ACTIONS.len() as i32 * 46 + 26;
        c.draw_text(
            &self.regular,
            "Shortcuts need Ctrl, Alt or Super plus one key.",
            sx + 2,
            hint_y,
            12.0,
            MUTED,
        );
        c.draw_text(
            &self.regular,
            "Esc cancels while recording.",
            sx + 2,
            hint_y + 22,
            12.0,
            MUTED,
        );
    }

    pub(crate) fn handle_shortcuts_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        let sx = SIDEBAR_WIDTH + 24;
        let card_w = i32::from(self.settings_geometry().2) - sx - 24;
        for idx in 0..SHORTCUT_ACTIONS.len() {
            let row_y = 112 + idx as i32 * 46;
            if x >= sx + 14 && x <= sx + card_w - 14 && y >= row_y - 6 && y <= row_y + 32 {
                self.settings.shortcut_capture = Some(idx);
                self.conn.set_input_focus(
                    InputFocus::POINTER_ROOT,
                    self.ui.settings,
                    CURRENT_TIME,
                )?;
                self.redraw_settings()?;
                return Ok(());
            }
        }
        if self.settings.shortcut_capture.take().is_some() {
            self.redraw_settings()?;
        }
        Ok(())
    }

    // -------------------------------------------------------------- brightness

    /// Apply brightness through the hardware backlight when available,
    /// falling back to xrandr's software gamma.
    pub(crate) fn apply_brightness_all(&self, percent: u8) -> Result<(), String> {
        let percent = percent.clamp(10, 100);
        let has_backlight = fs::read_dir("/sys/class/backlight")
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false);
        if has_backlight && command_exists("brightnessctl") {
            let mut cmd = Command::new("brightnessctl");
            cmd.args(["set", &format!("{percent}%")]);
            spawn_detached(cmd);
            return Ok(());
        }
        apply_xrandr_brightness(&self.display, self.current_display_output(), percent)
    }
}
