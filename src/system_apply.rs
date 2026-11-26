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
use crate::layout::*;
use crate::draw_helpers::*;
use crate::pixels::*;
use crate::system::*;
use crate::textutil::*;
use crate::procutil::*;
use crate::files::*;

impl Aurora {
    pub(crate) fn apply_display_mode(&mut self, idx: usize) {
        let Some(mode) = self.display_modes.get(idx).cloned() else {
            return;
        };
        let label = mode.label();
        let (native_w, native_h) = self.native_screen_size();
        if (mode.width, mode.height) != (native_w, native_h) {
            // Refused: on this build a live size change segfaults Xorg
            // (2026-11-26: "Allocate new frame buffer", then SIGSEGV, X restarted).
            self.settings.display_status =
                Some(format!("{label}: size change is off, it crashes Xorg here"));
            return;
        }
        match apply_xrandr_mode(&self.display, &mode) {
            Ok(()) => {
                self.settings.display_status = Some(format!("Requested {label}"));
                self.display_modes =
                    read_display_modes(&self.display, self.screen_width, self.screen_height);
                self.settings.selected_mode = self
                    .display_modes
                    .iter()
                    .position(|candidate| {
                        candidate.width == mode.width && candidate.height == mode.height
                    })
                    .unwrap_or(idx.min(self.display_modes.len().saturating_sub(1)));
            }
            Err(err) => {
                self.settings.display_status = Some(err);
            }
        }
    }

    pub(crate) fn current_display_output(&self) -> Option<&str> {
        self.display_modes
            .iter()
            .find(|mode| mode.current)
            .or_else(|| self.display_modes.get(self.settings.selected_mode))
            .and_then(|mode| mode.output.as_deref())
    }

    pub(crate) fn apply_sleep_timeout(&self) {
        // Blanking mode: the server paints the screen black when the timer
        // fires (no DPMS on apple-adbe); the backlight follows via
        // ScreensaverNotify. Cycle 0 = no periodic pattern change.
        let secs = self.settings.sleep_after_secs;
        let _ = self.conn.set_screen_saver(
            if secs == 0 { 0 } else { secs.min(32767) as i16 },
            0,
            Blanking::PREFERRED,
            Exposures::NOT_ALLOWED,
        );
        let _ = self.conn.flush();
    }

    /// Native (physical) root size, independent of the UI scale.
    pub(crate) fn native_screen_size(&self) -> (u16, u16) {
        // Raw (unscaled) root geometry: follows rotation, unlike the setup.
        match self.conn.0.get_geometry(self.root).ok().and_then(|c| c.reply().ok()) {
            Some(g) => (g.width, g.height),
            None => {
                let s = &self.conn.setup().roots[0];
                (s.width_in_pixels, s.height_in_pixels)
            }
        }
    }

    /// Re-exec aurora-wm on the same X server (after a scale change, or
    /// `aurora-wm --restart-wm`): client windows survive and are re-adopted.
    pub(crate) fn restart_in_place(&mut self) {
        self.restart_in_place_with(&[]);
    }

    /// Like `restart_in_place`, with extra environment for the new instance.
    pub(crate) fn restart_in_place_with(&mut self, env: &[(&str, &str)]) {
        use std::os::unix::process::CommandExt;
        let _ = self.conn.flush();
        // A binary replaced in place (mv over it) leaves /proc/self/exe ending in " (deleted)".
        let exe = std::env::current_exe()
            .ok()
            .filter(|path| !path.to_string_lossy().ends_with(" (deleted)"))
            .unwrap_or_else(|| "/usr/local/bin/aurora-wm".into());
        let args: Vec<String> = std::env::args().skip(1).collect();
        let err = Command::new(exe).args(args).envs(env.iter().copied()).exec();
        eprintln!("aurora-wm: restart failed: {err}");
    }

