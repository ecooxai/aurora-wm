#![allow(unused_imports)]
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

const TOPBAR_HEIGHT: u16 = 40;
const DOCK_ICON_SIZE: i32 = 44;
const DOCK_ICON_RADIUS: i32 = 12;
const DOCK_STRIDE: i32 = 50;
const DOCK_HEIGHT: u16 = DOCK_ICON_SIZE as u16;
const DOCK_BOTTOM_MARGIN: u16 = 12;
const TITLEBAR_HEIGHT: u16 = 34;
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const COMPOSITED_MOVE_INTERVAL: Duration = Duration::from_millis(16);
const NON_COMPOSITED_MOVE_INTERVAL: Duration = Duration::from_millis(8);
const NOT_IDLE_MARKER_PATH: &str = "/tmp/notidle";
const POWER_PROFILE_CACHE_PATH: &str = "/tmp/aurora-power-profile";
const POWER_PROFILE_LOCK_PATH: &str = "/tmp/aurora-power-profile.lock";
const FRAME_CORNER_RADIUS: i32 = 8;
const TERMINAL_HISTORY_LIMIT: usize = 1000;
const CLIPBOARD_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const SETTINGS_MIN_WIDTH: u16 = 420;
const SETTINGS_TARGET_WIDTH: u16 = 600;
const SETTINGS_MARGIN: u16 = 24;
const SIDEBAR_WIDTH: i32 = 58;
const SETTINGS_SIDEBAR_TOP: i32 = 26;
const MEDIA_SLOT_COUNT: usize = 5;
const MEDIA_WIDTH: u16 = 600;
const MEDIA_WINDOW_NUDGE_WIDTH: u16 = 1;
const RESIZE_EDGE: i16 = 1;
const RESIZE_CORNER: i16 = 28;
const FOLDER_HEADER_ICON: i32 = 30;
const FOLDER_TERMINAL_DEFAULT_COLS: usize = 90;
const FOLDER_TERMINAL_DEFAULT_ROWS: usize = 18;
const FOLDER_TERMINAL_CELL_W: i32 = 8;
const FOLDER_TERMINAL_CELL_H: i32 = 18;
const FOLDER_ENTRY_LIMIT: usize = 512;
const FOLDER_OTHER_ENTRY_LIMIT: usize = 64;
const AUTO_POWER_SAVER_MIN_MINUTES: u32 = 1;
const AUTO_POWER_SAVER_MAX_MINUTES: u32 = 1000;
const AUTO_POWER_SAVER_STEP_MINUTES: u32 = 50;
const TERMINAL_FALLBACKS: [&str; 5] = [
    "xfce4-terminal",
    "lxterminal",
    "gnome-terminal",
    "konsole",
    "xterm",
];
const FONT_REGULAR: &[u8] = include_bytes!("../fonts/NotoSans-Regular.ttf");
const FONT_BOLD: &[u8] = include_bytes!("../fonts/NotoSans-Bold.ttf");
const FONT_TERMINAL_REGULAR: &[u8] = include_bytes!("../fonts/NotoSansMono-Regular.ttf");
const FONT_TERMINAL_BOLD: &[u8] = include_bytes!("../fonts/NotoSansMono-Bold.ttf");

mod canvas;
mod wm_extras;
mod model;
mod wm_core;
mod events;
mod clients;
mod draw_chrome;
mod draw_settings;
mod workspaces;
mod clipboard_ui;
mod wifi_ui;
mod settings_events;
mod keys;
mod dock_menus;
mod folder_ui;
mod screenshot;
mod terminal_ui;
mod folder_actions;
mod media_ui;
mod system_apply;
mod layout;
mod draw_helpers;
mod pixels;
mod system;
mod textutil;
mod procutil;
mod files;
use canvas::*;
use wm_extras::*;
use model::*;
use wm_core::*;
use events::*;
use clients::*;
use draw_chrome::*;
use draw_settings::*;
use workspaces::*;
use clipboard_ui::*;
use wifi_ui::*;
use settings_events::*;
use keys::*;
use dock_menus::*;
use folder_ui::*;
use screenshot::*;
use terminal_ui::*;
use folder_actions::*;
use media_ui::*;
use system_apply::*;
use layout::*;
use draw_helpers::*;
use pixels::*;
use system::*;
use textutil::*;
use procutil::*;
use files::*;

fn main() {
    if let Err(err) = run() {
        eprintln!("aurora-wm: {err}");
        std::process::exit(1);
    }
}

