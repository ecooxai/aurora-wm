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
        let mut cmd = Command::new("xset");
        cmd.env("DISPLAY", &self.display);
        if self.settings.sleep_after_secs == 0 {
            cmd.args(["s", "off"]);
        } else {
            cmd.args(["s", &self.settings.sleep_after_secs.to_string()]);
        }
        spawn_detached(cmd);
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
            self.available_apps(DefaultAppKind::Terminal)
                .first()
                .map(|app| app.command.clone())
                .unwrap_or_default()
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
