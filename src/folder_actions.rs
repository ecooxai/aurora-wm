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
    pub(crate) fn folder_context_action_at(&self, x: i32, y: i32) -> Option<FolderContextAction> {
        let (_, _, w, h) = self.folder_geometry();
        let menu_x = self.folder_context_pos.0.min(i32::from(w) - 166).max(10);
        let menu_y = self.folder_context_pos.1.min(i32::from(h) - 178).max(78);
        if x < menu_x || x > menu_x + 156 || y < menu_y || y > menu_y + 164 {
            return None;
        }
        match (y - menu_y - 8) / 29 {
            0 => Some(FolderContextAction::OpenExternal),
            1 => Some(FolderContextAction::Copy),
            2 => Some(FolderContextAction::Cut),
            3 => Some(FolderContextAction::Paste),
            4 => Some(FolderContextAction::Info),
            _ => None,
        }
    }

    pub(crate) fn run_folder_context_action(&mut self, action: FolderContextAction) -> AnyResult<()> {
        match action {
            FolderContextAction::Copy => {
                if let Some(path) = self.folder_selected.clone() {
                    self.folder_clipboard = Some((path, false));
                    self.folder_info = Some("Copied".to_string());
                }
            }
            FolderContextAction::Cut => {
                if let Some(path) = self.folder_selected.clone() {
                    self.folder_clipboard = Some((path, true));
                    self.folder_info = Some("Cut".to_string());
                }
            }
            FolderContextAction::Paste => {
                if let Some((src, cut)) = self.folder_clipboard.clone() {
                    let dst = self.folder_path.join(src.file_name().unwrap_or_default());
                    if cut {
                        let _ = fs::rename(&src, &dst);
                        self.folder_clipboard = None;
                    } else if src.is_file() {
                        let _ = fs::copy(&src, &dst);
                    }
                    self.refresh_folder_entries();
                    self.folder_info = Some("Pasted".to_string());
                }
            }
            FolderContextAction::Info => {
                if let Some(path) = self.folder_selected.as_ref() {
                    let meta = fs::metadata(path).ok();
                    self.folder_info = Some(format!(
                        "{}  {} bytes",
                        path.file_name().and_then(|n| n.to_str()).unwrap_or("Item"),
                        meta.map(|m| m.len()).unwrap_or(0)
                    ));
                }
            }
            FolderContextAction::OpenExternal => {
                if let Some(path) = self.folder_selected.as_ref() {
                    let mut cmd = Command::new("xdg-open");
                    cmd.env("DISPLAY", &self.display).arg(path);
                    apply_pulse_env_defaults(&mut cmd);
                    spawn_detached(cmd);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn stop_ffplay_process(&mut self) {
        let Some(mut child) = self.ffplay_process.take() else {
            return;
        };
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub(crate) fn reap_ffplay_process(&mut self) {
        if self
            .ffplay_process
            .as_mut()
            .and_then(|child| child.try_wait().ok())
            .flatten()
            .is_some()
        {
            self.ffplay_process = None;
        }
    }

}
