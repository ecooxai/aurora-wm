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
use crate::system_apply::*;
use crate::layout::*;
use crate::draw_helpers::*;
use crate::pixels::*;
use crate::system::*;
use crate::textutil::*;
use crate::procutil::*;
use crate::files::*;

pub(crate) static WALLPAPERS: &[WallpaperAsset] = &[
    WallpaperAsset {
        name: "Signal shore",
        bytes: include_bytes!("../wallpaper/f7d4b278-3aef-4a94-b84e-f14acde427ac.png"),
    },
    WallpaperAsset {
        name: "Glass morning",
        bytes: include_bytes!("../wallpaper/e8436a5b-364d-4ccd-b7be-44de6b5c4da7.png"),
    },
    WallpaperAsset {
        name: "Violet rooftop",
        bytes: include_bytes!("../wallpaper/0e8ff753-7bc4-4ee2-a7f2-ce67a2d41677.png"),
    },
];

#[derive(Clone, Copy)]
pub(crate) struct WallpaperAsset {
    pub(crate) name: &'static str,
    pub(crate) bytes: &'static [u8],
}

#[derive(Clone)]
pub(crate) struct DisplayMode {
    pub(crate) output: Option<String>,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) refresh: Option<f32>,
    pub(crate) current: bool,
}

impl DisplayMode {
    pub(crate) fn label(&self) -> String {
        match self.refresh {
            Some(rate) => format!("{}x{}  {:.0} Hz", self.width, self.height, rate),
            None => format!("{}x{}", self.width, self.height),
        }
    }
}

#[derive(Clone)]
pub(crate) struct AudioDevice {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) label: String,
    pub(crate) is_default: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioDeviceKind {
    Output,
    Input,
}

impl AudioDeviceKind {
    pub(crate) fn pactl_list_arg(self) -> &'static str {
        match self {
            Self::Output => "sinks",
            Self::Input => "sources",
        }
    }

    pub(crate) fn pactl_default_key(self) -> &'static str {
        match self {
            Self::Output => "Default Sink:",
            Self::Input => "Default Source:",
        }
    }

    pub(crate) fn pactl_set_default_command(self) -> &'static str {
        match self {
            Self::Output => "set-default-sink",
            Self::Input => "set-default-source",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    Display,
    Power,
    Wallpaper,
    Audio,
    Network,
    Bluetooth,
    Startup,
    Apps,
    Shortcuts,
    About,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DefaultAppKind {
    Terminal,
    Browser,
    Photo,
    Video,
}

impl DefaultAppKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Terminal => "Terminal",
            Self::Browser => "Browser",
            Self::Photo => "Photos",
            Self::Video => "Videos",
        }
    }

    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Browser => "browser",
            Self::Photo => "photo",
            Self::Video => "video",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PowerMode {
    Saver,
    Balanced,
    Performance,
}

impl PowerMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Saver => "Battery saver",
            Self::Balanced => "Balanced",
            Self::Performance => "Performance",
        }
    }

    pub(crate) fn command_value(self) -> &'static str {
        match self {
            Self::Saver => "power-saver",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }

    pub(crate) fn from_command_value(value: &str) -> Option<Self> {
        match value.trim() {
            "power-saver" => Some(Self::Saver),
            "balanced" => Some(Self::Balanced),
            "performance" => Some(Self::Performance),
            _ => None,
        }
    }
}