    /// Enter/leave screen sleep: backlight fully off (brightness 0 plus
    /// bl_power powerdown) while blanked; restore the user's level on wake.
    /// Waking always re-powers the backlight, even when we already think we
    /// are awake: a forced OFF (xset s reset, omarchy-powerd after a button)
    /// must never leave a powered-down panel behind.
    pub(crate) fn set_screen_asleep(&mut self, asleep: bool) {
        if asleep && self.screen_asleep {
            return;
        }
        let was_asleep = self.screen_asleep;
        self.screen_asleep = asleep;
        let mut wrote = Vec::new();
        if let Ok(entries) = fs::read_dir("/sys/class/backlight") {
            for entry in entries.flatten() {
                let path = entry.path();
                let mut put = |file: &str, value: &str| {
                    let res = fs::write(path.join(file), value);
                    wrote.push(format!(
                        "{file}={}{}",
                        value.trim(),
                        res.err().map(|e| format!(" (FAILED: {e})")).unwrap_or_default()
                    ));
                };
                if asleep {
                    put("brightness", "0\n");
                    put("bl_power", "4\n");
                } else {
                    put("bl_power", "0\n");
                }
            }
        }
        if asleep {
            self.show_sleep_cover();
        } else {
            if let Some(cover) = self.sleep_cover {
                let _ = self.conn.unmap_window(cover);
            }
            if let Err(e) = self.apply_brightness_all(self.settings.brightness_percent) {
                wrote.push(format!("brightness FAILED: {e}"));
            }
        }
        sleep_log(&format!(
            "screen {} (was {}): {}; now {}",
            if asleep { "asleep" } else { "awake" },
            if was_asleep { "asleep" } else { "awake" },
            wrote.join(", "),
            backlight_summary()
        ));
        let _ = self.conn.flush();
    }

    /// At startup: adopt the server's screensaver state.
    pub(crate) fn sync_screen_sleep_with_server(&mut self) {
        let on = self
            .conn
            .screensaver_query_info(self.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|info| info.state == u8::from(x11rb::protocol::screensaver::State::ON));
        sleep_log(&format!("startup: X screensaver {}", if on { "on" } else { "off" }));
        self.set_screen_asleep(on);
    }

    /// Awake, the backlight must be powered. A wake that lands while the
    /// backlight driver is still resuming (Power after a suspend) loses our
    /// bl_power write and the driver restores powerdown: black screen with
    /// the session awake. Checked every couple of seconds from the run loop.
    pub(crate) fn keep_backlight_awake(&mut self) {
        if self.screen_asleep {
            // Asleep we wrote brightness 0. Lit again by someone else
            // (omarchy-powerd on Power/Home/Smart Cover, which X never sees):
            // end the X screensaver so ScreensaverNotify OFF wakes us fully.
            let lit = fs::read_dir("/sys/class/backlight").is_ok_and(|entries| {
                entries.flatten().any(|e| {
                    fs::read_to_string(e.path().join("brightness"))
                        .ok()
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .is_some_and(|v| v > 0)
                })
            });
            if lit {
                sleep_log(&format!("panel lit while screen asleep ({}); forcing screensaver off", backlight_summary()));
                let _ = self.conn.force_screen_saver(ScreenSaver::RESET);
                let _ = self.conn.flush();
            }
            return;
        }
        let mut fixed = false;
        if let Ok(entries) = fs::read_dir("/sys/class/backlight") {
            for entry in entries.flatten() {
                let path = entry.path();
                let off = fs::read_to_string(path.join("bl_power"))
                    .is_ok_and(|v| v.trim() != "0");
                if off && fs::write(path.join("bl_power"), "0\n").is_ok() {
                    fixed = true;
                }
            }
        }
        if fixed {
            sleep_log("backlight was powered down while awake; powered it up");
            let _ = self.apply_brightness_all(self.settings.brightness_percent);
            let _ = self.conn.flush();
        }
    }

