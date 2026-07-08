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
const DOCK_HEIGHT: u16 = 76;
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
const CSD_DRAG_TOP_HEIGHT: i16 = 44;
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

static WALLPAPERS: &[WallpaperAsset] = &[
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
struct WallpaperAsset {
    name: &'static str,
    bytes: &'static [u8],
}

#[derive(Clone, Copy, PartialEq)]
struct Color {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

impl Color {
    const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
}

const INK: Color = Color::rgb(32, 43, 54);
const MUTED: Color = Color::rgb(105, 118, 132);
const SOFT_INK: Color = Color::rgb(74, 88, 103);
const MINT_DARK: Color = Color::rgb(29, 145, 137);
const MINT_LIGHT: Color = Color::rgb(160, 238, 220);
const BLUE: Color = Color::rgb(73, 156, 231);
const CARD_LINE: Color = Color::rgba(198, 214, 224, 130);
const TOPBAR_ICON_SPACING: i32 = 33;
const TOPBAR_ICON_HIT_RADIUS: i32 = 18;
const CLIPBOARD_HISTORY_LIMIT: usize = 200;
const CLIPBOARD_MENU_VISIBLE_ROWS: usize = 10;
const CLIPBOARD_MENU_WIDTH: u16 = 420;
const CLIPBOARD_MENU_TEXT_ROW_HEIGHT: i32 = 58;
const CLIPBOARD_MENU_IMAGE_ROW_HEIGHT: i32 = 62;
const CLIPBOARD_MENU_IMAGE_PREVIEW_W: i32 = 184;
const CLIPBOARD_MENU_IMAGE_PREVIEW_H: i32 = 48;
const CLIPBOARD_MENU_NAV_Y: i32 = 8;
const CLIPBOARD_MENU_NAV_W: i32 = 36;
const CLIPBOARD_MENU_NAV_H: i32 = 28;
const CLIPBOARD_MENU_PREV_X: i32 = 118;
const CLIPBOARD_MENU_NEXT_X: i32 = 158;
const DEFAULT_WORKSPACE_COUNT: usize = 2;
const MAX_WORKSPACE_COUNT: usize = 8;
const WORKSPACE_STRIDE: i32 = 27;
const WORKSPACE_SIZE: i32 = 18;
const FOLDER_DEFAULT_WIDTH: u16 = 330;
const FOLDER_DEFAULT_HEIGHT: u16 = 220;
const FOLDER_MIN_WIDTH: u16 = 260;
const FOLDER_MIN_HEIGHT: u16 = 160;
const TERMINAL_MIN_WIDTH: u16 = 260;
const TERMINAL_MIN_HEIGHT: u16 = 120;
const TERMINAL_DEFAULT_WIDTH: u16 = FOLDER_DEFAULT_WIDTH;

#[derive(Clone)]
struct Canvas {
    width: u16,
    height: u16,
    data: Vec<u8>,
}

impl Canvas {
    fn new(width: u16, height: u16, color: Color) -> Self {
        let mut data = vec![0; usize::from(width) * usize::from(height) * 4];
        for px in data.chunks_exact_mut(4) {
            px[0] = color.b;
            px[1] = color.g;
            px[2] = color.r;
            px[3] = 0;
        }
        Self {
            width,
            height,
            data,
        }
    }

    fn from_wallpaper_crop(
        wallpaper: &[u8],
        screen_width: u16,
        x: i32,
        y: i32,
        width: u16,
        height: u16,
    ) -> Self {
        let mut canvas = Self::new(width, height, Color::rgb(238, 247, 252));
        for yy in 0..i32::from(height) {
            let sy = y + yy;
            if sy < 0 {
                continue;
            }
            for xx in 0..i32::from(width) {
                let sx = x + xx;
                if sx < 0 {
                    continue;
                }
                let src = (usize::try_from(sy).unwrap_or(0) * usize::from(screen_width)
                    + usize::try_from(sx).unwrap_or(0))
                    * 4;
                let dst = (usize::try_from(yy).unwrap() * usize::from(width)
                    + usize::try_from(xx).unwrap())
                    * 4;
                if src + 3 < wallpaper.len() {
                    canvas.data[dst..dst + 4].copy_from_slice(&wallpaper[src..src + 4]);
                }
            }
        }
        canvas
    }

    fn idx(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x >= i32::from(self.width) || y >= i32::from(self.height) {
            return None;
        }
        Some((usize::try_from(y).ok()? * usize::from(self.width) + usize::try_from(x).ok()?) * 4)
    }

    fn blend_pixel(&mut self, x: i32, y: i32, color: Color) {
        let Some(i) = self.idx(x, y) else {
            return;
        };
        if color.a == 255 {
            self.data[i] = color.b;
            self.data[i + 1] = color.g;
            self.data[i + 2] = color.r;
            self.data[i + 3] = 0;
            return;
        }
        let alpha = u32::from(color.a);
        let inv = 255 - alpha;
        self.data[i] = ((u32::from(color.b) * alpha + u32::from(self.data[i]) * inv) / 255) as u8;
        self.data[i + 1] =
            ((u32::from(color.g) * alpha + u32::from(self.data[i + 1]) * inv) / 255) as u8;
        self.data[i + 2] =
            ((u32::from(color.r) * alpha + u32::from(self.data[i + 2]) * inv) / 255) as u8;
        self.data[i + 3] = 0;
    }

    fn draw_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color) {
        if w <= 0 || h <= 0 {
            return;
        }
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(i32::from(self.width));
        let y1 = (y + h).min(i32::from(self.height));
        for yy in y0..y1 {
            for xx in x0..x1 {
                self.blend_pixel(xx, yy, color);
            }
        }
    }

    fn draw_round_rect(&mut self, x: i32, y: i32, w: i32, h: i32, radius: i32, color: Color) {
        if w <= 0 || h <= 0 {
            return;
        }
        let r = radius.max(0).min(w / 2).min(h / 2);
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(i32::from(self.width));
        let y1 = (y + h).min(i32::from(self.height));
        let rf = r as f32;

        for yy in y0..y1 {
            for xx in x0..x1 {
                let coverage = if r == 0 {
                    1.0
                } else {
                    let cx = if xx < x + r {
                        x + r
                    } else if xx >= x + w - r {
                        x + w - r - 1
                    } else {
                        xx
                    };
                    let cy = if yy < y + r {
                        y + r
                    } else if yy >= y + h - r {
                        y + h - r - 1
                    } else {
                        yy
                    };
                    let dx = xx - cx;
                    let dy = yy - cy;
                    let d = ((dx * dx + dy * dy) as f32).sqrt();
                    if d <= rf - 0.5 {
                        1.0
                    } else if d >= rf + 0.5 {
                        0.0
                    } else {
                        rf + 0.5 - d
                    }
                };
                if coverage > 0.0 {
                    let mut blended = color;
                    blended.a = (color.a as f32 * coverage).round() as u8;
                    self.blend_pixel(xx, yy, blended);
                }
            }
        }
    }

    fn draw_circle(&mut self, cx: i32, cy: i32, radius: i32, color: Color) {
        let r = radius as f32;
        let x_start = (cx - radius - 1).max(0);
        let x_end = (cx + radius + 1).min(i32::from(self.width));
        let y_start = (cy - radius - 1).max(0);
        let y_end = (cy + radius + 1).min(i32::from(self.height));

        for y in y_start..=y_end {
            for x in x_start..=x_end {
                let dx = x - cx;
                let dy = y - cy;
                let d = ((dx * dx + dy * dy) as f32).sqrt();

                let coverage = if d <= r - 0.5 {
                    1.0
                } else if d >= r + 0.5 {
                    0.0
                } else {
                    r + 0.5 - d
                };

                if coverage > 0.0 {
                    let mut blended = color;
                    blended.a = (color.a as f32 * coverage).round() as u8;
                    self.blend_pixel(x, y, blended);
                }
            }
        }
    }

    fn draw_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, thickness: i32, color: Color) {
        let x_min = x0.min(x1) - (thickness + 2);
        let x_max = x0.max(x1) + (thickness + 2);
        let y_min = y0.min(y1) - (thickness + 2);
        let y_max = y0.max(y1) + (thickness + 2);

        let x_start = x_min.max(0);
        let x_end = x_max.min(i32::from(self.width));
        let y_start = y_min.max(0);
        let y_end = y_max.min(i32::from(self.height));

        let dx = (x1 - x0) as f32;
        let dy = (y1 - y0) as f32;
        let len2 = dx * dx + dy * dy;

        let r = thickness as f32 / 2.0;

        for y in y_start..y_end {
            for x in x_start..x_end {
                let t = if len2 == 0.0 {
                    0.0
                } else {
                    (((x - x0) as f32 * dx + (y - y0) as f32 * dy) / len2).clamp(0.0, 1.0)
                };
                let proj_x = x0 as f32 + t * dx;
                let proj_y = y0 as f32 + t * dy;
                let dist_x = x as f32 - proj_x;
                let dist_y = y as f32 - proj_y;
                let d = (dist_x * dist_x + dist_y * dist_y).sqrt();

                let coverage = if d <= r - 0.5 {
                    1.0
                } else if d >= r + 0.5 {
                    0.0
                } else {
                    r + 0.5 - d
                };

                if coverage > 0.0 {
                    let mut blended = color;
                    blended.a = (color.a as f32 * coverage).round() as u8;
                    self.blend_pixel(x, y, blended);
                }
            }
        }
    }

    fn draw_text(
        &mut self,
        font: &Font<'static>,
        text: &str,
        x: i32,
        y: i32,
        size: f32,
        color: Color,
    ) {
        let scale = Scale::uniform(size);
        let metrics = font.v_metrics(scale);
        let glyphs: Vec<_> = font
            .layout(text, scale, point(x as f32, y as f32 + metrics.ascent))
            .collect();
        for glyph in glyphs {
            if let Some(bb) = glyph.pixel_bounding_box() {
                glyph.draw(|gx, gy, v| {
                    let alpha = (v * f32::from(color.a)).round().clamp(0.0, 255.0) as u8;
                    self.blend_pixel(
                        bb.min.x + i32::try_from(gx).unwrap(),
                        bb.min.y + i32::try_from(gy).unwrap(),
                        Color { a: alpha, ..color },
                    );
                });
            }
        }
    }

    fn draw_text_center(
        &mut self,
        font: &Font<'static>,
        text: &str,
        cx: i32,
        y: i32,
        size: f32,
        color: Color,
    ) {
        let w = measure_text(font, text, size);
        self.draw_text(font, text, cx - w / 2, y, size, color);
    }

    fn draw_text_right(
        &mut self,
        font: &Font<'static>,
        text: &str,
        right: i32,
        y: i32,
        size: f32,
        color: Color,
    ) {
        let w = measure_text(font, text, size);
        self.draw_text(font, text, right - w, y, size, color);
    }
}

fn measure_text(font: &Font<'static>, text: &str, size: f32) -> i32 {
    let scale = Scale::uniform(size);
    let mut width = 0.0f32;
    for glyph in font.layout(text, scale, point(0.0, 0.0)) {
        let advance = glyph.position().x + glyph.unpositioned().h_metrics().advance_width;
        width = width.max(advance);
    }
    width.ceil() as i32
}

fn point_in_rect(px: i32, py: i32, x: i32, y: i32, w: i32, h: i32) -> bool {
    px >= x && px < x + w && py >= y && py < y + h
}

#[derive(Clone)]
struct DisplayMode {
    output: Option<String>,
    width: u16,
    height: u16,
    refresh: Option<f32>,
    current: bool,
}

impl DisplayMode {
    fn label(&self) -> String {
        match self.refresh {
            Some(rate) => format!("{}x{}  {:.0} Hz", self.width, self.height, rate),
            None => format!("{}x{}", self.width, self.height),
        }
    }
}

#[derive(Clone)]
struct AudioDevice {
    id: String,
    name: String,
    label: String,
    is_default: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AudioDeviceKind {
    Output,
    Input,
}

impl AudioDeviceKind {
    fn pactl_list_arg(self) -> &'static str {
        match self {
            Self::Output => "sinks",
            Self::Input => "sources",
        }
    }

    fn pactl_default_key(self) -> &'static str {
        match self {
            Self::Output => "Default Sink:",
            Self::Input => "Default Source:",
        }
    }

    fn pactl_set_default_command(self) -> &'static str {
        match self {
            Self::Output => "set-default-sink",
            Self::Input => "set-default-source",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    Display,
    Power,
    Wallpaper,
    Audio,
    Network,
    Bluetooth,
    Startup,
    Apps,
    About,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DefaultAppKind {
    Terminal,
    Browser,
    Photo,
    Video,
}

impl DefaultAppKind {
    fn label(self) -> &'static str {
        match self {
            Self::Terminal => "Terminal",
            Self::Browser => "Browser",
            Self::Photo => "Photos",
            Self::Video => "Videos",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Browser => "browser",
            Self::Photo => "photo",
            Self::Video => "video",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PowerMode {
    Saver,
    Balanced,
    Performance,
}

impl PowerMode {
    fn label(self) -> &'static str {
        match self {
            Self::Saver => "Battery saver",
            Self::Balanced => "Balanced",
            Self::Performance => "Performance",
        }
    }

    fn command_value(self) -> &'static str {
        match self {
            Self::Saver => "power-saver",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }

    fn from_command_value(value: &str) -> Option<Self> {
        match value.trim() {
            "power-saver" => Some(Self::Saver),
            "balanced" => Some(Self::Balanced),
            "performance" => Some(Self::Performance),
            _ => None,
        }
    }
}

struct SettingsState {
    tab: SettingsTab,
    sleep_after_secs: u32,
    brightness_percent: u8,
    compositor_enabled: bool,
    power_mode: PowerMode,
    auto_power_saver_enabled: bool,
    auto_power_saver_minutes: u32,
    auto_power_saver_input: String,
    auto_power_saver_editing: bool,
    selected_mode: usize,
    scroll: i32,
    app_kind: DefaultAppKind,
    terminal_command: String,
    browser_command: String,
    photo_command: String,
    video_command: String,
    terminal_editing: bool,
    app_status: Option<String>,
    display_status: Option<String>,
    audio_status: Option<String>,
    wifi_networks: Vec<WifiNetwork>,
    wifi_scroll: usize,
    wifi_selected: Option<String>,
    wifi_password: String,
    wifi_password_editing: bool,
    wifi_status: Option<String>,
    wifi_disconnect_confirm: bool,
    wifi_radio_enabled: Option<bool>,
    wifi_connected: Option<Option<WifiConnection>>,
}

impl Default for SettingsState {
    fn default() -> Self {
        let auto_power_saver_minutes = read_u32_setting("auto_power_saver_minutes", 10).min(240);
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
        }
    }
}

#[derive(Default, Clone)]
struct Metrics {
    cpu_model: String,
    cpu_usage: f32,
    cpu_status: String,
    cpu_frequencies: Vec<String>,
    ram_total_kb: u64,
    ram_used_kb: u64,
    swap_total_kb: u64,
    swap_used_kb: u64,
    gpus: Vec<String>,
    nics: Vec<String>,
    net_rx_bps: f64,
    net_tx_bps: f64,
    battery: Option<String>,
}

#[derive(Clone, Copy)]
struct CpuTimes {
    idle: u64,
    total: u64,
}

#[derive(Clone, Copy)]
struct NetTotals {
    rx: u64,
    tx: u64,
    at: Instant,
}

struct SystemSampler {
    prev_cpu: Option<CpuTimes>,
    prev_net: Option<NetTotals>,
    cpu_model: String,
    gpus: Vec<String>,
    nics: Vec<String>,
}

impl SystemSampler {
    fn new() -> Self {
        Self {
            prev_cpu: None,
            prev_net: None,
            cpu_model: read_cpu_model(),
            gpus: read_gpus(),
            nics: read_nics(),
        }
    }

    fn sample(&mut self) -> Metrics {
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
            nics: self.nics.clone(),
            net_rx_bps,
            net_tx_bps,
            battery: read_battery(),
        }
    }
}

#[derive(Clone, Copy)]
struct UiWindows {
    topbar: Window,
    dock: Window,
    settings: Window,
    folder: Window,
    folder_terminal: Window,
    screenshot_overlay: Window,
    app_menu: Window,
    aurora_menu: Window,
    clipboard_menu: Window,
    media: [Window; MEDIA_SLOT_COUNT],
    dock_more_menu: Window,
}

#[derive(Clone, Copy)]
struct TopbarControls {
    clipboard_x: i32,
    screenshot_x: i32,
    display_x: i32,
    audio_x: i32,
    network_x: i32,
    battery_left: i32,
    battery_right: i32,
}

#[derive(Clone, Copy)]
struct ClientInfo {
    window: Window,
    frame: Window,
    workspace: usize,
    mapped: bool,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    titlebar: bool,
    saved: Option<(i16, i16, u16, u16)>,
}

#[derive(Clone, Copy)]
struct PendingWindowNudge {
    client: Window,
    base_width: u16,
    base_height: u16,
    step: u8,
    at: Instant,
}

#[derive(Clone, Copy)]
enum DragKind {
    Move,
    Resize,
}

#[derive(Clone, Copy, Default)]
struct ResizeEdges {
    left: bool,
    right: bool,
    top: bool,
    bottom: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TitleButton {
    Close,
    Minimize,
    Maximize,
}

#[derive(Clone, Copy)]
struct DragState {
    client: Window,
    offset_x: i16,
    offset_y: i16,
    start_root_x: i16,
    start_root_y: i16,
    start_x: i16,
    start_y: i16,
    start_w: u16,
    start_h: u16,
    kind: DragKind,
    resize_edges: ResizeEdges,
    last_update_at: Instant,
}

#[derive(Clone, Copy)]
struct PendingResize {
    client: Window,
    root_x: i16,
    root_y: i16,
    edges: ResizeEdges,
    pressed_at: Instant,
}

#[derive(Clone, Copy)]
enum UiResizeTarget {
    Folder,
    FolderTerminal,
}

#[derive(Clone, Copy)]
struct PendingUiResize {
    target: UiResizeTarget,
    root_x: i16,
    root_y: i16,
    pressed_at: Instant,
}

#[derive(Clone, Copy)]
struct UiResizeState {
    target: UiResizeTarget,
    start_root_x: i16,
    start_root_y: i16,
    start_w: u16,
    start_h: u16,
}

#[derive(Clone, Copy)]
struct PendingClientDrag {
    client: Window,
    root_x: i16,
    root_y: i16,
    pressed_at: Instant,
}

#[derive(Clone, Copy)]
struct DockClickState {
    client: Window,
    at: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FolderMode {
    Home,
    Pictures,
    Music,
    Videos,
}

impl FolderMode {
    fn title(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Pictures => "Pictures",
            Self::Music => "Music",
            Self::Videos => "Videos",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct FolderEntry {
    name: String,
    path: PathBuf,
    kind: FileKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FolderSort {
    Name,
    Date,
    Size,
}

impl FolderSort {
    fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Date => "Date",
            Self::Size => "Size",
        }
    }
}

struct FolderTerminal {
    visible: bool,
    cwd: PathBuf,
    focused: bool,
    master_fd: Option<RawFd>,
    child_pid: Option<libc::pid_t>,
    history: Vec<String>,
    scrollback: usize,
    screen: Vec<Vec<char>>,
    screen_fg: Vec<Vec<u8>>,
    screen_bg: Vec<Vec<u8>>,
    screen_bold: Vec<Vec<bool>>,
    cols: usize,
    rows: usize,
    cursor_x: usize,
    cursor_y: usize,
    saved_cursor_x: usize,
    saved_cursor_y: usize,
    esc: String,
    line_drawing: bool,
    saved_line_drawing: bool,
    normal_screen: Option<Vec<Vec<char>>>,
    normal_screen_fg: Option<Vec<Vec<u8>>>,
    normal_screen_bg: Option<Vec<Vec<u8>>>,
    normal_screen_bold: Option<Vec<Vec<bool>>>,
    scroll_top: usize,
    scroll_bottom: usize,
    insert_mode: bool,
    auto_wrap: bool,
    app_cursor_keys: bool,
    bracketed_paste: bool,
    mouse_enabled: bool,
    current_fg: u8,
    current_bg: u8,
    current_bold: bool,
    zoom: i8,
    dirty: bool,
}

struct WorkspaceUiState {
    folder_mode: FolderMode,
    folder_entries: Vec<FolderEntry>,
    folder_path: PathBuf,
    folder_selected: Option<PathBuf>,
    folder_scroll: usize,
    folder_front: bool,
    folder_more_open: bool,
    folder_sort_open: bool,
    folder_sort: FolderSort,
    folder_width: u16,
    folder_height: u16,
    folder_terminal_width: u16,
    folder_terminal_height: u16,
    folder_terminal: FolderTerminal,
    media: Option<MediaState>,
    media_slots: Vec<Option<MediaState>>,
    media_front: bool,
    media_front_slot: Option<usize>,
    media_text_selection: Option<MediaTextSelection>,
    media_text_selecting: bool,
    media_text_selection_redraw_at: Option<Instant>,
    media_text_live_rects: Vec<Rectangle>,
    media_context_open: Option<(usize, i32, i32)>,
    media_trash_prompt: Option<usize>,
    folder_context_open: bool,
    folder_context_pos: (i32, i32),
    folder_clipboard: Option<(PathBuf, bool)>,
    folder_info: Option<String>,
    folder_terminal_selection: Option<TerminalSelection>,
    folder_terminal_selecting: bool,
    folder_terminal_live_rects: Vec<Rectangle>,
    folder_drag: Option<PathBuf>,
    folder_press: Option<FolderPress>,
}

impl WorkspaceUiState {
    fn new(screen_height: u16) -> Self {
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
struct TerminalSelection {
    start_row: usize,
    start_col: usize,
    end_row: usize,
    end_col: usize,
}

impl FolderTerminal {
    fn new(cwd: PathBuf) -> Self {
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
struct ImagePreview {
    pixels: Vec<u8>,
    width: u16,
    height: u16,
    resolution: Option<(u32, u32)>,
}

#[derive(Clone)]
struct MediaState {
    entry: FolderEntry,
    playing: bool,
    progress: f32,
    text_lines: Vec<String>,
    text_scroll: usize,
    text_cursor_line: usize,
    text_cursor_col: usize,
    text_undo: Vec<Vec<String>>,
    editing: bool,
    file_info: Option<String>,
    image_preview: Option<ImagePreview>,
    notice: Option<String>,
}

#[derive(Clone, Copy)]
struct ScreenshotSelection {
    start_x: i16,
    start_y: i16,
    current_x: i16,
    current_y: i16,
}

#[derive(Clone, Copy)]
struct PendingScreenshotButton {
    pressed_at: Instant,
}

#[derive(Clone)]
enum ClipboardItem {
    Text(String),
    Image(PathBuf),
}

#[derive(Clone)]
struct ClipboardEntry {
    item: ClipboardItem,
}

struct ClipboardImagePreviewResult {
    path: PathBuf,
    preview: Option<ImagePreview>,
}

enum ClipboardPollItem {
    Text(String),
    Image(PathBuf, u64),
}

struct ClipboardPollResult {
    item: Option<ClipboardPollItem>,
}

#[derive(Clone, Copy)]
enum MediaContextAction {
    Rename,
    CopyImage,
    MoveTrash,
    ConfirmTrash,
    CancelTrash,
}

#[derive(Clone)]
struct FolderPress {
    entry: FolderEntry,
    root_x: i16,
    root_y: i16,
}

#[derive(Clone)]
struct MediaTextSelection {
    slot: usize,
    start_line: usize,
    start_col: usize,
    end_line: usize,
    end_col: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FolderContextAction {
    Copy,
    Paste,
    Cut,
    Info,
    OpenExternal,
}

#[derive(Clone)]
struct PlaceEntry {
    name: String,
    path: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Directory,
    Text,
    Image,
    Audio,
    Video,
    Other,
}

#[derive(Clone, Copy)]
enum AppAction {
    Terminal,
    Browser,
    Pictures,
    Music,
    Videos,
    Settings,
    More,
}

#[derive(Clone, Copy)]
struct AppMenuItem {
    label: &'static str,
    hint: &'static str,
    action: AppAction,
}

struct DesktopEntry {
    name: String,
    category: String,
    command: String,
    categories: String,
    mime_types: String,
}

#[derive(Clone)]
struct InstalledApp {
    name: String,
    command: String,
}

#[derive(Clone)]
struct WifiNetwork {
    ssid: String,
}

#[derive(Clone)]
struct WifiConnection {
    ssid: String,
    device: String,
    ip: Option<String>,
}

struct WifiRefreshResult {
    radio_enabled: bool,
    connected: Option<WifiConnection>,
    networks: Option<Result<Vec<WifiNetwork>, String>>,
}

struct Aurora {
    conn: RustConnection,
    display: String,
    root: Window,
    depth: u8,
    visual: Visualid,
    gc: Gcontext,
    cursor: Cursor,
    screen_width: u16,
    screen_height: u16,
    wallpaper_index: usize,
    wallpaper_pixels: Vec<u8>,
    wallpaper_cache: Vec<Option<Vec<u8>>>,
    wallpaper_previews: Vec<Option<Vec<u8>>>,
    wallpaper_pixmap: Option<Pixmap>,
    compositor_active: bool,
    shape_supported: bool,
    ui: UiWindows,
    regular: Font<'static>,
    bold: Font<'static>,
    terminal_regular: Font<'static>,
    terminal_bold: Font<'static>,
    settings: SettingsState,
    terminal_apps: Vec<InstalledApp>,
    browser_apps: Vec<InstalledApp>,
    photo_apps: Vec<InstalledApp>,
    video_apps: Vec<InstalledApp>,
    display_modes: Vec<DisplayMode>,
    sampler: SystemSampler,
    metrics: Metrics,
    clients: HashMap<Window, ClientInfo>,
    workspace_count: usize,
    active_workspace: usize,
    workspace_ui: Vec<WorkspaceUiState>,
    active_client: Option<Window>,
    drag: Option<DragState>,
    pending_resize: Option<PendingResize>,
    pending_ui_resize: Option<PendingUiResize>,
    ui_resize: Option<UiResizeState>,
    pending_client_drag: Option<PendingClientDrag>,
    title_hover: Option<(Window, TitleButton)>,
    ignored_unmaps: Vec<Window>,
    settings_visible: bool,
    settings_front: bool,
    folder_mode: FolderMode,
    folder_entries: Vec<FolderEntry>,
    folder_places: Vec<PlaceEntry>,
    folder_path: PathBuf,
    folder_selected: Option<PathBuf>,
    folder_scroll: usize,
    folder_front: bool,
    folder_more_open: bool,
    folder_sort_open: bool,
    folder_sort: FolderSort,
    folder_width: u16,
    folder_height: u16,
    folder_terminal_width: u16,
    folder_terminal_height: u16,
    folder_terminal: FolderTerminal,
    media: Option<MediaState>,
    media_slots: Vec<Option<MediaState>>,
    media_front: bool,
    media_front_slot: Option<usize>,
    media_text_selection: Option<MediaTextSelection>,
    media_text_selecting: bool,
    media_text_selection_redraw_at: Option<Instant>,
    media_text_live_rects: Vec<Rectangle>,
    media_context_open: Option<(usize, i32, i32)>,
    media_trash_prompt: Option<usize>,
    app_menu_visible: bool,
    app_menu_more: bool,
    app_menu_scroll: usize,
    dock_more_visible: bool,
    aurora_menu_visible: bool,
    aurora_menu_about: bool,
    aurora_menu_restart_confirm: bool,
    clipboard_menu_visible: bool,
    clipboard_history: Vec<ClipboardEntry>,
    clipboard_history_page: usize,
    clipboard_image_previews: HashMap<PathBuf, Option<ImagePreview>>,
    clipboard_image_preview_pending: HashSet<PathBuf>,
    clipboard_image_preview_tx: mpsc::Sender<ClipboardImagePreviewResult>,
    clipboard_image_preview_rx: Receiver<ClipboardImagePreviewResult>,
    clipboard_poll_rx: Option<Receiver<ClipboardPollResult>>,
    last_clipboard_poll: Instant,
    clipboard_watch_supported: bool,
    clipboard_dirty: bool,
    last_seen_clipboard_text: Option<String>,
    last_seen_clipboard_image_sig: Option<u64>,
    wm_s_atom: Atom,
    folder_context_open: bool,
    folder_context_pos: (i32, i32),
    folder_clipboard: Option<(PathBuf, bool)>,
    folder_info: Option<String>,
    folder_terminal_selection: Option<TerminalSelection>,
    folder_terminal_selecting: bool,
    folder_terminal_live_rects: Vec<Rectangle>,
    folder_drag: Option<PathBuf>,
    folder_press: Option<FolderPress>,
    xdnd_source: Option<Window>,
    dock_last_click: Option<DockClickState>,
    icon_cache: HashMap<String, Option<Vec<u8>>>,
    last_clock_label: String,
    last_tick: Instant,
    last_media_tick: Instant,
    last_pointer_pos: Option<(i16, i16)>,
    last_pointer_activity: Instant,
    pending_auto_power_saver_apply: Option<Instant>,
    screenshot_mode: bool,
    screenshot_selection: Option<ScreenshotSelection>,
    screenshot_base: Option<ImagePreview>,
    screenshot_live_rect: Option<(i16, i16, u16, u16)>,
    pending_screenshot_button: Option<PendingScreenshotButton>,
    topbar_notice: Option<(String, Instant)>,
    ffplay_process: Option<std::process::Child>,
    pending_window_nudges: Vec<PendingWindowNudge>,
    wifi_refresh_rx: Option<Receiver<WifiRefreshResult>>,
    focus_history: Vec<Window>,
    alt_tab_index: usize,
    alt_tab_windows: Vec<Window>,
    choose_file_mode: bool,
}

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

impl Aurora {
    fn new(
        conn: RustConnection,
        display: String,
        screen: &Screen,
        screen_num: usize,
        compositor_override: Option<bool>,
    ) -> AnyResult<Self> {
        let gc = conn.generate_id()?;
        conn.create_gc(
            gc,
            screen.root,
            &CreateGCAux::new()
                .graphics_exposures(0)
                .foreground(screen.black_pixel)
                .background(screen.white_pixel),
        )?;
        let cursor = create_pointer_cursor(&conn, screen.root)?;
        conn.change_window_attributes(
            screen.root,
            &ChangeWindowAttributesAux::new().cursor(cursor),
        )?;

        let regular = Font::try_from_bytes(FONT_REGULAR).ok_or("failed to load regular font")?;
        let bold = Font::try_from_bytes(FONT_BOLD).ok_or("failed to load bold font")?;
        let terminal_regular = Font::try_from_bytes(FONT_TERMINAL_REGULAR)
            .ok_or("failed to load terminal regular font")?;
        let terminal_bold =
            Font::try_from_bytes(FONT_TERMINAL_BOLD).ok_or("failed to load terminal bold font")?;
        let wallpaper_pixels = render_wallpaper_pixels(
            WALLPAPERS[0].bytes,
            screen.width_in_pixels,
            screen.height_in_pixels,
        )?;
        let (clipboard_image_preview_tx, clipboard_image_preview_rx) = mpsc::channel();
        let mut wallpaper_cache = vec![None; WALLPAPERS.len()];
        wallpaper_cache[0] = Some(wallpaper_pixels.clone());
        let wallpaper_previews = vec![None; WALLPAPERS.len()];
        let mut settings = SettingsState::default();
        if let Some(enabled) = compositor_override {
            settings.compositor_enabled = enabled;
            save_app_commands(&settings)?;
        }
        let compositor_active = if settings.compositor_enabled {
            init_light_compositor(&conn, screen.root)
        } else {
            eprintln!("aurora-wm: light compositor disabled by setting");
            false
        };
        let shape_supported = conn
            .extension_information(shape::X11_EXTENSION_NAME)?
            .is_some();
        let ui = UiWindows {
            topbar: conn.generate_id()?,
            dock: conn.generate_id()?,
            settings: conn.generate_id()?,
            folder: conn.generate_id()?,
            folder_terminal: conn.generate_id()?,
            screenshot_overlay: conn.generate_id()?,
            app_menu: conn.generate_id()?,
            aurora_menu: conn.generate_id()?,
            clipboard_menu: conn.generate_id()?,
            media: [
                conn.generate_id()?,
                conn.generate_id()?,
                conn.generate_id()?,
                conn.generate_id()?,
                conn.generate_id()?,
            ],
            dock_more_menu: conn.generate_id()?,
        };
        let mut sampler = SystemSampler::new();
        let metrics = sampler.sample();
        let (terminal_apps, browser_apps, photo_apps, video_apps) = discover_installed_apps();
        let display_modes =
            read_display_modes(&display, screen.width_in_pixels, screen.height_in_pixels);
        let workspace_ui = (0..DEFAULT_WORKSPACE_COUNT)
            .map(|_| WorkspaceUiState::new(screen.height_in_pixels))
            .collect();
        let wm_s_atom = conn
            .intern_atom(false, format!("WM_S{}", screen_num).as_bytes())?
            .reply()?
            .atom;
        let mut app = Self {
            conn,
            display,
            root: screen.root,
            depth: screen.root_depth,
            visual: screen.root_visual,
            gc,
            cursor,
            screen_width: screen.width_in_pixels,
            screen_height: screen.height_in_pixels,
            wallpaper_index: 0,
            wallpaper_pixels,
            wallpaper_cache,
            wallpaper_previews,
            wallpaper_pixmap: None,
            compositor_active,
            shape_supported,
            ui,
            regular,
            bold,
            terminal_regular,
            terminal_bold,
            settings,
            terminal_apps,
            browser_apps,
            photo_apps,
            video_apps,
            display_modes,
            sampler,
            metrics,
            clients: HashMap::new(),
            workspace_count: DEFAULT_WORKSPACE_COUNT,
            active_workspace: 0,
            workspace_ui,
            active_client: None,
            drag: None,
            pending_resize: None,
            pending_ui_resize: None,
            ui_resize: None,
            pending_client_drag: None,
            title_hover: None,
            ignored_unmaps: Vec::new(),
            settings_visible: true,
            settings_front: false,
            folder_mode: FolderMode::Home,
            folder_entries: folder_entries_for(FolderMode::Home, FolderSort::Name),
            folder_places: place_entries(),
            folder_path: folder_path_for(FolderMode::Home),
            folder_selected: None,
            folder_scroll: 0,
            folder_front: false,
            folder_more_open: false,
            folder_sort_open: false,
            folder_sort: FolderSort::Name,
            folder_width: FOLDER_DEFAULT_WIDTH,
            folder_height: (screen.height_in_pixels as f32 * 0.5) as u16,
            folder_terminal_width: TERMINAL_DEFAULT_WIDTH,
            folder_terminal_height: (screen.height_in_pixels as f32 * 0.4) as u16,
            folder_terminal: FolderTerminal::new(folder_path_for(FolderMode::Home)),
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
            app_menu_visible: false,
            app_menu_more: false,
            app_menu_scroll: 0,
            dock_more_visible: false,
            aurora_menu_visible: false,
            aurora_menu_about: false,
            aurora_menu_restart_confirm: false,
            clipboard_menu_visible: false,
            clipboard_history: read_clipboard_history_store(),
            clipboard_history_page: 0,
            clipboard_image_previews: HashMap::new(),
            clipboard_image_preview_pending: HashSet::new(),
            clipboard_image_preview_tx,
            clipboard_image_preview_rx,
            clipboard_poll_rx: None,
            last_clipboard_poll: Instant::now(),
            clipboard_watch_supported: false,
            clipboard_dirty: true,
            last_seen_clipboard_text: None,
            last_seen_clipboard_image_sig: None,
            wm_s_atom,
            folder_context_open: false,
            folder_context_pos: (0, 0),
            folder_clipboard: None,
            folder_info: None,
            folder_terminal_selection: None,
            folder_terminal_selecting: false,
            folder_terminal_live_rects: Vec::new(),
            folder_drag: None,
            folder_press: None,
            xdnd_source: None,
            dock_last_click: None,
            icon_cache: HashMap::new(),
            last_clock_label: format_clock(),
            last_tick: Instant::now(),
            last_media_tick: Instant::now(),
            last_pointer_pos: None,
            last_pointer_activity: Instant::now(),
            pending_auto_power_saver_apply: None,
            screenshot_mode: false,
            screenshot_selection: None,
            screenshot_base: None,
            screenshot_live_rect: None,
            pending_screenshot_button: None,
            topbar_notice: None,
            ffplay_process: None,
            pending_window_nudges: Vec::new(),
            wifi_refresh_rx: None,
            focus_history: Vec::new(),
            alt_tab_index: 0,
            alt_tab_windows: Vec::new(),
            choose_file_mode: false,
        };
        app.apply_sleep_timeout();
        if app.settings.auto_power_saver_enabled && app.settings.auto_power_saver_minutes > 0 {
            let _ = touch_notidle_marker();
            let _ = app.set_power_mode(PowerMode::Performance);
        }
        let _ = save_app_commands(&app.settings);
        app.create_ui_windows()?;
        if let Err(err) = app.init_clipboard_watcher() {
            eprintln!("aurora-wm: clipboard watcher unavailable, using polling: {err}");
        }
        Ok(app)
    }

    fn run_loop(&mut self) -> AnyResult<()> {
        let trace_events = env::var_os("AURORA_TRACE_EVENTS").is_some();
        let mut trace_counts: HashMap<&'static str, usize> = HashMap::new();
        let mut next_trace_log = Instant::now() + Duration::from_secs(1);
        let mut next_pointer_poll = Instant::now();
        loop {
            let mut handled_event = false;
            let mut pending_motion = None;
            while let Some(event) = self.conn.poll_for_event()? {
                if trace_events {
                    *trace_counts.entry(event_name(&event)).or_default() += 1;
                }
                if let Event::MotionNotify(ev) = event {
                    pending_motion = Some(ev);
                } else {
                    handled_event = true;
                    if let Some(ev) = pending_motion.take() {
                        handled_event |= self.handle_motion_notify(ev)?;
                    }
                    self.handle_event(event)?;
                }
            }
            if let Some(ev) = pending_motion.take() {
                handled_event |= self.handle_motion_notify(ev)?;
            }

            if self.folder_terminal.visible && self.poll_folder_terminal()? {
                handled_event = true;
            }
            if self.folder_terminal.visible && self.sync_folder_to_terminal_cwd()? {
                handled_event = true;
            }

            if let Some(pending) = self.pending_resize {
                if pending.pressed_at.elapsed() >= Duration::from_secs(2) {
                    self.pending_resize = None;
                    self.start_resize(
                        pending.client,
                        pending.root_x,
                        pending.root_y,
                        pending.edges,
                    )?;
                }
            }

            if let Some(pending) = self.pending_ui_resize {
                if pending.pressed_at.elapsed() >= Duration::from_secs(1) {
                    self.pending_ui_resize = None;
                    self.start_ui_resize(pending)?;
                }
            }

            let needs_pointer_poll = self.pending_resize.is_some()
                || self.pending_ui_resize.is_some()
                || self.pending_client_drag.is_some()
                || self.drag.is_some()
                || self.ui_resize.is_some()
                || self.pending_screenshot_button.is_some();
            let now = Instant::now();
            let pointer = if needs_pointer_poll && now >= next_pointer_poll {
                let interval = if self.ui_resize.is_some() {
                    COMPOSITED_MOVE_INTERVAL
                } else if self
                    .drag
                    .is_some_and(|drag| matches!(drag.kind, DragKind::Move))
                    && !self.compositor_active
                {
                    NON_COMPOSITED_MOVE_INTERVAL
                } else if self.drag.is_some() {
                    COMPOSITED_MOVE_INTERVAL
                } else {
                    Duration::from_millis(50)
                };
                next_pointer_poll = now + interval;
                Some(self.conn.query_pointer(self.root)?.reply()?)
            } else {
                None
            };

            if let (Some(pending), Some(pointer)) = (self.pending_resize, pointer.as_ref()) {
                let button_down = u16::from(pointer.mask) & u16::from(KeyButMask::BUTTON1) != 0;
                if !button_down || pending.pressed_at.elapsed() >= Duration::from_secs(5) {
                    self.pending_resize = None;
                    let _ = self.conn.ungrab_pointer(CURRENT_TIME);
                }
            }

            if let (Some(pending), Some(pointer)) = (self.pending_ui_resize, pointer.as_ref()) {
                let button_down = u16::from(pointer.mask) & u16::from(KeyButMask::BUTTON1) != 0;
                if !button_down || pending.pressed_at.elapsed() >= Duration::from_secs(5) {
                    self.pending_ui_resize = None;
                    let _ = self.conn.ungrab_pointer(CURRENT_TIME);
                }
            }

            if let (Some(pending), Some(pointer)) = (self.pending_client_drag, pointer.as_ref()) {
                let button_down = u16::from(pointer.mask) & u16::from(KeyButMask::BUTTON1) != 0;
                if !button_down {
                    self.pending_client_drag = None;
                } else {
                    let moved = (i32::from(pointer.root_x) - i32::from(pending.root_x)).abs() > 4
                        || (i32::from(pointer.root_y) - i32::from(pending.root_y)).abs() > 4;
                    if moved && pending.pressed_at.elapsed() >= Duration::from_millis(500) {
                        self.pending_client_drag = None;
                        self.start_drag(pending.client, pointer.root_x, pointer.root_y)?;
                    } else if pending.pressed_at.elapsed() >= Duration::from_secs(2) {
                        self.pending_client_drag = None;
                    }
                }
            }
            if let Some(pointer) = pointer.as_ref().filter(|_| self.drag.is_some()) {
                let button_down = u16::from(pointer.mask) & u16::from(KeyButMask::BUTTON1) != 0;
                if button_down {
                    handled_event |= self.update_drag_position(pointer.root_x, pointer.root_y)?;
                } else {
                    self.drag = None;
                    let _ = self.conn.ungrab_pointer(CURRENT_TIME);
                }
            }
            if let Some(pointer) = pointer.as_ref().filter(|_| self.ui_resize.is_some()) {
                let button_down = u16::from(pointer.mask) & u16::from(KeyButMask::BUTTON1) != 0;
                if button_down {
                    handled_event |= self.update_ui_resize(pointer.root_x, pointer.root_y)?;
                } else {
                    self.end_drag()?;
                }
            }

            if let (Some(pending), Some(pointer)) =
                (self.pending_screenshot_button, pointer.as_ref())
            {
                let button_down = u16::from(pointer.mask) & u16::from(KeyButMask::BUTTON1) != 0;
                if button_down && pending.pressed_at.elapsed() >= Duration::from_secs(2) {
                    self.pending_screenshot_button = None;
                    self.capture_screenshot(None)?;
                    handled_event = true;
                } else if !button_down {
                    self.pending_screenshot_button = None;
                }
            }

            if self
                .topbar_notice
                .as_ref()
                .is_some_and(|(_, until)| Instant::now() >= *until)
            {
                self.topbar_notice = None;
                self.redraw_topbar()?;
                handled_event = true;
            }

            if handled_event {
                self.conn.flush()?;
            }

            if self.has_playing_internal_media()
                && self.last_media_tick.elapsed() >= Duration::from_millis(250)
            {
                self.last_media_tick = Instant::now();
                if self.advance_internal_media()? {
                    self.conn.flush()?;
                }
            }

            if self.process_pending_window_nudges()? {
                handled_event = true;
            }
            self.reap_ffplay_process();

            if self
                .pending_auto_power_saver_apply
                .is_some_and(|at| Instant::now() >= at)
            {
                self.pending_auto_power_saver_apply = None;
                if self.apply_auto_power_saver_setting()? {
                    self.conn.flush()?;
                }
            }

            let interactive = self.drag.is_some()
                || self.ui_resize.is_some()
                || self.pending_resize.is_some()
                || self.pending_ui_resize.is_some()
                || self.pending_client_drag.is_some()
                || self.pending_screenshot_button.is_some();

            if !interactive && self.last_tick.elapsed() >= IDLE_CHECK_INTERVAL {
                self.last_tick = Instant::now();
                let mut idle_changed = false;

                if self.poll_wifi_refresh()? {
                    idle_changed = true;
                }
                if self.poll_clipboard_history()? {
                    idle_changed = true;
                }
                if self.poll_clipboard_image_previews()? {
                    idle_changed = true;
                }
                if self.refresh_folder_entries() {
                    self.redraw_folder()?;
                    idle_changed = true;
                }
                if self.sync_current_power_mode()? {
                    idle_changed = true;
                }
                if self.settings.auto_power_saver_enabled
                    && self.settings.auto_power_saver_minutes > 0
                    && self.update_auto_power_saver()?
                {
                    idle_changed = true;
                }

                let clock_label = format_clock();
                let clock_changed = clock_label != self.last_clock_label;
                let metrics_visible = self.settings_visible
                    && matches!(self.settings.tab, SettingsTab::Power | SettingsTab::About);
                if clock_changed || metrics_visible {
                    self.metrics = self.sampler.sample();
                }
                if clock_changed {
                    self.last_clock_label = clock_label;
                    self.redraw_topbar()?;
                    idle_changed = true;
                }
                if self.settings_visible {
                    if self.settings.tab == SettingsTab::Network {
                        self.redraw_settings()?;
                        idle_changed = true;
                    } else if matches!(self.settings.tab, SettingsTab::Power | SettingsTab::About) {
                        self.redraw_settings()?;
                        idle_changed = true;
                    }
                }
                if idle_changed {
                    self.conn.flush()?;
                }
            }

            if trace_events && Instant::now() >= next_trace_log {
                if !trace_counts.is_empty() {
                    eprintln!("event counts: {:?}", trace_counts);
                    trace_counts.clear();
                }
                next_trace_log = Instant::now() + Duration::from_secs(1);
            }

            wait_for_x_event_or_timeout(
                &self.conn,
                self.loop_wait_timeout(handled_event, needs_pointer_poll, next_pointer_poll),
            );
        }
    }

    fn has_playing_internal_media(&self) -> bool {
        self.media_slots.iter().any(|slot| {
            slot.as_ref().is_some_and(|media| {
                media.playing && matches!(media.entry.kind, FileKind::Audio | FileKind::Video)
            })
        })
    }

    fn loop_wait_timeout(
        &self,
        handled_event: bool,
        needs_pointer_poll: bool,
        next_pointer_poll: Instant,
    ) -> Duration {
        let now = Instant::now();
        let interactive = self.drag.is_some()
            || self.ui_resize.is_some()
            || self.pending_resize.is_some()
            || self.pending_ui_resize.is_some()
            || self.pending_client_drag.is_some()
            || self.pending_screenshot_button.is_some();
        let mut timeout = if handled_event {
            if interactive {
                Duration::from_millis(1)
            } else {
                Duration::from_millis(4)
            }
        } else if interactive {
            Duration::from_millis(16)
        } else {
            IDLE_CHECK_INTERVAL
        };

        if needs_pointer_poll {
            timeout = timeout.min(next_pointer_poll.saturating_duration_since(now));
        }
        if let Some(pending) = self.pending_resize {
            timeout = timeout
                .min((pending.pressed_at + Duration::from_secs(2)).saturating_duration_since(now));
        }
        if let Some(pending) = self.pending_ui_resize {
            timeout = timeout
                .min((pending.pressed_at + Duration::from_secs(1)).saturating_duration_since(now));
        }
        if let Some(pending) = self.pending_client_drag {
            timeout = timeout.min(
                (pending.pressed_at + Duration::from_millis(500)).saturating_duration_since(now),
            );
        }
        if let Some(pending) = self.pending_screenshot_button {
            timeout = timeout
                .min((pending.pressed_at + Duration::from_secs(2)).saturating_duration_since(now));
        }
        if let Some((_, until)) = self.topbar_notice.as_ref() {
            timeout = timeout.min((*until).saturating_duration_since(now));
        }
        if let Some(at) = self.pending_auto_power_saver_apply {
            timeout = timeout.min(at.saturating_duration_since(now));
        }
        if self.has_playing_internal_media() {
            timeout = timeout.min(
                (self.last_media_tick + Duration::from_millis(250)).saturating_duration_since(now),
            );
        }
        if !interactive {
            timeout =
                timeout.min((self.last_tick + IDLE_CHECK_INTERVAL).saturating_duration_since(now));
        }
        timeout
    }

    fn update_auto_power_saver(&mut self) -> AnyResult<bool> {
        self.mark_current_display_activity()?;
        let threshold =
            Duration::from_secs(u64::from(self.settings.auto_power_saver_minutes.max(1)) * 60);
        let idle_long_enough = notidle_marker_age().is_none_or(|age| age > threshold);
        if !idle_long_enough && self.settings.power_mode != PowerMode::Performance {
            self.set_power_mode(PowerMode::Performance)?;
            if self.settings_visible && self.settings.tab == SettingsTab::Power {
                self.redraw_settings()?;
            }
            return Ok(true);
        }
        if idle_long_enough && self.settings.power_mode != PowerMode::Saver {
            self.set_power_mode(PowerMode::Saver)?;
            if self.settings_visible && self.settings.tab == SettingsTab::Power {
                self.redraw_settings()?;
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn mark_current_display_activity(&mut self) -> AnyResult<()> {
        let active_window_ms = (IDLE_CHECK_INTERVAL + Duration::from_millis(250)).as_millis();
        if let Ok(cookie) = self.conn.screensaver_query_info(self.root) {
            if let Ok(info) = cookie.reply() {
                if u128::from(info.ms_since_user_input) <= active_window_ms {
                    touch_notidle_marker()?;
                }
                return Ok(());
            }
        }

        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        let pos = (pointer.root_x, pointer.root_y);
        let moved = self.last_pointer_pos.is_none_or(|last| last != pos);
        self.last_pointer_pos = Some(pos);
        if moved {
            self.last_pointer_activity = Instant::now();
        }
        if moved
            || self
                .last_pointer_activity
                .elapsed()
                .saturating_sub(IDLE_CHECK_INTERVAL)
                < Duration::from_millis(250)
        {
            touch_notidle_marker()?;
        }
        Ok(())
    }

    fn sync_current_power_mode(&mut self) -> AnyResult<bool> {
        let Some(mode) = current_power_mode_cached_or_refresh() else {
            return Ok(false);
        };
        if mode == self.settings.power_mode {
            return Ok(false);
        }
        self.settings.power_mode = mode;
        Ok(true)
    }

    fn apply_auto_power_saver_setting(&mut self) -> AnyResult<bool> {
        self.settings.auto_power_saver_minutes = self
            .settings
            .auto_power_saver_input
            .trim()
            .parse::<u32>()
            .unwrap_or(0)
            .min(240);
        if self.settings.auto_power_saver_minutes == 0 {
            self.settings.auto_power_saver_input = "0".to_string();
            self.settings.auto_power_saver_enabled = false;
            self.last_pointer_pos = None;
            save_app_commands(&self.settings)?;
            return Ok(true);
        }
        self.settings.auto_power_saver_input = self.settings.auto_power_saver_minutes.to_string();
        self.last_pointer_activity = Instant::now();
        self.last_pointer_pos = None;
        if self.settings.auto_power_saver_enabled {
            touch_notidle_marker()?;
            self.set_power_mode(PowerMode::Performance)?;
        }
        save_app_commands(&self.settings)?;
        Ok(true)
    }

    fn init_clipboard_watcher(&mut self) -> AnyResult<()> {
        if self
            .conn
            .extension_information(xfixes::X11_EXTENSION_NAME)?
            .is_none()
        {
            return Ok(());
        }
        self.conn.xfixes_query_version(5, 0)?.reply()?;
        let clipboard = self.atom(b"CLIPBOARD")?;
        let mask = xfixes::SelectionEventMask::SET_SELECTION_OWNER
            | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
            | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE;
        self.conn
            .xfixes_select_selection_input(self.root, clipboard, mask)?
            .check()?;
        self.clipboard_watch_supported = true;
        self.clipboard_dirty = true;
        Ok(())
    }

    fn create_ui_windows(&mut self) -> AnyResult<()> {
        self.grab_root_button1()?;
        self.grab_alt_tab()?;
        self.grab_workspace_keys()?;
        let top_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE)
            .cursor(self.cursor)
            .background_pixel(0);
        self.conn.create_window(
            self.depth,
            self.ui.topbar,
            self.root,
            0,
            0,
            self.screen_width,
            TOPBAR_HEIGHT,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &top_aux,
        )?;

        let dock = self.dock_geometry();
        let dock_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS)
            .cursor(self.cursor)
            .background_pixel(0);
        self.conn.create_window(
            self.depth,
            self.ui.dock,
            self.root,
            dock.0,
            dock.1,
            dock.2,
            dock.3,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &dock_aux,
        )?;

        let settings = self.settings_geometry();
        let settings_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::POINTER_MOTION
                    | EventMask::KEY_PRESS,
            )
            .cursor(self.cursor)
            .background_pixel(0)
            .bit_gravity(Gravity::NORTH_WEST)
            .backing_store(BackingStore::WHEN_MAPPED)
            .save_under(1);
        self.conn.create_window(
            self.depth,
            self.ui.settings,
            self.root,
            settings.0,
            settings.1,
            settings.2,
            settings.3,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &settings_aux,
        )?;

        let folder = self.folder_geometry();
        let folder_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION,
            )
            .cursor(self.cursor)
            .background_pixel(0)
            .bit_gravity(Gravity::NORTH_WEST)
            .backing_store(BackingStore::WHEN_MAPPED)
            .save_under(1);
        self.conn.create_window(
            self.depth,
            self.ui.folder,
            self.root,
            folder.0,
            folder.1,
            folder.2,
            folder.3,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &folder_aux,
        )?;
        self.init_folder_dnd()?;

        let terminal = self.folder_terminal_geometry();
        let terminal_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::KEY_PRESS,
            )
            .cursor(self.cursor)
            .background_pixel(0)
            .bit_gravity(Gravity::NORTH_WEST)
            .backing_store(BackingStore::WHEN_MAPPED)
            .save_under(1);
        self.conn.create_window(
            self.depth,
            self.ui.folder_terminal,
            self.root,
            terminal.0,
            terminal.1,
            terminal.2,
            terminal.3,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &terminal_aux,
        )?;

        let overlay_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION,
            )
            .cursor(self.cursor)
            .background_pixel(0);
        self.conn.create_window(
            self.depth,
            self.ui.screenshot_overlay,
            self.root,
            0,
            0,
            self.screen_width,
            self.screen_height,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &overlay_aux,
        )?;

        let menu = self.app_menu_geometry();
        let menu_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS)
            .cursor(self.cursor)
            .background_pixel(0);
        self.conn.create_window(
            self.depth,
            self.ui.app_menu,
            self.root,
            menu.0,
            menu.1,
            menu.2,
            menu.3,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &menu_aux,
        )?;

        let more_menu = self.dock_more_menu_geometry();
        let more_menu_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS)
            .cursor(self.cursor)
            .background_pixel(0);
        self.conn.create_window(
            self.depth,
            self.ui.dock_more_menu,
            self.root,
            more_menu.0,
            more_menu.1,
            more_menu.2,
            more_menu.3,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &more_menu_aux,
        )?;

        let aurora_menu = self.aurora_menu_geometry();
        let aurora_menu_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS)
            .cursor(self.cursor)
            .background_pixel(0);
        self.conn.create_window(
            self.depth,
            self.ui.aurora_menu,
            self.root,
            aurora_menu.0,
            aurora_menu.1,
            aurora_menu.2,
            aurora_menu.3,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &aurora_menu_aux,
        )?;

        let clipboard_menu = self.clipboard_menu_geometry();
        let clipboard_menu_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS)
            .cursor(self.cursor)
            .background_pixel(0);
        self.conn.create_window(
            self.depth,
            self.ui.clipboard_menu,
            self.root,
            clipboard_menu.0,
            clipboard_menu.1,
            clipboard_menu.2,
            clipboard_menu.3,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &clipboard_menu_aux,
        )?;

        let media_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::KEY_PRESS,
            )
            .cursor(self.cursor)
            .background_pixel(0);
        for (idx, window) in self.ui.media.iter().copied().enumerate() {
            let media = self.media_geometry(idx);
            self.conn.create_window(
                self.depth,
                window,
                self.root,
                media.0,
                media.1,
                media.2,
                media.3,
                0,
                WindowClass::INPUT_OUTPUT,
                self.visual,
                &media_aux,
            )?;
        }

        self.conn.map_window(self.ui.folder)?;
        self.conn.map_window(self.ui.topbar)?;
        self.conn.map_window(self.ui.dock)?;
        self.conn.map_window(self.ui.settings)?;

        // Initialize EWMH desktops on the root window
        if let Ok(num_atom) = self.atom(b"_NET_NUMBER_OF_DESKTOPS") {
            if let Ok(cardinal_atom) = self.atom(b"CARDINAL") {
                let _ = self.conn.change_property32(
                    PropMode::REPLACE,
                    self.root,
                    num_atom,
                    cardinal_atom,
                    &[self.workspace_count as u32],
                );
            }
        }
        if let Ok(cur_atom) = self.atom(b"_NET_CURRENT_DESKTOP") {
            if let Ok(cardinal_atom) = self.atom(b"CARDINAL") {
                let _ = self.conn.change_property32(
                    PropMode::REPLACE,
                    self.root,
                    cur_atom,
                    cardinal_atom,
                    &[self.active_workspace as u32],
                );
            }
        }

        self.install_pointer_cursor()?;
        self.raise_ui()?;
        self.conn.flush()?;
        Ok(())
    }

    fn install_pointer_cursor(&self) -> AnyResult<()> {
        let mut windows = vec![
            self.root,
            self.ui.topbar,
            self.ui.dock,
            self.ui.settings,
            self.ui.folder,
            self.ui.folder_terminal,
            self.ui.app_menu,
            self.ui.aurora_menu,
            self.ui.clipboard_menu,
            self.ui.dock_more_menu,
        ];
        windows.extend(self.ui.media);
        windows.extend(self.clients.values().map(|info| info.frame));

        for window in windows {
            self.conn.change_window_attributes(
                window,
                &ChangeWindowAttributesAux::new().cursor(self.cursor),
            )?;
        }
        if self
            .conn
            .extension_information(xfixes::X11_EXTENSION_NAME)?
            .is_some()
        {
            let _ = self.conn.xfixes_show_cursor(self.root);
        }
        Ok(())
    }

    fn init_folder_dnd(&self) -> AnyResult<()> {
        let xdnd_aware = self.atom(b"XdndAware")?;
        let atom_type = self.atom(b"ATOM")?;
        self.conn.change_property32(
            PropMode::REPLACE,
            self.ui.folder,
            xdnd_aware,
            atom_type,
            &[5],
        )?;
        Ok(())
    }

    fn scan_existing_windows(&mut self) -> AnyResult<()> {
        let reply = self.conn.query_tree(self.root)?.reply()?;
        for window in reply.children {
            if let Err(err) = self.adopt_mapped_root_window(window) {
                eprintln!("aurora-wm: failed to adopt existing window {window}: {err}");
            }
        }
        Ok(())
    }

    fn adopt_mapped_root_window(&mut self, window: Window) -> AnyResult<()> {
        if self.is_ui_window(window) || self.client_key_for(window).is_some() {
            return Ok(());
        }
        let attr = self.conn.get_window_attributes(window)?.reply()?;
        if !attr.override_redirect && attr.map_state != MapState::UNMAPPED {
            self.manage_window(window)?;
        }
        Ok(())
    }

    fn handle_event(&mut self, event: Event) -> AnyResult<()> {
        match event {
            Event::Expose(ev) => self.handle_expose(ev)?,
            Event::KeyPress(ev) => self.handle_key_press(ev)?,
            Event::KeyRelease(ev) => self.handle_key_release(ev)?,
            Event::ButtonPress(ev) => self.handle_button_press(ev)?,
            Event::ButtonRelease(ev) => {
                if self.pending_screenshot_button.is_some() {
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
            Event::MapRequest(ev) => self.manage_window(ev.window)?,
            Event::MapNotify(ev) => {
                if ev.event == self.root && !ev.override_redirect {
                    // Save-set restoration can map a surviving client after startup scanning.
                    let _ = self.adopt_mapped_root_window(ev.window);
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

    fn handle_xfixes_selection_notify(
        &mut self,
        ev: xfixes::SelectionNotifyEvent,
    ) -> AnyResult<()> {
        if ev.selection == self.atom(b"CLIPBOARD")? {
            self.clipboard_dirty = true;
        }
        Ok(())
    }

    fn handle_property_notify(&mut self, ev: PropertyNotifyEvent) -> AnyResult<()> {
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

    fn handle_expose(&mut self, ev: ExposeEvent) -> AnyResult<()> {
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

    fn handle_button_press(&mut self, ev: ButtonPressEvent) -> AnyResult<()> {
        self.last_pointer_activity = Instant::now();
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
        if self.screenshot_mode && ev.detail == 1 {
            self.start_screenshot_selection(ev.root_x, ev.root_y)?;
            if ev.event == self.root {
                self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
            }
        } else if ev.event == self.ui.settings {
            if ev.detail == 4 || ev.detail == 5 {
                self.handle_settings_scroll(
                    ev.detail,
                    i32::from(ev.event_x),
                    i32::from(ev.event_y),
                )?;
                self.conn.flush()?;
                return Ok(());
            }
            self.settings_front = true;
            self.folder_front = false;
            self.media_front = false;
            self.raise_ui()?;
            self.handle_settings_click(i32::from(ev.event_x), i32::from(ev.event_y))?;
        } else if ev.event == self.ui.dock {
            self.handle_dock_click(i32::from(ev.event_x), i32::from(ev.event_y))?;
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
        } else if ev.event == self.ui.app_menu {
            self.handle_app_menu_click(ev.detail, i32::from(ev.event_x), i32::from(ev.event_y))?;
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

    fn handle_motion_notify(&mut self, ev: MotionNotifyEvent) -> AnyResult<bool> {
        self.last_pointer_activity = Instant::now();
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
                if moved && pending.pressed_at.elapsed() >= Duration::from_millis(500) {
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

    fn update_drag_position(&mut self, root_x: i16, root_y: i16) -> AnyResult<bool> {
        self.update_drag_position_inner(root_x, root_y, false)
    }

    fn update_drag_position_inner(
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

    fn handle_leave_notify(&mut self, ev: LeaveNotifyEvent) -> AnyResult<()> {
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

    fn handle_enter_notify(&mut self, _ev: EnterNotifyEvent) -> AnyResult<()> {
        Ok(())
    }

    fn handle_client_message(&mut self, ev: ClientMessageEvent) -> AnyResult<()> {
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
            8 => self.start_drag(client, root_x, root_y)?,
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
            _ => {}
        }
        Ok(())
    }

    fn handle_xdnd_message(&mut self, ev: ClientMessageEvent) -> AnyResult<()> {
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

    fn handle_selection_request(&self, ev: SelectionRequestEvent) -> AnyResult<()> {
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

    fn handle_selection_notify(&mut self, ev: SelectionNotifyEvent) -> AnyResult<()> {
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

    fn handle_frame_click(&mut self, client: Window, ev: ButtonPressEvent) -> AnyResult<()> {
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
            self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
            self.start_drag(client, ev.root_x, ev.root_y)?;
        }
        Ok(())
    }

    fn handle_client_click(&mut self, client: Window, ev: ButtonPressEvent) -> AnyResult<()> {
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
            if !info.titlebar && ev.detail == 1 && client_y >= 0 && client_y <= CSD_DRAG_TOP_HEIGHT
            {
                self.pending_client_drag = Some(PendingClientDrag {
                    client,
                    root_x: ev.root_x,
                    root_y: ev.root_y,
                    pressed_at: Instant::now(),
                });
            }
            self.focus_window_at(client, ev.time)?;
            self.conn.flush()?;
            self.conn.allow_events(Allow::REPLAY_POINTER, ev.time)?;
        }
        Ok(())
    }

    fn handle_root_button_press(&mut self, ev: ButtonPressEvent) -> AnyResult<()> {
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
                let client_y = pointer
                    .root_y
                    .saturating_sub(info.y)
                    .saturating_sub(title_h);
                if !info.titlebar && client_y >= 0 && client_y <= CSD_DRAG_TOP_HEIGHT {
                    self.pending_client_drag = Some(PendingClientDrag {
                        client,
                        root_x: pointer.root_x,
                        root_y: pointer.root_y,
                        pressed_at: Instant::now(),
                    });
                }
            }
        }
        self.conn.allow_events(Allow::REPLAY_POINTER, ev.time)?;
        Ok(())
    }

    fn handle_topbar_press_x(&mut self, x: i32) -> AnyResult<bool> {
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

    fn grab_root_button1(&self) -> AnyResult<()> {
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

    fn grab_alt_tab(&self) -> AnyResult<()> {
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

    fn grab_workspace_keys(&self) -> AnyResult<()> {
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

    fn keycode_for_keysym(&self, target: u32) -> AnyResult<Option<u8>> {
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

    fn configure_managed_client(
        &self,
        info: &ClientInfo,
        stack: Option<StackMode>,
    ) -> AnyResult<()> {
        let title_h = self.titlebar_height(info);
        let mut frame_aux = ConfigureWindowAux::new()
            .x(i32::from(info.x))
            .y(i32::from(info.y))
            .width(u32::from(info.width))
            .height(u32::from(info.height + title_h));
        if let Some(stack) = stack {
            frame_aux = frame_aux.stack_mode(stack);
        }
        self.conn.configure_window(info.frame, &frame_aux)?;
        self.conn.configure_window(
            info.window,
            &ConfigureWindowAux::new()
                .x(0)
                .y(i32::from(title_h))
                .width(u32::from(info.width))
                .height(u32::from(info.height))
                .border_width(0),
        )?;
        self.apply_frame_shape(info)?;
        self.send_synthetic_configure(info)
    }

    fn send_synthetic_configure(&self, info: &ClientInfo) -> AnyResult<()> {
        let title_h = self.titlebar_height(info);
        let client_y = (i32::from(info.y) + i32::from(title_h))
            .clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        let event = ConfigureNotifyEvent {
            response_type: CONFIGURE_NOTIFY_EVENT,
            sequence: 0,
            event: info.window,
            window: info.window,
            above_sibling: x11rb::NONE,
            x: info.x,
            y: client_y,
            width: info.width,
            height: info.height,
            border_width: 0,
            override_redirect: false,
        };
        self.conn
            .send_event(false, info.window, EventMask::STRUCTURE_NOTIFY, event)?;
        Ok(())
    }

    fn ffplay_geometry(&self) -> (i16, i16, u16, u16) {
        let folder = self.folder_geometry();
        let preferred_x = i32::from(folder.0) + i32::from(folder.2) + 8;
        let preferred_w = (self.screen_width / 2).max(300.min(self.screen_width));
        let max_w_at_preferred = i32::from(self.screen_width)
            .saturating_sub(preferred_x + 16)
            .max(240) as u16;
        let width = preferred_w.min(max_w_at_preferred);
        let x = preferred_x
            .min(i32::from(self.screen_width.saturating_sub(width + 16)))
            .max(16) as i16;
        let height = folder
            .3
            .min(
                self.screen_height
                    .saturating_sub(TOPBAR_HEIGHT + DOCK_HEIGHT + 48),
            )
            .max(240.min(self.screen_height));
        (x, folder.1, width, height)
    }

    fn schedule_window_nudge(&mut self, client: Window, width: u16, height: u16) {
        self.pending_window_nudges
            .retain(|pending| pending.client != client);
        self.pending_window_nudges.push(PendingWindowNudge {
            client,
            base_width: width,
            base_height: height,
            step: 0,
            at: Instant::now() + Duration::from_millis(850),
        });
    }

    fn process_pending_window_nudges(&mut self) -> AnyResult<bool> {
        let now = Instant::now();
        let mut idx = 0;
        let mut changed = false;
        while idx < self.pending_window_nudges.len() {
            if now < self.pending_window_nudges[idx].at {
                idx += 1;
                continue;
            }
            let mut pending = self.pending_window_nudges[idx];
            let Some(mut info) = self.clients.get(&pending.client).copied() else {
                self.pending_window_nudges.swap_remove(idx);
                continue;
            };
            info.width = if pending.step == 0 {
                pending.base_width.saturating_add(MEDIA_WINDOW_NUDGE_WIDTH)
            } else {
                pending.base_width
            };
            info.height = pending.base_height;
            self.configure_managed_client(&info, Some(StackMode::ABOVE))?;
            self.clients.insert(pending.client, info);
            self.redraw_frame_titlebar(pending.client)?;
            changed = true;

            if pending.step == 0 {
                pending.step = 1;
                pending.at = now + Duration::from_millis(90);
                self.pending_window_nudges[idx] = pending;
                idx += 1;
            } else {
                self.pending_window_nudges.swap_remove(idx);
            }
        }
        Ok(changed)
    }

    fn start_drag(&mut self, client: Window, root_x: i16, root_y: i16) -> AnyResult<()> {
        let Some(info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        self.drag = Some(DragState {
            client,
            offset_x: root_x.saturating_sub(info.x),
            offset_y: root_y.saturating_sub(info.y),
            start_root_x: root_x,
            start_root_y: root_y,
            start_x: info.x,
            start_y: info.y,
            start_w: info.width,
            start_h: info.height,
            kind: DragKind::Move,
            resize_edges: ResizeEdges::default(),
            last_update_at: Instant::now() - Duration::from_millis(16),
        });
        self.settings_front = false;
        self.folder_front = false;
        self.media_front = false;
        self.focus_window(client)?;
        if let Ok(cookie) = self.conn.grab_pointer(
            false,
            self.root,
            EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            x11rb::NONE,
            self.cursor,
            CURRENT_TIME,
        ) {
            let _ = cookie.reply();
        }
        Ok(())
    }

    fn start_resize(
        &mut self,
        client: Window,
        root_x: i16,
        root_y: i16,
        resize_edges: ResizeEdges,
    ) -> AnyResult<()> {
        let Some(info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        self.drag = Some(DragState {
            client,
            offset_x: 0,
            offset_y: 0,
            start_root_x: root_x,
            start_root_y: root_y,
            start_x: info.x,
            start_y: info.y,
            start_w: info.width,
            start_h: info.height,
            kind: DragKind::Resize,
            resize_edges,
            last_update_at: Instant::now() - Duration::from_millis(33),
        });
        self.focus_window(client)?;
        let _ = self
            .conn
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
            .reply();
        Ok(())
    }

    fn start_ui_resize(&mut self, pending: PendingUiResize) -> AnyResult<()> {
        let (start_w, start_h) = match pending.target {
            UiResizeTarget::Folder => (self.folder_width, self.folder_height),
            UiResizeTarget::FolderTerminal => {
                (self.folder_terminal_width, self.folder_terminal_height)
            }
        };
        self.ui_resize = Some(UiResizeState {
            target: pending.target,
            start_root_x: pending.root_x,
            start_root_y: pending.root_y,
            start_w,
            start_h,
        });
        let _ = self
            .conn
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
            .reply();
        Ok(())
    }

    fn update_ui_resize(&mut self, root_x: i16, root_y: i16) -> AnyResult<bool> {
        let Some(resize) = self.ui_resize else {
            return Ok(false);
        };
        let dx = i32::from(root_x) - i32::from(resize.start_root_x);
        let dy = i32::from(root_y) - i32::from(resize.start_root_y);
        let old_folder = (self.folder_width, self.folder_height);
        let old_terminal = (self.folder_terminal_width, self.folder_terminal_height);
        match resize.target {
            UiResizeTarget::Folder => {
                let max_w = self.screen_width.saturating_sub(48).max(FOLDER_MIN_WIDTH);
                let max_h = self
                    .screen_height
                    .saturating_sub(TOPBAR_HEIGHT + DOCK_HEIGHT + 48)
                    .max(FOLDER_MIN_HEIGHT);
                self.folder_width = (i32::from(resize.start_w) + dx)
                    .clamp(FOLDER_MIN_WIDTH.into(), max_w.into())
                    as u16;
                self.folder_height = (i32::from(resize.start_h) + dy)
                    .clamp(FOLDER_MIN_HEIGHT.into(), max_h.into())
                    as u16;
                self.folder_terminal_width = self.folder_terminal_width.min(self.folder_width);
            }
            UiResizeTarget::FolderTerminal => {
                let folder = self.folder_geometry();
                let y = i32::from(folder.1) + i32::from(folder.3) + 8;
                let max_h = i32::from(self.screen_height)
                    .saturating_sub(y + 50)
                    .max(i32::from(TERMINAL_MIN_HEIGHT)) as u16;
                let max_w = self.screen_width.saturating_sub(48).max(TERMINAL_MIN_WIDTH);
                self.folder_terminal_width = (i32::from(resize.start_w) + dx)
                    .clamp(TERMINAL_MIN_WIDTH.into(), max_w.into())
                    as u16;
                self.folder_terminal_height = (i32::from(resize.start_h) + dy)
                    .clamp(TERMINAL_MIN_HEIGHT.into(), max_h.into())
                    as u16;
            }
        }
        if old_folder == (self.folder_width, self.folder_height)
            && old_terminal == (self.folder_terminal_width, self.folder_terminal_height)
        {
            return Ok(false);
        }
        let folder = self.folder_geometry();
        let terminal = self.folder_terminal_geometry();
        self.conn.configure_window(
            self.ui.folder,
            &ConfigureWindowAux::new()
                .width(u32::from(folder.2))
                .height(u32::from(folder.3)),
        )?;
        self.conn.configure_window(
            self.ui.folder_terminal,
            &ConfigureWindowAux::new()
                .x(i32::from(terminal.0))
                .y(i32::from(terminal.1))
                .width(u32::from(terminal.2))
                .height(u32::from(terminal.3)),
        )?;
        self.sync_folder_terminal_size();
        self.redraw_folder()?;
        if self.folder_terminal.visible {
            self.redraw_folder_terminal()?;
        }
        Ok(true)
    }

    fn end_drag(&mut self) -> AnyResult<()> {
        self.pending_resize = None;
        self.pending_ui_resize = None;
        self.ui_resize = None;
        if let Some(drag) = self.drag.take() {
            self.conn.ungrab_pointer(CURRENT_TIME)?;
            if matches!(drag.kind, DragKind::Move) {
                if let Some(info) = self.clients.get(&drag.client).copied() {
                    if !self.compositor_active {
                        let frame_h = info.height + self.titlebar_height(&info);
                        self.clear_root_region(
                            i32::from(info.x),
                            i32::from(info.y),
                            u32::from(info.width),
                            u32::from(frame_h),
                        )?;
                    }
                    self.redraw_frame_titlebar(drag.client)?;
                }
            }
        } else {
            let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        }
        Ok(())
    }

    fn handle_configure_request(&mut self, ev: ConfigureRequestEvent) -> AnyResult<()> {
        if self.is_ui_window(ev.window) {
            return Ok(());
        }
        if let Some(client) = self.client_key_for(ev.window) {
            let Some(mut info) = self.clients.get(&client).copied() else {
                return Ok(());
            };
            if mask_has(ev.value_mask, ConfigWindow::X) {
                info.x = ev.x;
            }
            if mask_has(ev.value_mask, ConfigWindow::Y) {
                info.y = ev.y;
            }
            if mask_has(ev.value_mask, ConfigWindow::WIDTH) {
                info.width = ev.width.max(160);
            }
            if mask_has(ev.value_mask, ConfigWindow::HEIGHT) {
                info.height = ev.height.max(120);
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
                    .height(u32::from(info.height))
                    .border_width(0),
            )?;
            self.apply_frame_shape(&info)?;
            self.clients.insert(client, info);
            self.send_synthetic_configure(&info)?;
            self.redraw_frame_titlebar(client)?;
            return Ok(());
        }
        let aux = ConfigureWindowAux::from_configure_request(&ev);
        self.conn.configure_window(ev.window, &aux)?;
        Ok(())
    }

    fn manage_window(&mut self, window: Window) -> AnyResult<()> {
        if self.is_ui_window(window) || self.client_key_for(window).is_some() {
            return Ok(());
        }
        let attr = self.conn.get_window_attributes(window)?.reply()?;
        if attr.override_redirect {
            self.conn.map_window(window)?;
            return Ok(());
        }
        let was_mapped = attr.map_state != MapState::UNMAPPED;
        let geom = self.conn.get_geometry(window)?.reply()?;
        let class = self.window_class(window);
        let title = self.window_title(window);
        let is_ffplay = client_is_ffplay(&class, &title);
        let titlebar = !client_uses_own_chrome(&class, &title);
        let title_h = if titlebar { TITLEBAR_HEIGHT } else { 0 };
        let max_w = self.screen_width.saturating_sub(80).max(300);
        let max_h = self
            .screen_height
            .saturating_sub(TOPBAR_HEIGHT + DOCK_HEIGHT + title_h + 62)
            .max(240);
        let (x, y, width, height) = if is_ffplay {
            self.ffplay_geometry()
        } else {
            let width = geom.width.min(max_w);
            let height = geom.height.min(max_h);
            let x = if geom.x <= 0 { 42 } else { geom.x.max(16) };
            let y = if geom.y <= 0 {
                i16::try_from(TOPBAR_HEIGHT + 26).unwrap()
            } else {
                geom.y.max(i16::try_from(TOPBAR_HEIGHT + 8).unwrap())
            };
            (x, y, width, height)
        };
        let frame = self.conn.generate_id()?;
        let frame_aux = CreateWindowAux::new()
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::LEAVE_WINDOW
                    | EventMask::SUBSTRUCTURE_NOTIFY,
            )
            .cursor(self.cursor)
            .background_pixel(0)
            .bit_gravity(Gravity::NORTH_WEST)
            .backing_store(BackingStore::WHEN_MAPPED)
            .save_under(1);
        self.conn.create_window(
            self.depth,
            frame,
            self.root,
            x,
            y,
            width,
            height + title_h,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &frame_aux,
        )?;
        self.conn.change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new().event_mask(
                EventMask::STRUCTURE_NOTIFY | EventMask::BUTTON_MOTION | EventMask::BUTTON_RELEASE,
            ),
        )?;
        self.conn.change_save_set(SetMode::INSERT, window)?;
        self.conn.configure_window(
            window,
            &ConfigureWindowAux::new()
                .x(0)
                .y(i32::from(title_h))
                .width(u32::from(width))
                .height(u32::from(height))
                .border_width(0),
        )?;
        if was_mapped {
            // A mapped reparent reports both structure and substructure unmaps.
            self.ignored_unmaps.extend([window, window]);
        }
        self.conn
            .reparent_window(window, frame, 0, title_h as i16)?;
        self.conn.map_window(window)?;
        self.conn.map_window(frame)?;
        // Set EWMH _NET_WM_DESKTOP on the client window and its frame
        if let Ok(desktop_atom) = self.atom(b"_NET_WM_DESKTOP") {
            if let Ok(cardinal_atom) = self.atom(b"CARDINAL") {
                let _ = self.conn.change_property32(
                    PropMode::REPLACE,
                    window,
                    desktop_atom,
                    cardinal_atom,
                    &[self.active_workspace as u32],
                );
                let _ = self.conn.change_property32(
                    PropMode::REPLACE,
                    frame,
                    desktop_atom,
                    cardinal_atom,
                    &[self.active_workspace as u32],
                );
            }
        }

        let info = ClientInfo {
            window,
            frame,
            workspace: self.active_workspace,
            mapped: true,
            x,
            y,
            width,
            height,
            titlebar,
            saved: None,
        };
        self.apply_frame_shape(&info)?;
        self.clients.insert(window, info);
        self.send_synthetic_configure(&info)?;
        if is_ffplay {
            self.schedule_window_nudge(window, width, height);
        }
        self.redraw_frame_titlebar(window)?;
        self.focus_window(window)?;
        self.redraw_dock()?;
        Ok(())
    }

    fn remove_client(&mut self, window: Window) -> AnyResult<()> {
        let Some(client) = self.client_key_for(window) else {
            return Ok(());
        };
        let Some(info) = self.clients.remove(&client) else {
            return Ok(());
        };
        let _ = self.conn.change_save_set(SetMode::DELETE, info.window);
        let _ = self
            .conn
            .reparent_window(info.window, self.root, info.x, info.y);
        let _ = self.conn.destroy_window(info.frame);
        self.focus_history.retain(|&w| w != client);
        if self.active_client == Some(client) {
            self.active_client = None;
            self.update_active_window_property()?;
        }
        self.redraw_dock()?;
        Ok(())
    }

    fn minimize_client(&mut self, client: Window) -> AnyResult<()> {
        if let Some(info) = self.clients.get_mut(&client) {
            info.mapped = false;
            self.ignored_unmaps.push(info.frame);
            self.conn.unmap_window(info.frame)?;
            self.focus_history.retain(|&w| w != client);
            if self.active_client == Some(client) {
                self.active_client = None;
                self.update_active_window_property()?;
            }
            self.redraw_dock()?;
        }
        Ok(())
    }

    fn toggle_maximize_client(&mut self, client: Window) -> AnyResult<()> {
        let Some(mut info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        if let Some((x, y, w, h)) = info.saved.take() {
            info.x = x;
            info.y = y;
            info.width = w;
            info.height = h;
        } else {
            info.saved = Some((info.x, info.y, info.width, info.height));
            info.x = 8;
            info.y = TOPBAR_HEIGHT as i16 + 6;
            info.width = self.screen_width.saturating_sub(16);
            info.height = self
                .screen_height
                .saturating_sub(TOPBAR_HEIGHT + DOCK_HEIGHT + self.titlebar_height(&info) + 18);
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
        self.send_synthetic_configure(&info)?;
        self.redraw_frame_titlebar(client)?;
        self.focus_window(client)?;
        Ok(())
    }

    fn focus_window(&mut self, window: Window) -> AnyResult<()> {
        self.focus_window_at(window, CURRENT_TIME)
    }

    fn focus_window_at(&mut self, window: Window, time: Timestamp) -> AnyResult<()> {
        self.hide_dock_more_menu()?;
        let Some(client) = self.client_key_for(window) else {
            return Ok(());
        };
        let Some(info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        if info.workspace != self.active_workspace {
            return Ok(());
        }
        let previous_active = self.active_client;
        self.active_client = Some(client);
        self.focus_history.retain(|&w| w != client);
        self.focus_history.push(client);
        if !info.mapped {
            let mut mapped = info;
            mapped.mapped = true;
            self.clients.insert(client, mapped);
            self.conn.map_window(mapped.frame)?;
            self.redraw_dock()?;
        }
        self.conn
            .set_input_focus(InputFocus::POINTER_ROOT, info.window, time)?;
        self.send_take_focus(&info, time)?;
        self.settings_front = false;
        self.folder_front = false;
        self.media_front = false;
        self.conn.configure_window(
            info.frame,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?;
        if previous_active.is_some_and(|old| old != client) {
            if let Some(old) = previous_active {
                let _ = self.redraw_frame_titlebar(old);
            }
        }
        self.redraw_frame_titlebar(client)?;
        if previous_active != Some(client) {
            self.redraw_dock()?;
        }
        self.conn.configure_window(
            self.ui.dock,
            &ConfigureWindowAux::new()
                .sibling(info.frame)
                .stack_mode(StackMode::BELOW),
        )?;
        self.raise_chrome()?;
        self.update_active_window_property()?;
        Ok(())
    }

    fn update_active_window_property(&self) -> AnyResult<()> {
        let active_atom = self.atom(b"_NET_ACTIVE_WINDOW")?;
        let window_atom = self.atom(b"WINDOW")?;
        let active_val = self.active_client.unwrap_or(0);
        self.conn.change_property32(
            PropMode::REPLACE,
            self.root,
            active_atom,
            window_atom,
            &[active_val],
        )?;
        Ok(())
    }

    fn send_take_focus(&self, info: &ClientInfo, time: Timestamp) -> AnyResult<()> {
        let wm_protocols = self.atom(b"WM_PROTOCOLS")?;
        let wm_take_focus = self.atom(b"WM_TAKE_FOCUS")?;
        let Ok(reply) = self
            .conn
            .get_property(false, info.window, wm_protocols, AtomEnum::ATOM, 0, 32)?
            .reply()
        else {
            return Ok(());
        };
        let supports_take_focus = reply
            .value32()
            .is_some_and(|mut atoms| atoms.any(|atom| atom == wm_take_focus));
        if supports_take_focus {
            let event = ClientMessageEvent::new(
                32,
                info.window,
                wm_protocols,
                [wm_take_focus, time, 0, 0, 0],
            );
            self.conn
                .send_event(false, info.window, EventMask::NO_EVENT, event)?;
        }
        Ok(())
    }

    fn titlebar_height(&self, info: &ClientInfo) -> u16 {
        if info.titlebar { TITLEBAR_HEIGHT } else { 0 }
    }

    fn update_client_chrome(&mut self, client: Window) -> AnyResult<()> {
        let Some(mut info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        if !info.titlebar
            || !client_uses_own_chrome(&self.window_class(client), &self.window_title(client))
        {
            return Ok(());
        }
        info.titlebar = false;
        self.conn.configure_window(
            info.frame,
            &ConfigureWindowAux::new()
                .width(u32::from(info.width))
                .height(u32::from(info.height)),
        )?;
        self.conn.configure_window(
            info.window,
            &ConfigureWindowAux::new()
                .x(0)
                .y(0)
                .width(u32::from(info.width))
                .height(u32::from(info.height)),
        )?;
        self.apply_frame_shape(&info)?;
        self.clients.insert(client, info);
        self.send_synthetic_configure(&info)?;
        self.redraw_frame_titlebar(client)?;
        Ok(())
    }

    fn close_client(&self, client: Window) -> AnyResult<()> {
        if let Some(info) = self.clients.get(&client) {
            let wm_protocols = self.conn.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
            let wm_delete_window = self
                .conn
                .intern_atom(false, b"WM_DELETE_WINDOW")?
                .reply()?
                .atom;
            let event = ClientMessageEvent::new(
                32,
                info.window,
                wm_protocols,
                [wm_delete_window, CURRENT_TIME, 0, 0, 0],
            );
            self.conn
                .send_event(false, info.window, EventMask::NO_EVENT, event)?;
        }
        Ok(())
    }

    fn redraw_frame_titlebar(&self, client: Window) -> AnyResult<()> {
        let Some(info) = self.clients.get(&client) else {
            return Ok(());
        };
        if self.titlebar_height(info) == 0 {
            return Ok(());
        }
        let mut c = Canvas::from_wallpaper_crop(
            &self.wallpaper_pixels,
            self.screen_width,
            i32::from(info.x),
            i32::from(info.y),
            info.width,
            TITLEBAR_HEIGHT,
        );
        let active = self.active_client == Some(client);
        c.draw_rect(
            0,
            0,
            i32::from(info.width),
            i32::from(TITLEBAR_HEIGHT),
            if active {
                Color::rgba(221, 238, 252, 232)
            } else {
                Color::rgba(250, 254, 255, 225)
            },
        );
        c.draw_circle(19, 17, 8, Color::rgba(241, 96, 105, 235));
        c.draw_circle(42, 17, 8, Color::rgba(246, 190, 82, 235));
        c.draw_circle(65, 17, 8, Color::rgba(76, 197, 178, 235));
        if let Some((hover_client, button)) = self.title_hover {
            if hover_client == client {
                match button {
                    TitleButton::Close => {
                        c.draw_line(15, 13, 23, 21, 2, Color::rgba(80, 20, 25, 230));
                        c.draw_line(23, 13, 15, 21, 2, Color::rgba(80, 20, 25, 230));
                    }
                    TitleButton::Minimize => {
                        c.draw_line(37, 17, 47, 17, 2, Color::rgba(90, 60, 15, 235));
                    }
                    TitleButton::Maximize => {
                        c.draw_round_rect(60, 12, 10, 10, 2, Color::rgba(30, 90, 82, 225));
                        c.draw_round_rect(62, 14, 6, 6, 1, Color::rgba(250, 254, 255, 190));
                    }
                }
            }
        }
        let title = compact(
            &self.window_title(info.window),
            ((info.width / 9).max(8)) as usize,
        );
        c.draw_text(&self.bold, &title, 92, 9, 13.0, INK);
        self.upload_canvas(info.frame, &c)
    }

    fn apply_frame_shape(&self, info: &ClientInfo) -> AnyResult<()> {
        if !self.shape_supported {
            return Ok(());
        }

        let title_h = self.titlebar_height(info);
        let frame_h = info.height + title_h;
        let radius = if title_h > 0 { FRAME_CORNER_RADIUS } else { 0 };
        let rects = rounded_top_shape_rects(info.width, frame_h, radius);
        self.conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::BOUNDING,
            ClipOrdering::YX_BANDED,
            info.frame,
            0,
            0,
            &rects,
        )?;
        Ok(())
    }

    fn window_title(&self, window: Window) -> String {
        let Ok(cookie) =
            self.conn
                .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 96)
        else {
            return "Window".to_string();
        };
        let Ok(reply) = cookie.reply() else {
            return "Window".to_string();
        };
        let title = String::from_utf8_lossy(&reply.value).trim().to_string();
        if title.is_empty() {
            "Window".to_string()
        } else {
            title
        }
    }

    fn window_class(&self, window: Window) -> String {
        let Ok(cookie) =
            self.conn
                .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 128)
        else {
            return String::new();
        };
        let Ok(reply) = cookie.reply() else {
            return String::new();
        };
        String::from_utf8_lossy(&reply.value).to_ascii_lowercase()
    }

    fn redraw_everything(&mut self) -> AnyResult<()> {
        self.redraw_wallpaper()?;
        self.redraw_folder()?;
        if self.folder_terminal.visible {
            self.redraw_folder_terminal()?;
        }
        self.redraw_topbar()?;
        self.redraw_dock()?;
        if self.dock_more_visible {
            self.redraw_dock_more_menu()?;
        }
        self.redraw_settings()?;
        if self.app_menu_visible {
            self.redraw_app_menu()?;
        }
        for slot in 0..MEDIA_SLOT_COUNT {
            if self
                .media_slots
                .get(slot)
                .and_then(|m| m.as_ref())
                .is_some()
            {
                self.redraw_media_slot(slot)?;
            }
        }
        for client in self.clients.keys().copied().collect::<Vec<_>>() {
            self.redraw_frame_titlebar(client)?;
        }
        self.raise_ui()?;
        self.conn.flush()?;
        Ok(())
    }

    fn redraw_wallpaper(&mut self) -> AnyResult<()> {
        let pixmap = self.conn.generate_id()?;
        self.conn.create_pixmap(
            self.depth,
            pixmap,
            self.root,
            self.screen_width,
            self.screen_height,
        )?;
        let canvas = Canvas {
            width: self.screen_width,
            height: self.screen_height,
            data: self.wallpaper_pixels.clone(),
        };
        self.upload_canvas(pixmap, &canvas)?;
        self.conn.change_window_attributes(
            self.root,
            &ChangeWindowAttributesAux::new().background_pixmap(pixmap),
        )?;
        self.conn.clear_area(
            false,
            self.root,
            0,
            0,
            self.screen_width,
            self.screen_height,
        )?;
        self.install_pointer_cursor()?;
        if let Some(old) = self.wallpaper_pixmap.replace(pixmap) {
            let _ = self.conn.free_pixmap(old);
        }
        Ok(())
    }

    fn clear_root_region(&self, x: i32, y: i32, width: u32, height: u32) -> AnyResult<()> {
        let x0 = x.clamp(0, i32::from(self.screen_width));
        let y0 = y.clamp(0, i32::from(self.screen_height));
        let x1 = (x.saturating_add(width.min(i32::MAX as u32) as i32))
            .clamp(0, i32::from(self.screen_width));
        let y1 = (y.saturating_add(height.min(i32::MAX as u32) as i32))
            .clamp(0, i32::from(self.screen_height));
        let clear_w = x1.saturating_sub(x0);
        let clear_h = y1.saturating_sub(y0);
        if clear_w <= 0 || clear_h <= 0 {
            return Ok(());
        }
        let x = x0 as i16;
        let y = y0 as i16;
        let w = clear_w.min(i32::from(u16::MAX)) as u16;
        let h = clear_h.min(i32::from(u16::MAX)) as u16;
        if let Some(pixmap) = self.wallpaper_pixmap {
            self.conn
                .copy_area(pixmap, self.root, self.gc, x, y, x, y, w, h)?;
        } else {
            self.conn.clear_area(false, self.root, x, y, w, h)?;
        }
        Ok(())
    }

    fn topbar_controls(&self) -> TopbarControls {
        let battery = self.metrics.battery.as_deref().unwrap_or("100%");
        let battery_right = i32::from(self.screen_width) - 16;
        let battery_left = battery_right - measure_text(&self.bold, battery, 19.0) - 44;
        let network_x = battery_left - 22;
        let audio_x = network_x - TOPBAR_ICON_SPACING;
        let display_x = audio_x - TOPBAR_ICON_SPACING;
        let screenshot_x = display_x - TOPBAR_ICON_SPACING;
        let clipboard_x = screenshot_x - TOPBAR_ICON_SPACING;
        TopbarControls {
            clipboard_x,
            screenshot_x,
            display_x,
            audio_x,
            network_x,
            battery_left,
            battery_right,
        }
    }

    fn workspace_x(&self, index: usize) -> i32 {
        let brand_x = 24;
        let aurora_width = measure_text(&self.bold, "Aurora", 16.0);
        let start_x = brand_x + 23 + aurora_width + 24;
        start_x + index as i32 * WORKSPACE_STRIDE
    }

    fn add_workspace_x(&self) -> i32 {
        self.workspace_x(self.workspace_count)
    }

    fn redraw_topbar(&self) -> AnyResult<()> {
        let mut c = Canvas::from_wallpaper_crop(
            &self.wallpaper_pixels,
            self.screen_width,
            0,
            0,
            self.screen_width,
            TOPBAR_HEIGHT,
        );
        c.draw_rect(
            0,
            0,
            i32::from(c.width),
            i32::from(c.height),
            Color::rgba(23, 34, 42, 178),
        );

        // Draw Brand on the far left
        let brand_x = 24;
        c.draw_circle(brand_x, 20, 10, Color::rgba(160, 238, 220, 38));
        c.draw_circle(brand_x, 20, 7, MINT_LIGHT);
        c.draw_circle(brand_x - 2, 18, 2, Color::rgb(248, 255, 254));
        c.draw_text(
            &self.bold,
            "Aurora",
            brand_x + 23,
            11,
            16.0,
            Color::rgb(239, 252, 250),
        );

        // Draw Workspace icons to the right of the Brand
        for index in 0..self.workspace_count {
            draw_workspace_icon(
                &mut c,
                self.workspace_x(index),
                11,
                index == self.active_workspace,
            );
        }
        let add_x = self.add_workspace_x();
        draw_add_workspace_icon(&mut c, add_x, 20);

        let clock = self
            .topbar_notice
            .as_ref()
            .map(|(message, _)| message.clone())
            .unwrap_or_else(format_clock);
        c.draw_text_center(
            &self.regular,
            &clock,
            i32::from(self.screen_width) / 2,
            10,
            16.0,
            Color::rgb(239, 252, 250),
        );

        let controls = self.topbar_controls();
        draw_clipboard_icon(&mut c, controls.clipboard_x, 20, MINT_LIGHT);
        draw_screenshot_icon(&mut c, controls.screenshot_x, 20, MINT_LIGHT);
        draw_sidebar_display_icon(&mut c, controls.display_x, 20, MINT_LIGHT);
        draw_sidebar_audio_icon(&mut c, controls.audio_x, 20, MINT_LIGHT);
        draw_sidebar_network_icon(&mut c, controls.network_x, 20, MINT_LIGHT);
        let battery = self.metrics.battery.as_deref().unwrap_or("100%");
        c.draw_round_rect(
            controls.battery_left,
            7,
            controls.battery_right - controls.battery_left,
            26,
            9,
            if self.settings_visible && self.settings.tab == SettingsTab::Power {
                Color::rgba(116, 213, 198, 118)
            } else {
                Color::rgba(255, 255, 255, 42)
            },
        );
        draw_power_icon(&mut c, controls.battery_left + 14, 20, MINT_LIGHT);
        c.draw_text(
            &self.bold,
            battery,
            controls.battery_left + 30,
            9,
            19.0,
            Color::rgb(239, 252, 250),
        );
        self.upload_canvas(self.ui.topbar, &c)
    }

    fn redraw_clipboard_menu(&mut self) -> AnyResult<()> {
        let (_, _, w, h) = self.clipboard_menu_geometry();
        let mut c = Canvas::new(w, h, Color::rgb(247, 252, 255));
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            14,
            Color::rgb(247, 252, 255),
        );
        c.draw_text(&self.bold, "Clipboard", 18, 16, 15.0, INK);
        let page_count = self.clipboard_page_count();
        let page = self.clamped_clipboard_page();
        let has_prev = page > 0;
        let has_next = page + 1 < page_count;
        self.draw_clipboard_nav_button(&mut c, CLIPBOARD_MENU_PREV_X, "<", has_prev);
        self.draw_clipboard_nav_button(&mut c, CLIPBOARD_MENU_NEXT_X, ">", has_next);
        let page_label = if page_count == 0 {
            "0/0".to_string()
        } else {
            format!("{}/{}", page + 1, page_count)
        };
        c.draw_text_right(
            &self.bold,
            &page_label,
            i32::from(w) - 16,
            16,
            12.0,
            SOFT_INK,
        );
        if self.clipboard_history.is_empty() {
            draw_clipboard_icon(&mut c, i32::from(w) / 2, 78, MUTED);
            c.draw_text_center(
                &self.regular,
                "No clipboard history yet",
                i32::from(w) / 2,
                112,
                13.0,
                MUTED,
            );
            return self.upload_canvas(self.ui.clipboard_menu, &c);
        }

        let (start, end) = self.clipboard_page_range();
        let visible_entries = self.clipboard_history[start..end].to_vec();
        let mut row_y = 46;
        for entry in visible_entries.iter() {
            let row_h = clipboard_entry_row_height(entry);
            c.draw_round_rect(
                12,
                row_y - 8,
                i32::from(w) - 24,
                row_h - 8,
                9,
                Color::rgba(255, 255, 255, 150),
            );
            match &entry.item {
                ClipboardItem::Text(text) => {
                    draw_text_file_icon(&mut c, 34, row_y + 16, SOFT_INK);
                    let (line_one, line_two) = clipboard_text_preview_lines(text);
                    c.draw_text(
                        &self.regular,
                        &line_one,
                        58,
                        if line_two.is_some() {
                            row_y + 2
                        } else {
                            row_y + 10
                        },
                        13.0,
                        INK,
                    );
                    if let Some(line_two) = line_two {
                        c.draw_text(&self.regular, &line_two, 58, row_y + 21, 13.0, MUTED);
                    }
                }
                ClipboardItem::Image(path) => {
                    self.ensure_clipboard_image_preview(path);
                    draw_picture_icon(&mut c, 34, row_y + 18, MINT_DARK);
                    let info_x = 58;
                    let preview_x = i32::from(w) / 2;
                    let preview_y = row_y - 1;
                    let preview_w = (i32::from(w) - preview_x - 20)
                        .max(80)
                        .min(CLIPBOARD_MENU_IMAGE_PREVIEW_W);
                    let preview_h = (row_h - 14).min(CLIPBOARD_MENU_IMAGE_PREVIEW_H);
                    c.draw_round_rect(
                        preview_x,
                        preview_y,
                        preview_w,
                        preview_h,
                        8,
                        Color::rgba(255, 255, 255, 220),
                    );
                    if let Some(Some(preview)) = self.clipboard_image_previews.get(path) {
                        paint_cached_image_preview_left(
                            &mut c, preview, preview_x, preview_y, preview_w, preview_h,
                        );
                    }
                    c.draw_text(&self.bold, "Image", info_x, row_y, 13.0, INK);
                    let label = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("image");
                    c.draw_text(
                        &self.regular,
                        &compact(label, 22),
                        info_x,
                        row_y + 15,
                        10.5,
                        MUTED,
                    );
                    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                    c.draw_text(
                        &self.regular,
                        &format!("Size {}", format_size_mb(size)),
                        info_x,
                        row_y + 30,
                        10.5,
                        SOFT_INK,
                    );
                    let kind = clipboard_image_type_label(path);
                    let resolution = self
                        .clipboard_image_previews
                        .get(path)
                        .and_then(|preview| preview.as_ref())
                        .and_then(|preview| preview.resolution)
                        .map(|(iw, ih)| format!("{iw}x{ih}"))
                        .unwrap_or_else(|| "...".to_string());
                    c.draw_text(
                        &self.regular,
                        &format!("{kind}  {resolution}"),
                        info_x,
                        row_y + 45,
                        10.5,
                        SOFT_INK,
                    );
                }
            }
            row_y += row_h;
        }
        self.upload_canvas(self.ui.clipboard_menu, &c)
    }

    fn clipboard_page_count(&self) -> usize {
        self.clipboard_history
            .len()
            .div_ceil(CLIPBOARD_MENU_VISIBLE_ROWS)
    }

    fn clamped_clipboard_page(&self) -> usize {
        self.clipboard_history_page
            .min(self.clipboard_page_count().saturating_sub(1))
    }

    fn clipboard_page_range(&self) -> (usize, usize) {
        let start = self.clamped_clipboard_page() * CLIPBOARD_MENU_VISIBLE_ROWS;
        let end = (start + CLIPBOARD_MENU_VISIBLE_ROWS).min(self.clipboard_history.len());
        (start, end)
    }

    fn clipboard_page_content_height(&self) -> i32 {
        if self.clipboard_history.is_empty() {
            return 0;
        }
        let (start, end) = self.clipboard_page_range();
        self.clipboard_history[start..end]
            .iter()
            .map(clipboard_entry_row_height)
            .sum()
    }

    fn configure_clipboard_menu(&self) -> AnyResult<()> {
        let menu = self.clipboard_menu_geometry();
        self.conn.configure_window(
            self.ui.clipboard_menu,
            &ConfigureWindowAux::new()
                .x(i32::from(menu.0))
                .y(i32::from(menu.1))
                .width(u32::from(menu.2))
                .height(u32::from(menu.3)),
        )?;
        Ok(())
    }

    fn ensure_clipboard_image_preview(&mut self, path: &Path) {
        if self.clipboard_image_previews.contains_key(path)
            || self.clipboard_image_preview_pending.contains(path)
        {
            return;
        }
        let path = path.to_path_buf();
        self.clipboard_image_preview_pending.insert(path.clone());
        let tx = self.clipboard_image_preview_tx.clone();
        thread::spawn(move || {
            let preview = render_image_preview(
                &path,
                CLIPBOARD_MENU_IMAGE_PREVIEW_W,
                CLIPBOARD_MENU_IMAGE_PREVIEW_H,
            );
            let _ = tx.send(ClipboardImagePreviewResult { path, preview });
        });
    }

    fn poll_clipboard_image_previews(&mut self) -> AnyResult<bool> {
        let mut changed = false;
        loop {
            match self.clipboard_image_preview_rx.try_recv() {
                Ok(result) => {
                    self.clipboard_image_preview_pending.remove(&result.path);
                    self.clipboard_image_previews
                        .insert(result.path, result.preview);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if changed && self.clipboard_menu_visible {
            self.redraw_clipboard_menu()?;
        }
        Ok(changed)
    }

    fn draw_clipboard_nav_button(&self, c: &mut Canvas, x: i32, label: &str, enabled: bool) {
        c.draw_round_rect(
            x,
            CLIPBOARD_MENU_NAV_Y,
            CLIPBOARD_MENU_NAV_W,
            CLIPBOARD_MENU_NAV_H,
            8,
            if enabled {
                Color::rgba(225, 246, 241, 210)
            } else {
                Color::rgba(226, 234, 239, 130)
            },
        );
        c.draw_text_center(
            &self.bold,
            label,
            x + CLIPBOARD_MENU_NAV_W / 2,
            CLIPBOARD_MENU_NAV_Y + 5,
            18.0,
            if enabled { MINT_DARK } else { MUTED },
        );
    }

    fn redraw_dock(&mut self) -> AnyResult<()> {
        let task_windows = self.task_client_windows();
        if task_windows.len() <= 10 {
            let _ = self.hide_dock_more_menu();
        }

        let (x, y, w, h) = self.dock_geometry();
        self.conn.configure_window(
            self.ui.dock,
            &ConfigureWindowAux::new()
                .x(i32::from(x))
                .y(i32::from(y))
                .width(u32::from(w))
                .height(u32::from(h)),
        )?;
        let mut c = Canvas::from_wallpaper_crop(
            &self.wallpaper_pixels,
            self.screen_width,
            i32::from(x),
            i32::from(y),
            w,
            h,
        );

        let buttons = self.dock_button_count();
        let stride = 58;
        let total = buttons as i32 * stride - 12;
        let mut bx = (i32::from(w) - total) / 2;
        let cy = i32::from(h) / 2;
        for i in 0..buttons {
            let icon_x = bx + 7;
            let icon_y = cy - 22;
            c.draw_round_rect(
                icon_x + 3,
                icon_y + 6,
                42,
                42,
                14,
                Color::rgba(44, 77, 91, 38),
            );
            if i < 5 {
                c.draw_round_rect(icon_x, icon_y, 44, 44, 12, Color::rgba(255, 255, 255, 215));
                c.draw_round_rect(
                    icon_x + 1,
                    icon_y + 1,
                    42,
                    42,
                    11,
                    Color::rgba(196, 219, 229, 95),
                );
                draw_dock_icon(&mut c, i, icon_x + 22, icon_y + 22);
            } else if i == 15 && task_windows.len() > 10 {
                c.draw_round_rect(icon_x, icon_y, 44, 44, 12, Color::rgba(255, 255, 255, 215));
                c.draw_round_rect(
                    icon_x + 1,
                    icon_y + 1,
                    42,
                    42,
                    11,
                    Color::rgba(196, 219, 229, 95),
                );
                let dot_color = Color::rgba(44, 77, 91, 220);
                c.draw_circle(icon_x + 14, icon_y + 22, 3, dot_color);
                c.draw_circle(icon_x + 22, icon_y + 22, 3, dot_color);
                c.draw_circle(icon_x + 30, icon_y + 22, 3, dot_color);
            } else if let Some(client) = task_windows
                .get(i - 5)
                .and_then(|window| self.clients.get(window))
                .copied()
            {
                let active = self.active_client == Some(client.window);
                c.draw_round_rect(
                    icon_x,
                    icon_y,
                    44,
                    44,
                    12,
                    if active {
                        Color::rgba(172, 218, 255, 235)
                    } else {
                        Color::rgba(255, 255, 255, 235)
                    },
                );
                let title = self.window_title(client.window);
                if !self.paint_window_icon(&mut c, client.window, icon_x + 8, icon_y + 8, 28)
                    && !self.paint_desktop_icon(&mut c, client.window, icon_x + 8, icon_y + 8, 28)
                {
                    draw_client_task_icon(
                        &mut c,
                        &self.bold,
                        icon_x + 22,
                        icon_y + 22,
                        client.mapped,
                        &title,
                    );
                }
            }
            bx += stride;
        }
        self.upload_canvas(self.ui.dock, &c)
    }

    fn redraw_settings(&mut self) -> AnyResult<()> {
        if self.settings.tab == SettingsTab::Wallpaper {
            self.ensure_wallpaper_previews();
        }
        let (x, y, w, h) = self.settings_geometry();
        let mut c = Canvas::from_wallpaper_crop(
            &self.wallpaper_pixels,
            self.screen_width,
            i32::from(x),
            i32::from(y),
            w,
            h,
        );
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            18,
            Color::rgba(247, 252, 255, 226),
        );
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            18,
            Color::rgba(214, 229, 237, 70),
        );
        c.draw_rect(
            SIDEBAR_WIDTH,
            20,
            1,
            i32::from(h) - 40,
            Color::rgba(176, 198, 210, 100),
        );
        self.draw_settings_sidebar(&mut c);

        match self.settings.tab {
            SettingsTab::Display => self.draw_display_tab(&mut c),
            SettingsTab::Power => self.draw_power_tab(&mut c),
            SettingsTab::Wallpaper => self.draw_wallpaper_tab(&mut c),
            SettingsTab::Audio => self.draw_audio_tab(&mut c),
            SettingsTab::Network => self.draw_network_tab(&mut c),
            SettingsTab::Bluetooth => self.draw_bluetooth_tab(&mut c),
            SettingsTab::Startup => self.draw_startup_tab(&mut c),
            SettingsTab::Apps => self.draw_apps_tab(&mut c),
            SettingsTab::About => self.draw_about_tab(&mut c),
        }
        self.upload_canvas(self.ui.settings, &c)
    }

    fn redraw_folder(&self) -> AnyResult<()> {
        let (x, y, w, h) = self.folder_geometry();
        let mut c = Canvas::from_wallpaper_crop(
            &self.wallpaper_pixels,
            self.screen_width,
            i32::from(x),
            i32::from(y),
            w,
            h,
        );
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            18,
            Color::rgba(247, 252, 255, 212),
        );
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            18,
            Color::rgba(214, 229, 237, 70),
        );
        c.draw_round_rect(18, 18, 30, 30, 10, Color::rgba(255, 255, 255, 155));
        draw_home_icon(&mut c, 33, 33, MINT_DARK);
        c.draw_round_rect(
            56,
            18,
            FOLDER_HEADER_ICON,
            FOLDER_HEADER_ICON,
            10,
            Color::rgba(255, 255, 255, 155),
        );
        draw_terminal_icon(
            &mut c,
            71,
            33,
            if self.folder_terminal.visible {
                MINT_DARK
            } else {
                SOFT_INK
            },
        );
        c.draw_round_rect(
            94,
            18,
            FOLDER_HEADER_ICON,
            FOLDER_HEADER_ICON,
            10,
            Color::rgba(255, 255, 255, 155),
        );
        draw_sort_icon(&mut c, 109, 33, MINT_DARK);
        c.draw_text(
            &self.bold,
            &compact_path(&self.folder_path, 28),
            18,
            54,
            14.0,
            MINT_DARK,
        );
        c.draw_round_rect(
            i32::from(w) - 50,
            18,
            30,
            30,
            10,
            Color::rgba(255, 255, 255, 155),
        );
        draw_more_icon(&mut c, i32::from(w) - 35, 33, MINT_DARK);
        c.draw_rect(
            18,
            72,
            i32::from(w) - 36,
            1,
            Color::rgba(178, 202, 214, 110),
        );

        if self.folder_entries.is_empty() {
            c.draw_text(
                &self.regular,
                "No common media files here.",
                24,
                108,
                13.0,
                MUTED,
            );
        } else {
            let limit = self.folder_visible_row_count();
            for (idx, entry) in self
                .folder_entries
                .iter()
                .skip(self.folder_scroll)
                .take(limit)
                .enumerate()
            {
                let row_y = 90 + idx as i32 * 42;
                let selected = self.folder_selected.as_ref() == Some(&entry.path);
                c.draw_round_rect(
                    16,
                    row_y - 4,
                    i32::from(w) - 32,
                    34,
                    9,
                    if selected {
                        Color::rgba(116, 213, 198, 118)
                    } else {
                        Color::rgba(255, 255, 255, 118)
                    },
                );
                draw_file_kind_icon(&mut c, entry.kind, 35, row_y + 13);
                c.draw_text(&self.bold, &compact(&entry.name, 28), 58, row_y, 13.0, INK);
                c.draw_text(
                    &self.regular,
                    file_kind_label(entry.kind),
                    58,
                    row_y + 18,
                    10.0,
                    MUTED,
                );
            }
        }

        if self.folder_more_open {
            let menu_x = i32::from(w) - 214;
            let menu_y = 54;
            let menu_h = 44 + self.folder_places.len().min(6) as i32 * 28;
            c.draw_round_rect(
                menu_x,
                menu_y,
                194,
                menu_h,
                12,
                Color::rgba(250, 254, 255, 238),
            );
            c.draw_text(&self.bold, "Places", menu_x + 14, menu_y + 12, 14.0, INK);
            for (idx, place) in self.folder_places.iter().take(6).enumerate() {
                let y = menu_y + 40 + idx as i32 * 28;
                c.draw_round_rect(
                    menu_x + 8,
                    y - 5,
                    178,
                    23,
                    7,
                    Color::rgba(234, 246, 249, 130),
                );
                draw_folder_icon(&mut c, menu_x + 22, y + 6, MINT_DARK);
                c.draw_text(
                    &self.regular,
                    &compact(&place.name, 20),
                    menu_x + 42,
                    y,
                    11.0,
                    INK,
                );
            }
        }
        if self.folder_sort_open {
            let menu_x = 94;
            let menu_y = 54;
            c.draw_round_rect(menu_x, menu_y, 122, 96, 12, Color::rgba(250, 254, 255, 242));
            for (idx, sort) in [FolderSort::Name, FolderSort::Date, FolderSort::Size]
                .iter()
                .copied()
                .enumerate()
            {
                let y = menu_y + 16 + idx as i32 * 28;
                if sort == self.folder_sort {
                    c.draw_round_rect(
                        menu_x + 8,
                        y - 5,
                        106,
                        23,
                        7,
                        Color::rgba(116, 213, 198, 92),
                    );
                }
                c.draw_text(&self.regular, sort.label(), menu_x + 18, y, 12.0, INK);
            }
        }
        if self.folder_context_open {
            let menu_x = self.folder_context_pos.0.min(i32::from(w) - 166).max(10);
            let menu_y = self.folder_context_pos.1.min(i32::from(h) - 178).max(78);
            let items = ["Open other app", "Copy", "Cut", "Paste", "Info"];
            c.draw_round_rect(
                menu_x,
                menu_y,
                156,
                164,
                10,
                Color::rgba(250, 254, 255, 242),
            );
            for (idx, item) in items.iter().enumerate() {
                let y = menu_y + 14 + idx as i32 * 29;
                c.draw_text(&self.regular, item, menu_x + 14, y, 12.0, INK);
            }
        }
        if let Some(info) = self.folder_info.as_ref() {
            c.draw_text(
                &self.regular,
                &compact(info, 46),
                28,
                i32::from(h) - 24,
                12.0,
                MUTED,
            );
        }
        let displayed_count = self.folder_visible_row_count();
        if self.folder_entries.len() > displayed_count {
            let track_h = i32::from(h) - 100 - if self.choose_file_mode { 42 } else { 0 };
            let track_x = i32::from(w) - 13;
            c.draw_round_rect(track_x, 84, 5, track_h, 3, Color::rgba(176, 198, 210, 90));
            let thumb_h = ((track_h as f32 * displayed_count as f32
                / self.folder_entries.len() as f32) as i32)
                .max(34)
                .min(track_h);
            let max_scroll = self
                .folder_entries
                .len()
                .saturating_sub(displayed_count)
                .max(1);
            let thumb_y = 84
                + ((track_h - thumb_h) as f32 * self.folder_scroll.min(max_scroll) as f32
                    / max_scroll as f32) as i32;
            c.draw_round_rect(
                track_x,
                thumb_y,
                5,
                thumb_h,
                3,
                Color::rgba(29, 145, 137, 180),
            );
        }

        if self.choose_file_mode {
            let cancel_x = i32::from(w) - 190;
            let choose_x = i32::from(w) - 100;
            let btn_y = i32::from(h) - 46;

            c.draw_round_rect(cancel_x, btn_y, 80, 32, 8, Color::rgba(241, 126, 135, 150));
            c.draw_text_center(
                &self.bold,
                "Cancel",
                cancel_x + 40,
                btn_y + 20,
                12.0,
                Color::rgb(160, 58, 68),
            );

            c.draw_round_rect(choose_x, btn_y, 80, 32, 8, Color::rgba(160, 238, 220, 200));
            c.draw_text_center(
                &self.bold,
                "Open",
                choose_x + 40,
                btn_y + 20,
                12.0,
                MINT_DARK,
            );

            if let Some(selected) = self.folder_selected.as_ref() {
                let name = selected.file_name().unwrap_or_default().to_string_lossy();
                c.draw_text(
                    &self.regular,
                    &compact(&format!("File: {name}"), 24),
                    24,
                    btn_y + 20,
                    12.0,
                    INK,
                );
            } else {
                c.draw_text(&self.regular, "Select a file", 24, btn_y + 20, 12.0, MUTED);
            }
        }

        self.upload_canvas(self.ui.folder, &c)
    }

    fn cancel_choose_file(&mut self) -> AnyResult<()> {
        self.choose_file_mode = false;
        let result_atom = self
            .conn
            .intern_atom(false, b"_AURORA_CHOOSE_FILE_RESULT")?
            .reply()?
            .atom;
        let string_atom = self.conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        self.conn.change_property8(
            PropMode::REPLACE,
            self.root,
            result_atom,
            string_atom,
            b"CANCEL",
        )?;
        self.conn.unmap_window(self.ui.folder)?;
        if self.folder_terminal.visible {
            self.conn.unmap_window(self.ui.folder_terminal)?;
        }
        self.redraw_folder()?;
        self.conn.flush()?;
        Ok(())
    }

    fn submit_choose_file(&mut self) -> AnyResult<()> {
        let Some(path) = self.folder_selected.clone() else {
            return Ok(());
        };
        self.choose_file_mode = false;
        let result_atom = self
            .conn
            .intern_atom(false, b"_AURORA_CHOOSE_FILE_RESULT")?
            .reply()?
            .atom;
        let string_atom = self.conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let path_str = path.to_string_lossy();
        self.conn.change_property8(
            PropMode::REPLACE,
            self.root,
            result_atom,
            string_atom,
            path_str.as_bytes(),
        )?;
        self.conn.unmap_window(self.ui.folder)?;
        if self.folder_terminal.visible {
            self.conn.unmap_window(self.ui.folder_terminal)?;
        }
        self.redraw_folder()?;
        self.conn.flush()?;
        Ok(())
    }

    fn redraw_app_menu(&self) -> AnyResult<()> {
        let (x, y, w, h) = self.app_menu_geometry();
        let mut c = Canvas::from_wallpaper_crop(
            &self.wallpaper_pixels,
            self.screen_width,
            i32::from(x),
            i32::from(y),
            w,
            h,
        );
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            16,
            Color::rgba(248, 253, 255, 232),
        );
        c.draw_text(&self.bold, "Apps", 20, 18, 20.0, INK);
        let apps = app_menu_items();
        for (idx, app) in apps.iter().enumerate() {
            let row_y = 58 + idx as i32 * 42;
            c.draw_round_rect(
                14,
                row_y - 5,
                i32::from(w) - 28,
                34,
                9,
                Color::rgba(255, 255, 255, 120),
            );
            draw_launcher_icon(&mut c, idx, 34, row_y + 12);
            c.draw_text(&self.bold, app.label, 58, row_y, 13.0, INK);
            c.draw_text(&self.regular, app.hint, 58, row_y + 17, 10.0, MUTED);
        }
        if self.app_menu_more {
            let x0 = 280;
            c.draw_rect(
                x0 - 12,
                20,
                1,
                i32::from(h) - 40,
                Color::rgba(176, 198, 210, 100),
            );
            c.draw_text(&self.bold, "Desktop Apps", x0, 18, 18.0, INK);
            let entries = read_desktop_entries();
            let visible = 15usize;
            let start = self.app_menu_scroll.min(entries.len().saturating_sub(1));
            let mut y = 56;
            let mut current = String::new();
            for entry in entries.iter().skip(start).take(visible) {
                if entry.category != current {
                    current = entry.category.clone();
                    c.draw_text(&self.bold, &current, x0, y, 12.0, MINT_DARK);
                    y += 22;
                }
                c.draw_text(
                    &self.regular,
                    &compact(&entry.name, 30),
                    x0 + 14,
                    y,
                    12.0,
                    INK,
                );
                y += 24;
            }
            if entries.len() > visible {
                let track_x = i32::from(w) - 22;
                let track_h = i32::from(h) - 86;
                c.draw_round_rect(track_x, 54, 6, track_h, 3, Color::rgba(176, 198, 210, 95));
                let thumb_h = ((track_h as f32 * visible as f32 / entries.len() as f32) as i32)
                    .max(34)
                    .min(track_h);
                let max_scroll = entries.len().saturating_sub(visible).max(1);
                let thumb_y = 54
                    + ((track_h - thumb_h) as f32 * self.app_menu_scroll.min(max_scroll) as f32
                        / max_scroll as f32) as i32;
                c.draw_round_rect(
                    track_x,
                    thumb_y,
                    6,
                    thumb_h,
                    3,
                    Color::rgba(29, 145, 137, 185),
                );
            }
        }
        self.upload_canvas(self.ui.app_menu, &c)
    }

    fn redraw_aurora_menu(&self) -> AnyResult<()> {
        let (x, y, w, h) = self.aurora_menu_geometry();
        self.conn.configure_window(
            self.ui.aurora_menu,
            &ConfigureWindowAux::new()
                .x(i32::from(x))
                .y(i32::from(y))
                .width(u32::from(w))
                .height(u32::from(h)),
        )?;
        let mut c = Canvas::from_wallpaper_crop(
            &self.wallpaper_pixels,
            self.screen_width,
            i32::from(x),
            i32::from(y),
            w,
            h,
        );
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            12,
            Color::rgba(248, 253, 255, 235),
        );
        c.draw_round_rect(
            1,
            1,
            i32::from(w) - 2,
            i32::from(h) - 2,
            11,
            Color::rgba(210, 229, 238, 130),
        );
        c.draw_circle(28, 28, 12, Color::rgba(116, 213, 198, 170));
        c.draw_circle(28, 28, 6, MINT_DARK);
        c.draw_text(&self.bold, "Aurora WM", 50, 17, 17.0, INK);
        c.draw_text(
            &self.regular,
            env!("CARGO_PKG_VERSION"),
            146,
            21,
            11.0,
            MUTED,
        );

        if self.aurora_menu_about {
            c.draw_text(&self.bold, "About", 18, 62, 14.0, INK);
            c.draw_text(
                &self.regular,
                "A small X11 desktop shell with dock, folders, media viewer,",
                18,
                84,
                11.0,
                INK,
            );
            c.draw_text(
                &self.regular,
                "settings, screenshots, and lightweight window controls.",
                18,
                100,
                11.0,
                INK,
            );
            c.draw_text(
                &self.bold,
                &format!("Display: {}", self.display),
                18,
                118,
                11.0,
                MINT_DARK,
            );
            c.draw_text(&self.bold, "Help", 18, 142, 13.0, INK);
            c.draw_text(
                &self.regular,
                "Alt+Tab switches running apps.",
                18,
                162,
                11.0,
                MUTED,
            );
            c.draw_text(
                &self.regular,
                "Use the dock for apps and settings; drag titlebars to move windows.",
                18,
                180,
                11.0,
                MUTED,
            );
            c.draw_text(
                &self.regular,
                "Bottom corners resize windows; settings are saved in ~/.config/aurora-wm.",
                18,
                198,
                11.0,
                MUTED,
            );
            c.draw_round_rect(16, 230, 76, 28, 8, Color::rgba(234, 244, 248, 220));
            c.draw_text_center(&self.bold, "Back", 54, 238, 12.0, MINT_DARK);
        } else {
            // Draw Restart WM
            {
                let row_y = 64;
                c.draw_round_rect(
                    14,
                    row_y - 8,
                    i32::from(w) - 28,
                    38,
                    9,
                    Color::rgba(255, 255, 255, 150),
                );
                draw_reload_menu_icon(&mut c, 32, row_y + 11, MINT_DARK);
                c.draw_text(&self.bold, "Restart WM", 50, row_y, 13.0, INK);
                c.draw_text(
                    &self.regular,
                    "Reload Aurora and keep saved settings",
                    50,
                    row_y + 17,
                    10.0,
                    MUTED,
                );
            }

            let mut next_row_y = 114;

            if self.aurora_menu_restart_confirm {
                c.draw_round_rect(
                    14,
                    102,
                    i32::from(w) - 28,
                    46,
                    9,
                    Color::rgba(238, 245, 248, 220),
                );
                c.draw_text(&self.bold, "Confirm?", 28, 118, 12.0, INK);

                // Yes button
                c.draw_round_rect(160, 110, 90, 28, 6, Color::rgba(232, 74, 95, 210));
                c.draw_text_center(&self.bold, "Yes", 205, 118, 12.0, Color::rgb(255, 255, 255));

                // No button
                c.draw_round_rect(270, 110, 90, 28, 6, Color::rgba(200, 215, 225, 180));
                c.draw_text_center(&self.bold, "No", 315, 118, 12.0, INK);

                next_row_y = 166;
            }

            // Draw About Aurora
            {
                let row_y = next_row_y;
                c.draw_round_rect(
                    14,
                    row_y - 8,
                    i32::from(w) - 28,
                    38,
                    9,
                    Color::rgba(255, 255, 255, 150),
                );
                draw_info_menu_icon(&mut c, 32, row_y + 11, MINT_LIGHT);
                c.draw_text(&self.bold, "About Aurora", 50, row_y, 13.0, INK);
                c.draw_text(
                    &self.regular,
                    "Version, description, and quick help",
                    50,
                    row_y + 17,
                    10.0,
                    MUTED,
                );
            }
        }
        self.upload_canvas(self.ui.aurora_menu, &c)
    }

    fn redraw_media_slot(&self, slot: usize) -> AnyResult<()> {
        let Some(media) = self.media_slots.get(slot).and_then(|m| m.as_ref()) else {
            return Ok(());
        };
        let (_, _, w, h) = self.media_geometry(slot);
        let mut c = Canvas::new(w, h, Color::rgb(247, 252, 255));
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            18,
            Color::rgb(247, 252, 255),
        );
        c.draw_text(
            &self.bold,
            &compact(&media.entry.name, 30),
            24,
            20,
            16.0,
            INK,
        );
        c.draw_text(
            &self.regular,
            file_kind_label(media.entry.kind),
            24,
            44,
            11.0,
            MUTED,
        );
        if media.entry.kind == FileKind::Text {
            let button_x = i32::from(w) - 78;
            c.draw_round_rect(button_x, 17, 28, 24, 8, Color::rgba(116, 213, 198, 110));
            if media.editing {
                draw_save_icon(&mut c, button_x + 14, 29, MINT_DARK);
            } else {
                draw_edit_icon(&mut c, button_x + 14, 29, MINT_DARK);
            }
        }
        c.draw_round_rect(
            i32::from(w) - 43,
            17,
            24,
            24,
            8,
            Color::rgba(241, 126, 135, 120),
        );
        c.draw_line(
            i32::from(w) - 37,
            23,
            i32::from(w) - 25,
            35,
            2,
            Color::rgb(255, 255, 255),
        );
        c.draw_line(
            i32::from(w) - 25,
            23,
            i32::from(w) - 37,
            35,
            2,
            Color::rgb(255, 255, 255),
        );

        let preview_x = 18;
        let preview_y = 58;
        let preview_w = i32::from(w) - 48;
        let preview_h = i32::from(h) - 130;
        let preview_bg = if media.entry.kind == FileKind::Text {
            Color::rgb(255, 255, 255)
        } else {
            Color::rgb(238, 247, 252)
        };
        c.draw_round_rect(preview_x, preview_y, preview_w, preview_h, 15, preview_bg);
        match media.entry.kind {
            FileKind::Text => {
                self.draw_text_viewer(
                    &mut c, slot, media, preview_x, preview_y, preview_w, preview_h,
                );
            }
            FileKind::Image => {
                let image_area_h = (preview_h - 34).max(80);
                if let Some(preview) = media.image_preview.as_ref() {
                    paint_cached_image_preview(
                        &mut c,
                        preview,
                        preview_x + 8,
                        preview_y + 8,
                        preview_w - 16,
                        image_area_h - 10,
                    );
                } else {
                    paint_file_preview(
                        &mut c,
                        &media.entry.path,
                        preview_x + 8,
                        preview_y + 8,
                        preview_w - 16,
                        image_area_h - 10,
                    );
                }
                c.draw_text(
                    &self.regular,
                    &image_info_line(
                        &media.entry.path,
                        media
                            .image_preview
                            .as_ref()
                            .and_then(|preview| preview.resolution),
                    ),
                    preview_x + 12,
                    preview_y + preview_h - 24,
                    11.0,
                    MUTED,
                );
                c.draw_round_rect(
                    preview_x,
                    preview_y,
                    preview_w,
                    preview_h,
                    15,
                    Color::rgba(255, 255, 255, 32),
                );
            }
            FileKind::Audio => {
                draw_music_icon(&mut c, i32::from(w) / 2, preview_y + 94, MINT_DARK);
                draw_sparkline(
                    &mut c,
                    preview_x + 44,
                    preview_y + 152,
                    preview_w - 88,
                    42,
                    4200.0,
                    MINT_DARK,
                );
                c.draw_text_center(
                    &self.bold,
                    if media.playing {
                        "Playing audio"
                    } else {
                        "Audio ready"
                    },
                    i32::from(w) / 2,
                    preview_y + 30,
                    18.0,
                    INK,
                );
            }
            FileKind::Video => {
                let frame_color = if media.playing {
                    Color::rgba(73, 156, 231, 80)
                } else {
                    Color::rgba(23, 34, 42, 38)
                };
                c.draw_round_rect(
                    preview_x + 18,
                    preview_y + 14,
                    preview_w - 36,
                    preview_h - 42,
                    10,
                    frame_color,
                );
                if paint_video_frame_preview(
                    &mut c,
                    &media.entry.path,
                    preview_x + 18,
                    preview_y + 14,
                    preview_w - 36,
                    preview_h - 42,
                )
                .is_none()
                {
                    draw_play_icon(&mut c, i32::from(w) / 2, preview_y + 88, BLUE);
                }
                c.draw_text_center(
                    &self.bold,
                    if media.playing {
                        "Playing video"
                    } else {
                        "Video ready"
                    },
                    i32::from(w) / 2,
                    preview_y + 30,
                    14.0,
                    INK,
                );
                c.draw_round_rect(
                    preview_x + 34,
                    preview_y + 122,
                    preview_w - 68,
                    10,
                    5,
                    Color::rgba(255, 255, 255, 120),
                );
                c.draw_round_rect(
                    preview_x + 34,
                    preview_y + 122,
                    ((preview_w - 68) as f32 * media.progress.clamp(0.0, 1.0)) as i32,
                    10,
                    5,
                    Color::rgba(73, 156, 231, 150),
                );
            }
            FileKind::Directory | FileKind::Other => {
                self.draw_unknown_file_view(
                    &mut c, media, preview_x, preview_y, preview_w, preview_h,
                );
            }
        }

        let controls_y = i32::from(h) - 62;
        c.draw_text(
            &self.regular,
            &compact_path(&media.entry.path, 42),
            24,
            controls_y - 24,
            11.0,
            MUTED,
        );
        let status = media.notice.clone().unwrap_or_else(|| viewer_status(media));
        c.draw_text_right(
            &self.regular,
            &status,
            i32::from(w) - 24,
            controls_y - 24,
            11.0,
            if media.notice.is_some() {
                MINT_DARK
            } else {
                MUTED
            },
        );
        if matches!(media.entry.kind, FileKind::Audio | FileKind::Video) {
            c.draw_round_rect(
                24,
                controls_y,
                i32::from(w) - 48,
                42,
                13,
                Color::rgba(116, 213, 198, 88),
            );
            if media.playing {
                c.draw_rect(44, controls_y + 12, 5, 18, MINT_DARK);
                c.draw_rect(54, controls_y + 12, 5, 18, MINT_DARK);
                c.draw_text(&self.bold, "Pause", 80, controls_y + 11, 13.0, INK);
            } else {
                draw_play_icon(&mut c, 50, controls_y + 21, MINT_DARK);
                c.draw_text(&self.bold, "Play", 80, controls_y + 11, 13.0, INK);
            }
            let bar_x = 150;
            let bar_w = i32::from(w) - bar_x - 48;
            c.draw_round_rect(
                bar_x,
                controls_y + 17,
                bar_w,
                8,
                4,
                Color::rgba(255, 255, 255, 140),
            );
            c.draw_round_rect(
                bar_x,
                controls_y + 17,
                (bar_w as f32 * media.progress.clamp(0.0, 1.0)) as i32,
                8,
                4,
                Color::rgba(29, 145, 137, 190),
            );
        }
        if self
            .media_context_open
            .is_some_and(|(ctx_slot, _, _)| ctx_slot == slot)
        {
            self.draw_media_context_menu(&mut c, slot, media);
        }
        if self.media_trash_prompt == Some(slot) {
            self.draw_media_trash_prompt(&mut c, media, i32::from(w), i32::from(h));
        }
        self.upload_canvas(self.ui.media[slot], &c)
    }

    fn draw_media_context_menu(&self, c: &mut Canvas, slot: usize, media: &MediaState) {
        let Some((_, x, y)) = self
            .media_context_open
            .filter(|(ctx_slot, _, _)| *ctx_slot == slot)
        else {
            return;
        };
        let (_, _, w, h) = self.media_geometry(slot);
        let menu_x = x.min(i32::from(w) - 184).max(12);
        let menu_y = y.min(i32::from(h) - 112).max(50);
        let items = if media.entry.kind == FileKind::Image {
            ["Rename", "Copy image", "Move to Trash"]
        } else {
            ["Rename", "Copy path", "Move to Trash"]
        };
        c.draw_round_rect(menu_x, menu_y, 172, 96, 10, Color::rgba(250, 254, 255, 244));
        for (idx, item) in items.iter().enumerate() {
            c.draw_text(
                &self.regular,
                item,
                menu_x + 14,
                menu_y + 16 + idx as i32 * 29,
                12.0,
                INK,
            );
        }
    }

    fn draw_media_trash_prompt(&self, c: &mut Canvas, media: &MediaState, w: i32, h: i32) {
        let box_w = 310;
        let box_h = 126;
        let x = (w - box_w) / 2;
        let y = (h - box_h) / 2;
        c.draw_round_rect(x, y, box_w, box_h, 14, Color::rgba(250, 254, 255, 246));
        c.draw_text(&self.bold, "Move to Trash?", x + 20, y + 18, 16.0, INK);
        c.draw_text(
            &self.regular,
            &compact(&media.entry.name, 34),
            x + 20,
            y + 46,
            12.0,
            MUTED,
        );
        c.draw_round_rect(x + 48, y + 82, 84, 30, 9, Color::rgba(241, 126, 135, 105));
        c.draw_text_center(&self.bold, "Yes", x + 90, y + 90, 12.0, INK);
        c.draw_round_rect(x + 174, y + 82, 84, 30, 9, Color::rgba(178, 202, 214, 110));
        c.draw_text_center(&self.bold, "No", x + 216, y + 90, 12.0, INK);
    }

    fn draw_text_viewer(
        &self,
        c: &mut Canvas,
        slot: usize,
        media: &MediaState,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) {
        let line_h = 19;
        let max_lines = ((h - 20) / line_h).max(1) as usize;
        let start = media
            .text_scroll
            .min(media.text_lines.len().saturating_sub(1));
        let gutter_w = 34;
        let text_x = x + gutter_w + 8;
        c.draw_rect(
            x + gutter_w,
            y + 8,
            1,
            h - 16,
            Color::rgba(178, 202, 214, 90),
        );
        for (idx, line) in media
            .text_lines
            .iter()
            .skip(start)
            .take(max_lines)
            .enumerate()
        {
            let yy = y + 12 + idx as i32 * line_h;
            c.draw_text_right(
                &self.regular,
                &(start + idx + 1).to_string(),
                x + gutter_w - 8,
                yy,
                11.0,
                MUTED,
            );
            let shown = compact(line, ((w - gutter_w - 18) / 7).max(20) as usize);
            if let Some((sel_start, sel_end)) = self
                .media_text_selection
                .as_ref()
                .filter(|selection| selection.slot == slot)
                .map(normalized_media_selection)
            {
                let line_no = start + idx;
                if line_no >= sel_start.0 && line_no <= sel_end.0 {
                    let line_len = line.chars().count();
                    let start_col = if line_no == sel_start.0 {
                        sel_start.1.min(line_len)
                    } else {
                        0
                    };
                    let end_col = if line_no == sel_end.0 {
                        sel_end.1.min(line_len)
                    } else {
                        line_len
                    };
                    if end_col > start_col {
                        let sx =
                            text_x + fast_text_width_cols(&self.regular, line, 0, start_col, 13.0);
                        let sw =
                            fast_text_width_cols(&self.regular, line, start_col, end_col, 13.0)
                                .max(3);
                        c.draw_round_rect(sx, yy + 1, sw, 16, 4, Color::rgba(73, 156, 231, 70));
                    }
                }
            }
            c.draw_text(&self.regular, &shown, text_x, yy, 13.0, INK);
        }
        if media.editing {
            let cursor_line = media
                .text_cursor_line
                .min(media.text_lines.len().saturating_sub(1));
            if cursor_line >= start && cursor_line < start + max_lines {
                let visible_idx = cursor_line - start;
                let line = media
                    .text_lines
                    .get(cursor_line)
                    .map(String::as_str)
                    .unwrap_or("");
                let cursor_x = text_x
                    + fast_text_width_cols(
                        &self.regular,
                        line,
                        0,
                        media.text_cursor_col.min(line.chars().count()),
                        13.0,
                    );
                let cursor_y = y + 13 + visible_idx as i32 * line_h;
                c.draw_rect(cursor_x, cursor_y, 2, 15, MINT_DARK);
            }
        }
        if self
            .media_text_selection
            .as_ref()
            .filter(|selection| selection.slot == slot)
            .is_some_and(|selection| {
                !selected_text_from_lines(&media.text_lines, selection).is_empty()
            })
        {
            let (bx, by, bw, bh) = media_text_copy_button_rect(x, y, w, h);
            c.draw_round_rect(bx, by, bw, bh, 9, Color::rgba(116, 213, 198, 150));
            draw_copy_icon(c, bx + bw / 2, by + bh / 2, MINT_DARK);
        }
    }

    fn draw_unknown_file_view(
        &self,
        c: &mut Canvas,
        media: &MediaState,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) {
        draw_file_kind_icon(c, media.entry.kind, x + w / 2, y + 58);
        let meta = fs::metadata(&media.entry.path).ok();
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| format!("modified {}s", d.as_secs()))
            .unwrap_or_else(|| "modified unknown".to_string());
        let kind = media.file_info.as_deref().unwrap_or("Unknown file type");
        let size_line = format_size_mb(size);
        let lines = [
            media.entry.name.as_str(),
            size_line.as_str(),
            modified.as_str(),
            kind,
        ];
        for (idx, line) in lines.iter().enumerate() {
            c.draw_text_center(
                &self.regular,
                &compact(line, 54),
                x + w / 2,
                y + 112 + idx as i32 * 24,
                if idx == 0 { 15.0 } else { 12.0 },
                if idx == 0 { INK } else { MUTED },
            );
        }
        c.draw_round_rect(
            x + 40,
            y + h - 56,
            w - 80,
            34,
            10,
            Color::rgba(116, 213, 198, 90),
        );
        c.draw_text_center(
            &self.bold,
            "Open as text",
            x + w / 2,
            y + h - 47,
            13.0,
            MINT_DARK,
        );
    }

    fn draw_settings_sidebar(&self, c: &mut Canvas) {
        let items = [
            SettingsTab::Display,
            SettingsTab::Power,
            SettingsTab::Wallpaper,
            SettingsTab::Audio,
            SettingsTab::Network,
            SettingsTab::Bluetooth,
            SettingsTab::Startup,
            SettingsTab::Apps,
            SettingsTab::About,
        ];
        for (idx, tab) in items.iter().enumerate() {
            let y = SETTINGS_SIDEBAR_TOP + idx as i32 * 48;
            let active = *tab == self.settings.tab;
            if active {
                c.draw_round_rect(
                    13,
                    y - 4,
                    SIDEBAR_WIDTH - 26,
                    35,
                    10,
                    Color::rgba(119, 215, 198, 92),
                );
            }
            draw_sidebar_icon(c, idx, 28, y + 12, if active { MINT_DARK } else { MUTED });
        }
    }

    fn draw_display_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        c.draw_text(&self.bold, "Display", sx, 22, 24.0, INK);
        c.draw_text(
            &self.regular,
            "Resolution, refresh rate, and idle sleep.",
            sx,
            54,
            13.0,
            MUTED,
        );

        draw_card(c, sx, 86, i32::from(c.width) - sx - 24, 156);
        c.draw_text(&self.bold, "Resolution", sx + 16, 104, 15.0, INK);
        let modes = self.display_modes.iter().take(4).collect::<Vec<_>>();
        for (idx, mode) in modes.iter().enumerate() {
            let y = 132 + idx as i32 * 25;
            let selected = idx == self.settings.selected_mode;
            c.draw_round_rect(
                sx + 14,
                y - 4,
                i32::from(c.width) - sx - 52,
                22,
                8,
                if selected {
                    Color::rgba(116, 213, 198, 90)
                } else {
                    Color::rgba(255, 255, 255, 120)
                },
            );
            c.draw_text(
                &self.regular,
                &mode.label(),
                sx + 24,
                y,
                12.0,
                if selected { MINT_DARK } else { INK },
            );
            if mode.current {
                c.draw_text_right(
                    &self.regular,
                    "current",
                    i32::from(c.width) - 38,
                    y,
                    11.0,
                    MINT_DARK,
                );
            }
        }

        draw_card(c, sx, 258, i32::from(c.width) - sx - 24, 86);
        c.draw_text(&self.bold, "Refresh rate", sx + 16, 276, 15.0, INK);
        let refresh = self
            .display_modes
            .get(self.settings.selected_mode)
            .and_then(|m| m.refresh)
            .unwrap_or(60.0);
        c.draw_text(
            &self.regular,
            &format!("{refresh:.0} Hz"),
            sx + 16,
            306,
            20.0,
            MINT_DARK,
        );
        c.draw_text(&self.regular, "xrandr mode list", sx + 92, 310, 11.0, MUTED);
        let compositor_switch_x = i32::from(c.width) - 78;
        let compositor_switch_y = 276;
        let compositor_label_x = (i32::from(c.width) - 180).max(sx + 150);
        c.draw_text(&self.bold, "Compositor", compositor_label_x, 276, 15.0, INK);
        if self.settings.compositor_enabled {
            c.draw_round_rect(
                compositor_switch_x,
                compositor_switch_y,
                40,
                24,
                12,
                Color::rgba(160, 238, 220, 210),
            );
            c.draw_circle(
                compositor_switch_x + 28,
                compositor_switch_y + 12,
                8,
                Color::rgb(255, 255, 255),
            );
        } else {
            c.draw_round_rect(
                compositor_switch_x,
                compositor_switch_y,
                40,
                24,
                12,
                Color::rgba(200, 200, 200, 180),
            );
            c.draw_circle(
                compositor_switch_x + 12,
                compositor_switch_y + 12,
                8,
                Color::rgb(255, 255, 255),
            );
        }
        c.draw_text(
            &self.regular,
            if self.compositor_active {
                "active"
            } else if self.settings.compositor_enabled {
                "saved"
            } else {
                "off"
            },
            compositor_label_x,
            306,
            11.0,
            if self.settings.compositor_enabled {
                MINT_DARK
            } else {
                MUTED
            },
        );
        if let Some(status) = self.settings.display_status.as_deref() {
            c.draw_text(
                &self.regular,
                &compact(status, 54),
                sx + 16,
                328,
                11.0,
                BLUE,
            );
        }

        draw_card(c, sx, 360, i32::from(c.width) - sx - 24, 86);
        c.draw_text(&self.bold, "Brightness", sx + 16, 379, 15.0, INK);
        c.draw_text_right(
            &self.bold,
            &format!("{}%", self.settings.brightness_percent),
            i32::from(c.width) - 42,
            379,
            15.0,
            MINT_DARK,
        );
        let bar_x = sx + 16;
        let bar_y = 412;
        let bar_w = 230;
        c.draw_round_rect(bar_x, bar_y, bar_w, 12, 6, Color::rgba(225, 235, 238, 235));
        let fill_w = ((i32::from(self.settings.brightness_percent) - 10) * bar_w / 90).max(6);
        c.draw_round_rect(bar_x, bar_y, fill_w, 12, 6, Color::rgba(116, 213, 198, 220));
        c.draw_text(&self.regular, "10%", bar_x, 430, 11.0, MUTED);
        c.draw_text_right(&self.regular, "100%", bar_x + bar_w, 430, 11.0, MUTED);

        draw_card(c, sx, 462, i32::from(c.width) - sx - 24, 94);
        c.draw_text(&self.bold, "Sleep after", sx + 16, 481, 15.0, INK);
        c.draw_round_rect(sx + 18, 511, 28, 28, 9, Color::rgba(234, 244, 248, 220));
        c.draw_text_center(&self.bold, "-", sx + 32, 513, 18.0, MINT_DARK);
        c.draw_text_center(
            &self.bold,
            &format!("{} s", self.settings.sleep_after_secs),
            sx + 112,
            515,
            15.0,
            INK,
        );
        c.draw_round_rect(sx + 178, 511, 28, 28, 9, Color::rgba(234, 244, 248, 220));
        c.draw_text_center(&self.bold, "+", sx + 192, 513, 18.0, MINT_DARK);
    }

    fn draw_power_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        c.draw_text(&self.bold, "Power", sx, 22, 24.0, INK);
        c.draw_text(
            &self.regular,
            "Battery mode and live resource pressure.",
            sx,
            54,
            13.0,
            MUTED,
        );
        draw_card(c, sx, 86, i32::from(c.width) - sx - 24, 196);
        c.draw_text(&self.bold, "Auto power saver", sx + 16, 106, 15.0, INK);
        let switch_x = i32::from(c.width) - 78;
        let switch_y = 98;
        if self.settings.auto_power_saver_enabled {
            c.draw_round_rect(
                switch_x,
                switch_y,
                40,
                24,
                12,
                Color::rgba(160, 238, 220, 210),
            );
            c.draw_circle(switch_x + 28, switch_y + 12, 8, Color::rgb(255, 255, 255));
        } else {
            c.draw_round_rect(
                switch_x,
                switch_y,
                40,
                24,
                12,
                Color::rgba(200, 200, 200, 180),
            );
            c.draw_circle(switch_x + 12, switch_y + 12, 8, Color::rgb(255, 255, 255));
        }
        let input_x = sx + 16;
        c.draw_round_rect(
            input_x,
            132,
            118,
            30,
            9,
            if self.settings.auto_power_saver_editing {
                Color::rgba(188, 224, 255, 245)
            } else if self.settings.auto_power_saver_enabled {
                Color::rgba(255, 255, 255, 190)
            } else {
                Color::rgba(235, 235, 235, 145)
            },
        );
        if self.settings.auto_power_saver_editing {
            c.draw_round_rect(input_x, 132, 118, 30, 9, Color::rgba(73, 156, 231, 45));
        }
        let minutes = if self.settings.auto_power_saver_editing {
            self.settings.auto_power_saver_input.as_str()
        } else {
            self.settings.auto_power_saver_input.as_str()
        };
        c.draw_text(
            &self.regular,
            if minutes.is_empty() { "0" } else { minutes },
            input_x + 14,
            140,
            14.0,
            if self.settings.auto_power_saver_enabled {
                INK
            } else {
                MUTED
            },
        );
        c.draw_text(&self.regular, "min", input_x + 72, 140, 13.0, MUTED);
        c.draw_text(
            &self.regular,
            "idle minutes before battery saver",
            input_x + 136,
            140,
            12.0,
            MUTED,
        );

        c.draw_text(&self.bold, "Power profile", sx + 16, 174, 15.0, INK);
        let modes = [
            PowerMode::Saver,
            PowerMode::Balanced,
            PowerMode::Performance,
        ];
        for (idx, mode) in modes.iter().enumerate() {
            let y = 202 + idx as i32 * 24;
            let active = *mode == self.settings.power_mode;
            c.draw_round_rect(
                sx + 16,
                y - 5,
                i32::from(c.width) - sx - 58,
                21,
                8,
                if active {
                    Color::rgba(116, 213, 198, 95)
                } else {
                    Color::rgba(255, 255, 255, 118)
                },
            );
            c.draw_text(
                &self.regular,
                mode.label(),
                sx + 28,
                y,
                12.0,
                if active { MINT_DARK } else { INK },
            );
        }

        draw_card(c, sx, 304, i32::from(c.width) - sx - 24, 154);
        c.draw_text(&self.bold, "System", sx + 16, 324, 15.0, INK);
        draw_metric_bar(
            c,
            &self.regular,
            sx + 16,
            354,
            "CPU",
            self.metrics.cpu_usage,
            "%",
        );
        c.draw_text(&self.regular, "CPU frequency", sx + 16, 390, 12.0, MUTED);
        let freq_lines = cpu_frequency_lines(&self.metrics.cpu_frequencies, 46);
        for (idx, line) in freq_lines.iter().take(3).enumerate() {
            c.draw_text(
                &self.regular,
                line,
                sx + 16,
                412 + idx as i32 * 16,
                11.0,
                INK,
            );
        }

        draw_card(c, sx, 476, i32::from(c.width) - sx - 24, 76);
        c.draw_text(&self.bold, "Battery", sx + 16, 496, 15.0, INK);
        c.draw_text(
            &self.regular,
            self.metrics
                .battery
                .as_deref()
                .unwrap_or("No battery exposed"),
            sx + 16,
            524,
            14.0,
            MINT_DARK,
        );
    }

    fn draw_wallpaper_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        c.draw_text(&self.bold, "Wallpaper", sx, 22, 24.0, INK);
        c.draw_text(
            &self.regular,
            "Select one of the embedded wallpapers.",
            sx,
            54,
            13.0,
            MUTED,
        );
        for (idx, asset) in WALLPAPERS.iter().enumerate() {
            let y = 88 + idx as i32 * 116;
            draw_card(c, sx, y, i32::from(c.width) - sx - 24, 94);
            let preview_x = sx + 14;
            let preview_y = y + 14;
            if let Some(preview) = self
                .wallpaper_previews
                .get(idx)
                .and_then(|preview| preview.as_ref())
            {
                paint_bgr_pixels(c, preview, preview_x, preview_y, 92, 56);
            }
            c.draw_round_rect(
                preview_x,
                preview_y,
                92,
                56,
                10,
                Color::rgba(255, 255, 255, 28),
            );
            c.draw_text(&self.bold, asset.name, sx + 122, y + 20, 14.0, INK);
            c.draw_text(
                &self.regular,
                if idx == self.wallpaper_index {
                    "Current wallpaper"
                } else {
                    "Click to apply"
                },
                sx + 122,
                y + 46,
                12.0,
                if idx == self.wallpaper_index {
                    MINT_DARK
                } else {
                    MUTED
                },
            );
            if idx == self.wallpaper_index {
                c.draw_circle(
                    i32::from(c.width) - 44,
                    y + 44,
                    12,
                    Color::rgba(116, 213, 198, 180),
                );
                c.draw_line(
                    i32::from(c.width) - 50,
                    y + 44,
                    i32::from(c.width) - 45,
                    y + 49,
                    2,
                    Color::rgb(255, 255, 255),
                );
                c.draw_line(
                    i32::from(c.width) - 45,
                    y + 49,
                    i32::from(c.width) - 37,
                    y + 39,
                    2,
                    Color::rgb(255, 255, 255),
                );
            }
        }
    }

    fn ensure_wallpaper_previews(&mut self) {
        for (idx, asset) in WALLPAPERS.iter().enumerate() {
            if self.wallpaper_previews[idx].is_none() {
                self.wallpaper_previews[idx] =
                    render_asset_preview_pixels(asset.bytes, 92, 56).ok();
            }
        }
    }

    fn draw_audio_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        c.draw_text(&self.bold, "Audio", sx, 22, 24.0, INK);
        draw_card(c, sx, 86, i32::from(c.width) - sx - 24, 112);
        c.draw_text(&self.bold, "Volume", sx + 16, 106, 15.0, INK);
        let volume = read_audio_volume_percent();
        let volume_pct = volume.unwrap_or(0);
        let bar_w = 230;
        c.draw_round_rect(sx + 16, 142, bar_w, 10, 5, Color::rgba(211, 225, 232, 170));
        c.draw_round_rect(
            sx + 16,
            142,
            bar_w * i32::from(volume_pct) / 100,
            10,
            5,
            Color::rgba(116, 213, 198, 210),
        );
        c.draw_text(
            &self.regular,
            &volume
                .map(|pct| format!("{pct}%"))
                .unwrap_or_else(|| "unavailable".to_string()),
            sx + 262,
            136,
            15.0,
            INK,
        );
        if let Some(status) = self.settings.audio_status.as_deref() {
            c.draw_text(
                &self.regular,
                &compact(status, 52),
                sx + 16,
                166,
                11.0,
                BLUE,
            );
        }
        let card_w = i32::from(c.width) - sx - 24;
        draw_card(c, sx, 220, card_w, 150);
        c.draw_text(&self.bold, "Output device", sx + 16, 240, 15.0, INK);
        let outputs = read_audio_devices(AudioDeviceKind::Output);
        if outputs.is_empty() {
            c.draw_text(
                &self.regular,
                "No output devices found",
                sx + 16,
                272,
                12.0,
                MUTED,
            );
        }
        for (idx, dev) in outputs.iter().take(3).enumerate() {
            let row_y = 260 + idx as i32 * 30;
            if dev.is_default {
                c.draw_round_rect(
                    sx + 12,
                    row_y - 3,
                    card_w - 24,
                    24,
                    6,
                    Color::rgba(116, 213, 198, 120),
                );
            }
            c.draw_text(
                &self.regular,
                &compact(&dev.label, 45),
                sx + 18,
                row_y + 4,
                12.0,
                INK,
            );
            if dev.is_default {
                c.draw_text(
                    &self.bold,
                    "default",
                    sx + card_w - 76,
                    row_y + 4,
                    11.0,
                    BLUE,
                );
            }
        }
        draw_card(c, sx, 392, card_w, 108);
        c.draw_text(&self.bold, "Input device", sx + 16, 412, 15.0, INK);
        let inputs = read_audio_devices(AudioDeviceKind::Input);
        if inputs.is_empty() {
            c.draw_text(
                &self.regular,
                "No input devices found",
                sx + 16,
                444,
                12.0,
                MUTED,
            );
        }
        for (idx, dev) in inputs.iter().take(2).enumerate() {
            let row_y = 432 + idx as i32 * 30;
            if dev.is_default {
                c.draw_round_rect(
                    sx + 12,
                    row_y - 3,
                    card_w - 24,
                    24,
                    6,
                    Color::rgba(116, 213, 198, 120),
                );
            }
            c.draw_text(
                &self.regular,
                &compact(&dev.label, 45),
                sx + 18,
                row_y + 4,
                12.0,
                INK,
            );
            if dev.is_default {
                c.draw_text(
                    &self.bold,
                    "default",
                    sx + card_w - 76,
                    row_y + 4,
                    11.0,
                    BLUE,
                );
            }
        }
    }

    fn draw_network_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        let card_w = i32::from(c.width) - sx - 24;
        c.draw_text(&self.bold, "Network", sx, 22, 24.0, INK);
        c.draw_text(
            &self.regular,
            "Wired and Wi-Fi interfaces.",
            sx,
            54,
            13.0,
            MUTED,
        );
        draw_card(c, sx, 86, card_w, 154);
        c.draw_text(&self.bold, "Current status", sx + 16, 106, 15.0, INK);

        // Scroll exactly with step 24 matching line spacing!
        let start = (self.settings.scroll / 24).max(0) as usize;
        for (idx, line) in read_network_details()
            .iter()
            .skip(start)
            .take(4)
            .enumerate()
        {
            c.draw_text(
                &self.regular,
                &compact(line, 62),
                sx + 16,
                134 + idx as i32 * 24,
                13.0,
                if idx % 3 == 0 { INK } else { MUTED },
            );
        }

        draw_card(c, sx, 258, card_w, 288);
        c.draw_text(&self.bold, "Wi-Fi", sx + 16, 278, 15.0, INK);

        let connected_wifi = self.settings.wifi_connected.clone().flatten();
        let wifi_enabled = self.settings.wifi_radio_enabled.unwrap_or(true);
        let disconnect_color = if connected_wifi.is_some() {
            INK
        } else {
            Color::rgba(120, 120, 120, 170)
        };

        c.draw_round_rect(sx + 75, 273, 58, 24, 7, Color::rgba(234, 244, 248, 220));
        c.draw_text_center(&self.bold, "Refresh", sx + 104, 281, 10.0, MINT_DARK);
        c.draw_round_rect(sx + 141, 273, 78, 24, 7, Color::rgba(234, 244, 248, 220));
        c.draw_text_center(
            &self.bold,
            "Disconnect",
            sx + 180,
            281,
            10.0,
            disconnect_color,
        );

        // Beautiful Premium 40x24 Sliding On/Off Switch
        let tx = sx + 227;
        let ty = 273;
        if wifi_enabled {
            c.draw_round_rect(tx, ty, 40, 24, 12, Color::rgba(160, 238, 220, 200));
            c.draw_circle(tx + 28, ty + 12, 8, Color::rgb(255, 255, 255));
        } else {
            c.draw_round_rect(tx, ty, 40, 24, 12, Color::rgba(200, 200, 200, 180));
            c.draw_circle(tx + 12, ty + 12, 8, Color::rgb(255, 255, 255));
        }

        let mut list_start_y = 344;
        if let Some(wifi) = connected_wifi.as_ref() {
            c.draw_text(&self.bold, "Connected", sx + 16, 302, 11.0, MINT_DARK);
            c.draw_text(
                &self.bold,
                &compact(&wifi.ssid, 44),
                sx + 16,
                318,
                13.0,
                INK,
            );
            c.draw_text(
                &self.regular,
                wifi.ip.as_deref().unwrap_or("no ip"),
                sx + 16,
                334,
                12.0,
                MUTED,
            );
            list_start_y = 376;
        } else {
            c.draw_text(&self.regular, "Not connected", sx + 16, 302, 12.0, MUTED);
        }

        let status_y = list_start_y - 22;
        if let Some(status) = self.settings.wifi_status.as_deref() {
            c.draw_text(
                &self.regular,
                &compact(status, 54),
                sx + 16,
                status_y,
                11.0,
                BLUE,
            );
        } else {
            c.draw_text(
                &self.regular,
                "Click Refresh to scan nearby Wi-Fi networks",
                sx + 16,
                status_y,
                11.0,
                MUTED,
            );
        }

        if self.settings.wifi_disconnect_confirm {
            c.draw_round_rect(
                sx + 12,
                list_start_y,
                card_w - 24,
                44,
                8,
                Color::rgba(255, 255, 255, 210),
            );
            c.draw_text(
                &self.bold,
                "Disconnect current Wi-Fi?",
                sx + 24,
                list_start_y + 26,
                13.0,
                INK,
            );
            c.draw_round_rect(
                sx + card_w - 194,
                list_start_y + 8,
                76,
                28,
                7,
                Color::rgba(211, 225, 232, 170),
            );
            c.draw_text_center(
                &self.bold,
                "Cancel",
                sx + card_w - 156,
                list_start_y + 26,
                11.0,
                INK,
            );
            c.draw_round_rect(
                sx + card_w - 108,
                list_start_y + 8,
                88,
                28,
                7,
                Color::rgba(241, 126, 135, 150),
            );
            c.draw_text_center(
                &self.bold,
                "Disconnect",
                sx + card_w - 64,
                list_start_y + 26,
                11.0,
                Color::rgb(160, 58, 68),
            );
            return;
        }

        if self.settings.wifi_networks.is_empty() {
            c.draw_text(
                &self.regular,
                "No Wi-Fi networks found",
                sx + 16,
                list_start_y + 10,
                13.0,
                MUTED,
            );
        } else {
            for (idx, network) in self
                .settings
                .wifi_networks
                .iter()
                .skip(self.settings.wifi_scroll)
                .take(5)
                .enumerate()
            {
                let y = list_start_y + idx as i32 * 24;
                let selected = self
                    .settings
                    .wifi_selected
                    .as_deref()
                    .is_some_and(|ssid| ssid == network.ssid);
                if selected {
                    c.draw_round_rect(
                        sx + 12,
                        y - 4, // Perfectly centers text inside the 22px high overlay
                        card_w - 24,
                        22,
                        7,
                        Color::rgba(160, 238, 220, 92),
                    );
                }
                c.draw_text(
                    &self.regular,
                    &compact(&network.ssid, 48),
                    sx + 18,
                    y,
                    13.0,
                    if selected { MINT_DARK } else { INK },
                );
            }
        }

        if let Some(ssid) = self.settings.wifi_selected.as_deref() {
            c.draw_text(
                &self.bold,
                &compact(&format!("Password for {ssid}"), 42),
                sx + 16,
                492,
                13.0,
                INK,
            );
            let input_x = sx + 16;
            let button_w = 34; // 34x34 icon button
            let gap = 12;
            let input_w = (card_w - 32 - button_w - gap).max(132);
            let input_y = 508;
            c.draw_round_rect(
                input_x,
                input_y,
                input_w,
                34,
                7,
                if self.settings.wifi_password_editing {
                    Color::rgba(255, 255, 255, 220)
                } else {
                    Color::rgba(255, 255, 255, 145)
                },
            );
            c.draw_rect(input_x + 8, input_y + 33, input_w - 16, 1, CARD_LINE);
            let shown = if self.settings.wifi_password.is_empty() {
                "enter password".to_string()
            } else {
                password_mask(self.settings.wifi_password.chars().count())
            };

            // Text y = input_y + 10 is perfectly vertically centered (top of bounding box)
            c.draw_text(
                &self.regular,
                &compact(&shown, 38),
                input_x + 12,
                input_y + 10,
                13.0,
                if self.settings.wifi_password.is_empty() {
                    MUTED
                } else {
                    INK
                },
            );

            let button_x = input_x + input_w + gap;
            c.draw_round_rect(
                button_x,
                input_y,
                button_w,
                34,
                7,
                Color::rgba(160, 238, 220, 170),
            );

            // Draw Connect icon arrow centered vertically and horizontally
            let cx = button_x + 17;
            let cy = input_y + 17;
            c.draw_line(cx - 6, cy, cx + 6, cy, 2, MINT_DARK);
            c.draw_line(cx + 6, cy, cx + 2, cy - 4, 2, MINT_DARK);
            c.draw_line(cx + 6, cy, cx + 2, cy + 4, 2, MINT_DARK);
        }
    }

    fn draw_bluetooth_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        c.draw_text(&self.bold, "Bluetooth", sx, 22, 24.0, INK);
        draw_card(c, sx, 86, i32::from(c.width) - sx - 24, 116);
        c.draw_text(&self.bold, "Connected devices", sx + 16, 106, 15.0, INK);
        let devices = read_bluetooth_devices();
        if devices.is_empty() {
            c.draw_text(
                &self.regular,
                "No connected devices",
                sx + 16,
                140,
                12.0,
                MUTED,
            );
        } else {
            for (idx, dev) in devices.iter().take(3).enumerate() {
                c.draw_text(
                    &self.regular,
                    &compact(dev, 50),
                    sx + 16,
                    140 + idx as i32 * 26,
                    12.0,
                    INK,
                );
            }
        }
        draw_card(c, sx, 224, i32::from(c.width) - sx - 24, 76);
        c.draw_text(&self.bold, "Add device", sx + 16, 246, 15.0, INK);
        c.draw_text(
            &self.regular,
            "Click to open bluetoothctl pairing helper",
            sx + 16,
            274,
            12.0,
            MUTED,
        );
    }

    fn draw_startup_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        c.draw_text(&self.bold, "Startup", sx, 22, 24.0, INK);
        c.draw_text(
            &self.regular,
            "Autostart apps for this desktop.",
            sx,
            54,
            13.0,
            MUTED,
        );
        draw_card(c, sx, 86, i32::from(c.width) - sx - 24, 394);
        let apps = read_autostart_apps();
        if apps.is_empty() {
            c.draw_text(
                &self.regular,
                "No autostart entries",
                sx + 16,
                116,
                12.0,
                MUTED,
            );
        } else {
            let start = (self.settings.scroll / 28).max(0) as usize;
            for (idx, app) in apps.iter().skip(start).take(12).enumerate() {
                c.draw_text(
                    &self.regular,
                    &compact(app, 54),
                    sx + 16,
                    116 + idx as i32 * 28,
                    13.0,
                    INK,
                );
            }
        }
    }

    fn draw_apps_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        let card_w = i32::from(c.width) - sx - 24;
        c.draw_text(&self.bold, "Apps", sx, 22, 24.0, INK);
        c.draw_text(
            &self.regular,
            "Choose default applications for this desktop.",
            sx,
            54,
            12.0,
            MUTED,
        );

        let kinds = [
            DefaultAppKind::Terminal,
            DefaultAppKind::Browser,
            DefaultAppKind::Photo,
            DefaultAppKind::Video,
        ];
        for (idx, kind) in kinds.iter().enumerate() {
            let x = sx + idx as i32 * ((card_w - 6) / 4);
            let w = (card_w - 12) / 4;
            c.draw_round_rect(
                x,
                84,
                w,
                34,
                8,
                if *kind == self.settings.app_kind {
                    Color::rgba(116, 213, 198, 95)
                } else {
                    Color::rgba(255, 255, 255, 118)
                },
            );
            c.draw_text_center(
                &self.bold,
                kind.label(),
                x + w / 2,
                94,
                11.0,
                if *kind == self.settings.app_kind {
                    MINT_DARK
                } else {
                    INK
                },
            );
        }

        draw_card(c, sx, 132, card_w, 234);
        c.draw_text(&self.bold, "Default apps", sx + 16, 151, 14.0, INK);
        c.draw_text(
            &self.regular,
            &format!("Choose default {}", self.settings.app_kind.label()),
            sx + 112,
            152,
            11.0,
            MUTED,
        );
        let selected = self.selected_app_command(self.settings.app_kind);
        let apps = self.available_apps(self.settings.app_kind);
        let start = (self.settings.scroll / 29).max(0) as usize;
        if apps.is_empty() {
            c.draw_text(
                &self.regular,
                "No installed applications found.",
                sx + 16,
                185,
                12.0,
                MUTED,
            );
        }
        for (idx, app) in apps.iter().skip(start).take(6).enumerate() {
            let y = 180 + idx as i32 * 29;
            let active = selected == app.command;
            c.draw_round_rect(
                sx + 14,
                y - 5,
                card_w - 28,
                24,
                8,
                if active {
                    Color::rgba(116, 213, 198, 95)
                } else {
                    Color::rgba(255, 255, 255, 118)
                },
            );
            c.draw_text(
                &self.regular,
                &compact(&app.name, 46),
                sx + 25,
                y,
                12.0,
                if active { MINT_DARK } else { INK },
            );
        }
        if apps.len() > 6 {
            c.draw_text(
                &self.regular,
                "Scroll to see more default apps",
                sx + card_w - 192,
                151,
                10.0,
                MUTED,
            );
        }

        if self.settings.app_kind == DefaultAppKind::Terminal {
            draw_card(c, sx, 382, card_w, 104);
            c.draw_text(
                &self.bold,
                "Custom terminal command",
                sx + 16,
                399,
                14.0,
                INK,
            );
            c.draw_round_rect(
                sx + 14,
                426,
                card_w - 28,
                32,
                9,
                if self.settings.terminal_editing {
                    Color::rgba(116, 213, 198, 95)
                } else {
                    Color::rgba(224, 236, 242, 170)
                },
            );
            let shown = if self.settings.terminal_command.is_empty() {
                "Click and type a command; Enter saves and launches"
            } else {
                self.settings.terminal_command.as_str()
            };
            c.draw_text(&self.regular, &compact(shown, 52), sx + 25, 435, 12.0, INK);
        }
        if let Some(status) = self.settings.app_status.as_ref() {
            c.draw_text(
                &self.regular,
                &compact(status, 64),
                sx + 16,
                506,
                12.0,
                MINT_DARK,
            );
        }
    }

    fn draw_about_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        c.draw_text(&self.bold, "About", sx, 22, 24.0, INK);
        c.draw_text(
            &self.regular,
            "Hardware and network telemetry.",
            sx,
            54,
            12.0,
            MUTED,
        );
        draw_card(c, sx, 86, i32::from(c.width) - sx - 24, 248);
        c.draw_text(&self.bold, "Computer", sx + 16, 106, 15.0, INK);
        let cpu = compact(&self.metrics.cpu_model, 34);
        draw_info_row(c, &self.regular, sx + 16, 136, "CPU", &cpu);
        draw_info_row(
            c,
            &self.regular,
            sx + 16,
            164,
            "Status",
            &self.metrics.cpu_status,
        );
        draw_info_row(
            c,
            &self.regular,
            sx + 16,
            192,
            "RAM",
            &format!(
                "{} / {}",
                format_kib(self.metrics.ram_used_kb),
                format_kib(self.metrics.ram_total_kb)
            ),
        );
        draw_info_row(
            c,
            &self.regular,
            sx + 16,
            220,
            "Swap",
            &format!(
                "{} / {}",
                format_kib(self.metrics.swap_used_kb),
                format_kib(self.metrics.swap_total_kb)
            ),
        );
        let gpus = if self.metrics.gpus.is_empty() {
            "No GPU info".to_string()
        } else {
            compact(&self.metrics.gpus.join(", "), 32)
        };
        draw_info_row(c, &self.regular, sx + 16, 248, "GPU", &gpus);
        let nics = if self.metrics.nics.is_empty() {
            "No network card".to_string()
        } else {
            compact(&self.metrics.nics.join(", "), 32)
        };
        draw_info_row(c, &self.regular, sx + 16, 276, "NIC", &nics);
        draw_info_row(c, &self.regular, sx + 16, 304, "Display", &self.display);

        draw_card(c, sx, 354, i32::from(c.width) - sx - 24, 154);
        c.draw_text(&self.bold, "Network speed", sx + 16, 376, 15.0, INK);
        c.draw_text(&self.regular, "Down", sx + 16, 408, 12.0, MUTED);
        c.draw_text(
            &self.bold,
            &format_bps(self.metrics.net_rx_bps),
            sx + 70,
            405,
            17.0,
            INK,
        );
        c.draw_text(&self.regular, "Up", sx + 16, 450, 12.0, MUTED);
        c.draw_text(
            &self.bold,
            &format_bps(self.metrics.net_tx_bps),
            sx + 70,
            447,
            17.0,
            INK,
        );
        draw_sparkline(
            c,
            sx + 152,
            402,
            i32::from(c.width) - sx - 190,
            22,
            self.metrics.net_rx_bps,
            BLUE,
        );
        draw_sparkline(
            c,
            sx + 152,
            444,
            i32::from(c.width) - sx - 190,
            22,
            self.metrics.net_tx_bps,
            MINT_DARK,
        );
    }

    fn take_workspace_ui_state(&mut self) -> WorkspaceUiState {
        WorkspaceUiState {
            folder_mode: self.folder_mode,
            folder_entries: std::mem::take(&mut self.folder_entries),
            folder_path: std::mem::take(&mut self.folder_path),
            folder_selected: self.folder_selected.take(),
            folder_scroll: self.folder_scroll,
            folder_front: self.folder_front,
            folder_more_open: self.folder_more_open,
            folder_sort_open: self.folder_sort_open,
            folder_sort: self.folder_sort,
            folder_width: self.folder_width,
            folder_height: self.folder_height,
            folder_terminal_width: self.folder_terminal_width,
            folder_terminal_height: self.folder_terminal_height,
            folder_terminal: std::mem::replace(
                &mut self.folder_terminal,
                FolderTerminal::new(folder_path_for(FolderMode::Home)),
            ),
            media: self.media.take(),
            media_slots: std::mem::take(&mut self.media_slots),
            media_front: self.media_front,
            media_front_slot: self.media_front_slot,
            media_text_selection: self.media_text_selection.take(),
            media_text_selecting: self.media_text_selecting,
            media_text_selection_redraw_at: self.media_text_selection_redraw_at.take(),
            media_text_live_rects: std::mem::take(&mut self.media_text_live_rects),
            media_context_open: self.media_context_open,
            media_trash_prompt: self.media_trash_prompt,
            folder_context_open: self.folder_context_open,
            folder_context_pos: self.folder_context_pos,
            folder_clipboard: self.folder_clipboard.take(),
            folder_info: self.folder_info.take(),
            folder_terminal_selection: self.folder_terminal_selection.take(),
            folder_terminal_selecting: self.folder_terminal_selecting,
            folder_terminal_live_rects: std::mem::take(&mut self.folder_terminal_live_rects),
            folder_drag: self.folder_drag.take(),
            folder_press: self.folder_press.take(),
        }
    }

    fn apply_workspace_ui_state(&mut self, state: WorkspaceUiState) {
        self.folder_mode = state.folder_mode;
        self.folder_entries = state.folder_entries;
        self.folder_path = state.folder_path;
        self.folder_selected = state.folder_selected;
        self.folder_scroll = state.folder_scroll;
        self.folder_front = state.folder_front;
        self.folder_more_open = state.folder_more_open;
        self.folder_sort_open = state.folder_sort_open;
        self.folder_sort = state.folder_sort;
        self.folder_width = state.folder_width;
        self.folder_height = state.folder_height;
        self.folder_terminal_width = state.folder_terminal_width;
        self.folder_terminal_height = state.folder_terminal_height;
        self.folder_terminal = state.folder_terminal;
        self.media = state.media;
        self.media_slots = state.media_slots;
        self.media_front = state.media_front;
        self.media_front_slot = state.media_front_slot;
        self.media_text_selection = state.media_text_selection;
        self.media_text_selecting = state.media_text_selecting;
        self.media_text_selection_redraw_at = state.media_text_selection_redraw_at;
        self.media_text_live_rects = state.media_text_live_rects;
        self.media_context_open = state.media_context_open;
        self.media_trash_prompt = state.media_trash_prompt;
        self.folder_context_open = state.folder_context_open;
        self.folder_context_pos = state.folder_context_pos;
        self.folder_clipboard = state.folder_clipboard;
        self.folder_info = state.folder_info;
        self.folder_terminal_selection = state.folder_terminal_selection;
        self.folder_terminal_selecting = state.folder_terminal_selecting;
        self.folder_terminal_live_rects = state.folder_terminal_live_rects;
        self.folder_drag = state.folder_drag;
        self.folder_press = state.folder_press;
    }

    fn restore_workspace_ui_windows(&mut self) -> AnyResult<()> {
        let folder = self.folder_geometry();
        let terminal = self.folder_terminal_geometry();
        self.conn.configure_window(
            self.ui.folder,
            &ConfigureWindowAux::new()
                .x(i32::from(folder.0))
                .y(i32::from(folder.1))
                .width(u32::from(folder.2))
                .height(u32::from(folder.3)),
        )?;
        self.conn.configure_window(
            self.ui.folder_terminal,
            &ConfigureWindowAux::new()
                .x(i32::from(terminal.0))
                .y(i32::from(terminal.1))
                .width(u32::from(terminal.2))
                .height(u32::from(terminal.3)),
        )?;
        self.sync_folder_terminal_size();
        self.conn.map_window(self.ui.folder)?;
        if self.folder_terminal.visible {
            self.conn.map_window(self.ui.folder_terminal)?;
        } else {
            self.conn.unmap_window(self.ui.folder_terminal)?;
        }
        for (idx, window) in self.ui.media.iter().copied().enumerate() {
            if self.media_slots.get(idx).and_then(|m| m.as_ref()).is_some() {
                self.conn.map_window(window)?;
            } else {
                self.conn.unmap_window(window)?;
            }
        }
        self.redraw_folder()?;
        if self.folder_terminal.visible {
            self.redraw_folder_terminal()?;
        }
        for slot in 0..MEDIA_SLOT_COUNT {
            if self
                .media_slots
                .get(slot)
                .and_then(|m| m.as_ref())
                .is_some()
            {
                self.redraw_media_slot(slot)?;
            }
        }
        Ok(())
    }

    fn add_workspace(&mut self) -> AnyResult<()> {
        if self.workspace_count >= MAX_WORKSPACE_COUNT {
            return Ok(());
        }
        let workspace = self.workspace_count;
        self.workspace_count += 1;
        self.workspace_ui
            .push(WorkspaceUiState::new(self.screen_height));

        // Update EWMH _NET_NUMBER_OF_DESKTOPS
        if let Ok(num_atom) = self.atom(b"_NET_NUMBER_OF_DESKTOPS") {
            if let Ok(cardinal_atom) = self.atom(b"CARDINAL") {
                let _ = self.conn.change_property32(
                    PropMode::REPLACE,
                    self.root,
                    num_atom,
                    cardinal_atom,
                    &[self.workspace_count as u32],
                );
            }
        }

        self.switch_workspace(workspace)
    }

    fn switch_workspace(&mut self, workspace: usize) -> AnyResult<()> {
        if workspace >= self.workspace_count || workspace == self.active_workspace {
            return Ok(());
        }
        if self.choose_file_mode {
            let _ = self.cancel_choose_file();
        }
        self.end_drag()?;
        let previous = self.active_workspace;
        let hidden_frames = self
            .clients
            .values()
            .filter(|info| info.workspace == previous && info.mapped)
            .map(|info| info.frame)
            .collect::<Vec<_>>();
        let shown_frames = self
            .clients
            .values()
            .filter(|info| info.workspace == workspace && info.mapped)
            .map(|info| info.frame)
            .collect::<Vec<_>>();
        for frame in hidden_frames {
            self.ignored_unmaps.push(frame);
            self.conn.unmap_window(frame)?;
        }
        while self.workspace_ui.len() <= workspace {
            self.workspace_ui
                .push(WorkspaceUiState::new(self.screen_height));
        }
        let previous_ui = self.take_workspace_ui_state();
        if let Some(slot) = self.workspace_ui.get_mut(previous) {
            *slot = previous_ui;
        }
        let next_ui = std::mem::replace(
            &mut self.workspace_ui[workspace],
            WorkspaceUiState::new(self.screen_height),
        );
        self.apply_workspace_ui_state(next_ui);
        self.active_workspace = workspace;

        // Update EWMH _NET_CURRENT_DESKTOP
        if let Ok(cur_atom) = self.atom(b"_NET_CURRENT_DESKTOP") {
            if let Ok(cardinal_atom) = self.atom(b"CARDINAL") {
                let _ = self.conn.change_property32(
                    PropMode::REPLACE,
                    self.root,
                    cur_atom,
                    cardinal_atom,
                    &[self.active_workspace as u32],
                );
            }
        }

        for frame in shown_frames {
            self.conn.map_window(frame)?;
        }
        self.hide_dock_more_menu()?;
        self.dock_last_click = None;
        self.active_client = None;
        self.update_active_window_property()?;
        self.conn
            .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
        self.restore_workspace_ui_windows()?;
        self.redraw_topbar()?;
        self.redraw_dock()?;
        self.raise_ui()
    }

    fn open_settings_tab(&mut self, tab: SettingsTab) -> AnyResult<()> {
        self.settings.tab = tab;
        self.settings.scroll = 0;
        self.settings_visible = true;
        self.settings_front = true;
        self.folder_front = false;
        self.media_front = false;
        self.conn.map_window(self.ui.settings)?;
        self.raise_ui()?;
        if tab == SettingsTab::Network {
            self.ensure_wifi_refresh_started(false);
        }
        self.redraw_settings()?;
        self.redraw_topbar()
    }

    fn remember_clipboard_item(&mut self, item: ClipboardItem) -> bool {
        if matches!(&item, ClipboardItem::Text(text) if text.is_empty()) {
            return false;
        }
        self.clipboard_history
            .retain(|entry| !clipboard_items_match(&entry.item, &item));
        self.clipboard_history.insert(0, ClipboardEntry { item });
        if self.clipboard_history.len() > CLIPBOARD_HISTORY_LIMIT {
            self.clipboard_history.truncate(CLIPBOARD_HISTORY_LIMIT);
        }
        self.clipboard_history_page = self.clamped_clipboard_page();
        true
    }

    fn poll_clipboard_history(&mut self) -> AnyResult<bool> {
        if let Some(rx) = self.clipboard_poll_rx.as_ref() {
            match rx.try_recv() {
                Ok(result) => {
                    self.clipboard_poll_rx = None;
                    return self.apply_clipboard_poll_result(result);
                }
                Err(TryRecvError::Empty) => return Ok(false),
                Err(TryRecvError::Disconnected) => {
                    self.clipboard_poll_rx = None;
                    return Ok(false);
                }
            }
        }

        if self.clipboard_watch_supported {
            if !self.clipboard_dirty {
                return Ok(false);
            }
            self.clipboard_dirty = false;
        } else if self.last_clipboard_poll.elapsed() < IDLE_CHECK_INTERVAL {
            return Ok(false);
        }
        self.last_clipboard_poll = Instant::now();
        self.start_clipboard_poll();
        Ok(false)
    }

    fn start_clipboard_poll(&mut self) {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let item = read_image_clipboard()
                .map(|(path, sig)| ClipboardPollItem::Image(path, sig))
                .or_else(|| read_text_clipboard().map(ClipboardPollItem::Text));
            let _ = tx.send(ClipboardPollResult { item });
        });
        self.clipboard_poll_rx = Some(rx);
    }

    fn apply_clipboard_poll_result(&mut self, result: ClipboardPollResult) -> AnyResult<bool> {
        let Some(item) = result.item else {
            return Ok(false);
        };
        match item {
            ClipboardPollItem::Image(path, sig) => {
                if self.last_seen_clipboard_image_sig == Some(sig) {
                    return Ok(false);
                }
                self.last_seen_clipboard_image_sig = Some(sig);
                let item = ClipboardItem::Image(path);
                append_clipboard_history(&item);
                self.remember_clipboard_item(item);
                if self.clipboard_menu_visible {
                    self.configure_clipboard_menu()?;
                    self.redraw_clipboard_menu()?;
                    return Ok(true);
                }
                return Ok(false);
            }
            ClipboardPollItem::Text(text) => {
                let text = text.trim_end_matches('\0').to_string();
                if text.is_empty() || text.len() > 1_000_000 {
                    return Ok(false);
                }
                if self.last_seen_clipboard_text.as_deref() == Some(text.as_str()) {
                    return Ok(false);
                }
                self.last_seen_clipboard_text = Some(text.clone());
                let item = ClipboardItem::Text(text);
                append_clipboard_history(&item);
                self.remember_clipboard_item(item);
                if self.clipboard_menu_visible {
                    self.configure_clipboard_menu()?;
                    self.redraw_clipboard_menu()?;
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn toggle_clipboard_menu(&mut self) -> AnyResult<()> {
        if self.clipboard_menu_visible {
            return self.hide_clipboard_menu();
        }
        self.clipboard_menu_visible = true;
        self.clipboard_history_page = 0;
        self.configure_clipboard_menu()?;
        self.conn.map_window(self.ui.clipboard_menu)?;
        self.redraw_clipboard_menu()?;
        self.raise_ui()
    }

    fn hide_clipboard_menu(&mut self) -> AnyResult<()> {
        if self.clipboard_menu_visible {
            self.clipboard_menu_visible = false;
            self.conn.unmap_window(self.ui.clipboard_menu)?;
        }
        Ok(())
    }

    fn handle_clipboard_menu_press(&mut self, detail: u8, x: i32, y: i32) -> AnyResult<()> {
        if detail == 4 || detail == 5 {
            return self.handle_clipboard_menu_scroll(detail);
        }
        if detail != 1 {
            return Ok(());
        }
        if y < 38 {
            if point_in_rect(
                x,
                y,
                CLIPBOARD_MENU_PREV_X,
                CLIPBOARD_MENU_NAV_Y,
                CLIPBOARD_MENU_NAV_W,
                CLIPBOARD_MENU_NAV_H,
            ) {
                if self.clamped_clipboard_page() > 0 {
                    self.clipboard_history_page = self.clamped_clipboard_page() - 1;
                    self.configure_clipboard_menu()?;
                    self.redraw_clipboard_menu()?;
                }
            } else if point_in_rect(
                x,
                y,
                CLIPBOARD_MENU_NEXT_X,
                CLIPBOARD_MENU_NAV_Y,
                CLIPBOARD_MENU_NAV_W,
                CLIPBOARD_MENU_NAV_H,
            ) {
                let page = self.clamped_clipboard_page();
                if page + 1 < self.clipboard_page_count() {
                    self.clipboard_history_page = page + 1;
                    self.configure_clipboard_menu()?;
                    self.redraw_clipboard_menu()?;
                }
            }
            return Ok(());
        }
        let (start, end) = self.clipboard_page_range();
        let mut row_y = 46;
        let mut selected_idx = None;
        for (offset, entry) in self.clipboard_history[start..end].iter().enumerate() {
            let row_h = clipboard_entry_row_height(entry);
            if y >= row_y - 8 && y < row_y - 8 + row_h - 8 {
                selected_idx = Some(start + offset);
                break;
            }
            row_y += row_h;
        }
        let Some(idx) = selected_idx else {
            return Ok(());
        };
        let Some(entry) = self.clipboard_history.get(idx).cloned() else {
            return Ok(());
        };
        match &entry.item {
            ClipboardItem::Text(text) => {
                copy_text_to_clipboard(text);
                self.last_seen_clipboard_text = Some(text.clone());
            }
            ClipboardItem::Image(path) => {
                copy_image_to_clipboard(path);
                self.last_seen_clipboard_image_sig = clipboard_file_image_signature(path);
            }
        }
        self.remember_clipboard_item(entry.item);
        self.hide_clipboard_menu()?;
        self.redraw_topbar()?;
        if let Some(client) = self.active_client {
            let _ = self.focus_window(client);
        }
        paste_clipboard_now(&self.display);
        Ok(())
    }

    fn handle_clipboard_menu_scroll(&mut self, detail: u8) -> AnyResult<()> {
        let page = self.clamped_clipboard_page();
        let next_page = match detail {
            4 => page.saturating_sub(1),
            5 if page + 1 < self.clipboard_page_count() => page + 1,
            _ => page,
        };
        if next_page != page {
            self.clipboard_history_page = next_page;
            self.configure_clipboard_menu()?;
            self.redraw_clipboard_menu()?;
        }
        Ok(())
    }

    fn ensure_wifi_refresh_started(&mut self, rescan: bool) {
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

    fn start_wifi_refresh(&mut self, rescan: bool) {
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

    fn poll_wifi_refresh(&mut self) -> AnyResult<bool> {
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

    fn apply_wifi_refresh_result(&mut self, result: WifiRefreshResult) {
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

    fn connect_selected_wifi(&mut self) -> AnyResult<()> {
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

    fn disconnect_wifi(&mut self) -> AnyResult<()> {
        self.settings.wifi_status = Some("Disconnecting Wi-Fi...".to_string());
        self.settings.wifi_disconnect_confirm = false;
        self.redraw_settings()?;
        self.settings.wifi_status = match disconnect_current_wifi() {
            Ok(()) => Some("Wi-Fi disconnect requested".to_string()),
            Err(err) => Some(err),
        };
        self.redraw_settings()
    }

    fn handle_settings_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        if self.settings.tab == SettingsTab::Power && self.settings.auto_power_saver_editing {
            let sx = SIDEBAR_WIDTH + 24;
            let input_x = sx + 16;
            let inside_input = y >= 132 && y <= 162 && x >= input_x && x <= input_x + 118;
            if !inside_input {
                self.settings.auto_power_saver_editing = false;
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
            }
        }
        if x < SIDEBAR_WIDTH {
            if y < SETTINGS_SIDEBAR_TOP - 4 {
                return Ok(());
            }
            let tab = match (y - (SETTINGS_SIDEBAR_TOP - 4)) / 48 {
                0 => Some(SettingsTab::Display),
                1 => Some(SettingsTab::Power),
                2 => Some(SettingsTab::Wallpaper),
                3 => Some(SettingsTab::Audio),
                4 => Some(SettingsTab::Network),
                5 => Some(SettingsTab::Bluetooth),
                6 => Some(SettingsTab::Startup),
                7 => Some(SettingsTab::Apps),
                8 => Some(SettingsTab::About),
                _ => None,
            };
            if let Some(tab) = tab {
                self.settings.tab = tab;
                self.settings.scroll = 0;
                if tab == SettingsTab::Network {
                    self.ensure_wifi_refresh_started(false);
                }
                self.redraw_settings()?;
                self.redraw_topbar()?;
            }
            return Ok(());
        }

        match self.settings.tab {
            SettingsTab::Display => self.handle_display_click(x, y)?,
            SettingsTab::Power => self.handle_power_click(x, y)?,
            SettingsTab::Wallpaper => self.handle_wallpaper_click(y)?,
            SettingsTab::Audio => self.handle_audio_click(x, y)?,
            SettingsTab::Bluetooth if y >= 224 && y <= 300 => {
                self.spawn_first_available(&["blueman-manager", "bluetoothctl"], &[]);
            }
            SettingsTab::Apps => self.handle_apps_click(x, y)?,
            SettingsTab::Network => self.handle_network_click(x, y)?,
            SettingsTab::Bluetooth | SettingsTab::Startup => {}
            SettingsTab::About => {}
        }
        Ok(())
    }

    fn handle_settings_scroll(&mut self, button: u8, x: i32, y: i32) -> AnyResult<()> {
        if x <= SIDEBAR_WIDTH {
            return Ok(());
        }
        if self.settings.tab == SettingsTab::Network
            && self.handle_wifi_list_scroll(button, x, y)?
        {
            return Ok(());
        }
        let max_scroll = match self.settings.tab {
            SettingsTab::Network => {
                let lines = read_network_details().len();
                (lines.saturating_sub(4) * 24) as i32
            }
            SettingsTab::Startup | SettingsTab::About => 180,
            SettingsTab::Audio | SettingsTab::Wallpaper => 80,
            SettingsTab::Apps => self
                .available_apps(self.settings.app_kind)
                .len()
                .saturating_sub(6)
                .saturating_mul(29) as i32,
            SettingsTab::Display => 120,
            SettingsTab::Power | SettingsTab::Bluetooth => 40,
        };
        let old_scroll = self.settings.scroll;
        let step = if self.settings.tab == SettingsTab::Apps {
            29
        } else if self.settings.tab == SettingsTab::Network {
            24
        } else {
            36
        };
        if button == 4 {
            self.settings.scroll = self.settings.scroll.saturating_sub(step);
        } else {
            self.settings.scroll = (self.settings.scroll + step).min(max_scroll);
        }
        if self.settings.scroll == old_scroll {
            return Ok(());
        }
        self.redraw_settings()?;
        Ok(())
    }

    fn handle_wifi_list_scroll(&mut self, button: u8, x: i32, y: i32) -> AnyResult<bool> {
        if button != 4 && button != 5 {
            return Ok(false);
        }
        let sx = SIDEBAR_WIDTH + 24;
        let card_w = i32::from(self.settings_geometry().2) - sx - 24;
        let list_start_y = if self
            .settings
            .wifi_connected
            .as_ref()
            .is_some_and(Option::is_some)
        {
            376
        } else {
            344
        };
        let list_end_y = list_start_y + 5 * 24;
        if x < sx + 12 || x > sx + card_w - 12 || y < list_start_y || y >= list_end_y {
            return Ok(false);
        }
        let max_scroll = self.settings.wifi_networks.len().saturating_sub(5);
        let old_scroll = self.settings.wifi_scroll;
        if button == 4 {
            self.settings.wifi_scroll = self.settings.wifi_scroll.saturating_sub(1);
        } else {
            self.settings.wifi_scroll = (self.settings.wifi_scroll + 1).min(max_scroll);
        }
        if self.settings.wifi_scroll != old_scroll {
            self.redraw_settings()?;
        }
        Ok(true)
    }

    fn handle_display_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        let sx = SIDEBAR_WIDTH + 24;
        if x >= sx + 14 && x <= i32::from(self.settings_geometry().2) - 24 {
            for idx in 0..self.display_modes.len().min(4) {
                let row_y = 132 + idx as i32 * 25;
                if y >= row_y - 6 && y <= row_y + 18 {
                    self.settings.selected_mode = idx;
                    self.apply_display_mode(idx);
                    self.redraw_settings()?;
                    return Ok(());
                }
            }
        }
        let compositor_switch_x = i32::from(self.settings_geometry().2) - 78;
        if y >= 274 && y <= 302 && x >= compositor_switch_x && x <= compositor_switch_x + 40 {
            self.set_compositor_enabled(!self.settings.compositor_enabled)?;
            self.redraw_settings()?;
            return Ok(());
        }
        let bar_x = sx + 16;
        let bar_w = 230;
        if x >= bar_x && x <= bar_x + bar_w && (404..=432).contains(&y) {
            let percent = (10 + ((x - bar_x) * 90) / bar_w).clamp(10, 100) as u8;
            self.settings.brightness_percent = percent;
            self.settings.display_status = match apply_xrandr_brightness(
                &self.display,
                self.current_display_output(),
                percent,
            ) {
                Ok(()) => Some(format!("Brightness set to {percent}%")),
                Err(err) => Some(err),
            };
            save_app_commands(&self.settings)?;
            self.redraw_settings()?;
            return Ok(());
        }
        if y >= 509 && y <= 542 {
            if x >= sx + 16 && x <= sx + 50 {
                self.settings.sleep_after_secs =
                    self.settings.sleep_after_secs.saturating_sub(60).max(0);
                self.apply_sleep_timeout();
                save_app_commands(&self.settings)?;
                self.redraw_settings()?;
            } else if x >= sx + 174 && x <= sx + 212 {
                self.settings.sleep_after_secs = (self.settings.sleep_after_secs + 60).min(7200);
                self.apply_sleep_timeout();
                save_app_commands(&self.settings)?;
                self.redraw_settings()?;
            }
        }
        Ok(())
    }

    fn handle_audio_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        let sx = SIDEBAR_WIDTH + 24;
        let bar_x = sx + 16;
        let bar_w = 230;
        if x >= bar_x && x <= bar_x + bar_w && (132..=162).contains(&y) {
            let percent = (((x - bar_x) * 100) / bar_w).clamp(0, 100) as u8;
            self.settings.audio_status = match set_audio_volume_percent(percent) {
                Ok(()) => Some(format!("Volume set to {percent}%")),
                Err(err) => Some(err),
            };
            self.redraw_settings()?;
        }
        let card_w = i32::from(self.settings_geometry().2) - sx - 24;
        if x >= sx + 12 && x <= sx + card_w - 12 {
            for (idx, dev) in read_audio_devices(AudioDeviceKind::Output)
                .iter()
                .take(3)
                .enumerate()
            {
                let row_y = 260 + idx as i32 * 30;
                if y >= row_y - 3 && y <= row_y + 21 {
                    self.settings.audio_status =
                        match set_default_audio_device(AudioDeviceKind::Output, dev) {
                            Ok(()) => Some(format!("Output set to {}", dev.label)),
                            Err(err) => Some(err),
                        };
                    self.redraw_settings()?;
                    return Ok(());
                }
            }
            for (idx, dev) in read_audio_devices(AudioDeviceKind::Input)
                .iter()
                .take(2)
                .enumerate()
            {
                let row_y = 432 + idx as i32 * 30;
                if y >= row_y - 3 && y <= row_y + 21 {
                    self.settings.audio_status =
                        match set_default_audio_device(AudioDeviceKind::Input, dev) {
                            Ok(()) => Some(format!("Input set to {}", dev.label)),
                            Err(err) => Some(err),
                        };
                    self.redraw_settings()?;
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn handle_network_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        let sx = SIDEBAR_WIDTH + 24;
        let card_w = i32::from(self.settings_geometry().2) - sx - 24;

        // Clicks on the Refresh text button next to Wi-Fi title
        if y >= 273 && y <= 297 && x >= sx + 75 && x <= sx + 133 {
            self.settings.wifi_disconnect_confirm = false;
            self.start_wifi_refresh(true);
            self.redraw_settings()?;
            return Ok(());
        }

        // Clicks on the Disconnect text button next to Wi-Fi title
        if y >= 273 && y <= 297 && x >= sx + 141 && x <= sx + 219 {
            if self
                .settings
                .wifi_connected
                .as_ref()
                .is_some_and(Option::is_some)
            {
                self.settings.wifi_disconnect_confirm = true;
                self.settings.wifi_password_editing = false;
                self.redraw_settings()?;
                return Ok(());
            }
        }

        // Clicks on the new On/Off toggle switch next to Wi-Fi title
        if y >= 273 && y <= 297 && x >= sx + 227 && x <= sx + 267 {
            let current_enabled = self.settings.wifi_radio_enabled.unwrap_or(true);
            if let Err(e) = set_wifi_radio_enabled(!current_enabled) {
                self.settings.wifi_status = Some(format!("Error setting Wi-Fi radio: {e}"));
            } else {
                self.settings.wifi_status = Some(format!(
                    "Wi-Fi turned {}",
                    if !current_enabled { "on" } else { "off" }
                ));
                self.settings.wifi_radio_enabled = Some(!current_enabled);
                if !current_enabled {
                    self.start_wifi_refresh(true);
                } else {
                    self.wifi_refresh_rx = None;
                    self.settings.wifi_networks.clear();
                    self.settings.wifi_scroll = 0;
                    self.settings.wifi_selected = None;
                    self.settings.wifi_connected = Some(None);
                }
            }
            self.redraw_settings()?;
            return Ok(());
        }

        if self.settings.wifi_disconnect_confirm {
            let list_start_y = if self
                .settings
                .wifi_connected
                .as_ref()
                .is_some_and(Option::is_some)
            {
                376
            } else {
                344
            };
            if y >= list_start_y + 8
                && y <= list_start_y + 36
                && x >= sx + card_w - 194
                && x <= sx + card_w - 118
            {
                self.settings.wifi_disconnect_confirm = false;
                self.redraw_settings()?;
                return Ok(());
            }
            if y >= list_start_y + 8
                && y <= list_start_y + 36
                && x >= sx + card_w - 108
                && x <= sx + card_w - 20
            {
                self.disconnect_wifi()?;
                return Ok(());
            }
            self.settings.wifi_disconnect_confirm = false;
            self.redraw_settings()?;
            return Ok(());
        }

        // Dynamic Wi-Fi list coordinates
        let list_start_y = if self
            .settings
            .wifi_connected
            .as_ref()
            .is_some_and(Option::is_some)
        {
            376
        } else {
            344
        };
        let list_end_y = list_start_y + 5 * 24;
        if y >= list_start_y && y < list_end_y {
            let idx = self.settings.wifi_scroll + ((y - list_start_y) / 24) as usize;
            if let Some(network) = self.settings.wifi_networks.get(idx) {
                self.settings.wifi_selected = Some(network.ssid.clone());
                self.settings.wifi_password.clear();
                self.settings.wifi_password_editing = true;
                self.settings.wifi_disconnect_confirm = false;
                self.settings.wifi_status = Some(format!("Selected {}", network.ssid));
                self.conn.set_input_focus(
                    InputFocus::POINTER_ROOT,
                    self.ui.settings,
                    CURRENT_TIME,
                )?;
                self.redraw_settings()?;
                return Ok(());
            }
        }

        if self.settings.wifi_selected.is_some() {
            let input_x = sx + 16;
            let button_w = 34; // Updated button_w to 34
            let gap = 12;
            let input_w = (card_w - 32 - button_w - gap).max(132);
            let input_y = 508;
            let inside_input =
                y >= input_y && y <= input_y + 34 && x >= input_x && x <= input_x + input_w;
            let button_x = input_x + input_w + gap;
            let inside_button =
                y >= input_y && y <= input_y + 34 && x >= button_x && x <= button_x + button_w;

            if inside_input {
                self.settings.wifi_password_editing = true;
                self.conn.set_input_focus(
                    InputFocus::POINTER_ROOT,
                    self.ui.settings,
                    CURRENT_TIME,
                )?;
                self.redraw_settings()?;
            } else if inside_button {
                self.connect_selected_wifi()?;
            } else if self.settings.wifi_password_editing {
                self.settings.wifi_password_editing = false;
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
                self.redraw_settings()?;
            }
        }
        Ok(())
    }

    fn handle_power_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        let width = i32::from(self.settings_geometry().2);
        let switch_x = width - 78;
        if y >= 98 && y <= 122 && x >= switch_x && x <= switch_x + 40 {
            self.settings.auto_power_saver_enabled = !self.settings.auto_power_saver_enabled;
            self.settings.auto_power_saver_editing = false;
            self.pending_auto_power_saver_apply = None;
            self.last_pointer_activity = Instant::now();
            self.last_pointer_pos = None;
            if self.settings.auto_power_saver_enabled && self.settings.auto_power_saver_minutes == 0
            {
                self.settings.auto_power_saver_minutes = 10;
                self.settings.auto_power_saver_input = "10".to_string();
            }
            if self.settings.auto_power_saver_enabled && self.settings.auto_power_saver_minutes > 0
            {
                touch_notidle_marker()?;
                self.set_power_mode(PowerMode::Performance)?;
            }
            save_app_commands(&self.settings)?;
            self.redraw_settings()?;
            return Ok(());
        }
        let sx = SIDEBAR_WIDTH + 24;
        let input_x = sx + 16;
        if y >= 132 && y <= 162 && x >= input_x && x <= input_x + 118 {
            self.settings.auto_power_saver_editing = true;
            self.settings.auto_power_saver_input.clear();
            self.conn
                .set_input_focus(InputFocus::POINTER_ROOT, self.ui.settings, CURRENT_TIME)?;
            self.redraw_settings()?;
            return Ok(());
        }
        let modes = [
            PowerMode::Saver,
            PowerMode::Balanced,
            PowerMode::Performance,
        ];
        for (idx, mode) in modes.iter().enumerate() {
            let row_y = 202 + idx as i32 * 24;
            if y >= row_y - 7 && y <= row_y + 18 {
                self.settings.auto_power_saver_editing = false;
                self.pending_auto_power_saver_apply = None;
                self.set_power_mode(*mode)?;
                self.redraw_settings()?;
                return Ok(());
            }
        }
        Ok(())
    }

    fn handle_wallpaper_click(&mut self, y: i32) -> AnyResult<()> {
        for idx in 0..WALLPAPERS.len() {
            let row_y = 88 + idx as i32 * 116;
            if y >= row_y && y <= row_y + 94 {
                if idx == self.wallpaper_index {
                    return Ok(());
                }
                self.wallpaper_index = idx;
                if self.wallpaper_cache[idx].is_none() {
                    self.wallpaper_cache[idx] = Some(render_wallpaper_pixels(
                        WALLPAPERS[idx].bytes,
                        self.screen_width,
                        self.screen_height,
                    )?);
                }
                if let Some(pixels) = self.wallpaper_cache[idx].as_ref() {
                    self.wallpaper_pixels.clone_from(pixels);
                }
                self.redraw_everything()?;
                return Ok(());
            }
        }
        Ok(())
    }

    fn handle_apps_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        let sx = SIDEBAR_WIDTH + 24;
        let card_w = i32::from(self.settings_geometry().2) - sx - 24;
        if x < sx || x > sx + card_w {
            return Ok(());
        }
        let kinds = [
            DefaultAppKind::Terminal,
            DefaultAppKind::Browser,
            DefaultAppKind::Photo,
            DefaultAppKind::Video,
        ];
        if (84..=118).contains(&y) {
            let item_w = (card_w - 6) / 4;
            let idx = ((x - sx) / item_w).clamp(0, 3) as usize;
            if let Some(kind) = kinds.get(idx) {
                self.settings.app_kind = *kind;
                self.settings.scroll = 0;
                self.settings.terminal_editing = false;
                self.settings.app_status = None;
                self.redraw_settings()?;
            }
            return Ok(());
        }
        let apps = self.available_apps(self.settings.app_kind).to_vec();
        let start = (self.settings.scroll / 29).max(0) as usize;
        for (idx, app) in apps.iter().skip(start).take(6).enumerate() {
            let row_y = 180 + idx as i32 * 29;
            if y >= row_y - 5 && y <= row_y + 19 {
                self.set_selected_app_command(self.settings.app_kind, app.command.clone());
                save_app_commands(&self.settings)?;
                if self.settings.app_kind == DefaultAppKind::Terminal {
                    self.test_terminal_launch(&app.command, &app.name);
                } else {
                    self.settings.app_status = Some(format!("{} set as default.", app.name));
                }
                self.settings.terminal_editing = false;
                self.redraw_settings()?;
                return Ok(());
            }
        }
        if self.settings.app_kind == DefaultAppKind::Terminal && (426..=458).contains(&y) {
            self.settings.terminal_command.clear();
            self.settings.terminal_editing = true;
            self.settings.app_status = None;
            self.conn
                .set_input_focus(InputFocus::POINTER_ROOT, self.ui.settings, CURRENT_TIME)?;
            self.redraw_settings()?;
        }
        Ok(())
    }

    fn handle_key_press(&mut self, ev: KeyPressEvent) -> AnyResult<()> {
        self.last_pointer_activity = Instant::now();
        if let Some(forward) = self.alt_tab_direction(&ev)? {
            self.switch_running_app(forward)?;
            return Ok(());
        }
        self.reset_alt_tab_sequence();
        if let Some(forward) = self.is_workspace_switch_key(&ev)? {
            let current = self.active_workspace;
            let count = self.workspace_count;
            if forward {
                if current + 1 < count {
                    self.switch_workspace(current + 1)?;
                }
            } else {
                if current > 0 {
                    self.switch_workspace(current - 1)?;
                }
            }
            return Ok(());
        }
        if ev.event == self.ui.folder_terminal && self.folder_terminal.visible {
            self.handle_folder_terminal_key(ev)?;
            return Ok(());
        }
        if let Some(slot) = self.media_slot_for_window(ev.event) {
            self.handle_media_key(slot, ev)?;
            return Ok(());
        }
        if ev.event != self.ui.settings {
            return Ok(());
        }
        let mapping = self.conn.get_keyboard_mapping(ev.detail, 1)?.reply()?;
        let shifted = u16::from(ev.state) & u16::from(KeyButMask::SHIFT) != 0;
        let column = if shifted && mapping.keysyms_per_keycode > 1 {
            1
        } else {
            0
        };
        let Some(&keysym) = mapping.keysyms.get(column) else {
            return Ok(());
        };
        if self.settings.tab == SettingsTab::Network && self.settings.wifi_password_editing {
            match keysym {
                0xff08 => {
                    self.settings.wifi_password.pop();
                }
                0xff0d => {
                    self.connect_selected_wifi()?;
                    return Ok(());
                }
                0xff1b => {
                    self.settings.wifi_password.clear();
                    self.settings.wifi_password_editing = false;
                    self.conn
                        .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
                }
                0x20..=0x7e if self.settings.wifi_password.len() < 128 => {
                    self.settings
                        .wifi_password
                        .push(char::from_u32(keysym).unwrap());
                }
                _ => return Ok(()),
            }
            self.redraw_settings()?;
            return Ok(());
        }
        if self.settings.tab == SettingsTab::Power && self.settings.auto_power_saver_editing {
            let mut changed = false;
            match keysym {
                0xff08 => {
                    self.settings.auto_power_saver_input.pop();
                    changed = true;
                }
                0xff0d => {
                    self.settings.auto_power_saver_editing = false;
                    self.conn
                        .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
                }
                0xff1b => {
                    self.settings.auto_power_saver_input =
                        self.settings.auto_power_saver_minutes.to_string();
                    self.settings.auto_power_saver_editing = false;
                    self.pending_auto_power_saver_apply = None;
                    self.conn
                        .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
                }
                0x30..=0x39 if self.settings.auto_power_saver_input.len() < 3 => {
                    let digit = char::from_u32(keysym).unwrap();
                    if self.settings.auto_power_saver_input == "0" {
                        self.settings.auto_power_saver_input.clear();
                    }
                    self.settings.auto_power_saver_input.push(digit);
                    changed = true;
                }
                _ => return Ok(()),
            }
            if changed {
                self.settings.auto_power_saver_minutes = self
                    .settings
                    .auto_power_saver_input
                    .trim()
                    .parse::<u32>()
                    .unwrap_or(0)
                    .min(240);
                if self.settings.auto_power_saver_input.is_empty() {
                    self.settings.auto_power_saver_minutes = 0;
                }
                self.pending_auto_power_saver_apply = Some(Instant::now() + Duration::from_secs(3));
            }
            self.redraw_settings()?;
            return Ok(());
        }
        if self.settings.tab != SettingsTab::Apps
            || self.settings.app_kind != DefaultAppKind::Terminal
            || !self.settings.terminal_editing
        {
            return Ok(());
        }
        match keysym {
            0xff08 => {
                self.settings.terminal_command.pop();
            }
            0xff0d => {
                save_app_commands(&self.settings)?;
                self.settings.terminal_editing = false;
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
                let command = self.settings.terminal_command.clone();
                self.test_terminal_launch(&command, &command);
            }
            0xff1b => {
                self.settings.terminal_command = read_app_command(DefaultAppKind::Terminal);
                self.settings.terminal_editing = false;
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
            }
            0x20..=0x7e if self.settings.terminal_command.len() < 200 => {
                self.settings
                    .terminal_command
                    .push(char::from_u32(keysym).unwrap());
            }
            _ => return Ok(()),
        }
        self.redraw_settings()?;
        Ok(())
    }

    fn handle_key_release(&mut self, ev: KeyReleaseEvent) -> AnyResult<()> {
        let mapping = self.conn.get_keyboard_mapping(ev.detail, 1)?.reply()?;
        if mapping.keysyms.contains(&0xffe9) || mapping.keysyms.contains(&0xffea) {
            self.reset_alt_tab_sequence();
        }
        Ok(())
    }

    fn alt_tab_direction(&self, ev: &KeyPressEvent) -> AnyResult<Option<bool>> {
        let is_alt = u16::from(ev.state) & u16::from(KeyButMask::MOD1) != 0;
        let is_shift = u16::from(ev.state) & u16::from(KeyButMask::SHIFT) != 0;
        if !is_alt {
            return Ok(None);
        }
        let mapping = self.conn.get_keyboard_mapping(ev.detail, 1)?.reply()?;
        if mapping.keysyms.contains(&0xff09) {
            Ok(Some(!is_shift))
        } else {
            Ok(None)
        }
    }

    fn is_workspace_switch_key(&self, ev: &KeyPressEvent) -> AnyResult<Option<bool>> {
        if u16::from(ev.state) & u16::from(KeyButMask::MOD4) == 0 {
            return Ok(None);
        }
        let mapping = self.conn.get_keyboard_mapping(ev.detail, 1)?.reply()?;
        if mapping.keysyms.contains(&0xff51) {
            return Ok(Some(false)); // Left
        } else if mapping.keysyms.contains(&0xff53) {
            return Ok(Some(true)); // Right
        }
        Ok(None)
    }

    fn reset_alt_tab_sequence(&mut self) {
        self.alt_tab_index = 0;
        self.alt_tab_windows.clear();
    }

    fn alt_tab_start_window(
        &self,
        windows: &[Window],
        active: Option<Window>,
        forward: bool,
    ) -> Option<usize> {
        if windows.is_empty() {
            return None;
        }
        if let Some(previous) = self
            .focus_history
            .iter()
            .rev()
            .copied()
            .find(|&window| Some(window) != active && windows.contains(&window))
        {
            return windows.iter().position(|&window| window == previous);
        }
        active
            .and_then(|window| windows.iter().position(|&candidate| candidate == window))
            .map(|pos| {
                if forward {
                    (pos + 1) % windows.len()
                } else {
                    (pos + windows.len() - 1) % windows.len()
                }
            })
            .or(Some(0))
    }

    fn build_alt_tab_sequence(
        &self,
        windows: &[Window],
        active: Option<Window>,
        forward: bool,
    ) -> Vec<Window> {
        let Some(start) = self.alt_tab_start_window(windows, active, forward) else {
            return Vec::new();
        };
        (0..windows.len())
            .map(|offset| windows[(start + offset) % windows.len()])
            .collect()
    }

    fn switch_running_app(&mut self, forward: bool) -> AnyResult<()> {
        let windows = self.task_client_windows();
        if windows.is_empty() {
            self.reset_alt_tab_sequence();
            return Ok(());
        }
        let active = self.active_client;
        let needs_new_sequence = self.alt_tab_windows.is_empty()
            || self.alt_tab_index >= self.alt_tab_windows.len()
            || self.alt_tab_windows.len() != windows.len()
            || active.is_some_and(|client| {
                self.alt_tab_windows.get(self.alt_tab_index) != Some(&client)
            })
            || self
                .alt_tab_windows
                .iter()
                .any(|window| !windows.contains(window));
        if needs_new_sequence {
            self.alt_tab_windows = self.build_alt_tab_sequence(&windows, active, forward);
            self.alt_tab_index = 0;
        } else {
            let len = self.alt_tab_windows.len();
            self.alt_tab_index = if forward {
                (self.alt_tab_index + 1) % len
            } else {
                (self.alt_tab_index + len - 1) % len
            };
        }
        let Some(&next) = self.alt_tab_windows.get(self.alt_tab_index) else {
            return Ok(());
        };
        self.focus_window(next)?;
        self.redraw_dock()?;
        self.conn.flush()?;
        Ok(())
    }

    fn handle_dock_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        let (_, _, w, h) = self.dock_geometry();
        let buttons = self.dock_button_count();
        let task_windows = self.task_client_windows();
        let stride = 58;
        let total = buttons as i32 * stride - 12;
        let mut bx = (i32::from(w) - total) / 2;
        let cy = i32::from(h) / 2;
        for i in 0..buttons {
            let rx = bx + 7;
            let ry = cy - 22;
            if x >= rx && x <= rx + 44 && y >= ry && y <= ry + 44 {
                if i == 0 {
                    self.dock_last_click = None;
                    self.hide_dock_more_menu()?;
                    self.toggle_app_menu()?;
                } else if i >= 5 {
                    self.hide_app_menu()?;
                    if i == 15 && task_windows.len() > 10 {
                        if self.dock_more_visible {
                            self.hide_dock_more_menu()?;
                        } else {
                            self.show_dock_more_menu()?;
                        }
                    } else if let Some(client) = task_windows.get(i - 5).copied() {
                        self.hide_dock_more_menu()?;
                        self.handle_task_icon_click(client)?;
                    }
                } else {
                    self.dock_last_click = None;
                    self.hide_app_menu()?;
                    self.hide_dock_more_menu()?;
                    if i == 1 {
                        self.show_folder(FolderMode::Pictures, true)?;
                    } else if i == 2 {
                        self.show_folder(FolderMode::Music, true)?;
                    } else if i == 3 {
                        self.show_folder(FolderMode::Videos, true)?;
                    } else if i == 4 {
                        self.settings_visible = !self.settings_visible;
                        if self.settings_visible {
                            self.settings_front = true;
                            self.folder_front = false;
                            self.media_front = false;
                            self.conn.map_window(self.ui.settings)?;
                            self.raise_ui()?;
                            self.redraw_settings()?;
                            self.redraw_topbar()?;
                        } else {
                            self.conn.unmap_window(self.ui.settings)?;
                            self.redraw_topbar()?;
                        }
                    }
                }
                return Ok(());
            }
            bx += stride;
        }
        self.hide_dock_more_menu()?;
        Ok(())
    }

    fn handle_task_icon_click(&mut self, client: Window) -> AnyResult<()> {
        let now = Instant::now();
        let double_click = self.dock_last_click.is_some_and(|last| {
            last.client == client && now.duration_since(last.at) <= Duration::from_millis(360)
        });
        self.dock_last_click = Some(DockClickState { client, at: now });
        if double_click {
            self.snap_client_top_center(client)?;
        } else {
            self.focus_window(client)?;
        }
        Ok(())
    }

    fn snap_client_top_center(&mut self, client: Window) -> AnyResult<()> {
        let Some(mut info) = self.clients.get(&client).copied() else {
            return Ok(());
        };
        info.x = ((self.screen_width.saturating_sub(info.width)) / 2) as i16;
        info.y = (TOPBAR_HEIGHT + 2) as i16;
        self.conn.configure_window(
            info.frame,
            &ConfigureWindowAux::new()
                .x(i32::from(info.x))
                .y(i32::from(info.y))
                .stack_mode(StackMode::ABOVE),
        )?;
        self.clients.insert(client, info);
        self.send_synthetic_configure(&info)?;
        self.focus_window(client)?;
        Ok(())
    }

    fn toggle_app_menu(&mut self) -> AnyResult<()> {
        self.app_menu_visible = !self.app_menu_visible;
        if self.app_menu_visible {
            self.hide_dock_more_menu()?;
            let menu = self.app_menu_geometry();
            self.conn.configure_window(
                self.ui.app_menu,
                &ConfigureWindowAux::new()
                    .x(i32::from(menu.0))
                    .y(i32::from(menu.1))
                    .width(u32::from(menu.2))
                    .height(u32::from(menu.3))
                    .stack_mode(StackMode::ABOVE),
            )?;
            self.conn.map_window(self.ui.app_menu)?;
            self.redraw_app_menu()?;
        } else {
            self.conn.unmap_window(self.ui.app_menu)?;
        }
        self.raise_ui()?;
        Ok(())
    }

    fn hide_app_menu(&mut self) -> AnyResult<()> {
        if self.app_menu_visible {
            self.app_menu_visible = false;
            self.conn.unmap_window(self.ui.app_menu)?;
        }
        Ok(())
    }

    fn redraw_dock_more_menu(&mut self) -> AnyResult<()> {
        let (x, y, w, h) = self.dock_more_menu_geometry();
        self.conn.configure_window(
            self.ui.dock_more_menu,
            &ConfigureWindowAux::new()
                .x(i32::from(x))
                .y(i32::from(y))
                .width(u32::from(w))
                .height(u32::from(h)),
        )?;
        let mut c = Canvas::from_wallpaper_crop(
            &self.wallpaper_pixels,
            self.screen_width,
            i32::from(x),
            i32::from(y),
            w,
            h,
        );
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            16,
            Color::rgba(248, 253, 255, 232),
        );
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            16,
            Color::rgba(214, 229, 237, 70),
        );

        let task_windows = self.task_client_windows();
        if task_windows.len() > 10 {
            let hidden_apps = &task_windows[10..];
            for (idx, &window) in hidden_apps.iter().enumerate() {
                let row_y = 8 + idx as i32 * 40;
                let active = self.active_client == Some(window);
                c.draw_round_rect(
                    8,
                    row_y,
                    i32::from(w) - 16,
                    32,
                    8,
                    if active {
                        Color::rgba(172, 218, 255, 180)
                    } else {
                        Color::rgba(255, 255, 255, 120)
                    },
                );

                let icon_x = 16;
                let icon_y = row_y + 2;
                let title = self.window_title(window);
                if !self.paint_window_icon(&mut c, window, icon_x, icon_y, 28)
                    && !self.paint_desktop_icon(&mut c, window, icon_x, icon_y, 28)
                {
                    let mapped = self
                        .clients
                        .get(&window)
                        .map(|info| info.mapped)
                        .unwrap_or(true);
                    draw_client_task_icon(
                        &mut c,
                        &self.bold,
                        icon_x + 14,
                        icon_y + 14,
                        mapped,
                        &title,
                    );
                }

                let text_x = 52;
                let text_y = row_y + 8;
                let display_title = compact(&title, 20);
                c.draw_text(&self.bold, &display_title, text_x, text_y, 12.0, INK);
            }
        }

        self.upload_canvas(self.ui.dock_more_menu, &c)?;
        Ok(())
    }

    fn show_dock_more_menu(&mut self) -> AnyResult<()> {
        self.dock_more_visible = true;
        let menu = self.dock_more_menu_geometry();
        self.conn.configure_window(
            self.ui.dock_more_menu,
            &ConfigureWindowAux::new()
                .x(i32::from(menu.0))
                .y(i32::from(menu.1))
                .width(u32::from(menu.2))
                .height(u32::from(menu.3))
                .stack_mode(StackMode::ABOVE),
        )?;
        self.conn.map_window(self.ui.dock_more_menu)?;
        self.redraw_dock_more_menu()?;
        self.redraw_dock()?;
        Ok(())
    }

    fn hide_dock_more_menu(&mut self) -> AnyResult<()> {
        if self.dock_more_visible {
            self.dock_more_visible = false;
            self.conn.unmap_window(self.ui.dock_more_menu)?;
            self.redraw_dock()?;
        }
        Ok(())
    }

    fn handle_dock_more_menu_click(&mut self, _x: i32, y: i32) -> AnyResult<()> {
        let task_windows = self.task_client_windows();
        if task_windows.len() > 10 {
            let hidden_apps = &task_windows[10..];
            let idx = (y - 8) / 40;
            if idx >= 0 && idx < hidden_apps.len() as i32 {
                let client = hidden_apps[idx as usize];
                self.handle_task_icon_click(client)?;
                self.hide_dock_more_menu()?;
            }
        }
        Ok(())
    }

    fn toggle_aurora_menu(&mut self) -> AnyResult<()> {
        self.aurora_menu_visible = !self.aurora_menu_visible;
        if self.aurora_menu_visible {
            self.hide_dock_more_menu()?;
            self.app_menu_visible = false;
            self.app_menu_more = false;
            self.app_menu_scroll = 0;
            let _ = self.conn.unmap_window(self.ui.app_menu);
            self.aurora_menu_about = false;
            self.aurora_menu_restart_confirm = false;
            let menu = self.aurora_menu_geometry();
            self.conn.configure_window(
                self.ui.aurora_menu,
                &ConfigureWindowAux::new()
                    .x(i32::from(menu.0))
                    .y(i32::from(menu.1))
                    .width(u32::from(menu.2))
                    .height(u32::from(menu.3))
                    .stack_mode(StackMode::ABOVE),
            )?;
            self.conn.map_window(self.ui.aurora_menu)?;
            self.redraw_aurora_menu()?;
        } else {
            self.conn.unmap_window(self.ui.aurora_menu)?;
        }
        self.raise_ui()?;
        Ok(())
    }

    fn hide_aurora_menu(&mut self) -> AnyResult<()> {
        if self.aurora_menu_visible {
            self.aurora_menu_visible = false;
            self.aurora_menu_about = false;
            self.aurora_menu_restart_confirm = false;
            self.conn.unmap_window(self.ui.aurora_menu)?;
        }
        Ok(())
    }

    fn handle_aurora_menu_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        if self.aurora_menu_about {
            if (16..=92).contains(&x) && (230..=258).contains(&y) {
                self.aurora_menu_about = false;
                self.redraw_aurora_menu()?;
            }
            return Ok(());
        }

        if self.aurora_menu_restart_confirm {
            if (110..=138).contains(&y) {
                if (160..=250).contains(&x) {
                    self.restart_aurora()?;
                } else if (270..=360).contains(&x) {
                    self.aurora_menu_restart_confirm = false;
                    self.redraw_aurora_menu()?;
                }
            } else if (158..=196).contains(&y) {
                self.aurora_menu_about = true;
                self.aurora_menu_restart_confirm = false;
                self.redraw_aurora_menu()?;
            }
        } else {
            if (56..=94).contains(&y) {
                self.aurora_menu_restart_confirm = true;
                self.redraw_aurora_menu()?;
            } else if (106..=144).contains(&y) {
                self.aurora_menu_about = true;
                self.redraw_aurora_menu()?;
            }
        }
        Ok(())
    }

    fn restart_aurora(&mut self) -> AnyResult<()> {
        save_app_commands(&self.settings)?;
        let exe = env::current_exe()?;
        let display = self.display.clone();
        let display_id = display.trim_start_matches(':').replace(['/', '.'], "_");
        let log_path = format!("/tmp/aurora-wm-display{display_id}.log");
        let script = format!(
            "sleep 0.35; exec {} > {} 2>&1",
            shell_quote(&exe),
            shell_quote_text(&log_path),
        );
        Command::new("setsid")
            .arg("sh")
            .arg("-c")
            .arg(script)
            .env("DISPLAY", display)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        process::exit(0);
    }

    fn show_folder(&mut self, mode: FolderMode, front: bool) -> AnyResult<()> {
        self.folder_mode = mode;
        self.folder_path = folder_path_for(mode);
        self.folder_entries = folder_entries_for(mode, self.folder_sort);
        self.folder_selected = None;
        self.folder_scroll = 0;
        self.folder_front = front;
        self.folder_more_open = false;
        self.folder_sort_open = false;
        self.sync_folder_terminal_cwd();
        if front {
            self.settings_front = false;
            self.media_front = false;
        }
        let folder = self.folder_geometry();
        let terminal = self.folder_terminal_geometry();
        self.conn.configure_window(
            self.ui.folder,
            &ConfigureWindowAux::new()
                .x(i32::from(folder.0))
                .y(i32::from(folder.1))
                .width(u32::from(folder.2))
                .height(u32::from(folder.3))
                .stack_mode(if front {
                    StackMode::ABOVE
                } else {
                    StackMode::BELOW
                }),
        )?;
        self.conn.configure_window(
            self.ui.folder_terminal,
            &ConfigureWindowAux::new()
                .x(i32::from(terminal.0))
                .y(i32::from(terminal.1))
                .width(u32::from(terminal.2))
                .height(u32::from(terminal.3)),
        )?;
        self.conn.map_window(self.ui.folder)?;
        self.redraw_folder()?;
        if self.folder_terminal.visible {
            self.redraw_folder_terminal()?;
        }
        self.raise_ui()?;
        Ok(())
    }

    fn handle_folder_click(&mut self, ev: ButtonPressEvent) -> AnyResult<()> {
        let x = i32::from(ev.event_x);
        let y = i32::from(ev.event_y);
        let (_, _, w, h) = self.folder_geometry();
        self.folder_press = None;
        if self.choose_file_mode {
            let cancel_x = i32::from(w) - 190;
            let choose_x = i32::from(w) - 100;
            let btn_y = i32::from(h) - 46;
            if y >= btn_y && y <= btn_y + 32 {
                if x >= cancel_x && x <= cancel_x + 80 {
                    self.cancel_choose_file()?;
                    return Ok(());
                }
                if x >= choose_x && x <= choose_x + 80 {
                    self.submit_choose_file()?;
                    return Ok(());
                }
            }
        }
        if self.folder_sort_open {
            if let Some(sort) = self.folder_sort_at(x, y) {
                self.folder_sort = sort;
                self.folder_sort_open = false;
                self.refresh_folder_entries();
                self.folder_info = Some(format!("Sorted by {}", sort.label().to_lowercase()));
                self.redraw_folder()?;
                return Ok(());
            }
            self.folder_sort_open = false;
        }
        if self.folder_context_open {
            if let Some(action) = self.folder_context_action_at(x, y) {
                self.run_folder_context_action(action)?;
                self.folder_context_open = false;
                self.redraw_folder()?;
                return Ok(());
            }
            self.folder_context_open = false;
        }
        if (18..=48).contains(&x) && (18..=48).contains(&y) {
            self.folder_mode = FolderMode::Home;
            self.folder_path = folder_path_for(FolderMode::Home);
            self.folder_entries = folder_entries_for(FolderMode::Home, self.folder_sort);
            self.folder_selected = None;
            self.folder_scroll = 0;
            self.folder_more_open = false;
            self.folder_sort_open = false;
            self.folder_info = None;
            self.sync_folder_terminal_cwd();
            self.redraw_folder()?;
            if self.folder_terminal.visible {
                self.redraw_folder_terminal()?;
            }
            return Ok(());
        }
        if (56..=86).contains(&x) && (18..=48).contains(&y) {
            self.toggle_folder_terminal()?;
            return Ok(());
        }
        if (94..=124).contains(&x) && (18..=48).contains(&y) {
            self.folder_sort_open = !self.folder_sort_open;
            self.folder_more_open = false;
            self.redraw_folder()?;
            return Ok(());
        }
        if x >= 58 && x <= i32::from(w) - 58 && (36..=60).contains(&y) {
            copy_text_to_clipboard(&self.folder_path.to_string_lossy());
            self.folder_info = Some("Path copied to clipboard".to_string());
            self.redraw_folder()?;
            return Ok(());
        }
        if x >= i32::from(w) - 50 && x <= i32::from(w) - 20 && (18..=48).contains(&y) {
            self.folder_more_open = !self.folder_more_open;
            self.redraw_folder()?;
            return Ok(());
        }
        if self.folder_more_open {
            let menu_x = i32::from(w) - 214;
            for (idx, place) in self.folder_places.iter().take(6).enumerate() {
                let row_y = 94 + idx as i32 * 28;
                if x >= menu_x + 8 && x <= menu_x + 186 && y >= row_y - 5 && y <= row_y + 18 {
                    self.folder_mode = FolderMode::Home;
                    self.folder_path = place.path.clone();
                    self.folder_entries = folder_entries_in(place.path.clone(), self.folder_sort);
                    self.folder_selected = None;
                    self.folder_scroll = 0;
                    self.folder_more_open = false;
                    self.sync_folder_terminal_cwd();
                    self.redraw_folder()?;
                    if self.folder_terminal.visible {
                        self.redraw_folder_terminal()?;
                    }
                    return Ok(());
                }
            }
        }
        self.folder_more_open = false;
        if y < 86 {
            self.folder_info = None;
            self.redraw_folder()?;
            return Ok(());
        }
        let idx = (y - 86) / 42;
        if idx < 0 || idx as usize >= self.folder_visible_row_count() {
            self.folder_info = None;
            self.redraw_folder()?;
            return Ok(());
        }
        let Some(entry) = self
            .folder_entries
            .get(self.folder_scroll + idx as usize)
            .cloned()
        else {
            self.redraw_folder()?;
            return Ok(());
        };
        self.folder_drag = Some(entry.path.clone());
        self.folder_press = Some(FolderPress {
            entry: entry.clone(),
            root_x: ev.root_x,
            root_y: ev.root_y,
        });
        match entry.kind {
            FileKind::Directory => {
                self.folder_selected = Some(entry.path.clone());
                self.folder_info = Some(folder_entry_info(&entry));
                self.redraw_folder()?;
            }
            FileKind::Text
            | FileKind::Image
            | FileKind::Audio
            | FileKind::Video
            | FileKind::Other => {
                if self.folder_selected.as_ref() == Some(&entry.path) {
                    if self.choose_file_mode {
                        self.submit_choose_file()?;
                    } else {
                        self.open_media(entry)?;
                    }
                } else {
                    self.folder_selected = Some(entry.path.clone());
                    self.folder_info = Some(folder_entry_info(&entry));
                    self.redraw_folder()?;
                }
            }
        }
        Ok(())
    }

    fn handle_folder_release(&mut self, ev: ButtonReleaseEvent) -> AnyResult<()> {
        let press = self.folder_press.take();
        let Some(path) = self.folder_drag.take() else {
            return Ok(());
        };
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        let mut target = pointer.child;
        let moved = press.as_ref().is_some_and(|press| {
            (i32::from(ev.root_x) - i32::from(press.root_x)).abs() > 6
                || (i32::from(ev.root_y) - i32::from(press.root_y)).abs() > 6
        });
        if target == self.ui.folder && !moved {
            if let Some(press) = press.filter(|press| {
                press.entry.kind == FileKind::Directory
                    || self.folder_selected.as_ref() != Some(&press.entry.path)
            }) {
                self.activate_folder_entry(press.entry)?;
            }
            return Ok(());
        }
        if target == self.ui.folder_terminal {
            self.ensure_folder_terminal_pty();
            self.folder_terminal.focused = true;
            self.conn.set_input_focus(
                InputFocus::POINTER_ROOT,
                self.ui.folder_terminal,
                CURRENT_TIME,
            )?;
            self.write_folder_terminal(shell_quote(&path).as_bytes());
            self.redraw_folder_terminal()?;
            return Ok(());
        }
        if target == x11rb::NONE || target == self.ui.folder || self.is_ui_window(target) {
            return Ok(());
        }
        if let Some(client) = self.client_key_for(target) {
            if let Some(info) = self.clients.get(&client) {
                target = info.window;
            }
        }
        self.folder_drag = Some(path);
        let selection = self.atom(b"XdndSelection")?;
        self.conn
            .set_selection_owner(self.ui.folder, selection, CURRENT_TIME)?;
        let xdnd_enter = self.atom(b"XdndEnter")?;
        let xdnd_position = self.atom(b"XdndPosition")?;
        let xdnd_drop = self.atom(b"XdndDrop")?;
        let uri = self.atom(b"text/uri-list")?;
        let action_copy = self.atom(b"XdndActionCopy")?;
        let packed_xy =
            ((u32::from(pointer.root_x as u16)) << 16) | u32::from(pointer.root_y as u16);
        self.conn.send_event(
            false,
            target,
            EventMask::NO_EVENT,
            ClientMessageEvent::new(32, target, xdnd_enter, [self.ui.folder, 5 << 24, uri, 0, 0]),
        )?;
        self.conn.send_event(
            false,
            target,
            EventMask::NO_EVENT,
            ClientMessageEvent::new(
                32,
                target,
                xdnd_position,
                [self.ui.folder, 0, packed_xy, CURRENT_TIME, action_copy],
            ),
        )?;
        self.conn.send_event(
            false,
            target,
            EventMask::NO_EVENT,
            ClientMessageEvent::new(
                32,
                target,
                xdnd_drop,
                [self.ui.folder, 0, CURRENT_TIME, 0, 0],
            ),
        )?;
        Ok(())
    }

    fn activate_folder_entry(&mut self, entry: FolderEntry) -> AnyResult<()> {
        match entry.kind {
            FileKind::Directory => {
                self.folder_path = entry.path.clone();
                self.folder_entries = folder_entries_in(entry.path, self.folder_sort);
                self.folder_selected = None;
                self.folder_info = None;
                self.folder_scroll = 0;
                self.sync_folder_terminal_cwd();
                self.redraw_folder()?;
                if self.folder_terminal.visible {
                    self.redraw_folder_terminal()?;
                }
            }
            FileKind::Text
            | FileKind::Image
            | FileKind::Audio
            | FileKind::Video
            | FileKind::Other => {
                if self.folder_selected.as_ref() == Some(&entry.path) {
                    if self.choose_file_mode {
                        self.submit_choose_file()?;
                    } else {
                        self.open_media(entry)?;
                    }
                } else {
                    self.folder_selected = Some(entry.path.clone());
                    self.folder_info = Some(folder_entry_info(&entry));
                    self.redraw_folder()?;
                }
            }
        }
        Ok(())
    }

    fn handle_folder_context(&mut self, x: i32, y: i32) -> AnyResult<()> {
        if y >= 86 {
            let idx = (y - 86) / 42;
            if idx >= 0 && (idx as usize) < self.folder_visible_row_count() {
                if let Some(entry) = self.folder_entries.get(self.folder_scroll + idx as usize) {
                    self.folder_selected = Some(entry.path.clone());
                }
            }
        }
        self.folder_context_open = true;
        self.folder_context_pos = (x, y);
        self.folder_more_open = false;
        self.folder_sort_open = false;
        self.redraw_folder()?;
        Ok(())
    }

    fn handle_folder_scroll(&mut self, button: u8) -> AnyResult<()> {
        let max_scroll = self
            .folder_entries
            .len()
            .saturating_sub(self.folder_visible_row_count());
        let old_scroll = self.folder_scroll;
        if button == 4 {
            self.folder_scroll = self.folder_scroll.saturating_sub(3);
        } else {
            self.folder_scroll = (self.folder_scroll + 3).min(max_scroll);
        }
        if self.folder_scroll == old_scroll {
            return Ok(());
        }
        self.redraw_folder()?;
        Ok(())
    }

    fn folder_sort_at(&self, x: i32, y: i32) -> Option<FolderSort> {
        let menu_x = 94;
        let menu_y = 54;
        if x < menu_x || x > menu_x + 122 || y < menu_y || y > menu_y + 96 {
            return None;
        }
        let idx = (y - menu_y - 8) / 28;
        match idx {
            0 => Some(FolderSort::Name),
            1 => Some(FolderSort::Date),
            2 => Some(FolderSort::Size),
            _ => None,
        }
    }

    fn refresh_folder_entries(&mut self) -> bool {
        let previous_entries = self.folder_entries.clone();
        let previous_scroll = self.folder_scroll;
        let previous_selected = self.folder_selected.clone();
        let anchor = self
            .folder_entries
            .get(self.folder_scroll)
            .map(|entry| entry.path.clone());

        let new_entries = self.current_folder_entries();
        if new_entries == self.folder_entries {
            self.clamp_folder_scroll();
            return self.folder_scroll != previous_scroll
                || self.folder_selected != previous_selected;
        }

        self.folder_entries = new_entries;
        if let Some(anchor) = anchor {
            if let Some(idx) = self
                .folder_entries
                .iter()
                .position(|entry| entry.path == anchor)
            {
                self.folder_scroll = idx;
            }
        }
        self.clamp_folder_scroll();
        self.folder_selected = self
            .folder_selected
            .take()
            .filter(|path| self.folder_entries.iter().any(|entry| &entry.path == path));

        self.folder_entries != previous_entries
            || self.folder_scroll != previous_scroll
            || self.folder_selected != previous_selected
    }

    fn current_folder_entries(&self) -> Vec<FolderEntry> {
        if self.folder_path == folder_path_for(self.folder_mode) {
            folder_entries_for(self.folder_mode, self.folder_sort)
        } else {
            folder_entries_in(self.folder_path.clone(), self.folder_sort)
        }
    }

    fn clamp_folder_scroll(&mut self) {
        self.folder_scroll = self.folder_scroll.min(
            self.folder_entries
                .len()
                .saturating_sub(self.folder_visible_row_count()),
        );
    }

    fn folder_visible_row_count(&self) -> usize {
        if self.choose_file_mode { 7 } else { 9 }
    }

    fn sync_folder_terminal_cwd(&mut self) {
        self.folder_terminal.cwd = self.folder_path.clone();
        if self.folder_terminal.master_fd.is_some() {
            let command = format!("cd {}\n", shell_quote(&self.folder_path));
            self.write_folder_terminal(command.as_bytes());
        }
    }

    fn sync_folder_to_terminal_cwd(&mut self) -> AnyResult<bool> {
        let Some(pid) = self.folder_terminal.child_pid else {
            return Ok(false);
        };
        let Ok(cwd) = fs::read_link(format!("/proc/{pid}/cwd")) else {
            return Ok(false);
        };
        if cwd == self.folder_terminal.cwd && cwd == self.folder_path {
            return Ok(false);
        }
        self.folder_terminal.cwd = cwd.clone();
        if cwd == self.folder_path || !cwd.is_dir() {
            if self.folder_terminal.visible {
                self.redraw_folder_terminal()?;
            }
            return Ok(false);
        }
        self.folder_mode = FolderMode::Home;
        self.folder_path = cwd;
        self.folder_entries = folder_entries_in(self.folder_path.clone(), self.folder_sort);
        self.folder_selected = None;
        self.folder_scroll = 0;
        self.folder_more_open = false;
        self.folder_sort_open = false;
        self.redraw_folder()?;
        if self.folder_terminal.visible {
            self.redraw_folder_terminal()?;
        }
        Ok(true)
    }

    fn set_topbar_notice(&mut self, message: &str, duration: Duration) -> AnyResult<()> {
        self.topbar_notice = Some((message.to_string(), Instant::now() + duration));
        self.redraw_topbar()?;
        Ok(())
    }

    fn handle_topbar_release(&mut self, _ev: ButtonReleaseEvent) -> AnyResult<()> {
        self.pending_screenshot_button = None;
        Ok(())
    }

    fn toggle_folder_terminal(&mut self) -> AnyResult<()> {
        self.folder_terminal.visible = !self.folder_terminal.visible;
        self.folder_terminal.focused = self.folder_terminal.visible;
        if self.folder_terminal.visible {
            self.ensure_folder_terminal_pty();
            self.sync_folder_terminal_cwd();
            let terminal = self.folder_terminal_geometry();
            self.conn.configure_window(
                self.ui.folder_terminal,
                &ConfigureWindowAux::new()
                    .x(i32::from(terminal.0))
                    .y(i32::from(terminal.1))
                    .width(u32::from(terminal.2))
                    .height(u32::from(terminal.3))
                    .stack_mode(StackMode::ABOVE),
            )?;
            self.conn.map_window(self.ui.folder_terminal)?;
            self.conn.set_input_focus(
                InputFocus::POINTER_ROOT,
                self.ui.folder_terminal,
                CURRENT_TIME,
            )?;
            self.redraw_folder_terminal()?;
        } else {
            self.conn.unmap_window(self.ui.folder_terminal)?;
            self.conn
                .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
        }
        self.redraw_folder()?;
        self.raise_ui()?;
        Ok(())
    }

    fn toggle_screenshot_mode(&mut self) -> AnyResult<()> {
        if self.screenshot_mode {
            self.capture_screenshot(None)?;
        } else {
            self.screenshot_mode = true;
            self.screenshot_selection = None;
            self.screenshot_base = capture_screen_preview(&self.conn, self.root);
            self.set_topbar_notice(
                "Drag to pick area. Hold camera 2s for full screen.",
                Duration::from_secs(4),
            )?;
            self.conn.configure_window(
                self.ui.screenshot_overlay,
                &ConfigureWindowAux::new()
                    .x(0)
                    .y(0)
                    .width(u32::from(self.screen_width))
                    .height(u32::from(self.screen_height))
                    .stack_mode(StackMode::ABOVE),
            )?;
            self.conn.map_window(self.ui.screenshot_overlay)?;
            self.redraw_screenshot_overlay()?;
            self.raise_ui()?;
        }
        Ok(())
    }

    fn start_screenshot_selection(&mut self, root_x: i16, root_y: i16) -> AnyResult<()> {
        self.erase_screenshot_live_rect()?;
        self.screenshot_selection = Some(ScreenshotSelection {
            start_x: root_x,
            start_y: root_y,
            current_x: root_x,
            current_y: root_y,
        });
        let _ = self
            .conn
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
            .reply();
        self.update_screenshot_live_rect()?;
        Ok(())
    }

    fn finish_screenshot_selection(&mut self, root_x: i16, root_y: i16) -> AnyResult<()> {
        self.erase_screenshot_live_rect()?;
        let Some(selection) = self.screenshot_selection.take() else {
            return Ok(());
        };
        let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        let x1 = i32::from(selection.start_x).min(i32::from(root_x)).max(0);
        let y1 = i32::from(selection.start_y).min(i32::from(root_y)).max(0);
        let x2 = i32::from(selection.start_x)
            .max(i32::from(root_x))
            .min(i32::from(self.screen_width));
        let y2 = i32::from(selection.start_y)
            .max(i32::from(root_y))
            .min(i32::from(self.screen_height));
        if x2 - x1 >= 8 && y2 - y1 >= 8 {
            self.capture_screenshot(Some((x1, y1, x2 - x1, y2 - y1)))?;
        } else {
            self.screenshot_mode = false;
            self.screenshot_base = None;
            self.conn.unmap_window(self.ui.screenshot_overlay)?;
            self.set_topbar_notice("Screenshot cancelled", Duration::from_secs(2))?;
        }
        Ok(())
    }

    fn erase_screenshot_live_rect(&mut self) -> AnyResult<()> {
        if let Some((x, y, w, h)) = self.screenshot_live_rect.take() {
            let rects = selection_border_rects(x, y, w, h);
            self.draw_xor_rects(self.ui.screenshot_overlay, &rects)?;
        }
        Ok(())
    }

    fn update_screenshot_live_rect(&mut self) -> AnyResult<()> {
        let Some(selection) = self.screenshot_selection else {
            return Ok(());
        };
        let x1 = i32::from(selection.start_x)
            .min(i32::from(selection.current_x))
            .max(0);
        let y1 = i32::from(selection.start_y)
            .min(i32::from(selection.current_y))
            .max(0);
        let x2 = i32::from(selection.start_x)
            .max(i32::from(selection.current_x))
            .min(i32::from(self.screen_width));
        let y2 = i32::from(selection.start_y)
            .max(i32::from(selection.current_y))
            .min(i32::from(self.screen_height));
        let w = (x2 - x1).max(10) as u16;
        let h = (y2 - y1).max(10) as u16;
        let x = (x1.min(i32::from(self.screen_width.saturating_sub(w)))) as i16;
        let y = (y1.min(i32::from(self.screen_height.saturating_sub(h)))) as i16;
        let next = (x, y, w, h);
        if self.screenshot_live_rect == Some(next) {
            return Ok(());
        }
        self.erase_screenshot_live_rect()?;
        self.screenshot_live_rect = Some(next);
        let rects = selection_border_rects(x, y, w, h);
        self.draw_xor_rects(self.ui.screenshot_overlay, &rects)?;
        Ok(())
    }

    fn capture_screenshot(&mut self, rect: Option<(i32, i32, i32, i32)>) -> AnyResult<()> {
        self.screenshot_mode = false;
        self.screenshot_selection = None;
        self.screenshot_base = None;
        self.screenshot_live_rect = None;
        let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        self.conn.unmap_window(self.ui.screenshot_overlay)?;
        self.conn.flush()?;
        let desktop = home_dir().join("Desktop");
        let _ = fs::create_dir_all(&desktop);
        let path = desktop.join(format!(
            "Aurora Screenshot {}.png",
            OffsetDateTime::now_utc().unix_timestamp()
        ));
        let (x, y, w, h) = rect.unwrap_or((
            0,
            0,
            i32::from(self.screen_width),
            i32::from(self.screen_height),
        ));
        let Ok((pixels, width, height)) = capture_root_rgba(
            &self.conn,
            self.root,
            x as i16,
            y as i16,
            w.max(1) as u16,
            h.max(1) as u16,
        ) else {
            self.set_topbar_notice("Screenshot failed", Duration::from_secs(3))?;
            return Ok(());
        };
        if image::save_buffer_with_format(
            &path,
            &pixels,
            width,
            height,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .is_err()
        {
            self.set_topbar_notice("Screenshot failed", Duration::from_secs(3))?;
            return Ok(());
        }
        copy_image_to_clipboard(&path);
        self.last_seen_clipboard_image_sig = clipboard_file_image_signature(&path);
        self.remember_clipboard_item(ClipboardItem::Image(path.clone()));
        self.set_topbar_notice(
            "Screenshot saved and copied to clipboard",
            Duration::from_secs(3),
        )?;
        self.open_media(FolderEntry {
            name: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Screenshot.png")
                .to_string(),
            path,
            kind: FileKind::Image,
        })?;
        if let Some(media) = self.media_slots.get_mut(0).and_then(|m| m.as_mut()) {
            media.notice = Some("Copied image to clipboard".to_string());
        }
        self.redraw_media_slot(0)?;
        Ok(())
    }

    fn redraw_screenshot_overlay(&self) -> AnyResult<()> {
        let mut c = self
            .screenshot_base
            .as_ref()
            .map(|preview| canvas_from_preview(preview, self.screen_width, self.screen_height))
            .unwrap_or_else(|| {
                Canvas::from_wallpaper_crop(
                    &self.wallpaper_pixels,
                    self.screen_width,
                    0,
                    0,
                    self.screen_width,
                    self.screen_height,
                )
            });
        c.draw_rect(
            0,
            0,
            i32::from(self.screen_width),
            i32::from(self.screen_height),
            Color::rgba(0, 0, 0, 128),
        );
        if let Some(selection) = self.screenshot_selection {
            let x1 = i32::from(selection.start_x)
                .min(i32::from(selection.current_x))
                .max(0);
            let y1 = i32::from(selection.start_y)
                .min(i32::from(selection.current_y))
                .max(0);
            let x2 = i32::from(selection.start_x)
                .max(i32::from(selection.current_x))
                .min(i32::from(self.screen_width));
            let y2 = i32::from(selection.start_y)
                .max(i32::from(selection.current_y))
                .min(i32::from(self.screen_height));
            let w = x2 - x1;
            let h = y2 - y1;
            if w > 0 && h > 0 {
                if let Some(base) = self.screenshot_base.as_ref() {
                    paint_preview_region(&mut c, base, x1, y1, w, h);
                }
                c.draw_rect(x1, y1, w, 2, MINT_LIGHT);
                c.draw_rect(x1, y2 - 2, w, 2, MINT_LIGHT);
                c.draw_rect(x1, y1, 2, h, MINT_LIGHT);
                c.draw_rect(x2 - 2, y1, 2, h, MINT_LIGHT);
            } else {
                c.draw_rect(x1 - 5, y1 - 5, 10, 2, MINT_LIGHT);
                c.draw_rect(x1 - 5, y1 + 4, 10, 2, MINT_LIGHT);
                c.draw_rect(x1 - 5, y1 - 5, 2, 10, MINT_LIGHT);
                c.draw_rect(x1 + 4, y1 - 5, 2, 10, MINT_LIGHT);
            }
        }
        self.upload_canvas(self.ui.screenshot_overlay, &c)
    }

    fn redraw_folder_terminal(&self) -> AnyResult<()> {
        let (x, y, w, h) = self.folder_terminal_geometry();
        let mut c = Canvas::from_wallpaper_crop(
            &self.wallpaper_pixels,
            self.screen_width,
            i32::from(x),
            i32::from(y),
            w,
            h,
        );
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            16,
            Color::rgba(247, 252, 255, 212),
        );
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            16,
            Color::rgba(214, 229, 237, 70),
        );
        c.draw_text(&self.bold, "Terminal", 18, 14, 14.0, MINT_DARK);
        c.draw_text(
            &self.regular,
            &compact_path(&self.folder_terminal.cwd, 30),
            98,
            14,
            14.0,
            MUTED,
        );
        c.draw_rect(
            16,
            42,
            i32::from(w) - 32,
            1,
            Color::rgba(178, 202, 214, 100),
        );
        let font_size = self.folder_terminal_font_size();
        let cell_h = self.folder_terminal_cell_h();
        let cell_w = self.folder_terminal_cell_w();
        let cols = self.folder_terminal.cols;
        let rows_count = self.folder_terminal.rows;
        let visible_rows = ((i32::from(h) - 56) / cell_h).max(1).min(rows_count as i32) as usize;
        let rows = self.folder_terminal_display_rows(visible_rows);
        if let Some(selection) = self.folder_terminal_selection {
            for rect in
                terminal_selection_rects(selection, &rows, self.folder_terminal_cell_w(), cell_h)
            {
                c.draw_round_rect(
                    i32::from(rect.x),
                    i32::from(rect.y),
                    i32::from(rect.width),
                    i32::from(rect.height),
                    3,
                    Color::rgba(175, 229, 245, 92),
                );
            }
        }
        for (idx, row) in rows.iter().enumerate() {
            let y = 52 + idx as i32 * cell_h;
            let row_chars: Vec<char> = row.chars().take(cols).collect();
            let mut col_idx = 0;
            while col_idx < row_chars.len() {
                let fg_color_idx = self
                    .folder_terminal
                    .screen_fg
                    .get(idx)
                    .and_then(|r| r.get(col_idx))
                    .copied()
                    .unwrap_or(255);
                let bg_color_idx = self
                    .folder_terminal
                    .screen_bg
                    .get(idx)
                    .and_then(|r| r.get(col_idx))
                    .copied()
                    .unwrap_or(255);
                let is_bold = self
                    .folder_terminal
                    .screen_bold
                    .get(idx)
                    .and_then(|r| r.get(col_idx))
                    .copied()
                    .unwrap_or(false);

                let mut run_len = 1;
                while col_idx + run_len < row_chars.len()
                    && self
                        .folder_terminal
                        .screen_fg
                        .get(idx)
                        .and_then(|r| r.get(col_idx + run_len))
                        .copied()
                        .unwrap_or(255)
                        == fg_color_idx
                    && self
                        .folder_terminal
                        .screen_bg
                        .get(idx)
                        .and_then(|r| r.get(col_idx + run_len))
                        .copied()
                        .unwrap_or(255)
                        == bg_color_idx
                    && self
                        .folder_terminal
                        .screen_bold
                        .get(idx)
                        .and_then(|r| r.get(col_idx + run_len))
                        .copied()
                        .unwrap_or(false)
                        == is_bold
                {
                    run_len += 1;
                }

                let run_chars = &row_chars[col_idx..col_idx + run_len];
                let x_pos = 18 + col_idx as i32 * cell_w;
                let width_px = run_len as i32 * cell_w;

                if bg_color_idx != 255 {
                    let bg_color = ansi_color(bg_color_idx);
                    c.draw_rect(x_pos, y, width_px, cell_h, bg_color);
                }

                let run_str: String = run_chars.iter().collect();
                let trimmed_len = run_str.trim_end().len();
                if trimmed_len > 0 {
                    let fg_color = if fg_color_idx == 255 {
                        INK
                    } else {
                        ansi_color(fg_color_idx)
                    };
                    let font = if is_bold {
                        &self.terminal_bold
                    } else {
                        &self.terminal_regular
                    };
                    c.draw_text(font, &run_str[..trimmed_len], x_pos, y, font_size, fg_color);
                }

                col_idx += run_len;
            }
        }
        if self.folder_terminal.focused {
            let row = self
                .folder_terminal
                .screen
                .get(self.folder_terminal.cursor_y.min(rows_count - 1))
                .map(|line| line.iter().collect::<String>())
                .unwrap_or_default();
            let prefix = row
                .chars()
                .take(self.folder_terminal.cursor_x.min(cols - 1))
                .collect::<String>();
            let cursor_x = 18 + measure_text(&self.terminal_regular, &prefix, font_size);
            let cursor_y = 53 + self.folder_terminal.cursor_y.min(rows_count - 1) as i32 * cell_h;
            c.draw_rect(cursor_x, cursor_y, 2, (font_size + 1.0) as i32, MINT_DARK);
        }
        self.upload_canvas(self.ui.folder_terminal, &c)
    }

    fn folder_terminal_display_rows(&self, visible_rows: usize) -> Vec<String> {
        if self.folder_terminal.scrollback == 0 {
            return self
                .folder_terminal
                .screen
                .iter()
                .take(visible_rows)
                .map(|row| row.iter().collect::<String>())
                .collect();
        }
        let history_len = self.folder_terminal.history.len();
        let start = history_len.saturating_sub(self.folder_terminal.scrollback + visible_rows);
        let end = (start + visible_rows).min(history_len);
        let mut rows = self.folder_terminal.history[start..end].to_vec();
        while rows.len() < visible_rows {
            rows.push(String::new());
        }
        rows
    }

    fn folder_terminal_font_size(&self) -> f32 {
        13.0 + f32::from(self.folder_terminal.zoom)
    }

    fn folder_terminal_cell_w(&self) -> i32 {
        measure_text(
            &self.terminal_regular,
            "A",
            self.folder_terminal_font_size(),
        )
        .max(6)
    }

    fn folder_terminal_cell_h(&self) -> i32 {
        (FOLDER_TERMINAL_CELL_H + i32::from(self.folder_terminal.zoom) * 2).max(12)
    }

    fn handle_folder_terminal_key(&mut self, ev: KeyPressEvent) -> AnyResult<()> {
        let mapping = self.conn.get_keyboard_mapping(ev.detail, 1)?.reply()?;
        let shifted = u16::from(ev.state) & u16::from(KeyButMask::SHIFT) != 0;
        let controlled = u16::from(ev.state) & u16::from(KeyButMask::CONTROL) != 0;
        let alted = u16::from(ev.state) & u16::from(KeyButMask::MOD1) != 0;
        let column = if shifted && mapping.keysyms_per_keycode > 1 {
            1
        } else {
            0
        };
        let Some(&keysym) = mapping.keysyms.get(column) else {
            return Ok(());
        };
        let base_keysym = mapping.keysyms.first().copied().unwrap_or(keysym);
        if controlled && shifted && matches!(base_keysym, 0x3d | 0x2b) {
            self.folder_terminal.zoom = (self.folder_terminal.zoom + 1).min(8);
            self.sync_folder_terminal_size();
            self.folder_terminal.dirty = true;
            self.redraw_folder_terminal()?;
            return Ok(());
        }
        if controlled && shifted && matches!(base_keysym, 0x2d | 0x5f) {
            self.folder_terminal.zoom = (self.folder_terminal.zoom - 1).max(-4);
            self.sync_folder_terminal_size();
            self.folder_terminal.dirty = true;
            self.redraw_folder_terminal()?;
            return Ok(());
        }
        if controlled && matches!(base_keysym, 0x76 | 0x56) {
            if let Some(text) = read_text_clipboard() {
                self.folder_terminal.scrollback = 0;
                if self.folder_terminal.bracketed_paste {
                    self.write_folder_terminal(b"\x1b[200~");
                    self.write_folder_terminal(text.as_bytes());
                    self.write_folder_terminal(b"\x1b[201~");
                } else {
                    self.write_folder_terminal(text.as_bytes());
                }
            }
            return Ok(());
        }
        if controlled && shifted && matches!(base_keysym, 0x63 | 0x43) {
            if let Some(selection) = self.folder_terminal_selection {
                let rows = self.folder_terminal_display_rows(self.folder_terminal.rows);
                let text = selected_terminal_text(selection, &rows);
                if !text.is_empty() {
                    copy_text_to_clipboard(&text);
                }
            }
            return Ok(());
        }
        let mut bytes = match keysym {
            0xff08 => b"\x7f".to_vec(),
            0xff09 => b"\t".to_vec(),
            0xff0d => b"\r".to_vec(),
            0xff1b => b"\x1b".to_vec(),
            0xffff => b"\x1b[3~".to_vec(),
            0xff50 => b"\x1b[H".to_vec(),
            0xff57 => b"\x1b[F".to_vec(),
            0xff55 => b"\x1b[5~".to_vec(),
            0xff56 => b"\x1b[6~".to_vec(),
            0xff51 => {
                if self.folder_terminal.app_cursor_keys {
                    b"\x1bOD".to_vec()
                } else {
                    b"\x1b[D".to_vec()
                }
            }
            0xff52 => {
                if self.folder_terminal.app_cursor_keys {
                    b"\x1bOA".to_vec()
                } else {
                    b"\x1b[A".to_vec()
                }
            }
            0xff53 => {
                if self.folder_terminal.app_cursor_keys {
                    b"\x1bOC".to_vec()
                } else {
                    b"\x1b[C".to_vec()
                }
            }
            0xff54 => {
                if self.folder_terminal.app_cursor_keys {
                    b"\x1bOB".to_vec()
                } else {
                    b"\x1b[B".to_vec()
                }
            }
            0xffbe..=0xffc9 => {
                const FKEYS: [&[u8]; 12] = [
                    b"\x1bOP",
                    b"\x1bOQ",
                    b"\x1bOR",
                    b"\x1bOS",
                    b"\x1b[15~",
                    b"\x1b[17~",
                    b"\x1b[18~",
                    b"\x1b[19~",
                    b"\x1b[20~",
                    b"\x1b[21~",
                    b"\x1b[23~",
                    b"\x1b[24~",
                ];
                FKEYS[(keysym - 0xffbe) as usize].to_vec()
            }
            0x40..=0x5f if controlled => vec![(keysym as u8) & 0x1f],
            0x61..=0x7a if controlled => vec![((keysym as u8) - b'a' + 1)],
            0x20..=0x7e => vec![keysym as u8],
            _ => return Ok(()),
        };
        if alted {
            bytes.insert(0, 0x1b);
        }
        self.folder_terminal.scrollback = 0;
        self.write_folder_terminal(&bytes);
        Ok(())
    }

    fn handle_folder_terminal_scroll(&mut self, button: u8) -> AnyResult<()> {
        let max_scroll = self.folder_terminal.history.len();
        let old = self.folder_terminal.scrollback;
        if button == 4 {
            self.folder_terminal.scrollback = (self.folder_terminal.scrollback + 3).min(max_scroll);
        } else {
            self.folder_terminal.scrollback = self.folder_terminal.scrollback.saturating_sub(3);
        }
        if self.folder_terminal.scrollback != old {
            self.redraw_folder_terminal()?;
        }
        Ok(())
    }

    fn handle_folder_terminal_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        if y >= 52 && !self.folder_terminal.mouse_enabled {
            let (row, col) = terminal_point_to_cell(
                x,
                y,
                self.folder_terminal_cell_w(),
                self.folder_terminal_cell_h(),
                self.folder_terminal.cols,
                self.folder_terminal.rows,
            );
            self.folder_terminal_selection = Some(TerminalSelection {
                start_row: row,
                start_col: col,
                end_row: row,
                end_col: col,
            });
            self.folder_terminal_selecting = true;
            self.folder_terminal_live_rects.clear();
            self.update_folder_terminal_live_selection()?;
            return Ok(());
        }
        if y < 44 || !self.folder_terminal.mouse_enabled {
            return Ok(());
        }
        let col = ((x - 18).max(0) / self.folder_terminal_cell_w() + 1)
            .clamp(1, self.folder_terminal.cols as i32);
        let row = ((y - 52).max(0) / self.folder_terminal_cell_h() + 1)
            .clamp(1, self.folder_terminal.rows as i32);
        let press = format!("\x1b[<0;{col};{row}M");
        let release = format!("\x1b[<0;{col};{row}m");
        self.write_folder_terminal(press.as_bytes());
        self.write_folder_terminal(release.as_bytes());
        Ok(())
    }

    fn handle_folder_terminal_motion(
        &mut self,
        x: i32,
        y: i32,
        button_down: bool,
    ) -> AnyResult<()> {
        if !button_down {
            if self.folder_terminal_selecting {
                self.handle_folder_terminal_release()?;
            }
            return Ok(());
        }
        if !self.folder_terminal_selecting {
            return Ok(());
        }
        let (row, col) = terminal_point_to_cell(
            x,
            y,
            self.folder_terminal_cell_w(),
            self.folder_terminal_cell_h(),
            self.folder_terminal.cols,
            self.folder_terminal.rows,
        );
        let Some(selection) = self.folder_terminal_selection.as_mut() else {
            return Ok(());
        };
        if selection.end_row == row && selection.end_col == col {
            return Ok(());
        }
        selection.end_row = row;
        selection.end_col = col;
        self.update_folder_terminal_live_selection()
    }

    fn handle_folder_terminal_release(&mut self) -> AnyResult<()> {
        self.folder_terminal_selecting = false;
        self.erase_folder_terminal_live_selection()?;
        if let Some(selection) = self.folder_terminal_selection {
            let rows = self.folder_terminal_display_rows(self.folder_terminal.rows);
            let text = selected_terminal_text(selection, &rows);
            if !text.is_empty() {
                copy_text_to_clipboard(&text);
            }
        }
        self.redraw_folder_terminal()
    }

    fn erase_folder_terminal_live_selection(&mut self) -> AnyResult<()> {
        if self.folder_terminal_live_rects.is_empty() {
            return Ok(());
        }
        let rects = std::mem::take(&mut self.folder_terminal_live_rects);
        self.draw_xor_rects(self.ui.folder_terminal, &rects)?;
        Ok(())
    }

    fn update_folder_terminal_live_selection(&mut self) -> AnyResult<()> {
        let rows = self.folder_terminal_display_rows(self.folder_terminal.rows);
        let Some(selection) = self.folder_terminal_selection else {
            return Ok(());
        };
        let rects = terminal_selection_rects(
            selection,
            &rows,
            self.folder_terminal_cell_w(),
            self.folder_terminal_cell_h(),
        );
        if same_rects(&rects, &self.folder_terminal_live_rects) {
            return Ok(());
        }
        self.erase_folder_terminal_live_selection()?;
        if !rects.is_empty() {
            self.draw_xor_rects(self.ui.folder_terminal, &rects)?;
            self.folder_terminal_live_rects = rects;
        }
        Ok(())
    }

    fn ensure_folder_terminal_pty(&mut self) {
        if self.folder_terminal.master_fd.is_some() {
            self.resize_folder_terminal_pty();
            return;
        }
        self.sync_folder_terminal_size();
        match spawn_terminal_pty(
            &self.folder_terminal.cwd,
            self.folder_terminal.cols,
            self.folder_terminal.rows,
        ) {
            Ok((fd, pid)) => {
                self.folder_terminal.master_fd = Some(fd);
                self.folder_terminal.child_pid = Some(pid);
                self.folder_terminal.history.clear();
                self.folder_terminal.scrollback = 0;
                self.folder_terminal.screen =
                    vec![vec![' '; self.folder_terminal.cols]; self.folder_terminal.rows];
                self.folder_terminal.screen_fg =
                    vec![vec![255; self.folder_terminal.cols]; self.folder_terminal.rows];
                self.folder_terminal.screen_bg =
                    vec![vec![255; self.folder_terminal.cols]; self.folder_terminal.rows];
                self.folder_terminal.screen_bold =
                    vec![vec![false; self.folder_terminal.cols]; self.folder_terminal.rows];
                self.folder_terminal.cursor_x = 0;
                self.folder_terminal.cursor_y = 0;
                self.folder_terminal.saved_cursor_x = 0;
                self.folder_terminal.saved_cursor_y = 0;
                self.folder_terminal.esc.clear();
                self.folder_terminal.line_drawing = false;
                self.folder_terminal.saved_line_drawing = false;
                self.folder_terminal.normal_screen = None;
                self.folder_terminal.normal_screen_fg = None;
                self.folder_terminal.normal_screen_bg = None;
                self.folder_terminal.normal_screen_bold = None;
                self.folder_terminal.scroll_top = 0;
                self.folder_terminal.scroll_bottom = self.folder_terminal.rows.saturating_sub(1);
                self.folder_terminal.insert_mode = false;
                self.folder_terminal.auto_wrap = true;
                self.folder_terminal.app_cursor_keys = false;
                self.folder_terminal.bracketed_paste = false;
                self.folder_terminal.mouse_enabled = false;
                self.folder_terminal.dirty = true;
            }
            Err(err) => {
                self.draw_terminal_message(&format!("terminal error: {err}"));
            }
        }
    }

    fn sync_folder_terminal_size(&mut self) {
        let (_, _, w, h) = self.folder_terminal_geometry();
        let cols = ((i32::from(w) - 36) / self.folder_terminal_cell_w())
            .max(24)
            .min(160) as usize;
        let rows = ((i32::from(h) - 56) / self.folder_terminal_cell_h())
            .max(3)
            .min(48) as usize;
        if cols == self.folder_terminal.cols && rows == self.folder_terminal.rows {
            return;
        }
        self.resize_folder_terminal_screen(cols, rows);
        self.resize_folder_terminal_pty();
    }

    fn resize_folder_terminal_screen(&mut self, cols: usize, rows: usize) {
        let old_rows = std::mem::take(&mut self.folder_terminal.screen);
        let old_fg = std::mem::take(&mut self.folder_terminal.screen_fg);
        let old_bg = std::mem::take(&mut self.folder_terminal.screen_bg);
        let old_bold = std::mem::take(&mut self.folder_terminal.screen_bold);
        let mut next = vec![vec![' '; cols]; rows];
        let mut next_fg = vec![vec![255; cols]; rows];
        let mut next_bg = vec![vec![255; cols]; rows];
        let mut next_bold = vec![vec![false; cols]; rows];
        let copy_rows = old_rows.len().min(rows);
        let copy_cols = self.folder_terminal.cols.min(cols);
        let old_start = old_rows.len().saturating_sub(copy_rows);
        let new_start = rows.saturating_sub(copy_rows);
        for idx in 0..copy_rows {
            for col in 0..copy_cols {
                next[new_start + idx][col] = old_rows[old_start + idx][col];
                if old_fg.len() > old_start + idx && old_fg[old_start + idx].len() > col {
                    next_fg[new_start + idx][col] = old_fg[old_start + idx][col];
                }
                if old_bg.len() > old_start + idx && old_bg[old_start + idx].len() > col {
                    next_bg[new_start + idx][col] = old_bg[old_start + idx][col];
                }
                if old_bold.len() > old_start + idx && old_bold[old_start + idx].len() > col {
                    next_bold[new_start + idx][col] = old_bold[old_start + idx][col];
                }
            }
        }
        self.folder_terminal.cols = cols;
        self.folder_terminal.rows = rows;
        self.folder_terminal.cursor_x = self.folder_terminal.cursor_x.min(cols.saturating_sub(1));
        self.folder_terminal.cursor_y = self.folder_terminal.cursor_y.min(rows.saturating_sub(1));
        self.folder_terminal.saved_cursor_x = self
            .folder_terminal
            .saved_cursor_x
            .min(cols.saturating_sub(1));
        self.folder_terminal.saved_cursor_y = self
            .folder_terminal
            .saved_cursor_y
            .min(rows.saturating_sub(1));
        self.folder_terminal.scroll_top =
            self.folder_terminal.scroll_top.min(rows.saturating_sub(1));
        self.folder_terminal.scroll_bottom = self
            .folder_terminal
            .scroll_bottom
            .min(rows.saturating_sub(1))
            .max(self.folder_terminal.scroll_top);
        self.folder_terminal.screen = next;
        self.folder_terminal.screen_fg = next_fg;
        self.folder_terminal.screen_bg = next_bg;
        self.folder_terminal.screen_bold = next_bold;
        self.folder_terminal.dirty = true;
    }

    fn resize_folder_terminal_pty(&mut self) {
        let Some(fd) = self.folder_terminal.master_fd else {
            return;
        };
        let mut winsize = libc::winsize {
            ws_row: self.folder_terminal.rows as u16,
            ws_col: self.folder_terminal.cols as u16,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            let _ = libc::ioctl(fd, libc::TIOCSWINSZ, &mut winsize);
            let pgrp = libc::tcgetpgrp(fd);
            if pgrp > 0 {
                let _ = libc::kill(-pgrp, libc::SIGWINCH);
            } else if let Some(pid) = self.folder_terminal.child_pid {
                let _ = libc::kill(-pid, libc::SIGWINCH);
            }
        }
    }

    fn write_folder_terminal(&mut self, bytes: &[u8]) {
        if let Some(fd) = self.folder_terminal.master_fd {
            unsafe {
                let _ = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
            }
        }
    }

    fn poll_folder_terminal(&mut self) -> AnyResult<bool> {
        let Some(fd) = self.folder_terminal.master_fd else {
            return Ok(false);
        };
        let mut changed = false;
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                changed = true;
                self.folder_terminal.scrollback = 0;
                let text = String::from_utf8_lossy(&buf[..n as usize]).to_string();
                self.feed_folder_terminal(&text);
            } else {
                break;
            }
        }
        if changed || self.folder_terminal.dirty {
            self.folder_terminal.dirty = false;
            self.redraw_folder_terminal()?;
        }
        Ok(changed)
    }

    fn draw_terminal_message(&mut self, message: &str) {
        self.folder_terminal.history.clear();
        self.folder_terminal.scrollback = 0;
        self.folder_terminal.screen =
            vec![vec![' '; self.folder_terminal.cols]; self.folder_terminal.rows];
        self.folder_terminal.screen_fg =
            vec![vec![255; self.folder_terminal.cols]; self.folder_terminal.rows];
        self.folder_terminal.screen_bg =
            vec![vec![255; self.folder_terminal.cols]; self.folder_terminal.rows];
        self.folder_terminal.screen_bold =
            vec![vec![false; self.folder_terminal.cols]; self.folder_terminal.rows];
        for (idx, ch) in message.chars().take(self.folder_terminal.cols).enumerate() {
            self.folder_terminal.screen[0][idx] = ch;
        }
        self.folder_terminal.dirty = true;
    }

    fn feed_folder_terminal(&mut self, text: &str) {
        for ch in text.chars() {
            self.feed_terminal_char(ch);
        }
    }

    fn feed_terminal_char(&mut self, ch: char) {
        if !self.folder_terminal.esc.is_empty() || ch == '\x1b' {
            self.feed_terminal_escape(ch);
            return;
        }
        match ch {
            '\r' => self.folder_terminal.cursor_x = 0,
            '\n' => self.terminal_newline(),
            '\x08' => {
                self.folder_terminal.cursor_x = self.folder_terminal.cursor_x.saturating_sub(1);
            }
            '\t' => {
                let next = ((self.folder_terminal.cursor_x / 8) + 1) * 8;
                self.folder_terminal.cursor_x = next.min(self.folder_terminal.cols - 1);
            }
            c if !c.is_control() => self.terminal_put_char(c),
            _ => {}
        }
    }

    fn feed_terminal_escape(&mut self, ch: char) {
        if ch == '\x1b' {
            self.folder_terminal.esc.clear();
            self.folder_terminal.esc.push('\x1b');
            return;
        }
        if self.folder_terminal.esc.is_empty() {
            self.folder_terminal.esc.push(ch);
            return;
        }
        self.folder_terminal.esc.push(ch);
        if self.folder_terminal.esc.len() > 4096 {
            self.folder_terminal.esc.clear();
            return;
        }
        if self.folder_terminal.esc.starts_with("\x1b]") {
            if ch == '\x07' || self.folder_terminal.esc.ends_with("\x1b\\") {
                self.folder_terminal.esc.clear();
            }
            return;
        }
        if self.folder_terminal.esc.starts_with("\x1bP")
            || self.folder_terminal.esc.starts_with("\x1b^")
            || self.folder_terminal.esc.starts_with("\x1b_")
        {
            if self.folder_terminal.esc.ends_with("\x1b\\") {
                self.folder_terminal.esc.clear();
            }
            return;
        }
        if self.folder_terminal.esc.starts_with("\x1b#") {
            if self.folder_terminal.esc.len() >= 3 {
                if self.folder_terminal.esc.ends_with('8') {
                    for row in &mut self.folder_terminal.screen {
                        row.fill('E');
                    }
                }
                self.folder_terminal.esc.clear();
            }
            return;
        }
        if self.folder_terminal.esc.starts_with("\x1b(")
            || self.folder_terminal.esc.starts_with("\x1b)")
        {
            if self.folder_terminal.esc.len() >= 3 {
                self.folder_terminal.line_drawing = self.folder_terminal.esc.ends_with('0');
                self.folder_terminal.esc.clear();
            }
            return;
        }
        if self.folder_terminal.esc.len() == 2 {
            match self.folder_terminal.esc.as_str() {
                "\x1b7" => {
                    self.save_terminal_cursor();
                    self.folder_terminal.esc.clear();
                    return;
                }
                "\x1b8" => {
                    self.restore_terminal_cursor();
                    self.folder_terminal.esc.clear();
                    return;
                }
                "\x1bD" => {
                    self.terminal_linefeed();
                    self.folder_terminal.esc.clear();
                    return;
                }
                "\x1bE" => {
                    self.terminal_newline();
                    self.folder_terminal.esc.clear();
                    return;
                }
                "\x1bM" => {
                    self.terminal_reverse_index();
                    self.folder_terminal.esc.clear();
                    return;
                }
                "\x1bc" => {
                    self.reset_terminal_emulation();
                    self.folder_terminal.esc.clear();
                    return;
                }
                "\x1b=" | "\x1b>" | "\x1b<" => {
                    self.folder_terminal.esc.clear();
                    return;
                }
                _ => {
                    let second = self.folder_terminal.esc.chars().nth(1).unwrap();
                    if !matches!(second, '[' | '(' | ')' | '#' | 'O' | ']' | 'P' | '^' | '_') {
                        self.folder_terminal.esc.clear();
                        return;
                    }
                }
            }
        } else {
            match self.folder_terminal.esc.as_str() {
                "\x1b7" => {
                    self.save_terminal_cursor();
                    self.folder_terminal.esc.clear();
                    return;
                }
                "\x1b8" => {
                    self.restore_terminal_cursor();
                    self.folder_terminal.esc.clear();
                    return;
                }
                "\x1bD" => {
                    self.terminal_linefeed();
                    self.folder_terminal.esc.clear();
                    return;
                }
                "\x1bE" => {
                    self.terminal_newline();
                    self.folder_terminal.esc.clear();
                    return;
                }
                "\x1bM" => {
                    self.terminal_reverse_index();
                    self.folder_terminal.esc.clear();
                    return;
                }
                "\x1bc" => {
                    self.reset_terminal_emulation();
                    self.folder_terminal.esc.clear();
                    return;
                }
                _ => {}
            }
        }
        if self.folder_terminal.esc.starts_with("\x1bO") {
            if self.folder_terminal.esc.len() >= 3 {
                self.folder_terminal.esc.clear();
            }
            return;
        }
        if self.folder_terminal.esc == "\x1b[" {
            return;
        }
        if !('\x40'..='\x7e').contains(&ch) {
            return;
        }
        let esc = std::mem::take(&mut self.folder_terminal.esc);
        if let Some(body) = esc.strip_prefix("\x1b[") {
            self.apply_terminal_csi(body);
        }
    }

    fn apply_terminal_csi(&mut self, body: &str) {
        let command = body.chars().last().unwrap_or('m');
        let private = body.starts_with('?');
        let params = body[..body.len().saturating_sub(1)]
            .trim_start_matches(['?', '>', '!', '='])
            .trim_matches(|ch: char| ch == ' ' || ch == '$' || ch == '"' || ch == '\'');
        let values = csi_values(params);
        let cols = self.folder_terminal.cols;
        let rows = self.folder_terminal.rows;
        if private && matches!(command, 'h' | 'l') {
            let enabled = command == 'h';
            for value in values {
                match value {
                    1 => self.folder_terminal.app_cursor_keys = enabled,
                    3 => {
                        self.folder_terminal.screen = vec![vec![' '; cols]; rows];
                        self.folder_terminal.cursor_x = 0;
                        self.folder_terminal.cursor_y = 0;
                    }
                    7 => self.folder_terminal.auto_wrap = enabled,
                    9 | 1000 | 1002 | 1003 | 1006 => self.folder_terminal.mouse_enabled = enabled,
                    1047 => self.set_terminal_alt_screen(enabled, false),
                    1048 => {
                        if enabled {
                            self.save_terminal_cursor();
                        } else {
                            self.restore_terminal_cursor();
                        }
                    }
                    1049 => self.set_terminal_alt_screen(enabled, true),
                    2004 => self.folder_terminal.bracketed_paste = enabled,
                    _ => {}
                }
            }
            return;
        }
        match command {
            'H' | 'f' => {
                let row = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1)
                    .saturating_sub(1);
                let col = values
                    .get(1)
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1)
                    .saturating_sub(1);
                self.folder_terminal.cursor_y = row.min(rows - 1);
                self.folder_terminal.cursor_x = col.min(cols - 1);
            }
            'A' => {
                let amt = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1);
                self.folder_terminal.cursor_y = self.folder_terminal.cursor_y.saturating_sub(amt);
            }
            'B' => {
                let amt = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1);
                self.folder_terminal.cursor_y = (self.folder_terminal.cursor_y + amt).min(rows - 1);
            }
            'C' => {
                let amt = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1);
                self.folder_terminal.cursor_x = (self.folder_terminal.cursor_x + amt).min(cols - 1);
            }
            'D' => {
                let amt = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1);
                self.folder_terminal.cursor_x = self.folder_terminal.cursor_x.saturating_sub(amt);
            }
            'E' => {
                let amt = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1);
                self.folder_terminal.cursor_y = (self.folder_terminal.cursor_y + amt).min(rows - 1);
                self.folder_terminal.cursor_x = 0;
            }
            'F' => {
                let amt = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1);
                self.folder_terminal.cursor_y = self.folder_terminal.cursor_y.saturating_sub(amt);
                self.folder_terminal.cursor_x = 0;
            }
            'G' => {
                let col = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1)
                    .saturating_sub(1);
                self.folder_terminal.cursor_x = col.min(cols - 1);
            }
            'I' => {
                let amt = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1);
                let next = ((self.folder_terminal.cursor_x / 8) + amt) * 8;
                self.folder_terminal.cursor_x = next.min(cols - 1);
            }
            'Z' => {
                let amt = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1);
                let previous = (self.folder_terminal.cursor_x / 8).saturating_sub(amt) * 8;
                self.folder_terminal.cursor_x = previous.min(cols - 1);
            }
            'd' => {
                let row = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1)
                    .saturating_sub(1);
                self.folder_terminal.cursor_y = row.min(rows - 1);
            }
            'J' => match values.first().copied().unwrap_or(0) {
                0 => {
                    let y = self.folder_terminal.cursor_y.min(rows - 1);
                    for x in self.folder_terminal.cursor_x..cols {
                        self.folder_terminal.screen[y][x] = ' ';
                        self.folder_terminal.screen_fg[y][x] = 255;
                        self.folder_terminal.screen_bg[y][x] = 255;
                        self.folder_terminal.screen_bold[y][x] = false;
                    }
                    for yy in y + 1..rows {
                        self.folder_terminal.screen[yy].fill(' ');
                        self.folder_terminal.screen_fg[yy].fill(255);
                        self.folder_terminal.screen_bg[yy].fill(255);
                        self.folder_terminal.screen_bold[yy].fill(false);
                    }
                }
                1 => {
                    let y = self.folder_terminal.cursor_y.min(rows - 1);
                    for yy in 0..y {
                        self.folder_terminal.screen[yy].fill(' ');
                        self.folder_terminal.screen_fg[yy].fill(255);
                        self.folder_terminal.screen_bg[yy].fill(255);
                        self.folder_terminal.screen_bold[yy].fill(false);
                    }
                    for x in 0..=self.folder_terminal.cursor_x.min(cols - 1) {
                        self.folder_terminal.screen[y][x] = ' ';
                        self.folder_terminal.screen_fg[y][x] = 255;
                        self.folder_terminal.screen_bg[y][x] = 255;
                        self.folder_terminal.screen_bold[y][x] = false;
                    }
                }
                3 => self.folder_terminal.history.clear(),
                _ => {
                    // Erase entire display (CSI 2 J) - Cursor position does NOT change!
                    self.folder_terminal.screen = vec![vec![' '; cols]; rows];
                    self.folder_terminal.screen_fg = vec![vec![255; cols]; rows];
                    self.folder_terminal.screen_bg = vec![vec![255; cols]; rows];
                    self.folder_terminal.screen_bold = vec![vec![false; cols]; rows];
                }
            },
            'K' => {
                let y = self.folder_terminal.cursor_y.min(rows - 1);
                match values.first().copied().unwrap_or(0) {
                    0 => {
                        for x in self.folder_terminal.cursor_x..cols {
                            self.folder_terminal.screen[y][x] = ' ';
                            self.folder_terminal.screen_fg[y][x] = 255;
                            self.folder_terminal.screen_bg[y][x] = 255;
                            self.folder_terminal.screen_bold[y][x] = false;
                        }
                    }
                    1 => {
                        for x in 0..=self.folder_terminal.cursor_x.min(cols - 1) {
                            self.folder_terminal.screen[y][x] = ' ';
                            self.folder_terminal.screen_fg[y][x] = 255;
                            self.folder_terminal.screen_bg[y][x] = 255;
                            self.folder_terminal.screen_bold[y][x] = false;
                        }
                    }
                    _ => {
                        self.folder_terminal.screen[y].fill(' ');
                        self.folder_terminal.screen_fg[y].fill(255);
                        self.folder_terminal.screen_bg[y].fill(255);
                        self.folder_terminal.screen_bold[y].fill(false);
                    }
                }
            }
            'X' => {
                let count = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1);
                let y = self.folder_terminal.cursor_y.min(rows - 1);
                for x in
                    self.folder_terminal.cursor_x..(self.folder_terminal.cursor_x + count).min(cols)
                {
                    self.folder_terminal.screen[y][x] = ' ';
                    self.folder_terminal.screen_fg[y][x] = 255;
                    self.folder_terminal.screen_bg[y][x] = 255;
                    self.folder_terminal.screen_bold[y][x] = false;
                }
            }
            'P' => {
                let count = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1)
                    .min(cols);
                let y = self.folder_terminal.cursor_y.min(rows - 1);
                for x in self.folder_terminal.cursor_x..cols {
                    let src = x + count;
                    if src < cols {
                        self.folder_terminal.screen[y][x] = self.folder_terminal.screen[y][src];
                        self.folder_terminal.screen_fg[y][x] =
                            self.folder_terminal.screen_fg[y][src];
                        self.folder_terminal.screen_bg[y][x] =
                            self.folder_terminal.screen_bg[y][src];
                        self.folder_terminal.screen_bold[y][x] =
                            self.folder_terminal.screen_bold[y][src];
                    } else {
                        self.folder_terminal.screen[y][x] = ' ';
                        self.folder_terminal.screen_fg[y][x] = 255;
                        self.folder_terminal.screen_bg[y][x] = 255;
                        self.folder_terminal.screen_bold[y][x] = false;
                    }
                }
            }
            'm' => {
                if values.is_empty() {
                    self.folder_terminal.current_fg = 255;
                    self.folder_terminal.current_bg = 255;
                    self.folder_terminal.current_bold = false;
                } else {
                    let mut i = 0;
                    while i < values.len() {
                        let val = values[i];
                        match val {
                            0 => {
                                self.folder_terminal.current_fg = 255;
                                self.folder_terminal.current_bg = 255;
                                self.folder_terminal.current_bold = false;
                            }
                            1 => {
                                self.folder_terminal.current_bold = true;
                            }
                            22 => {
                                self.folder_terminal.current_bold = false;
                            }
                            30..=37 => {
                                self.folder_terminal.current_fg = (val - 30) as u8;
                            }
                            38 => {
                                if i + 2 < values.len() && values[i + 1] == 5 {
                                    self.folder_terminal.current_fg = values[i + 2] as u8;
                                    i += 2;
                                } else if i + 4 < values.len() && values[i + 1] == 2 {
                                    i += 4;
                                }
                            }
                            39 => {
                                self.folder_terminal.current_fg = 255;
                            }
                            40..=47 => {
                                self.folder_terminal.current_bg = (val - 40) as u8;
                            }
                            48 => {
                                if i + 2 < values.len() && values[i + 1] == 5 {
                                    self.folder_terminal.current_bg = values[i + 2] as u8;
                                    i += 2;
                                } else if i + 4 < values.len() && values[i + 1] == 2 {
                                    i += 4;
                                }
                            }
                            49 => {
                                self.folder_terminal.current_bg = 255;
                            }
                            90..=97 => {
                                self.folder_terminal.current_fg = (val - 90 + 8) as u8;
                            }
                            100..=107 => {
                                self.folder_terminal.current_bg = (val - 100 + 8) as u8;
                            }
                            _ => {}
                        }
                        i += 1;
                    }
                }
            }
            '@' => {
                let count = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1);
                self.terminal_insert_blanks(count);
            }
            'L' => {
                let count = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1);
                self.terminal_insert_lines(count);
            }
            'M' => {
                let count = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1);
                self.terminal_delete_lines(count);
            }
            'S' => {
                let count = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1);
                self.terminal_scroll_up(count);
            }
            'T' => {
                let count = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1);
                self.terminal_scroll_down(count);
            }
            'r' => {
                let top = values
                    .first()
                    .copied()
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1)
                    .saturating_sub(1);
                let bottom = values
                    .get(1)
                    .copied()
                    .map(|v| if v == 0 { rows } else { v })
                    .unwrap_or(rows)
                    .saturating_sub(1);
                if top < bottom && bottom < rows {
                    self.folder_terminal.scroll_top = top;
                    self.folder_terminal.scroll_bottom = bottom;
                } else {
                    self.folder_terminal.scroll_top = 0;
                    self.folder_terminal.scroll_bottom = rows - 1;
                }
                self.folder_terminal.cursor_x = 0;
                self.folder_terminal.cursor_y = 0;
            }
            's' => self.save_terminal_cursor(),
            'u' => self.restore_terminal_cursor(),
            'h' | 'l' => {
                let enabled = command == 'h';
                for value in values {
                    if value == 4 {
                        self.folder_terminal.insert_mode = enabled;
                    }
                }
            }
            'c' => {
                self.write_folder_terminal(b"\x1b[?1;2c");
            }
            'n' => {
                let val = values.first().copied().unwrap_or(0);
                if val == 6 {
                    let row = self.folder_terminal.cursor_y + 1;
                    let col = self.folder_terminal.cursor_x + 1;
                    let response = format!("\x1b[{};{}R", row, col);
                    self.write_folder_terminal(response.as_bytes());
                } else if val == 5 {
                    self.write_folder_terminal(b"\x1b[0n");
                }
            }
            _ => {}
        }
    }

    fn save_terminal_cursor(&mut self) {
        self.folder_terminal.saved_cursor_x = self.folder_terminal.cursor_x;
        self.folder_terminal.saved_cursor_y = self.folder_terminal.cursor_y;
        self.folder_terminal.saved_line_drawing = self.folder_terminal.line_drawing;
    }

    fn restore_terminal_cursor(&mut self) {
        self.folder_terminal.cursor_x = self
            .folder_terminal
            .saved_cursor_x
            .min(self.folder_terminal.cols.saturating_sub(1));
        self.folder_terminal.cursor_y = self
            .folder_terminal
            .saved_cursor_y
            .min(self.folder_terminal.rows.saturating_sub(1));
        self.folder_terminal.line_drawing = self.folder_terminal.saved_line_drawing;
    }

    fn reset_terminal_emulation(&mut self) {
        let cols = self.folder_terminal.cols;
        let rows = self.folder_terminal.rows;
        self.folder_terminal.screen = vec![vec![' '; cols]; rows];
        self.folder_terminal.screen_fg = vec![vec![255; cols]; rows];
        self.folder_terminal.screen_bg = vec![vec![255; cols]; rows];
        self.folder_terminal.screen_bold = vec![vec![false; cols]; rows];
        self.folder_terminal.cursor_x = 0;
        self.folder_terminal.cursor_y = 0;
        self.folder_terminal.saved_cursor_x = 0;
        self.folder_terminal.saved_cursor_y = 0;
        self.folder_terminal.line_drawing = false;
        self.folder_terminal.saved_line_drawing = false;
        self.folder_terminal.normal_screen = None;
        self.folder_terminal.normal_screen_fg = None;
        self.folder_terminal.normal_screen_bg = None;
        self.folder_terminal.normal_screen_bold = None;
        self.folder_terminal.current_fg = 255;
        self.folder_terminal.current_bg = 255;
        self.folder_terminal.current_bold = false;
        self.folder_terminal.scroll_top = 0;
        self.folder_terminal.scroll_bottom = rows.saturating_sub(1);
        self.folder_terminal.insert_mode = false;
        self.folder_terminal.auto_wrap = true;
        self.folder_terminal.app_cursor_keys = false;
        self.folder_terminal.bracketed_paste = false;
        self.folder_terminal.mouse_enabled = false;
    }

    fn set_terminal_alt_screen(&mut self, enabled: bool, save_cursor: bool) {
        let cols = self.folder_terminal.cols;
        let rows = self.folder_terminal.rows;
        if enabled {
            if save_cursor {
                self.save_terminal_cursor();
            }
            if self.folder_terminal.normal_screen.is_none() {
                self.folder_terminal.normal_screen = Some(self.folder_terminal.screen.clone());
            }
            if self.folder_terminal.normal_screen_fg.is_none() {
                self.folder_terminal.normal_screen_fg =
                    Some(self.folder_terminal.screen_fg.clone());
            }
            if self.folder_terminal.normal_screen_bg.is_none() {
                self.folder_terminal.normal_screen_bg =
                    Some(self.folder_terminal.screen_bg.clone());
            }
            if self.folder_terminal.normal_screen_bold.is_none() {
                self.folder_terminal.normal_screen_bold =
                    Some(self.folder_terminal.screen_bold.clone());
            }
            self.folder_terminal.screen = vec![vec![' '; cols]; rows];
            self.folder_terminal.screen_fg = vec![vec![255; cols]; rows];
            self.folder_terminal.screen_bg = vec![vec![255; cols]; rows];
            self.folder_terminal.screen_bold = vec![vec![false; cols]; rows];
            self.folder_terminal.cursor_x = 0;
            self.folder_terminal.cursor_y = 0;
            self.folder_terminal.scroll_top = 0;
            self.folder_terminal.scroll_bottom = rows.saturating_sub(1);
        } else {
            if let Some(screen) = self.folder_terminal.normal_screen.take() {
                self.folder_terminal.screen = screen;
            }
            if let Some(screen_fg) = self.folder_terminal.normal_screen_fg.take() {
                self.folder_terminal.screen_fg = screen_fg;
            }
            if let Some(screen_bg) = self.folder_terminal.normal_screen_bg.take() {
                self.folder_terminal.screen_bg = screen_bg;
            }
            if let Some(screen_bold) = self.folder_terminal.normal_screen_bold.take() {
                self.folder_terminal.screen_bold = screen_bold;
            }
            if save_cursor {
                self.restore_terminal_cursor();
            }
            self.folder_terminal.scroll_top = 0;
            self.folder_terminal.scroll_bottom = rows.saturating_sub(1);
            self.folder_terminal.mouse_enabled = false;
        }
    }

    fn terminal_insert_blanks(&mut self, count: usize) {
        let cols = self.folder_terminal.cols;
        let y = self.folder_terminal.cursor_y;
        let count = count.min(cols);
        for x in (self.folder_terminal.cursor_x..cols).rev() {
            let src_opt = x
                .checked_sub(count)
                .filter(|src| *src >= self.folder_terminal.cursor_x);
            self.folder_terminal.screen[y][x] = src_opt
                .map(|src| self.folder_terminal.screen[y][src])
                .unwrap_or(' ');
            self.folder_terminal.screen_fg[y][x] = src_opt
                .map(|src| self.folder_terminal.screen_fg[y][src])
                .unwrap_or(255);
            self.folder_terminal.screen_bg[y][x] = src_opt
                .map(|src| self.folder_terminal.screen_bg[y][src])
                .unwrap_or(255);
            self.folder_terminal.screen_bold[y][x] = src_opt
                .map(|src| self.folder_terminal.screen_bold[y][src])
                .unwrap_or(false);
        }
    }

    fn terminal_insert_lines(&mut self, count: usize) {
        let cols = self.folder_terminal.cols;
        let bottom = self
            .folder_terminal
            .scroll_bottom
            .min(self.folder_terminal.rows - 1);
        if self.folder_terminal.cursor_y > bottom {
            return;
        }
        for _ in 0..count.min(self.folder_terminal.rows) {
            self.folder_terminal
                .screen
                .insert(self.folder_terminal.cursor_y, vec![' '; cols]);
            self.folder_terminal.screen.remove(bottom + 1);
            self.folder_terminal
                .screen_fg
                .insert(self.folder_terminal.cursor_y, vec![255; cols]);
            self.folder_terminal.screen_fg.remove(bottom + 1);
            self.folder_terminal
                .screen_bg
                .insert(self.folder_terminal.cursor_y, vec![255; cols]);
            self.folder_terminal.screen_bg.remove(bottom + 1);
            self.folder_terminal
                .screen_bold
                .insert(self.folder_terminal.cursor_y, vec![false; cols]);
            self.folder_terminal.screen_bold.remove(bottom + 1);
        }
    }

    fn terminal_delete_lines(&mut self, count: usize) {
        let cols = self.folder_terminal.cols;
        let bottom = self
            .folder_terminal
            .scroll_bottom
            .min(self.folder_terminal.rows - 1);
        if self.folder_terminal.cursor_y > bottom {
            return;
        }
        for _ in 0..count.min(self.folder_terminal.rows) {
            self.folder_terminal
                .screen
                .remove(self.folder_terminal.cursor_y);
            self.folder_terminal.screen.insert(bottom, vec![' '; cols]);
            self.folder_terminal
                .screen_fg
                .remove(self.folder_terminal.cursor_y);
            self.folder_terminal
                .screen_fg
                .insert(bottom, vec![255; cols]);
            self.folder_terminal
                .screen_bg
                .remove(self.folder_terminal.cursor_y);
            self.folder_terminal
                .screen_bg
                .insert(bottom, vec![255; cols]);
            self.folder_terminal
                .screen_bold
                .remove(self.folder_terminal.cursor_y);
            self.folder_terminal
                .screen_bold
                .insert(bottom, vec![false; cols]);
        }
    }

    fn terminal_scroll_up(&mut self, count: usize) {
        let cols = self.folder_terminal.cols;
        let top = self.folder_terminal.scroll_top;
        let bottom = self
            .folder_terminal
            .scroll_bottom
            .min(self.folder_terminal.rows - 1);
        for _ in 0..count.max(1) {
            let removed = self.folder_terminal.screen.remove(top);
            self.folder_terminal.screen_fg.remove(top);
            self.folder_terminal.screen_bg.remove(top);
            self.folder_terminal.screen_bold.remove(top);
            if top == 0 && bottom + 1 == self.folder_terminal.rows {
                self.folder_terminal
                    .history
                    .push(removed.iter().collect::<String>());
                if self.folder_terminal.history.len() > TERMINAL_HISTORY_LIMIT {
                    let extra = self.folder_terminal.history.len() - TERMINAL_HISTORY_LIMIT;
                    self.folder_terminal.history.drain(0..extra);
                }
            }
            self.folder_terminal.screen.insert(bottom, vec![' '; cols]);
            self.folder_terminal
                .screen_fg
                .insert(bottom, vec![255; cols]);
            self.folder_terminal
                .screen_bg
                .insert(bottom, vec![255; cols]);
            self.folder_terminal
                .screen_bold
                .insert(bottom, vec![false; cols]);
        }
    }

    fn terminal_scroll_down(&mut self, count: usize) {
        let cols = self.folder_terminal.cols;
        let top = self.folder_terminal.scroll_top;
        let bottom = self
            .folder_terminal
            .scroll_bottom
            .min(self.folder_terminal.rows - 1);
        for _ in 0..count.max(1) {
            self.folder_terminal.screen.remove(bottom);
            self.folder_terminal.screen_fg.remove(bottom);
            self.folder_terminal.screen_bg.remove(bottom);
            self.folder_terminal.screen_bold.remove(bottom);
            self.folder_terminal.screen.insert(top, vec![' '; cols]);
            self.folder_terminal.screen_fg.insert(top, vec![255; cols]);
            self.folder_terminal.screen_bg.insert(top, vec![255; cols]);
            self.folder_terminal
                .screen_bold
                .insert(top, vec![false; cols]);
        }
    }

    fn terminal_reverse_index(&mut self) {
        if self.folder_terminal.cursor_y == self.folder_terminal.scroll_top {
            self.terminal_scroll_down(1);
        } else {
            self.folder_terminal.cursor_y = self.folder_terminal.cursor_y.saturating_sub(1);
        }
    }

    fn terminal_put_char(&mut self, ch: char) {
        let cols = self.folder_terminal.cols;
        let rows = self.folder_terminal.rows;
        if self.folder_terminal.cursor_x >= cols {
            if self.folder_terminal.auto_wrap {
                self.terminal_newline();
            } else {
                self.folder_terminal.cursor_x = cols.saturating_sub(1);
            }
        }
        let x = self.folder_terminal.cursor_x.min(cols - 1);
        let y = self.folder_terminal.cursor_y.min(rows - 1);
        if self.folder_terminal.insert_mode {
            self.terminal_insert_blanks(1);
        }
        self.folder_terminal.screen[y][x] =
            terminal_display_char(ch, self.folder_terminal.line_drawing);
        let mut fg = self.folder_terminal.current_fg;
        if self.folder_terminal.current_bold && fg < 8 {
            fg += 8;
        }
        self.folder_terminal.screen_fg[y][x] = fg;
        self.folder_terminal.screen_bg[y][x] = self.folder_terminal.current_bg;
        self.folder_terminal.screen_bold[y][x] = self.folder_terminal.current_bold;
        self.folder_terminal.cursor_x += 1;
    }

    fn terminal_linefeed(&mut self) {
        if self.folder_terminal.cursor_y >= self.folder_terminal.scroll_bottom {
            self.terminal_scroll_up(1);
        } else {
            self.folder_terminal.cursor_y += 1;
        }
    }

    fn terminal_newline(&mut self) {
        self.folder_terminal.cursor_x = 0;
        self.terminal_linefeed();
    }

    fn folder_context_action_at(&self, x: i32, y: i32) -> Option<FolderContextAction> {
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

    fn run_folder_context_action(&mut self, action: FolderContextAction) -> AnyResult<()> {
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

    fn stop_ffplay_process(&mut self) {
        let Some(mut child) = self.ffplay_process.take() else {
            return;
        };
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn reap_ffplay_process(&mut self) {
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

    fn open_media(&mut self, entry: FolderEntry) -> AnyResult<()> {
        self.stop_ffplay_process();

        for (idx, state) in self.media_slots.iter_mut().enumerate() {
            if state.is_some() {
                *state = None;
                let _ = self.conn.unmap_window(self.ui.media[idx]);
            }
        }
        let slot = 0;
        let text_lines = if entry.kind == FileKind::Text {
            read_text_lines_limited(&entry.path, 5000)
        } else {
            Vec::new()
        };
        let file_info = (entry.kind == FileKind::Other).then(|| file_command_summary(&entry.path));
        let is_playable = entry.kind == FileKind::Audio || entry.kind == FileKind::Video;

        // For playable media, open ffplay in its own standalone window
        if is_playable {
            let (ffplay_x, ffplay_y, ffplay_w, ffplay_h) = self.ffplay_geometry();
            let path_str = entry.path.to_string_lossy().into_owned();
            let mut cmd = Command::new("ffplay");
            cmd.env("DISPLAY", &self.display)
                .args(["-window_title", "Aurora ffplay"])
                .args(["-x", &ffplay_w.to_string()])
                .args(["-y", &ffplay_h.to_string()])
                .args(["-left", &i32::from(ffplay_x).to_string()])
                .args(["-top", &i32::from(ffplay_y).to_string()])
                .arg(&path_str);
            apply_pulse_env_defaults(&mut cmd);
            match cmd
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    self.ffplay_process = Some(child);
                }
                Err(e) => {
                    eprintln!("aurora-wm: ffplay launch failed: {e}");
                }
            }
            // Do not open the internal media panel for playable files
            return Ok(());
        }

        let media_geom = self.media_geometry(slot);
        let image_preview = if entry.kind == FileKind::Image {
            render_image_preview(
                &entry.path,
                i32::from(media_geom.2) - 64,
                i32::from(media_geom.3) - 146,
            )
        } else {
            None
        };
        let state = MediaState {
            entry,
            playing: false,
            progress: 0.0,
            text_lines,
            text_scroll: 0,
            text_cursor_line: 0,
            text_cursor_col: 0,
            text_undo: Vec::new(),
            editing: false,
            file_info,
            image_preview,
            notice: None,
        };
        self.media = Some(state.clone());
        self.media_slots[slot] = Some(state);
        self.media_front = true;
        self.media_front_slot = Some(slot);
        self.folder_front = false;
        self.settings_front = false;
        self.conn.configure_window(
            self.ui.media[slot],
            &ConfigureWindowAux::new()
                .x(i32::from(media_geom.0))
                .y(i32::from(media_geom.1))
                .width(u32::from(media_geom.2))
                .height(u32::from(media_geom.3))
                .stack_mode(StackMode::ABOVE),
        )?;
        self.conn.map_window(self.ui.media[slot])?;
        self.redraw_media_slot(slot)?;
        self.raise_media()?;

        Ok(())
    }

    fn handle_media_click(&mut self, slot: usize, button: u8, x: i32, y: i32) -> AnyResult<()> {
        let (_, _, w, h) = self.media_geometry(slot);
        if button == 3 {
            self.media_context_open = Some((slot, x, y));
            self.media_trash_prompt = None;
            self.redraw_media_slot(slot)?;
            self.raise_media()?;
            return Ok(());
        }
        if x >= i32::from(w) - 43 && x <= i32::from(w) - 19 && (17..=41).contains(&y) {
            self.media_slots[slot] = None;
            if self.media_front_slot == Some(slot) {
                self.media_front_slot = None;
                self.media_front = false;
            }
            self.media = self.media_slots.iter().rev().find_map(|m| m.clone());
            self.conn.unmap_window(self.ui.media[slot])?;
            self.stop_ffplay_process();
            return Ok(());
        }
        if let Some(action) = self.media_context_action_at(slot, x, y) {
            self.run_media_context_action(slot, action)?;
            return Ok(());
        }
        self.media_context_open = None;
        self.media_trash_prompt = None;
        let active_text_selection = self
            .media_text_selection
            .as_ref()
            .filter(|selection| selection.slot == slot)
            .cloned();
        if let Some(media) = self.media_slots.get_mut(slot).and_then(|m| m.as_mut()) {
            if x >= 24 && x <= i32::from(w) - 92 && (18..=38).contains(&y) {
                copy_text_to_clipboard(&media.entry.path.to_string_lossy());
                media.notice = Some("Copied full path".to_string());
                self.redraw_media_slot(slot)?;
                return Ok(());
            }
            if media.entry.kind == FileKind::Text
                && x >= i32::from(w) - 78
                && x <= i32::from(w) - 50
                && (17..=41).contains(&y)
            {
                if media.editing {
                    let _ = fs::write(&media.entry.path, media.text_lines.join("\n"));
                    media.notice = Some("Saved".to_string());
                } else if media.text_lines.is_empty() {
                    media.text_lines.push(String::new());
                    media.text_cursor_line = 0;
                    media.text_cursor_col = 0;
                }
                media.editing = !media.editing;
                self.redraw_media_slot(slot)?;
                return Ok(());
            }
            if media.entry.kind == FileKind::Text {
                let preview_x = 18;
                let preview_y = 58;
                let preview_w = i32::from(w) - 48;
                let preview_h = i32::from(h) - 130;
                if active_text_selection.is_some() {
                    let (bx, by, bw, bh) =
                        media_text_copy_button_rect(preview_x, preview_y, preview_w, preview_h);
                    if x >= bx && x <= bx + bw && y >= by && y <= by + bh {
                        if let Some(selection) = active_text_selection.as_ref() {
                            let selected = selected_text_from_lines(&media.text_lines, selection);
                            if !selected.is_empty() {
                                copy_text_to_clipboard(&selected);
                                media.notice = Some("Copied selection".to_string());
                            }
                        }
                        self.redraw_media_slot(slot)?;
                        return Ok(());
                    }
                }
                if x >= preview_x
                    && x <= preview_x + preview_w
                    && y >= preview_y
                    && y <= preview_y + preview_h
                {
                    if media.text_lines.is_empty() {
                        media.text_lines.push(String::new());
                    }
                    let line_h = 19;
                    let clicked = ((y - preview_y - 12).max(0) / line_h) as usize;
                    let line_idx =
                        (media.text_scroll + clicked).min(media.text_lines.len().saturating_sub(1));
                    let text_x = preview_x + 42;
                    let line = media
                        .text_lines
                        .get(line_idx)
                        .map(String::as_str)
                        .unwrap_or("");
                    media.text_cursor_line = line_idx;
                    media.text_cursor_col = cursor_col_for_x(&self.regular, line, x - text_x, 13.0);
                    self.media_text_selection = Some(MediaTextSelection {
                        slot,
                        start_line: line_idx,
                        start_col: media.text_cursor_col,
                        end_line: line_idx,
                        end_col: media.text_cursor_col,
                    });
                    self.media_text_selecting = true;
                    self.media_text_selection_redraw_at = Some(Instant::now());
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
            }
            if media.entry.kind == FileKind::Other {
                let preview_y = 58;
                let preview_h = i32::from(h) - 130;
                if let Some(line) = unknown_file_info_line(media, y - preview_y) {
                    copy_text_to_clipboard(&line);
                    media.notice = Some("Copied text".to_string());
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
                if y >= preview_y + preview_h - 56 && y <= preview_y + preview_h - 22 {
                    media.entry.kind = FileKind::Text;
                    media.text_lines = read_text_lines_limited(&media.entry.path, 5000);
                    media.text_scroll = 0;
                    media.text_cursor_line = 0;
                    media.text_cursor_col = 0;
                    media.text_undo.clear();
                    media.notice = None;
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
            }
            let playable = matches!(media.entry.kind, FileKind::Audio | FileKind::Video);
            let controls_y = i32::from(h) - 62;
            if playable
                && x >= 24
                && x <= i32::from(w) - 24
                && y >= controls_y
                && y <= controls_y + 42
            {
                let bar_x = 150;
                let bar_w = i32::from(w) - bar_x - 48;
                if x >= bar_x && x <= bar_x + bar_w {
                    media.progress = ((x - bar_x) as f32 / bar_w.max(1) as f32).clamp(0.0, 1.0);
                    media.playing = true;
                    // Seek: write to named pipe
                    let seek_pct = (media.progress * 100.0) as i32;
                    if let Ok(f) = std::fs::OpenOptions::new()
                        .write(true)
                        .open("/tmp/aurora-player-control")
                    {
                        use std::io::Write;
                        let mut w = f;
                        let _ = w.write_all(format!("seek {}\n", seek_pct).as_bytes());
                    }
                } else {
                    media.playing = !media.playing;
                    // Pause/resume: write to named pipe
                    if let Ok(f) = std::fs::OpenOptions::new()
                        .write(true)
                        .open("/tmp/aurora-player-control")
                    {
                        use std::io::Write;
                        let mut w = f;
                        let _ = w.write_all(b"pause\n");
                    }
                }
                self.media = self.media_slots.iter().rev().find_map(|m| m.clone());
                self.redraw_media_slot(slot)?;
            }
        }
        self.raise_media()?;
        Ok(())
    }

    fn media_context_action_at(&self, slot: usize, x: i32, y: i32) -> Option<MediaContextAction> {
        if self.media_trash_prompt == Some(slot) {
            let (_, _, w, h) = self.media_geometry(slot);
            let box_w = 310;
            let box_h = 126;
            let px = (i32::from(w) - box_w) / 2;
            let py = (i32::from(h) - box_h) / 2;
            if x >= px + 48 && x <= px + 132 && y >= py + 82 && y <= py + 112 {
                return Some(MediaContextAction::ConfirmTrash);
            }
            if x >= px + 174 && x <= px + 258 && y >= py + 82 && y <= py + 112 {
                return Some(MediaContextAction::CancelTrash);
            }
        }
        let (ctx_slot, ctx_x, ctx_y) = self.media_context_open?;
        if ctx_slot != slot {
            return None;
        }
        let (_, _, w, h) = self.media_geometry(slot);
        let menu_x = ctx_x.min(i32::from(w) - 184).max(12);
        let menu_y = ctx_y.min(i32::from(h) - 112).max(50);
        if x < menu_x || x > menu_x + 172 || y < menu_y || y > menu_y + 96 {
            return None;
        }
        match (y - menu_y) / 29 {
            0 => Some(MediaContextAction::Rename),
            1 => Some(MediaContextAction::CopyImage),
            2 => Some(MediaContextAction::MoveTrash),
            _ => None,
        }
    }

    fn run_media_context_action(
        &mut self,
        slot: usize,
        action: MediaContextAction,
    ) -> AnyResult<()> {
        match action {
            MediaContextAction::Rename => {
                if let Some(media) = self.media_slots.get_mut(slot).and_then(|m| m.as_mut()) {
                    media.notice = Some("Rename from the folder context menu".to_string());
                }
                self.media_context_open = None;
            }
            MediaContextAction::CopyImage => {
                let mut copied_image: Option<PathBuf> = None;
                if let Some(media) = self.media_slots.get_mut(slot).and_then(|m| m.as_mut()) {
                    if media.entry.kind == FileKind::Image {
                        copy_image_to_clipboard(&media.entry.path);
                        copied_image = Some(media.entry.path.clone());
                        media.notice = Some("Copied image to clipboard".to_string());
                    } else {
                        copy_text_to_clipboard(&media.entry.path.to_string_lossy());
                        media.notice = Some("Copied path".to_string());
                    }
                }
                if let Some(path) = copied_image {
                    self.last_seen_clipboard_image_sig = clipboard_file_image_signature(&path);
                    self.remember_clipboard_item(ClipboardItem::Image(path));
                    if self.clipboard_menu_visible {
                        self.redraw_clipboard_menu()?;
                    }
                }
                self.media_context_open = None;
            }
            MediaContextAction::MoveTrash => {
                self.media_trash_prompt = Some(slot);
                self.media_context_open = None;
            }
            MediaContextAction::ConfirmTrash => {
                if let Some(media) = self.media_slots.get(slot).and_then(|m| m.as_ref()).cloned() {
                    if move_to_trash(&media.entry.path).is_ok() {
                        self.media_slots[slot] = None;
                        self.media = self.media_slots.iter().rev().find_map(|m| m.clone());
                        self.conn.unmap_window(self.ui.media[slot])?;
                        self.refresh_folder_entries();
                        self.folder_info = Some("Moved to Trash".to_string());
                        self.redraw_folder()?;
                    } else if let Some(media) =
                        self.media_slots.get_mut(slot).and_then(|m| m.as_mut())
                    {
                        media.notice = Some("Could not move to Trash".to_string());
                    }
                }
                self.media_trash_prompt = None;
                self.media_context_open = None;
            }
            MediaContextAction::CancelTrash => {
                self.media_trash_prompt = None;
                self.media_context_open = None;
            }
        }
        if self
            .media_slots
            .get(slot)
            .and_then(|m| m.as_ref())
            .is_some()
        {
            self.redraw_media_slot(slot)?;
        }
        Ok(())
    }

    fn handle_media_motion(
        &mut self,
        slot: usize,
        x: i32,
        y: i32,
        button_down: bool,
    ) -> AnyResult<()> {
        if !button_down {
            if self.media_text_selecting {
                self.media_text_selecting = false;
                self.media_text_selection_redraw_at = None;
                self.erase_media_live_selection()?;
                self.redraw_media_slot(slot)?;
            }
            return Ok(());
        }
        if !self.media_text_selecting {
            return Ok(());
        }
        let Some(selection_slot) = self
            .media_text_selection
            .as_ref()
            .map(|selection| selection.slot)
        else {
            return Ok(());
        };
        if selection_slot != slot {
            return Ok(());
        }
        let (_, _, w, h) = self.media_geometry(slot);
        let Some(media) = self.media_slots.get(slot).and_then(|m| m.as_ref()) else {
            return Ok(());
        };
        if media.entry.kind != FileKind::Text {
            return Ok(());
        }
        let preview_x = 18;
        let preview_y = 58;
        let preview_w = i32::from(w) - 48;
        let preview_h = i32::from(h) - 130;
        if x < preview_x || x > preview_x + preview_w || y < preview_y || y > preview_y + preview_h
        {
            return Ok(());
        }
        let (line, col) = text_position_for_point(media, &self.regular, x, y, preview_x, preview_y);
        let Some(selection) = self.media_text_selection.as_mut() else {
            return Ok(());
        };
        if selection.end_line == line && selection.end_col == col {
            return Ok(());
        }
        selection.end_line = line;
        selection.end_col = col;
        let should_redraw = self
            .media_text_selection_redraw_at
            .is_none_or(|last| last.elapsed() >= Duration::from_millis(16));
        if should_redraw {
            self.media_text_selection_redraw_at = Some(Instant::now());
            self.update_media_live_selection(slot)?;
        }
        Ok(())
    }

    fn handle_media_release(&mut self, slot: usize) -> AnyResult<()> {
        self.media_text_selecting = false;
        self.media_text_selection_redraw_at = None;
        self.erase_media_live_selection()?;
        let Some(selection) = self.media_text_selection.as_ref().cloned() else {
            return Ok(());
        };
        if selection.slot != slot {
            return Ok(());
        }
        self.redraw_media_slot(slot)?;
        Ok(())
    }

    fn erase_media_live_selection(&mut self) -> AnyResult<()> {
        if self.media_text_live_rects.is_empty() {
            return Ok(());
        }
        let slot = self
            .media_text_selection
            .as_ref()
            .map(|s| s.slot)
            .unwrap_or(0);
        let rects = std::mem::take(&mut self.media_text_live_rects);
        self.draw_xor_rects(self.ui.media[slot], &rects)?;
        Ok(())
    }

    fn update_media_live_selection(&mut self, slot: usize) -> AnyResult<()> {
        let rects = self.media_text_selection_rects(slot);
        if same_rects(&rects, &self.media_text_live_rects) {
            return Ok(());
        }
        self.erase_media_live_selection()?;
        if !rects.is_empty() {
            self.draw_xor_rects(self.ui.media[slot], &rects)?;
            self.media_text_live_rects = rects;
        }
        Ok(())
    }

    fn media_text_selection_rects(&self, slot: usize) -> Vec<Rectangle> {
        let Some(media) = self.media_slots.get(slot).and_then(|m| m.as_ref()) else {
            return Vec::new();
        };
        let Some(selection) = self
            .media_text_selection
            .as_ref()
            .filter(|selection| selection.slot == slot)
        else {
            return Vec::new();
        };
        let (_, _, w, h) = self.media_geometry(slot);
        let preview_x = 18;
        let preview_y = 58;
        let preview_w = i32::from(w) - 48;
        let preview_h = i32::from(h) - 130;
        let line_h = 19;
        let max_lines = ((preview_h - 20) / line_h).max(1) as usize;
        let start = media
            .text_scroll
            .min(media.text_lines.len().saturating_sub(1));
        let text_x = preview_x + 42;
        let (sel_start, sel_end) = normalized_media_selection(selection);
        let mut rects = Vec::new();
        for idx in 0..max_lines {
            let line_no = start + idx;
            if line_no > sel_end.0 || line_no >= media.text_lines.len() {
                break;
            }
            if line_no < sel_start.0 {
                continue;
            }
            let Some(line) = media.text_lines.get(line_no) else {
                continue;
            };
            let line_len = line.chars().count();
            let start_col = if line_no == sel_start.0 {
                sel_start.1.min(line_len)
            } else {
                0
            };
            let end_col = if line_no == sel_end.0 {
                sel_end.1.min(line_len)
            } else {
                line_len
            };
            if end_col <= start_col {
                continue;
            }
            let sx = text_x + fast_text_width_cols(&self.regular, line, 0, start_col, 13.0);
            let sw = fast_text_width_cols(&self.regular, line, start_col, end_col, 13.0).max(3);
            let yy = preview_y + 13 + idx as i32 * line_h;
            let x = sx.max(preview_x).min(preview_x + preview_w - 1) as i16;
            let y = yy.max(preview_y).min(preview_y + preview_h - 1) as i16;
            let width = sw.min(preview_x + preview_w - i32::from(x)).max(1) as u16;
            rects.push(Rectangle {
                x,
                y,
                width,
                height: 16,
            });
        }
        rects
    }

    fn handle_media_scroll(&mut self, slot: usize, button: u8) -> AnyResult<()> {
        let Some(media) = self.media_slots.get_mut(slot).and_then(|m| m.as_mut()) else {
            return Ok(());
        };
        if media.entry.kind != FileKind::Text {
            return Ok(());
        }
        let old = media.text_scroll;
        let max_scroll = media.text_lines.len().saturating_sub(1);
        if button == 4 {
            media.text_scroll = media.text_scroll.saturating_sub(4);
        } else {
            media.text_scroll = (media.text_scroll + 4).min(max_scroll);
        }
        if media.text_scroll != old {
            self.redraw_media_slot(slot)?;
        }
        Ok(())
    }

    fn handle_media_key(&mut self, slot: usize, ev: KeyPressEvent) -> AnyResult<()> {
        let (_, _, _, h) = self.media_geometry(slot);
        let visible_lines = ((i32::from(h) - 150) / 19).max(1) as usize;
        let active_selection = self
            .media_text_selection
            .as_ref()
            .filter(|selection| selection.slot == slot)
            .cloned();
        let Some(media) = self.media_slots.get_mut(slot).and_then(|m| m.as_mut()) else {
            return Ok(());
        };
        if media.entry.kind != FileKind::Text || !media.editing {
            return Ok(());
        }
        let mapping = self.conn.get_keyboard_mapping(ev.detail, 1)?.reply()?;
        let shifted = u16::from(ev.state) & u16::from(KeyButMask::SHIFT) != 0;
        let column = if shifted && mapping.keysyms_per_keycode > 1 {
            1
        } else {
            0
        };
        let Some(&keysym) = mapping.keysyms.get(column) else {
            return Ok(());
        };
        if media.text_lines.is_empty() {
            media.text_lines.push(String::new());
        }
        let ctrl = u16::from(ev.state) & u16::from(KeyButMask::CONTROL) != 0;
        if ctrl {
            match keysym {
                0x61 | 0x41 => {
                    let last_line = media.text_lines.len().saturating_sub(1);
                    let last_col = media
                        .text_lines
                        .get(last_line)
                        .map(|line| line.chars().count())
                        .unwrap_or(0);
                    self.media_text_selection = Some(MediaTextSelection {
                        slot,
                        start_line: 0,
                        start_col: 0,
                        end_line: last_line,
                        end_col: last_col,
                    });
                    self.media_text_selecting = false;
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
                0x63 | 0x43 => {
                    if let Some(selection) = active_selection.as_ref() {
                        let selected = selected_text_from_lines(&media.text_lines, selection);
                        if !selected.is_empty() {
                            copy_text_to_clipboard(&selected);
                            media.notice = Some("Copied selection".to_string());
                        }
                    }
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
                0x78 | 0x58 => {
                    if let Some(selection) = active_selection.as_ref() {
                        let selected = selected_text_from_lines(&media.text_lines, selection);
                        if !selected.is_empty() {
                            copy_text_to_clipboard(&selected);
                            push_text_undo(media);
                            delete_text_selection(media, selection);
                            self.media_text_selection = None;
                            media.notice = Some("Cut selection".to_string());
                        }
                    }
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
                0x76 | 0x56 => {
                    if let Some(text) = read_text_clipboard() {
                        if !text.is_empty() {
                            push_text_undo(media);
                            if let Some(selection) = active_selection.as_ref() {
                                delete_text_selection(media, selection);
                                self.media_text_selection = None;
                            }
                            insert_text_at_cursor(media, &text);
                        }
                    }
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
                0x7a | 0x5a => {
                    if let Some(previous) = media.text_undo.pop() {
                        media.text_lines = previous;
                        media.text_cursor_line = media
                            .text_cursor_line
                            .min(media.text_lines.len().saturating_sub(1));
                        media.text_cursor_col = media
                            .text_lines
                            .get(media.text_cursor_line)
                            .map(|line| media.text_cursor_col.min(line.chars().count()))
                            .unwrap_or(0);
                        self.media_text_selection = None;
                    }
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
        match keysym {
            0xff08 => {
                if let Some(selection) = active_selection.as_ref() {
                    push_text_undo(media);
                    delete_text_selection(media, selection);
                    self.media_text_selection = None;
                    self.redraw_media_slot(slot)?;
                    return Ok(());
                }
                let line_idx = media
                    .text_cursor_line
                    .min(media.text_lines.len().saturating_sub(1));
                let col = media
                    .text_cursor_col
                    .min(media.text_lines[line_idx].chars().count());
                if col > 0 {
                    push_text_undo(media);
                    let byte_idx = nth_char_byte(&media.text_lines[line_idx], col - 1);
                    media.text_lines[line_idx].remove(byte_idx);
                    media.text_cursor_col = col - 1;
                } else if line_idx > 0 {
                    push_text_undo(media);
                    let removed = media.text_lines.remove(line_idx);
                    media.text_cursor_line = line_idx - 1;
                    media.text_cursor_col =
                        media.text_lines[media.text_cursor_line].chars().count();
                    media.text_lines[media.text_cursor_line].push_str(&removed);
                }
            }
            0xff0d => {
                if let Some(selection) = active_selection.as_ref() {
                    push_text_undo(media);
                    delete_text_selection(media, selection);
                    self.media_text_selection = None;
                } else {
                    push_text_undo(media);
                }
                let line_idx = media
                    .text_cursor_line
                    .min(media.text_lines.len().saturating_sub(1));
                let col = media
                    .text_cursor_col
                    .min(media.text_lines[line_idx].chars().count());
                let byte_idx = nth_char_byte(&media.text_lines[line_idx], col);
                let rest = media.text_lines[line_idx].split_off(byte_idx);
                media.text_lines.insert(line_idx + 1, rest);
                media.text_cursor_line = line_idx + 1;
                media.text_cursor_col = 0;
            }
            0xff51 => {
                media.text_cursor_col = media.text_cursor_col.saturating_sub(1);
            }
            0xff53 => {
                let line_idx = media
                    .text_cursor_line
                    .min(media.text_lines.len().saturating_sub(1));
                let len = media.text_lines[line_idx].chars().count();
                media.text_cursor_col = (media.text_cursor_col + 1).min(len);
            }
            0xff52 => {
                media.text_cursor_line = media.text_cursor_line.saturating_sub(1);
                let len = media.text_lines[media.text_cursor_line].chars().count();
                media.text_cursor_col = media.text_cursor_col.min(len);
            }
            0xff54 => {
                media.text_cursor_line =
                    (media.text_cursor_line + 1).min(media.text_lines.len().saturating_sub(1));
                let len = media.text_lines[media.text_cursor_line].chars().count();
                media.text_cursor_col = media.text_cursor_col.min(len);
            }
            0xff50 => media.text_cursor_col = 0,
            0xff57 => {
                let line_idx = media
                    .text_cursor_line
                    .min(media.text_lines.len().saturating_sub(1));
                media.text_cursor_col = media.text_lines[line_idx].chars().count();
            }
            0x20..=0x7e => {
                if let Some(selection) = active_selection.as_ref() {
                    push_text_undo(media);
                    delete_text_selection(media, selection);
                    self.media_text_selection = None;
                } else {
                    push_text_undo(media);
                }
                let ch = char::from_u32(keysym).unwrap();
                let line_idx = media
                    .text_cursor_line
                    .min(media.text_lines.len().saturating_sub(1));
                let col = media
                    .text_cursor_col
                    .min(media.text_lines[line_idx].chars().count());
                let byte_idx = nth_char_byte(&media.text_lines[line_idx], col);
                media.text_lines[line_idx].insert(byte_idx, ch);
                media.text_cursor_col = col + 1;
            }
            _ => return Ok(()),
        }
        media.text_cursor_line = media
            .text_cursor_line
            .min(media.text_lines.len().saturating_sub(1));
        let line_len = media.text_lines[media.text_cursor_line].chars().count();
        media.text_cursor_col = media.text_cursor_col.min(line_len);
        if media.text_cursor_line < media.text_scroll {
            media.text_scroll = media.text_cursor_line;
        } else if media.text_cursor_line >= media.text_scroll + visible_lines {
            media.text_scroll = media.text_cursor_line.saturating_sub(visible_lines - 1);
        }
        self.redraw_media_slot(slot)?;
        Ok(())
    }

    fn advance_internal_media(&mut self) -> AnyResult<bool> {
        let mut changed = false;
        // Read real playback progress from the C player's progress file
        let file_progress: Option<f32> = std::fs::read_to_string("/tmp/aurora-player-progress")
            .ok()
            .and_then(|s| s.trim().parse::<f32>().ok());

        for slot in 0..MEDIA_SLOT_COUNT {
            let Some(media) = self.media_slots.get_mut(slot).and_then(|m| m.as_mut()) else {
                continue;
            };
            if !media.playing || !matches!(media.entry.kind, FileKind::Audio | FileKind::Video) {
                continue;
            }
            if let Some(p) = file_progress {
                let clamped = p.clamp(0.0, 1.0);
                if (clamped - media.progress).abs() > 0.001 {
                    media.progress = clamped;
                    self.redraw_media_slot(slot)?;
                    changed = true;
                }
            }
        }
        if changed {
            self.media = self.media_slots.iter().rev().find_map(|m| m.clone());
        }
        Ok(changed)
    }

    fn handle_app_menu_click(&mut self, button: u8, x: i32, y: i32) -> AnyResult<()> {
        if self.app_menu_more && x >= 270 && (button == 4 || button == 5) {
            let entries = read_desktop_entries();
            let max_scroll = entries.len().saturating_sub(15);
            if button == 4 {
                self.app_menu_scroll = self.app_menu_scroll.saturating_sub(3);
            } else {
                self.app_menu_scroll = (self.app_menu_scroll + 3).min(max_scroll);
            }
            self.redraw_app_menu()?;
            return Ok(());
        }
        if self.app_menu_more && x >= 270 && button == 1 {
            let entries = read_desktop_entries();
            let visible = 15usize;
            let start = self.app_menu_scroll.min(entries.len().saturating_sub(1));
            let mut cy_draw = 56;
            let mut current = String::new();
            for entry in entries.iter().skip(start).take(visible) {
                if entry.category != current {
                    current = entry.category.clone();
                    cy_draw += 22;
                }
                if (cy_draw - 14..=cy_draw + 8).contains(&y) {
                    self.spawn_configured_app(&entry.command, None);
                    self.app_menu_visible = false;
                    self.app_menu_more = false;
                    self.app_menu_scroll = 0;
                    self.conn.unmap_window(self.ui.app_menu)?;
                    return Ok(());
                }
                cy_draw += 24;
            }
            return Ok(());
        }
        let idx = (y - 53) / 42;
        if idx < 0 {
            return Ok(());
        }
        let apps = app_menu_items();
        let Some(item) = apps.get(idx as usize) else {
            return Ok(());
        };
        match item.action {
            AppAction::Terminal => self.launch_terminal(),
            AppAction::Browser => self.launch_browser(),
            AppAction::Pictures => self.show_folder(FolderMode::Pictures, true)?,
            AppAction::Music => self.show_folder(FolderMode::Music, true)?,
            AppAction::Videos => self.show_folder(FolderMode::Videos, true)?,
            AppAction::Settings => {
                self.settings_visible = true;
                self.settings_front = true;
                self.folder_front = false;
                self.media_front = false;
                self.conn.map_window(self.ui.settings)?;
                self.raise_ui()?;
                self.redraw_settings()?;
            }
            AppAction::More => {
                self.app_menu_more = !self.app_menu_more;
                self.app_menu_scroll = 0;
                let menu = self.app_menu_geometry();
                self.conn.configure_window(
                    self.ui.app_menu,
                    &ConfigureWindowAux::new()
                        .width(u32::from(menu.2))
                        .height(u32::from(menu.3)),
                )?;
                self.redraw_app_menu()?;
                return Ok(());
            }
        }
        self.app_menu_visible = false;
        self.app_menu_more = false;
        self.app_menu_scroll = 0;
        self.conn.unmap_window(self.ui.app_menu)?;
        Ok(())
    }

    fn apply_display_mode(&mut self, idx: usize) {
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

    fn current_display_output(&self) -> Option<&str> {
        self.display_modes
            .iter()
            .find(|mode| mode.current)
            .or_else(|| self.display_modes.get(self.settings.selected_mode))
            .and_then(|mode| mode.output.as_deref())
    }

    fn apply_sleep_timeout(&self) {
        let mut cmd = Command::new("xset");
        cmd.env("DISPLAY", &self.display);
        if self.settings.sleep_after_secs == 0 {
            cmd.args(["s", "off"]);
        } else {
            cmd.args(["s", &self.settings.sleep_after_secs.to_string()]);
        }
        spawn_detached(cmd);
    }

    fn set_compositor_enabled(&mut self, enabled: bool) -> AnyResult<()> {
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

    fn set_power_mode(&mut self, mode: PowerMode) -> AnyResult<()> {
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

    fn selected_app_command(&self, kind: DefaultAppKind) -> &str {
        match kind {
            DefaultAppKind::Terminal => &self.settings.terminal_command,
            DefaultAppKind::Browser => &self.settings.browser_command,
            DefaultAppKind::Photo => &self.settings.photo_command,
            DefaultAppKind::Video => &self.settings.video_command,
        }
    }

    fn available_apps(&self, kind: DefaultAppKind) -> &[InstalledApp] {
        match kind {
            DefaultAppKind::Terminal => &self.terminal_apps,
            DefaultAppKind::Browser => &self.browser_apps,
            DefaultAppKind::Photo => &self.photo_apps,
            DefaultAppKind::Video => &self.video_apps,
        }
    }

    fn set_selected_app_command(&mut self, kind: DefaultAppKind, command: String) {
        match kind {
            DefaultAppKind::Terminal => self.settings.terminal_command = command,
            DefaultAppKind::Browser => self.settings.browser_command = command,
            DefaultAppKind::Photo => self.settings.photo_command = command,
            DefaultAppKind::Video => self.settings.video_command = command,
        }
    }

    fn test_terminal_launch(&mut self, command: &str, label: &str) {
        self.settings.app_status = Some(
            if !command.trim().is_empty() && self.spawn_configured_app(command, None) {
                format!("Launched {label}.")
            } else {
                format!("Could not launch {label}; try another terminal.")
            },
        );
    }

    fn launch_terminal(&mut self) {
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

    fn launch_browser(&mut self) {
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

    fn spawn_configured_app(&self, command: &str, path: Option<&Path>) -> bool {
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

    fn spawn_first_available(&self, names: &[&str], args: &[&str]) {
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

    fn resize_to_root(&mut self) -> AnyResult<()> {
        let geom = self.conn.get_geometry(self.root)?.reply()?;
        if geom.width == self.screen_width && geom.height == self.screen_height {
            return Ok(());
        }
        self.screen_width = geom.width;
        self.screen_height = geom.height;
        self.display_modes =
            read_display_modes(&self.display, self.screen_width, self.screen_height);
        if let Some(current) = self.display_modes.iter().position(|mode| mode.current) {
            self.settings.selected_mode = current;
        }
        self.wallpaper_cache = vec![None; WALLPAPERS.len()];
        self.wallpaper_pixels = render_wallpaper_pixels(
            WALLPAPERS[self.wallpaper_index].bytes,
            self.screen_width,
            self.screen_height,
        )?;
        self.wallpaper_cache[self.wallpaper_index] = Some(self.wallpaper_pixels.clone());
        self.hide_dock_more_menu()?;
        let dock = self.dock_geometry();
        let settings = self.settings_geometry();
        let folder = self.folder_geometry();
        let terminal = self.folder_terminal_geometry();
        let menu = self.app_menu_geometry();
        let clipboard_menu = self.clipboard_menu_geometry();
        self.conn.configure_window(
            self.ui.topbar,
            &ConfigureWindowAux::new()
                .x(0)
                .y(0)
                .width(u32::from(self.screen_width))
                .height(u32::from(TOPBAR_HEIGHT)),
        )?;
        self.conn.configure_window(
            self.ui.dock,
            &ConfigureWindowAux::new()
                .x(i32::from(dock.0))
                .y(i32::from(dock.1))
                .width(u32::from(dock.2))
                .height(u32::from(dock.3)),
        )?;
        self.conn.configure_window(
            self.ui.settings,
            &ConfigureWindowAux::new()
                .x(i32::from(settings.0))
                .y(i32::from(settings.1))
                .width(u32::from(settings.2))
                .height(u32::from(settings.3)),
        )?;
        self.conn.configure_window(
            self.ui.folder,
            &ConfigureWindowAux::new()
                .x(i32::from(folder.0))
                .y(i32::from(folder.1))
                .width(u32::from(folder.2))
                .height(u32::from(folder.3)),
        )?;
        self.conn.configure_window(
            self.ui.folder_terminal,
            &ConfigureWindowAux::new()
                .x(i32::from(terminal.0))
                .y(i32::from(terminal.1))
                .width(u32::from(terminal.2))
                .height(u32::from(terminal.3)),
        )?;
        self.conn.configure_window(
            self.ui.app_menu,
            &ConfigureWindowAux::new()
                .x(i32::from(menu.0))
                .y(i32::from(menu.1))
                .width(u32::from(menu.2))
                .height(u32::from(menu.3)),
        )?;
        let aurora_menu = self.aurora_menu_geometry();
        self.conn.configure_window(
            self.ui.aurora_menu,
            &ConfigureWindowAux::new()
                .x(i32::from(aurora_menu.0))
                .y(i32::from(aurora_menu.1))
                .width(u32::from(aurora_menu.2))
                .height(u32::from(aurora_menu.3)),
        )?;
        self.conn.configure_window(
            self.ui.clipboard_menu,
            &ConfigureWindowAux::new()
                .x(i32::from(clipboard_menu.0))
                .y(i32::from(clipboard_menu.1))
                .width(u32::from(clipboard_menu.2))
                .height(u32::from(clipboard_menu.3)),
        )?;
        self.conn.configure_window(
            self.ui.screenshot_overlay,
            &ConfigureWindowAux::new()
                .x(0)
                .y(0)
                .width(u32::from(self.screen_width))
                .height(u32::from(self.screen_height)),
        )?;
        for (idx, window) in self.ui.media.iter().copied().enumerate() {
            let media = self.media_geometry(idx);
            self.conn.configure_window(
                window,
                &ConfigureWindowAux::new()
                    .x(i32::from(media.0))
                    .y(i32::from(media.1))
                    .width(u32::from(media.2))
                    .height(u32::from(media.3)),
            )?;
        }
        self.redraw_everything()?;
        Ok(())
    }

    fn raise_ui(&self) -> AnyResult<()> {
        if self.settings_visible {
            self.conn.configure_window(
                self.ui.settings,
                &ConfigureWindowAux::new().stack_mode(if self.settings_front {
                    StackMode::ABOVE
                } else {
                    StackMode::BELOW
                }),
            )?;
        }
        self.conn.configure_window(
            self.ui.folder,
            &ConfigureWindowAux::new().stack_mode(if self.folder_front {
                StackMode::ABOVE
            } else {
                StackMode::BELOW
            }),
        )?;
        if self.folder_terminal.visible {
            self.conn.configure_window(
                self.ui.folder_terminal,
                &ConfigureWindowAux::new().stack_mode(if self.folder_front {
                    StackMode::ABOVE
                } else {
                    StackMode::BELOW
                }),
            )?;
        }
        for (idx, window) in self.ui.media.iter().copied().enumerate() {
            if self.media_slots.get(idx).and_then(|m| m.as_ref()).is_some() {
                self.conn.configure_window(
                    window,
                    &ConfigureWindowAux::new().stack_mode(if self.media_front_slot == Some(idx) {
                        StackMode::ABOVE
                    } else {
                        StackMode::BELOW
                    }),
                )?;
            }
        }
        if self.screenshot_mode {
            self.conn.configure_window(
                self.ui.screenshot_overlay,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
            self.conn.configure_window(
                self.ui.topbar,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        self.raise_chrome()?;
        if self.app_menu_visible {
            self.conn.configure_window(
                self.ui.app_menu,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        if self.aurora_menu_visible {
            self.conn.configure_window(
                self.ui.aurora_menu,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        if self.clipboard_menu_visible {
            self.conn.configure_window(
                self.ui.clipboard_menu,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        Ok(())
    }

    fn raise_chrome(&self) -> AnyResult<()> {
        self.conn.configure_window(
            self.ui.topbar,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?;
        if self.app_menu_visible {
            self.conn.configure_window(
                self.ui.app_menu,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        if self.aurora_menu_visible {
            self.conn.configure_window(
                self.ui.aurora_menu,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        if self.clipboard_menu_visible {
            self.conn.configure_window(
                self.ui.clipboard_menu,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        if self.dock_more_visible {
            self.conn.configure_window(
                self.ui.dock_more_menu,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        Ok(())
    }

    fn raise_media(&self) -> AnyResult<()> {
        if let Some(slot) = self.media_front_slot {
            self.conn.configure_window(
                self.ui.media[slot],
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        self.raise_chrome()
    }

    fn upload_canvas(&self, drawable: Drawable, canvas: &Canvas) -> AnyResult<()> {
        self.upload_canvas_at(drawable, canvas, 0, 0)
    }

    fn upload_canvas_at(
        &self,
        drawable: Drawable,
        canvas: &Canvas,
        x: i32,
        y: i32,
    ) -> AnyResult<()> {
        let img = Image::new(
            canvas.width,
            canvas.height,
            ScanlinePad::Pad32,
            self.depth,
            BitsPerPixel::B32,
            XrbImageOrder::LsbFirst,
            Cow::Borrowed(&canvas.data),
        )?;
        img.put(&self.conn, drawable, self.gc, x as i16, y as i16)?;
        Ok(())
    }

    fn draw_xor_rects(&self, drawable: Drawable, rects: &[Rectangle]) -> AnyResult<()> {
        if rects.is_empty() {
            return Ok(());
        }
        self.conn.change_gc(
            self.gc,
            &ChangeGCAux::new()
                .function(GX::XOR)
                .foreground(0x00af_e5f5)
                .line_width(2),
        )?;
        self.conn.poly_fill_rectangle(drawable, self.gc, rects)?;
        self.conn.change_gc(
            self.gc,
            &ChangeGCAux::new()
                .function(GX::COPY)
                .foreground(0)
                .line_width(1),
        )?;
        Ok(())
    }

    fn atom(&self, name: &[u8]) -> AnyResult<Atom> {
        Ok(self.conn.intern_atom(false, name)?.reply()?.atom)
    }

    fn paint_window_icon(
        &self,
        canvas: &mut Canvas,
        window: Window,
        x: i32,
        y: i32,
        size: i32,
    ) -> bool {
        let Ok(cookie) = self.conn.intern_atom(false, b"_NET_WM_ICON") else {
            return false;
        };
        let Ok(atom) = cookie.reply() else {
            return false;
        };
        let Ok(cookie) =
            self.conn
                .get_property(false, window, atom.atom, AtomEnum::CARDINAL, 0, 262_144)
        else {
            return false;
        };
        let Ok(reply) = cookie.reply() else {
            return false;
        };
        let Some(values) = reply.value32() else {
            return false;
        };
        let data = values.collect::<Vec<_>>();
        let mut pos = 0usize;
        let mut best: Option<(usize, usize, usize)> = None;
        while pos + 2 <= data.len() {
            let w = data[pos] as usize;
            let h = data[pos + 1] as usize;
            pos += 2;
            let count = w.saturating_mul(h);
            if w == 0 || h == 0 || pos + count > data.len() {
                break;
            }
            let score = (w as i32 - size).abs() + (h as i32 - size).abs();
            let replace = best
                .map(|(_, bw, bh)| score < (bw as i32 - size).abs() + (bh as i32 - size).abs())
                .unwrap_or(true);
            if replace {
                best = Some((pos, w, h));
            }
            pos += count;
        }
        let Some((start, w, h)) = best else {
            return false;
        };
        for yy in 0..size {
            for xx in 0..size {
                let sx = (xx as usize * w / size as usize).min(w - 1);
                let sy = (yy as usize * h / size as usize).min(h - 1);
                let argb = data[start + sy * w + sx];
                let a = ((argb >> 24) & 0xff) as u8;
                if a == 0 {
                    continue;
                }
                let r = ((argb >> 16) & 0xff) as u8;
                let g = ((argb >> 8) & 0xff) as u8;
                let b = (argb & 0xff) as u8;
                canvas.blend_pixel(x + xx, y + yy, Color::rgba(r, g, b, a));
            }
        }
        true
    }

    fn paint_desktop_icon(
        &mut self,
        canvas: &mut Canvas,
        window: Window,
        x: i32,
        y: i32,
        size: i32,
    ) -> bool {
        let class = self.window_class(window);
        let title = self.window_title(window).to_ascii_lowercase();
        let key = format!("{class}|{title}");
        if !self.icon_cache.contains_key(&key) {
            let icon = resolve_window_icon(&class, &title)
                .and_then(|path| fs::read(path).ok())
                .and_then(|bytes| decode_icon_pixels(&bytes, size).ok());
            self.icon_cache.insert(key.clone(), icon);
        }
        let Some(Some(pixels)) = self.icon_cache.get(&key) else {
            return false;
        };
        paint_rgba_pixels(canvas, pixels, x, y, size, size);
        true
    }

    fn dock_geometry(&self) -> (i16, i16, u16, u16) {
        let buttons = self.dock_button_count().max(5);
        let width = (buttons as u16 * 58 + 28)
            .min(self.screen_width.saturating_sub(24))
            .max(318u16.min(self.screen_width));
        let x = ((self.screen_width.saturating_sub(width)) / 2) as i16;
        let y = self.screen_height.saturating_sub(DOCK_HEIGHT + 4) as i16;
        (x, y, width, DOCK_HEIGHT)
    }

    fn settings_geometry(&self) -> (i16, i16, u16, u16) {
        let width = SETTINGS_TARGET_WIDTH
            .min(self.screen_width.saturating_sub(SETTINGS_MARGIN * 2))
            .max(SETTINGS_MIN_WIDTH.min(self.screen_width));
        let height = 578u16
            .min(
                self.screen_height
                    .saturating_sub(TOPBAR_HEIGHT + SETTINGS_MARGIN * 2),
            )
            .max(440.min(self.screen_height));
        let x = self.screen_width.saturating_sub(width + SETTINGS_MARGIN) as i16;
        let y = (TOPBAR_HEIGHT + SETTINGS_MARGIN) as i16;
        (x, y, width, height)
    }

    fn folder_geometry(&self) -> (i16, i16, u16, u16) {
        let width = self
            .folder_width
            .min(self.screen_width.saturating_sub(48))
            .max(FOLDER_MIN_WIDTH.min(self.screen_width));
        let height = self
            .folder_height
            .min(
                self.screen_height
                    .saturating_sub(TOPBAR_HEIGHT + DOCK_HEIGHT + 48),
            )
            .max(FOLDER_MIN_HEIGHT.min(self.screen_height));
        (24, (TOPBAR_HEIGHT + 26) as i16, width, height)
    }

    fn folder_terminal_geometry(&self) -> (i16, i16, u16, u16) {
        let folder = self.folder_geometry();
        let y = i32::from(folder.1) + i32::from(folder.3) + 8;
        let available = i32::from(self.screen_height).saturating_sub(y + 50);
        let width = self
            .folder_terminal_width
            .min(self.screen_width.saturating_sub(48))
            .max(TERMINAL_MIN_WIDTH.min(self.screen_width));
        let height = self
            .folder_terminal_height
            .min(available.max(i32::from(TERMINAL_MIN_HEIGHT)) as u16)
            .max(TERMINAL_MIN_HEIGHT.min(self.screen_height));
        (folder.0, y as i16, width, height)
    }

    fn ui_bottom_right_resize_hit(&self, target: UiResizeTarget, x: i16, y: i16) -> bool {
        let (_, _, width, height) = match target {
            UiResizeTarget::Folder => self.folder_geometry(),
            UiResizeTarget::FolderTerminal => self.folder_terminal_geometry(),
        };
        let width = i16::try_from(width).unwrap_or(i16::MAX);
        let height = i16::try_from(height).unwrap_or(i16::MAX);
        x >= width - RESIZE_CORNER && y >= height - RESIZE_CORNER
    }

    fn app_menu_geometry(&self) -> (i16, i16, u16, u16) {
        let width = if self.app_menu_more { 590u16 } else { 260u16 };
        let height = if self.app_menu_more { 500u16 } else { 360u16 };
        let dock = self.dock_geometry();
        let x = dock.0.max(18);
        let y = dock.1.saturating_sub(height as i16 + 10);
        (x, y, width, height)
    }

    fn aurora_menu_geometry(&self) -> (i16, i16, u16, u16) {
        let width = 390u16.min(self.screen_width.saturating_sub(24)).max(260);
        let height = if self.aurora_menu_about {
            276
        } else if self.aurora_menu_restart_confirm {
            220
        } else {
            168
        };
        (12, TOPBAR_HEIGHT as i16 + 8, width, height)
    }

    fn clipboard_menu_geometry(&self) -> (i16, i16, u16, u16) {
        let width = CLIPBOARD_MENU_WIDTH
            .min(self.screen_width.saturating_sub(24))
            .max(260.min(self.screen_width));
        let height = if self.clipboard_history.is_empty() {
            150
        } else {
            (56 + self.clipboard_page_content_height() + 10) as u16
        };
        let controls = self.topbar_controls();
        let mut x = controls.clipboard_x - i32::from(width) / 2;
        let max_x = i32::from(self.screen_width.saturating_sub(width)).saturating_sub(12);
        x = x.max(12).min(max_x.max(12));
        (x as i16, TOPBAR_HEIGHT as i16 + 4, width, height)
    }

    fn media_geometry(&self, slot: usize) -> (i16, i16, u16, u16) {
        let folder = self.folder_geometry();
        let width = MEDIA_WIDTH
            .min(self.screen_width.saturating_sub(48))
            .max(320.min(self.screen_width));
        let height = folder
            .3
            .min(self.screen_height.saturating_sub(TOPBAR_HEIGHT + 56))
            .max(300.min(self.screen_height));
        let desired_x = i32::from(folder.0) + i32::from(folder.2);
        let max_x = i32::from(self.screen_width.saturating_sub(width));
        let x = desired_x.min(max_x).max(0) as i16;
        let y = i32::from(folder.1) + (slot.min(4) as i32 * 10);
        (x, y as i16, width, height)
    }

    fn dock_button_count(&self) -> usize {
        let task_windows = self.task_client_windows();
        if task_windows.len() <= 10 {
            5 + task_windows.len()
        } else {
            5 + 10 + 1
        }
    }

    fn task_client_windows(&self) -> Vec<Window> {
        let mut windows = self
            .clients
            .iter()
            .filter_map(|(window, info)| {
                (info.workspace == self.active_workspace).then_some(*window)
            })
            .collect::<Vec<_>>();
        windows.sort_unstable();
        windows
    }

    fn dock_more_menu_geometry(&self) -> (i16, i16, u16, u16) {
        let (dx, dy, dw, _dh) = self.dock_geometry();
        let buttons = self.dock_button_count();
        let stride = 58;
        let total = buttons as i32 * stride - 12;
        let mut bx = (i32::from(dw) - total) / 2;
        bx += 15 * stride;
        let icon_x = bx + 7;
        let center_x = dx + icon_x as i16 + 22;

        let task_windows = self.task_client_windows();
        let hidden_count = task_windows.len().saturating_sub(10);
        let width = 240u16;
        let height = (hidden_count as u16 * 40 + 16).max(40);

        let mut x = center_x - (width as i16 / 2);
        if x + width as i16 > self.screen_width as i16 - 12 {
            x = self.screen_width as i16 - width as i16 - 12;
        }
        if x < 12 {
            x = 12;
        }
        let y = dy - height as i16 - 8;
        (x, y, width, height)
    }

    fn client_key_for(&self, window: Window) -> Option<Window> {
        if self.clients.contains_key(&window) {
            return Some(window);
        }
        self.clients
            .iter()
            .find_map(|(client, info)| (info.frame == window).then_some(*client))
    }

    fn client_or_ancestor_key_for(&self, window: Window) -> Option<Window> {
        if let Some(client) = self.client_key_for(window) {
            return Some(client);
        }
        let mut current = window;
        for _ in 0..8 {
            let Ok(cookie) = self.conn.query_tree(current) else {
                return None;
            };
            let Ok(reply) = cookie.reply() else {
                return None;
            };
            if reply.parent == self.root || reply.parent == x11rb::NONE {
                return self.client_key_for(reply.parent);
            }
            if let Some(client) = self.client_key_for(reply.parent) {
                return Some(client);
            }
            current = reply.parent;
        }
        None
    }

    fn is_ui_window(&self, window: Window) -> bool {
        window == self.ui.topbar
            || window == self.ui.dock
            || window == self.ui.settings
            || window == self.ui.folder
            || window == self.ui.folder_terminal
            || window == self.ui.screenshot_overlay
            || window == self.ui.app_menu
            || window == self.ui.aurora_menu
            || window == self.ui.clipboard_menu
            || window == self.ui.dock_more_menu
            || self.ui.media.contains(&window)
    }

    fn media_slot_for_window(&self, window: Window) -> Option<usize> {
        self.ui.media.iter().position(|&media| media == window)
    }
}

fn draw_card(c: &mut Canvas, x: i32, y: i32, w: i32, h: i32) {
    c.draw_round_rect(x, y, w, h, 12, Color::rgba(255, 255, 255, 184));
    c.draw_round_rect(x, y, w, h, 12, Color::rgba(214, 230, 237, 42));
    c.draw_rect(x + 12, y, w - 24, 1, CARD_LINE);
}

fn draw_metric_bar(
    c: &mut Canvas,
    font: &Font<'static>,
    x: i32,
    y: i32,
    name: &str,
    value: f32,
    suffix: &str,
) {
    c.draw_text(font, name, x, y - 1, 12.0, MUTED);
    c.draw_round_rect(x + 72, y, 104, 8, 4, Color::rgba(211, 225, 232, 170));
    c.draw_round_rect(
        x + 72,
        y,
        (104.0 * (value / 100.0).clamp(0.0, 1.0)) as i32,
        8,
        4,
        Color::rgba(116, 213, 198, 210),
    );
    c.draw_text_right(
        font,
        &format!("{value:.0}{suffix}"),
        x + 212,
        y - 6,
        12.0,
        INK,
    );
}

fn draw_info_row(c: &mut Canvas, font: &Font<'static>, x: i32, y: i32, key: &str, value: &str) {
    c.draw_text(font, key, x, y, 12.0, MUTED);
    c.draw_text(font, value, x + 62, y, 12.0, INK);
}

fn mask_has(mask: ConfigWindow, flag: ConfigWindow) -> bool {
    u16::from(mask) & u16::from(flag) != 0
}

fn hover_title_button(x: i16, y: i16) -> Option<TitleButton> {
    if !(8..=28).contains(&x) || !(6..=28).contains(&y) {
        if (31..=53).contains(&x) && (6..=28).contains(&y) {
            return Some(TitleButton::Minimize);
        }
        if (54..=76).contains(&x) && (6..=28).contains(&y) {
            return Some(TitleButton::Maximize);
        }
        return None;
    }
    Some(TitleButton::Close)
}

fn resize_corner_edges_for_frame(
    info: &ClientInfo,
    title_h: u16,
    x: i16,
    y: i16,
) -> Option<ResizeEdges> {
    let frame_h = i16::try_from(info.height + title_h).unwrap_or(i16::MAX);
    let width = i16::try_from(info.width).unwrap_or(i16::MAX);

    // Bottom-left corner: (0, frame_h) - within 20px
    let left = x.abs() <= 20 && (frame_h - y).abs() <= 20;

    // Bottom-right corner: (width, frame_h) - within 20px
    let right = (width - x).abs() <= 20 && (frame_h - y).abs() <= 20;

    if left || right {
        Some(ResizeEdges {
            left,
            right,
            top: false,
            bottom: true,
        })
    } else {
        None
    }
}

fn resize_corner_edges_for_client(info: &ClientInfo, x: i16, y: i16) -> Option<ResizeEdges> {
    let width = i16::try_from(info.width).unwrap_or(i16::MAX);
    let height = i16::try_from(info.height).unwrap_or(i16::MAX);

    // Bottom-left corner: (0, height) - within 20px
    let left = x.abs() <= 20 && (height - y).abs() <= 20;

    // Bottom-right corner: (width, height) - within 20px
    let right = (width - x).abs() <= 20 && (height - y).abs() <= 20;

    if left || right {
        Some(ResizeEdges {
            left,
            right,
            top: false,
            bottom: true,
        })
    } else {
        None
    }
}

fn resize_side_hint_for_frame(info: &ClientInfo, x: i16) -> bool {
    let width = i16::try_from(info.width).unwrap_or(i16::MAX);
    x <= RESIZE_EDGE || x >= width - RESIZE_EDGE
}

fn resize_side_hint_for_client(info: &ClientInfo, x: i16) -> bool {
    let width = i16::try_from(info.width).unwrap_or(i16::MAX);
    x <= RESIZE_EDGE || x >= width - RESIZE_EDGE
}

fn terminal_default_height_for(folder_height: u16, screen_height: u16) -> u16 {
    let y = i32::from(TOPBAR_HEIGHT) + 26 + i32::from(folder_height) + 8;
    i32::from(screen_height)
        .saturating_sub(y + 50)
        .max(i32::from(TERMINAL_MIN_HEIGHT)) as u16
}

fn client_uses_own_chrome(class: &str, title: &str) -> bool {
    let text = format!("{} {}", class, title.to_ascii_lowercase());
    ["firefox", "chromium", "google-chrome", "brave", "vivaldi"]
        .iter()
        .any(|needle| text.contains(needle))
}

fn client_is_ffplay(class: &str, title: &str) -> bool {
    let text = format!("{} {}", class, title.to_ascii_lowercase());
    text.contains("ffplay") || text.contains("aurora ffplay")
}

fn rounded_top_shape_rects(width: u16, height: u16, radius: i32) -> Vec<Rectangle> {
    let width_i = i32::from(width);
    let height_i = i32::from(height);
    let r = radius.max(0).min(width_i / 2).min(height_i);
    if r == 0 {
        return vec![Rectangle {
            x: 0,
            y: 0,
            width,
            height,
        }];
    }

    let mut rects = Vec::with_capacity(usize::try_from(r + 1).unwrap_or(1));
    for y in 0..r {
        let dy = y - r;
        let dx = ((r * r - dy * dy) as f64).sqrt().round() as i32;
        let inset = (r - dx).clamp(0, width_i / 2);
        let row_w = (width_i - inset * 2).max(0) as u16;
        if row_w > 0 {
            rects.push(Rectangle {
                x: inset as i16,
                y: y as i16,
                width: row_w,
                height: 1,
            });
        }
    }
    if height_i > r {
        rects.push(Rectangle {
            x: 0,
            y: r as i16,
            width,
            height: (height_i - r) as u16,
        });
    }
    rects
}

fn create_pointer_cursor(conn: &RustConnection, root: Window) -> AnyResult<Cursor> {
    create_standard_left_ptr_cursor(conn).or_else(|_| create_pixmap_pointer_cursor(conn, root))
}

fn create_standard_left_ptr_cursor(conn: &RustConnection) -> AnyResult<Cursor> {
    const XC_LEFT_PTR: u16 = 68;

    let font = conn.generate_id()?;
    let cursor = conn.generate_id()?;
    conn.open_font(font, b"cursor")?;
    conn.create_glyph_cursor(
        cursor,
        font,
        font,
        XC_LEFT_PTR,
        XC_LEFT_PTR + 1,
        0,
        0,
        0,
        0xffff,
        0xffff,
        0xffff,
    )?;
    conn.close_font(font)?;
    Ok(cursor)
}

fn create_pixmap_pointer_cursor(conn: &RustConnection, root: Window) -> AnyResult<Cursor> {
    let source = conn.generate_id()?;
    let mask = conn.generate_id()?;
    let source_gc = conn.generate_id()?;
    let mask_gc = conn.generate_id()?;
    let cursor = conn.generate_id()?;
    conn.create_pixmap(1, source, root, 40, 40)?;
    conn.create_pixmap(1, mask, root, 40, 40)?;
    conn.create_gc(
        source_gc,
        source,
        &CreateGCAux::new().foreground(0).background(0),
    )?;
    conn.create_gc(
        mask_gc,
        mask,
        &CreateGCAux::new().foreground(0).background(0),
    )?;
    let clear = [Rectangle {
        x: 0,
        y: 0,
        width: 40,
        height: 40,
    }];
    conn.poly_fill_rectangle(source, source_gc, &clear)?;
    conn.poly_fill_rectangle(mask, mask_gc, &clear)?;

    conn.change_gc(source_gc, &ChangeGCAux::new().foreground(1))?;
    conn.change_gc(mask_gc, &ChangeGCAux::new().foreground(1))?;
    let mask_points = [
        Point { x: 4, y: 2 },
        Point { x: 7, y: 37 },
        Point { x: 17, y: 26 },
        Point { x: 24, y: 39 },
        Point { x: 31, y: 35 },
        Point { x: 24, y: 23 },
        Point { x: 38, y: 22 },
    ];
    let source_points = [
        Point { x: 8, y: 7 },
        Point { x: 10, y: 29 },
        Point { x: 16, y: 21 },
        Point { x: 24, y: 34 },
        Point { x: 27, y: 32 },
        Point { x: 19, y: 18 },
        Point { x: 30, y: 18 },
    ];
    conn.fill_poly(
        mask,
        mask_gc,
        PolyShape::CONVEX,
        CoordMode::ORIGIN,
        &mask_points,
    )?;
    conn.fill_poly(
        source,
        source_gc,
        PolyShape::CONVEX,
        CoordMode::ORIGIN,
        &source_points,
    )?;
    conn.create_cursor(
        cursor,
        source,
        mask,
        u16::from(BLUE.r) * 257,
        u16::from(BLUE.g) * 257,
        u16::from(BLUE.b) * 257,
        0xffff,
        0xffff,
        0xffff,
        4,
        2,
    )?;
    conn.free_gc(source_gc)?;
    conn.free_gc(mask_gc)?;
    conn.free_pixmap(source)?;
    conn.free_pixmap(mask)?;
    Ok(cursor)
}

fn draw_workspace_icon(c: &mut Canvas, x: i32, y: i32, active: bool) {
    let fill = if active {
        Color::rgb(51, 116, 198)
    } else {
        Color::rgba(222, 242, 246, 62)
    };
    let stroke = if active {
        Color::rgba(200, 232, 255, 160)
    } else {
        Color::rgba(188, 230, 226, 156)
    };
    c.draw_round_rect(x, y, WORKSPACE_SIZE, WORKSPACE_SIZE, 5, stroke);
    c.draw_round_rect(
        x + 2,
        y + 2,
        WORKSPACE_SIZE - 4,
        WORKSPACE_SIZE - 4,
        4,
        fill,
    );
}

fn draw_add_workspace_icon(c: &mut Canvas, x: i32, cy: i32) {
    c.draw_round_rect(
        x,
        cy - WORKSPACE_SIZE / 2,
        WORKSPACE_SIZE,
        WORKSPACE_SIZE,
        6,
        Color::rgba(160, 238, 220, 38),
    );
    draw_round_line(c, x + 5, cy, x + WORKSPACE_SIZE - 5, cy, 2, MINT_LIGHT);
    draw_round_line(
        c,
        x + WORKSPACE_SIZE / 2,
        cy - 4,
        x + WORKSPACE_SIZE / 2,
        cy + 4,
        2,
        MINT_LIGHT,
    );
}

fn draw_sparkline(c: &mut Canvas, x: i32, y: i32, w: i32, h: i32, value: f64, color: Color) {
    if w <= 4 {
        return;
    }
    let points = 18;
    let seed = (value as u64).wrapping_mul(1103515245).wrapping_add(12345);
    let mut last_x = x;
    let mut last_y = y + h - 3;
    for i in 0..points {
        let px = x + i * w / (points - 1);
        let wiggle = ((seed >> (i % 12)) & 7) as i32;
        let amp = ((value.log10().max(0.0) * 4.0) as i32).min(h - 5);
        let py = y + h - 3 - ((i * 3 + wiggle) % (amp + 1).max(1));
        if i > 0 {
            c.draw_line(last_x, last_y, px, py, 2, Color { a: 180, ..color });
        }
        last_x = px;
        last_y = py;
    }
}

fn draw_sidebar_icon(c: &mut Canvas, idx: usize, cx: i32, cy: i32, color: Color) {
    match idx {
        0 => draw_sidebar_display_icon(c, cx, cy, color),
        1 => draw_power_icon(c, cx, cy, color),
        2 => draw_sidebar_wallpaper_icon(c, cx, cy, color),
        3 => draw_sidebar_audio_icon(c, cx, cy, color),
        4 => draw_sidebar_network_icon(c, cx, cy, color),
        5 => draw_sidebar_bluetooth_icon(c, cx, cy, color),
        6 => draw_sidebar_startup_icon(c, cx, cy, color),
        7 => draw_sidebar_apps_icon(c, cx, cy, color),
        _ => draw_sidebar_about_icon(c, cx, cy, color),
    }
}

fn draw_round_line(
    c: &mut Canvas,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    thickness: i32,
    color: Color,
) {
    c.draw_line(x0, y0, x1, y1, thickness, color);
    let radius = (thickness.max(1) + 1) / 2;
    c.draw_circle(x0, y0, radius, color);
    c.draw_circle(x1, y1, radius, color);
}

fn draw_arc(
    c: &mut Canvas,
    cx: i32,
    cy: i32,
    radius: i32,
    start_degrees: f32,
    end_degrees: f32,
    _steps: i32,
    thickness: i32,
    color: Color,
) {
    let r = radius as f32;
    let t = thickness as f32;
    let half_t = t / 2.0;

    let margin = thickness + 3;
    let x_min = (cx - radius - margin).max(0);
    let x_max = (cx + radius + margin).min(i32::from(c.width));
    let y_min = (cy - radius - margin).max(0);
    let y_max = (cy + radius + margin).min(i32::from(c.height));

    let get_end_point = |deg: f32| {
        let rad = deg.to_radians();
        (cx as f32 + rad.cos() * r, cy as f32 + rad.sin() * r)
    };
    let ep0 = get_end_point(start_degrees);
    let ep1 = get_end_point(end_degrees);

    for y in y_min..y_max {
        for x in x_min..x_max {
            let dx = x as f32 - cx as f32;
            let dy = y as f32 - cy as f32;
            let d_center = (dx * dx + dy * dy).sqrt();

            let mut angle = dy.atan2(dx).to_degrees();
            if angle < 0.0 {
                angle += 360.0;
            }

            let in_angle_range = if start_degrees <= end_degrees {
                angle >= start_degrees && angle <= end_degrees
            } else {
                angle >= start_degrees || angle <= end_degrees
            };

            let d = if in_angle_range {
                (d_center - r).abs()
            } else {
                let d0 = ((x as f32 - ep0.0).powi(2) + (y as f32 - ep0.1).powi(2)).sqrt();
                let d1 = ((x as f32 - ep1.0).powi(2) + (y as f32 - ep1.1).powi(2)).sqrt();
                d0.min(d1)
            };

            let coverage = if d <= half_t - 0.5 {
                1.0
            } else if d >= half_t + 0.5 {
                0.0
            } else {
                half_t + 0.5 - d
            };

            if coverage > 0.0 {
                let mut blended = color;
                blended.a = (color.a as f32 * coverage).round() as u8;
                c.blend_pixel(x, y, blended);
            }
        }
    }
}

fn draw_sidebar_tile(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    if color == MINT_LIGHT {
        return; // Transparent background in the topbar!
    }
    c.draw_round_rect(
        cx - 13,
        cy - 13,
        26,
        26,
        7,
        Color::rgba(255, 255, 255, 180), // Sleek translucent white glass
    );
    c.draw_round_rect(
        cx - 14,
        cy - 14,
        28,
        28,
        8,
        Color::rgba(220, 235, 245, 60), // Soft glass outer shadow/border
    );
}

fn draw_sidebar_display_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_sidebar_tile(c, cx, cy, color);
    let is_topbar = color == MINT_LIGHT;
    let base_color = if is_topbar {
        Color::rgb(175, 218, 245)
    } else {
        Color::rgb(60, 75, 96)
    };
    let accent_color = if is_topbar {
        Color::rgb(175, 218, 245)
    } else {
        Color::rgb(82, 196, 180)
    };

    let (outer_x, outer_y, outer_w, outer_h, inner_x, inner_y, inner_w, inner_h) = if is_topbar {
        (cx - 11, cy - 10, 22, 20, cx - 8, cy - 7, 16, 14)
    } else {
        (cx - 9, cy - 7, 18, 14, cx - 7, cy - 5, 14, 10)
    };
    c.draw_round_rect(outer_x, outer_y, outer_w, outer_h, 3, base_color);
    c.draw_round_rect(inner_x, inner_y, inner_w, inner_h, 2, accent_color);
}

fn draw_sidebar_wallpaper_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_sidebar_tile(c, cx, cy, color);
    let is_topbar = color == MINT_LIGHT;
    let base_color = if is_topbar {
        Color::rgb(175, 218, 245)
    } else {
        Color::rgb(60, 75, 96)
    };
    let accent_color = if is_topbar {
        Color::rgb(175, 218, 245)
    } else {
        Color::rgb(82, 196, 180)
    };

    // Frame
    c.draw_round_rect(cx - 9, cy - 8, 18, 16, 3, base_color);
    // Moon/Sun
    c.draw_circle(cx + 4, cy - 4, 2, accent_color);
    // Left mountain peak
    draw_round_line(
        c,
        cx - 7,
        cy + 6,
        cx - 3,
        cy + 1,
        2,
        if is_topbar {
            Color::rgb(175, 218, 245)
        } else {
            Color::rgb(110, 125, 145)
        },
    );
    draw_round_line(
        c,
        cx - 3,
        cy + 1,
        cx + 1,
        cy + 6,
        2,
        if is_topbar {
            Color::rgb(175, 218, 245)
        } else {
            Color::rgb(110, 125, 145)
        },
    );
    // Right mountain peak
    draw_round_line(
        c,
        cx - 2,
        cy + 6,
        cx + 3,
        cy - 1,
        2,
        if is_topbar {
            Color::rgb(195, 228, 250)
        } else {
            Color::rgb(130, 145, 165)
        },
    );
    draw_round_line(
        c,
        cx + 3,
        cy - 1,
        cx + 7,
        cy + 6,
        2,
        if is_topbar {
            Color::rgb(195, 228, 250)
        } else {
            Color::rgb(130, 145, 165)
        },
    );
}

fn draw_sidebar_audio_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_sidebar_tile(c, cx, cy, color);
    draw_speaker_icon_small(c, cx, cy, color);
}

fn draw_sidebar_network_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_sidebar_tile(c, cx, cy, color);
    draw_wifi_icon_small(c, cx, cy, color);
}

fn draw_sidebar_bluetooth_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_sidebar_tile(c, cx, cy, color);
    let is_topbar = color == MINT_LIGHT;
    let base_color = if is_topbar {
        Color::rgb(175, 218, 245)
    } else {
        Color::rgb(60, 75, 96)
    };
    let accent_color = if is_topbar {
        Color::rgb(175, 218, 245)
    } else {
        Color::rgb(82, 196, 180)
    };

    // Spine
    draw_round_line(c, cx - 4, cy - 8, cx - 4, cy + 8, 3, accent_color);
    // Top filled triangle
    for dx in 0..=8 {
        let x = cx - 3 + dx;
        let y0 = cy - 8 + dx / 2;
        let y1 = cy - dx / 2;
        draw_round_line(c, x, y0, x, y1, 2, base_color);
    }
    // Bottom filled triangle
    for dx in 0..=8 {
        let x = cx - 3 + dx;
        let y0 = cy + dx / 2;
        let y1 = cy + 8 - dx / 2;
        draw_round_line(c, x, y0, x, y1, 2, base_color);
    }
}

fn draw_sidebar_startup_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_sidebar_tile(c, cx, cy, color);
    let is_topbar = color == MINT_LIGHT;
    let base_color = if is_topbar {
        Color::rgb(175, 218, 245)
    } else {
        Color::rgb(60, 75, 96)
    };
    let accent_color = if is_topbar {
        Color::rgb(175, 218, 245)
    } else {
        Color::rgb(82, 196, 180)
    };

    // Filled right-pointing triangle using vertical slices
    for dx in 0..=12 {
        let x = cx - 6 + dx;
        let half_h = (12 - dx) / 2;
        draw_round_line(c, x, cy - half_h, x, cy + half_h, 2, base_color);
    }
    // Accent dot
    c.draw_circle(cx + 5, cy + 7, 2, accent_color);
}

fn draw_sidebar_apps_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_sidebar_tile(c, cx, cy, color);
    let is_topbar = color == MINT_LIGHT;
    let base_color = if is_topbar {
        Color::rgb(175, 218, 245)
    } else {
        Color::rgb(60, 75, 96)
    };

    // 2x2 grid of rounded squares
    for row in 0..2 {
        for col in 0..2 {
            c.draw_round_rect(cx - 7 + col * 8, cy - 7 + row * 8, 6, 6, 2, base_color);
        }
    }
}

fn draw_sidebar_about_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_sidebar_tile(c, cx, cy, color);
    let is_topbar = color == MINT_LIGHT;
    let base_color = if is_topbar {
        Color::rgb(175, 218, 245)
    } else {
        Color::rgb(60, 75, 96)
    };
    let accent_color = if is_topbar {
        Color::rgb(175, 218, 245)
    } else {
        Color::rgb(82, 196, 180)
    };

    // Vertical capsule
    draw_round_line(c, cx, cy - 1, cx, cy + 7, 4, base_color);
    // Floating teal dot
    c.draw_circle(cx, cy - 7, 3, accent_color);
}

fn draw_dock_icon(c: &mut Canvas, idx: usize, cx: i32, cy: i32) {
    match idx {
        0 => draw_apps_icon(c, cx, cy, BLUE),
        1 => draw_picture_icon(c, cx, cy, MINT_DARK),
        2 => draw_music_icon(c, cx, cy, MINT_DARK),
        3 => draw_play_icon(c, cx, cy, BLUE),
        _ => draw_gear_icon(c, cx, cy, SOFT_INK),
    }
}

fn draw_apps_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    for row in 0..2 {
        for col in 0..2 {
            let x = cx - 11 + col * 14;
            let y = cy - 11 + row * 14;
            c.draw_round_rect(x, y, 9, 9, 3, Color::rgba(color.r, color.g, color.b, 48));
            c.draw_round_rect(x + 2, y + 2, 5, 5, 2, color);
        }
    }
}

fn draw_client_icon(c: &mut Canvas, cx: i32, cy: i32, active: bool) {
    let color = if active { BLUE } else { MUTED };
    c.draw_round_rect(
        cx - 12,
        cy - 9,
        24,
        17,
        4,
        Color::rgba(color.r, color.g, color.b, 50),
    );
    c.draw_line(cx - 12, cy + 10, cx + 12, cy + 10, 2, color);
}

fn draw_text_file_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.draw_round_rect(
        cx - 10,
        cy - 13,
        20,
        26,
        4,
        Color::rgba(color.r, color.g, color.b, 48),
    );
    for y in [-6, 0, 6] {
        c.draw_line(cx - 5, cy + y, cx + 6, cy + y, 2, color);
    }
}

fn draw_client_task_icon(
    c: &mut Canvas,
    font: &Font<'static>,
    cx: i32,
    cy: i32,
    active: bool,
    title: &str,
) {
    let color = if active { BLUE } else { MUTED };
    c.draw_round_rect(
        cx - 13,
        cy - 11,
        26,
        19,
        5,
        Color::rgba(color.r, color.g, color.b, 58),
    );
    c.draw_rect(cx - 10, cy - 7, 20, 11, Color::rgba(255, 255, 255, 138));
    c.draw_line(cx - 12, cy + 10, cx + 12, cy + 10, 2, color);
    let initials = title
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(2)
        .collect::<String>();
    let label = if initials.is_empty() {
        "A"
    } else {
        initials.as_str()
    };
    c.draw_text_center(font, label, cx, cy - 7, 9.0, INK);
}

fn draw_file_kind_icon(c: &mut Canvas, kind: FileKind, cx: i32, cy: i32) {
    match kind {
        FileKind::Directory => draw_folder_icon(c, cx, cy, MINT_DARK),
        FileKind::Text => draw_text_file_icon(c, cx, cy, SOFT_INK),
        FileKind::Image => draw_picture_icon(c, cx, cy, MINT_DARK),
        FileKind::Audio => draw_music_icon(c, cx, cy, MINT_DARK),
        FileKind::Video => draw_play_icon(c, cx, cy, BLUE),
        FileKind::Other => draw_client_icon(c, cx, cy, true),
    }
}

fn file_kind_label(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Directory => "Folder",
        FileKind::Text => "Text file",
        FileKind::Image => "Image file",
        FileKind::Audio => "Audio file",
        FileKind::Video => "Video file",
        FileKind::Other => "Open file",
    }
}

fn draw_launcher_icon(c: &mut Canvas, idx: usize, cx: i32, cy: i32) {
    match idx {
        0 => draw_play_icon(c, cx, cy, BLUE),
        1 => draw_globe_icon(c, cx, cy, BLUE),
        2 => draw_picture_icon(c, cx, cy, MINT_DARK),
        3 => draw_music_icon(c, cx, cy, MINT_DARK),
        4 => draw_play_icon(c, cx, cy, MINT_DARK),
        _ => draw_gear_icon(c, cx, cy, SOFT_INK),
    }
}

fn draw_folder_icon(c: &mut Canvas, cx: i32, cy: i32, _color: Color) {
    c.draw_round_rect(
        cx - 12,
        cy - 12,
        24,
        24,
        6,
        Color::rgb(175, 218, 245), // Simple beautiful light blue square
    );
}

fn draw_home_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_round_line(c, cx - 10, cy, cx, cy - 9, 2, color);
    draw_round_line(c, cx, cy - 9, cx + 10, cy, 2, color);
    c.draw_round_rect(
        cx - 7,
        cy,
        14,
        11,
        4,
        Color::rgba(color.r, color.g, color.b, 45),
    );
    c.draw_round_rect(cx - 2, cy + 5, 4, 6, 2, color);
}

fn draw_more_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.draw_circle(cx - 7, cy, 2, color);
    c.draw_circle(cx, cy, 2, color);
    c.draw_circle(cx + 7, cy, 2, color);
}

fn draw_sort_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.draw_line(cx - 8, cy - 6, cx + 7, cy - 6, 2, color);
    c.draw_line(cx - 8, cy, cx + 3, cy, 2, color);
    c.draw_line(cx - 8, cy + 6, cx - 1, cy + 6, 2, color);
}

fn draw_terminal_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_round_line(c, cx - 8, cy - 5, cx - 3, cy, 2, color);
    draw_round_line(c, cx - 8, cy + 5, cx - 3, cy, 2, color);
    c.draw_line(cx + 1, cy + 5, cx + 8, cy + 5, 2, color);
}

fn draw_screenshot_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    let fill = if color == MINT_LIGHT {
        Color::rgb(175, 218, 245)
    } else {
        color
    };
    let (x, y, w, h, lens) = if color == MINT_LIGHT {
        (cx - 11, cy - 10, 22, 21, 6)
    } else {
        (cx - 10, cy - 8, 20, 16, 4)
    };
    c.draw_round_rect(x, y, w, h, 5, fill);
    c.draw_circle(cx, cy, lens, Color::rgba(255, 255, 255, 210));
    c.draw_circle(cx, cy, (lens - 2).max(2), fill);
}

fn draw_clipboard_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    let fill = if color == MINT_LIGHT {
        Color::rgb(175, 218, 245)
    } else {
        color
    };
    let paper = if color == MINT_LIGHT {
        Color::rgba(23, 34, 42, 118)
    } else {
        Color::rgba(255, 255, 255, 150)
    };
    if color == MINT_LIGHT {
        c.draw_round_rect(cx - 10, cy - 7, 20, 18, 5, fill);
        c.draw_round_rect(cx - 6, cy - 3, 12, 11, 2, paper);
        c.draw_round_rect(cx - 6, cy - 11, 12, 7, 4, fill);
        c.draw_round_rect(cx - 3, cy - 9, 6, 2, 1, paper);
    } else {
        c.draw_round_rect(cx - 8, cy - 6, 16, 16, 4, fill);
        c.draw_round_rect(cx - 5, cy - 3, 10, 10, 2, paper);
        c.draw_round_rect(cx - 5, cy - 9, 10, 6, 3, fill);
        c.draw_round_rect(cx - 2, cy - 7, 4, 2, 1, paper);
    }
}

fn draw_copy_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.draw_round_rect(cx - 5, cy - 7, 10, 12, 2, Color::rgba(255, 255, 255, 150));
    c.draw_round_rect(
        cx - 2,
        cy - 4,
        10,
        12,
        2,
        Color::rgba(color.r, color.g, color.b, 155),
    );
    c.draw_rect(cx + 1, cy - 1, 4, 1, Color::rgba(255, 255, 255, 210));
    c.draw_rect(cx + 1, cy + 3, 4, 1, Color::rgba(255, 255, 255, 210));
}

fn draw_edit_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    let square = Color::rgba(color.r, color.g, color.b, 86);
    let pen = Color::rgb(32, 58, 68);
    c.draw_line(cx - 8, cy - 7, cx - 8, cy + 7, 1, square);
    c.draw_line(cx - 8, cy + 7, cx + 5, cy + 7, 1, square);
    c.draw_line(cx - 8, cy - 7, cx + 4, cy - 7, 1, square);
    c.draw_line(cx + 8, cy - 1, cx + 8, cy + 7, 1, square);
    draw_round_line(c, cx - 4, cy + 4, cx + 6, cy - 6, 3, pen);
    draw_round_line(c, cx - 6, cy + 6, cx - 4, cy + 4, 2, pen);
    draw_round_line(c, cx + 6, cy - 6, cx + 8, cy - 8, 2, pen);
}

fn draw_save_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.draw_round_rect(cx - 8, cy - 8, 16, 16, 3, color);
    c.draw_rect(cx + 3, cy - 8, 5, 5, Color::rgba(255, 255, 255, 180));
    c.draw_round_rect(cx - 5, cy + 1, 10, 5, 2, Color::rgba(255, 255, 255, 210));
}

fn draw_picture_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.draw_round_rect(
        cx - 13,
        cy - 11,
        26,
        22,
        7,
        Color::rgba(color.r, color.g, color.b, 44),
    );
    c.draw_circle(
        cx - 6,
        cy - 4,
        3,
        Color::rgba(color.r, color.g, color.b, 176),
    );
    draw_round_line(c, cx - 10, cy + 7, cx - 3, cy, 2, color);
    draw_round_line(c, cx - 3, cy, cx + 4, cy + 6, 2, color);
    draw_round_line(c, cx + 4, cy + 6, cx + 10, cy - 2, 2, color);
}

fn draw_globe_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.draw_circle(cx, cy, 12, Color::rgba(color.r, color.g, color.b, 45));
    c.draw_circle(cx, cy, 10, Color::rgba(255, 255, 255, 80));
    c.draw_line(cx - 10, cy, cx + 10, cy, 2, color);
    c.draw_line(cx, cy - 10, cx, cy + 10, 2, color);
    c.draw_line(cx - 7, cy - 7, cx + 7, cy - 7, 1, color);
    c.draw_line(cx - 7, cy + 7, cx + 7, cy + 7, 1, color);
}

fn draw_music_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.draw_line(cx - 2, cy - 12, cx - 2, cy + 7, 3, color);
    c.draw_line(cx - 2, cy - 12, cx + 10, cy - 15, 3, color);
    c.draw_line(cx + 10, cy - 15, cx + 10, cy + 4, 3, color);
    c.draw_circle(
        cx - 7,
        cy + 8,
        5,
        Color::rgba(color.r, color.g, color.b, 170),
    );
    c.draw_circle(
        cx + 5,
        cy + 5,
        5,
        Color::rgba(color.r, color.g, color.b, 170),
    );
}

fn draw_play_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.draw_round_rect(
        cx - 13,
        cy - 10,
        26,
        20,
        5,
        Color::rgba(color.r, color.g, color.b, 45),
    );
    c.draw_line(cx - 4, cy - 7, cx + 8, cy, 3, color);
    c.draw_line(cx + 8, cy, cx - 4, cy + 7, 3, color);
    c.draw_line(cx - 4, cy + 7, cx - 4, cy - 7, 3, color);
}

fn draw_gear_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.draw_circle(cx, cy, 12, Color::rgba(color.r, color.g, color.b, 45));
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::TAU / 8.0;
        let x1 = cx + (a.cos() * 8.0) as i32;
        let y1 = cy + (a.sin() * 8.0) as i32;
        let x2 = cx + (a.cos() * 13.0) as i32;
        let y2 = cy + (a.sin() * 13.0) as i32;
        c.draw_line(x1, y1, x2, y2, 2, color);
    }
    c.draw_circle(cx, cy, 4, Color::rgba(255, 255, 255, 200));
}

fn draw_power_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_sidebar_tile(c, cx, cy, color);
    let is_topbar = color == MINT_LIGHT;
    let base_color = if is_topbar {
        Color::rgb(175, 218, 245)
    } else {
        Color::rgb(60, 75, 96)
    };
    let accent_color = if is_topbar {
        Color::rgb(175, 218, 245)
    } else {
        Color::rgb(82, 196, 180)
    };

    if is_topbar {
        c.draw_round_rect(cx - 11, cy - 9, 20, 18, 4, base_color);
        c.draw_round_rect(cx + 9, cy - 5, 3, 10, 1, base_color);
        c.draw_round_rect(cx - 8, cy - 6, 6, 12, 1, accent_color);
    } else {
        c.draw_round_rect(cx - 9, cy - 6, 16, 12, 3, base_color);
        c.draw_round_rect(cx + 7, cy - 3, 2, 6, 1, base_color);
        c.draw_round_rect(cx - 7, cy - 4, 4, 8, 1, accent_color);
    }
}

fn draw_wifi_icon_small(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    let is_topbar = color == MINT_LIGHT;
    let base_color = if is_topbar {
        Color::rgb(175, 218, 245)
    } else {
        Color::rgb(60, 75, 96)
    };
    let accent_color = if is_topbar {
        Color::rgb(175, 218, 245)
    } else {
        Color::rgb(82, 196, 180)
    };

    // Two concentric arcs centered at bottom dot (radii: 12 and 7, thickness: 3)
    draw_arc(c, cx, cy + 6, 12, 220.0, 320.0, 10, 3, base_color);
    draw_arc(c, cx, cy + 6, 7, 220.0, 320.0, 8, 3, base_color);

    // Bottom center dot
    c.draw_circle(cx, cy + 6, 3, accent_color);
}

fn draw_speaker_icon_small(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    let is_topbar = color == MINT_LIGHT;
    let base_color = if is_topbar {
        Color::rgb(175, 218, 245)
    } else {
        Color::rgb(60, 75, 96)
    };

    let (base_x, base_y, base_w, base_h, x_start, x_end, y_start, y_end) = if is_topbar {
        (cx - 12, cy - 5, 7, 10, cx - 5, cx + 8, cy - 10, cy + 10)
    } else {
        (cx - 10, cy - 3, 5, 6, cx - 5, cx + 5, cy - 7, cy + 7)
    };
    c.draw_round_rect(base_x, base_y, base_w, base_h, 2, base_color);

    // Flared cone in float distance space
    let cone_w = (x_end - x_start).max(1) as f32;
    let top_left = cy - base_h / 2;
    let bottom_left = cy + base_h / 2;

    for y in y_start..=y_end {
        for x in x_start..=x_end {
            let x_f = x as f32;
            let y_f = y as f32;

            let top_y =
                top_left as f32 - (x_f - x_start as f32) * ((top_left - y_start) as f32 / cone_w);
            let bottom_y = bottom_left as f32
                + (x_f - x_start as f32) * ((y_end - bottom_left) as f32 / cone_w);

            let coverage_top = (y_f - top_y + 0.5).clamp(0.0, 1.0);
            let coverage_bottom = (bottom_y - y_f + 0.5).clamp(0.0, 1.0);
            let coverage_left = (x_f - (cx - 5) as f32 + 0.5).clamp(0.0, 1.0);
            let coverage_right = ((cx + 5) as f32 - x_f + 0.5).clamp(0.0, 1.0);

            let coverage = coverage_top * coverage_bottom * coverage_left * coverage_right;

            if coverage > 0.0 {
                let mut blended = base_color;
                blended.a = (base_color.a as f32 * coverage).round() as u8;
                c.blend_pixel(x, y, blended);
            }
        }
    }
}

fn render_wallpaper_pixels(
    bytes: &[u8],
    screen_width: u16,
    screen_height: u16,
) -> AnyResult<Vec<u8>> {
    render_cover_pixels_nearest(bytes, screen_width, screen_height)
}

fn render_asset_preview_pixels(bytes: &[u8], w: u16, h: u16) -> AnyResult<Vec<u8>> {
    render_cover_pixels_nearest(bytes, w, h)
}

fn render_cover_pixels_nearest(bytes: &[u8], w: u16, h: u16) -> AnyResult<Vec<u8>> {
    let img = image::load_from_memory(bytes)?.to_rgba8();
    let (iw, ih) = img.dimensions();
    if iw == 0 || ih == 0 || w == 0 || h == 0 {
        return Ok(Vec::new());
    }

    let scale = (f32::from(w) / iw as f32).max(f32::from(h) / ih as f32);
    let view_w = f32::from(w) / scale;
    let view_h = f32::from(h) / scale;
    let src_x0 = ((iw as f32 - view_w) * 0.5).max(0.0);
    let src_y0 = ((ih as f32 - view_h) * 0.5).max(0.0);
    let raw = img.as_raw();
    let x_map = (0..u32::from(w))
        .map(|x| {
            (src_x0 + (x as f32 + 0.5) * view_w / f32::from(w))
                .floor()
                .clamp(0.0, iw.saturating_sub(1) as f32) as usize
        })
        .collect::<Vec<_>>();
    let y_map = (0..u32::from(h))
        .map(|y| {
            (src_y0 + (y as f32 + 0.5) * view_h / f32::from(h))
                .floor()
                .clamp(0.0, ih.saturating_sub(1) as f32) as usize
        })
        .collect::<Vec<_>>();
    let mut out = vec![0; usize::from(w) * usize::from(h) * 4];
    let src_stride = iw as usize * 4;
    for (dy, sy) in y_map.iter().copied().enumerate() {
        let src_row = sy * src_stride;
        let dst_row = dy * usize::from(w) * 4;
        for (dx, sx) in x_map.iter().copied().enumerate() {
            let src = src_row + sx * 4;
            let dst = dst_row + dx * 4;
            out[dst] = raw[src + 2];
            out[dst + 1] = raw[src + 1];
            out[dst + 2] = raw[src];
            out[dst + 3] = 0;
        }
    }
    Ok(out)
}

fn paint_bgr_pixels(c: &mut Canvas, pixels: &[u8], x: i32, y: i32, w: i32, h: i32) {
    if w <= 0 || h <= 0 {
        return;
    }
    for yy in 0..h {
        for xx in 0..w {
            let idx = ((yy * w + xx) * 4) as usize;
            if idx + 2 < pixels.len() {
                c.blend_pixel(
                    x + xx,
                    y + yy,
                    Color::rgba(pixels[idx + 2], pixels[idx + 1], pixels[idx], 255),
                );
            }
        }
    }
}

fn paint_rgba_pixels(c: &mut Canvas, pixels: &[u8], x: i32, y: i32, w: i32, h: i32) {
    if w <= 0 || h <= 0 {
        return;
    }
    for yy in 0..h {
        for xx in 0..w {
            let idx = ((yy * w + xx) * 4) as usize;
            if idx + 3 < pixels.len() {
                c.blend_pixel(
                    x + xx,
                    y + yy,
                    Color::rgba(
                        pixels[idx],
                        pixels[idx + 1],
                        pixels[idx + 2],
                        pixels[idx + 3],
                    ),
                );
            }
        }
    }
}

fn decode_icon_pixels(bytes: &[u8], size: i32) -> AnyResult<Vec<u8>> {
    let img = image::load_from_memory(bytes)?.to_rgba8();
    let resized = image::imageops::resize(
        &img,
        size.max(1) as u32,
        size.max(1) as u32,
        FilterType::Triangle,
    );
    Ok(resized.into_raw())
}

fn resolve_window_icon(class: &str, title: &str) -> Option<PathBuf> {
    let terms = window_match_terms(class, title);
    let icon_name = find_desktop_icon_name(&terms)?;
    resolve_icon_path(&icon_name)
}

fn window_match_terms(class: &str, title: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for raw in class.split('\0').chain(title.split([' ', '-', '_', '.'])) {
        let term = raw.trim().trim_matches(char::from(0)).to_ascii_lowercase();
        if term.len() >= 2 && !terms.contains(&term) {
            terms.push(term);
        }
    }
    terms
}

fn find_desktop_icon_name(terms: &[String]) -> Option<String> {
    for dir in desktop_search_dirs() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("desktop") {
                continue;
            }
            if let Some(icon) = desktop_icon_if_matches(&path, terms) {
                return Some(icon);
            }
        }
    }
    None
}

fn desktop_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(home) = env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }
    if let Ok(data_home) = env::var("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(data_home).join("applications"));
    }
    if let Ok(data_dirs) = env::var("XDG_DATA_DIRS") {
        dirs.extend(
            data_dirs
                .split(':')
                .filter(|dir| !dir.is_empty())
                .map(|dir| PathBuf::from(dir).join("applications")),
        );
    }
    dirs.push(PathBuf::from("/usr/share/applications"));
    dirs
}

fn desktop_icon_if_matches(path: &Path, terms: &[String]) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let mut name = String::new();
    let mut startup_class = String::new();
    let mut icon = String::new();
    let mut no_display = false;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "Name" => name = value.to_ascii_lowercase(),
            "StartupWMClass" => startup_class = value.to_ascii_lowercase(),
            "Icon" => icon = value.trim().to_string(),
            "NoDisplay" => no_display = value.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }
    if icon.is_empty() || no_display {
        return None;
    }
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let matched = terms.iter().any(|term| {
        startup_class == *term
            || stem == *term
            || stem.ends_with(&format!(".{term}"))
            || name == *term
            || (!name.is_empty() && name.contains(term))
    });
    matched.then_some(icon)
}

fn resolve_icon_path(icon_name: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(icon_name);
    if direct.is_absolute() && direct.exists() {
        return Some(direct);
    }
    let candidates = icon_candidate_paths(icon_name);
    candidates.into_iter().find(|path| path.exists())
}

fn icon_candidate_paths(icon_name: &str) -> Vec<PathBuf> {
    let mut bases = Vec::new();
    if let Ok(home) = env::var("HOME") {
        bases.push(PathBuf::from(home).join(".local/share/icons"));
    }
    bases.push(PathBuf::from("/usr/share/icons/hicolor"));
    bases.push(PathBuf::from("/usr/share/icons"));
    bases.push(PathBuf::from("/usr/share/pixmaps"));

    let sizes = [
        "64x64", "48x48", "32x32", "128x128", "256x256", "scalable", "symbolic",
    ];
    let contexts = ["apps", "categories", "places", "mimetypes"];
    let exts = ["png", "webp", "jpg", "jpeg", "gif"];
    let mut paths = Vec::new();
    for base in bases {
        for size in sizes {
            for context in contexts {
                for ext in exts {
                    paths.push(
                        base.join(size)
                            .join(context)
                            .join(format!("{icon_name}.{ext}")),
                    );
                }
            }
        }
        for ext in exts {
            paths.push(base.join(format!("{icon_name}.{ext}")));
        }
    }
    paths
}

fn paint_file_preview(c: &mut Canvas, path: &std::path::Path, x: i32, y: i32, w: i32, h: i32) {
    if w <= 0 || h <= 0 {
        return;
    }
    let Ok(bytes) = fs::read(path) else {
        return;
    };
    let Ok(img) = image::load_from_memory(&bytes).map(|img| img.to_rgba8()) else {
        return;
    };
    let (iw, ih) = img.dimensions();
    let scale = (w as f32 / iw as f32).min(h as f32 / ih as f32);
    let nw = (iw as f32 * scale).round().max(1.0) as u32;
    let nh = (ih as f32 * scale).round().max(1.0) as u32;
    let resized = image::imageops::resize(&img, nw, nh, FilterType::Triangle);
    let dx = x + (w - nw as i32) / 2;
    let dy = y + (h - nh as i32) / 2;
    for yy in 0..nh as i32 {
        for xx in 0..nw as i32 {
            let p = resized.get_pixel(xx as u32, yy as u32);
            c.blend_pixel(dx + xx, dy + yy, Color::rgba(p[0], p[1], p[2], 255));
        }
    }
}

fn render_image_preview(path: &std::path::Path, w: i32, h: i32) -> Option<ImagePreview> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let resolution = image_dimensions(path);
    let bytes = fs::read(path).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (iw, ih) = img.dimensions();
    let scale = (w as f32 / iw as f32).min(h as f32 / ih as f32);
    let nw = (iw as f32 * scale).round().max(1.0) as u32;
    let nh = (ih as f32 * scale).round().max(1.0) as u32;
    let resized = image::imageops::resize(&img, nw, nh, FilterType::Triangle);
    Some(ImagePreview {
        pixels: resized.into_raw(),
        width: nw.min(u16::MAX as u32) as u16,
        height: nh.min(u16::MAX as u32) as u16,
        resolution,
    })
}

fn capture_screen_preview(conn: &RustConnection, root: Window) -> Option<ImagePreview> {
    let (pixels, iw, ih) = capture_root_rgba(conn, root, 0, 0, u16::MAX, u16::MAX).ok()?;
    Some(ImagePreview {
        pixels,
        width: iw.min(u16::MAX as u32) as u16,
        height: ih.min(u16::MAX as u32) as u16,
        resolution: Some((iw, ih)),
    })
}

fn capture_root_rgba(
    conn: &RustConnection,
    root: Window,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
) -> AnyResult<(Vec<u8>, u32, u32)> {
    let geom = conn.get_geometry(root)?.reply()?;
    let capture_x = x.max(0);
    let capture_y = y.max(0);
    let max_w = i32::from(geom.width).saturating_sub(i32::from(capture_x));
    let max_h = i32::from(geom.height).saturating_sub(i32::from(capture_y));
    let capture_w = u16::try_from(i32::from(width).min(max_w).max(1))?;
    let capture_h = u16::try_from(i32::from(height).min(max_h).max(1))?;
    let reply = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            root,
            capture_x,
            capture_y,
            capture_w,
            capture_h,
            u32::MAX,
        )?
        .reply()?;
    let setup = conn.setup();
    let format = setup
        .pixmap_formats
        .iter()
        .find(|format| format.depth == reply.depth)
        .ok_or("missing X11 pixmap format for screenshot depth")?;
    let visual = find_visual(setup, reply.visual).ok_or("missing X11 visual for screenshot")?;
    let bits_per_pixel = usize::from(format.bits_per_pixel);
    let bytes_per_pixel = bits_per_pixel.div_ceil(8);
    if bytes_per_pixel == 0 || bits_per_pixel > 32 {
        return Err("unsupported X11 screenshot pixel format".into());
    }
    let stride_bits = usize::from(capture_w)
        .checked_mul(bits_per_pixel)
        .ok_or("screenshot row is too wide")?;
    let pad = usize::from(format.scanline_pad).max(8);
    let stride = stride_bits.div_ceil(pad) * (pad / 8);
    let mut rgba = vec![0; usize::from(capture_w) * usize::from(capture_h) * 4];
    for row in 0..usize::from(capture_h) {
        let row_start = row * stride;
        for col in 0..usize::from(capture_w) {
            let src = row_start + col * bytes_per_pixel;
            if src + bytes_per_pixel > reply.data.len() {
                return Err("short X11 screenshot data".into());
            }
            let pixel = read_x11_pixel(
                &reply.data[src..src + bytes_per_pixel],
                setup.image_byte_order,
            );
            let dst = (row * usize::from(capture_w) + col) * 4;
            rgba[dst] = scale_masked_channel(pixel, visual.red_mask);
            rgba[dst + 1] = scale_masked_channel(pixel, visual.green_mask);
            rgba[dst + 2] = scale_masked_channel(pixel, visual.blue_mask);
            rgba[dst + 3] = 255;
        }
    }
    Ok((rgba, u32::from(capture_w), u32::from(capture_h)))
}

fn find_visual(setup: &Setup, visual_id: Visualid) -> Option<Visualtype> {
    setup.roots.iter().find_map(|screen| {
        screen.allowed_depths.iter().find_map(|depth| {
            depth
                .visuals
                .iter()
                .find(|visual| visual.visual_id == visual_id)
                .copied()
        })
    })
}

fn read_x11_pixel(bytes: &[u8], order: ImageOrder) -> u32 {
    let mut pixel = 0u32;
    match order {
        ImageOrder::LSB_FIRST => {
            for (shift, byte) in bytes.iter().enumerate() {
                pixel |= u32::from(*byte) << (shift * 8);
            }
        }
        ImageOrder::MSB_FIRST => {
            for byte in bytes {
                pixel = (pixel << 8) | u32::from(*byte);
            }
        }
        _ => {}
    }
    pixel
}

fn scale_masked_channel(pixel: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let max = mask >> shift;
    let value = (pixel & mask) >> shift;
    ((value * 255 + max / 2) / max) as u8
}

fn canvas_from_preview(preview: &ImagePreview, width: u16, height: u16) -> Canvas {
    let mut c = Canvas::new(width, height, Color::rgba(0, 0, 0, 255));
    let pw = usize::from(preview.width);
    let ph = usize::from(preview.height);
    let cw = usize::from(width);
    let ch = usize::from(height);
    for yy in 0..ph.min(ch) {
        for xx in 0..pw.min(cw) {
            let src = (yy * pw + xx) * 4;
            let dst = (yy * cw + xx) * 4;
            if src + 3 < preview.pixels.len() && dst + 3 < c.data.len() {
                c.data[dst] = preview.pixels[src + 2];
                c.data[dst + 1] = preview.pixels[src + 1];
                c.data[dst + 2] = preview.pixels[src];
                c.data[dst + 3] = preview.pixels[src + 3];
            }
        }
    }
    c
}

fn paint_preview_region(c: &mut Canvas, preview: &ImagePreview, x: i32, y: i32, w: i32, h: i32) {
    let pw = i32::from(preview.width);
    let ph = i32::from(preview.height);
    for yy in 0..h {
        for xx in 0..w {
            let sx = x + xx;
            let sy = y + yy;
            if sx < 0 || sy < 0 || sx >= pw || sy >= ph {
                continue;
            }
            let idx = ((sy * pw + sx) * 4) as usize;
            if idx + 3 < preview.pixels.len() {
                c.blend_pixel(
                    sx,
                    sy,
                    Color::rgba(
                        preview.pixels[idx],
                        preview.pixels[idx + 1],
                        preview.pixels[idx + 2],
                        preview.pixels[idx + 3],
                    ),
                );
            }
        }
    }
}

fn paint_cached_image_preview(
    c: &mut Canvas,
    preview: &ImagePreview,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    paint_cached_image_preview_aligned(c, preview, x, y, w, h, true);
}

fn paint_cached_image_preview_left(
    c: &mut Canvas,
    preview: &ImagePreview,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    paint_cached_image_preview_aligned(c, preview, x, y, w, h, false);
}

fn paint_cached_image_preview_aligned(
    c: &mut Canvas,
    preview: &ImagePreview,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    center_x: bool,
) {
    let pw = i32::from(preview.width);
    let ph = i32::from(preview.height);
    let dx = if center_x { x + (w - pw) / 2 } else { x };
    let dy = y + (h - ph) / 2;
    for yy in 0..ph {
        for xx in 0..pw {
            let px = dx + xx;
            let py = dy + yy;
            if px < x || py < y || px >= x + w || py >= y + h {
                continue;
            }
            let idx = ((yy * pw + xx) * 4) as usize;
            if idx + 3 < preview.pixels.len() {
                c.blend_pixel(
                    px,
                    py,
                    Color::rgba(
                        preview.pixels[idx],
                        preview.pixels[idx + 1],
                        preview.pixels[idx + 2],
                        preview.pixels[idx + 3],
                    ),
                );
            }
        }
    }
}

fn image_dimensions(path: &std::path::Path) -> Option<(u32, u32)> {
    image::image_dimensions(path).ok()
}

fn paint_video_frame_preview(
    c: &mut Canvas,
    path: &std::path::Path,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Option<()> {
    c.draw_round_rect(x, y, w, h, 10, Color::rgba(23, 34, 42, 54));
    c.draw_text_center(
        &Font::try_from_bytes(FONT_REGULAR)?,
        &compact(
            path.file_name().and_then(|n| n.to_str()).unwrap_or("Video"),
            28,
        ),
        x + w / 2,
        y + h / 2 - 12,
        13.0,
        Color::rgb(255, 255, 255),
    );
    None
}

fn read_text_lines_limited(path: &std::path::Path, max_lines: usize) -> Vec<String> {
    let Ok(mut file) = fs::File::open(path) else {
        return vec!["Could not open text file".to_string()];
    };
    let mut buf = String::new();
    let _ = file.by_ref().take(512 * 1024).read_to_string(&mut buf);
    let mut lines = buf
        .lines()
        .take(max_lines)
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn file_command_summary(path: &std::path::Path) -> String {
    Command::new("file")
        .arg("-b")
        .arg(path)
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "Unknown file type".to_string())
}

fn read_display_modes(display: &str, current_width: u16, current_height: u16) -> Vec<DisplayMode> {
    let mut modes = Vec::new();
    if let Ok(output) = Command::new("xrandr")
        .env("DISPLAY", display)
        .arg("--query")
        .output()
    {
        let text = String::from_utf8_lossy(&output.stdout);
        let mut current_output: Option<String> = None;
        for line in text.lines() {
            if !line.chars().next().is_some_and(char::is_whitespace) {
                let mut parts = line.split_whitespace();
                let Some(name) = parts.next() else {
                    continue;
                };
                current_output = parts
                    .next()
                    .is_some_and(|state| state == "connected")
                    .then(|| name.to_string());
                continue;
            }
            let trimmed = line.trim_start();
            let Some(first) = trimmed.split_whitespace().next() else {
                continue;
            };
            let Some((w, h)) = first.split_once('x') else {
                continue;
            };
            let (Ok(width), Ok(height)) = (w.parse::<u16>(), h.parse::<u16>()) else {
                continue;
            };
            let tokens = trimmed.split_whitespace().collect::<Vec<_>>();
            let current = tokens.iter().any(|token| token.contains('*'));
            let refresh = tokens
                .iter()
                .skip(1)
                .find_map(|token| token.trim_end_matches(['*', '+']).parse::<f32>().ok())
                .filter(|rate| *rate >= 1.0);
            let output_name = current_output.clone();
            if !modes.iter().any(|m: &DisplayMode| {
                m.output == output_name && m.width == width && m.height == height
            }) {
                modes.push(DisplayMode {
                    output: output_name,
                    width,
                    height,
                    refresh,
                    current,
                });
            }
        }
    }
    if !modes.iter().any(|mode| mode.current) {
        for mode in &mut modes {
            mode.current = mode.width == current_width && mode.height == current_height;
        }
    }
    modes.sort_by(|a, b| {
        b.current.cmp(&a.current).then_with(|| {
            (u32::from(b.width) * u32::from(b.height))
                .cmp(&(u32::from(a.width) * u32::from(a.height)))
        })
    });
    if modes.is_empty() {
        modes.push(DisplayMode {
            output: None,
            width: current_width,
            height: current_height,
            refresh: Some(60.0),
            current: true,
        });
        modes.push(DisplayMode {
            output: None,
            width: 1366,
            height: 768,
            refresh: Some(60.0),
            current: false,
        });
        modes.push(DisplayMode {
            output: None,
            width: 1600,
            height: 900,
            refresh: Some(60.0),
            current: false,
        });
        modes.push(DisplayMode {
            output: None,
            width: 1920,
            height: 1080,
            refresh: Some(60.0),
            current: false,
        });
    }
    modes
}

fn apply_xrandr_mode(display: &str, mode: &DisplayMode) -> Result<(), String> {
    let size = format!("{}x{}", mode.width, mode.height);
    if let Some(output) = mode.output.as_deref() {
        let mut cmd = Command::new("xrandr");
        cmd.env("DISPLAY", display)
            .args(["--output", output, "--mode", &size]);
        if let Some(rate) = mode.refresh {
            cmd.args(["--rate", &format!("{rate:.2}")]);
        }
        if command_status_success(&mut cmd) {
            return Ok(());
        }

        let mut without_rate = Command::new("xrandr");
        without_rate
            .env("DISPLAY", display)
            .args(["--output", output, "--mode", &size]);
        if command_status_success(&mut without_rate) {
            return Ok(());
        }
    }

    let mut by_size = Command::new("xrandr");
    by_size.env("DISPLAY", display).args(["-s", &size]);
    if command_status_success(&mut by_size) {
        return Ok(());
    }

    Err(format!("Could not switch to {size} with xrandr"))
}

fn apply_xrandr_brightness(
    display: &str,
    output: Option<&str>,
    brightness_percent: u8,
) -> Result<(), String> {
    let brightness = f32::from(brightness_percent.clamp(10, 100)) / 100.0;
    if let Some(output) = output {
        let mut cmd = Command::new("xrandr");
        cmd.env("DISPLAY", display).args([
            "--output",
            output,
            "--brightness",
            &format!("{brightness:.2}"),
        ]);
        if command_status_success(&mut cmd) {
            return Ok(());
        }
        return Err(format!("Could not set brightness for {output}"));
    }

    Err("Could not find an active display output for brightness".to_string())
}

fn read_cpu_model() -> String {
    let Ok(text) = fs::read_to_string("/proc/cpuinfo") else {
        return "Unknown CPU".to_string();
    };
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("model name") {
            if let Some((_, model)) = value.split_once(':') {
                return model.trim().to_string();
            }
        }
    }
    "Unknown CPU".to_string()
}

fn read_cpu_times() -> Option<CpuTimes> {
    let text = fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().next()?;
    let nums: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|p| p.parse().ok())
        .collect();
    if nums.len() < 5 {
        return None;
    }
    let idle = nums[3] + nums.get(4).copied().unwrap_or(0);
    let total = nums.iter().sum();
    Some(CpuTimes { idle, total })
}

fn read_cpu_status(cpu_usage: f32) -> String {
    let temp = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp")
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .map(|v| format!("{}%, {:.0} C", cpu_usage.round(), v / 1000.0));
    temp.unwrap_or_else(|| format!("{}% load", cpu_usage.round()))
}

fn read_cpu_frequencies() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/devices/system/cpu") {
        let mut cpus = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                let idx = name.strip_prefix("cpu")?.parse::<usize>().ok()?;
                Some((idx, entry.path()))
            })
            .collect::<Vec<_>>();
        cpus.sort_by_key(|(idx, _)| *idx);
        for (idx, path) in cpus {
            let freq_path = path.join("cpufreq/scaling_cur_freq");
            let fallback_path = path.join("cpufreq/cpuinfo_cur_freq");
            let freq = fs::read_to_string(&freq_path)
                .or_else(|_| fs::read_to_string(&fallback_path))
                .ok()
                .and_then(|text| text.trim().parse::<u64>().ok());
            if let Some(khz) = freq {
                out.push(format!("c{idx}: {:.2}GHz", khz as f64 / 1_000_000.0));
            }
        }
    }
    if !out.is_empty() {
        return out;
    }
    let Ok(text) = fs::read_to_string("/proc/cpuinfo") else {
        return out;
    };
    for (idx, mhz) in text
        .lines()
        .filter_map(|line| line.strip_prefix("cpu MHz").and_then(|v| v.split_once(':')))
        .filter_map(|(_, v)| v.trim().parse::<f64>().ok())
        .enumerate()
    {
        out.push(format!("c{idx}: {:.2}GHz", mhz / 1000.0));
    }
    out
}

fn cpu_frequency_lines(freqs: &[String], max_chars: usize) -> Vec<String> {
    if freqs.is_empty() {
        return vec!["No CPU frequency data".to_string()];
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    for freq in freqs {
        let sep = if line.is_empty() { "" } else { "  " };
        if !line.is_empty() && line.len() + sep.len() + freq.len() > max_chars {
            lines.push(line);
            line = String::new();
        }
        if !line.is_empty() {
            line.push_str(sep);
        }
        line.push_str(freq);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn read_memory() -> (u64, u64, u64, u64) {
    let Ok(text) = fs::read_to_string("/proc/meminfo") else {
        return (0, 0, 0, 0);
    };
    let mut total = 0;
    let mut available = 0;
    let mut swap_total = 0;
    let mut swap_free = 0;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let key = parts.next().unwrap_or("");
        let value = parts
            .next()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        match key {
            "MemTotal:" => total = value,
            "MemAvailable:" => available = value,
            "SwapTotal:" => swap_total = value,
            "SwapFree:" => swap_free = value,
            _ => {}
        }
    }
    (
        total,
        total.saturating_sub(available),
        swap_total,
        swap_total.saturating_sub(swap_free),
    )
}

fn read_gpus() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("card") || name.contains('-') {
                continue;
            }
            let vendor = fs::read_to_string(entry.path().join("device/vendor"))
                .unwrap_or_default()
                .trim()
                .to_string();
            let device = fs::read_to_string(entry.path().join("device/device"))
                .unwrap_or_default()
                .trim()
                .to_string();
            if vendor.is_empty() && device.is_empty() {
                out.push(name);
            } else {
                out.push(format!("{name} {vendor}:{device}"));
            }
        }
    }
    out
}

fn read_nics() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name != "lo" {
                out.push(name);
            }
        }
    }
    out.sort();
    out
}

fn read_audio_devices(kind: AudioDeviceKind) -> Vec<AudioDevice> {
    let default_name = read_pactl_default_audio_device(kind);
    let mut devices = read_pactl_audio_devices(kind, default_name.as_deref());
    if devices.is_empty() {
        devices = read_wpctl_audio_devices(kind);
    }
    if kind == AudioDeviceKind::Input {
        let filtered = devices
            .iter()
            .filter(|device| !device.name.ends_with(".monitor"))
            .cloned()
            .collect::<Vec<_>>();
        if !filtered.is_empty() {
            devices = filtered;
        }
    }
    devices.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.label.cmp(&b.label)));
    devices
}

fn read_pactl_default_audio_device(kind: AudioDeviceKind) -> Option<String> {
    let output = pulse_command_output("pactl", &["info"])?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix(kind.pactl_default_key())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn read_pactl_audio_devices(kind: AudioDeviceKind, default_name: Option<&str>) -> Vec<AudioDevice> {
    let Some(output) = pulse_command_output("pactl", &["list", kind.pactl_list_arg()]) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut devices = Vec::new();
    let mut id = String::new();
    let mut name = String::new();
    let mut label = String::new();
    let header = match kind {
        AudioDeviceKind::Output => "Sink #",
        AudioDeviceKind::Input => "Source #",
    };
    let push_device =
        |devices: &mut Vec<AudioDevice>, id: &mut String, name: &mut String, label: &mut String| {
            if name.is_empty() {
                id.clear();
                label.clear();
                return;
            }
            let display = if label.is_empty() {
                prettify_audio_name(name)
            } else {
                label.clone()
            };
            let is_default = default_name.is_some_and(|default| default == name || default == id);
            devices.push(AudioDevice {
                id: id.clone(),
                name: name.clone(),
                label: display,
                is_default,
            });
            id.clear();
            name.clear();
            label.clear();
        };

    for line in text.lines() {
        if let Some(value) = line.trim().strip_prefix(header) {
            push_device(&mut devices, &mut id, &mut name, &mut label);
            id = value.trim().to_string();
        } else if let Some(value) = line.trim().strip_prefix("Name:") {
            name = value.trim().to_string();
        } else if let Some(value) = line.trim().strip_prefix("Description:") {
            label = value.trim().to_string();
        }
    }
    push_device(&mut devices, &mut id, &mut name, &mut label);
    if devices.is_empty() {
        read_pactl_short_audio_devices(kind, default_name)
    } else {
        devices
    }
}

fn read_pactl_short_audio_devices(
    kind: AudioDeviceKind,
    default_name: Option<&str>,
) -> Vec<AudioDevice> {
    let Some(output) = pulse_command_output("pactl", &["list", "short", kind.pactl_list_arg()])
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let id = parts.next()?.to_string();
            let name = parts.next()?.to_string();
            let is_default = default_name.is_some_and(|default| default == name || default == id);
            Some(AudioDevice {
                id,
                label: prettify_audio_name(&name),
                name,
                is_default,
            })
        })
        .collect()
}

fn read_wpctl_audio_devices(kind: AudioDeviceKind) -> Vec<AudioDevice> {
    let Some(output) = pulse_command_output("wpctl", &["status"]) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let wanted = match kind {
        AudioDeviceKind::Output => "Sinks:",
        AudioDeviceKind::Input => "Sources:",
    };
    let mut in_audio = false;
    let mut in_section = false;
    let mut devices = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "Audio" {
            in_audio = true;
            in_section = false;
            continue;
        }
        if matches!(trimmed, "Video" | "Settings") {
            in_audio = false;
            in_section = false;
        }
        if !in_audio {
            continue;
        }
        if trimmed.contains(wanted) {
            in_section = true;
            continue;
        }
        if in_section && (trimmed.starts_with("├─") || trimmed.starts_with("└─")) {
            break;
        }
        if !in_section {
            continue;
        }
        let Some(dot) = trimmed.find('.') else {
            continue;
        };
        let prefix = trimmed[..dot].replace(['│', '*'], " ");
        let Some(id) = prefix.split_whitespace().last() else {
            continue;
        };
        if !id.chars().all(|ch| ch.is_ascii_digit()) {
            continue;
        }
        let rest = trimmed[dot + 1..].trim();
        let label = rest
            .split_once("  [")
            .map(|(label, _)| label)
            .or_else(|| rest.split_once(" [").map(|(label, _)| label))
            .unwrap_or(rest)
            .trim();
        if label.is_empty() {
            continue;
        }
        devices.push(AudioDevice {
            id: id.to_string(),
            name: id.to_string(),
            label: label.to_string(),
            is_default: trimmed.contains('*'),
        });
    }
    devices
}

fn prettify_audio_name(name: &str) -> String {
    name.replace("alsa_output.", "")
        .replace("alsa_input.", "")
        .replace("pci-", "PCI ")
        .replace("usb-", "USB ")
        .replace(['_', '.'], " ")
}

fn read_audio_volume_percent() -> Option<u8> {
    if let Some(output) = pulse_command_output("pactl", &["get-sink-volume", "@DEFAULT_SINK@"]) {
        if output.status.success() {
            if let Some(percent) = parse_first_percent(&String::from_utf8_lossy(&output.stdout)) {
                return Some(percent);
            }
        }
    }
    if let Some(output) = pulse_command_output("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"]) {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Some(value) = text
                .split_whitespace()
                .find_map(|token| token.parse::<f32>().ok())
            {
                return Some((value * 100.0).round().clamp(0.0, 100.0) as u8);
            }
        }
    }
    None
}

fn parse_first_percent(text: &str) -> Option<u8> {
    text.split_whitespace().find_map(|token| {
        token
            .trim_end_matches(',')
            .strip_suffix('%')?
            .parse::<u16>()
            .ok()
            .map(|value| value.min(100) as u8)
    })
}

fn set_audio_volume_percent(percent: u8) -> Result<(), String> {
    let percent = percent.min(100);
    let pactl_percent = format!("{percent}%");
    let mut pactl = Command::new("pactl");
    pactl.args(["set-sink-volume", "@DEFAULT_SINK@", &pactl_percent]);
    apply_pulse_env_defaults(&mut pactl);
    if command_status_success(&mut pactl) {
        let mut unmute = Command::new("pactl");
        unmute.args(["set-sink-mute", "@DEFAULT_SINK@", "0"]);
        apply_pulse_env_defaults(&mut unmute);
        let _ = command_status_success(&mut unmute);
        return Ok(());
    }

    let wpctl_value = format!("{:.2}", f32::from(percent) / 100.0);
    let mut wpctl = Command::new("wpctl");
    wpctl.args(["set-volume", "@DEFAULT_AUDIO_SINK@", &wpctl_value]);
    apply_pulse_env_defaults(&mut wpctl);
    if command_status_success(&mut wpctl) {
        let mut unmute = Command::new("wpctl");
        unmute.args(["set-mute", "@DEFAULT_AUDIO_SINK@", "0"]);
        apply_pulse_env_defaults(&mut unmute);
        let _ = command_status_success(&mut unmute);
        return Ok(());
    }

    let mut amixer = Command::new("amixer");
    amixer.args(["-D", "pulse", "sset", "Master", &pactl_percent]);
    if command_status_success(&mut amixer) {
        return Ok(());
    }

    Err("Could not set audio volume".to_string())
}

fn set_default_audio_device(kind: AudioDeviceKind, device: &AudioDevice) -> Result<(), String> {
    let mut pactl = Command::new("pactl");
    pactl.args([kind.pactl_set_default_command(), &device.name]);
    apply_pulse_env_defaults(&mut pactl);
    if command_status_success(&mut pactl) {
        move_current_audio_streams(kind, &device.name);
        return Ok(());
    }

    if !device.id.is_empty() {
        let mut wpctl = Command::new("wpctl");
        wpctl.args(["set-default", &device.id]);
        apply_pulse_env_defaults(&mut wpctl);
        if command_status_success(&mut wpctl) {
            return Ok(());
        }
    }

    Err(format!("Could not set {} as default", device.label))
}

fn move_current_audio_streams(kind: AudioDeviceKind, device_name: &str) {
    let (list_arg, move_arg) = match kind {
        AudioDeviceKind::Output => ("sink-inputs", "move-sink-input"),
        AudioDeviceKind::Input => ("source-outputs", "move-source-output"),
    };
    let Some(output) = pulse_command_output("pactl", &["list", "short", list_arg]) else {
        return;
    };
    if !output.status.success() {
        return;
    }
    for stream_id in String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
    {
        let mut cmd = Command::new("pactl");
        cmd.args([move_arg, stream_id, device_name]);
        apply_pulse_env_defaults(&mut cmd);
        let _ = command_status_success(&mut cmd);
    }
}

fn read_network_details() -> Vec<String> {
    let mut out = Vec::new();
    for nic in read_nics() {
        let state = fs::read_to_string(format!("/sys/class/net/{nic}/operstate"))
            .unwrap_or_default()
            .trim()
            .to_string();
        out.push(format!("{nic}  {state}"));
        if let Ok(output) = Command::new("ip")
            .args(["-o", "addr", "show", "dev", &nic])
            .output()
        {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let parts = line.split_whitespace().collect::<Vec<_>>();
                if parts.len() > 3 && (parts[2] == "inet" || parts[2] == "inet6") {
                    out.push(format!("{} {}", parts[2], parts[3]));
                }
            }
        }
    }
    if out.is_empty() {
        out.push("No network devices found".to_string());
    }
    out
}

fn scan_wifi_networks(rescan: bool) -> Result<Vec<WifiNetwork>, String> {
    let rescan_val = if rescan { "yes" } else { "no" };
    let output = Command::new("nmcli")
        .args([
            "-t", "-f", "SSID", "dev", "wifi", "list", "--rescan", rescan_val,
        ])
        .output()
        .map_err(|err| format!("Could not run nmcli Wi-Fi scan: {err}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(compact(&format!("Wi-Fi scan failed: {}", err.trim()), 70));
    }

    let mut networks = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let ssid = unescape_nmcli_field(line.trim());
        if ssid.is_empty()
            || networks
                .iter()
                .any(|network: &WifiNetwork| network.ssid == ssid)
        {
            continue;
        }
        networks.push(WifiNetwork { ssid });
    }
    Ok(networks)
}

fn connect_wifi_network(ssid: &str, password: &str) -> Result<(), String> {
    let mut cmd = Command::new("nmcli");
    cmd.args(["dev", "wifi", "connect", ssid]);
    if !password.is_empty() {
        cmd.args(["password", password]);
    }
    let output = cmd
        .output()
        .map_err(|err| format!("Could not run nmcli connect: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        Err(compact(&format!("Wi-Fi connect failed: {message}"), 70))
    }
}

fn disconnect_current_wifi() -> Result<(), String> {
    let Some(wifi) = read_connected_wifi() else {
        return Err("No connected Wi-Fi to disconnect".to_string());
    };
    let output = Command::new("nmcli")
        .args(["dev", "disconnect", &wifi.device])
        .output()
        .map_err(|err| format!("Could not run nmcli disconnect: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        Err(compact(&format!("Wi-Fi disconnect failed: {message}"), 70))
    }
}

fn read_connected_wifi() -> Option<WifiConnection> {
    let output = Command::new("nmcli")
        .args(["-t", "-f", "DEVICE,TYPE,STATE", "dev", "status"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let device = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let parts = split_nmcli_line(line);
            (parts.len() >= 3 && parts[1] == "wifi" && parts[2] == "connected")
                .then(|| parts[0].clone())
        })?;

    let ssid = Command::new("nmcli")
        .args(["-t", "-f", "ACTIVE,SSID", "dev", "wifi"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| {
                    let parts = split_nmcli_line(line);
                    (parts.len() >= 2 && parts[0] == "yes").then(|| parts[1].clone())
                })
        })
        .unwrap_or_else(|| device.clone());
    let ip = Command::new("ip")
        .args(["-o", "-4", "addr", "show", "dev", &device])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .collect::<Vec<_>>()
                .windows(2)
                .find_map(|parts| (parts[0] == "inet").then(|| parts[1].to_string()))
        });
    Some(WifiConnection { ssid, device, ip })
}

fn unescape_nmcli_field(value: &str) -> String {
    let mut out = String::new();
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    if escaped {
        out.push('\\');
    }
    out
}

fn split_nmcli_line(line: &str) -> Vec<String> {
    line.split(':').map(unescape_nmcli_field).collect()
}

fn read_wifi_radio_enabled() -> bool {
    if let Ok(output) = Command::new("nmcli").args(["radio", "wifi"]).output() {
        if output.status.success() {
            let status = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return status == "enabled";
        }
    }
    true
}

fn set_wifi_radio_enabled(enabled: bool) -> Result<(), String> {
    let arg = if enabled { "on" } else { "off" };
    let output = Command::new("nmcli")
        .args(["radio", "wifi", arg])
        .output()
        .map_err(|err| format!("Could not run nmcli radio wifi: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn password_mask(len: usize) -> String {
    "*".repeat(len.min(32))
}

fn read_bluetooth_devices() -> Vec<String> {
    let Ok(output) = Command::new("bluetoothctl")
        .arg("devices")
        .arg("Connected")
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim_start_matches("Device ").to_string())
        .collect()
}

fn read_autostart_apps() -> Vec<String> {
    let mut apps = Vec::new();
    for dir in [
        home_dir().join(".config/autostart"),
        PathBuf::from("/etc/xdg/autostart"),
    ] {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let text = fs::read_to_string(entry.path()).unwrap_or_default();
            let name = text
                .lines()
                .find_map(|line| line.strip_prefix("Name=").map(str::to_string))
                .unwrap_or_else(|| entry.file_name().to_string_lossy().to_string());
            apps.push(name);
        }
    }
    apps.sort();
    apps.dedup();
    apps
}

fn terminal_settings_path() -> PathBuf {
    home_dir().join(".config/aurora-wm/settings.conf")
}

fn read_setting_value(key: &str) -> Option<String> {
    fs::read_to_string(terminal_settings_path())
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let (line_key, value) = line.split_once('=')?;
                (line_key == key).then(|| value.to_string())
            })
        })
}

fn read_u32_setting(key: &str, fallback: u32) -> u32 {
    read_setting_value(key)
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(fallback)
}

fn read_bool_setting(key: &str, fallback: bool) -> bool {
    read_setting_value(key)
        .and_then(|value| match value.trim() {
            "1" | "true" | "on" | "yes" => Some(true),
            "0" | "false" | "off" | "no" => Some(false),
            _ => None,
        })
        .unwrap_or(fallback)
}

fn read_app_command(kind: DefaultAppKind) -> String {
    read_setting_value(kind.key()).unwrap_or_default()
}

fn save_app_commands(settings: &SettingsState) -> AnyResult<()> {
    let path = terminal_settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let clean = |command: &str| command.replace(['\n', '\r'], "");
    fs::write(
        path,
        format!(
            "terminal={}\nbrowser={}\nphoto={}\nvideo={}\nsleep_after_secs={}\nbrightness_percent={}\ncompositor_enabled={}\nauto_power_saver_enabled={}\nauto_power_saver_minutes={}\n",
            clean(&settings.terminal_command),
            clean(&settings.browser_command),
            clean(&settings.photo_command),
            clean(&settings.video_command),
            settings.sleep_after_secs.min(7200),
            settings.brightness_percent.clamp(10, 100),
            u8::from(settings.compositor_enabled),
            u8::from(settings.auto_power_saver_enabled),
            settings.auto_power_saver_minutes.min(240),
        ),
    )?;
    Ok(())
}

fn read_current_power_mode() -> Option<PowerMode> {
    let output = Command::new("powerprofilesctl").arg("get").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout);
    PowerMode::from_command_value(value.trim())
}

fn current_power_mode_cached_or_refresh() -> Option<PowerMode> {
    if power_mode_cache_fresh() {
        return read_cached_power_mode();
    }

    if let Some(_lock) = try_tmp_file_lock(
        POWER_PROFILE_LOCK_PATH,
        IDLE_CHECK_INTERVAL + Duration::from_secs(5),
    ) {
        if power_mode_cache_fresh() {
            return read_cached_power_mode();
        }
        if let Some(mode) = read_current_power_mode() {
            let _ = write_power_mode_cache(mode);
            return Some(mode);
        }
    }

    read_cached_power_mode()
}

fn power_mode_cache_fresh() -> bool {
    file_age(POWER_PROFILE_CACHE_PATH).is_some_and(|age| age < IDLE_CHECK_INTERVAL)
}

fn read_cached_power_mode() -> Option<PowerMode> {
    let text = fs::read_to_string(POWER_PROFILE_CACHE_PATH).ok()?;
    PowerMode::from_command_value(text.trim())
}

fn write_power_mode_cache(mode: PowerMode) -> AnyResult<()> {
    fs::write(
        POWER_PROFILE_CACHE_PATH,
        format!("{}\n", mode.command_value()),
    )?;
    Ok(())
}

struct TmpFileLock {
    path: &'static str,
}

impl Drop for TmpFileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path);
    }
}

fn try_tmp_file_lock(path: &'static str, stale_after: Duration) -> Option<TmpFileLock> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(_) => Some(TmpFileLock { path }),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            if file_age(path).is_some_and(|age| age > stale_after) {
                let _ = fs::remove_file(path);
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .ok()
                    .map(|_| TmpFileLock { path })
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

fn touch_notidle_marker() -> AnyResult<()> {
    if file_age(NOT_IDLE_MARKER_PATH).is_some_and(|age| age < Duration::from_secs(1)) {
        return Ok(());
    }
    fs::write(NOT_IDLE_MARKER_PATH, b"notidle\n")?;
    Ok(())
}

fn notidle_marker_age() -> Option<Duration> {
    file_age(NOT_IDLE_MARKER_PATH)
}

fn file_age(path: &str) -> Option<Duration> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()?
        .elapsed()
        .ok()
}

fn read_desktop_entries() -> Vec<DesktopEntry> {
    let mut entries = Vec::new();
    let mut dirs = vec![
        home_dir().join(".local/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
        PathBuf::from("/usr/share/applications"),
    ];
    dirs.dedup();
    for dir in dirs {
        let Ok(read_dir) = fs::read_dir(dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            if entry.path().extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let text = fs::read_to_string(entry.path()).unwrap_or_default();
            if text.lines().any(|line| line == "NoDisplay=true") {
                continue;
            }
            let name = text
                .lines()
                .find_map(|line| line.strip_prefix("Name=").map(str::to_string))
                .unwrap_or_else(|| entry.file_name().to_string_lossy().to_string());
            let cats = text
                .lines()
                .find_map(|line| line.strip_prefix("Categories=").map(str::to_string))
                .unwrap_or_default();
            let mime_types = text
                .lines()
                .find_map(|line| line.strip_prefix("MimeType=").map(str::to_string))
                .unwrap_or_default();
            let command = text
                .lines()
                .find_map(|line| line.strip_prefix("Exec=").map(clean_desktop_command))
                .unwrap_or_default();
            let category = if cats.contains("Network") {
                "Internet"
            } else if cats.contains("System") || cats.contains("Settings") {
                "System"
            } else if cats.contains("Utility") || cats.contains("Development") {
                "Program"
            } else if cats.contains("Audio") || cats.contains("Video") || cats.contains("Graphics")
            {
                "Media"
            } else {
                "Other"
            }
            .to_string();
            entries.push(DesktopEntry {
                name,
                category,
                command,
                categories: cats,
                mime_types,
            });
        }
    }
    entries.sort_by(|a, b| a.category.cmp(&b.category).then(a.name.cmp(&b.name)));
    entries
}

fn clean_desktop_command(value: &str) -> String {
    value
        .split_whitespace()
        .map(|arg| {
            ["%f", "%F", "%u", "%U", "%i", "%c", "%k"]
                .iter()
                .fold(arg.to_string(), |clean, field| clean.replace(field, ""))
        })
        .filter(|arg| !arg.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn discover_installed_apps() -> (
    Vec<InstalledApp>,
    Vec<InstalledApp>,
    Vec<InstalledApp>,
    Vec<InstalledApp>,
) {
    let entries = read_desktop_entries();
    (
        installed_apps(DefaultAppKind::Terminal, &entries),
        installed_apps(DefaultAppKind::Browser, &entries),
        installed_apps(DefaultAppKind::Photo, &entries),
        installed_apps(DefaultAppKind::Video, &entries),
    )
}

fn installed_apps(kind: DefaultAppKind, entries: &[DesktopEntry]) -> Vec<InstalledApp> {
    let mut apps = Vec::new();
    if kind == DefaultAppKind::Terminal {
        for command in TERMINAL_FALLBACKS {
            if command_exists(command) {
                push_installed_app(&mut apps, command.to_string(), command.to_string());
            }
        }
    }
    for entry in entries {
        if entry.command.is_empty() || !command_can_launch(&entry.command) {
            continue;
        }
        let command_lower = entry.command.to_ascii_lowercase();
        let name_lower = entry.name.to_ascii_lowercase();
        let matches = match kind {
            DefaultAppKind::Terminal => {
                entry.categories.contains("TerminalEmulator")
                    || [
                        "terminal",
                        "konsole",
                        "xterm",
                        "kitty",
                        "alacritty",
                        "wezterm",
                    ]
                    .iter()
                    .any(|term| name_lower.contains(term) || command_lower.contains(term))
            }
            DefaultAppKind::Browser => {
                entry.categories.contains("WebBrowser")
                    || entry.mime_types.contains("x-scheme-handler/http")
            }
            DefaultAppKind::Photo => entry.mime_types.contains("image/"),
            DefaultAppKind::Video => entry.mime_types.contains("video/"),
        };
        if matches {
            push_installed_app(&mut apps, entry.name.clone(), entry.command.clone());
        }
    }
    if kind != DefaultAppKind::Terminal {
        apps.sort_by(|left, right| left.name.cmp(&right.name));
    }
    apps
}

fn push_installed_app(apps: &mut Vec<InstalledApp>, name: String, command: String) {
    if apps.iter().any(|app| app.command == command) {
        return;
    }
    apps.push(InstalledApp { name, command });
}

fn command_can_launch(command: &str) -> bool {
    let executable = command
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(['\'', '"']);
    let path = Path::new(executable);
    if path.is_absolute() {
        path.exists()
    } else {
        command_exists(executable)
    }
}

fn read_net_totals() -> Option<NetTotals> {
    let text = fs::read_to_string("/proc/net/dev").ok()?;
    let mut rx = 0u64;
    let mut tx = 0u64;
    for line in text.lines().skip(2) {
        let Some((iface, data)) = line.split_once(':') else {
            continue;
        };
        if iface.trim() == "lo" {
            continue;
        }
        let nums: Vec<u64> = data
            .split_whitespace()
            .filter_map(|p| p.parse::<u64>().ok())
            .collect();
        if nums.len() >= 16 {
            rx = rx.saturating_add(nums[0]);
            tx = tx.saturating_add(nums[8]);
        }
    }
    Some(NetTotals {
        rx,
        tx,
        at: Instant::now(),
    })
}

fn read_battery() -> Option<String> {
    let entries = fs::read_dir("/sys/class/power_supply").ok()?;
    for entry in entries.flatten() {
        let ty = fs::read_to_string(entry.path().join("type")).unwrap_or_default();
        if ty.trim() != "Battery" {
            continue;
        }
        let cap = fs::read_to_string(entry.path().join("capacity")).ok()?;
        let status = fs::read_to_string(entry.path().join("status")).unwrap_or_default();
        return Some(format!("{}% {}", cap.trim(), status.trim()));
    }
    None
}

fn format_kib(kib: u64) -> String {
    if kib >= 1024 * 1024 {
        format!("{:.1} GiB", kib as f64 / 1024.0 / 1024.0)
    } else if kib >= 1024 {
        format!("{:.0} MiB", kib as f64 / 1024.0)
    } else {
        format!("{kib} KiB")
    }
}

fn format_bps(value: f64) -> String {
    if value >= 1024.0 * 1024.0 {
        format!("{:.1} MB/s", value / 1024.0 / 1024.0)
    } else if value >= 1024.0 {
        format!("{:.1} KB/s", value / 1024.0)
    } else {
        format!("{value:.0} B/s")
    }
}

fn compact(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        let mut out = value
            .chars()
            .take(max_chars.saturating_sub(3))
            .collect::<String>();
        out.push_str("...");
        out
    }
}

fn clipboard_text_preview_lines(text: &str) -> (String, Option<String>) {
    let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let total_chars = cleaned.chars().count();
    if total_chars > 60 {
        let first = cleaned.chars().take(40).collect::<String>();
        let mut second = String::from("...");
        second.extend(cleaned.chars().skip(total_chars - 20));
        return (first, Some(second));
    }
    if total_chars > 40 {
        let first = cleaned.chars().take(40).collect::<String>();
        let second = cleaned.chars().skip(40).collect::<String>();
        return (first, Some(second));
    }
    (cleaned, None)
}

fn clipboard_entry_row_height(entry: &ClipboardEntry) -> i32 {
    match entry.item {
        ClipboardItem::Text(_) => CLIPBOARD_MENU_TEXT_ROW_HEIGHT,
        ClipboardItem::Image(_) => CLIPBOARD_MENU_IMAGE_ROW_HEIGHT,
    }
}

fn clipboard_image_type_label(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_uppercase())
        .filter(|ext| !ext.is_empty())
        .unwrap_or_else(|| "IMAGE".to_string())
}

fn format_size_mb(bytes: u64) -> String {
    format!("{:.2} MB", bytes as f64 / 1024.0 / 1024.0)
}

fn folder_entry_info(entry: &FolderEntry) -> String {
    let size = fs::metadata(&entry.path).map(|m| m.len()).unwrap_or(0);
    let mut parts = vec![
        entry.name.clone(),
        file_kind_label(entry.kind).to_string(),
        format_size_mb(size),
    ];
    if entry.kind == FileKind::Image {
        if let Some((w, h)) = image_dimensions(&entry.path) {
            parts.push(format!("{w}x{h}"));
        }
    }
    parts.join("  ")
}

fn image_info_line(path: &Path, cached_resolution: Option<(u32, u32)>) -> String {
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut parts = vec![format_size_mb(size)];
    if let Some((w, h)) = cached_resolution.or_else(|| image_dimensions(path)) {
        parts.push(format!("{w}x{h}"));
    }
    parts.push(
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Image")
            .to_string(),
    );
    parts.join("  ")
}

fn viewer_status(media: &MediaState) -> String {
    let size = fs::metadata(&media.entry.path)
        .map(|m| m.len())
        .unwrap_or(0);
    if media.entry.kind == FileKind::Text {
        let lines = media.text_lines.len();
        let words = media
            .text_lines
            .iter()
            .map(|line| line.split_whitespace().count())
            .sum::<usize>();
        format!("{lines} lines  {words} words  {}", format_size_mb(size))
    } else {
        format_size_mb(size)
    }
}

fn unknown_file_info_line(media: &MediaState, local_y: i32) -> Option<String> {
    let idx = (local_y - 112) / 24;
    if !(0..4).contains(&idx) {
        return None;
    }
    let meta = fs::metadata(&media.entry.path).ok();
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let modified = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| format!("modified {}s", d.as_secs()))
        .unwrap_or_else(|| "modified unknown".to_string());
    let kind = media
        .file_info
        .as_deref()
        .unwrap_or("Unknown file type")
        .to_string();
    match idx {
        0 => Some(media.entry.name.clone()),
        1 => Some(format_size_mb(size)),
        2 => Some(modified),
        3 => Some(kind),
        _ => None,
    }
}

fn normalized_media_selection(selection: &MediaTextSelection) -> ((usize, usize), (usize, usize)) {
    let start = (selection.start_line, selection.start_col);
    let end = (selection.end_line, selection.end_col);
    if start <= end {
        (start, end)
    } else {
        (end, start)
    }
}

fn selected_text_from_lines(lines: &[String], selection: &MediaTextSelection) -> String {
    let (start, end) = normalized_media_selection(selection);
    if start == end {
        return String::new();
    }
    let mut out = String::new();
    for line_no in start.0..=end.0.min(lines.len().saturating_sub(1)) {
        let Some(line) = lines.get(line_no) else {
            continue;
        };
        let line_len = line.chars().count();
        let start_col = if line_no == start.0 {
            start.1.min(line_len)
        } else {
            0
        };
        let end_col = if line_no == end.0 {
            end.1.min(line_len)
        } else {
            line_len
        };
        if end_col > start_col {
            out.push_str(
                &line
                    .chars()
                    .skip(start_col)
                    .take(end_col - start_col)
                    .collect::<String>(),
            );
        }
        if line_no != end.0 {
            out.push('\n');
        }
    }
    out
}

fn push_text_undo(media: &mut MediaState) {
    media.text_undo.push(media.text_lines.clone());
    if media.text_undo.len() > 64 {
        media.text_undo.remove(0);
    }
}

fn delete_text_selection(media: &mut MediaState, selection: &MediaTextSelection) {
    if media.text_lines.is_empty() {
        media.text_lines.push(String::new());
        media.text_cursor_line = 0;
        media.text_cursor_col = 0;
        return;
    }
    let (start, end) = normalized_media_selection(selection);
    if start == end {
        media.text_cursor_line = start.0.min(media.text_lines.len().saturating_sub(1));
        media.text_cursor_col = start.1;
        return;
    }
    let start_line = start.0.min(media.text_lines.len().saturating_sub(1));
    let end_line = end.0.min(media.text_lines.len().saturating_sub(1));
    let start_col = start.1.min(media.text_lines[start_line].chars().count());
    let end_col = end.1.min(media.text_lines[end_line].chars().count());
    let start_byte = nth_char_byte(&media.text_lines[start_line], start_col);
    let end_byte = nth_char_byte(&media.text_lines[end_line], end_col);
    if start_line == end_line {
        media.text_lines[start_line].replace_range(start_byte..end_byte, "");
    } else {
        let prefix = media.text_lines[start_line][..start_byte].to_string();
        let suffix = media.text_lines[end_line][end_byte..].to_string();
        media
            .text_lines
            .splice(start_line..=end_line, [format!("{prefix}{suffix}")]);
    }
    if media.text_lines.is_empty() {
        media.text_lines.push(String::new());
    }
    media.text_cursor_line = start_line.min(media.text_lines.len().saturating_sub(1));
    media.text_cursor_col = start_col.min(media.text_lines[media.text_cursor_line].chars().count());
}

fn insert_text_at_cursor(media: &mut MediaState, text: &str) {
    if media.text_lines.is_empty() {
        media.text_lines.push(String::new());
    }
    let line_idx = media
        .text_cursor_line
        .min(media.text_lines.len().saturating_sub(1));
    let col = media
        .text_cursor_col
        .min(media.text_lines[line_idx].chars().count());
    let byte_idx = nth_char_byte(&media.text_lines[line_idx], col);
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let parts = normalized.split('\n').collect::<Vec<_>>();
    if parts.len() == 1 {
        media.text_lines[line_idx].insert_str(byte_idx, &normalized);
        media.text_cursor_line = line_idx;
        media.text_cursor_col = col + normalized.chars().count();
        return;
    }
    let tail = media.text_lines[line_idx].split_off(byte_idx);
    media.text_lines[line_idx].push_str(parts[0]);
    let mut insert_at = line_idx + 1;
    for part in parts.iter().skip(1).take(parts.len().saturating_sub(2)) {
        media.text_lines.insert(insert_at, (*part).to_string());
        insert_at += 1;
    }
    let last = parts.last().copied().unwrap_or("");
    media.text_lines.insert(insert_at, format!("{last}{tail}"));
    media.text_cursor_line = insert_at;
    media.text_cursor_col = last.chars().count();
}

fn media_text_copy_button_rect(x: i32, y: i32, w: i32, h: i32) -> (i32, i32, i32, i32) {
    (x + w - 48, y + h - 42, 32, 30)
}

fn selection_border_rects(x: i16, y: i16, w: u16, h: u16) -> [Rectangle; 4] {
    let bw = 2u16;
    let right_x = x.saturating_add(w.saturating_sub(bw) as i16);
    let bottom_y = y.saturating_add(h.saturating_sub(bw) as i16);
    [
        Rectangle {
            x,
            y,
            width: w,
            height: bw,
        },
        Rectangle {
            x,
            y: bottom_y,
            width: w,
            height: bw,
        },
        Rectangle {
            x,
            y,
            width: bw,
            height: h,
        },
        Rectangle {
            x: right_x,
            y,
            width: bw,
            height: h,
        },
    ]
}

fn same_rects(a: &[Rectangle], b: &[Rectangle]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(left, right)| {
            left.x == right.x
                && left.y == right.y
                && left.width == right.width
                && left.height == right.height
        })
}

fn terminal_point_to_cell(
    x: i32,
    y: i32,
    cell_w: i32,
    cell_h: i32,
    cols: usize,
    rows: usize,
) -> (usize, usize) {
    let row = ((y - 52).max(0) / cell_h).clamp(0, rows as i32 - 1) as usize;
    let col = ((x - 18).max(0) / cell_w).clamp(0, cols as i32 - 1) as usize;
    (row, col)
}

fn terminal_selection_rects(
    selection: TerminalSelection,
    rows: &[String],
    cell_w: i32,
    cell_h: i32,
) -> Vec<Rectangle> {
    let start = (selection.start_row, selection.start_col);
    let end = (selection.end_row, selection.end_col);
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    let mut rects = Vec::new();
    for row in start.0..=end.0.min(rows.len().saturating_sub(1)) {
        let line_len = rows.get(row).map(|line| line.chars().count()).unwrap_or(0);
        let start_col = if row == start.0 {
            start
                .1
                .min(rows.get(row).map(|line| line.chars().count()).unwrap_or(0))
        } else {
            0
        };
        let end_col = if row == end.0 {
            end.1
                .min(rows.get(row).map(|line| line.chars().count()).unwrap_or(0))
        } else {
            line_len.max(start_col)
        };
        if end_col <= start_col {
            continue;
        }
        rects.push(Rectangle {
            x: (18 + start_col as i32 * cell_w) as i16,
            y: (53 + row as i32 * cell_h) as i16,
            width: ((end_col - start_col) as i32 * cell_w).max(4) as u16,
            height: (cell_h - 3).max(10) as u16,
        });
    }
    rects
}

fn selected_terminal_text(selection: TerminalSelection, rows: &[String]) -> String {
    let start = (selection.start_row, selection.start_col);
    let end = (selection.end_row, selection.end_col);
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    if start == end {
        return String::new();
    }
    let mut out = String::new();
    for row in start.0..=end.0.min(rows.len().saturating_sub(1)) {
        let line = rows.get(row).map(String::as_str).unwrap_or("");
        let line_len = line.chars().count();
        let start_col = if row == start.0 {
            start.1.min(line_len)
        } else {
            0
        };
        let end_col = if row == end.0 {
            end.1.min(line_len)
        } else {
            line_len
        };
        if end_col > start_col {
            out.push_str(
                &line
                    .chars()
                    .skip(start_col)
                    .take(end_col - start_col)
                    .collect::<String>(),
            );
        }
        if row != end.0 {
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

fn text_position_for_point(
    media: &MediaState,
    font: &Font<'static>,
    x: i32,
    y: i32,
    preview_x: i32,
    preview_y: i32,
) -> (usize, usize) {
    let line_h = 19;
    let clicked = ((y - preview_y - 12).max(0) / line_h) as usize;
    let line_idx = (media.text_scroll + clicked).min(media.text_lines.len().saturating_sub(1));
    let text_x = preview_x + 42;
    let line = media
        .text_lines
        .get(line_idx)
        .map(String::as_str)
        .unwrap_or("");
    (line_idx, cursor_col_for_x(font, line, x - text_x, 13.0))
}

fn nth_char_byte(value: &str, col: usize) -> usize {
    value
        .char_indices()
        .nth(col)
        .map(|(idx, _)| idx)
        .unwrap_or(value.len())
}

fn fast_text_width_cols(
    font: &Font<'static>,
    line: &str,
    start_col: usize,
    end_col: usize,
    size: f32,
) -> i32 {
    if end_col <= start_col {
        return 0;
    }
    let scale = Scale::uniform(size);
    let space = font
        .glyph(' ')
        .scaled(scale)
        .h_metrics()
        .advance_width
        .max(1.0);
    let width = line
        .chars()
        .skip(start_col)
        .take(end_col - start_col)
        .map(|ch| {
            if ch == '\t' {
                space * 4.0
            } else {
                font.glyph(ch)
                    .scaled(scale)
                    .h_metrics()
                    .advance_width
                    .max(space)
            }
        })
        .sum::<f32>();
    width.ceil() as i32
}

fn cursor_col_for_x(font: &Font<'static>, line: &str, x: i32, size: f32) -> usize {
    if x <= 0 {
        return 0;
    }
    let scale = Scale::uniform(size);
    let space = font
        .glyph(' ')
        .scaled(scale)
        .h_metrics()
        .advance_width
        .max(1.0);
    let mut width = 0.0f32;
    let target = x as f32;
    for (col, ch) in line.chars().enumerate() {
        let advance = if ch == '\t' {
            space * 4.0
        } else {
            font.glyph(ch)
                .scaled(scale)
                .h_metrics()
                .advance_width
                .max(space)
        };
        let next = width + advance;
        if target < next {
            return if target - width < next - target {
                col
            } else {
                col + 1
            };
        }
        width = next;
    }
    line.chars().count()
}

fn terminal_display_char(ch: char, line_drawing: bool) -> char {
    if !line_drawing {
        return ch;
    }
    match ch {
        'q' => '-',
        'x' => '|',
        'l' | 'k' | 'm' | 'j' | 't' | 'u' | 'v' | 'w' | 'n' => '+',
        _ => ch,
    }
}

fn ansi_color(idx: u8) -> Color {
    match idx {
        0 => Color::rgb(40, 40, 40),     // Black
        1 => Color::rgb(205, 0, 0),      // Red
        2 => Color::rgb(0, 205, 0),      // Green
        3 => Color::rgb(205, 205, 0),    // Yellow
        4 => Color::rgb(0, 0, 238),      // Blue
        5 => Color::rgb(205, 0, 205),    // Magenta
        6 => Color::rgb(0, 205, 205),    // Cyan
        7 => Color::rgb(229, 229, 229),  // White
        8 => Color::rgb(127, 127, 127),  // Bright Black
        9 => Color::rgb(255, 0, 0),      // Bright Red
        10 => Color::rgb(0, 255, 0),     // Bright Green
        11 => Color::rgb(255, 255, 0),   // Bright Yellow
        12 => Color::rgb(92, 92, 255),   // Bright Blue
        13 => Color::rgb(255, 0, 255),   // Bright Magenta
        14 => Color::rgb(0, 255, 255),   // Bright Cyan
        15 => Color::rgb(255, 255, 255), // Bright White
        16..=231 => {
            let offset = idx - 16;
            let r = offset / 36;
            let g = (offset % 36) / 6;
            let b = offset % 6;
            let scale = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
            Color::rgb(scale(r), scale(g), scale(b))
        }
        232..=255 => {
            let val = 8 + (idx - 232) * 10;
            Color::rgb(val, val, val)
        }
    }
}

fn csi_values(params: &str) -> Vec<usize> {
    if params.is_empty() {
        return Vec::new();
    }
    params
        .split(';')
        .map(|part| {
            let number = part
                .split(':')
                .next()
                .unwrap_or_default()
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>();
            number.parse::<usize>().unwrap_or(0)
        })
        .collect()
}

fn format_clock() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let month = match u8::from(now.month()) {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        _ => "Dec",
    };
    format!(
        "{} {}   {:02}:{:02}",
        month,
        now.day(),
        now.hour(),
        now.minute()
    )
}

fn command_exists(name: &str) -> bool {
    env::var_os("PATH")
        .and_then(|paths| {
            env::split_paths(&paths)
                .map(|path| path.join(name))
                .find(|path| path.exists())
        })
        .is_some()
}

fn command_status_success(cmd: &mut Command) -> bool {
    cmd.status().is_ok_and(|status| status.success())
}

fn shell_quote_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn pulse_command_output(program: &str, args: &[&str]) -> Option<std::process::Output> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    apply_pulse_env_defaults(&mut cmd);
    cmd.output().ok()
}

fn command_output_timeout(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Option<std::process::Output> {
    let mut cmd = if command_exists("timeout") {
        let mut timeout_cmd = Command::new("timeout");
        timeout_cmd.arg(format!("{:.3}", timeout.as_secs_f64()));
        timeout_cmd.arg(program);
        timeout_cmd
    } else {
        Command::new(program)
    };
    cmd.args(args).stderr(Stdio::null()).output().ok()
}

fn apply_pulse_env_defaults(cmd: &mut Command) {
    let runtime_dir = env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| {
        let uid = unsafe { libc::geteuid() };
        format!("/run/user/{uid}").into()
    });
    cmd.env("XDG_RUNTIME_DIR", &runtime_dir);

    if env::var_os("PULSE_SERVER").is_none() {
        let native = PathBuf::from(&runtime_dir).join("pulse/native");
        let server = format!("unix:{}", native.to_string_lossy());
        cmd.env("PULSE_SERVER", server);
    }
    if env::var_os("PULSE_RUNTIME_PATH").is_none() {
        cmd.env(
            "PULSE_RUNTIME_PATH",
            PathBuf::from(&runtime_dir).join("pulse"),
        );
    }
    if env::var_os("PULSE_COOKIE").is_none() {
        let cookie = home_dir().join(".config/pulse/cookie");
        if cookie.exists() {
            cmd.env("PULSE_COOKIE", cookie);
        }
    }
}

fn spawn_detached(mut cmd: Command) {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Ok(mut child) = cmd.spawn() {
        thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

fn copy_text_to_clipboard(text: &str) {
    for name in ["xclip", "xsel", "wl-copy"] {
        if !command_exists(name) {
            continue;
        }
        let mut cmd = Command::new(name);
        match name {
            "xclip" => {
                cmd.args(["-selection", "clipboard"]);
            }
            "xsel" => {
                cmd.args(["--clipboard", "--input"]);
            }
            _ => {}
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(mut child) = cmd.spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            append_clipboard_history(&ClipboardItem::Text(text.to_string()));
            break;
        }
    }
}

fn read_text_clipboard() -> Option<String> {
    let commands: [(&str, &[&str]); 3] = [
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["--clipboard", "--output"]),
        ("wl-paste", &[]),
    ];
    for (name, args) in commands {
        if !command_exists(name) {
            continue;
        }
        if let Some(output) = command_output_timeout(name, args, CLIPBOARD_COMMAND_TIMEOUT) {
            if output.status.success() {
                return Some(String::from_utf8_lossy(&output.stdout).to_string());
            }
        }
    }
    None
}

fn read_image_clipboard() -> Option<(PathBuf, u64)> {
    let target = clipboard_image_target()?;
    let output = command_output_timeout(
        "xclip",
        &["-selection", "clipboard", "-target", target, "-o"],
        CLIPBOARD_COMMAND_TIMEOUT,
    )?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    let sig = clipboard_image_signature(target, &output.stdout);
    let img = image::load_from_memory(&output.stdout).ok()?.to_rgba8();
    let (width, height) = img.dimensions();
    let dir = clipboard_image_history_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("clipboard-{sig:016x}-{width}x{height}.png"));
    if !path.exists()
        && image::save_buffer_with_format(
            &path,
            img.as_raw(),
            width,
            height,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .is_err()
    {
        return None;
    }
    Some((path, sig))
}

fn clipboard_image_target() -> Option<&'static str> {
    if !command_exists("xclip") {
        return None;
    }
    let output = command_output_timeout(
        "xclip",
        &["-selection", "clipboard", "-target", "TARGETS", "-o"],
        CLIPBOARD_COMMAND_TIMEOUT,
    )?;
    if !output.status.success() {
        return None;
    }
    let targets = String::from_utf8_lossy(&output.stdout);
    [
        "image/png",
        "image/jpeg",
        "image/jpg",
        "image/bmp",
        "image/tiff",
    ]
    .into_iter()
    .find(|target| targets.lines().any(|line| line.trim() == *target))
}

fn clipboard_image_signature(target: &str, bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    target.hash(&mut hasher);
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn clipboard_file_image_signature(path: &Path) -> Option<u64> {
    let bytes = fs::read(path).ok()?;
    Some(clipboard_image_signature("image/png", &bytes))
}

fn clipboard_image_history_dir() -> PathBuf {
    if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("aurora-clipboard-images");
    }
    PathBuf::from(format!("/tmp/aurora-clipboard-images-{}", unsafe {
        libc::geteuid()
    }))
}

fn copy_image_to_clipboard(path: &Path) {
    if command_exists("xclip") {
        let copied = Command::new("xclip")
            .args(["-selection", "clipboard", "-target", "image/png", "-i"])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if copied {
            append_clipboard_history(&ClipboardItem::Image(path.to_path_buf()));
        }
    } else {
        copy_text_to_clipboard(&path.to_string_lossy());
    }
}

fn paste_clipboard_now(display: &str) {
    if !command_exists("xdotool") {
        return;
    }
    let mut cmd = Command::new("sh");
    cmd.env("DISPLAY", display)
        .arg("-c")
        .arg("sleep 0.08; xdotool key ctrl+v")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    spawn_detached(cmd);
}

fn clipboard_history_path() -> PathBuf {
    if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("aurora-clipboard-history");
    }
    PathBuf::from(format!("/tmp/aurora-clipboard-history-{}", unsafe {
        libc::geteuid()
    }))
}

fn append_clipboard_history(item: &ClipboardItem) {
    let path = clipboard_history_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let line = match item {
        ClipboardItem::Text(text) if text.is_empty() || text.len() > 1_000_000 => return,
        ClipboardItem::Text(text) => format!("T\t{}\n", escape_history_field(text)),
        ClipboardItem::Image(path) => {
            format!("I\t{}\n", escape_history_field(&path.to_string_lossy()))
        }
    };
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write;
        let _ = file.write_all(line.as_bytes());
    }
    compact_clipboard_history_store(&path);
}

fn compact_clipboard_history_store(path: &Path) {
    let entries = read_clipboard_history_store_from(path);
    let mut out = String::new();
    for entry in entries.iter().rev() {
        match &entry.item {
            ClipboardItem::Text(text) => {
                out.push_str("T\t");
                out.push_str(&escape_history_field(text));
                out.push('\n');
            }
            ClipboardItem::Image(path) => {
                out.push_str("I\t");
                out.push_str(&escape_history_field(&path.to_string_lossy()));
                out.push('\n');
            }
        }
    }
    let _ = fs::write(path, out);
}

fn read_clipboard_history_store() -> Vec<ClipboardEntry> {
    read_clipboard_history_store_from(&clipboard_history_path())
}

fn read_clipboard_history_store_from(path: &Path) -> Vec<ClipboardEntry> {
    let Ok(data) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut entries: Vec<ClipboardEntry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for line in data.lines().rev() {
        let Some((kind, value)) = line.split_once('\t') else {
            continue;
        };
        let Some(value) = unescape_history_field(value) else {
            continue;
        };
        let item = match kind {
            "T" if !value.is_empty() => ClipboardItem::Text(value),
            "I" => {
                let path = PathBuf::from(value);
                if !path.exists() {
                    continue;
                }
                ClipboardItem::Image(path)
            }
            _ => continue,
        };
        let key = clipboard_item_key(&item);
        if !seen.insert(key) {
            continue;
        }
        entries.push(ClipboardEntry { item });
        if entries.len() >= CLIPBOARD_HISTORY_LIMIT {
            break;
        }
    }
    entries
}

fn clipboard_item_key(item: &ClipboardItem) -> String {
    match item {
        ClipboardItem::Text(text) => {
            let mut key = String::with_capacity(text.len() + 2);
            key.push_str("T\t");
            key.push_str(text);
            key
        }
        ClipboardItem::Image(path) => {
            let value = path.to_string_lossy();
            let mut key = String::with_capacity(value.len() + 2);
            key.push_str("I\t");
            key.push_str(&value);
            key
        }
    }
}

fn clipboard_items_match(a: &ClipboardItem, b: &ClipboardItem) -> bool {
    match (a, b) {
        (ClipboardItem::Text(left), ClipboardItem::Text(right)) => left == right,
        (ClipboardItem::Image(left), ClipboardItem::Image(right)) => left == right,
        _ => false,
    }
}

fn escape_history_field(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'%' | b'\t' | b'\n' | b'\r' => out.push_str(&format!("%{byte:02X}")),
            0x20..=0x7e => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn unescape_history_field(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn move_to_trash(path: &Path) -> AnyResult<()> {
    let trash_files = home_dir().join(".local/share/Trash/files");
    let trash_info = home_dir().join(".local/share/Trash/info");
    fs::create_dir_all(&trash_files)?;
    fs::create_dir_all(&trash_info)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("item");
    let mut dst = trash_files.join(name);
    if dst.exists() {
        let stamp = OffsetDateTime::now_utc().unix_timestamp();
        dst = trash_files.join(format!("{stamp}-{name}"));
    }
    fs::rename(path, &dst)?;
    let info_name = dst.file_name().and_then(|n| n.to_str()).unwrap_or(name);
    let deletion = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string());
    let info = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        path.to_string_lossy(),
        deletion
    );
    fs::write(trash_info.join(format!("{info_name}.trashinfo")), info)?;
    Ok(())
}

fn file_uri(path: &std::path::Path) -> String {
    let mut out = String::from("file://");
    for ch in path.to_string_lossy().chars() {
        match ch {
            ' ' => out.push_str("%20"),
            '#' => out.push_str("%23"),
            '%' => out.push_str("%25"),
            '\n' | '\r' => {}
            _ => out.push(ch),
        }
    }
    out
}

fn path_from_file_uri(uri: &str) -> Option<PathBuf> {
    let raw = uri.strip_prefix("file://")?;
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            let a = chars.next()?;
            let b = chars.next()?;
            let hex = format!("{a}{b}");
            if let Ok(v) = u8::from_str_radix(&hex, 16) {
                out.push(v as char);
            }
        } else {
            out.push(ch);
        }
    }
    Some(PathBuf::from(out))
}

fn app_menu_items() -> Vec<AppMenuItem> {
    vec![
        AppMenuItem {
            label: "Terminal",
            hint: "Open shell",
            action: AppAction::Terminal,
        },
        AppMenuItem {
            label: "Browser",
            hint: "Launch web browser",
            action: AppAction::Browser,
        },
        AppMenuItem {
            label: "Pictures",
            hint: "Browse images",
            action: AppAction::Pictures,
        },
        AppMenuItem {
            label: "Music",
            hint: "Browse audio",
            action: AppAction::Music,
        },
        AppMenuItem {
            label: "Videos",
            hint: "Browse movies",
            action: AppAction::Videos,
        },
        AppMenuItem {
            label: "Settings",
            hint: "Display and power",
            action: AppAction::Settings,
        },
        AppMenuItem {
            label: "More",
            hint: "All desktop apps",
            action: AppAction::More,
        },
    ]
}

fn folder_entries_for(mode: FolderMode, sort: FolderSort) -> Vec<FolderEntry> {
    let home = home_dir();
    let path = folder_path_for(mode);
    let mut entries = folder_entries_in(path, sort);
    if entries.is_empty() && mode == FolderMode::Home {
        for (name, mode) in [
            ("Pictures", FolderMode::Pictures),
            ("Music", FolderMode::Music),
            ("Videos", FolderMode::Videos),
        ] {
            entries.push(FolderEntry {
                name: name.to_string(),
                path: home.join(mode.title()),
                kind: FileKind::Directory,
            });
        }
        sort_folder_entries(&mut entries, sort);
    }
    entries
}

fn folder_path_for(mode: FolderMode) -> PathBuf {
    let home = home_dir();
    match mode {
        FolderMode::Home => home,
        FolderMode::Pictures => home.join("Pictures"),
        FolderMode::Music => home.join("Music"),
        FolderMode::Videos => home.join("Videos"),
    }
}

fn folder_entries_in(path: PathBuf, sort: FolderSort) -> Vec<FolderEntry> {
    let mut entries = Vec::new();
    let mut other_count = 0usize;
    let Ok(read_dir) = fs::read_dir(&path) else {
        return entries;
    };
    for entry in read_dir.flatten() {
        let entry_path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let kind = if entry.file_type().is_ok_and(|ty| ty.is_dir()) {
            FileKind::Directory
        } else {
            file_kind_for(&entry_path)
        };
        if kind == FileKind::Other {
            if other_count >= FOLDER_OTHER_ENTRY_LIMIT {
                continue;
            }
            other_count += 1;
        }
        entries.push(FolderEntry {
            name,
            path: entry_path,
            kind,
        });
        if entries.len() >= FOLDER_ENTRY_LIMIT {
            break;
        }
    }
    sort_folder_entries(&mut entries, sort);
    entries
}

fn sort_folder_entries(entries: &mut [FolderEntry], sort: FolderSort) {
    entries.sort_by(|a, b| {
        let base = (a.kind != FileKind::Directory)
            .cmp(&(b.kind != FileKind::Directory))
            .then((a.kind == FileKind::Other).cmp(&(b.kind == FileKind::Other)));
        if base != std::cmp::Ordering::Equal {
            return base;
        }
        match sort {
            FolderSort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            FolderSort::Date => entry_modified_secs(b)
                .cmp(&entry_modified_secs(a))
                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase())),
            FolderSort::Size => entry_size(b)
                .cmp(&entry_size(a))
                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase())),
        }
    });
}

fn entry_modified_secs(entry: &FolderEntry) -> u64 {
    fs::metadata(&entry.path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn entry_size(entry: &FolderEntry) -> u64 {
    fs::metadata(&entry.path)
        .map(|meta| meta.len())
        .unwrap_or(0)
}

fn spawn_terminal_pty(cwd: &Path, cols: usize, rows: usize) -> AnyResult<(RawFd, libc::pid_t)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut winsize = libc::winsize {
        ws_row: rows as u16,
        ws_col: cols as u16,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &mut winsize,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        return Err(std::io::Error::last_os_error().into());
    }
    if pid == 0 {
        unsafe {
            libc::close(master);
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY, 0);
            libc::dup2(slave, libc::STDIN_FILENO);
            libc::dup2(slave, libc::STDOUT_FILENO);
            libc::dup2(slave, libc::STDERR_FILENO);
            if slave > libc::STDERR_FILENO {
                libc::close(slave);
            }
        }
        let _ = env::set_current_dir(cwd);
        unsafe {
            env::set_var("TERM", "xterm-256color");
            env::set_var("COLORTERM", "truecolor");
            env::set_var("LINES", rows.to_string());
            env::set_var("COLUMNS", cols.to_string());
            env::set_var("PS1", "$ ");
            env::set_var("ENV", "/dev/null");
            env::set_var("BASH_ENV", "/dev/null");
        }
        let shell_c = CString::new("/bin/bash").unwrap();
        let arg1 = CString::new("--norc").unwrap();
        let arg2 = CString::new("--noprofile").unwrap();
        unsafe {
            libc::execlp(
                shell_c.as_ptr(),
                shell_c.as_ptr(),
                arg1.as_ptr(),
                arg2.as_ptr(),
                std::ptr::null::<libc::c_char>(),
            );
            libc::_exit(127);
        }
    }
    unsafe {
        libc::close(slave);
        let flags = libc::fcntl(master, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    Ok((master, pid))
}

fn shell_quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn file_kind_for(path: &std::path::Path) -> FileKind {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "md" | "rs" | "toml" | "json" | "yaml" | "yml" | "log" | "conf" | "ini" | "csv"
        | "html" | "css" | "js" | "ts" | "sh" => FileKind::Text,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => FileKind::Image,
        "mp3" | "flac" | "ogg" | "wav" | "m4a" | "aac" => FileKind::Audio,
        "mp4" | "mkv" | "webm" | "mov" | "avi" => FileKind::Video,
        _ => FileKind::Other,
    }
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn place_entries() -> Vec<PlaceEntry> {
    let home = home_dir();
    let mut places = vec![
        PlaceEntry {
            name: "Home".to_string(),
            path: home.clone(),
        },
        PlaceEntry {
            name: "Downloads".to_string(),
            path: home.join("Downloads"),
        },
        PlaceEntry {
            name: "Documents".to_string(),
            path: home.join("Documents"),
        },
        PlaceEntry {
            name: "Trash".to_string(),
            path: home.join(".local/share/Trash/files"),
        },
        PlaceEntry {
            name: "Root /".to_string(),
            path: PathBuf::from("/"),
        },
    ];
    if let Ok(entries) = fs::read_dir("/mnt") {
        for entry in entries.flatten().take(4) {
            let path = entry.path();
            if path.is_dir() {
                places.push(PlaceEntry {
                    name: format!("/mnt/{}", entry.file_name().to_string_lossy()),
                    path,
                });
            }
        }
    }
    if let Ok(entries) = fs::read_dir("/media") {
        for entry in entries.flatten().take(4) {
            let path = entry.path();
            if path.is_dir() {
                places.push(PlaceEntry {
                    name: format!("/media/{}", entry.file_name().to_string_lossy()),
                    path,
                });
            }
        }
    }
    places
}

fn compact_path(path: &std::path::Path, max_chars: usize) -> String {
    let text = path.to_string_lossy();
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let tail = text
            .chars()
            .rev()
            .take(max_chars.saturating_sub(3))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        format!("...{tail}")
    }
}

fn draw_reload_menu_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_arc(c, cx, cy, 6, 40.0, 320.0, 0, 2, color);
    let tip_x = cx + 4;
    let tip_y = cy + 4;
    draw_round_line(c, tip_x, tip_y, tip_x - 4, tip_y, 2, color);
    draw_round_line(c, tip_x, tip_y, tip_x, tip_y - 4, 2, color);
}

fn draw_info_menu_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_arc(c, cx, cy, 7, 0.0, 359.9, 0, 2, color);
    c.draw_circle(cx, cy - 3, 1, color);
    draw_round_line(c, cx, cy - 1, cx, cy + 3, 2, color);
}