fn run() -> AnyResult<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() >= 3 && args[1] == "--open-folder" {
        let display = env::var("DISPLAY").unwrap_or_else(|_| ":11".to_string());
        let (conn, _screen_num) = RustConnection::connect(Some(&display))?;
        let setup = conn.setup();
        let root = setup.roots[0].root;
        let path_atom = conn
            .intern_atom(false, b"_AURORA_OPEN_FOLDER_PATH")?
            .reply()?
            .atom;
        let string_atom = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let path = std::path::PathBuf::from(&args[2]);
        let abs_path = std::fs::canonicalize(&path).unwrap_or(path);
        let path_str = abs_path.to_string_lossy().into_owned();
        conn.change_property8(
            PropMode::REPLACE,
            root,
            path_atom,
            string_atom,
            path_str.as_bytes(),
        )?;
        let open_atom = conn
            .intern_atom(false, b"_AURORA_OPEN_FOLDER")?
            .reply()?
            .atom;
        let event = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: root,
            type_: open_atom,
            data: ClientMessageData::from([0, 0, 0, 0, 0]),
        };
        conn.send_event(false, root, EventMask::STRUCTURE_NOTIFY, event)?;
        conn.flush()?;
        println!("Requested folder opening for {}", path_str);
        return Ok(());
    }

    if args.len() >= 2 && args[1] == "--choose-file" {
        let display = env::var("DISPLAY").unwrap_or_else(|_| ":11".to_string());
        let (conn, _screen_num) = RustConnection::connect(Some(&display))?;
        let setup = conn.setup();
        let root = setup.roots[0].root;
        let result_atom = conn
            .intern_atom(false, b"_AURORA_CHOOSE_FILE_RESULT")?
            .reply()?
            .atom;
        conn.delete_property(root, result_atom)?;

        let choose_atom = conn
            .intern_atom(false, b"_AURORA_CHOOSE_FILE")?
            .reply()?
            .atom;
        let event = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: root,
            type_: choose_atom,
            data: ClientMessageData::from([0, 0, 0, 0, 0]),
        };
        conn.send_event(false, root, EventMask::STRUCTURE_NOTIFY, event)?;
        conn.flush()?;

        let string_atom = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let start_time = std::time::Instant::now();
        loop {
            if let Ok(prop) = conn
                .get_property(false, root, result_atom, string_atom, 0, 65535)?
                .reply()
            {
                if !prop.value.is_empty() {
                    let result_str = String::from_utf8_lossy(&prop.value);
                    if result_str == "CANCEL" {
                        eprintln!("File selection cancelled.");
                        conn.delete_property(root, result_atom)?;
                        std::process::exit(1);
                    } else {
                        println!("{}", result_str);
                        conn.delete_property(root, result_atom)?;
                        return Ok(());
                    }
                }
            }
            if start_time.elapsed() > std::time::Duration::from_secs(300) {
                eprintln!("File selection timed out.");
                std::process::exit(1);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    let replace = args.iter().any(|arg| arg == "--replace");
    let compositor_override = parse_compositor_arg(&args)?;

    let display = env::var("DISPLAY").unwrap_or_else(|_| ":111".to_string());
    let (conn, screen_num) = RustConnection::connect(Some(&display))?;
    let screen = conn.setup().roots[screen_num].clone();

    // Acquire WM_S<screen_num> selection to announce presence and/or replace existing WM
    let selection_name = format!("WM_S{}", screen_num);
    let wm_s_atom = conn
        .intern_atom(false, selection_name.as_bytes())?
        .reply()?
        .atom;

    let wm_window = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_FROM_PARENT as u8,
        wm_window,
        screen.root,
        -10,
        -10,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new(),
    )?;

    if replace {
        conn.set_selection_owner(wm_window, wm_s_atom, CURRENT_TIME)?;
        let manager_atom = conn.intern_atom(false, b"MANAGER")?.reply()?.atom;
        let client_message = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: screen.root,
            type_: manager_atom,
            data: ClientMessageData::from([CURRENT_TIME, wm_s_atom, wm_window, 0, 0]),
        };
        conn.send_event(
            false,
            screen.root,
            EventMask::STRUCTURE_NOTIFY,
            client_message,
        )?;
    }

    // Now try to become WM (with retries if --replace)
    let mut retry_count = 0;
    loop {
        match become_wm(&conn, &screen) {
            Ok(()) => break,
            Err(err) => {
                if replace && retry_count < 15 {
                    retry_count += 1;
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    continue;
                }
                if let ReplyError::X11Error(ref x11_err) = err {
                    if x11_err.error_kind == ErrorKind::Access {
                        eprintln!("Another window manager already owns this X display.");
                    }
                }
                return Err(err.into());
            }
        }
    }

    if !replace {
        conn.set_selection_owner(wm_window, wm_s_atom, CURRENT_TIME)?;
    }

    let mut app = Aurora::new(conn, display, &screen, screen_num, compositor_override)?;
    app.scan_existing_windows()?;
    app.redraw_everything()?;
    app.run_loop()
}

