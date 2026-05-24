use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::CString;
use std::env;
use std::fs;
use std::io::Read;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use image::imageops::FilterType;
use rusttype::{Font, Scale, point};
use time::OffsetDateTime;
use x11rb::CURRENT_TIME;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::ReplyError;
use x11rb::image::{BitsPerPixel, Image, ImageOrder, ScanlinePad};
use x11rb::protocol::composite::{self, ConnectionExt as CompositeConnectionExt};
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
const FRAME_CORNER_RADIUS: i32 = 8;
const TERMINAL_HISTORY_LIMIT: usize = 1000;
const SETTINGS_MIN_WIDTH: u16 = 420;
const SETTINGS_TARGET_WIDTH: u16 = 600;
const SETTINGS_MARGIN: u16 = 24;
const SIDEBAR_WIDTH: i32 = 58;
const SETTINGS_SIDEBAR_TOP: i32 = 26;
const MEDIA_SLOT_COUNT: usize = 5;
const MEDIA_WIDTH: u16 = 600;
const RESIZE_EDGE: i16 = 10;
const FOLDER_HEADER_ICON: i32 = 30;
const FOLDER_TERMINAL_COLS: usize = 58;
const FOLDER_TERMINAL_ROWS: usize = 10;
const FOLDER_TERMINAL_CELL_W: i32 = 8;
const FOLDER_TERMINAL_CELL_H: i32 = 18;
const TERMINAL_FALLBACKS: [&str; 5] = [
    "xfce4-terminal",
    "lxterminal",
    "gnome-terminal",
    "konsole",
    "xterm",
];
const FONT_REGULAR: &[u8] = include_bytes!("/usr/share/fonts/noto/NotoSans-Regular.ttf");
const FONT_BOLD: &[u8] = include_bytes!("/usr/share/fonts/noto/NotoSans-Bold.ttf");
const FONT_MONO: &[u8] = include_bytes!("/usr/share/fonts/noto/NotoSansMono-Regular.ttf");

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
const DEFAULT_WORKSPACE_COUNT: usize = 2;
const MAX_WORKSPACE_COUNT: usize = 8;
const WORKSPACE_STRIDE: i32 = 27;
const WORKSPACE_SIZE: i32 = 18;

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

    fn draw_line(
        &mut self,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        thickness: i32,
        color: Color,
    ) {
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

#[derive(Clone)]
struct DisplayMode {
    width: u16,
    height: u16,
    refresh: Option<f32>,
}

impl DisplayMode {
    fn label(&self) -> String {
        match self.refresh {
            Some(rate) => format!("{}x{}  {:.0} Hz", self.width, self.height, rate),
            None => format!("{}x{}", self.width, self.height),
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
}

struct SettingsState {
    tab: SettingsTab,
    sleep_after_secs: u32,
    power_mode: PowerMode,
    selected_mode: usize,
    scroll: i32,
    app_kind: DefaultAppKind,
    terminal_command: String,
    browser_command: String,
    photo_command: String,
    video_command: String,
    terminal_editing: bool,
    app_status: Option<String>,
}

impl Default for SettingsState {
    fn default() -> Self {
        Self {
            tab: SettingsTab::Display,
            sleep_after_secs: 600,
            power_mode: PowerMode::Balanced,
            selected_mode: 0,
            scroll: 0,
            app_kind: DefaultAppKind::Terminal,
            terminal_command: read_app_command(DefaultAppKind::Terminal),
            browser_command: read_app_command(DefaultAppKind::Browser),
            photo_command: read_app_command(DefaultAppKind::Photo),
            video_command: read_app_command(DefaultAppKind::Video),
            terminal_editing: false,
            app_status: None,
        }
    }
}

#[derive(Default, Clone)]
struct Metrics {
    cpu_model: String,
    cpu_usage: f32,
    cpu_status: String,
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
    app_menu: Window,
    media: [Window; MEDIA_SLOT_COUNT],
}

#[derive(Clone, Copy)]
struct TopbarControls {
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

#[derive(Clone)]
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
    cursor_x: usize,
    cursor_y: usize,
    esc: String,
    mouse_enabled: bool,
    dirty: bool,
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
            screen: vec![vec![' '; FOLDER_TERMINAL_COLS]; FOLDER_TERMINAL_ROWS],
            cursor_x: 0,
            cursor_y: 0,
            esc: String::new(),
            mouse_enabled: false,
            dirty: true,
        }
    }
}

#[derive(Clone)]
struct MediaState {
    entry: FolderEntry,
    playing: bool,
    progress: f32,
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
    wallpaper_previews: Vec<Vec<u8>>,
    wallpaper_pixmap: Option<Pixmap>,
    shape_supported: bool,
    ui: UiWindows,
    regular: Font<'static>,
    bold: Font<'static>,
    mono: Font<'static>,
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
    active_client: Option<Window>,
    drag: Option<DragState>,
    pending_resize: Option<PendingResize>,
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
    folder_terminal: FolderTerminal,
    media: Option<MediaState>,
    media_slots: Vec<Option<MediaState>>,
    media_next_slot: usize,
    media_front: bool,
    media_front_slot: Option<usize>,
    app_menu_visible: bool,
    app_menu_more: bool,
    app_menu_scroll: usize,
    folder_context_open: bool,
    folder_context_pos: (i32, i32),
    folder_clipboard: Option<(PathBuf, bool)>,
    folder_info: Option<String>,
    folder_drag: Option<PathBuf>,
    xdnd_source: Option<Window>,
    dock_last_click: Option<DockClickState>,
    icon_cache: HashMap<String, Option<Vec<u8>>>,
    last_clock_label: String,
    last_tick: Instant,
    last_media_tick: Instant,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("aurora-wm: {err}");
        std::process::exit(1);
    }
}

fn run() -> AnyResult<()> {
    let display = env::var("DISPLAY").unwrap_or_else(|_| ":111".to_string());
    let (conn, screen_num) = RustConnection::connect(Some(&display))?;
    let screen = conn.setup().roots[screen_num].clone();
    become_wm(&conn, &screen)?;

    let mut app = Aurora::new(conn, display, &screen)?;
    app.scan_existing_windows()?;
    app.redraw_everything()?;
    app.run_loop()
}

