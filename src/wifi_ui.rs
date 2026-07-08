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
    pub(crate) fn ensure_wifi_refresh_started(&mut self, rescan: bool) {
        if self.wifi_refresh_rx.is_some() {
            return;
        }
        if rescan
            || self.settings.wifi_networks.is_empty()
            || self.settings.wifi_radio_enabled.is_none()
        {
            self.start_wifi_refresh(rescan);
        }
    }

    pub(crate) fn start_wifi_refresh(&mut self, rescan: bool) {
        self.settings.wifi_status = Some(
            if rescan {
                "Refreshing Wi-Fi networks..."
            } else {
                "Loading Wi-Fi networks..."
            }
            .to_string(),
        );

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let radio_enabled = read_wifi_radio_enabled();
            let connected = read_connected_wifi();
            let networks = radio_enabled.then(|| scan_wifi_networks(rescan));
            let _ = tx.send(WifiRefreshResult {
                radio_enabled,
                connected,
                networks,
            });
        });
        self.wifi_refresh_rx = Some(rx);
    }

    pub(crate) fn poll_wifi_refresh(&mut self) -> AnyResult<bool> {
        let Some(rx) = self.wifi_refresh_rx.as_ref() else {
            return Ok(false);
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return Ok(false),
            Err(TryRecvError::Disconnected) => {
                self.wifi_refresh_rx = None;
                self.settings.wifi_status = Some("Wi-Fi refresh stopped".to_string());
                if self.settings_visible && self.settings.tab == SettingsTab::Network {
                    self.redraw_settings()?;
                }
                return Ok(true);
            }
        };
        self.wifi_refresh_rx = None;
        self.apply_wifi_refresh_result(result);
        if self.settings_visible && self.settings.tab == SettingsTab::Network {
            self.redraw_settings()?;
        }
        Ok(true)
    }

    pub(crate) fn apply_wifi_refresh_result(&mut self, result: WifiRefreshResult) {
        self.settings.wifi_radio_enabled = Some(result.radio_enabled);
        self.settings.wifi_connected = Some(result.connected);
        if !result.radio_enabled {
            self.settings.wifi_networks.clear();
            self.settings.wifi_scroll = 0;
            self.settings.wifi_selected = None;
            self.settings.wifi_password.clear();
            self.settings.wifi_password_editing = false;
            self.settings.wifi_status = Some("Wi-Fi is off".to_string());
            return;
        }

        match result.networks {
            Some(Ok(networks)) => {
                self.settings.wifi_networks = networks;
                self.settings.wifi_scroll = 0;
                self.settings.wifi_status = Some(format!(
                    "Found {} Wi-Fi network{}",
                    self.settings.wifi_networks.len(),
                    if self.settings.wifi_networks.len() == 1 {
                        ""
                    } else {
                        "s"
                    }
                ));
                if let Some(selected) = self.settings.wifi_selected.as_deref() {
                    if !self
                        .settings
                        .wifi_networks
                        .iter()
                        .any(|network| network.ssid == selected)
                    {
                        self.settings.wifi_selected = None;
                        self.settings.wifi_password.clear();
                        self.settings.wifi_password_editing = false;
                    }
                }
            }
            Some(Err(err)) => {
                self.settings.wifi_networks.clear();
                self.settings.wifi_scroll = 0;
                self.settings.wifi_status = Some(err);
            }
            None => {}
        }
    }

    pub(crate) fn connect_selected_wifi(&mut self) -> AnyResult<()> {
        let Some(ssid) = self.settings.wifi_selected.clone() else {
            return Ok(());
        };
        self.settings.wifi_status = Some(format!("Connecting to {ssid}..."));
        self.redraw_settings()?;
        self.settings.wifi_status = match connect_wifi_network(&ssid, &self.settings.wifi_password)
        {
            Ok(()) => Some(format!("Connection requested for {ssid}")),
            Err(err) => Some(err),
        };
        self.settings.wifi_password_editing = false;
        self.conn
            .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
        self.redraw_settings()
    }

    pub(crate) fn disconnect_wifi(&mut self) -> AnyResult<()> {
        self.settings.wifi_status = Some("Disconnecting Wi-Fi...".to_string());
        self.settings.wifi_disconnect_confirm = false;
        self.redraw_settings()?;
        self.settings.wifi_status = match disconnect_current_wifi() {
            Ok(()) => Some("Wi-Fi disconnect requested".to_string()),
            Err(err) => Some(err),
        };
        self.redraw_settings()
    }

}