    pub(crate) fn raise_sleep_cover(&self) {
        if let Some(cover) = self.sleep_cover {
            let _ = self
                .conn
                .configure_window(cover, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        }
    }

    fn show_sleep_cover(&mut self) {
        let cover = match self.sleep_cover {
            Some(w) => w,
            None => {
                let Ok(w) = self.conn.generate_id() else { return };
                let aux = CreateWindowAux::new()
                    .background_pixel(0)
                    .override_redirect(1)
                    .cursor(x11rb::NONE)
                    .event_mask(
                        EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                    );
                if self
                    .conn
                    .create_window(
                        x11rb::COPY_DEPTH_FROM_PARENT,
                        w,
                        self.root,
                        0,
                        0,
                        self.screen_width,
                        self.screen_height,
                        0,
                        WindowClass::INPUT_OUTPUT,
                        x11rb::COPY_FROM_PARENT,
                        &aux,
                    )
                    .is_err()
                {
                    return;
                }
                self.sleep_cover = Some(w);
                w
            }
        };
        let _ = self.conn.configure_window(
            cover,
            &ConfigureWindowAux::new()
                .x(0)
                .y(0)
                .width(u32::from(self.screen_width))
                .height(u32::from(self.screen_height))
                .stack_mode(StackMode::ABOVE),
        );
        let _ = self.conn.map_window(cover);
    }

    pub(crate) fn set_compositor_enabled(&mut self, enabled: bool) -> AnyResult<()> {
        if enabled {
            if self.compositor_active || init_light_compositor(&self.conn, self.root) {
                self.compositor_active = true;
                self.settings.compositor_enabled = true;
                self.settings.display_status = Some("Compositor enabled".to_string());
            } else {
                self.settings.compositor_enabled = false;
                self.settings.display_status =
                    Some("Composite extension unavailable or already owned".to_string());
            }
        } else {
            self.settings.compositor_enabled = false;
            if self.compositor_active {
                match disable_light_compositor(&self.conn, self.root) {
                    Ok(()) => {
                        self.compositor_active = false;
                        self.settings.display_status = Some("Compositor disabled".to_string());
                    }
                    Err(err) => {
                        self.settings.display_status =
                            Some(format!("Compositor off after restart: {err}"));
                    }
                }
            } else {
                self.settings.display_status = Some("Compositor disabled".to_string());
            }
        }
        save_app_commands(&self.settings)?;
        Ok(())
    }

    pub(crate) fn set_power_mode(&mut self, mode: PowerMode) -> AnyResult<()> {
        if read_cached_power_mode() == Some(mode) {
            self.settings.power_mode = mode;
            return Ok(());
        }
        if let Some(_lock) = try_tmp_file_lock(
            POWER_PROFILE_LOCK_PATH,
            IDLE_CHECK_INTERVAL + Duration::from_secs(5),
        ) {
            if read_cached_power_mode() != Some(mode) {
                write_power_mode_cache(mode)?;
                let mut cmd = Command::new("powerprofilesctl");
                cmd.args(["set", mode.command_value()]);
                spawn_detached(cmd);
            }
        }
        self.settings.power_mode = mode;
        Ok(())
    }

    pub(crate) fn selected_app_command(&self, kind: DefaultAppKind) -> &str {
        match kind {
            DefaultAppKind::Terminal => &self.settings.terminal_command,
            DefaultAppKind::Browser => &self.settings.browser_command,
            DefaultAppKind::Photo => &self.settings.photo_command,
            DefaultAppKind::Video => &self.settings.video_command,
        }
    }

    pub(crate) fn available_apps(&self, kind: DefaultAppKind) -> &[InstalledApp] {
        match kind {
            DefaultAppKind::Terminal => &self.terminal_apps,
            DefaultAppKind::Browser => &self.browser_apps,
            DefaultAppKind::Photo => &self.photo_apps,
            DefaultAppKind::Video => &self.video_apps,
        }
    }

    pub(crate) fn set_selected_app_command(&mut self, kind: DefaultAppKind, command: String) {
        match kind {
            DefaultAppKind::Terminal => self.settings.terminal_command = command,
            DefaultAppKind::Browser => self.settings.browser_command = command,
            DefaultAppKind::Photo => self.settings.photo_command = command,
            DefaultAppKind::Video => self.settings.video_command = command,
        }
    }

    pub(crate) fn test_terminal_launch(&mut self, command: &str, label: &str) {
        self.settings.app_status = Some(
            if !command.trim().is_empty() && self.spawn_configured_app(command, None) {
                format!("Launched {label}.")
            } else {
                format!("Could not launch {label}; try another terminal.")
            },
        );
    }

    pub(crate) fn launch_terminal(&mut self) {
        let selected = if self.settings.terminal_command.trim().is_empty() {
            if let Some(files_bin) = aurora_files_binary() {
                // Prefer the bundled Aurora Files tabbed terminal.
                format!("\"{files_bin}\" --terminal")
            } else {
                self.available_apps(DefaultAppKind::Terminal)
                    .first()
                    .map(|app| app.command.clone())
                    .unwrap_or_default()
            }
        } else {
            self.settings.terminal_command.clone()
        };
        if !self.spawn_configured_app(&selected, None) {
            self.settings.app_status =
                Some("Could not launch terminal; select another one in Apps settings.".to_string());
        }
    }

    pub(crate) fn launch_browser(&mut self) {
        let selected = if self.settings.browser_command.trim().is_empty() {
            self.available_apps(DefaultAppKind::Browser)
                .first()
                .map(|app| app.command.clone())
                .unwrap_or_default()
        } else {
            self.settings.browser_command.clone()
        };
        if !self.spawn_configured_app(&selected, None) {
            self.settings.app_status =
                Some("Could not launch browser; select another one in Apps settings.".to_string());
        }
    }

    pub(crate) fn launch_desktop_app_matching(
        &mut self,
        name_fragments: &[&str],
        fallback_commands: &[&str],
    ) -> bool {
        let entries = read_desktop_entries();
        for fragment in name_fragments {
            if let Some(entry) = entries.iter().find(|entry| {
                entry.name.to_ascii_lowercase().contains(&fragment.to_ascii_lowercase())
                    && !entry.command.trim().is_empty()
            }) {
                if self.spawn_configured_app(&entry.command, None) {
                    return true;
                }
            }
        }
        fallback_commands
            .iter()
            .any(|command| self.spawn_configured_app(command, None))
    }

    pub(crate) fn spawn_configured_app(&self, command: &str, path: Option<&Path>) -> bool {
        if command.trim().is_empty() {
            return false;
        }
        let mut cmd = Command::new("sh");
        cmd.env("DISPLAY", &self.display)
            .arg("-c")
            .arg(if path.is_some() {
                format!("exec {command} \"$1\"")
            } else {
                format!("exec {command}")
            })
            .arg("aurora-launch");
        if let Some(path) = path {
            cmd.arg(path);
        }
        apply_pulse_env_defaults(&mut cmd);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let Ok(mut child) = cmd.spawn() else {
            return false;
        };
        thread::sleep(Duration::from_millis(120));
        match child.try_wait() {
            Ok(Some(status)) => status.success(),
            Ok(None) => {
                thread::spawn(move || {
                    let _ = child.wait();
                });
                true
            }
            Err(_) => false,
        }
    }

    /// Launch the standalone Aurora Files app at `path`. Returns false when
    /// the binary is not available (callers fall back to the built-in UI).
    pub(crate) fn launch_file_manager(&self, path: &Path) -> bool {
        let Some(files_bin) = aurora_files_binary() else {
            return false;
        };
        let mut cmd = Command::new(files_bin);
        cmd.env("DISPLAY", &self.display).arg(path);
        spawn_detached(cmd);
        true
    }

    /// Open `path` as a new tab in the active Aurora Files window. If no
    /// suitable window exists, launch Aurora Files normally.
    pub(crate) fn open_file_manager_tab(&mut self, path: &Path) -> bool {
        let is_files_window = |window: Window| {
            self.clients.get(&window).is_some_and(|info| {
                self.window_class(info.window).contains("aurora-files")
                    && self.window_title(info.window) == "Aurora Files"
            })
        };
        let target = self
            .active_client
            .filter(|window| is_files_window(*window))
            .or_else(|| {
                self.focus_history
                    .iter()
                    .rev()
                    .copied()
                    .find(|window| is_files_window(*window))
            })
            .or_else(|| {
                self.clients
                    .keys()
                    .copied()
                    .find(|window| is_files_window(*window))
            });
        let Some(target) = target else {
            return self.launch_file_manager(path);
        };
        let Some(info) = self.clients.get(&target).copied() else {
            return self.launch_file_manager(path);
        };
        let Ok(message_atom) = self.atom(b"_AURORA_FILES_OPEN_FOLDER_TAB") else {
            return self.launch_file_manager(path);
        };
        let Ok(path_atom) = self.atom(b"_AURORA_FILES_FOLDER_TAB_PATH") else {
            return self.launch_file_manager(path);
        };
        let Ok(utf8_atom) = self.atom(b"UTF8_STRING") else {
            return self.launch_file_manager(path);
        };
        if self
            .conn
            .change_property8(
                PropMode::REPLACE,
                info.window,
                path_atom,
                utf8_atom,
                path.to_string_lossy().as_bytes(),
            )
            .is_err()
        {
            return self.launch_file_manager(path);
        }
        let event = ClientMessageEvent::new(32, info.window, message_atom, [0, 0, 0, 0, 0]);
        if self
            .conn
            .send_event(false, info.window, EventMask::NO_EVENT, event)
            .is_err()
            || self.conn.flush().is_err()
        {
            return self.launch_file_manager(path);
        }
        let _ = self.focus_window(target);
        true
    }

    /// Launch Aurora Files with its terminal focused.
    pub(crate) fn launch_file_manager_terminal(&self, path: &Path) -> bool {
        let Some(files_bin) = aurora_files_binary() else {
            return false;
        };
        let mut cmd = Command::new(files_bin);
        cmd.env("DISPLAY", &self.display)
            .arg("--terminal")
            .arg(path);
        spawn_detached(cmd);
        true
    }

    /// Launch a minimal image-only screenshot viewer. The viewer owns its
    /// timeout and exits automatically after one minute.
    pub(crate) fn launch_screenshot_viewer(&self, path: &Path) -> bool {
        let Some(files_bin) = aurora_files_binary() else {
            return false;
        };
        let mut cmd = Command::new(files_bin);
        cmd.env("DISPLAY", &self.display)
            .arg("--image-viewer")
            .arg(path);
        spawn_detached(cmd);
        true
    }

    pub(crate) fn spawn_first_available(&self, names: &[&str], args: &[&str]) {
        for name in names {
            if command_exists(name) {
                let mut cmd = Command::new(name);
                cmd.env("DISPLAY", &self.display).args(args);
                apply_pulse_env_defaults(&mut cmd);
                spawn_detached(cmd);
                break;
            }
        }
    }

}

/// Screen-sleep trace line in the session log (rare: a few per sleep).
pub(crate) fn sleep_log(msg: &str) {
    let now = OffsetDateTime::now_utc();
    eprintln!(
        "aurora-wm: [{:02}:{:02}:{:02}] sleep: {msg}",
        now.hour(),
        now.minute(),
        now.second()
    );
}

/// "bl_power=N brightness=N" of every backlight.
pub(crate) fn backlight_summary() -> String {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/backlight") {
        for entry in entries.flatten() {
            let read = |f: &str| {
                fs::read_to_string(entry.path().join(f)).map(|v| v.trim().to_string()).unwrap_or_else(|_| "?".into())
            };
            out.push(format!("bl_power={} brightness={}", read("bl_power"), read("brightness")));
        }
    }
    out.join("; ")
}