fn become_wm(conn: &RustConnection, screen: &Screen) -> Result<(), ReplyError> {
    let mask = EventMask::SUBSTRUCTURE_REDIRECT
        | EventMask::SUBSTRUCTURE_NOTIFY
        | EventMask::STRUCTURE_NOTIFY
        | EventMask::EXPOSURE
        | EventMask::PROPERTY_CHANGE
        | EventMask::BUTTON_PRESS
        | EventMask::KEY_RELEASE;
    conn.change_window_attributes(
        screen.root,
        &ChangeWindowAttributesAux::new().event_mask(mask),
    )?
    .check()
}

fn event_name(event: &Event) -> &'static str {
    match event {
        Event::KeyPress(_) => "KeyPress",
        Event::KeyRelease(_) => "KeyRelease",
        Event::ButtonPress(_) => "ButtonPress",
        Event::ButtonRelease(_) => "ButtonRelease",
        Event::MotionNotify(_) => "MotionNotify",
        Event::EnterNotify(_) => "EnterNotify",
        Event::LeaveNotify(_) => "LeaveNotify",
        Event::FocusIn(_) => "FocusIn",
        Event::FocusOut(_) => "FocusOut",
        Event::Expose(_) => "Expose",
        Event::GraphicsExposure(_) => "GraphicsExposure",
        Event::NoExposure(_) => "NoExposure",
        Event::VisibilityNotify(_) => "VisibilityNotify",
        Event::CreateNotify(_) => "CreateNotify",
        Event::DestroyNotify(_) => "DestroyNotify",
        Event::UnmapNotify(_) => "UnmapNotify",
        Event::MapNotify(_) => "MapNotify",
        Event::MapRequest(_) => "MapRequest",
        Event::ReparentNotify(_) => "ReparentNotify",
        Event::ConfigureNotify(_) => "ConfigureNotify",
        Event::ConfigureRequest(_) => "ConfigureRequest",
        Event::GravityNotify(_) => "GravityNotify",
        Event::ResizeRequest(_) => "ResizeRequest",
        Event::CirculateNotify(_) => "CirculateNotify",
        Event::CirculateRequest(_) => "CirculateRequest",
        Event::PropertyNotify(_) => "PropertyNotify",
        Event::SelectionClear(_) => "SelectionClear",
        Event::SelectionRequest(_) => "SelectionRequest",
        Event::SelectionNotify(_) => "SelectionNotify",
        Event::ColormapNotify(_) => "ColormapNotify",
        Event::ClientMessage(_) => "ClientMessage",
        Event::MappingNotify(_) => "MappingNotify",
        _ => "Other",
    }
}

fn wait_for_x_event_or_timeout(conn: &RustConnection, timeout: Duration) {
    let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    let mut poll_fd = libc::pollfd {
        fd: conn.stream().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let rc = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if rc >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            break;
        }
    }
}

fn parse_compositor_arg(args: &[String]) -> AnyResult<Option<bool>> {
    let mut override_value = None;
    let mut idx = 1;
    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--compositor" {
            let value = args.get(idx + 1).ok_or("--compositor requires yes or no")?;
            override_value = Some(parse_compositor_value(value)?);
            idx += 2;
        } else if let Some(value) = arg.strip_prefix("--compositor=") {
            override_value = Some(parse_compositor_value(value)?);
            idx += 1;
        } else {
            idx += 1;
        }
    }
    Ok(override_value)
}

fn parse_compositor_value(value: &str) -> AnyResult<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "yes" | "on" | "true" | "1" => Ok(true),
        "no" | "off" | "false" | "0" => Ok(false),
        _ => Err(format!("invalid --compositor value {value:?}; use yes or no").into()),
    }
}

fn init_light_compositor(conn: &RustConnection, root: Window) -> bool {
    let Ok(Some(_)) = conn.extension_information(composite::X11_EXTENSION_NAME) else {
        eprintln!("aurora-wm: Composite extension unavailable; compositor disabled");
        return false;
    };
    let Ok(cookie) = conn.composite_query_version(0, 4) else {
        eprintln!("aurora-wm: Composite version query failed; compositor disabled");
        return false;
    };
    if cookie.reply().is_err() {
        eprintln!("aurora-wm: Composite version query failed; compositor disabled");
        return false;
    }
    let Ok(cookie) = conn.composite_redirect_subwindows(root, composite::Redirect::AUTOMATIC)
    else {
        eprintln!("aurora-wm: light compositor disabled: redirect request failed");
        return false;
    };
    match cookie.check() {
        Ok(()) => {
            eprintln!("aurora-wm: light compositor enabled");
            true
        }
        Err(err) => {
            eprintln!("aurora-wm: light compositor disabled: {err}");
            false
        }
    }
}

fn disable_light_compositor(conn: &RustConnection, root: Window) -> AnyResult<()> {
    conn.composite_unredirect_subwindows(root, composite::Redirect::AUTOMATIC)?
        .check()?;
    conn.flush()?;
    eprintln!("aurora-wm: light compositor disabled");
    Ok(())
}