fn become_wm(conn: &RustConnection, screen: &Screen) -> Result<(), ReplyError> {
    let mask = EventMask::SUBSTRUCTURE_REDIRECT
        | EventMask::SUBSTRUCTURE_NOTIFY
        | EventMask::STRUCTURE_NOTIFY
        | EventMask::PROPERTY_CHANGE
        | EventMask::BUTTON_PRESS;
    let res = conn
        .change_window_attributes(
            screen.root,
            &ChangeWindowAttributesAux::new().event_mask(mask),
        )?
        .check();
    if let Err(ReplyError::X11Error(ref err)) = res {
        if err.error_kind == ErrorKind::Access {
            eprintln!("Another window manager already owns this X display.");
        }
    }
    res
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

impl Aurora {
    fn new(conn: RustConnection, display: String, screen: &Screen) -> AnyResult<Self> {
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
        let mono = Font::try_from_bytes(FONT_MONO).ok_or("failed to load mono font")?;
        let wallpaper_pixels = render_wallpaper_pixels(
            WALLPAPERS[0].bytes,
            screen.width_in_pixels,
            screen.height_in_pixels,
        )?;
        let mut wallpaper_cache = vec![None; WALLPAPERS.len()];
        wallpaper_cache[0] = Some(wallpaper_pixels.clone());
        let wallpaper_previews = WALLPAPERS
            .iter()
            .map(|asset| render_asset_preview_pixels(asset.bytes, 92, 56).unwrap_or_default())
            .collect();
        init_light_compositor(&conn, screen.root);
        let shape_supported = conn
            .extension_information(shape::X11_EXTENSION_NAME)?
            .is_some();
        let ui = UiWindows {
            topbar: conn.generate_id()?,
            dock: conn.generate_id()?,
            settings: conn.generate_id()?,
            folder: conn.generate_id()?,
            folder_terminal: conn.generate_id()?,
            app_menu: conn.generate_id()?,
            media: [
                conn.generate_id()?,
                conn.generate_id()?,
                conn.generate_id()?,
                conn.generate_id()?,
                conn.generate_id()?,
            ],
        };
        let mut sampler = SystemSampler::new();
        let metrics = sampler.sample();
        let (terminal_apps, browser_apps, photo_apps, video_apps) = discover_installed_apps();
        let display_modes =
            read_display_modes(&display, screen.width_in_pixels, screen.height_in_pixels);
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
            shape_supported,
            ui,
            regular,
            bold,
            mono,
            settings: SettingsState::default(),
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
            active_client: None,
            drag: None,
            pending_resize: None,
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
            folder_terminal: FolderTerminal::new(folder_path_for(FolderMode::Home)),
            media: None,
            media_slots: vec![None; MEDIA_SLOT_COUNT],
            media_next_slot: 0,
            media_front: false,
            media_front_slot: None,
            app_menu_visible: false,
            app_menu_more: false,
            app_menu_scroll: 0,
            folder_context_open: false,
            folder_context_pos: (0, 0),
            folder_clipboard: None,
            folder_info: None,
            folder_drag: None,
            xdnd_source: None,
            dock_last_click: None,
            icon_cache: HashMap::new(),
            last_clock_label: format_clock(),
            last_tick: Instant::now(),
            last_media_tick: Instant::now(),
        };
        app.create_ui_windows()?;
        Ok(app)
    }

    fn run_loop(&mut self) -> AnyResult<()> {
        loop {
            let mut handled_event = false;
            let mut pending_motion = None;
            while let Some(event) = self.conn.poll_for_event()? {
                if let Event::MotionNotify(ev) = event {
                    if self.drag.is_some() {
                        handled_event = true;
                        pending_motion = Some(ev);
                    } else {
                        handled_event = true;
                        self.handle_motion_notify(ev)?;
                    }
                } else {
                    handled_event = true;
                    if let Some(ev) = pending_motion.take() {
                        self.handle_motion_notify(ev)?;
                    }
                    self.handle_event(event)?;
                }
            }
            if let Some(ev) = pending_motion.take() {
                self.handle_motion_notify(ev)?;
            }

            if self.folder_terminal.visible && self.poll_folder_terminal()? {
                handled_event = true;
            }

            if let Some(pending) = self.pending_resize {
                if pending.pressed_at.elapsed() >= Duration::from_secs(2) {
                    self.pending_resize = None;
                    self.start_resize(pending.client, pending.root_x, pending.root_y, pending.edges)?;
                }
            }

            if handled_event {
                self.conn.flush()?;
            }

            if self.last_media_tick.elapsed() >= Duration::from_millis(250) {
                self.last_media_tick = Instant::now();
                if self.advance_internal_media()? {
                    self.conn.flush()?;
                }
            }

            if self.last_tick.elapsed() >= Duration::from_millis(2000) {
                self.last_tick = Instant::now();
                self.metrics = self.sampler.sample();
                let clock_label = format_clock();
                if clock_label != self.last_clock_label {
                    self.last_clock_label = clock_label;
                    self.redraw_topbar()?;
                }
                if self.settings_visible
                    && matches!(self.settings.tab, SettingsTab::Power | SettingsTab::About)
                {
                    self.redraw_settings()?;
                }
                self.conn.flush()?;
            }

            thread::sleep(if handled_event {
                if self.drag.is_some() {
                    Duration::from_millis(1)
                } else {
                    Duration::from_millis(3)
                }
            } else {
                Duration::from_millis(8)
            });
        }
    }

    fn create_ui_windows(&mut self) -> AnyResult<()> {
        let top_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS)
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
            .background_pixel(0);
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
            .background_pixel(0);
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
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::KEY_PRESS)
            .cursor(self.cursor)
            .background_pixel(0);
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

        let media_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS)
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
            Event::ButtonPress(ev) => self.handle_button_press(ev)?,
            Event::ButtonRelease(ev) => {
                if ev.event == self.ui.folder {
                    self.handle_folder_release(ev)?;
                } else {
                    self.end_drag()?;
                }
            }
            Event::MotionNotify(ev) => self.handle_motion_notify(ev)?,
            Event::LeaveNotify(ev) => self.handle_leave_notify(ev)?,
            Event::EnterNotify(ev) => self.handle_enter_notify(ev)?,
            Event::ClientMessage(ev) => self.handle_client_message(ev)?,
            Event::SelectionRequest(ev) => self.handle_selection_request(ev)?,
            Event::SelectionNotify(ev) => self.handle_selection_notify(ev)?,
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
            Event::PropertyNotify(ev) => {
                if self.clients.contains_key(&ev.window) {
                    self.update_client_chrome(ev.window)?;
                    self.redraw_dock()?;
                }
            }
            Event::ConfigureNotify(ev) => {
                if ev.window == self.root {
                    self.resize_to_root()?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_expose(&mut self, ev: ExposeEvent) -> AnyResult<()> {
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
        } else if ev.window == self.ui.app_menu && self.app_menu_visible {
            self.redraw_app_menu()?;
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
        if ev.event == self.ui.settings {
            if ev.detail == 4 || ev.detail == 5 {
                self.handle_settings_scroll(ev.detail, i32::from(ev.event_x))?;
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
            if ev.detail == 3 {
                self.handle_folder_context(i32::from(ev.event_x), i32::from(ev.event_y))?;
            } else {
                self.handle_folder_click(i32::from(ev.event_x), i32::from(ev.event_y))?;
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
            self.conn
                .set_input_focus(InputFocus::POINTER_ROOT, self.ui.folder_terminal, CURRENT_TIME)?;
            self.raise_ui()?;
            self.handle_folder_terminal_click(i32::from(ev.event_x), i32::from(ev.event_y))?;
            self.redraw_folder_terminal()?;
        } else if ev.event == self.ui.app_menu {
            self.handle_app_menu_click(ev.detail, i32::from(ev.event_x), i32::from(ev.event_y))?;
        } else if let Some(slot) = self.media_slot_for_window(ev.event) {
            self.media_front = true;
            self.media_front_slot = Some(slot);
            self.settings_front = false;
            self.folder_front = false;
            self.handle_media_click(slot, i32::from(ev.event_x), i32::from(ev.event_y))?;
        } else if ev.event == self.ui.topbar {
            let x = i32::from(ev.event_x);
            let controls = self.topbar_controls();
            let workspace = (0..self.workspace_count).find(|&index| {
                (self.workspace_x(index)..=self.workspace_x(index) + WORKSPACE_SIZE).contains(&x)
            });
            if let Some(workspace) = workspace {
                self.switch_workspace(workspace)?;
            } else if (self.add_workspace_x()..=self.add_workspace_x() + WORKSPACE_SIZE)
                .contains(&x)
            {
                self.add_workspace()?;
            } else if (controls.display_x - 15..=controls.display_x + 15).contains(&x) {
                self.open_settings_tab(SettingsTab::Display)?;
            } else if (controls.audio_x - 15..=controls.audio_x + 15).contains(&x) {
                self.open_settings_tab(SettingsTab::Audio)?;
            } else if (controls.network_x - 15..=controls.network_x + 15).contains(&x) {
                self.open_settings_tab(SettingsTab::Network)?;
            } else if (controls.battery_left..=controls.battery_right).contains(&x) {
                self.open_settings_tab(SettingsTab::Power)?;
            }
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

    fn handle_motion_notify(&mut self, ev: MotionNotifyEvent) -> AnyResult<()> {
        let Some(drag) = self.drag else {
            if let Some(ref mut pending) = self.pending_resize {
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
                }
            }
            return Ok(());
        };
        let Some(mut info) = self.clients.get(&drag.client).copied() else {
            self.drag = None;
            return Ok(());
        };
        match drag.kind {
            DragKind::Move => {
                info.x = ev.root_x.saturating_sub(drag.offset_x);
                info.y = ev.root_y.saturating_sub(drag.offset_y);
                if self
                    .clients
                    .get(&drag.client)
                    .is_some_and(|old| old.x == info.x && old.y == info.y)
                {
                    return Ok(());
                }
                self.conn.configure_window(
                    info.frame,
                    &ConfigureWindowAux::new()
                        .x(i32::from(info.x))
                        .y(i32::from(info.y)),
                )?;
            }
            DragKind::Resize => {
                let dx = i32::from(ev.root_x) - i32::from(drag.start_root_x);
                let dy = i32::from(ev.root_y) - i32::from(drag.start_root_y);
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
        self.clients.insert(drag.client, info);
        Ok(())
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
        let Some(client) = self.client_key_for(ev.window) else {
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
        if let Some(edges) =
            resize_edges_for_frame(&info, self.titlebar_height(&info), ev.event_x, ev.event_y)
        {
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
        let client_y = ev
            .root_y
            .saturating_sub(info.y)
            .saturating_sub(title_h);
        if let Some(edges) = resize_edges_for_client(&info, client_x, client_y) {
            self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
            self.pending_resize = Some(PendingResize {
                client,
                root_x: ev.root_x,
                root_y: ev.root_y,
                edges,
                pressed_at: Instant::now(),
            });
        } else {
            self.focus_window_at(client, ev.time)?;
            self.conn.flush()?;
            self.conn.allow_events(Allow::REPLAY_POINTER, ev.time)?;
        }
        Ok(())
    }

    fn grab_client_buttons(&self, window: Window) -> AnyResult<()> {
        let lock = u16::from(ModMask::LOCK);
        let num_lock = u16::from(ModMask::M2);
        let alt = u16::from(ModMask::M1);
        let modifiers = [
            0,
            lock,
            num_lock,
            lock | num_lock,
            alt,
            alt | lock,
            alt | num_lock,
            alt | lock | num_lock,
        ];
        let buttons = [
            ButtonIndex::M1,
            ButtonIndex::M2,
            ButtonIndex::M3,
            ButtonIndex::M4,
            ButtonIndex::M5,
        ];

        for modifier in modifiers {
            for button in buttons {
                let res = self
                    .conn
                    .grab_button(
                        false,
                        window,
                        EventMask::BUTTON_PRESS,
                        GrabMode::SYNC,
                        GrabMode::ASYNC,
                        x11rb::NONE,
                        x11rb::NONE,
                        button,
                        ModMask::from(modifier),
                    )?
                    .check();
                if let Err(ReplyError::X11Error(ref err)) = res {
                    if err.error_kind == ErrorKind::Access {
                        continue;
                    }
                }
                res?;
            }
        }
        Ok(())
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
        });
        self.settings_front = false;
        self.folder_front = false;
        self.media_front = false;
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

    fn end_drag(&mut self) -> AnyResult<()> {
        self.pending_resize = None;
        if self.drag.take().is_some() {
            self.conn.ungrab_pointer(CURRENT_TIME)?;
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
        let titlebar =
            !client_uses_own_chrome(&self.window_class(window), &self.window_title(window));
        let title_h = if titlebar { TITLEBAR_HEIGHT } else { 0 };
        let max_w = self.screen_width.saturating_sub(80).max(300);
        let max_h = self
            .screen_height
            .saturating_sub(TOPBAR_HEIGHT + DOCK_HEIGHT + title_h + 62)
            .max(240);
        let width = geom.width.min(max_w);
        let height = geom.height.min(max_h);
        let x = if geom.x <= 0 { 42 } else { geom.x.max(16) };
        let y = if geom.y <= 0 {
            i16::try_from(TOPBAR_HEIGHT + 26).unwrap()
        } else {
            geom.y.max(i16::try_from(TOPBAR_HEIGHT + 8).unwrap())
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
            .background_pixel(0);
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
            &ChangeWindowAttributesAux::new()
                .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
        )?;
        self.grab_client_buttons(window)?;
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
        self.conn.reparent_window(window, frame, 0, title_h as i16)?;
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
        if self.active_client == Some(client) {
            self.active_client = None;
        }
        self.redraw_dock()?;
        Ok(())
    }

    fn minimize_client(&mut self, client: Window) -> AnyResult<()> {
        if let Some(info) = self.clients.get_mut(&client) {
            info.mapped = false;
            self.ignored_unmaps.push(info.frame);
            self.conn.unmap_window(info.frame)?;
            if self.active_client == Some(client) {
                self.active_client = None;
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
        self.redraw_frame_titlebar(client)?;
        self.focus_window(client)?;
        Ok(())
    }

    fn focus_window(&mut self, window: Window) -> AnyResult<()> {
        self.focus_window_at(window, CURRENT_TIME)
    }

    fn focus_window_at(&mut self, window: Window, time: Timestamp) -> AnyResult<()> {
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
        if self.settings_visible {
            self.conn.configure_window(
                self.ui.settings,
                &ConfigureWindowAux::new()
                    .sibling(info.frame)
                    .stack_mode(StackMode::BELOW),
            )?;
        }
        self.conn.configure_window(
            self.ui.folder,
            &ConfigureWindowAux::new()
                .sibling(info.frame)
                .stack_mode(StackMode::BELOW),
        )?;
        for (idx, media_window) in self.ui.media.iter().copied().enumerate() {
            if self.media_slots.get(idx).and_then(|m| m.as_ref()).is_none() {
                continue;
            }
            self.conn.configure_window(
                media_window,
                &ConfigureWindowAux::new()
                    .sibling(info.frame)
                    .stack_mode(StackMode::BELOW),
            )?;
        }
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
        self.conn.configure_window(
            self.ui.dock,
            &ConfigureWindowAux::new()
                .sibling(info.frame)
                .stack_mode(StackMode::BELOW),
        )?;
        self.raise_chrome()?;
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
            let event =
                ClientMessageEvent::new(32, info.window, wm_protocols, [wm_take_focus, time, 0, 0, 0]);
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
        let mut c = Canvas::from_wallpaper_crop(
            &self.wallpaper_pixels,
            self.screen_width,
            i32::from(info.x),
            i32::from(info.y),
            info.width,
            TITLEBAR_HEIGHT,
        );
        if self.titlebar_height(info) == 0 {
            return Ok(());
        }
        c.draw_round_rect(
            0,
            0,
            i32::from(info.width),
            i32::from(TITLEBAR_HEIGHT) + 16,
            FRAME_CORNER_RADIUS,
            Color::rgba(250, 254, 255, 225),
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

    fn topbar_controls(&self) -> TopbarControls {
        let battery = self.metrics.battery.as_deref().unwrap_or("100%");
        let battery_right = i32::from(self.screen_width) - 16;
        let battery_left = battery_right - measure_text(&self.bold, battery, 19.0) - 20;
        let network_x = battery_left - 18;
        let audio_x = network_x - TOPBAR_ICON_SPACING;
        let display_x = audio_x - TOPBAR_ICON_SPACING;
        TopbarControls {
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

        let clock = format_clock();
        c.draw_text_center(
            &self.regular,
            &clock,
            i32::from(self.screen_width) / 2,
            10,
            16.0,
            Color::rgb(239, 252, 250),
        );

        let controls = self.topbar_controls();
        draw_sidebar_display_icon(&mut c, controls.display_x, 20, MINT_LIGHT);
        draw_sidebar_audio_icon(&mut c, controls.audio_x, 20, MINT_LIGHT);
        draw_sidebar_network_icon(&mut c, controls.network_x, 20, MINT_LIGHT);
        let battery = self.metrics.battery.as_deref().unwrap_or("100%");
        c.draw_text(
            &self.bold,
            battery,
            controls.battery_left + 10,
            9,
            19.0,
            Color::rgb(239, 252, 250),
        );
        self.upload_canvas(self.ui.topbar, &c)
    }

    fn redraw_dock(&mut self) -> AnyResult<()> {
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
        let task_windows = self.task_client_windows();
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
            c.draw_round_rect(icon_x, icon_y, 44, 44, 12, Color::rgba(255, 255, 255, 215));
            c.draw_round_rect(
                icon_x + 1,
                icon_y + 1,
                42,
                42,
                11,
                Color::rgba(196, 219, 229, 95),
            );
            if i < 5 {
                draw_dock_icon(&mut c, i, icon_x + 22, icon_y + 22);
            } else if let Some(client) = task_windows
                .get(i - 5)
                .and_then(|window| self.clients.get(window))
                .copied()
            {
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

    fn redraw_settings(&self) -> AnyResult<()> {
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
        c.draw_round_rect(56, 18, FOLDER_HEADER_ICON, FOLDER_HEADER_ICON, 10, Color::rgba(255, 255, 255, 155));
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
        c.draw_round_rect(94, 18, FOLDER_HEADER_ICON, FOLDER_HEADER_ICON, 10, Color::rgba(255, 255, 255, 155));
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
            for (idx, entry) in self
                .folder_entries
                .iter()
                .skip(self.folder_scroll)
                .take(9)
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
            c.draw_round_rect(
                menu_x,
                menu_y,
                122,
                96,
                12,
                Color::rgba(250, 254, 255, 242),
            );
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
            c.draw_round_rect(
                16,
                i32::from(h) - 64,
                i32::from(w) - 32,
                44,
                10,
                Color::rgba(250, 254, 255, 230),
            );
            c.draw_text(
                &self.regular,
                &compact(info, 46),
                28,
                i32::from(h) - 49,
                12.0,
                INK,
            );
        }
        if self.folder_entries.len() > 9 {
            let track_h = i32::from(h) - 100;
            let track_x = i32::from(w) - 13;
            c.draw_round_rect(track_x, 84, 5, track_h, 3, Color::rgba(176, 198, 210, 90));
            let thumb_h = ((track_h as f32 * 9.0 / self.folder_entries.len() as f32) as i32)
                .max(34)
                .min(track_h);
            let max_scroll = self.folder_entries.len().saturating_sub(9).max(1);
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
        self.upload_canvas(self.ui.folder, &c)
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

    fn redraw_media_slot(&self, slot: usize) -> AnyResult<()> {
        let Some(media) = self.media_slots.get(slot).and_then(|m| m.as_ref()) else {
            return Ok(());
        };
        let (x, y, w, h) = self.media_geometry(slot);
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
        c.draw_rect(
            20,
            68,
            i32::from(w) - 40,
            1,
            Color::rgba(178, 202, 214, 100),
        );

        let preview_x = 24;
        let preview_y = 88;
        let preview_w = i32::from(w) - 48;
        let preview_h = 174;
        c.draw_round_rect(
            preview_x,
            preview_y,
            preview_w,
            preview_h,
            15,
            Color::rgba(23, 34, 42, 24),
        );
        match media.entry.kind {
            FileKind::Text => {
                draw_text_preview(
                    &mut c,
                    &self.regular,
                    &media.entry.path,
                    preview_x + 14,
                    preview_y + 14,
                    preview_w - 28,
                    preview_h - 28,
                );
            }
            FileKind::Image => {
                paint_file_preview(
                    &mut c,
                    &media.entry.path,
                    preview_x + 8,
                    preview_y + 8,
                    preview_w - 16,
                    preview_h - 16,
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
                draw_music_icon(&mut c, i32::from(w) / 2, preview_y + 78, MINT_DARK);
                draw_sparkline(
                    &mut c,
                    preview_x + 44,
                    preview_y + 122,
                    preview_w - 88,
                    28,
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
                    preview_y + 20,
                    14.0,
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
                    preview_y + 16,
                    preview_w - 36,
                    preview_h - 54,
                    10,
                    frame_color,
                );
                draw_play_icon(&mut c, i32::from(w) / 2, preview_y + 72, BLUE);
                c.draw_text_center(
                    &self.bold,
                    if media.playing {
                        "Playing video"
                    } else {
                        "Video ready"
                    },
                    i32::from(w) / 2,
                    preview_y + 20,
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
                draw_file_kind_icon(&mut c, media.entry.kind, i32::from(w) / 2, preview_y + 78);
                c.draw_text_center(
                    &self.regular,
                    "No embedded preview",
                    i32::from(w) / 2,
                    preview_y + 118,
                    12.0,
                    MUTED,
                );
            }
        }

        c.draw_text(
            &self.regular,
            &compact_path(&media.entry.path, 48),
            24,
            278,
            11.0,
            MUTED,
        );
        if matches!(media.entry.kind, FileKind::Audio | FileKind::Video) {
            c.draw_round_rect(
                24,
                304,
                i32::from(w) - 48,
                42,
                13,
                Color::rgba(116, 213, 198, 88),
            );
            if media.playing {
                c.draw_rect(44, 316, 5, 18, MINT_DARK);
                c.draw_rect(54, 316, 5, 18, MINT_DARK);
                c.draw_text(&self.bold, "Pause in Aurora", 80, 315, 13.0, INK);
            } else {
                draw_play_icon(&mut c, 50, 325, MINT_DARK);
                c.draw_text(&self.bold, "Play in Aurora", 80, 315, 13.0, INK);
            }
            let bar_x = 250;
            let bar_w = i32::from(w) - bar_x - 48;
            c.draw_round_rect(bar_x, 321, bar_w, 8, 4, Color::rgba(255, 255, 255, 140));
            c.draw_round_rect(
                bar_x,
                321,
                (bar_w as f32 * media.progress.clamp(0.0, 1.0)) as i32,
                8,
                4,
                Color::rgba(29, 145, 137, 190),
            );
        } else {
            c.draw_text(
                &self.regular,
                "Preview rendered inside Aurora.",
                24,
                316,
                12.0,
                MUTED,
            );
        }
        self.upload_canvas(self.ui.media[slot], &c)
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

        draw_card(c, sx, 360, i32::from(c.width) - sx - 24, 94);
        c.draw_text(&self.bold, "Sleep after", sx + 16, 379, 15.0, INK);
        c.draw_round_rect(sx + 18, 409, 28, 28, 9, Color::rgba(234, 244, 248, 220));
        c.draw_text_center(&self.bold, "-", sx + 32, 411, 18.0, MINT_DARK);
        c.draw_text_center(
            &self.bold,
            &format!("{} s", self.settings.sleep_after_secs),
            sx + 112,
            413,
            15.0,
            INK,
        );
        c.draw_round_rect(sx + 178, 409, 28, 28, 9, Color::rgba(234, 244, 248, 220));
        c.draw_text_center(&self.bold, "+", sx + 192, 411, 18.0, MINT_DARK);
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
        draw_card(c, sx, 86, i32::from(c.width) - sx - 24, 126);
        c.draw_text(&self.bold, "Power profile", sx + 16, 106, 15.0, INK);
        let modes = [
            PowerMode::Saver,
            PowerMode::Balanced,
            PowerMode::Performance,
        ];
        for (idx, mode) in modes.iter().enumerate() {
            let y = 134 + idx as i32 * 24;
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

        draw_card(c, sx, 230, i32::from(c.width) - sx - 24, 156);
        c.draw_text(&self.bold, "System", sx + 16, 250, 15.0, INK);
        draw_metric_bar(
            c,
            &self.regular,
            sx + 16,
            280,
            "CPU",
            self.metrics.cpu_usage,
            "%",
        );
        let ram_pct = percent(self.metrics.ram_used_kb, self.metrics.ram_total_kb);
        draw_metric_bar(c, &self.regular, sx + 16, 318, "Memory", ram_pct, "%");
        let swap_pct = percent(self.metrics.swap_used_kb, self.metrics.swap_total_kb);
        draw_metric_bar(c, &self.regular, sx + 16, 356, "Swap", swap_pct, "%");

        draw_card(c, sx, 404, i32::from(c.width) - sx - 24, 76);
        c.draw_text(&self.bold, "Battery", sx + 16, 424, 15.0, INK);
        c.draw_text(
            &self.regular,
            self.metrics
                .battery
                .as_deref()
                .unwrap_or("No battery exposed"),
            sx + 16,
            452,
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
            if let Some(preview) = self.wallpaper_previews.get(idx) {
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

    fn draw_audio_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        c.draw_text(&self.bold, "Audio", sx, 22, 24.0, INK);
        draw_card(c, sx, 86, i32::from(c.width) - sx - 24, 112);
        c.draw_text(&self.bold, "Volume", sx + 16, 106, 15.0, INK);
        c.draw_round_rect(sx + 16, 142, 230, 10, 5, Color::rgba(211, 225, 232, 170));
        c.draw_round_rect(sx + 16, 142, 138, 10, 5, Color::rgba(116, 213, 198, 210));
        c.draw_text(&self.regular, "60%", sx + 262, 136, 15.0, INK);
        draw_card(c, sx, 220, i32::from(c.width) - sx - 24, 150);
        c.draw_text(&self.bold, "Output device", sx + 16, 240, 15.0, INK);
        for (idx, dev) in read_audio_devices("Sink").iter().take(3).enumerate() {
            c.draw_text(
                &self.regular,
                &compact(dev, 48),
                sx + 16,
                272 + idx as i32 * 28,
                12.0,
                INK,
            );
        }
        draw_card(c, sx, 392, i32::from(c.width) - sx - 24, 108);
        c.draw_text(&self.bold, "Input device", sx + 16, 412, 15.0, INK);
        for (idx, dev) in read_audio_devices("Source").iter().take(2).enumerate() {
            c.draw_text(
                &self.regular,
                &compact(dev, 48),
                sx + 16,
                444 + idx as i32 * 28,
                12.0,
                INK,
            );
        }
    }

    fn draw_network_tab(&self, c: &mut Canvas) {
        let sx = SIDEBAR_WIDTH + 24;
        c.draw_text(&self.bold, "Network", sx, 22, 24.0, INK);
        c.draw_text(
            &self.regular,
            "Wired and Wi-Fi interfaces.",
            sx,
            54,
            13.0,
            MUTED,
        );
        draw_card(c, sx, 86, i32::from(c.width) - sx - 24, 394);
        let start = (self.settings.scroll / 29).max(0) as usize;
        for (idx, line) in read_network_details()
            .iter()
            .skip(start)
            .take(12)
            .enumerate()
        {
            c.draw_text(
                &self.regular,
                &compact(line, 62),
                sx + 16,
                112 + idx as i32 * 29,
                13.0,
                if idx % 3 == 0 { INK } else { MUTED },
            );
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
        c.draw_text(
            &self.bold,
            &format!("Installed {} apps", self.settings.app_kind.label()),
            sx + 16,
            151,
            14.0,
            INK,
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
                "Scroll to see more installed apps",
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
        draw_card(c, sx, 86, i32::from(c.width) - sx - 24, 220);
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

        draw_card(c, sx, 326, i32::from(c.width) - sx - 24, 154);
        c.draw_text(&self.bold, "Network speed", sx + 16, 348, 15.0, INK);
        c.draw_text(&self.regular, "Down", sx + 16, 380, 12.0, MUTED);
        c.draw_text(
            &self.bold,
            &format_bps(self.metrics.net_rx_bps),
            sx + 70,
            377,
            17.0,
            INK,
        );
        c.draw_text(&self.regular, "Up", sx + 16, 422, 12.0, MUTED);
        c.draw_text(
            &self.bold,
            &format_bps(self.metrics.net_tx_bps),
            sx + 70,
            419,
            17.0,
            INK,
        );
        draw_sparkline(
            c,
            sx + 152,
            374,
            i32::from(c.width) - sx - 190,
            22,
            self.metrics.net_rx_bps,
            BLUE,
        );
        draw_sparkline(
            c,
            sx + 152,
            416,
            i32::from(c.width) - sx - 190,
            22,
            self.metrics.net_tx_bps,
            MINT_DARK,
        );
    }

    fn add_workspace(&mut self) -> AnyResult<()> {
        if self.workspace_count >= MAX_WORKSPACE_COUNT {
            return Ok(());
        }
        let workspace = self.workspace_count;
        self.workspace_count += 1;
        
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
        self.dock_last_click = None;
        self.conn
            .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
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
        self.redraw_settings()
    }

    fn handle_settings_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
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
                self.redraw_settings()?;
            }
            return Ok(());
        }

        match self.settings.tab {
            SettingsTab::Display => self.handle_display_click(x, y)?,
            SettingsTab::Power => self.handle_power_click(x, y)?,
            SettingsTab::Wallpaper => self.handle_wallpaper_click(y)?,
            SettingsTab::Bluetooth if y >= 224 && y <= 300 => {
                self.spawn_first_available(&["blueman-manager", "bluetoothctl"], &[]);
            }
            SettingsTab::Apps => self.handle_apps_click(x, y)?,
            SettingsTab::Audio
            | SettingsTab::Network
            | SettingsTab::Bluetooth
            | SettingsTab::Startup => {}
            SettingsTab::About => {}
        }
        Ok(())
    }

    fn handle_settings_scroll(&mut self, button: u8, x: i32) -> AnyResult<()> {
        if x <= SIDEBAR_WIDTH {
            return Ok(());
        }
        let max_scroll = match self.settings.tab {
            SettingsTab::Network | SettingsTab::Startup | SettingsTab::About => 180,
            SettingsTab::Audio | SettingsTab::Wallpaper => 80,
            SettingsTab::Apps => self
                .available_apps(self.settings.app_kind)
                .len()
                .saturating_sub(6)
                .saturating_mul(29) as i32,
            SettingsTab::Display | SettingsTab::Power | SettingsTab::Bluetooth => 40,
        };
        let old_scroll = self.settings.scroll;
        let step = if self.settings.tab == SettingsTab::Apps {
            29
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
        if y >= 407 && y <= 440 {
            if x >= sx + 16 && x <= sx + 50 {
                self.settings.sleep_after_secs =
                    self.settings.sleep_after_secs.saturating_sub(60).max(0);
                self.apply_sleep_timeout();
                self.redraw_settings()?;
            } else if x >= sx + 174 && x <= sx + 212 {
                self.settings.sleep_after_secs = (self.settings.sleep_after_secs + 60).min(7200);
                self.apply_sleep_timeout();
                self.redraw_settings()?;
            }
        }
        Ok(())
    }

    fn handle_power_click(&mut self, _x: i32, y: i32) -> AnyResult<()> {
        let modes = [
            PowerMode::Saver,
            PowerMode::Balanced,
            PowerMode::Performance,
        ];
        for (idx, mode) in modes.iter().enumerate() {
            let row_y = 134 + idx as i32 * 24;
            if y >= row_y - 7 && y <= row_y + 18 {
                self.settings.power_mode = *mode;
                self.apply_power_mode(*mode);
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
        if ev.event == self.ui.folder_terminal && self.folder_terminal.visible {
            self.handle_folder_terminal_key(ev)?;
            return Ok(());
        }
        if ev.event != self.ui.settings
            || self.settings.tab != SettingsTab::Apps
            || self.settings.app_kind != DefaultAppKind::Terminal
            || !self.settings.terminal_editing
        {
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
                    self.toggle_app_menu()?;
                } else if i >= 5 {
                    self.hide_app_menu()?;
                    if let Some(client) = task_windows.get(i - 5).copied() {
                        self.handle_task_icon_click(client)?;
                    }
                } else {
                    self.dock_last_click = None;
                    self.hide_app_menu()?;
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
                        } else {
                            self.conn.unmap_window(self.ui.settings)?;
                        }
                    }
                }
                return Ok(());
            }
            bx += stride;
        }
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
        self.focus_window(client)?;
        Ok(())
    }

    fn toggle_app_menu(&mut self) -> AnyResult<()> {
        self.app_menu_visible = !self.app_menu_visible;
        if self.app_menu_visible {
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

    fn handle_folder_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
        let (_, _, w, _) = self.folder_geometry();
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
        self.folder_info = None;
        if y < 86 {
            self.redraw_folder()?;
            return Ok(());
        }
        let idx = (y - 86) / 42;
        if !(0..9).contains(&idx) {
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
        match entry.kind {
            FileKind::Directory => {
                self.folder_path = entry.path.clone();
                self.folder_entries = folder_entries_in(entry.path, self.folder_sort);
                self.folder_selected = None;
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
                    self.open_media(entry)?;
                } else {
                    self.folder_selected = Some(entry.path.clone());
                    self.redraw_folder()?;
                }
            }
        }
        Ok(())
    }

    fn handle_folder_release(&mut self, _ev: ButtonReleaseEvent) -> AnyResult<()> {
        let Some(path) = self.folder_drag.take() else {
            return Ok(());
        };
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        let mut target = pointer.child;
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

    fn handle_folder_context(&mut self, x: i32, y: i32) -> AnyResult<()> {
        if y >= 86 {
            let idx = (y - 86) / 42;
            if (0..9).contains(&idx) {
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
        let max_scroll = self.folder_entries.len().saturating_sub(9);
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

    fn refresh_folder_entries(&mut self) {
        self.folder_entries = folder_entries_in(self.folder_path.clone(), self.folder_sort);
        self.folder_scroll = self
            .folder_scroll
            .min(self.folder_entries.len().saturating_sub(9));
        self.folder_selected = self
            .folder_selected
            .take()
            .filter(|path| self.folder_entries.iter().any(|entry| &entry.path == path));
    }

    fn sync_folder_terminal_cwd(&mut self) {
        self.folder_terminal.cwd = self.folder_path.clone();
        if self.folder_terminal.master_fd.is_some() {
            let command = format!("cd {}\n", shell_quote(&self.folder_path));
            self.write_folder_terminal(command.as_bytes());
        }
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
            self.conn
                .set_input_focus(InputFocus::POINTER_ROOT, self.ui.folder_terminal, CURRENT_TIME)?;
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
        let visible_rows = ((i32::from(h) - 56) / FOLDER_TERMINAL_CELL_H)
            .max(1)
            .min(FOLDER_TERMINAL_ROWS as i32) as usize;
        let rows = self.folder_terminal_display_rows(visible_rows);
        for (idx, row) in rows.iter().enumerate() {
            let y = 52 + idx as i32 * FOLDER_TERMINAL_CELL_H;
            for (col, ch) in row.chars().take(FOLDER_TERMINAL_COLS).enumerate() {
                if ch != ' ' {
                    c.draw_text(
                        &self.mono,
                        &ch.to_string(),
                        18 + col as i32 * FOLDER_TERMINAL_CELL_W,
                        y,
                        13.5,
                        INK,
                    );
                }
            }
        }
        if self.folder_terminal.focused {
            let cursor_x = 18
                + self.folder_terminal.cursor_x.min(FOLDER_TERMINAL_COLS - 1) as i32
                    * FOLDER_TERMINAL_CELL_W;
            let cursor_y = 53
                + self.folder_terminal.cursor_y.min(FOLDER_TERMINAL_ROWS - 1) as i32
                    * FOLDER_TERMINAL_CELL_H;
            c.draw_rect(cursor_x, cursor_y, 2, 14, MINT_DARK);
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
        let mut bytes = match keysym {
            0xff08 => b"\x7f".to_vec(),
            0xff09 => b"\t".to_vec(),
            0xff0d => b"\r".to_vec(),
            0xff1b => b"\x1b".to_vec(),
            0xff51 => b"\x1b[D".to_vec(),
            0xff52 => b"\x1b[A".to_vec(),
            0xff53 => b"\x1b[C".to_vec(),
            0xff54 => b"\x1b[B".to_vec(),
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
        if y < 44 || !self.folder_terminal.mouse_enabled {
            return Ok(());
        }
        let col = ((x - 18).max(0) / FOLDER_TERMINAL_CELL_W + 1)
            .clamp(1, FOLDER_TERMINAL_COLS as i32);
        let row = ((y - 52).max(0) / FOLDER_TERMINAL_CELL_H + 1)
            .clamp(1, FOLDER_TERMINAL_ROWS as i32);
        let press = format!("\x1b[<0;{col};{row}M");
        let release = format!("\x1b[<0;{col};{row}m");
        self.write_folder_terminal(press.as_bytes());
        self.write_folder_terminal(release.as_bytes());
        Ok(())
    }

    fn ensure_folder_terminal_pty(&mut self) {
        if self.folder_terminal.master_fd.is_some() {
            return;
        }
        match spawn_terminal_pty(&self.folder_terminal.cwd, FOLDER_TERMINAL_COLS, FOLDER_TERMINAL_ROWS) {
            Ok((fd, pid)) => {
                self.folder_terminal.master_fd = Some(fd);
                self.folder_terminal.child_pid = Some(pid);
                self.folder_terminal.history.clear();
                self.folder_terminal.scrollback = 0;
                self.folder_terminal.screen = vec![vec![' '; FOLDER_TERMINAL_COLS]; FOLDER_TERMINAL_ROWS];
                self.folder_terminal.cursor_x = 0;
                self.folder_terminal.cursor_y = 0;
                self.folder_terminal.mouse_enabled = false;
                self.folder_terminal.dirty = true;
            }
            Err(err) => {
                self.draw_terminal_message(&format!("terminal error: {err}"));
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
        self.folder_terminal.screen = vec![vec![' '; FOLDER_TERMINAL_COLS]; FOLDER_TERMINAL_ROWS];
        for (idx, ch) in message.chars().take(FOLDER_TERMINAL_COLS).enumerate() {
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
                let next = ((self.folder_terminal.cursor_x / 4) + 1) * 4;
                self.folder_terminal.cursor_x = next.min(FOLDER_TERMINAL_COLS - 1);
            }
            c if !c.is_control() => self.terminal_put_char(c),
            _ => {}
        }
    }

    fn feed_terminal_escape(&mut self, ch: char) {
        if self.folder_terminal.esc.is_empty() {
            self.folder_terminal.esc.push(ch);
            return;
        }
        self.folder_terminal.esc.push(ch);
        if self.folder_terminal.esc.starts_with("\x1b]") {
            if ch == '\x07' || self.folder_terminal.esc.ends_with("\x1b\\") {
                self.folder_terminal.esc.clear();
            }
            return;
        }
        if self.folder_terminal.esc.starts_with("\x1b(")
            || self.folder_terminal.esc.starts_with("\x1b)")
        {
            if self.folder_terminal.esc.len() >= 3 {
                self.folder_terminal.esc.clear();
            }
            return;
        }
        if !ch.is_ascii_alphabetic() && ch != '~' {
            return;
        }
        let esc = std::mem::take(&mut self.folder_terminal.esc);
        if let Some(body) = esc.strip_prefix("\x1b[") {
            self.apply_terminal_csi(body);
        }
    }

    fn apply_terminal_csi(&mut self, body: &str) {
        let command = body.chars().last().unwrap_or('m');
        let params = &body[..body.len().saturating_sub(1)];
        let private = params.starts_with('?');
        let clean = params.trim_start_matches('?');
        let values = clean
            .split(';')
            .filter_map(|part| part.parse::<usize>().ok())
            .collect::<Vec<_>>();
        if private && matches!(command, 'h' | 'l') {
            if values
                .iter()
                .any(|value| matches!(*value, 1000 | 1002 | 1003 | 1006))
            {
                self.folder_terminal.mouse_enabled = command == 'h';
            }
            return;
        }
        match command {
            'H' | 'f' => {
                let row = values.first().copied().unwrap_or(1).saturating_sub(1);
                let col = values.get(1).copied().unwrap_or(1).saturating_sub(1);
                self.folder_terminal.cursor_y = row.min(FOLDER_TERMINAL_ROWS - 1);
                self.folder_terminal.cursor_x = col.min(FOLDER_TERMINAL_COLS - 1);
            }
            'A' => self.folder_terminal.cursor_y = self.folder_terminal.cursor_y.saturating_sub(values.first().copied().unwrap_or(1)),
            'B' => self.folder_terminal.cursor_y = (self.folder_terminal.cursor_y + values.first().copied().unwrap_or(1)).min(FOLDER_TERMINAL_ROWS - 1),
            'C' => self.folder_terminal.cursor_x = (self.folder_terminal.cursor_x + values.first().copied().unwrap_or(1)).min(FOLDER_TERMINAL_COLS - 1),
            'D' => self.folder_terminal.cursor_x = self.folder_terminal.cursor_x.saturating_sub(values.first().copied().unwrap_or(1)),
            'J' => {
                self.folder_terminal.screen = vec![vec![' '; FOLDER_TERMINAL_COLS]; FOLDER_TERMINAL_ROWS];
                self.folder_terminal.cursor_x = 0;
                self.folder_terminal.cursor_y = 0;
            }
            'K' => {
                let y = self.folder_terminal.cursor_y;
                for x in self.folder_terminal.cursor_x..FOLDER_TERMINAL_COLS {
                    self.folder_terminal.screen[y][x] = ' ';
                }
            }
            _ => {}
        }
    }

    fn terminal_put_char(&mut self, ch: char) {
        if self.folder_terminal.cursor_x >= FOLDER_TERMINAL_COLS {
            self.terminal_newline();
        }
        let x = self.folder_terminal.cursor_x.min(FOLDER_TERMINAL_COLS - 1);
        let y = self.folder_terminal.cursor_y.min(FOLDER_TERMINAL_ROWS - 1);
        self.folder_terminal.screen[y][x] = ch;
        self.folder_terminal.cursor_x += 1;
    }

    fn terminal_newline(&mut self) {
        self.folder_terminal.cursor_x = 0;
        if self.folder_terminal.cursor_y + 1 >= FOLDER_TERMINAL_ROWS {
            let removed = self.folder_terminal.screen.remove(0);
            self.folder_terminal
                .history
                .push(removed.iter().collect::<String>());
            if self.folder_terminal.history.len() > TERMINAL_HISTORY_LIMIT {
                let extra = self.folder_terminal.history.len() - TERMINAL_HISTORY_LIMIT;
                self.folder_terminal.history.drain(0..extra);
            }
            self.folder_terminal.screen.push(vec![' '; FOLDER_TERMINAL_COLS]);
        } else {
            self.folder_terminal.cursor_y += 1;
        }
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
                    spawn_detached(cmd);
                }
            }
        }
        Ok(())
    }

    fn open_media(&mut self, entry: FolderEntry) -> AnyResult<()> {
        let default_kind = match entry.kind {
            FileKind::Image => Some(DefaultAppKind::Photo),
            FileKind::Video => Some(DefaultAppKind::Video),
            _ => None,
        };
        if let Some(kind) = default_kind {
            let command = self.selected_app_command(kind).to_string();
            if !command.is_empty() {
                if self.spawn_configured_app(&command, Some(&entry.path)) {
                    return Ok(());
                }
                self.folder_info = Some(format!(
                    "Could not launch default {}; choose another app.",
                    kind.label()
                ));
                self.redraw_folder()?;
                return Ok(());
            }
        }
        let slot = self.media_next_slot % MEDIA_SLOT_COUNT;
        self.media_next_slot = (slot + 1) % MEDIA_SLOT_COUNT;
        let state = MediaState {
            entry,
            playing: false,
            progress: 0.0,
        };
        self.media = Some(state.clone());
        self.media_slots[slot] = Some(state);
        self.media_front = true;
        self.media_front_slot = Some(slot);
        self.folder_front = false;
        self.settings_front = false;
        let media = self.media_geometry(slot);
        self.conn.configure_window(
            self.ui.media[slot],
            &ConfigureWindowAux::new()
                .x(i32::from(media.0))
                .y(i32::from(media.1))
                .width(u32::from(media.2))
                .height(u32::from(media.3))
                .stack_mode(StackMode::ABOVE),
        )?;
        self.conn.map_window(self.ui.media[slot])?;
        self.redraw_media_slot(slot)?;
        self.raise_media()?;
        Ok(())
    }

    fn handle_media_click(&mut self, slot: usize, x: i32, y: i32) -> AnyResult<()> {
        let (_, _, w, _) = self.media_geometry(slot);
        if x >= i32::from(w) - 43 && x <= i32::from(w) - 19 && (17..=41).contains(&y) {
            self.media_slots[slot] = None;
            if self.media_front_slot == Some(slot) {
                self.media_front_slot = None;
                self.media_front = false;
            }
            self.media = self.media_slots.iter().rev().find_map(|m| m.clone());
            self.conn.unmap_window(self.ui.media[slot])?;
            return Ok(());
        }
        if let Some(media) = self.media_slots.get_mut(slot).and_then(|m| m.as_mut()) {
            let playable = matches!(media.entry.kind, FileKind::Audio | FileKind::Video);
            if playable && x >= 26 && x <= i32::from(w) - 26 && y >= 275 && y <= 330 {
                media.playing = !media.playing;
                self.media = self.media_slots.iter().rev().find_map(|m| m.clone());
                self.redraw_media_slot(slot)?;
            }
        }
        self.raise_media()?;
        Ok(())
    }

    fn advance_internal_media(&mut self) -> AnyResult<bool> {
        let mut changed = false;
        for slot in 0..MEDIA_SLOT_COUNT {
            let Some(media) = self.media_slots.get_mut(slot).and_then(|m| m.as_mut()) else {
                continue;
            };
            if !media.playing || !matches!(media.entry.kind, FileKind::Audio | FileKind::Video) {
                continue;
            }
            media.progress += 0.006;
            if media.progress >= 1.0 {
                media.progress = 0.0;
                media.playing = false;
            }
            self.redraw_media_slot(slot)?;
            changed = true;
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

    fn apply_display_mode(&self, idx: usize) {
        if let Some(mode) = self.display_modes.get(idx) {
            let size = format!("{}x{}", mode.width, mode.height);
            let mut cmd = Command::new("xrandr");
            cmd.env("DISPLAY", &self.display).arg("-s").arg(size);
            if let Some(rate) = mode.refresh {
                cmd.arg("-r").arg(format!("{rate:.0}"));
            }
            spawn_detached(cmd);
        }
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

    fn apply_power_mode(&self, mode: PowerMode) {
        let mut cmd = Command::new("powerprofilesctl");
        cmd.args(["set", mode.command_value()]);
        spawn_detached(cmd);
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
        self.wallpaper_cache = vec![None; WALLPAPERS.len()];
        self.wallpaper_pixels = render_wallpaper_pixels(
            WALLPAPERS[self.wallpaper_index].bytes,
            self.screen_width,
            self.screen_height,
        )?;
        self.wallpaper_cache[self.wallpaper_index] = Some(self.wallpaper_pixels.clone());
        let dock = self.dock_geometry();
        let settings = self.settings_geometry();
        let folder = self.folder_geometry();
        let terminal = self.folder_terminal_geometry();
        let menu = self.app_menu_geometry();
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
        self.raise_chrome()?;
        if self.app_menu_visible {
            self.conn.configure_window(
                self.ui.app_menu,
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
        let img = Image::new(
            canvas.width,
            canvas.height,
            ScanlinePad::Pad32,
            self.depth,
            BitsPerPixel::B32,
            ImageOrder::LsbFirst,
            Cow::Borrowed(&canvas.data),
        )?;
        img.put(&self.conn, drawable, self.gc, 0, 0)?;
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
        let width = 330u16
            .min(self.screen_width.saturating_sub(48))
            .max(260.min(self.screen_width));
        let height = 480u16
            .min(
                self.screen_height
                    .saturating_sub(TOPBAR_HEIGHT + DOCK_HEIGHT + 48),
            )
            .max(320.min(self.screen_height));
        (24, (TOPBAR_HEIGHT + 26) as i16, width, height)
    }

    fn folder_terminal_geometry(&self) -> (i16, i16, u16, u16) {
        let folder = self.folder_geometry();
        let y = i32::from(folder.1) + i32::from(folder.3) + 8;
        let dock = self.dock_geometry();
        let available = i32::from(dock.1).saturating_sub(y + 10);
        let height = available.clamp(120, 260) as u16;
        (folder.0, y as i16, folder.2, height)
    }

    fn app_menu_geometry(&self) -> (i16, i16, u16, u16) {
        let width = if self.app_menu_more { 590u16 } else { 260u16 };
        let height = if self.app_menu_more { 500u16 } else { 360u16 };
        let dock = self.dock_geometry();
        let x = dock.0.max(18);
        let y = dock.1.saturating_sub(height as i16 + 10);
        (x, y, width, height)
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
        5 + self
            .clients
            .values()
            .filter(|info| info.workspace == self.active_workspace)
            .count()
            .min(5)
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
        windows.truncate(5);
        windows
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
            || window == self.ui.app_menu
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

fn resize_edges_for_frame(info: &ClientInfo, title_h: u16, x: i16, y: i16) -> Option<ResizeEdges> {
    let frame_h = i16::try_from(info.height + title_h).unwrap_or(i16::MAX);
    let width = i16::try_from(info.width).unwrap_or(i16::MAX);
    let edges = ResizeEdges {
        left: x <= RESIZE_EDGE,
        right: x >= width - RESIZE_EDGE,
        top: false,
        bottom: y >= frame_h - RESIZE_EDGE,
    };
    (edges.left || edges.right || edges.bottom).then_some(edges)
}

fn resize_edges_for_client(info: &ClientInfo, x: i16, y: i16) -> Option<ResizeEdges> {
    let width = i16::try_from(info.width).unwrap_or(i16::MAX);
    let height = i16::try_from(info.height).unwrap_or(i16::MAX);
    let edges = ResizeEdges {
        left: x <= RESIZE_EDGE,
        right: x >= width - RESIZE_EDGE,
        top: false,
        bottom: y >= height - RESIZE_EDGE,
    };
    (edges.left || edges.right || edges.bottom).then_some(edges)
}

fn client_uses_own_chrome(class: &str, title: &str) -> bool {
    let text = format!("{} {}", class, title.to_ascii_lowercase());
    ["firefox", "chromium", "google-chrome", "brave", "vivaldi"]
        .iter()
        .any(|needle| text.contains(needle))
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
        (
            cx as f32 + rad.cos() * r,
            cy as f32 + rad.sin() * r,
        )
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
    let base_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(60, 75, 96) };
    let accent_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(82, 196, 180) };

    // Monitor casing
    c.draw_round_rect(cx - 9, cy - 7, 18, 14, 3, base_color);
    // Inner bezel / screen
    c.draw_round_rect(cx - 7, cy - 5, 14, 10, 2, accent_color);
}

fn draw_sidebar_wallpaper_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_sidebar_tile(c, cx, cy, color);
    let is_topbar = color == MINT_LIGHT;
    let base_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(60, 75, 96) };
    let accent_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(82, 196, 180) };

    // Frame
    c.draw_round_rect(cx - 9, cy - 8, 18, 16, 3, base_color);
    // Moon/Sun
    c.draw_circle(cx + 4, cy - 4, 2, accent_color);
    // Left mountain peak
    draw_round_line(c, cx - 7, cy + 6, cx - 3, cy + 1, 2, if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(110, 125, 145) });
    draw_round_line(c, cx - 3, cy + 1, cx + 1, cy + 6, 2, if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(110, 125, 145) });
    // Right mountain peak
    draw_round_line(c, cx - 2, cy + 6, cx + 3, cy - 1, 2, if is_topbar { Color::rgb(195, 228, 250) } else { Color::rgb(130, 145, 165) });
    draw_round_line(c, cx + 3, cy - 1, cx + 7, cy + 6, 2, if is_topbar { Color::rgb(195, 228, 250) } else { Color::rgb(130, 145, 165) });
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
    let base_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(60, 75, 96) };
    let accent_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(82, 196, 180) };

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
    let base_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(60, 75, 96) };
    let accent_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(82, 196, 180) };

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
    let base_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(60, 75, 96) };

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
    let base_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(60, 75, 96) };
    let accent_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(82, 196, 180) };

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
    let base_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(60, 75, 96) };
    let accent_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(82, 196, 180) };

    // Horizontal battery outline
    c.draw_round_rect(cx - 9, cy - 6, 16, 12, 3, base_color);
    // Small tip on right
    c.draw_round_rect(cx + 7, cy - 3, 2, 6, 1, base_color);
    // Green charge bar on left
    c.draw_round_rect(cx - 7, cy - 4, 4, 8, 1, accent_color);
}

fn draw_wifi_icon_small(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    let is_topbar = color == MINT_LIGHT;
    let base_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(60, 75, 96) };
    let accent_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(82, 196, 180) };

    // Two concentric arcs centered at bottom dot (radii: 12 and 7, thickness: 3)
    draw_arc(c, cx, cy + 6, 12, 220.0, 320.0, 10, 3, base_color);
    draw_arc(c, cx, cy + 6, 7, 220.0, 320.0, 8, 3, base_color);
    
    // Bottom center dot
    c.draw_circle(cx, cy + 6, 3, accent_color);
}

fn draw_speaker_icon_small(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    let is_topbar = color == MINT_LIGHT;
    let base_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(60, 75, 96) };

    // Speaker flat base
    c.draw_round_rect(cx - 10, cy - 3, 5, 6, 2, base_color);

    // Flared cone in float distance space
    let x_start = cx - 5;
    let x_end = cx + 5;
    let y_start = cy - 7;
    let y_end = cy + 7;

    for y in y_start..=y_end {
        for x in x_start..=x_end {
            let x_f = x as f32;
            let y_f = y as f32;

            // Top slope line: passes through (cx-5, cy-3) and (cx+5, cy-7)
            let top_y = (cy - 3) as f32 - (x_f - (cx - 5) as f32) * (4.0 / 10.0);
            // Bottom slope line: passes through (cx-5, cy+3) and (cx+5, cy+7)
            let bottom_y = (cy + 3) as f32 + (x_f - (cx - 5) as f32) * (4.0 / 10.0);

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
    let img = image::load_from_memory(bytes)?.to_rgba8();
    let (iw, ih) = img.dimensions();
    let sw = u32::from(screen_width);
    let sh = u32::from(screen_height);
    let scale = (sw as f32 / iw as f32).max(sh as f32 / ih as f32);
    let nw = (iw as f32 * scale).ceil() as u32;
    let nh = (ih as f32 * scale).ceil() as u32;
    let resized = image::imageops::resize(&img, nw, nh, FilterType::Triangle);
    let ox = (nw.saturating_sub(sw)) / 2;
    let oy = (nh.saturating_sub(sh)) / 2;
    let cropped = image::imageops::crop_imm(&resized, ox, oy, sw, sh).to_image();
    let mut out = vec![0; usize::from(screen_width) * usize::from(screen_height) * 4];
    for (idx, px) in cropped.pixels().enumerate() {
        out[idx * 4] = px[2];
        out[idx * 4 + 1] = px[1];
        out[idx * 4 + 2] = px[0];
        out[idx * 4 + 3] = 0;
    }
    Ok(out)
}

fn render_asset_preview_pixels(bytes: &[u8], w: u16, h: u16) -> AnyResult<Vec<u8>> {
    let img = image::load_from_memory(bytes)?.to_rgba8();
    let (iw, ih) = img.dimensions();
    let scale = (f32::from(w) / iw as f32).max(f32::from(h) / ih as f32);
    let nw = (iw as f32 * scale).ceil() as u32;
    let nh = (ih as f32 * scale).ceil() as u32;
    let resized = image::imageops::resize(&img, nw, nh, FilterType::Triangle);
    let ox = (nw.saturating_sub(u32::from(w))) / 2;
    let oy = (nh.saturating_sub(u32::from(h))) / 2;
    let cropped =
        image::imageops::crop_imm(&resized, ox, oy, u32::from(w), u32::from(h)).to_image();
    let mut out = vec![0; usize::from(w) * usize::from(h) * 4];
    for (idx, px) in cropped.pixels().enumerate() {
        out[idx * 4] = px[2];
        out[idx * 4 + 1] = px[1];
        out[idx * 4 + 2] = px[0];
        out[idx * 4 + 3] = 0;
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

fn draw_text_preview(
    c: &mut Canvas,
    font: &Font<'static>,
    path: &std::path::Path,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    let Ok(mut file) = fs::File::open(path) else {
        return;
    };
    let mut buf = String::new();
    let _ = file.by_ref().take(4096).read_to_string(&mut buf);
    let max_lines = (h / 18).max(1) as usize;
    for (idx, line) in buf.lines().take(max_lines).enumerate() {
        c.draw_text(
            font,
            &compact(line.trim_end(), (w / 7).max(12) as usize),
            x,
            y + idx as i32 * 18,
            12.0,
            INK,
        );
    }
}

fn read_display_modes(display: &str, current_width: u16, current_height: u16) -> Vec<DisplayMode> {
    let mut modes = Vec::new();
    if let Ok(output) = Command::new("xrandr")
        .env("DISPLAY", display)
        .arg("--query")
        .output()
    {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
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
            let refresh = trimmed
                .split_whitespace()
                .skip(1)
                .find_map(|token| token.trim_end_matches(['*', '+']).parse::<f32>().ok())
                .map(|rate| if rate < 1.0 { 60.0 } else { rate });
            if !modes
                .iter()
                .any(|m: &DisplayMode| m.width == width && m.height == height)
            {
                modes.push(DisplayMode {
                    width,
                    height,
                    refresh,
                });
            }
        }
    }
    if modes.is_empty() {
        modes.push(DisplayMode {
            width: current_width,
            height: current_height,
            refresh: Some(60.0),
        });
        modes.push(DisplayMode {
            width: 1366,
            height: 768,
            refresh: Some(60.0),
        });
        modes.push(DisplayMode {
            width: 1600,
            height: 900,
            refresh: Some(60.0),
        });
        modes.push(DisplayMode {
            width: 1920,
            height: 1080,
            refresh: Some(60.0),
        });
    }
    modes
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

fn read_audio_devices(kind: &str) -> Vec<String> {
    let Ok(output) = Command::new("pactl")
        .arg("list")
        .arg("short")
        .arg(kind)
        .output()
    else {
        return vec!["PulseAudio/PipeWire device list unavailable".to_string()];
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1).map(str::to_string))
        .collect()
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

fn read_app_command(kind: DefaultAppKind) -> String {
    fs::read_to_string(terminal_settings_path())
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix(&format!("{}=", kind.key()))
                    .map(str::to_string)
            })
        })
        .unwrap_or_default()
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
            "terminal={}\nbrowser={}\nphoto={}\nvideo={}\n",
            clean(&settings.terminal_command),
            clean(&settings.browser_command),
            clean(&settings.photo_command),
            clean(&settings.video_command),
        ),
    )?;
    Ok(())
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

fn percent(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (used as f32 * 100.0 / total as f32).clamp(0.0, 100.0)
    }
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
            break;
        }
    }
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
        if kind == FileKind::Other && entries.len() > 10 {
            continue;
        }
        entries.push(FolderEntry {
            name,
            path: entry_path,
            kind,
        });
        if entries.len() >= 64 {
            break;
        }
    }
    sort_folder_entries(&mut entries, sort);
    entries.truncate(18);
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
            FolderSort::Date => entry_modified_secs(b).cmp(&entry_modified_secs(a)).then(
                a.name
                    .to_lowercase()
                    .cmp(&b.name.to_lowercase()),
            ),
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
    fs::metadata(&entry.path).map(|meta| meta.len()).unwrap_or(0)
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
        let shell_c = CString::new("/bin/sh").unwrap();
        let interactive = CString::new("-i").unwrap();
        unsafe {
            libc::execlp(
                shell_c.as_ptr(),
                shell_c.as_ptr(),
                interactive.as_ptr(),
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