pub(crate) struct SettingsState {
    pub(crate) tab: SettingsTab,
    pub(crate) sleep_after_secs: u32,
    pub(crate) brightness_percent: u8,
    pub(crate) compositor_enabled: bool,
    pub(crate) power_mode: PowerMode,
    pub(crate) auto_power_saver_enabled: bool,
    pub(crate) auto_power_saver_minutes: u32,
    pub(crate) auto_power_saver_input: String,
    pub(crate) auto_power_saver_editing: bool,
    pub(crate) auto_power_saver_slider_dragging: bool,
    pub(crate) selected_mode: usize,
    pub(crate) scroll: i32,
    pub(crate) app_kind: DefaultAppKind,
    pub(crate) terminal_command: String,
    pub(crate) browser_command: String,
    pub(crate) photo_command: String,
    pub(crate) video_command: String,
    pub(crate) terminal_editing: bool,
    pub(crate) app_status: Option<String>,
    pub(crate) display_status: Option<String>,
    pub(crate) audio_status: Option<String>,
    pub(crate) wifi_networks: Vec<WifiNetwork>,
    pub(crate) wifi_scroll: usize,
    pub(crate) wifi_selected: Option<String>,
    pub(crate) wifi_password: String,
    pub(crate) wifi_password_editing: bool,
    pub(crate) wifi_status: Option<String>,
    pub(crate) wifi_disconnect_confirm: bool,
    pub(crate) wifi_radio_enabled: Option<bool>,
    pub(crate) wifi_connected: Option<Option<WifiConnection>>,
    pub(crate) shortcuts: ShortcutConfig,
    pub(crate) shortcut_capture: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShortcutSpec {
    pub(crate) ctrl: bool,
    pub(crate) alt: bool,
    pub(crate) shift: bool,
    pub(crate) super_key: bool,
    pub(crate) keysym: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShortcutConfig {
    pub(crate) folder: ShortcutSpec,
    pub(crate) terminal: ShortcutSpec,
    pub(crate) clipboard: ShortcutSpec,
    pub(crate) screenshot: ShortcutSpec,
}

impl Default for SettingsState {
    fn default() -> Self {
        let auto_power_saver_minutes = read_u32_setting("auto_power_saver_minutes", 50)
            .clamp(AUTO_POWER_SAVER_MIN_MINUTES, AUTO_POWER_SAVER_MAX_MINUTES);
        let auto_power_saver_enabled =
            read_bool_setting("auto_power_saver_enabled", auto_power_saver_minutes > 0);
        Self {
            tab: SettingsTab::Display,
            sleep_after_secs: read_u32_setting("sleep_after_secs", 600).min(7200),
            brightness_percent: read_u32_setting("brightness_percent", 100).clamp(10, 100) as u8,
            compositor_enabled: read_bool_setting("compositor_enabled", false),
            power_mode: read_current_power_mode().unwrap_or(PowerMode::Balanced),
            auto_power_saver_enabled,
            auto_power_saver_minutes,
            auto_power_saver_input: auto_power_saver_minutes.to_string(),
            auto_power_saver_editing: false,
            auto_power_saver_slider_dragging: false,
            selected_mode: 0,
            scroll: 0,
            app_kind: DefaultAppKind::Terminal,
            terminal_command: read_app_command(DefaultAppKind::Terminal),
            browser_command: read_app_command(DefaultAppKind::Browser),
            photo_command: read_app_command(DefaultAppKind::Photo),
            video_command: read_app_command(DefaultAppKind::Video),
            terminal_editing: false,
            app_status: None,
            display_status: None,
            audio_status: None,
            wifi_networks: Vec::new(),
            wifi_scroll: 0,
            wifi_selected: None,
            wifi_password: String::new(),
            wifi_password_editing: false,
            wifi_status: None,
            wifi_disconnect_confirm: false,
            wifi_radio_enabled: None,
            wifi_connected: None,
            shortcuts: read_shortcut_config(),
            shortcut_capture: None,
        }
    }
}

#[derive(Default, Clone)]
pub(crate) struct Metrics {
    pub(crate) cpu_model: String,
    pub(crate) cpu_usage: f32,
    pub(crate) cpu_status: String,
    pub(crate) cpu_frequencies: Vec<String>,
    pub(crate) ram_total_kb: u64,
    pub(crate) ram_used_kb: u64,
    pub(crate) swap_total_kb: u64,
    pub(crate) swap_used_kb: u64,
    pub(crate) gpus: Vec<String>,
    pub(crate) gpu_usage: Vec<GpuUsage>,
    pub(crate) nics: Vec<String>,
    pub(crate) net_rx_bps: f64,
    pub(crate) net_tx_bps: f64,
    pub(crate) battery: Option<String>,
}

#[derive(Clone)]
pub(crate) struct GpuUsage {
    pub(crate) name: String,
    pub(crate) percent: f32,
}

#[derive(Clone, Copy)]
pub(crate) struct CpuTimes {
    pub(crate) idle: u64,
    pub(crate) total: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct NetTotals {
    pub(crate) rx: u64,
    pub(crate) tx: u64,
    pub(crate) at: Instant,
}

pub(crate) struct SystemSampler {
    pub(crate) prev_cpu: Option<CpuTimes>,
    pub(crate) prev_net: Option<NetTotals>,
    pub(crate) cpu_model: String,
    pub(crate) gpus: Vec<String>,
    pub(crate) nics: Vec<String>,
}

impl SystemSampler {
    pub(crate) fn new() -> Self {
        Self {
            prev_cpu: None,
            prev_net: None,
            cpu_model: read_cpu_model(),
            gpus: read_gpus(),
            nics: read_nics(),
        }
    }

    pub(crate) fn sample(&mut self) -> Metrics {
        let cpu_now = read_cpu_times();
        let cpu_usage = match (self.prev_cpu, cpu_now) {
            (Some(prev), Some(now)) if now.total > prev.total => {
                let total = now.total - prev.total;
                let idle = now.idle.saturating_sub(prev.idle);
                (100.0 * (1.0 - idle as f32 / total as f32)).clamp(0.0, 100.0)
            }
            _ => 0.0,
        };
        if let Some(now) = cpu_now {
            self.prev_cpu = Some(now);
        }

        let (ram_total_kb, ram_used_kb, swap_total_kb, swap_used_kb) = read_memory();
        let net_now = read_net_totals();
        let (net_rx_bps, net_tx_bps) = match (self.prev_net, net_now) {
            (Some(prev), Some(now)) if now.at > prev.at => {
                let dt = now.at.duration_since(prev.at).as_secs_f64().max(0.001);
                (
                    now.rx.saturating_sub(prev.rx) as f64 / dt,
                    now.tx.saturating_sub(prev.tx) as f64 / dt,
                )
            }
            _ => (0.0, 0.0),
        };
        if let Some(now) = net_now {
            self.prev_net = Some(now);
        }
        Metrics {
            cpu_model: self.cpu_model.clone(),
            cpu_usage,
            cpu_status: read_cpu_status(cpu_usage),
            cpu_frequencies: read_cpu_frequencies(),
            ram_total_kb,
            ram_used_kb,
            swap_total_kb,
            swap_used_kb,
            gpus: self.gpus.clone(),
            // GPU usage is sampled asynchronously (can invoke nvidia-smi which
            // is slow); the caller merges the cached value into the metrics.
            gpu_usage: Vec::new(),
            nics: self.nics.clone(),
            net_rx_bps,
            net_tx_bps,
            battery: read_battery(),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct UiWindows {
    pub(crate) topbar: Window,
    pub(crate) dock: Window,
    pub(crate) settings: Window,
    pub(crate) folder: Window,
    pub(crate) folder_terminal: Window,
    pub(crate) screenshot_overlay: Window,
    pub(crate) app_menu: Window,
    pub(crate) aurora_menu: Window,
    pub(crate) clipboard_menu: Window,
    pub(crate) media: [Window; MEDIA_SLOT_COUNT],
    pub(crate) dock_more_menu: Window,
    pub(crate) title_menu: Window,
    pub(crate) confirm_dialog: Window,
    pub(crate) tooltip: Window,
}

#[derive(Clone, Copy)]
pub(crate) struct TopbarControls {
    pub(crate) clipboard_x: i32,
    pub(crate) screenshot_x: i32,
    pub(crate) display_x: i32,
    pub(crate) audio_x: i32,
    pub(crate) network_x: i32,
    pub(crate) battery_left: i32,
    pub(crate) battery_right: i32,
}

#[derive(Clone, Copy)]
pub(crate) struct ClientInfo {
    pub(crate) window: Window,
    pub(crate) frame: Window,
    pub(crate) workspace: usize,
    pub(crate) mapped: bool,
    pub(crate) x: i16,
    pub(crate) y: i16,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) titlebar: bool,
    pub(crate) saved: Option<(i16, i16, u16, u16)>,
    /// Visible on every workspace.
    pub(crate) sticky: bool,
    /// _NET_WM_STATE_FULLSCREEN is active.
    pub(crate) fullscreen: bool,
    /// Geometry + titlebar flag saved when entering fullscreen.
    pub(crate) fs_saved: Option<(i16, i16, u16, u16, bool)>,
}

#[derive(Clone, Copy)]
pub(crate) struct PendingWindowNudge {
    pub(crate) client: Window,
    pub(crate) base_width: u16,
    pub(crate) base_height: u16,
    pub(crate) step: u8,
    pub(crate) at: Instant,
}

#[derive(Clone, Copy)]
pub(crate) enum DragKind {
    Move,
    Resize,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ResizeEdges {
    pub(crate) left: bool,
    pub(crate) right: bool,
    pub(crate) top: bool,
    pub(crate) bottom: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TitleButton {
    Close,
    Minimize,
    Maximize,
}

#[derive(Clone, Copy)]
pub(crate) struct DragState {
    pub(crate) client: Window,
    pub(crate) offset_x: i16,
    pub(crate) offset_y: i16,
    pub(crate) start_root_x: i16,
    pub(crate) start_root_y: i16,
    pub(crate) start_x: i16,
    pub(crate) start_y: i16,
    pub(crate) start_w: u16,
    pub(crate) start_h: u16,
    pub(crate) kind: DragKind,
    pub(crate) resize_edges: ResizeEdges,
    pub(crate) last_update_at: Instant,
}

#[derive(Clone, Copy)]
pub(crate) struct PendingResize {
    pub(crate) client: Window,
    pub(crate) root_x: i16,
    pub(crate) root_y: i16,
    pub(crate) edges: ResizeEdges,
    pub(crate) pressed_at: Instant,
}

#[derive(Clone, Copy)]
pub(crate) enum UiResizeTarget {
    Folder,
    FolderTerminal,
}

#[derive(Clone, Copy)]
pub(crate) struct PendingUiResize {
    pub(crate) target: UiResizeTarget,
    pub(crate) root_x: i16,
    pub(crate) root_y: i16,
    pub(crate) pressed_at: Instant,
}

#[derive(Clone, Copy)]
pub(crate) struct UiResizeState {
    pub(crate) target: UiResizeTarget,
    pub(crate) start_root_x: i16,
    pub(crate) start_root_y: i16,
    pub(crate) start_w: u16,
    pub(crate) start_h: u16,
}

#[derive(Clone, Copy)]
pub(crate) struct PendingClientDrag {
    pub(crate) client: Window,
    pub(crate) root_x: i16,
    pub(crate) root_y: i16,
    pub(crate) pressed_at: Instant,
}

#[derive(Clone, Copy)]
pub(crate) struct DockClickState {
    pub(crate) client: Window,
    pub(crate) at: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FolderMode {
    Home,
    Pictures,
    Music,
    Videos,
}

impl FolderMode {
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Pictures => "Pictures",
            Self::Music => "Music",
            Self::Videos => "Videos",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct FolderEntry {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) kind: FileKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FolderSort {
    Name,
    Date,
    Size,
}

impl FolderSort {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Date => "Date",
            Self::Size => "Size",
        }
    }
}

pub(crate) struct FolderTerminal {
    pub(crate) visible: bool,
    pub(crate) cwd: PathBuf,
    pub(crate) focused: bool,
    pub(crate) master_fd: Option<RawFd>,
    pub(crate) child_pid: Option<libc::pid_t>,
    pub(crate) history: Vec<String>,
    pub(crate) scrollback: usize,
    pub(crate) screen: Vec<Vec<char>>,
    pub(crate) screen_fg: Vec<Vec<u8>>,
    pub(crate) screen_bg: Vec<Vec<u8>>,
    pub(crate) screen_bold: Vec<Vec<bool>>,
    pub(crate) cols: usize,
    pub(crate) rows: usize,
    pub(crate) cursor_x: usize,
    pub(crate) cursor_y: usize,
    pub(crate) saved_cursor_x: usize,
    pub(crate) saved_cursor_y: usize,
    pub(crate) esc: String,
    pub(crate) line_drawing: bool,
    pub(crate) saved_line_drawing: bool,
    pub(crate) normal_screen: Option<Vec<Vec<char>>>,
    pub(crate) normal_screen_fg: Option<Vec<Vec<u8>>>,
    pub(crate) normal_screen_bg: Option<Vec<Vec<u8>>>,
    pub(crate) normal_screen_bold: Option<Vec<Vec<bool>>>,
    pub(crate) scroll_top: usize,
    pub(crate) scroll_bottom: usize,
    pub(crate) insert_mode: bool,
    pub(crate) auto_wrap: bool,
    pub(crate) app_cursor_keys: bool,
    pub(crate) bracketed_paste: bool,
    pub(crate) mouse_enabled: bool,
    pub(crate) current_fg: u8,
    pub(crate) current_bg: u8,
    pub(crate) current_bold: bool,
    pub(crate) zoom: i8,
    pub(crate) dirty: bool,
}

pub(crate) struct WorkspaceUiState {
    pub(crate) folder_mode: FolderMode,
    pub(crate) folder_entries: Vec<FolderEntry>,
    pub(crate) folder_path: PathBuf,
    pub(crate) folder_selected: Option<PathBuf>,
    pub(crate) folder_scroll: usize,
    pub(crate) folder_front: bool,
    pub(crate) folder_more_open: bool,
    pub(crate) folder_sort_open: bool,
    pub(crate) folder_sort: FolderSort,
    pub(crate) folder_width: u16,
    pub(crate) folder_height: u16,
    pub(crate) folder_terminal_width: u16,
    pub(crate) folder_terminal_height: u16,
    pub(crate) folder_terminal: FolderTerminal,
    pub(crate) media: Option<MediaState>,
    pub(crate) media_slots: Vec<Option<MediaState>>,
    pub(crate) media_front: bool,
    pub(crate) media_front_slot: Option<usize>,
    pub(crate) media_text_selection: Option<MediaTextSelection>,
    pub(crate) media_text_selecting: bool,
    pub(crate) media_text_selection_redraw_at: Option<Instant>,
    pub(crate) media_text_live_rects: Vec<Rectangle>,
    pub(crate) media_context_open: Option<(usize, i32, i32)>,
    pub(crate) media_trash_prompt: Option<usize>,
    pub(crate) folder_context_open: bool,
    pub(crate) folder_context_pos: (i32, i32),
    pub(crate) folder_clipboard: Option<(PathBuf, bool)>,
    pub(crate) folder_info: Option<String>,
    pub(crate) folder_terminal_selection: Option<TerminalSelection>,
    pub(crate) folder_terminal_selecting: bool,
    pub(crate) folder_terminal_live_rects: Vec<Rectangle>,
    pub(crate) folder_drag: Option<PathBuf>,
    pub(crate) folder_press: Option<FolderPress>,
}

impl WorkspaceUiState {
    pub(crate) fn new(screen_height: u16) -> Self {
        let folder_mode = FolderMode::Home;
        let folder_sort = FolderSort::Name;
        let folder_path = folder_path_for(folder_mode);
        let fh = (screen_height as f32 * 0.5) as u16;
        let th = (screen_height as f32 * 0.4) as u16;
        Self {
            folder_mode,
            folder_entries: folder_entries_in(folder_path.clone(), folder_sort),
            folder_path: folder_path.clone(),
            folder_selected: None,
            folder_scroll: 0,
            folder_front: false,
            folder_more_open: false,
            folder_sort_open: false,
            folder_sort,
            folder_width: FOLDER_DEFAULT_WIDTH,
            folder_height: fh,
            folder_terminal_width: TERMINAL_DEFAULT_WIDTH,
            folder_terminal_height: th,
            folder_terminal: FolderTerminal::new(folder_path),
            media: None,
            media_slots: vec![None; MEDIA_SLOT_COUNT],
            media_front: false,
            media_front_slot: None,
            media_text_selection: None,
            media_text_selecting: false,
            media_text_selection_redraw_at: None,
            media_text_live_rects: Vec::new(),
            media_context_open: None,
            media_trash_prompt: None,
            folder_context_open: false,
            folder_context_pos: (0, 0),
            folder_clipboard: None,
            folder_info: None,
            folder_terminal_selection: None,
            folder_terminal_selecting: false,
            folder_terminal_live_rects: Vec::new(),
            folder_drag: None,
            folder_press: None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TerminalSelection {
    pub(crate) start_row: usize,
    pub(crate) start_col: usize,
    pub(crate) end_row: usize,
    pub(crate) end_col: usize,
}

impl FolderTerminal {
    pub(crate) fn new(cwd: PathBuf) -> Self {
        Self {
            visible: false,
            cwd,
            focused: false,
            master_fd: None,
            child_pid: None,
            history: Vec::new(),
            scrollback: 0,
            screen: vec![vec![' '; FOLDER_TERMINAL_DEFAULT_COLS]; FOLDER_TERMINAL_DEFAULT_ROWS],
            screen_fg: vec![vec![255; FOLDER_TERMINAL_DEFAULT_COLS]; FOLDER_TERMINAL_DEFAULT_ROWS],
            screen_bg: vec![vec![255; FOLDER_TERMINAL_DEFAULT_COLS]; FOLDER_TERMINAL_DEFAULT_ROWS],
            screen_bold: vec![
                vec![false; FOLDER_TERMINAL_DEFAULT_COLS];
                FOLDER_TERMINAL_DEFAULT_ROWS
            ],
            cols: FOLDER_TERMINAL_DEFAULT_COLS,
            rows: FOLDER_TERMINAL_DEFAULT_ROWS,
            cursor_x: 0,
            cursor_y: 0,
            saved_cursor_x: 0,
            saved_cursor_y: 0,
            esc: String::new(),
            line_drawing: false,
            saved_line_drawing: false,
            normal_screen: None,
            normal_screen_fg: None,
            normal_screen_bg: None,
            normal_screen_bold: None,
            scroll_top: 0,
            scroll_bottom: FOLDER_TERMINAL_DEFAULT_ROWS - 1,
            insert_mode: false,
            auto_wrap: true,
            app_cursor_keys: false,
            bracketed_paste: false,
            mouse_enabled: false,
            current_fg: 255,
            current_bg: 255,
            current_bold: false,
            zoom: 0,
            dirty: true,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ImagePreview {
    pub(crate) pixels: Vec<u8>,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) resolution: Option<(u32, u32)>,
}

#[derive(Clone)]
pub(crate) struct MediaState {
    pub(crate) entry: FolderEntry,
    pub(crate) playing: bool,
    pub(crate) progress: f32,
    pub(crate) text_lines: Vec<String>,
    pub(crate) text_scroll: usize,
    pub(crate) text_cursor_line: usize,
    pub(crate) text_cursor_col: usize,
    pub(crate) text_undo: Vec<Vec<String>>,
    pub(crate) editing: bool,
    pub(crate) file_info: Option<String>,
    pub(crate) image_preview: Option<ImagePreview>,
    pub(crate) notice: Option<String>,
}

#[derive(Clone, Copy)]
pub(crate) struct ScreenshotSelection {
    pub(crate) start_x: i16,
    pub(crate) start_y: i16,
    pub(crate) current_x: i16,
    pub(crate) current_y: i16,
}

#[derive(Clone, Copy)]
pub(crate) struct PendingScreenshotButton {
    pub(crate) pressed_at: Instant,
}

#[derive(Clone)]
pub(crate) enum ClipboardItem {
    Text(String),
    Image(PathBuf),
}

#[derive(Clone)]
pub(crate) struct ClipboardEntry {
    pub(crate) item: ClipboardItem,
}

pub(crate) struct ClipboardImagePreviewResult {
    pub(crate) path: PathBuf,
    pub(crate) preview: Option<ImagePreview>,
}

pub(crate) enum ClipboardPollItem {
    Text(String),
    Image(PathBuf, u64),
}

pub(crate) struct ClipboardPollResult {
    pub(crate) item: Option<ClipboardPollItem>,
}

#[derive(Clone, Copy)]
pub(crate) enum MediaContextAction {
    Rename,
    CopyImage,
    MoveTrash,
    ConfirmTrash,
    CancelTrash,
}

#[derive(Clone)]
pub(crate) struct FolderPress {
    pub(crate) entry: FolderEntry,
    pub(crate) root_x: i16,
    pub(crate) root_y: i16,
}

#[derive(Clone)]
pub(crate) struct MediaTextSelection {
    pub(crate) slot: usize,
    pub(crate) start_line: usize,
    pub(crate) start_col: usize,
    pub(crate) end_line: usize,
    pub(crate) end_col: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FolderContextAction {
    Copy,
    Paste,
    Cut,
    Info,
    OpenExternal,
}

#[derive(Clone)]
pub(crate) struct PlaceEntry {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileKind {
    Directory,
    Text,
    Image,
    Audio,
    Video,
    Other,
}

#[derive(Clone, Copy)]
pub(crate) enum AppAction {
    Terminal,
    Browser,
    Camera,
    Recorder,
    Settings,
    More,
}

#[derive(Clone, Copy)]
pub(crate) struct AppMenuItem {
    pub(crate) label: &'static str,
    pub(crate) hint: &'static str,
    pub(crate) action: AppAction,
}

#[derive(Clone)]
pub(crate) struct DesktopEntry {
    pub(crate) name: String,
    pub(crate) category: String,
    pub(crate) command: String,
    pub(crate) categories: String,
    pub(crate) mime_types: String,
    pub(crate) keywords: String,
}

pub(crate) enum AppCatalogRow {
    Category {
        name: String,
        count: usize,
        expanded: bool,
    },
    App {
        name: String,
        command: String,
    },
}

#[derive(Clone)]
pub(crate) struct InstalledApp {
    pub(crate) name: String,
    pub(crate) command: String,
}

#[derive(Clone)]
pub(crate) struct WifiNetwork {
    pub(crate) ssid: String,
}

#[derive(Clone)]
pub(crate) struct WifiConnection {
    pub(crate) ssid: String,
    pub(crate) device: String,
    pub(crate) ip: Option<String>,
}

pub(crate) struct WifiRefreshResult {
    pub(crate) radio_enabled: bool,
    pub(crate) connected: Option<WifiConnection>,
    pub(crate) networks: Option<Result<Vec<WifiNetwork>, String>>,
}

/// Cached results of slow settings-tab probes (subprocess calls).
/// `None` means "not loaded yet"; tabs draw instantly from this cache and a
/// background thread refreshes it, so switching settings areas never blocks.
#[derive(Default)]
pub(crate) struct SettingsDataCache {
    pub(crate) audio_volume: Option<Option<u8>>,
    pub(crate) audio_outputs: Option<Vec<AudioDevice>>,
    pub(crate) audio_inputs: Option<Vec<AudioDevice>>,
    pub(crate) network_details: Option<Vec<String>>,
    pub(crate) bluetooth_devices: Option<Vec<String>>,
    pub(crate) autostart_apps: Option<Vec<String>>,
    pub(crate) gpu_usage: Option<Vec<GpuUsage>>,
}

/// Message from a settings background-loader thread.
pub(crate) enum SettingsData {
    Audio {
        volume: Option<u8>,
        outputs: Vec<AudioDevice>,
        inputs: Vec<AudioDevice>,
    },
    Network(Vec<String>),
    Bluetooth(Vec<String>),
    Autostart(Vec<String>),
    GpuUsage(Vec<GpuUsage>),
}

pub(crate) mod settings_pending {
    pub(crate) const AUDIO: u8 = 1;
    pub(crate) const NETWORK: u8 = 2;
    pub(crate) const BLUETOOTH: u8 = 4;
    pub(crate) const AUTOSTART: u8 = 8;
    pub(crate) const GPU: u8 = 16;
}

pub(crate) struct Aurora {
    pub(crate) conn: RustConnection,
    pub(crate) display: String,
    pub(crate) root: Window,
    pub(crate) depth: u8,
    pub(crate) visual: Visualid,
    pub(crate) gc: Gcontext,
    pub(crate) cursor: Cursor,
    pub(crate) screen_width: u16,
    pub(crate) screen_height: u16,
    pub(crate) wallpaper_index: usize,
    pub(crate) wallpaper_pixels: Vec<u8>,
    pub(crate) wallpaper_cache: Vec<Option<Vec<u8>>>,
    pub(crate) wallpaper_previews: Vec<Option<Vec<u8>>>,
    pub(crate) wallpaper_pixmap: Option<Pixmap>,
    pub(crate) compositor_active: bool,
    pub(crate) shape_supported: bool,
    pub(crate) ui: UiWindows,
    pub(crate) regular: Font<'static>,
    pub(crate) bold: Font<'static>,
    pub(crate) terminal_regular: Font<'static>,
    pub(crate) terminal_bold: Font<'static>,
    pub(crate) settings: SettingsState,
    pub(crate) terminal_apps: Vec<InstalledApp>,
    pub(crate) browser_apps: Vec<InstalledApp>,
    pub(crate) photo_apps: Vec<InstalledApp>,
    pub(crate) video_apps: Vec<InstalledApp>,
    pub(crate) display_modes: Vec<DisplayMode>,
    pub(crate) sampler: SystemSampler,
    pub(crate) metrics: Metrics,
    pub(crate) clients: HashMap<Window, ClientInfo>,
    pub(crate) workspace_count: usize,
    pub(crate) active_workspace: usize,
    pub(crate) workspace_ui: Vec<WorkspaceUiState>,
    pub(crate) active_client: Option<Window>,
    pub(crate) drag: Option<DragState>,
    pub(crate) pending_resize: Option<PendingResize>,
    pub(crate) pending_ui_resize: Option<PendingUiResize>,
    pub(crate) ui_resize: Option<UiResizeState>,
    pub(crate) pending_client_drag: Option<PendingClientDrag>,
    pub(crate) title_hover: Option<(Window, TitleButton)>,
    pub(crate) ignored_unmaps: Vec<Window>,
    pub(crate) settings_visible: bool,
    pub(crate) settings_front: bool,
    pub(crate) folder_mode: FolderMode,
    pub(crate) folder_entries: Vec<FolderEntry>,
    pub(crate) folder_places: Vec<PlaceEntry>,
    pub(crate) folder_path: PathBuf,
    pub(crate) folder_selected: Option<PathBuf>,
    pub(crate) folder_scroll: usize,
    pub(crate) folder_front: bool,
    pub(crate) folder_more_open: bool,
    pub(crate) folder_sort_open: bool,
    pub(crate) folder_sort: FolderSort,
    pub(crate) folder_width: u16,
    pub(crate) folder_height: u16,
    pub(crate) folder_terminal_width: u16,
    pub(crate) folder_terminal_height: u16,
    pub(crate) folder_terminal: FolderTerminal,
    pub(crate) media: Option<MediaState>,
    pub(crate) media_slots: Vec<Option<MediaState>>,
    pub(crate) media_front: bool,
    pub(crate) media_front_slot: Option<usize>,
    pub(crate) media_text_selection: Option<MediaTextSelection>,
    pub(crate) media_text_selecting: bool,
    pub(crate) media_text_selection_redraw_at: Option<Instant>,
    pub(crate) media_text_live_rects: Vec<Rectangle>,
    pub(crate) media_context_open: Option<(usize, i32, i32)>,
    pub(crate) media_trash_prompt: Option<usize>,
    pub(crate) app_menu_visible: bool,
    pub(crate) app_menu_more: bool,
    pub(crate) app_menu_scroll: usize,
    pub(crate) app_menu_query: String,
    pub(crate) app_menu_expanded_categories: HashSet<String>,
    pub(crate) dock_more_visible: bool,
    pub(crate) aurora_menu_visible: bool,
    pub(crate) aurora_menu_about: bool,
    pub(crate) aurora_menu_restart_confirm: bool,
    pub(crate) clipboard_menu_visible: bool,
    pub(crate) clipboard_history: Vec<ClipboardEntry>,
    pub(crate) clipboard_history_page: usize,
    pub(crate) clipboard_image_previews: HashMap<PathBuf, Option<ImagePreview>>,
    pub(crate) clipboard_image_preview_pending: HashSet<PathBuf>,
    pub(crate) clipboard_image_preview_tx: mpsc::Sender<ClipboardImagePreviewResult>,
    pub(crate) clipboard_image_preview_rx: Receiver<ClipboardImagePreviewResult>,
    pub(crate) clipboard_poll_rx: Option<Receiver<ClipboardPollResult>>,
    pub(crate) last_clipboard_poll: Instant,
    pub(crate) clipboard_watch_supported: bool,
    pub(crate) clipboard_dirty: bool,
    pub(crate) last_seen_clipboard_text: Option<String>,
    pub(crate) last_seen_clipboard_image_sig: Option<u64>,
    pub(crate) command_paste_armed: bool,
    pub(crate) wm_s_atom: Atom,
    pub(crate) folder_context_open: bool,
    pub(crate) folder_context_pos: (i32, i32),
    pub(crate) folder_clipboard: Option<(PathBuf, bool)>,
    pub(crate) folder_info: Option<String>,
    pub(crate) folder_terminal_selection: Option<TerminalSelection>,
    pub(crate) folder_terminal_selecting: bool,
    pub(crate) folder_terminal_live_rects: Vec<Rectangle>,
    pub(crate) folder_drag: Option<PathBuf>,
    pub(crate) folder_press: Option<FolderPress>,
    pub(crate) xdnd_source: Option<Window>,
    pub(crate) dock_last_click: Option<DockClickState>,
    pub(crate) icon_cache: HashMap<String, Option<Vec<u8>>>,
    pub(crate) last_clock_label: String,
    pub(crate) last_tick: Instant,
    pub(crate) last_media_tick: Instant,
    pub(crate) last_pointer_pos: Option<(i16, i16)>,
    pub(crate) last_pointer_activity: Instant,
    pub(crate) pending_auto_power_saver_apply: Option<Instant>,
    pub(crate) screenshot_mode: bool,
    pub(crate) screenshot_selection: Option<ScreenshotSelection>,
    pub(crate) screenshot_base: Option<ImagePreview>,
    pub(crate) screenshot_live_rect: Option<(i16, i16, u16, u16)>,
    pub(crate) pending_screenshot_button: Option<PendingScreenshotButton>,
    pub(crate) topbar_notice: Option<(String, Instant)>,
    pub(crate) ffplay_process: Option<std::process::Child>,
    pub(crate) pending_window_nudges: Vec<PendingWindowNudge>,
    pub(crate) wifi_refresh_rx: Option<Receiver<WifiRefreshResult>>,
    pub(crate) settings_cache: SettingsDataCache,
    pub(crate) settings_data_tx: mpsc::Sender<SettingsData>,
    pub(crate) settings_data_rx: Receiver<SettingsData>,
    pub(crate) settings_data_pending: u8,
    /// When the settings panel was hidden; after 10 s its caches are dropped
    /// so a hidden panel costs no CPU or memory.
    pub(crate) settings_hidden_at: Option<Instant>,
    pub(crate) focus_history: Vec<Window>,
    pub(crate) alt_tab_index: usize,
    pub(crate) alt_tab_windows: Vec<Window>,
    pub(crate) choose_file_mode: bool,
    /// Client whose title dropdown menu is open.
    pub(crate) title_menu_open: Option<Window>,
    /// When the title menu is open, whether it is showing the workspace picker submenu.
    pub(crate) title_menu_workspaces: bool,
    /// Client with a pending close-confirmation dialog.
    pub(crate) confirm_close: Option<Window>,
    /// Currently shown topbar tooltip: (anchor x, label).
    pub(crate) tooltip_shown: Option<(i32, String)>,
}
