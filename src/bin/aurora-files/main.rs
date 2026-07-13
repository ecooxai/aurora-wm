//! Aurora Files: standalone file manager with a built-in tabbed terminal
//! and viewers for text, images, audio/video, PDF, office documents and
//! 3D models. Designed to pair with aurora-wm but works under any X11 WM.
//!
//! Usage:
//!   aurora-files [PATH]        open browsing at PATH (default: home)
//!   aurora-files --terminal    open with the terminal focused
//!   aurora-files --register-default   register as default file manager

mod canvas;
mod fsmodel;
mod imgwin;
mod term;
mod viewer;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use canvas::*;
use fsmodel::*;
use imgwin::{ImgState, IMG_TOP_BAR};
use rusttype::Font;
use term::{Tab, TERM_BG};
use viewer::*;
use x11rb::connection::Connection;
use x11rb::image::{BitsPerPixel, Image, ImageOrder, ScanlinePad};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const FONT_REGULAR: &[u8] = include_bytes!("../../../fonts/NotoSans-Regular.ttf");
const FONT_BOLD: &[u8] = include_bytes!("../../../fonts/NotoSans-Bold.ttf");
const FONT_MONO: &[u8] = include_bytes!("../../../fonts/NotoSansMono-Regular.ttf");

const HEADER_H: i32 = 74;
const SIDEBAR_W: i32 = 172;
const TAB_BAR_H: i32 = 30;
const ROW_H: i32 = 34;
const CELL_W: i32 = 8;
const CELL_H: i32 = 17;
const TERMINAL_COPY_DELAY: Duration = Duration::from_secs(2);
/// Maximum number of folder tabs; opening more replaces the last one.
const MAX_FOLDER_TABS: usize = 20;
const TERMINAL_NOTICE_DURATION: Duration = Duration::from_secs(3);
/// How often the current folder is re-scanned for new/removed entries.
const DIR_REFRESH_INTERVAL: Duration = Duration::from_secs(3);
/// Height of the toolbar at the top of the text view/editor window.
const TXT_TOP_BAR: i32 = 40;
/// Line height of the text view/editor window.
const TXT_LINE_H: i32 = 18;
const TEXT_FONT_SIZE: f32 = 13.0;

/// Options in the image window's top-right dropdown menu.
const IMG_MENU_ITEMS: [&str; 8] = [
    "Zoom in",
    "Zoom out",
    "Reset zoom",
    "Rotate 90",
    "Flip horizontal",
    "Flip vertical",
    "Crop",
    "Copy image",
];
/// Options in the image window's right-click context menu.
const IMG_CTX_ITEMS: [&str; 3] = ["Copy image", "Copy path", "Reset zoom"];

/// Geometry of the image window's right-click menu, clamped to the window.
fn img_ctx_geometry(mx: i32, my: i32, w: i32, h: i32) -> (i32, i32, i32, i32) {
    let gw = 170;
    let gh = IMG_CTX_ITEMS.len() as i32 * 28 + 10;
    (
        mx.min(w - gw - 6).max(4),
        my.min(h - gh - 6).max(4),
        gw,
        gh,
    )
}

type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Files,
    Terminal,
    Editor,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortMode {
    Name,
    Date,
    Size,
}

impl SortMode {
    fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Date => "Date",
            Self::Size => "Size",
        }
    }
}

#[derive(Clone, Copy)]
struct TerminalSelection {
    tab: usize,
    start: (usize, usize),
    end: (usize, usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FolderTabRole {
    Folder,
    Screenshots,
}

struct FolderTab {
    path: PathBuf,
    role: FolderTabRole,
    last_file: Option<PathBuf>,
}

struct TerminalGroup {
    tabs: Vec<Tab>,
    active_tab: usize,
}

/// Atoms used to act as an XDND (drag-and-drop) source so files/images can be
/// dragged out of the viewer/list into other applications.
struct XdndAtoms {
    aware: Atom,
    selection: Atom,
    enter: Atom,
    position: Atom,
    status: Atom,
    leave: Atom,
    drop: Atom,
    finished: Atom,
    action_copy: Atom,
    uri_list: Atom,
}

impl XdndAtoms {
    fn intern(conn: &RustConnection) -> AnyResult<Self> {
        let a = |name: &[u8]| -> AnyResult<Atom> {
            Ok(conn.intern_atom(false, name)?.reply()?.atom)
        };
        Ok(Self {
            aware: a(b"XdndAware")?,
            selection: a(b"XdndSelection")?,
            enter: a(b"XdndEnter")?,
            position: a(b"XdndPosition")?,
            status: a(b"XdndStatus")?,
            leave: a(b"XdndLeave")?,
            drop: a(b"XdndDrop")?,
            finished: a(b"XdndFinished")?,
            action_copy: a(b"XdndActionCopy")?,
            uri_list: a(b"text/uri-list")?,
        })
    }
}

/// State of a file being dragged out to another application.
struct FileDrag {
    path: PathBuf,
    start_x: i32,
    start_y: i32,
    started: bool,
    target: Window,
    target_ver: u8,
    accepted: bool,
    /// Window in which the drag started (main window or image window).
    src_win: Window,
}

/// The standalone image viewer window shown at the right of the file list.
struct ImgWin {
    window: Window,
    gc: Gcontext,
    width: u16,
    height: u16,
    maximized: bool,
    compact: bool,
    state: ImgState,
    dirty: bool,
}

/// The standalone text viewer/editor window shown at the right of the file
/// list, mirroring the image viewer window.
struct TxtWin {
    window: Window,
    gc: Gcontext,
    width: u16,
    height: u16,
    maximized: bool,
    compact: bool,
    selecting: bool,
    text: TextView,
    dirty: bool,
}

#[derive(Clone, Copy)]
enum PendingOpen {
    Preview(usize),
    Open(usize),
}

struct App {
    conn: RustConnection,
    display: String,
    root: Window,
    window: Window,
    media_embed: Window,
    gc: Gcontext,
    depth: u8,
    width: u16,
    height: u16,
    screen_w: u16,
    screen_h: u16,
    regular: Font<'static>,
    bold: Font<'static>,
    mono: Font<'static>,
    wm_protocols: Atom,
    wm_delete: Atom,
    current_desktop_atom: Atom,
    open_screenshot_atom: Atom,
    screenshot_path_atom: Atom,
    open_folder_tab_atom: Atom,
    folder_tab_path_atom: Atom,
    utf8_string_atom: Atom,

    cwd: PathBuf,
    entries: Vec<Entry>,
    places: Vec<fsmodel::Place>,
    selected: Option<usize>,
    last_click: Option<(usize, Instant)>,
    scroll: usize,
    show_hidden: bool,
    sort_mode: SortMode,
    sort_open: bool,
    folder_tabs_open: bool,
    more_open: bool,
    status: String,
    folder_tabs: Vec<FolderTab>,
    active_folder_tab: usize,
    workspace_tabs: HashMap<u32, usize>,
    current_workspace: u32,
    last_workspace_check: Instant,
    viewer_only: bool,
    close_at: Option<Instant>,

    terminal_visible: bool,
    terminal_h: i32,
    tabs: Vec<Tab>,
    active_tab: usize,
    terminal_groups: HashMap<u32, TerminalGroup>,
    last_terminal_cwd_check: Instant,
    terminal_sync_suppress_until: Instant,
    suppress_terminal_navigation: bool,
    terminal_selection: Option<TerminalSelection>,
    terminal_selecting: bool,
    terminal_copy_due: Option<Instant>,
    terminal_notice: Option<(String, Instant)>,

    /// Last time the current folder was re-scanned for changes.
    last_dir_refresh: Instant,
    /// Paths known to exist in the current folder (for change detection).
    known_paths: HashSet<PathBuf>,
    /// Recently created paths, newest first; shown at the top of the list.
    recent_paths: Vec<PathBuf>,
    /// Right-click context menu for a file/folder row: (x, y, entry path).
    file_menu: Option<(i32, i32, PathBuf)>,
    /// Path stored by the context menu's "Copy" action, used by "Paste".
    copied_path: Option<PathBuf>,
    /// Row pressed with button 1, opened on release if no drag started.
    pending_open: Option<PendingOpen>,
    /// The standalone image viewer window, when open.
    img_win: Option<ImgWin>,
    /// The standalone text viewer/editor window, when open.
    txt_win: Option<TxtWin>,

    viewer: Option<Viewer>,
    focus: Focus,
    /// Right-click context menu for the image viewer, anchored at (x, y).
    image_menu: Option<(i32, i32)>,
    xdnd: XdndAtoms,
    /// A file being dragged out of the app to another application.
    file_drag: Option<FileDrag>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--register-default") {
        register_default();
        return;
    }
    if let Err(err) = run(&args) {
        eprintln!("aurora-files: {err}");
        std::process::exit(1);
    }
}

fn register_default() {
    let status = Command::new("xdg-mime")
        .args(["default", "aurora-files.desktop", "inode/directory"])
        .status();
    match status {
        Ok(s) if s.success() => println!("aurora-files registered as default file manager."),
        _ => eprintln!("could not run xdg-mime; is xdg-utils installed?"),
    }
}

fn normalized_terminal_selection(
    selection: TerminalSelection,
) -> ((usize, usize), (usize, usize)) {
    if selection.start <= selection.end {
        (selection.start, selection.end)
    } else {
        (selection.end, selection.start)
    }
}

/// Translate a printable terminal key into the bytes expected by a PTY.
/// In particular, Ctrl+V is ASCII SYN (0x16), which lets the shell handle its
/// normal quoted-insert binding instead of turning it into a clipboard paste.
fn terminal_printable_input(keysym: u32, ctrl: bool) -> Option<u8> {
    let ch = u8::try_from(keysym).ok()?;
    if !(0x20..=0x7e).contains(&ch) {
        return None;
    }
    Some(if ctrl {
        ch.to_ascii_uppercase().wrapping_sub(b'@') & 0x1f
    } else {
        ch
    })
}

#[cfg(test)]
mod terminal_key_tests {
    use super::terminal_printable_input;

    #[test]
    fn ctrl_v_is_forwarded_as_shell_control_character() {
        assert_eq!(terminal_printable_input('v' as u32, true), Some(0x16));
        assert_eq!(terminal_printable_input('V' as u32, true), Some(0x16));
    }

    #[test]
    fn unmodified_v_remains_printable() {
        assert_eq!(terminal_printable_input('v' as u32, false), Some(b'v'));
    }
}

fn text_prefix_width(font: &Font<'static>, line: &str, col: usize) -> i32 {
    let prefix: String = line.chars().take(col).collect();
    measure_text(font, &prefix, TEXT_FONT_SIZE)
}

/// Return the character boundary nearest to a click position in rendered text.
fn text_column_at_x(font: &Font<'static>, line: &str, x: i32) -> usize {
    if x <= 0 {
        return 0;
    }
    let mut prefix = String::new();
    let mut previous = 0;
    for (col, ch) in line.chars().enumerate() {
        prefix.push(ch);
        let next = measure_text(font, &prefix, TEXT_FONT_SIZE);
        if x < previous + (next - previous) / 2 {
            return col;
        }
        previous = next;
    }
    line.chars().count()
}

fn text_selection_columns(
    selection: TextSelection,
    line: usize,
    char_count: usize,
) -> Option<(usize, usize)> {
    let (start, end) = if selection.start <= selection.end {
        (selection.start, selection.end)
    } else {
        (selection.end, selection.start)
    };
    if start == end || line < start.0 || line > end.0 {
        return None;
    }
    let first = if line == start.0 { start.1.min(char_count) } else { 0 };
    let last = if line == end.0 { end.1.min(char_count) } else { char_count };
    (last > first).then_some((first, last))
}

fn copy_text_to_system_clipboard(text: &str) -> bool {
    for (program, args) in [
        ("xclip", &["-selection", "clipboard"][..]),
        ("xsel", &["--clipboard", "--input"][..]),
    ] {
        let Ok(mut child) = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        let wrote = child
            .stdin
            .take()
            .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
        let succeeded = child.wait().is_ok_and(|status| status.success());
        if wrote && succeeded {
            return true;
        }
    }
    false
}

/// Copy an image file to the system clipboard as image/png (best effort), so it can be
/// pasted into other applications.
fn copy_image_to_system_clipboard(path: &Path) -> bool {
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        _ => "image/png",
    };
    Command::new("xclip")
        .args(["-selection", "clipboard", "-t", mime, "-i"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .and_then(|mut child| child.wait())
        .map(|status| status.success())
        .unwrap_or(false)
}

fn read_text_from_system_clipboard() -> Option<String> {
    for (program, args) in [
        (
            "xclip",
            &["-selection", "clipboard", "-o", "-target", "UTF8_STRING"][..],
        ),
        ("xclip", &["-selection", "clipboard", "-o"][..]),
        ("xsel", &["--clipboard", "--output"][..]),
    ] {
        let Ok(output) = Command::new(program)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        else {
            continue;
        };
        if output.status.success() {
            return String::from_utf8(output.stdout).ok();
        }
    }
    None
}

fn read_current_desktop(
    conn: &RustConnection,
    root: Window,
    current_desktop_atom: Atom,
) -> u32 {
    conn.get_property(
        false,
        root,
        current_desktop_atom,
        AtomEnum::CARDINAL,
        0,
        1,
    )
    .ok()
    .and_then(|cookie| cookie.reply().ok())
    .and_then(|reply| reply.value32().and_then(|mut values| values.next()))
    .unwrap_or(0)
}

fn request_sticky(
    conn: &RustConnection,
    root: Window,
    window: Window,
) -> AnyResult<()> {
    let state_atom = conn.intern_atom(false, b"_NET_WM_STATE")?.reply()?.atom;
    let sticky_atom = conn
        .intern_atom(false, b"_NET_WM_STATE_STICKY")?
        .reply()?
        .atom;
    let desktop_atom = conn
        .intern_atom(false, b"_NET_WM_DESKTOP")?
        .reply()?
        .atom;
    conn.change_property32(
        PropMode::REPLACE,
        window,
        desktop_atom,
        AtomEnum::CARDINAL,
        &[u32::MAX],
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        window,
        state_atom,
        AtomEnum::ATOM,
        &[sticky_atom],
    )?;
    let event = ClientMessageEvent::new(
        32,
        window,
        state_atom,
        [1, sticky_atom, 0, 1, 0],
    );
    conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    )?;
    Ok(())
}

fn run(args: &[String]) -> AnyResult<()> {
    let viewer_only = args.iter().any(|arg| arg == "--image-viewer");
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
    let (conn, screen_num) = RustConnection::connect(None)?;
    let screen = conn.setup().roots[screen_num].clone();
    let window = conn.generate_id()?;
    let compact_size = (screen.width_in_pixels / 3)
        .clamp(360, 500)
        .min(screen.height_in_pixels.saturating_sub(80));
    let width = compact_size;
    let height = (screen.height_in_pixels * 4 / 5)
        .max(360)
        .min(screen.height_in_pixels.saturating_sub(40));
    conn.create_window(
        screen.root_depth,
        window,
        screen.root,
        60,
        60,
        width,
        height,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.white_pixel)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::KEY_PRESS
                    | EventMask::STRUCTURE_NOTIFY,
            ),
    )?;
    let title = if viewer_only {
        "Aurora Screenshot"
    } else {
        "Aurora Files"
    };
    conn.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        title.as_bytes(),
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"aurora-files\0Aurora Files\0",
    )?;
    let wm_protocols = conn.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
    let wm_delete = conn.intern_atom(false, b"WM_DELETE_WINDOW")?.reply()?.atom;
    let current_desktop_atom = conn
        .intern_atom(false, b"_NET_CURRENT_DESKTOP")?
        .reply()?
        .atom;
    let open_screenshot_atom = conn
        .intern_atom(false, b"_AURORA_FILES_OPEN_SCREENSHOT")?
        .reply()?
        .atom;
    let screenshot_path_atom = conn
        .intern_atom(false, b"_AURORA_FILES_SCREENSHOT_PATH")?
        .reply()?
        .atom;
    let open_folder_tab_atom = conn
        .intern_atom(false, b"_AURORA_FILES_OPEN_FOLDER_TAB")?
        .reply()?
        .atom;
    let folder_tab_path_atom = conn
        .intern_atom(false, b"_AURORA_FILES_FOLDER_TAB_PATH")?
        .reply()?
        .atom;
    let utf8_string_atom = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
    let xdnd = XdndAtoms::intern(&conn)?;
    // Advertise ourselves as an XDND source (version 5).
    conn.change_property32(
        PropMode::REPLACE,
        window,
        xdnd.aware,
        AtomEnum::ATOM,
        &[5u32],
    )?;
    let current_workspace = read_current_desktop(&conn, screen.root, current_desktop_atom);
    conn.change_property32(
        PropMode::REPLACE,
        window,
        wm_protocols,
        AtomEnum::ATOM,
        &[wm_delete],
    )?;
    // Child window used for embedding mpv video output.
    let media_embed = conn.generate_id()?;
    conn.create_window(
        screen.root_depth,
        media_embed,
        window,
        0,
        0,
        16,
        16,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new().background_pixel(0),
    )?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, window, &CreateGCAux::new().graphics_exposures(0))?;
    conn.map_window(window)?;
    if !viewer_only {
        request_sticky(&conn, screen.root, window)?;
    }
    conn.flush()?;

    let start_path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .unwrap_or_else(home_dir);
    let start_dir = if start_path.is_dir() {
        start_path.clone()
    } else {
        start_path.parent().map(Path::to_path_buf).unwrap_or_else(home_dir)
    };
    let mut workspace_tabs = HashMap::new();
    workspace_tabs.insert(current_workspace, 0);

    let mut app = App {
        conn,
        display,
        root: screen.root,
        window,
        media_embed,
        gc,
        depth: screen.root_depth,
        width,
        height,
        screen_w: screen.width_in_pixels,
        screen_h: screen.height_in_pixels,
        regular: Font::try_from_bytes(FONT_REGULAR).ok_or("font")?,
        bold: Font::try_from_bytes(FONT_BOLD).ok_or("font")?,
        mono: Font::try_from_bytes(FONT_MONO).ok_or("font")?,
        wm_protocols,
        wm_delete,
        current_desktop_atom,
        open_screenshot_atom,
        screenshot_path_atom,
        open_folder_tab_atom,
        folder_tab_path_atom,
        utf8_string_atom,
        entries: list_dir(&start_dir, false),
        places: places(),
        cwd: start_dir.clone(),
        selected: None,
        last_click: None,
        scroll: 0,
        show_hidden: false,
        sort_mode: SortMode::Name,
        sort_open: false,
        folder_tabs_open: false,
        more_open: false,
        status: String::new(),
        folder_tabs: vec![FolderTab {
            path: start_dir.clone(),
            role: FolderTabRole::Folder,
            last_file: None,
        }],
        active_folder_tab: 0,
        workspace_tabs,
        current_workspace,
        last_workspace_check: Instant::now(),
        viewer_only,
        close_at: viewer_only.then(|| Instant::now() + Duration::from_secs(60)),
        terminal_visible: !viewer_only,
        terminal_h: i32::from(height) / 2,
        tabs: Vec::new(),
        active_tab: 0,
        terminal_groups: HashMap::new(),
        last_terminal_cwd_check: Instant::now(),
        terminal_sync_suppress_until: Instant::now(),
        suppress_terminal_navigation: false,
        terminal_selection: None,
        terminal_selecting: false,
        terminal_copy_due: None,
        terminal_notice: None,
        last_dir_refresh: Instant::now(),
        known_paths: HashSet::new(),
        recent_paths: Vec::new(),
        file_menu: None,
        copied_path: None,
        pending_open: None,
        img_win: None,
        txt_win: None,
        viewer: None,
        image_menu: None,
        xdnd,
        file_drag: None,
        focus: if args.iter().any(|a| a == "--terminal") {
            Focus::Terminal
        } else {
            Focus::Files
        },
    };
    app.reset_dir_watch();
    app.refresh_entries();
    if !start_path.is_dir() {
        app.open_file_by_path(&start_path.clone());
    }
    if !viewer_only {
        app.new_tab();
    }
    app.event_loop()
}

impl App {
    // ------------------------------------------------------------ layout

    fn terminal_rect(&self) -> (i32, i32, i32, i32) {
        let h = if self.terminal_visible { self.terminal_h } else { 0 };
        (0, i32::from(self.height) - h, i32::from(self.width), h)
    }

    fn content_rect(&self) -> (i32, i32, i32, i32) {
        if self.viewer_only {
            return (0, 0, i32::from(self.width), i32::from(self.height));
        }
        let (_, ty, _, th) = self.terminal_rect();
        let _ = th;
        (
            SIDEBAR_W,
            HEADER_H,
            i32::from(self.width) - SIDEBAR_W,
            ty - HEADER_H,
        )
    }

    fn visible_rows(&self) -> usize {
        let (_, _, _, ch) = self.content_rect();
        ((ch - 8).max(0) / ROW_H) as usize
    }

    fn term_grid_size(&self) -> (usize, usize) {
        let (_, _, tw, th) = self.terminal_rect();
        let cols = ((tw - 20) / CELL_W).max(10) as usize;
        let rows = ((th - TAB_BAR_H - 14) / CELL_H).max(3) as usize;
        (cols, rows)
    }

    // ------------------------------------------------------------ terminal

    fn new_tab(&mut self) {
        let (cols, rows) = self.term_grid_size();
        if let Some(tab) = Tab::new(self.cwd.clone(), cols, rows) {
            self.tabs.push(tab);
            self.active_tab = self.tabs.len() - 1;
        } else {
            self.status = "Could not start shell".into();
        }
    }

    fn close_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.tabs.remove(idx);
        }
        if self.tabs.is_empty() {
            self.new_tab();
        }
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
    }

    fn switch_terminal_group(&mut self, workspace: u32) {
        self.terminal_groups.insert(
            self.current_workspace,
            TerminalGroup {
                tabs: std::mem::take(&mut self.tabs),
                active_tab: self.active_tab,
            },
        );
        if let Some(group) = self.terminal_groups.remove(&workspace) {
            self.tabs = group.tabs;
            self.active_tab = group.active_tab.min(self.tabs.len().saturating_sub(1));
        } else {
            self.active_tab = 0;
            self.new_tab();
        }
        self.clear_terminal_selection();
        self.terminal_notice = None;
    }

    /// cd the terminal to `path`; keeps busy tabs intact by opening a new one.
    fn terminal_cd(&mut self, path: &Path) {
        if !self.terminal_visible || self.tabs.is_empty() {
            return;
        }
        let active_busy = self
            .tabs
            .get(self.active_tab)
            .map(|t| t.busy() || t.dead)
            .unwrap_or(true);
        if active_busy {
            let (cols, rows) = self.term_grid_size();
            if let Some(tab) = Tab::new(path.to_path_buf(), cols, rows) {
                self.tabs.push(tab);
                self.active_tab = self.tabs.len() - 1;
                self.status = "Opened new terminal tab (previous one is busy)".into();
            }
        } else if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.send_cd(path);
            tab.cwd = path.to_path_buf();
            tab.title = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "/".into());
        }
        self.terminal_sync_suppress_until = Instant::now() + Duration::from_millis(500);
    }

    fn sync_folder_to_active_terminal(&mut self) -> bool {
        if self.viewer_only
            || self.last_terminal_cwd_check.elapsed() < Duration::from_millis(150)
            || Instant::now() < self.terminal_sync_suppress_until
        {
            return false;
        }
        self.last_terminal_cwd_check = Instant::now();
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return false;
        };
        let Ok(path) = std::fs::read_link(format!("/proc/{}/cwd", tab.pid)) else {
            return false;
        };
        if !path.is_dir() || path == self.cwd {
            return false;
        }
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.cwd = path.clone();
            tab.title = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/".into());
        }
        self.cwd = path.clone();
        self.reset_dir_watch();
        self.refresh_entries();
        self.selected = None;
        self.scroll = 0;
        self.viewer_close();
        self.focus = Focus::Terminal;
        if let Some(tab) = self.folder_tabs.get_mut(self.active_folder_tab) {
            tab.path = path;
        }
        if self
            .folder_tabs
            .get(self.active_folder_tab)
            .is_some_and(|tab| tab.role == FolderTabRole::Folder)
        {
            self.workspace_tabs
                .insert(self.current_workspace, self.active_folder_tab);
        }
        self.status = "Folder followed terminal directory".into();
        true
    }

    // ------------------------------------------------------------ folder tabs

    fn select_folder_tab(&mut self, idx: usize) {
        let Some((path, role, last_file)) = self
            .folder_tabs
            .get(idx)
            .map(|tab| (tab.path.clone(), tab.role, tab.last_file.clone()))
        else {
            return;
        };
        self.active_folder_tab = idx;
        if role == FolderTabRole::Folder {
            self.workspace_tabs.insert(self.current_workspace, idx);
        }
        self.folder_tabs_open = false;
        self.navigate(&path);
        if role == FolderTabRole::Screenshots {
            if let Some(path) = last_file.filter(|path| path.exists()) {
                self.open_file_by_path(&path);
            }
        }
    }

    /// Append a folder tab, enforcing the tab cap: at the limit the last tab is
    /// replaced instead of growing the list unbounded. Returns the tab's index.
    fn push_folder_tab(&mut self, tab: FolderTab) -> usize {
        if self.folder_tabs.len() >= MAX_FOLDER_TABS {
            let idx = self.folder_tabs.len() - 1;
            self.folder_tabs[idx] = tab;
            idx
        } else {
            self.folder_tabs.push(tab);
            self.folder_tabs.len() - 1
        }
    }

    /// Index of an existing plain-folder tab already showing `path`, if any.
    fn existing_folder_tab(&self, path: &Path) -> Option<usize> {
        self.folder_tabs
            .iter()
            .position(|tab| tab.role == FolderTabRole::Folder && tab.path == path)
    }

    fn add_folder_tab(&mut self) {
        let path = home_dir();
        // Don't create a duplicate tab for a path that already has one.
        if let Some(idx) = self.existing_folder_tab(&path) {
            self.select_folder_tab(idx);
            self.status = "Switched to existing tab".into();
            return;
        }
        let idx = self.push_folder_tab(FolderTab {
            path,
            role: FolderTabRole::Folder,
            last_file: None,
        });
        self.select_folder_tab(idx);
        self.status = format!("New folder tab for Workspace {}", self.current_workspace + 1);
    }

    fn open_folder_tab(&mut self, path: PathBuf) {
        if !path.is_dir() {
            return;
        }
        // Switch to an existing tab for this path rather than duplicating it.
        if let Some(idx) = self.existing_folder_tab(&path) {
            self.select_folder_tab(idx);
            self.status = "Switched to existing tab".into();
            return;
        }
        let idx = self.push_folder_tab(FolderTab {
            path,
            role: FolderTabRole::Folder,
            last_file: None,
        });
        self.select_folder_tab(idx);
        self.status = "Opened folder in a new tab".into();
    }

    fn open_screenshot(&mut self, path: PathBuf) {
        if file_kind_for(&path) != FileKind::Image {
            return;
        }
        let parent = path.parent().map(Path::to_path_buf).unwrap_or_else(home_dir);
        let idx = self
            .folder_tabs
            .iter()
            .position(|tab| tab.role == FolderTabRole::Screenshots)
            .unwrap_or_else(|| {
                let idx = self.folder_tabs.len();
                self.folder_tabs.push(FolderTab {
                    path: parent.clone(),
                    role: FolderTabRole::Screenshots,
                    last_file: None,
                });
                idx
            });
        if let Some(tab) = self.folder_tabs.get_mut(idx) {
            tab.path = parent.clone();
            tab.last_file = Some(path.clone());
        }
        self.active_folder_tab = idx;
        self.folder_tabs_open = false;
        self.navigate(&parent);
        self.open_file_by_path(&path);
        self.status = "Latest screenshot".into();
    }

    fn handle_screenshot_message(&mut self) {
        let Ok(cookie) = self.conn.get_property(
            false,
            self.window,
            self.screenshot_path_atom,
            self.utf8_string_atom,
            0,
            65535,
        ) else {
            return;
        };
        let Ok(reply) = cookie.reply() else {
            return;
        };
        if reply.value.is_empty() {
            return;
        }
        let path = PathBuf::from(String::from_utf8_lossy(&reply.value).into_owned());
        if path.exists() {
            self.open_screenshot(path);
        }
    }

    fn handle_folder_tab_message(&mut self) {
        let Ok(cookie) = self.conn.get_property(
            false,
            self.window,
            self.folder_tab_path_atom,
            self.utf8_string_atom,
            0,
            65535,
        ) else {
            return;
        };
        let Ok(reply) = cookie.reply() else {
            return;
        };
        if reply.value.is_empty() {
            return;
        }
        self.open_folder_tab(PathBuf::from(
            String::from_utf8_lossy(&reply.value).into_owned(),
        ));
    }

    fn sync_workspace_folder_tab(&mut self) -> bool {
        if self.last_workspace_check.elapsed() < Duration::from_millis(100) {
            return false;
        }
        self.last_workspace_check = Instant::now();
        let workspace =
            read_current_desktop(&self.conn, self.root, self.current_desktop_atom);
        if workspace == self.current_workspace {
            return false;
        }
        self.switch_terminal_group(workspace);
        self.current_workspace = workspace;
        let idx = self
            .workspace_tabs
            .get(&workspace)
            .copied()
            .filter(|idx| *idx < self.folder_tabs.len())
            .unwrap_or_else(|| {
                let idx = self.folder_tabs.len();
                self.folder_tabs.push(FolderTab {
                    path: home_dir(),
                    role: FolderTabRole::Folder,
                    last_file: None,
                });
                self.workspace_tabs.insert(workspace, idx);
                idx
            });
        self.suppress_terminal_navigation = true;
        self.select_folder_tab(idx);
        self.suppress_terminal_navigation = false;
        self.terminal_sync_suppress_until = Instant::now() + Duration::from_millis(300);
        self.status = format!("Workspace {}", workspace + 1);
        true
    }

    // ------------------------------------------------------------ navigation

    fn refresh_entries(&mut self) {
        self.entries = list_dir(&self.cwd, self.show_hidden);
        let mode = self.sort_mode;
        self.entries.sort_by(|a, b| {
            let folders = (a.kind != FileKind::Directory).cmp(&(b.kind != FileKind::Directory));
            if folders != std::cmp::Ordering::Equal {
                return folders;
            }
            match mode {
                SortMode::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortMode::Date => b
                    .modified
                    .cmp(&a.modified)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                SortMode::Size => b
                    .size
                    .cmp(&a.size)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
            }
        });
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            let parent_name = parent
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "/".into());
            self.entries.insert(
                0,
                Entry {
                    name: format!(".. {parent_name}"),
                    path: parent,
                    kind: FileKind::Directory,
                    size: 0,
                    modified: std::time::SystemTime::UNIX_EPOCH,
                },
            );
        }
        // Hoist recently created entries to the top (below the parent entry),
        // newest first.
        if !self.recent_paths.is_empty() {
            let insert_at = usize::from(self.cwd.parent().is_some());
            for path in self.recent_paths.iter().rev() {
                if let Some(pos) = self.entries.iter().position(|entry| entry.path == *path) {
                    if pos > insert_at {
                        let entry = self.entries.remove(pos);
                        self.entries.insert(insert_at, entry);
                    }
                }
            }
        }
    }

    /// Reset change tracking for the current folder (called on navigation).
    fn reset_dir_watch(&mut self) {
        self.recent_paths.clear();
        self.known_paths = list_dir(&self.cwd, self.show_hidden)
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        self.last_dir_refresh = Instant::now();
    }

    /// Re-scan the current folder every DIR_REFRESH_INTERVAL; new files and
    /// folders are hoisted to the top of the list. Returns true on change.
    fn poll_directory_refresh(&mut self) -> bool {
        if self.viewer_only || self.last_dir_refresh.elapsed() < DIR_REFRESH_INTERVAL {
            return false;
        }
        self.last_dir_refresh = Instant::now();
        let fresh: HashSet<PathBuf> = list_dir(&self.cwd, self.show_hidden)
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        if fresh == self.known_paths {
            return false;
        }
        for path in &fresh {
            if !self.known_paths.contains(path) && !self.recent_paths.contains(path) {
                self.recent_paths.insert(0, path.clone());
            }
        }
        self.recent_paths.retain(|path| fresh.contains(path));
        self.known_paths = fresh;
        // Keep the selection on the same entry after the list is rebuilt.
        let selected_path = self
            .selected
            .and_then(|idx| self.entries.get(idx))
            .map(|entry| entry.path.clone());
        self.refresh_entries();
        self.selected = selected_path
            .and_then(|path| self.entries.iter().position(|entry| entry.path == path));
        self.status = "Folder updated".into();
        true
    }

    fn navigate(&mut self, path: &Path) {
        if !path.is_dir() {
            return;
        }
        self.cwd = path.to_path_buf();
        self.reset_dir_watch();
        self.refresh_entries();
        self.selected = None;
        self.scroll = 0;
        self.file_menu = None;
        self.sort_open = false;
        self.folder_tabs_open = false;
        self.more_open = false;
        self.viewer_close();
        if !self.suppress_terminal_navigation {
            self.terminal_cd(&path.to_path_buf());
        }
        if let Some(tab) = self.folder_tabs.get_mut(self.active_folder_tab) {
            tab.path = path.to_path_buf();
        }
        if self
            .folder_tabs
            .get(self.active_folder_tab)
            .is_some_and(|tab| tab.role == FolderTabRole::Folder)
        {
            self.workspace_tabs
                .insert(self.current_workspace, self.active_folder_tab);
        }
        self.status.clear();
    }

    fn open_entry(&mut self, idx: usize) {
        let Some(entry) = self.entries.get(idx).cloned() else {
            return;
        };
        if entry.kind == FileKind::Directory {
            self.navigate(&entry.path);
        } else {
            self.open_file_by_path(&entry.path);
        }
    }

    fn preview_entry(&mut self, idx: usize) {
        let Some(entry) = self.entries.get(idx).cloned() else {
            return;
        };
        if entry.kind == FileKind::Directory {
            return;
        }
        let kind = file_kind_for(&entry.path);
        match kind {
            FileKind::Image => self.open_image_window(&entry.path, true),
            FileKind::Text => self.open_text_window(TextView::open(&entry.path), true),
            FileKind::Doc => {
                if let Ok(text) = extract_doc_text(&entry.path) {
                    self.open_text_window(TextView::from_text(&entry.path, text), true);
                }
            }
            FileKind::Other if std::fs::read_to_string(&entry.path).is_ok() => {
                self.open_text_window(TextView::open(&entry.path), true);
            }
            _ => {}
        }
    }

    fn open_file_by_path(&mut self, path: &Path) {
        let (_, _, cw, ch) = self.content_rect();
        let kind = file_kind_for(path);
        if !self.viewer_only {
            match kind {
                FileKind::Image => {
                    self.viewer_close();
                    self.open_image_window(path, false);
                    return;
                }
                FileKind::Text => {
                    self.viewer_close();
                    self.open_text_window(TextView::open(path), false);
                    return;
                }
                FileKind::Doc => {
                    self.viewer_close();
                    match extract_doc_text(path) {
                        Ok(text) => self.open_text_window(TextView::from_text(path, text), false),
                        Err(err) => self.viewer = Some(Viewer::Info(err)),
                    }
                    return;
                }
                FileKind::Other => {
                    // Unknown kinds that read as UTF-8 open in the text window.
                    if std::fs::read_to_string(path).is_ok() {
                        self.viewer_close();
                        self.open_text_window(TextView::open(path), false);
                        return;
                    }
                }
                _ => {}
            }
        }
        self.viewer_close();
        let viewer = match kind {
            FileKind::Text => Viewer::Text(TextView::open(path)),
            FileKind::Image => Viewer::Image(ImageView::open(
                path,
                (cw - 40).max(50) as u32,
                (ch - 70).max(50) as u32,
            )),
            FileKind::Pdf => Viewer::Pdf(PdfView::open(
                path,
                (cw - 40).max(50) as u32,
                (ch - 70).max(50) as u32,
            )),
            FileKind::Doc => match extract_doc_text(path) {
                Ok(text) => Viewer::Text(TextView::from_text(path, text)),
                Err(err) => Viewer::Info(err),
            },
            FileKind::Model3d => Viewer::Model(ModelView::open(path)),
            FileKind::Audio | FileKind::Video => {
                self.position_media_embed();
                let view = MediaView::open(path, kind, self.media_embed, &self.display);
                if view.embedded && kind == FileKind::Video {
                    let _ = self.conn.map_window(self.media_embed);
                }
                Viewer::Media(view)
            }
            _ => {
                // Try opening as UTF-8 text; otherwise offer xdg-open.
                match std::fs::read_to_string(path) {
                    Ok(_) => Viewer::Text(TextView::open(path)),
                    Err(_) => {
                        let mut cmd = Command::new("xdg-open");
                        cmd.arg(path);
                        let _ = cmd.spawn();
                        self.status = "Opened with system handler".into();
                        return;
                    }
                }
            }
        };
        self.viewer = Some(viewer);
        self.focus = match self.viewer {
            Some(Viewer::Text(_)) => Focus::Editor,
            _ => Focus::Files,
        };
    }

    // ------------------------------------------------------------ image window

    /// Normal geometry for a side view window: to the right of the file list.
    fn side_window_normal_geometry(&self) -> AnyResult<(i16, i16, u16, u16)> {
        let abs = self
            .conn
            .translate_coordinates(self.window, self.root, 0, 0)?
            .reply()?;
        let sw = i32::from(self.screen_w);
        let sh = i32::from(self.screen_h);
        let want_x = i32::from(abs.dst_x) + i32::from(self.width) + 8;
        let w = (sw - want_x - 12).clamp(340, 900) as u16;
        let x = want_x.min(sw - i32::from(w) - 8).max(0) as i16;
        let h = (i32::from(self.height)).min(sh - 20).max(300) as u16;
        let y = abs.dst_y.max(10);
        Ok((x, y, w, h))
    }

    /// Compact first-click preview: half the normal viewer dimensions.
    fn side_window_geometry(&self) -> AnyResult<(i16, i16, u16, u16)> {
        let (x, y, w, h) = self.side_window_normal_geometry()?;
        Ok((x, y, (w / 2).max(300), (h / 2).max(220)))
    }

    fn view_window_geometry(&self, compact: bool) -> AnyResult<(i16, i16, u16, u16)> {
        if compact {
            self.side_window_geometry()
        } else {
            self.side_window_normal_geometry()
        }
    }

    /// Target geometry when maximizing a side view window: 80% of the screen,
    /// centered, kept below the desktop top bar so the window title bar (and
    /// its close button) always stays visible and clickable.
    fn side_window_max_geometry(&self) -> (i32, i32, u32, u32) {
        let sw = i32::from(self.screen_w);
        let sh = i32::from(self.screen_h);
        let w = (sw * 4 / 5).clamp(320.min(sw), sw) as u32;
        let h = (sh * 4 / 5).clamp(240.min(sh), sh) as u32;
        let x = ((sw - w as i32) / 2).max(0);
        let y = ((sh - h as i32) / 2).max(48);
        (x, y, w, h)
    }

    /// Open or replace an image while preserving the existing viewer surface.
    fn open_image_window(&mut self, path: &Path, compact: bool) {
        let Ok((x, y, w, h)) = self.view_window_geometry(compact) else {
            self.status = "Could not position image window".into();
            return;
        };
        let mut state = ImgState::open(path);
        state.fit(i32::from(w), i32::from(h));
        if let Some(win) = self.img_win.as_mut() {
            if win.state.path == path {
                win.state.fit(i32::from(w), i32::from(h));
            } else {
                win.state = state;
            }
            win.maximized = false;
            win.compact = compact;
            win.width = w;
            win.height = h;
            win.dirty = true;
            let _ = self.conn.configure_window(
                win.window,
                &ConfigureWindowAux::new()
                    .x(i32::from(x))
                    .y(i32::from(y))
                    .width(u32::from(w))
                    .height(u32::from(h)),
            );
            let _ = self.conn.map_window(win.window);
            let focus = if compact { self.window } else { win.window };
            let _ = self.conn.set_input_focus(InputFocus::POINTER_ROOT, focus, x11rb::CURRENT_TIME);
            let _ = self.conn.flush();
            return;
        }
        if let Some(old) = self.txt_win.take() {
            let _ = self.conn.change_property8(
                PropMode::REPLACE,
                old.window,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                b"Aurora Image",
            );
            let _ = self.conn.configure_window(
                old.window,
                &ConfigureWindowAux::new()
                    .x(i32::from(x))
                    .y(i32::from(y))
                    .width(u32::from(w))
                    .height(u32::from(h)),
            );
            self.img_win = Some(ImgWin {
                window: old.window,
                gc: old.gc,
                width: w,
                height: h,
                maximized: false,
                compact,
                state,
                dirty: true,
            });
            let focus = if compact { self.window } else { old.window };
            let _ = self.conn.set_input_focus(InputFocus::POINTER_ROOT, focus, x11rb::CURRENT_TIME);
            let _ = self.conn.flush();
            return;
        }
        match self.create_image_window(state, compact) {
            Ok(win) => {
                let focus = if compact { self.window } else { win.window };
                let _ = self.conn.set_input_focus(
                    InputFocus::POINTER_ROOT,
                    focus,
                    x11rb::CURRENT_TIME,
                );
                self.img_win = Some(win);
            }
            Err(_) => self.status = "Could not open image window".into(),
        }
    }

    fn create_image_window(&mut self, mut state: ImgState, compact: bool) -> AnyResult<ImgWin> {
        let (x, y, w, h) = self.view_window_geometry(compact)?;
        let window = self.conn.generate_id()?;
        self.conn.create_window(
            self.depth,
            window,
            self.root,
            x,
            y,
            w,
            h,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new()
                .background_pixel(0x152742)
                .event_mask(
                    EventMask::EXPOSURE
                        | EventMask::BUTTON_PRESS
                        | EventMask::BUTTON_RELEASE
                        | EventMask::POINTER_MOTION
                        | EventMask::KEY_PRESS
                        | EventMask::STRUCTURE_NOTIFY,
                ),
        )?;
        self.conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"Aurora Image",
        )?;
        self.conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"aurora-files\0Aurora Files\0",
        )?;
        self.conn.change_property32(
            PropMode::REPLACE,
            window,
            self.wm_protocols,
            AtomEnum::ATOM,
            &[self.wm_delete],
        )?;
        let gc = self.conn.generate_id()?;
        self.conn
            .create_gc(gc, window, &CreateGCAux::new().graphics_exposures(0))?;
        self.conn.map_window(window)?;
        self.conn.flush()?;
        state.fit(i32::from(w), i32::from(h));
        Ok(ImgWin {
            window,
            gc,
            width: w,
            height: h,
            maximized: false,
            compact,
            state,
            dirty: true,
        })
    }

    fn close_image_window(&mut self) {
        if let Some(win) = self.img_win.take() {
            let _ = self.conn.destroy_window(win.window);
            let _ = self.conn.free_gc(win.gc);
            let _ = self.conn.flush();
        }
    }

    fn toggle_img_maximize(&mut self) -> AnyResult<()> {
        if self.img_win.is_none() {
            return Ok(());
        }
        let restore = if self.img_win.as_ref().is_some_and(|win| win.compact) {
            self.side_window_geometry()?
        } else {
            self.side_window_normal_geometry()?
        };
        let target = self.side_window_max_geometry();
        let Some(win) = self.img_win.as_mut() else {
            return Ok(());
        };
        let aux = if win.maximized {
            // Restore: back to the default spot beside the file list.
            win.maximized = false;
            ConfigureWindowAux::new()
                .x(i32::from(restore.0))
                .y(i32::from(restore.1))
                .width(u32::from(restore.2))
                .height(u32::from(restore.3))
        } else {
            // Maximize: grow to 80% of the screen, centered below the top bar.
            win.maximized = true;
            ConfigureWindowAux::new()
                .x(target.0)
                .y(target.1)
                .width(target.2)
                .height(target.3)
        };
        self.conn.configure_window(win.window, &aux)?;
        // Refit once the new size arrives via ConfigureNotify.
        win.state.user_zoomed = false;
        win.dirty = true;
        self.conn.flush()?;
        Ok(())
    }

    /// Copy the current (possibly edited) image to the system clipboard.
    fn img_copy_image(&mut self) {
        let Some(win) = self.img_win.as_mut() else {
            return;
        };
        let tmp = std::env::temp_dir().join(format!(
            "aurora-files-clip-{}.png",
            std::process::id()
        ));
        let ok = win.state.img.save(&tmp).is_ok() && copy_image_to_system_clipboard(&tmp);
        win.state.status = if ok {
            "Image copied to clipboard".into()
        } else {
            "Copy failed (install xclip)".into()
        };
    }

    fn img_menu_action(&mut self, row: usize, w: i32, h: i32) {
        if row == 7 {
            self.img_copy_image();
            return;
        }
        let Some(win) = self.img_win.as_mut() else {
            return;
        };
        match row {
            0 => win.state.zoom_at(w / 2, (IMG_TOP_BAR + h) / 2, 1.25),
            1 => win.state.zoom_at(w / 2, (IMG_TOP_BAR + h) / 2, 0.8),
            2 => win.state.fit(w, h),
            3 => win.state.rotate90(w, h),
            4 => win.state.flip_horizontal(),
            5 => win.state.flip_vertical(),
            6 => {
                win.state.crop_mode = !win.state.crop_mode;
                win.state.crop_drag = None;
                win.state.status = if win.state.crop_mode {
                    "Crop: drag to select an area".into()
                } else {
                    "Crop cancelled".into()
                };
            }
            _ => {}
        }
    }

    fn on_img_click(&mut self, x: i32, y: i32, button: u8) {
        let Some((w, h, ctx_menu, menu_open, crop_mode)) = self.img_win.as_ref().map(|win| {
            (
                i32::from(win.width),
                i32::from(win.height),
                win.state.ctx_menu,
                win.state.menu_open,
                win.state.crop_mode,
            )
        }) else {
            return;
        };
        // Right-click context menu takes priority while open.
        if let Some((mx, my)) = ctx_menu {
            if let Some(win) = self.img_win.as_mut() {
                win.state.ctx_menu = None;
            }
            let (gx, gy, gw, gh) = img_ctx_geometry(mx, my, w, h);
            if button == 1 && x >= gx && x < gx + gw && y >= gy && y < gy + gh {
                let row = ((y - gy - 5) / 28).clamp(0, IMG_CTX_ITEMS.len() as i32 - 1);
                match row {
                    0 => self.img_copy_image(),
                    1 => {
                        if let Some(win) = self.img_win.as_mut() {
                            let text = win.state.path.to_string_lossy().into_owned();
                            win.state.status = if copy_text_to_system_clipboard(&text) {
                                "Path copied to clipboard".into()
                            } else {
                                "Copy failed (install xclip)".into()
                            };
                        }
                    }
                    _ => {
                        if let Some(win) = self.img_win.as_mut() {
                            win.state.fit(w, h);
                        }
                    }
                }
            }
            return;
        }
        // Dropdown menu.
        if menu_open {
            if let Some(win) = self.img_win.as_mut() {
                win.state.menu_open = false;
            }
            let gx = w - 196;
            let gy = IMG_TOP_BAR + 4;
            let gh = IMG_MENU_ITEMS.len() as i32 * 28 + 10;
            if button == 1 && x >= gx && x < gx + 188 && y >= gy && y < gy + gh {
                let row = ((y - gy - 5) / 28).clamp(0, IMG_MENU_ITEMS.len() as i32 - 1);
                self.img_menu_action(row as usize, w, h);
            }
            return;
        }
        // Toolbar buttons: [menu] [max] [close] at the top right.
        if y < IMG_TOP_BAR {
            if button != 1 {
                return;
            }
            if x >= w - 36 && x < w - 8 {
                self.close_image_window();
            } else if x >= w - 70 && x < w - 42 {
                let _ = self.toggle_img_maximize();
            } else if x >= w - 104 && x < w - 76 {
                if let Some(win) = self.img_win.as_mut() {
                    win.state.menu_open = true;
                }
            }
            return;
        }
        // Image area.
        let Some(win) = self.img_win.as_mut() else {
            return;
        };
        match button {
            4 => win.state.zoom_at(x, y, 1.15),
            5 => win.state.zoom_at(x, y, 1.0 / 1.15),
            3 => win.state.ctx_menu = Some((x, y)),
            1 => {
                if crop_mode {
                    win.state.crop_drag = Some((x, y, x, y));
                } else if win.state.overflows(w, h) {
                    // Zoomed in: left-drag pans the image.
                    win.state.panning = Some((x, y));
                } else {
                    // Arm a drag-out of the image file to another app.
                    let path = win.state.path.clone();
                    let src_win = win.window;
                    self.file_drag = Some(FileDrag {
                        path,
                        start_x: x,
                        start_y: y,
                        started: false,
                        target: 0,
                        target_ver: 0,
                        accepted: false,
                        src_win,
                    });
                }
            }
            _ => {}
        }
    }

    /// Pointer motion inside the image window (crop rubber band / panning).
    fn on_img_motion(&mut self, x: i32, y: i32) {
        let Some(win) = self.img_win.as_mut() else {
            return;
        };
        if let Some((sx, sy, _, _)) = win.state.crop_drag {
            win.state.crop_drag = Some((sx, sy, x, y));
            win.dirty = true;
        } else if let Some((lx, ly)) = win.state.panning {
            win.state.pan(x - lx, y - ly);
            win.state.panning = Some((x, y));
            win.dirty = true;
        }
    }

    /// Button release inside the image window: apply crop / stop panning.
    fn on_img_release(&mut self) {
        let Some(win) = self.img_win.as_mut() else {
            return;
        };
        if win.state.crop_drag.is_some() {
            let (w, h) = (i32::from(win.width), i32::from(win.height));
            win.state.apply_crop(w, h);
            win.dirty = true;
        }
        if win.state.panning.take().is_some() {
            win.dirty = true;
        }
    }

    fn on_img_key(&mut self, ev: KeyPressEvent) -> AnyResult<()> {
        let mapping = self.conn.get_keyboard_mapping(ev.detail, 1)?.reply()?;
        let keysym = mapping.keysyms.first().copied().unwrap_or(0);
        let mut close = false;
        if let Some(win) = self.img_win.as_mut() {
            let (w, h) = (i32::from(win.width), i32::from(win.height));
            match keysym {
                0xff1b => {
                    if win.state.crop_mode || win.state.crop_drag.is_some() {
                        win.state.crop_mode = false;
                        win.state.crop_drag = None;
                        win.state.status = "Crop cancelled".into();
                    } else {
                        close = true;
                    }
                }
                0x2b | 0x3d => win.state.zoom_at(w / 2, (IMG_TOP_BAR + h) / 2, 1.25),
                0x2d => win.state.zoom_at(w / 2, (IMG_TOP_BAR + h) / 2, 0.8),
                0x72 => win.state.rotate90(w, h),
                0x30 => win.state.fit(w, h),
                _ => {}
            }
            win.dirty = true;
        }
        if close {
            self.close_image_window();
        }
        Ok(())
    }

    fn draw_image_window(&mut self) -> AnyResult<()> {
        let Some(win) = self.img_win.as_ref() else {
            return Ok(());
        };
        let mut c = Canvas::new(win.width, win.height, PAPER);
        let w = i32::from(win.width);
        let h = i32::from(win.height);
        win.state.render(&mut c);
        // Toolbar
        c.draw_rect(0, 0, w, IMG_TOP_BAR, Color::rgb(236, 245, 250));
        c.draw_rect(0, IMG_TOP_BAR - 1, w, 1, Color::rgba(176, 198, 210, 120));
        let name = win
            .state
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let label = if let Some(err) = &win.state.error {
            format!("{name}  -  {err}")
        } else {
            format!(
                "{name}  -  {} x {} px  -  {}%",
                win.state.img.width(),
                win.state.img.height(),
                (win.state.zoom * 100.0).round() as i32
            )
        };
        c.draw_text(
            &self.bold,
            &compact(&label, ((w - 130) / 7).max(8) as usize),
            14,
            11,
            13.0,
            INK,
        );
        // Menu button (3 dots)
        c.draw_round_rect(w - 104, 6, 28, 28, 9, CARD);
        self.draw_more_icon(&mut c, w - 90, 20);
        // Maximize button
        c.draw_round_rect(w - 70, 6, 28, 28, 9, CARD);
        c.draw_rect(w - 63, 13, 14, 2, MINT_DARK);
        c.draw_rect(w - 63, 25, 14, 2, MINT_DARK);
        c.draw_rect(w - 63, 13, 2, 14, MINT_DARK);
        c.draw_rect(w - 51, 13, 2, 14, MINT_DARK);
        // Close button
        c.draw_round_rect(w - 36, 6, 28, 28, 14, Color::rgba(226, 92, 101, 220));
        c.draw_line(w - 27, 15, w - 17, 25, 2, Color::rgb(255, 255, 255));
        c.draw_line(w - 17, 15, w - 27, 25, 2, Color::rgb(255, 255, 255));
        // Status chip at the bottom left.
        if !win.state.status.is_empty() {
            let chip_w = measure_text(&self.regular, &win.state.status, 11.0) + 22;
            c.draw_round_rect(10, h - 34, chip_w, 24, 8, Color::rgba(250, 254, 255, 225));
            c.draw_text(&self.regular, &win.state.status, 21, h - 30, 11.0, INK);
        }
        // Dropdown menu
        if win.state.menu_open {
            let gx = w - 196;
            let gy = IMG_TOP_BAR + 4;
            let gh = IMG_MENU_ITEMS.len() as i32 * 28 + 10;
            c.draw_round_rect(gx, gy, 188, gh, 10, Color::rgb(250, 254, 255));
            c.draw_round_rect(gx, gy, 188, gh, 10, Color::rgba(176, 198, 210, 90));
            for (idx, item) in IMG_MENU_ITEMS.iter().enumerate() {
                let iy = gy + 5 + idx as i32 * 28;
                if idx == 6 && win.state.crop_mode {
                    c.draw_round_rect(gx + 5, iy, 178, 26, 7, Color::rgba(116, 213, 198, 92));
                }
                c.draw_text(&self.regular, item, gx + 16, iy + 6, 13.0, INK);
            }
        }
        // Right-click context menu
        if let Some((mx, my)) = win.state.ctx_menu {
            let (gx, gy, gw, gh) = img_ctx_geometry(mx, my, w, h);
            c.draw_round_rect(gx, gy, gw, gh, 10, Color::rgb(250, 254, 255));
            c.draw_round_rect(gx, gy, gw, gh, 10, Color::rgba(176, 198, 210, 90));
            for (idx, item) in IMG_CTX_ITEMS.iter().enumerate() {
                let iy = gy + 5 + idx as i32 * 28;
                c.draw_text(&self.regular, item, gx + 16, iy + 6, 13.0, INK);
            }
        }
        let xi = Image::new(
            c.width,
            c.height,
            ScanlinePad::Pad32,
            self.depth,
            BitsPerPixel::B32,
            ImageOrder::LsbFirst,
            Cow::Borrowed(&c.data),
        )?;
        xi.put(&self.conn, win.window, win.gc, 0, 0)?;
        Ok(())
    }

    // ------------------------------------------------------------ text window

    /// Open or replace text while preserving the existing viewer surface.
    fn open_text_window(&mut self, text: TextView, compact: bool) {
        let Ok((x, y, w, h)) = self.view_window_geometry(compact) else {
            self.status = "Could not position text window".into();
            return;
        };
        if let Some(win) = self.txt_win.as_mut() {
            if win.text.path != text.path {
                win.text = text;
            }
            win.maximized = false;
            win.compact = compact;
            win.selecting = false;
            win.width = w;
            win.height = h;
            win.dirty = true;
            let _ = self.conn.configure_window(
                win.window,
                &ConfigureWindowAux::new()
                    .x(i32::from(x))
                    .y(i32::from(y))
                    .width(u32::from(w))
                    .height(u32::from(h)),
            );
            let _ = self.conn.map_window(win.window);
            let focus = if compact { self.window } else { win.window };
            let _ = self.conn.set_input_focus(InputFocus::POINTER_ROOT, focus, x11rb::CURRENT_TIME);
            let _ = self.conn.flush();
            return;
        }
        if let Some(old) = self.img_win.take() {
            let _ = self.conn.change_property8(
                PropMode::REPLACE,
                old.window,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                b"Aurora Text",
            );
            let _ = self.conn.configure_window(
                old.window,
                &ConfigureWindowAux::new()
                    .x(i32::from(x))
                    .y(i32::from(y))
                    .width(u32::from(w))
                    .height(u32::from(h)),
            );
            self.txt_win = Some(TxtWin {
                window: old.window,
                gc: old.gc,
                width: w,
                height: h,
                maximized: false,
                compact,
                selecting: false,
                text,
                dirty: true,
            });
            let focus = if compact { self.window } else { old.window };
            let _ = self.conn.set_input_focus(InputFocus::POINTER_ROOT, focus, x11rb::CURRENT_TIME);
            let _ = self.conn.flush();
            return;
        }
        match self.create_text_window(text, compact) {
            Ok(win) => {
                let focus = if compact { self.window } else { win.window };
                let _ = self.conn.set_input_focus(
                    InputFocus::POINTER_ROOT,
                    focus,
                    x11rb::CURRENT_TIME,
                );
                let _ = self.conn.flush();
                self.txt_win = Some(win);
            }
            Err(_) => self.status = "Could not open text window".into(),
        }
    }

    fn create_text_window(&mut self, text: TextView, compact: bool) -> AnyResult<TxtWin> {
        let (x, y, w, h) = self.view_window_geometry(compact)?;
        let window = self.conn.generate_id()?;
        self.conn.create_window(
            self.depth,
            window,
            self.root,
            x,
            y,
            w,
            h,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new()
                .background_pixel(0xF7FCFF)
                .event_mask(
                    EventMask::EXPOSURE
                        | EventMask::BUTTON_PRESS
                        | EventMask::BUTTON_RELEASE
                        | EventMask::POINTER_MOTION
                        | EventMask::KEY_PRESS
                        | EventMask::STRUCTURE_NOTIFY,
                ),
        )?;
        self.conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"Aurora Text",
        )?;
        self.conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"aurora-files\0Aurora Files\0",
        )?;
        self.conn.change_property32(
            PropMode::REPLACE,
            window,
            self.wm_protocols,
            AtomEnum::ATOM,
            &[self.wm_delete],
        )?;
        let gc = self.conn.generate_id()?;
        self.conn
            .create_gc(gc, window, &CreateGCAux::new().graphics_exposures(0))?;
        self.conn.map_window(window)?;
        self.conn.flush()?;
        Ok(TxtWin {
            window,
            gc,
            width: w,
            height: h,
            maximized: false,
            compact,
            selecting: false,
            text,
            dirty: true,
        })
    }

    fn close_text_window(&mut self) {
        if let Some(win) = self.txt_win.take() {
            let _ = self.conn.destroy_window(win.window);
            let _ = self.conn.free_gc(win.gc);
            // Hand keyboard focus back to the main window.
            let _ = self.conn.set_input_focus(
                InputFocus::POINTER_ROOT,
                self.window,
                x11rb::CURRENT_TIME,
            );
            let _ = self.conn.flush();
        }
    }

    fn toggle_txt_maximize(&mut self) -> AnyResult<()> {
        if self.txt_win.is_none() {
            return Ok(());
        }
        let restore = if self.txt_win.as_ref().is_some_and(|win| win.compact) {
            self.side_window_geometry()?
        } else {
            self.side_window_normal_geometry()?
        };
        let target = self.side_window_max_geometry();
        let Some(win) = self.txt_win.as_mut() else {
            return Ok(());
        };
        let aux = if win.maximized {
            // Restore: back to the default spot beside the file list.
            win.maximized = false;
            ConfigureWindowAux::new()
                .x(i32::from(restore.0))
                .y(i32::from(restore.1))
                .width(u32::from(restore.2))
                .height(u32::from(restore.3))
        } else {
            // Maximize: grow to 80% of the screen, centered below the top bar.
            win.maximized = true;
            ConfigureWindowAux::new()
                .x(target.0)
                .y(target.1)
                .width(target.2)
                .height(target.3)
        };
        self.conn.configure_window(win.window, &aux)?;
        win.dirty = true;
        self.conn.flush()?;
        Ok(())
    }

    /// Number of text lines visible in the text window.
    fn txt_visible_lines(height: u16) -> usize {
        ((i32::from(height) - TXT_TOP_BAR - 16) / TXT_LINE_H).max(1) as usize
    }

    fn on_txt_click(&mut self, x: i32, y: i32, button: u8) {
        let Some((w, editable)) = self
            .txt_win
            .as_ref()
            .map(|win| (i32::from(win.width), win.text.editable))
        else {
            return;
        };
        // Toolbar buttons: [Save] [max] [close] at the top right.
        if y < TXT_TOP_BAR {
            if button != 1 {
                return;
            }
            if x >= w - 36 && x < w - 8 {
                self.close_text_window();
            } else if x >= w - 70 && x < w - 42 {
                let _ = self.toggle_txt_maximize();
            } else if x >= w - 128 && x < w - 76 && editable {
                if let Some(win) = self.txt_win.as_mut() {
                    win.text.save();
                    win.dirty = true;
                }
            }
            return;
        }
        let Some(win) = self.txt_win.as_mut() else {
            return;
        };
        match button {
            4 => win.text.scroll = win.text.scroll.saturating_sub(3),
            5 => {
                win.text.scroll =
                    (win.text.scroll + 3).min(win.text.lines.len().saturating_sub(4));
            }
            1 => {
                // Place the cursor under the pointer.
                let line = (win.text.scroll as i32 + (y - TXT_TOP_BAR - 8) / TXT_LINE_H)
                    .max(0) as usize;
                if line < win.text.lines.len() {
                    let col = text_column_at_x(&self.mono, &win.text.lines[line], x - 14);
                    win.text.cursor = (line, col);
                    win.text.selection = Some(TextSelection {
                        start: (line, col),
                        end: (line, col),
                    });
                    win.selecting = true;
                }
            }
            _ => {}
        }
        win.dirty = true;
    }

    fn on_txt_motion(&mut self, x: i32, y: i32) {
        let Some(win) = self.txt_win.as_mut() else {
            return;
        };
        if !win.selecting || win.text.lines.is_empty() {
            return;
        }
        let line = (win.text.scroll as i32 + (y - TXT_TOP_BAR - 8) / TXT_LINE_H)
            .max(0) as usize;
        let line = line.min(win.text.lines.len() - 1);
        let col = text_column_at_x(&self.mono, &win.text.lines[line], x - 14);
        if let Some(selection) = win.text.selection.as_mut() {
            selection.end = (line, col);
        }
        win.text.cursor = (line, col);
        win.dirty = true;
    }

    fn on_txt_release(&mut self) {
        let Some(win) = self.txt_win.as_mut() else {
            return;
        };
        if !win.selecting {
            return;
        }
        win.selecting = false;
        if let Some(text) = win.text.selected_text().filter(|text| !text.is_empty()) {
            win.text.status = if copy_text_to_system_clipboard(&text) {
                "Selection copied".into()
            } else {
                "Copy failed (install xclip)".into()
            };
        }
        win.dirty = true;
    }

    fn on_txt_key(&mut self, ev: KeyPressEvent) -> AnyResult<()> {
        let mapping = self.conn.get_keyboard_mapping(ev.detail, 1)?.reply()?;
        let state = u16::from(ev.state);
        let shift = state & u16::from(KeyButMask::SHIFT) != 0;
        let ctrl = state & u16::from(KeyButMask::CONTROL) != 0;
        let column = usize::from(shift && mapping.keysyms_per_keycode > 1);
        let keysym = mapping
            .keysyms
            .get(column)
            .copied()
            .filter(|&k| k != 0)
            .or_else(|| mapping.keysyms.first().copied())
            .unwrap_or(0);
        let mut close = false;
        if let Some(win) = self.txt_win.as_mut() {
            let text = &mut win.text;
            match keysym {
                _ if ctrl && (keysym == 'c' as u32 || keysym == 'C' as u32) => {
                    if let Some(selected) = text.selected_text().filter(|value| !value.is_empty()) {
                        text.status = if copy_text_to_system_clipboard(&selected) {
                            "Selection copied".into()
                        } else {
                            "Copy failed (install xclip)".into()
                        };
                    }
                }
                _ if ctrl && (keysym == 's' as u32 || keysym == 'S' as u32) => text.save(),
                0xff0d => text.newline(),
                0xff08 => text.backspace(),
                0xff1b => close = true,
                0xff51 => text.move_cursor(-1, 0),
                0xff53 => text.move_cursor(1, 0),
                0xff52 => text.move_cursor(0, -1),
                0xff54 => text.move_cursor(0, 1),
                0xff50 => text.cursor.1 = 0,
                0xff57 => text.cursor.1 = text.lines[text.cursor.0].chars().count(),
                0xff55 => text.scroll = text.scroll.saturating_sub(20),
                0xff56 => {
                    text.scroll = (text.scroll + 20).min(text.lines.len().saturating_sub(4));
                }
                0x20..=0x7e if !ctrl => {
                    if let Some(ch) = char::from_u32(keysym) {
                        text.insert_char(ch);
                    }
                }
                _ => {}
            }
            // Keep the cursor on screen.
            let visible = Self::txt_visible_lines(win.height);
            let text = &mut win.text;
            if text.cursor.0 < text.scroll {
                text.scroll = text.cursor.0;
            } else if text.cursor.0 >= text.scroll + visible {
                text.scroll = text.cursor.0 + 1 - visible;
            }
            win.dirty = true;
        }
        if close {
            self.close_text_window();
        }
        Ok(())
    }

    fn draw_text_window(&mut self) -> AnyResult<()> {
        let Some(win) = self.txt_win.as_ref() else {
            return Ok(());
        };
        let w = i32::from(win.width);
        let h = i32::from(win.height);
        let mut c = Canvas::new(win.width, win.height, PAPER);
        let text = &win.text;
        // Text content below the toolbar.
        let visible = Self::txt_visible_lines(win.height);
        let max_chars = ((w - 28) / 8).max(8) as usize;
        for (row, line) in text.lines.iter().skip(text.scroll).take(visible).enumerate() {
            let y = TXT_TOP_BAR + 8 + row as i32 * TXT_LINE_H;
            let line_idx = text.scroll + row;
            if let Some((first, last)) = text.selection.and_then(|selection| {
                text_selection_columns(selection, line_idx, max_chars)
            }) {
                let first = first.min(max_chars);
                let last = last.min(max_chars);
                let x1 = 14 + text_prefix_width(&self.mono, line, first);
                let x2 = 14 + text_prefix_width(&self.mono, line, last);
                if last > first {
                    c.draw_rect(
                        x1,
                        y - 2,
                        (x2 - x1).max(2),
                        17,
                        Color::rgba(73, 156, 231, 105),
                    );
                }
            }
            if text.editable && line_idx == text.cursor.0 {
                c.draw_rect(8, y - 2, w - 16, TXT_LINE_H, Color::rgba(116, 213, 198, 40));
                if text.cursor.1 <= max_chars {
                    let cursor_x = 14 + text_prefix_width(&self.mono, line, text.cursor.1);
                    c.draw_rect(cursor_x, y - 2, 2, 17, MINT_DARK);
                }
            }
            c.draw_text(
                &self.mono,
                &compact(line, max_chars),
                14,
                y,
                TEXT_FONT_SIZE,
                INK,
            );
        }
        // Toolbar
        c.draw_rect(0, 0, w, TXT_TOP_BAR, Color::rgb(236, 245, 250));
        c.draw_rect(0, TXT_TOP_BAR - 1, w, 1, Color::rgba(176, 198, 210, 120));
        let name = text
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let label = format!(
            "{}{}  -  {} lines{}",
            if text.dirty { "* " } else { "" },
            name,
            text.lines.len(),
            if text.editable { "" } else { "  (read-only)" },
        );
        c.draw_text(
            &self.bold,
            &compact(&label, ((w - 150) / 7).max(8) as usize),
            14,
            11,
            13.0,
            INK,
        );
        // Save button
        if text.editable {
            c.draw_round_rect(w - 128, 6, 52, 28, 9, Color::rgba(116, 213, 198, 150));
            c.draw_text_center(&self.bold, "Save", w - 102, 13, 12.0, MINT_DARK);
        }
        // Maximize button
        c.draw_round_rect(w - 70, 6, 28, 28, 9, CARD);
        c.draw_rect(w - 63, 13, 14, 2, MINT_DARK);
        c.draw_rect(w - 63, 25, 14, 2, MINT_DARK);
        c.draw_rect(w - 63, 13, 2, 14, MINT_DARK);
        c.draw_rect(w - 51, 13, 2, 14, MINT_DARK);
        // Close button
        c.draw_round_rect(w - 36, 6, 28, 28, 14, Color::rgba(226, 92, 101, 220));
        c.draw_line(w - 27, 15, w - 17, 25, 2, Color::rgb(255, 255, 255));
        c.draw_line(w - 17, 15, w - 27, 25, 2, Color::rgb(255, 255, 255));
        // Status chip at the bottom left.
        if !text.status.is_empty() {
            let chip_w = measure_text(&self.regular, &text.status, 11.0) + 22;
            c.draw_round_rect(10, h - 34, chip_w, 24, 8, Color::rgba(250, 254, 255, 225));
            c.draw_text(&self.regular, &text.status, 21, h - 30, 11.0, INK);
        }
        let xi = Image::new(
            c.width,
            c.height,
            ScanlinePad::Pad32,
            self.depth,
            BitsPerPixel::B32,
            ImageOrder::LsbFirst,
            Cow::Borrowed(&c.data),
        )?;
        xi.put(&self.conn, win.window, win.gc, 0, 0)?;
        Ok(())
    }

    fn viewer_close(&mut self) {
        if let Some(Viewer::Media(view)) = self.viewer.as_mut() {
            view.stop();
            let _ = self.conn.unmap_window(self.media_embed);
        }
        self.viewer = None;
        self.focus = Focus::Files;
    }

    fn position_media_embed(&self) {
        let (cx, cy, cw, ch) = self.content_rect();
        let _ = self.conn.configure_window(
            self.media_embed,
            &ConfigureWindowAux::new()
                .x(cx + 16)
                .y(cy + 44)
                .width((cw - 32).max(32) as u32)
                .height((ch - 60).max(32) as u32),
        );
    }

    // ------------------------------------------------------------ terminal selection

    fn clear_terminal_selection(&mut self) {
        self.terminal_selection = None;
        self.terminal_selecting = false;
        self.terminal_copy_due = None;
    }

    fn terminal_cell_at(&self, x: i32, y: i32) -> Option<(usize, usize)> {
        let tab = self.tabs.get(self.active_tab)?;
        if tab.rows == 0 || tab.cols == 0 {
            return None;
        }
        let (_, ty, _, th) = self.terminal_rect();
        let origin_y = ty + TAB_BAR_H + 6;
        if y < origin_y || y >= ty + th {
            return None;
        }
        let row = ((y - origin_y) / CELL_H).clamp(0, tab.rows as i32 - 1) as usize;
        let col = ((x - 10) / CELL_W).clamp(0, tab.cols as i32 - 1) as usize;
        Some((row, col))
    }

    fn update_terminal_selection(&mut self, x: i32, y: i32) -> bool {
        if !self.terminal_selecting {
            return false;
        }
        let Some(cell) = self.terminal_cell_at(x, y) else {
            return false;
        };
        let Some(selection) = self.terminal_selection.as_mut() else {
            return false;
        };
        if selection.end == cell {
            return false;
        }
        selection.end = cell;
        self.terminal_copy_due = None;
        true
    }

    fn finish_terminal_selection(&mut self) {
        if !self.terminal_selecting {
            return;
        }
        self.terminal_selecting = false;
        if self
            .terminal_selected_text()
            .is_some_and(|text| text.chars().count() >= 3)
        {
            self.terminal_copy_due = Some(Instant::now() + TERMINAL_COPY_DELAY);
        } else {
            self.terminal_copy_due = None;
        }
    }

    fn terminal_cell_selected(&self, row: usize, col: usize) -> bool {
        let Some(selection) = self.terminal_selection else {
            return false;
        };
        if selection.tab != self.active_tab {
            return false;
        }
        let (start, end) = normalized_terminal_selection(selection);
        (row, col) >= start && (row, col) <= end
    }

    fn terminal_selected_text(&self) -> Option<String> {
        let selection = self.terminal_selection?;
        let tab = self.tabs.get(selection.tab)?;
        let (start, end) = normalized_terminal_selection(selection);
        let mut lines = Vec::new();
        for row in start.0..=end.0.min(tab.rows.saturating_sub(1)) {
            let cells = tab.display_row(row)?;
            let first = if row == start.0 { start.1 } else { 0 };
            let last = if row == end.0 {
                end.1.min(cells.len().saturating_sub(1))
            } else {
                cells.len().saturating_sub(1)
            };
            let line: String = if first <= last {
                cells[first..=last].iter().map(|cell| cell.ch).collect()
            } else {
                String::new()
            };
            lines.push(line.trim_end().to_string());
        }
        Some(lines.join("\n").trim_end().to_string())
    }

    fn copy_terminal_selection_if_due(&mut self) -> bool {
        let Some(due) = self.terminal_copy_due else {
            return false;
        };
        if Instant::now() < due {
            return false;
        }
        self.terminal_copy_due = None;
        let Some(text) = self
            .terminal_selected_text()
            .filter(|text| text.chars().count() >= 3)
        else {
            return false;
        };
        if copy_text_to_system_clipboard(&text) {
            self.terminal_notice = Some(("Copied to clipboard".into(), Instant::now()));
            return true;
        }
        false
    }

    // ------------------------------------------------------------ event loop

    fn event_loop(&mut self) -> AnyResult<()> {
        let mut last_draw = Instant::now();
        let mut needs_draw = true;
        loop {
            if self.close_at.is_some_and(|close_at| Instant::now() >= close_at) {
                self.viewer_close();
                return Ok(());
            }
            while let Some(event) = self.conn.poll_for_event()? {
                let img_window = self.img_win.as_ref().map(|w| w.window);
                let txt_window = self.txt_win.as_ref().map(|w| w.window);
                match event {
                    Event::Expose(ev) if ev.count == 0 => {
                        if Some(ev.window) == img_window {
                            if let Some(win) = self.img_win.as_mut() {
                                win.dirty = true;
                            }
                        } else if Some(ev.window) == txt_window {
                            if let Some(win) = self.txt_win.as_mut() {
                                win.dirty = true;
                            }
                        } else {
                            needs_draw = true;
                        }
                    }
                    Event::ConfigureNotify(ev) if Some(ev.window) == img_window => {
                        if let Some(win) = self.img_win.as_mut() {
                            if ev.width != win.width || ev.height != win.height {
                                win.width = ev.width;
                                win.height = ev.height;
                                if !win.state.user_zoomed {
                                    win.state
                                        .fit(i32::from(ev.width), i32::from(ev.height));
                                }
                                win.dirty = true;
                            }
                        }
                    }
                    Event::ConfigureNotify(ev) if Some(ev.window) == txt_window => {
                        if let Some(win) = self.txt_win.as_mut() {
                            if ev.width != win.width || ev.height != win.height {
                                win.width = ev.width;
                                win.height = ev.height;
                                win.dirty = true;
                            }
                        }
                    }
                    Event::ConfigureNotify(ev) if ev.window == self.window => {
                        if ev.width != self.width || ev.height != self.height {
                            self.width = ev.width;
                            self.height = ev.height;
                            if self.terminal_visible {
                                self.terminal_h = i32::from(self.height) / 2;
                            }
                            let (cols, rows) = self.term_grid_size();
                            for tab in &mut self.tabs {
                                tab.resize(cols, rows);
                            }
                            if matches!(self.viewer, Some(Viewer::Media(_))) {
                                self.position_media_embed();
                            }
                            needs_draw = true;
                        }
                    }
                    Event::ButtonPress(ev) => {
                        if Some(ev.event) == img_window {
                            self.on_img_click(
                                i32::from(ev.event_x),
                                i32::from(ev.event_y),
                                ev.detail,
                            );
                            if let Some(win) = self.img_win.as_mut() {
                                win.dirty = true;
                            }
                        } else if Some(ev.event) == txt_window {
                            self.on_txt_click(
                                i32::from(ev.event_x),
                                i32::from(ev.event_y),
                                ev.detail,
                            );
                        } else {
                            self.on_click(
                                i32::from(ev.event_x),
                                i32::from(ev.event_y),
                                ev.detail,
                                u16::from(ev.state),
                            );
                            needs_draw = true;
                        }
                    }
                    Event::ButtonRelease(ev) => {
                        if Some(ev.event) == txt_window {
                            self.on_txt_release();
                        }
                        let drag_started =
                            self.file_drag.as_ref().is_some_and(|drag| drag.started);
                        if self.file_drag.is_some() {
                            self.drag_out_release(ev.time)?;
                        }
                        if drag_started {
                            self.pending_open = None;
                        } else if let Some(action) = self.pending_open.take() {
                            match action {
                                PendingOpen::Preview(idx) => self.preview_entry(idx),
                                PendingOpen::Open(idx) => self.open_entry(idx),
                            }
                        }
                        self.on_img_release();
                        self.finish_terminal_selection();
                        if let Some(Viewer::Model(model)) = self.viewer.as_mut() {
                            model.dragging = None;
                        }
                        needs_draw = true;
                    }
                    Event::MotionNotify(ev) => {
                        if self.file_drag.is_some() {
                            self.drag_out_motion(
                                i32::from(ev.event_x),
                                i32::from(ev.event_y),
                                ev.root_x,
                                ev.root_y,
                                ev.time,
                            )?;
                        }
                        if Some(ev.event) == img_window {
                            self.on_img_motion(
                                i32::from(ev.event_x),
                                i32::from(ev.event_y),
                            );
                        } else if Some(ev.event) == txt_window {
                            self.on_txt_motion(
                                i32::from(ev.event_x),
                                i32::from(ev.event_y),
                            );
                        } else {
                            if self.update_terminal_selection(
                                i32::from(ev.event_x),
                                i32::from(ev.event_y),
                            ) {
                                needs_draw = true;
                            }
                            if let Some(Viewer::Model(model)) = self.viewer.as_mut() {
                                if let Some((sx, sy)) = model.dragging {
                                    model.yaw += f32::from(ev.event_x - sx) * 0.01;
                                    model.pitch += f32::from(ev.event_y - sy) * 0.01;
                                    model.dragging = Some((ev.event_x, ev.event_y));
                                    needs_draw = true;
                                }
                            }
                        }
                    }
                    Event::KeyPress(ev) => {
                        if Some(ev.event) == img_window {
                            self.on_img_key(ev)?;
                        } else if Some(ev.event) == txt_window {
                            self.on_txt_key(ev)?;
                        } else {
                            if self.on_key(ev)? {
                                return Ok(());
                            }
                            needs_draw = true;
                        }
                    }
                    Event::SelectionRequest(ev) => {
                        self.handle_selection_request(&ev)?;
                    }
                    Event::ClientMessage(ev) => {
                        if self.handle_xdnd_client_message(&ev) {
                            needs_draw = true;
                        } else if ev.type_ == self.open_screenshot_atom {
                            self.handle_screenshot_message();
                            needs_draw = true;
                        } else if ev.type_ == self.open_folder_tab_atom {
                            self.handle_folder_tab_message();
                            needs_draw = true;
                        } else if ev.data.as_data32()[0] == self.wm_delete {
                            if Some(ev.window) == img_window {
                                self.close_image_window();
                            } else if Some(ev.window) == txt_window {
                                self.close_text_window();
                            } else {
                                self.viewer_close();
                                return Ok(());
                            }
                        }
                    }
                    _ => {}
                }
            }
            // Poll terminals
            let mut term_changed = false;
            for tab in &mut self.tabs {
                if tab.poll() {
                    term_changed = true;
                }
            }
            for group in self.terminal_groups.values_mut() {
                for tab in &mut group.tabs {
                    let _ = tab.poll();
                }
            }
            if term_changed {
                needs_draw = true;
            }
            if self.sync_workspace_folder_tab() {
                needs_draw = true;
            }
            if self.sync_folder_to_active_terminal() {
                needs_draw = true;
            }
            if self.copy_terminal_selection_if_due() {
                needs_draw = true;
            }
            if self.poll_directory_refresh() {
                needs_draw = true;
            }
            if self
                .terminal_notice
                .as_ref()
                .is_some_and(|(_, shown_at)| shown_at.elapsed() >= TERMINAL_NOTICE_DURATION)
            {
                self.terminal_notice = None;
                needs_draw = true;
            }
            if needs_draw && last_draw.elapsed() >= Duration::from_millis(16) {
                self.draw()?;
                self.conn.flush()?;
                needs_draw = false;
                last_draw = Instant::now();
            }
            if self.img_win.as_ref().is_some_and(|win| win.dirty) {
                self.draw_image_window()?;
                self.conn.flush()?;
                if let Some(win) = self.img_win.as_mut() {
                    win.dirty = false;
                }
            }
            if self.txt_win.as_ref().is_some_and(|win| win.dirty) {
                self.draw_text_window()?;
                self.conn.flush()?;
                if let Some(win) = self.txt_win.as_mut() {
                    win.dirty = false;
                }
            }
            std::thread::sleep(Duration::from_millis(if term_changed { 2 } else { 9 }));
        }
    }

    // ------------------------------------------------------------ input

    fn on_click(&mut self, x: i32, y: i32, button: u8, _state: u16) {
        if self.folder_tabs_open {
            let menu_x = 132;
            let menu_y = 54;
            let menu_w = (i32::from(self.width) - menu_x - 20).min(320);
            let row = (y - menu_y - 8) / 32;
            if x >= menu_x
                && x <= menu_x + menu_w
                && y >= menu_y + 8
                && row >= 0
                && (row as usize) < self.folder_tabs.len()
            {
                self.select_folder_tab(row as usize);
                return;
            }
            let add_y = menu_y + 8 + self.folder_tabs.len() as i32 * 32;
            if x >= menu_x
                && x <= menu_x + menu_w
                && y >= add_y
                && y <= add_y + 30
            {
                self.add_folder_tab();
                return;
            }
            self.folder_tabs_open = false;
        }
        // File/folder context menu: consume clicks while it is open.
        if self.file_menu.is_some() && (button == 1 || button == 3) {
            let handled = self.handle_file_menu_click(x, y);
            self.file_menu = None;
            if handled {
                return;
            }
        }
        let (tx, ty, tw, th) = self.terminal_rect();
        let _ = (tx, tw);
        // Terminal area
        if self.terminal_visible && y >= ty && th > 0 {
            self.focus = Focus::Terminal;
            let rel_y = y - ty;
            if rel_y < TAB_BAR_H {
                self.clear_terminal_selection();
                // Tab bar: [tabs...] [+]   [hide]
                let tab_w = 148;
                let idx = (x - 8) / tab_w;
                if x >= i32::from(self.width) - 34 {
                    self.terminal_visible = false;
                    self.focus = Focus::Files;
                } else if (0..self.tabs.len() as i32).contains(&idx) {
                    let close_x = 8 + idx * tab_w + tab_w - 22;
                    if x >= close_x {
                        self.close_tab(idx as usize);
                    } else {
                        self.active_tab = idx as usize;
                    }
                } else if x >= 8 + self.tabs.len() as i32 * tab_w
                    && x <= 8 + self.tabs.len() as i32 * tab_w + 28
                {
                    self.new_tab();
                }
            } else if button == 4 || button == 5 {
                self.clear_terminal_selection();
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.scroll_view(if button == 4 { 3 } else { -3 });
                }
            } else if button == 1 {
                let mouse_enabled = self
                    .tabs
                    .get(self.active_tab)
                    .is_some_and(|tab| tab.mouse_enabled);
                if mouse_enabled {
                    if let Some((row, col)) = self.terminal_cell_at(x, y) {
                        let event = format!("\x1b[<0;{};{}M\x1b[<0;{};{}m", col + 1, row + 1, col + 1, row + 1);
                        if let Some(tab) = self.tabs.get(self.active_tab) {
                            tab.write_input(event.as_bytes());
                        }
                    }
                    self.clear_terminal_selection();
                } else if let Some(cell) = self.terminal_cell_at(x, y) {
                    self.terminal_selection = Some(TerminalSelection {
                        tab: self.active_tab,
                        start: cell,
                        end: cell,
                    });
                    self.terminal_selecting = true;
                    self.terminal_copy_due = None;
                    self.terminal_notice = None;
                }
            }
            return;
        }
        if self.sort_open {
            if x >= 94 && x <= 216 && y >= 54 && y <= 150 {
                let idx = ((y - 62) / 28).clamp(0, 2);
                self.sort_mode = [SortMode::Name, SortMode::Date, SortMode::Size][idx as usize];
                self.sort_open = false;
                self.refresh_entries();
                self.selected = None;
                self.scroll = 0;
                self.status = format!("Sorted by {}", self.sort_mode.label().to_lowercase());
                return;
            }
            self.sort_open = false;
        }
        if self.more_open {
            let menu_x = i32::from(self.width) - 234;
            let menu_y = 54;
            let item_count = self.more_menu_items().len() as i32;
            let row = (y - menu_y - 36) / 30;
            if x >= menu_x + 8
                && x <= menu_x + 206
                && y >= menu_y + 36
                && row >= 0
                && row < item_count
            {
                self.more_open = false;
                self.more_menu_action(row as usize);
                return;
            }
            self.more_open = false;
        }
        // Header
        if y < HEADER_H {
            if (18..=48).contains(&x) && (18..=48).contains(&y) {
                self.navigate(&home_dir());
            } else if (56..=86).contains(&x) && (18..=48).contains(&y) {
                self.terminal_visible = !self.terminal_visible;
                if self.terminal_visible {
                    self.terminal_h = i32::from(self.height) / 2;
                }
                let (cols, rows) = self.term_grid_size();
                for tab in &mut self.tabs {
                    tab.resize(cols, rows);
                }
            } else if (94..=124).contains(&x) && (18..=48).contains(&y) {
                self.sort_open = !self.sort_open;
                self.folder_tabs_open = false;
                self.more_open = false;
            } else if (132..=162).contains(&x) && (18..=48).contains(&y) {
                self.folder_tabs_open = !self.folder_tabs_open;
                self.sort_open = false;
                self.more_open = false;
            } else if x >= i32::from(self.width) - 50
                && x <= i32::from(self.width) - 20
                && (18..=48).contains(&y)
            {
                self.more_open = !self.more_open;
                self.sort_open = false;
                self.folder_tabs_open = false;
            }
            return;
        }
        // Sidebar
        if x < SIDEBAR_W {
            if button == 4 {
                self.scroll = self.scroll.saturating_sub(3);
                return;
            }
            if button == 5 {
                let max = self.entries.len().saturating_sub(self.visible_rows());
                self.scroll = (self.scroll + 3).min(max);
                return;
            }
            if button != 1 {
                return;
            }
            let idx = ((y - HEADER_H - 10) / 32) as usize;
            if let Some(place) = self.places.get(idx) {
                let path = place.path.clone();
                if path.is_dir() {
                    self.navigate(&path);
                } else {
                    self.status = "Place not available".into();
                }
            }
            self.focus = Focus::Files;
            return;
        }
        // Image context menu (right-click): handle its own clicks first.
        if let Some((mx, my)) = self.image_menu {
            if button == 1 || button == 3 {
                let handled = self.handle_image_menu_click(mx, my, x, y);
                self.image_menu = None;
                if handled {
                    return;
                }
            }
        }
        // Viewer interactions
        let (cx, cy, cw, _ch) = self.content_rect();
        // Viewer close (X) button.
        if self.viewer.is_some() && button == 1 {
            let (bx, by, bw, bh) = self.viewer_close_button_rect();
            if x >= bx && x < bx + bw && y >= by && y < by + bh {
                self.viewer_close();
                return;
            }
        }
        // Right-click on an image opens the context menu (copy / zoom).
        if button == 3 && matches!(self.viewer, Some(Viewer::Image(_))) {
            self.image_menu = Some((x, y));
            return;
        }
        // Left press on the image arms a potential drag-out to another app; it only
        // becomes a real drag once the pointer moves past a small threshold.
        if button == 1 {
            if let Some(Viewer::Image(img)) = &self.viewer {
                self.file_drag = Some(FileDrag {
                    path: img.path.clone(),
                    start_x: x,
                    start_y: y,
                    started: false,
                    target: 0,
                    target_ver: 0,
                    accepted: false,
                    src_win: self.window,
                });
            }
        }
        if let Some(viewer) = self.viewer.as_mut() {
            match viewer {
                Viewer::Pdf(pdf) => {
                    if y < cy + 40 {
                        if x >= cx + cw - 170 && x < cx + cw - 120 {
                            if pdf.page > 1 {
                                pdf.page -= 1;
                                let (w, h) = ((cw - 40).max(50) as u32, 600);
                                pdf.render(w, h);
                            }
                        } else if x >= cx + cw - 110 && x < cx + cw - 60 && pdf.page < pdf.pages {
                            pdf.page += 1;
                            let (w, h) = ((cw - 40).max(50) as u32, 600);
                            pdf.render(w, h);
                        }
                    }
                }
                Viewer::Model(model) => {
                    if button == 1 {
                        model.dragging = Some((x as i16, y as i16));
                    }
                }
                Viewer::Text(text) => {
                    self.focus = Focus::Editor;
                    let line = (text.scroll as i32 + (y - cy - 44) / 18).max(0) as usize;
                    if line < text.lines.len() {
                        let col =
                            text_column_at_x(&self.mono, &text.lines[line], x - cx - 18);
                        text.cursor = (line, col);
                    }
                }
                Viewer::Media(media) => {
                    if y < cy + 40 && x >= cx + cw - 110 {
                        media.stop();
                    }
                }
                _ => {}
            }
            // Scroll wheel in viewers
            if button == 4 || button == 5 {
                match viewer {
                    Viewer::Text(text) => {
                        if button == 4 {
                            text.scroll = text.scroll.saturating_sub(3);
                        } else {
                            text.scroll =
                                (text.scroll + 3).min(text.lines.len().saturating_sub(4));
                        }
                    }
                    Viewer::Pdf(pdf) => {
                        let (w, h) = ((cw - 40).max(50) as u32, 600);
                        if button == 4 && pdf.page > 1 {
                            pdf.page -= 1;
                            pdf.render(w, h);
                        } else if button == 5 && pdf.page < pdf.pages {
                            pdf.page += 1;
                            pdf.render(w, h);
                        }
                    }
                    _ => {}
                }
            }
            return;
        }
        // File list
        self.focus = Focus::Files;
        if button == 4 {
            self.scroll = self.scroll.saturating_sub(3);
            return;
        }
        if button == 5 {
            let max = self.entries.len().saturating_sub(self.visible_rows());
            self.scroll = (self.scroll + 3).min(max);
            return;
        }
        let (_, cy, _, _) = self.content_rect();
        let row = (y - cy - 4) / ROW_H;
        if row >= 0 {
            let idx = self.scroll + row as usize;
            if idx < self.entries.len() {
                // Right click: open the context menu for this entry.
                if button == 3 {
                    self.selected = Some(idx);
                    if let Some(entry) = self.entries.get(idx) {
                        self.file_menu = Some((x, y, entry.path.clone()));
                    }
                    return;
                }
                if button != 1 {
                    return;
                }
                let same_entry = self.selected == Some(idx);
                let is_directory = self.entries[idx].kind == FileKind::Directory;
                let path = &self.entries[idx].path;
                let current_view_is_normal = self
                    .img_win
                    .as_ref()
                    .is_some_and(|win| win.state.path == *path && !win.compact)
                    || self
                        .txt_win
                        .as_ref()
                        .is_some_and(|win| win.text.path == *path && !win.compact);
                // First click selects and (for text/images) shows a compact
                // preview. Repeated clicks toggle compact and normal sizes.
                self.pending_open = Some(if same_entry && current_view_is_normal {
                    PendingOpen::Preview(idx)
                } else if same_entry {
                    PendingOpen::Open(idx)
                } else if is_directory {
                    // Directories have no preview; the first click only selects.
                    self.pending_open = None;
                    self.last_click = Some((idx, Instant::now()));
                    self.selected = Some(idx);
                    return;
                } else {
                    PendingOpen::Preview(idx)
                });
                self.last_click = Some((idx, Instant::now()));
                self.selected = Some(idx);
                // Arm a potential drag of this entry to another application.
                if let Some(entry) = self.entries.get(idx) {
                    self.file_drag = Some(FileDrag {
                        path: entry.path.clone(),
                        start_x: x,
                        start_y: y,
                        started: false,
                        target: 0,
                        target_ver: 0,
                        accepted: false,
                        src_win: self.window,
                    });
                }
            }
        }
    }

    /// Returns Ok(true) to quit.
    fn on_key(&mut self, ev: KeyPressEvent) -> AnyResult<bool> {
        let mapping = self.conn.get_keyboard_mapping(ev.detail, 1)?.reply()?;
        let state = u16::from(ev.state);
        let shift = state & u16::from(KeyButMask::SHIFT) != 0;
        let ctrl = state & u16::from(KeyButMask::CONTROL) != 0;
        let super_key = state & u16::from(KeyButMask::MOD4) != 0;
        let column = usize::from(shift && mapping.keysyms_per_keycode > 1);
        let keysym = mapping
            .keysyms
            .get(column)
            .copied()
            .filter(|&k| k != 0)
            .or_else(|| mapping.keysyms.first().copied())
            .unwrap_or(0);

        match self.focus {
            Focus::Terminal => {
                if self.tabs.get(self.active_tab).is_some() {
                    if ctrl && (keysym == 't' as u32 || keysym == 'T' as u32) && shift {
                        self.new_tab();
                        return Ok(false);
                    }
                    // Clipboard paste is deliberately limited to Ctrl+Shift+V
                    // (and Super+V). Plain Ctrl+V must fall through and be sent
                    // to the PTY as 0x16 for standard shell quoted-insert.
                    let paste_shortcut =
                        matches!(keysym, 0x76 | 0x56) && ((ctrl && shift) || super_key);
                    if paste_shortcut {
                        if let Some(text) = read_text_from_system_clipboard() {
                            self.clear_terminal_selection();
                            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                                tab.scroll_to_bottom();
                                if tab.bracketed_paste {
                                    tab.write_input(b"\x1b[200~");
                                    tab.write_input(text.as_bytes());
                                    tab.write_input(b"\x1b[201~");
                                } else {
                                    tab.write_input(text.as_bytes());
                                }
                            }
                        }
                        return Ok(false);
                    }
                    if ctrl && shift && matches!(keysym, 0x63 | 0x43) {
                        if let Some(text) = self
                            .terminal_selected_text()
                            .filter(|value| !value.is_empty())
                        {
                            if copy_text_to_system_clipboard(&text) {
                                self.terminal_notice =
                                    Some(("Copied to clipboard".into(), Instant::now()));
                            }
                        }
                        return Ok(false);
                    }
                    if shift && keysym == 0xff55 {
                        self.clear_terminal_selection();
                        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                            tab.scroll_view(tab.rows.saturating_sub(1) as i32);
                        }
                        return Ok(false);
                    }
                    if shift && keysym == 0xff56 {
                        self.clear_terminal_selection();
                        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                            tab.scroll_view(-(tab.rows.saturating_sub(1) as i32));
                        }
                        return Ok(false);
                    }
                    let bytes: Vec<u8> = match keysym {
                        0xff0d => vec![b'\r'],
                        0xff08 => vec![0x7f],
                        0xff09 => vec![b'\t'],
                        0xff1b => vec![0x1b],
                        0xff52 => b"\x1b[A".to_vec(),
                        0xff54 => b"\x1b[B".to_vec(),
                        0xff53 => b"\x1b[C".to_vec(),
                        0xff51 => b"\x1b[D".to_vec(),
                        0xff50 => b"\x1b[H".to_vec(),
                        0xff57 => b"\x1b[F".to_vec(),
                        0xff55 => b"\x1b[5~".to_vec(),
                        0xff56 => b"\x1b[6~".to_vec(),
                        0xffff => b"\x1b[3~".to_vec(),
                        0x20..=0x7e => terminal_printable_input(keysym, ctrl)
                            .into_iter()
                            .collect(),
                        _ => Vec::new(),
                    };
                    if !bytes.is_empty() {
                        self.clear_terminal_selection();
                        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                            tab.scroll_to_bottom();
                            tab.write_input(&bytes);
                        }
                    }
                }
            }
            Focus::Editor => {
                if let Some(Viewer::Text(text)) = self.viewer.as_mut() {
                    match keysym {
                        _ if ctrl && (keysym == 's' as u32 || keysym == 'S' as u32) => {
                            text.save();
                        }
                        0xff0d => text.newline(),
                        0xff08 => text.backspace(),
                        0xff1b => self.viewer_close(),
                        0xff51 => text.move_cursor(-1, 0),
                        0xff53 => text.move_cursor(1, 0),
                        0xff52 => text.move_cursor(0, -1),
                        0xff54 => text.move_cursor(0, 1),
                        0xff55 => text.scroll = text.scroll.saturating_sub(20),
                        0xff56 => {
                            text.scroll =
                                (text.scroll + 20).min(text.lines.len().saturating_sub(4));
                        }
                        0x20..=0x7e if !ctrl => {
                            if let Some(ch) = char::from_u32(keysym) {
                                text.insert_char(ch);
                            }
                        }
                        _ => {}
                    }
                    // Keep the cursor on screen.
                    if let Some(Viewer::Text(text)) = self.viewer.as_mut() {
                        let visible = 24usize;
                        if text.cursor.0 < text.scroll {
                            text.scroll = text.cursor.0;
                        } else if text.cursor.0 >= text.scroll + visible {
                            text.scroll = text.cursor.0 - visible + 1;
                        }
                    }
                }
            }
            Focus::Files => match keysym {
                0xff1b => {
                    if self.viewer.is_some() {
                        self.viewer_close();
                    }
                }
                0xff08 => {
                    if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
                        self.navigate(&parent);
                    }
                }
                0xff52 => {
                    let idx = self.selected.map(|i| i.saturating_sub(1)).unwrap_or(0);
                    self.selected = Some(idx);
                    if idx < self.scroll {
                        self.scroll = idx;
                    }
                }
                0xff54 => {
                    let idx = self
                        .selected
                        .map(|i| (i + 1).min(self.entries.len().saturating_sub(1)))
                        .unwrap_or(0);
                    self.selected = Some(idx);
                    if idx >= self.scroll + self.visible_rows() {
                        self.scroll = idx + 1 - self.visible_rows();
                    }
                }
                0xff0d => {
                    if let Some(idx) = self.selected {
                        self.open_entry(idx);
                    }
                }
                _ => {}
            },
        }
        Ok(false)
    }

    // ------------------------------------------------------------ drawing

    fn draw(&mut self) -> AnyResult<()> {
        let mut c = Canvas::new(self.width, self.height, PAPER);
        if self.viewer_only {
            self.draw_viewer(&mut c);
            self.draw_image_menu(&mut c);
            self.upload(&c)?;
            return Ok(());
        }
        self.draw_header(&mut c);
        self.draw_sidebar(&mut c);
        if self.viewer.is_some() {
            self.draw_viewer(&mut c);
        } else {
            self.draw_file_list(&mut c);
        }
        if self.terminal_visible {
            self.draw_terminal(&mut c);
        }
        self.draw_header_menus(&mut c);
        self.draw_file_menu(&mut c);
        self.draw_image_menu(&mut c);
        self.upload(&c)?;
        Ok(())
    }

    fn draw_header(&self, c: &mut Canvas) {
        c.draw_rect(0, 0, i32::from(self.width), HEADER_H, Color::rgb(236, 245, 250));
        c.draw_rect(0, HEADER_H - 1, i32::from(self.width), 1, Color::rgba(176, 198, 210, 120));
        // Match the desktop folder toolbar: Home, Terminal, Sort, Tabs, More.
        c.draw_round_rect(18, 18, 30, 30, 10, CARD);
        self.draw_home_icon(c, 33, 33);
        c.draw_round_rect(56, 18, 30, 30, 10, CARD);
        self.draw_terminal_icon(
            c,
            71,
            33,
            if self.terminal_visible { MINT_DARK } else { SOFT_INK },
        );
        c.draw_round_rect(94, 18, 30, 30, 10, CARD);
        self.draw_sort_icon(c, 109, 33);
        c.draw_round_rect(132, 18, 30, 30, 10, CARD);
        self.draw_folder_tabs_icon(c, 147, 33);
        c.draw_round_rect(i32::from(self.width) - 50, 18, 30, 30, 10, CARD);
        self.draw_more_icon(c, i32::from(self.width) - 35, 33);
        // Path or viewer title
        let label = if let Some(viewer) = &self.viewer {
            let name = match viewer {
                Viewer::Text(v) => v.path.clone(),
                Viewer::Image(v) => v.path.clone(),
                Viewer::Pdf(v) => v.path.clone(),
                Viewer::Model(v) => v.path.clone(),
                Viewer::Media(v) => v.path.clone(),
                Viewer::Info(_) => self.cwd.clone(),
            };
            compact_path(&name, 60)
        } else {
            compact_path(&self.cwd, 60)
        };
        c.draw_text(&self.bold, &label, 18, 54, 14.0, MINT_DARK);
        if !self.status.is_empty() {
            c.draw_text(
                &self.regular,
                &compact(&self.status, 52),
                176,
                27,
                11.0,
                MUTED,
            );
        }
    }

    fn draw_header_menus(&self, c: &mut Canvas) {
        if self.sort_open {
            let menu_x = 94;
            let menu_y = 54;
            c.draw_round_rect(menu_x, menu_y, 122, 96, 12, Color::rgba(250, 254, 255, 242));
            for (idx, sort) in [SortMode::Name, SortMode::Date, SortMode::Size]
                .iter()
                .copied()
                .enumerate()
            {
                let y = menu_y + 16 + idx as i32 * 28;
                if sort == self.sort_mode {
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
        if self.folder_tabs_open {
            let menu_x = 132;
            let menu_y = 54;
            let menu_w = (i32::from(self.width) - menu_x - 20).min(320);
            let menu_h = 12 + (self.folder_tabs.len() as i32 + 1) * 32;
            c.draw_round_rect(
                menu_x,
                menu_y,
                menu_w,
                menu_h,
                12,
                Color::rgba(250, 254, 255, 248),
            );
            for (idx, tab) in self.folder_tabs.iter().enumerate() {
                let y = menu_y + 8 + idx as i32 * 32;
                if idx == self.active_folder_tab {
                    c.draw_round_rect(
                        menu_x + 7,
                        y + 2,
                        menu_w - 14,
                        28,
                        8,
                        Color::rgba(116, 213, 198, 92),
                    );
                }
                let label = if tab.role == FolderTabRole::Screenshots {
                    let file = tab
                        .last_file
                        .as_ref()
                        .and_then(|path| path.file_name())
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "Latest image".into());
                    format!("Screenshots  ·  {file}")
                } else {
                    let workspace = self
                        .workspace_tabs
                        .iter()
                        .filter_map(|(workspace, tab_idx)| (*tab_idx == idx).then_some(*workspace))
                        .min();
                    let folder = tab
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| "/".into());
                    workspace.map_or_else(
                        || format!("Tab {}  ·  {folder}", idx + 1),
                        |workspace| format!("Workspace {}  ·  {folder}", workspace + 1),
                    )
                };
                c.draw_text(
                    &self.regular,
                    &compact(&label, ((menu_w - 30) / 7).max(8) as usize),
                    menu_x + 16,
                    y + 8,
                    12.0,
                    if idx == self.active_folder_tab {
                        MINT_DARK
                    } else {
                        INK
                    },
                );
            }
            let add_y = menu_y + 8 + self.folder_tabs.len() as i32 * 32;
            c.draw_rect(
                menu_x + 10,
                add_y,
                menu_w - 20,
                1,
                Color::rgba(176, 198, 210, 110),
            );
            c.draw_text(
                &self.bold,
                "+  New folder tab",
                menu_x + 16,
                add_y + 9,
                12.0,
                MINT_DARK,
            );
        }
        if self.more_open {
            let menu_x = i32::from(self.width) - 234;
            let menu_y = 54;
            let items = self.more_menu_items();
            let menu_h = 40 + items.len() as i32 * 30 + 8;
            c.draw_round_rect(
                menu_x,
                menu_y,
                214,
                menu_h,
                12,
                Color::rgba(250, 254, 255, 246),
            );
            let target = self.menu_target_path();
            let target_name = target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/".into());
            c.draw_text(
                &self.bold,
                &compact(&target_name, 24),
                menu_x + 14,
                menu_y + 10,
                13.0,
                INK,
            );
            for (idx, label) in items.iter().enumerate() {
                let iy = menu_y + 36 + idx as i32 * 30;
                c.draw_round_rect(
                    menu_x + 8,
                    iy,
                    198,
                    26,
                    7,
                    Color::rgba(234, 246, 249, 130),
                );
                let color = if *label == "Paste" && self.copied_path.is_none() {
                    MUTED
                } else {
                    INK
                };
                c.draw_text(&self.regular, label, menu_x + 18, iy + 4, 12.0, color);
                if idx == 0 && self.show_hidden {
                    c.draw_circle(menu_x + 192, iy + 13, 4, MINT_DARK);
                }
            }
        }
    }

    fn more_menu_items(&self) -> Vec<&'static str> {
        vec![
            if self.show_hidden {
                "Hide hidden files"
            } else {
                "Show hidden files"
            },
            "Pin folder to sidebar",
            "Copy path",
            "Copy",
            "Paste",
            "Copy parent path",
        ]
    }

    /// Path the 3-dot menu operates on: the selected entry, or the folder.
    fn menu_target_path(&self) -> PathBuf {
        self.selected
            .and_then(|idx| self.entries.get(idx))
            .map(|entry| entry.path.clone())
            .unwrap_or_else(|| self.cwd.clone())
    }

    fn more_menu_action(&mut self, row: usize) {
        let target = self.menu_target_path();
        match row {
            0 => {
                self.show_hidden = !self.show_hidden;
                self.reset_dir_watch();
                self.refresh_entries();
                self.selected = None;
                self.scroll = 0;
                self.status = if self.show_hidden {
                    "Showing hidden files".into()
                } else {
                    "Hidden files are hidden".into()
                };
            }
            1 => self.pin_current_folder(),
            2 => self.action_copy_path(&target),
            3 => self.action_copy_entry(&target),
            4 => self.paste_copied_file(),
            _ => self.action_copy_parent_path(&target),
        }
    }

    /// Add the current folder to the sidebar places (persisted).
    fn pin_current_folder(&mut self) {
        if self.places.iter().any(|place| place.path == self.cwd) {
            self.status = "Folder already in sidebar".into();
            return;
        }
        add_pinned(&self.cwd);
        self.places = places();
        self.status = "Folder added to sidebar".into();
    }

    fn action_copy_path(&mut self, path: &Path) {
        let text = path.to_string_lossy().into_owned();
        self.status = if copy_text_to_system_clipboard(&text) {
            format!("Path copied: {}", compact(&text, 40))
        } else {
            "Copy failed (install xclip)".into()
        };
    }

    fn action_copy_entry(&mut self, path: &Path) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".into());
        let _ = copy_text_to_system_clipboard(&path.to_string_lossy());
        self.copied_path = Some(path.to_path_buf());
        self.status = format!("Copied {} (use Paste)", compact(&name, 28));
    }

    fn action_copy_parent_path(&mut self, path: &Path) {
        let parent = path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".into());
        self.status = if copy_text_to_system_clipboard(&parent) {
            format!("Parent path copied: {}", compact(&parent, 34))
        } else {
            "Copy failed (install xclip)".into()
        };
    }

    fn draw_home_icon(&self, c: &mut Canvas, cx: i32, cy: i32) {
        c.draw_line(cx - 10, cy, cx, cy - 9, 2, MINT_DARK);
        c.draw_line(cx, cy - 9, cx + 10, cy, 2, MINT_DARK);
        c.draw_round_rect(cx - 7, cy, 14, 11, 4, Color::rgba(29, 145, 137, 45));
        c.draw_round_rect(cx - 2, cy + 5, 4, 6, 2, MINT_DARK);
    }

    fn draw_folder_tabs_icon(&self, c: &mut Canvas, cx: i32, cy: i32) {
        c.draw_round_rect(cx - 9, cy - 7, 13, 10, 3, Color::rgba(29, 145, 137, 42));
        c.draw_round_rect(cx - 5, cy - 3, 13, 10, 3, Color::rgba(29, 145, 137, 72));
        c.draw_line(cx - 1, cy + 1, cx + 3, cy + 5, 2, MINT_DARK);
        c.draw_line(cx + 3, cy + 5, cx + 7, cy + 1, 2, MINT_DARK);
    }

    fn draw_terminal_icon(&self, c: &mut Canvas, cx: i32, cy: i32, color: Color) {
        c.draw_line(cx - 8, cy - 5, cx - 3, cy, 2, color);
        c.draw_line(cx - 8, cy + 5, cx - 3, cy, 2, color);
        c.draw_line(cx + 1, cy + 5, cx + 8, cy + 5, 2, color);
    }

    fn draw_sort_icon(&self, c: &mut Canvas, cx: i32, cy: i32) {
        c.draw_line(cx - 8, cy - 6, cx + 7, cy - 6, 2, MINT_DARK);
        c.draw_line(cx - 8, cy, cx + 3, cy, 2, MINT_DARK);
        c.draw_line(cx - 8, cy + 6, cx - 1, cy + 6, 2, MINT_DARK);
    }

    fn draw_more_icon(&self, c: &mut Canvas, cx: i32, cy: i32) {
        c.draw_circle(cx - 7, cy, 2, MINT_DARK);
        c.draw_circle(cx, cy, 2, MINT_DARK);
        c.draw_circle(cx + 7, cy, 2, MINT_DARK);
    }

    fn draw_sidebar(&self, c: &mut Canvas) {
        let (_, ty, _, _) = self.terminal_rect();
        c.draw_rect(0, HEADER_H, SIDEBAR_W, ty - HEADER_H, Color::rgb(240, 248, 252));
        c.draw_rect(SIDEBAR_W - 1, HEADER_H, 1, ty - HEADER_H, Color::rgba(176, 198, 210, 110));
        for (idx, place) in self.places.iter().enumerate() {
            let y = HEADER_H + 10 + idx as i32 * 32;
            if y + 28 > ty {
                break;
            }
            let active = place.path == self.cwd;
            if active {
                c.draw_round_rect(6, y - 3, SIDEBAR_W - 12, 28, 8, Color::rgba(116, 213, 198, 95));
            }
            c.draw_round_rect(12, y + 2, 14, 12, 3, Color::rgb(175, 218, 245));
            c.draw_text(
                &self.regular,
                &compact(&place.name, 17),
                34,
                y,
                13.0,
                if active { MINT_DARK } else { INK },
            );
        }
    }

    fn draw_file_list(&self, c: &mut Canvas) {
        let (cx, cy, cw, ch) = self.content_rect();
        if self.entries.is_empty() {
            c.draw_text(&self.regular, "Empty folder", cx + 18, cy + 16, 13.0, MUTED);
            return;
        }
        let visible = self.visible_rows();
        for (row, entry) in self
            .entries
            .iter()
            .skip(self.scroll)
            .take(visible)
            .enumerate()
        {
            let idx = self.scroll + row;
            let y = cy + 6 + row as i32 * ROW_H;
            let selected = self.selected == Some(idx);
            c.draw_round_rect(
                cx + 8,
                y,
                cw - 20,
                ROW_H - 4,
                8,
                if selected {
                    Color::rgba(116, 213, 198, 110)
                } else {
                    Color::rgba(255, 255, 255, 135)
                },
            );
            self.draw_kind_icon(c, entry.kind, cx + 26, y + ROW_H / 2 - 2);
            let name_x = cx + 46;
            let info = (entry.kind != FileKind::Directory)
                .then(|| format!("{}  ·  {}", entry.kind.label(), format_size(entry.size)));
            let info_x = info.as_ref().map_or(cx + cw - 18, |info| {
                cx + cw - 18 - measure_text(&self.regular, info, 11.0)
            });
            let name_chars = ((info_x - name_x - 12).max(28) / 7) as usize;
            c.draw_text(
                &self.bold,
                &compact(&entry.name, name_chars),
                name_x,
                y + 3,
                13.0,
                INK,
            );
            if let Some(info) = info {
                c.draw_text(&self.regular, &info, info_x, y + 6, 11.0, MUTED);
            }
        }
        // Scrollbar
        if self.entries.len() > visible {
            let track_h = ch - 16;
            let thumb_h = (track_h * visible as i32 / self.entries.len() as i32).max(24);
            let max_scroll = (self.entries.len() - visible).max(1);
            let thumb_y = cy + 8
                + (track_h - thumb_h) * self.scroll.min(max_scroll) as i32 / max_scroll as i32;
            c.draw_round_rect(cx + cw - 9, cy + 8, 5, track_h, 3, Color::rgba(176, 198, 210, 80));
            c.draw_round_rect(cx + cw - 9, thumb_y, 5, thumb_h, 3, Color::rgba(29, 145, 137, 170));
        }
    }

    fn draw_kind_icon(&self, c: &mut Canvas, kind: FileKind, cx: i32, cy: i32) {
        match kind {
            FileKind::Directory => {
                c.draw_round_rect(cx - 9, cy - 7, 18, 14, 4, Color::rgb(175, 218, 245));
            }
            FileKind::Image => {
                c.draw_round_rect(cx - 9, cy - 7, 18, 14, 4, Color::rgba(29, 145, 137, 60));
                c.draw_circle(cx - 3, cy - 2, 2, MINT_DARK);
                c.draw_line(cx - 6, cy + 5, cx + 1, cy - 1, 2, MINT_DARK);
                c.draw_line(cx + 1, cy - 1, cx + 7, cy + 4, 2, MINT_DARK);
            }
            FileKind::Audio => {
                c.draw_line(cx - 2, cy - 6, cx - 2, cy + 4, 2, MINT_DARK);
                c.draw_circle(cx - 4, cy + 5, 3, MINT_DARK);
                c.draw_line(cx - 2, cy - 6, cx + 6, cy - 8, 2, MINT_DARK);
            }
            FileKind::Video => {
                c.draw_round_rect(cx - 9, cy - 7, 18, 14, 4, Color::rgba(73, 156, 231, 60));
                c.draw_line(cx - 2, cy - 4, cx + 4, cy, 2, BLUE);
                c.draw_line(cx + 4, cy, cx - 2, cy + 4, 2, BLUE);
            }
            FileKind::Pdf => {
                c.draw_round_rect(cx - 8, cy - 9, 16, 18, 3, Color::rgba(226, 92, 101, 70));
                c.draw_text_center(&self.bold, "P", cx, cy - 7, 11.0, Color::rgb(196, 64, 74));
            }
            FileKind::Doc => {
                c.draw_round_rect(cx - 8, cy - 9, 16, 18, 3, Color::rgba(73, 156, 231, 70));
                c.draw_text_center(&self.bold, "D", cx, cy - 7, 11.0, BLUE);
            }
            FileKind::Model3d => {
                c.draw_round_rect(cx - 8, cy - 8, 16, 16, 3, Color::rgba(29, 145, 137, 55));
                c.draw_text_center(&self.bold, "3D", cx, cy - 6, 9.0, MINT_DARK);
            }
            _ => {
                c.draw_round_rect(cx - 8, cy - 9, 16, 18, 3, Color::rgba(105, 118, 132, 55));
                for dy in [-4, 0, 4] {
                    c.draw_line(cx - 4, cy + dy, cx + 4, cy + dy, 1, SOFT_INK);
                }
            }
        }
    }

    fn draw_viewer(&mut self, c: &mut Canvas) {
        let (cx, cy, cw, ch) = self.content_rect();
        let Some(viewer) = &self.viewer else {
            return;
        };
        match viewer {
            Viewer::Text(text) => {
                let bar = format!(
                    "{}{}  -  {} lines{}",
                    if text.dirty { "* " } else { "" },
                    text.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    text.lines.len(),
                    if text.editable { "  -  Ctrl+S saves, Esc closes" } else { "  (read-only preview)" },
                );
                c.draw_text(&self.bold, &compact(&bar, 78), cx + 18, cy + 10, 12.0, SOFT_INK);
                if !text.status.is_empty() {
                    c.draw_text(&self.regular, &text.status, cx + cw - 130, cy + 10, 11.0, MINT_DARK);
                }
                let visible = ((ch - 56) / 18).max(1) as usize;
                let max_chars = ((cw - 40) / 8).max(1) as usize;
                for (row, line) in text.lines.iter().skip(text.scroll).take(visible).enumerate() {
                    let y = cy + 44 + row as i32 * 18;
                    let line_idx = text.scroll + row;
                    if text.editable && line_idx == text.cursor.0 && self.focus == Focus::Editor {
                        c.draw_rect(cx + 12, y - 2, cw - 26, 18, Color::rgba(116, 213, 198, 40));
                        if text.cursor.1 <= max_chars {
                            let cursor_x = cx
                                + 18
                                + text_prefix_width(&self.mono, line, text.cursor.1);
                            c.draw_rect(cursor_x, y - 2, 2, 17, MINT_DARK);
                        }
                    }
                    c.draw_text(
                        &self.mono,
                        &compact(line, max_chars),
                        cx + 18,
                        y,
                        TEXT_FONT_SIZE,
                        INK,
                    );
                }
            }
            Viewer::Image(img) => {
                if let Some(err) = &img.error {
                    c.draw_text(&self.regular, err, cx + 18, cy + 20, 13.0, MUTED);
                } else if self.viewer_only {
                    c.draw_rect(cx, cy, cw, ch, Color::rgb(21, 39, 66));
                    let x = cx + (cw - img.width as i32) / 2;
                    let y = cy + (ch - img.height as i32) / 2;
                    c.draw_rect(
                        x - 3,
                        y - 3,
                        img.width as i32 + 6,
                        img.height as i32 + 6,
                        Color::rgb(38, 68, 110),
                    );
                    c.paint_rgba(&img.pixels, x, y, img.width as i32, img.height as i32);
                } else {
                    let x = cx + (cw - img.width as i32) / 2;
                    let y = cy + 40 + (ch - 60 - img.height as i32).max(0) / 2;
                    // Dark navy backdrop + border make the image pop against
                    // the light chrome, whatever the picture's own colors.
                    c.draw_round_rect(cx + 8, cy + 34, cw - 16, ch - 46, 12, Color::rgb(21, 39, 66));
                    c.draw_rect(x - 3, y - 3, img.width as i32 + 6, img.height as i32 + 6, Color::rgb(38, 68, 110));
                    c.paint_rgba(&img.pixels, x, y, img.width as i32, img.height as i32);
                    let size_bytes = std::fs::metadata(&img.path).map(|m| m.len()).unwrap_or(0);
                    let mut label = format!(
                        "{} x {} px  -  {}",
                        img.orig_width,
                        img.orig_height,
                        format_size(size_bytes)
                    );
                    if (img.zoom - 1.0).abs() > 0.01 {
                        label.push_str(&format!("  -  {}%", (img.zoom * 100.0).round() as i32));
                    }
                    c.draw_text(&self.regular, &label, cx + 18, cy + 10, 12.0, MUTED);
                }
            }
            Viewer::Pdf(pdf) => {
                c.draw_text(
                    &self.bold,
                    &format!("Page {} / {}", pdf.page, pdf.pages),
                    cx + 18,
                    cy + 10,
                    13.0,
                    SOFT_INK,
                );
                // Prev / next buttons
                c.draw_round_rect(cx + cw - 170, cy + 6, 50, 26, 8, CARD);
                c.draw_text_center(&self.bold, "<", cx + cw - 145, cy + 10, 13.0, MINT_DARK);
                c.draw_round_rect(cx + cw - 110, cy + 6, 50, 26, 8, CARD);
                c.draw_text_center(&self.bold, ">", cx + cw - 85, cy + 10, 13.0, MINT_DARK);
                if let Some(err) = &pdf.error {
                    c.draw_text(&self.regular, err, cx + 18, cy + 50, 13.0, MUTED);
                } else if let Some(img) = &pdf.image {
                    let x = cx + (cw - img.width as i32) / 2;
                    c.paint_rgba(&img.pixels, x, cy + 40, img.width as i32, img.height as i32);
                }
            }
            Viewer::Model(model) => {
                c.draw_text(
                    &self.regular,
                    &format!(
                        "{} vertices, {} edges - drag to rotate",
                        model.vertices.len(),
                        model.edges.len()
                    ),
                    cx + 18,
                    cy + 10,
                    12.0,
                    MUTED,
                );
                if let Some(err) = &model.error {
                    c.draw_text(&self.regular, err, cx + 18, cy + 50, 13.0, MUTED);
                } else {
                    c.draw_round_rect(cx + 12, cy + 36, cw - 24, ch - 52, 12, Color::rgb(26, 36, 46));
                    let projected = model.project(cw - 24, ch - 52);
                    for &(a, b) in model.edges.iter().take(60_000) {
                        let (ax, ay) = projected[a as usize];
                        let (bx, by) = projected[b as usize];
                        c.draw_line(cx + 12 + ax, cy + 36 + ay, cx + 12 + bx, cy + 36 + by, 1, Color::rgba(120, 220, 200, 200));
                    }
                }
            }
            Viewer::Media(media) => {
                c.draw_text(
                    &self.bold,
                    &compact(
                        &media
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        48,
                    ),
                    cx + 18,
                    cy + 10,
                    13.0,
                    INK,
                );
                c.draw_round_rect(cx + cw - 106, cy + 6, 90, 26, 8, Color::rgba(226, 92, 101, 200));
                c.draw_text_center(&self.bold, "Stop", cx + cw - 61, cy + 10, 12.0, Color::rgb(255, 255, 255));
                if let Some(err) = &media.error {
                    c.draw_text(&self.regular, err, cx + 18, cy + 50, 13.0, MUTED);
                } else if media.kind == FileKind::Audio {
                    c.draw_text(
                        &self.regular,
                        &format!("Playing audio via {}", media.player_name),
                        cx + 18,
                        cy + 50,
                        13.0,
                        MINT_DARK,
                    );
                } else if !media.embedded {
                    c.draw_text(
                        &self.regular,
                        &format!("Playing in external window via {}", media.player_name),
                        cx + 18,
                        cy + 50,
                        13.0,
                        MINT_DARK,
                    );
                }
            }
            Viewer::Info(message) => {
                c.draw_text(&self.regular, message, cx + 18, cy + 20, 13.0, MUTED);
            }
        }
        // Close button, always at the top-right of the viewer.
        let (bx, by, bw, bh) = self.viewer_close_button_rect();
        c.draw_round_rect(bx, by, bw, bh, bw / 2, Color::rgba(226, 92, 101, 220));
        c.draw_line(bx + 9, by + 9, bx + bw - 9, by + bh - 9, 2, Color::rgb(255, 255, 255));
        c.draw_line(bx + bw - 9, by + 9, bx + 9, by + bh - 9, 2, Color::rgb(255, 255, 255));
    }

    /// Rectangle of the viewer's close (X) button, at the top-right corner.
    fn viewer_close_button_rect(&self) -> (i32, i32, i32, i32) {
        let (cx, cy, cw, _ch) = self.content_rect();
        let bw = 26;
        (cx + cw - bw - 10, cy + 6, bw, bw)
    }

    const FILE_MENU_ITEMS: [&'static str; 4] =
        ["Copy path", "Copy", "Paste", "Copy parent path"];

    fn file_menu_geometry(&self) -> (i32, i32, i32, i32) {
        let (mx, my) = self
            .file_menu
            .as_ref()
            .map(|(x, y, _)| (*x, *y))
            .unwrap_or((0, 0));
        let w = 190;
        let h = Self::FILE_MENU_ITEMS.len() as i32 * 30 + 10;
        let x = mx.min(i32::from(self.width) - w - 6).max(4);
        let y = my.min(i32::from(self.height) - h - 6).max(4);
        (x, y, w, h)
    }

    fn draw_file_menu(&self, c: &mut Canvas) {
        if self.file_menu.is_none() {
            return;
        }
        let (gx, gy, gw, gh) = self.file_menu_geometry();
        c.draw_round_rect(gx, gy, gw, gh, 10, Color::rgb(250, 254, 255));
        c.draw_round_rect(gx, gy, gw, gh, 10, Color::rgba(176, 198, 210, 90));
        for (i, label) in Self::FILE_MENU_ITEMS.iter().enumerate() {
            let ry = gy + 5 + i as i32 * 30;
            // Grey out "Paste" until something has been copied.
            let color = if i == 2 && self.copied_path.is_none() {
                MUTED
            } else {
                INK
            };
            c.draw_text(&self.regular, label, gx + 16, ry + 9, 13.0, color);
        }
    }

    /// Returns true when the click landed on a menu item (so the caller consumes it).
    fn handle_file_menu_click(&mut self, x: i32, y: i32) -> bool {
        let Some((_, _, path)) = self.file_menu.clone() else {
            return false;
        };
        let (gx, gy, gw, gh) = self.file_menu_geometry();
        if x < gx || x >= gx + gw || y < gy || y >= gy + gh {
            return false;
        }
        let row = ((y - gy - 5) / 30).clamp(0, Self::FILE_MENU_ITEMS.len() as i32 - 1);
        match row {
            0 => self.action_copy_path(&path),
            1 => self.action_copy_entry(&path),
            2 => self.paste_copied_file(),
            _ => self.action_copy_parent_path(&path),
        }
        true
    }

    /// Paste the previously copied file/folder into the current folder.
    fn paste_copied_file(&mut self) {
        let Some(src) = self.copied_path.clone() else {
            self.status = "Nothing to paste (use Copy first)".into();
            return;
        };
        if !src.exists() {
            self.status = "Copied item no longer exists".into();
            return;
        }
        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "copy".into());
        let mut dest = self.cwd.join(&name);
        let mut counter = 1u32;
        while dest.exists() {
            dest = self.cwd.join(format!("{name} (copy {counter})"));
            counter += 1;
            if counter > 99 {
                self.status = "Too many copies of that name".into();
                return;
            }
        }
        let ok = Command::new("cp")
            .arg("-a")
            .arg("--")
            .arg(&src)
            .arg(&dest)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            self.known_paths.insert(dest.clone());
            self.recent_paths.insert(0, dest.clone());
            self.refresh_entries();
            self.selected = self.entries.iter().position(|entry| entry.path == dest);
            self.scroll = 0;
            self.status = format!("Pasted {}", compact(&name, 30));
        } else {
            self.status = "Paste failed".into();
        }
    }

    const IMAGE_MENU_ITEMS: [&'static str; 4] =
        ["Copy image", "Zoom in", "Zoom out", "Reset zoom"];

    fn image_menu_geometry(&self) -> (i32, i32, i32, i32) {
        let (mx, my) = self.image_menu.unwrap_or((0, 0));
        let w = 170;
        let h = Self::IMAGE_MENU_ITEMS.len() as i32 * 30 + 10;
        let x = mx.min(i32::from(self.width) - w - 6).max(4);
        let y = my.min(i32::from(self.height) - h - 6).max(4);
        (x, y, w, h)
    }

    fn draw_image_menu(&self, c: &mut Canvas) {
        if self.image_menu.is_none() {
            return;
        }
        let (gx, gy, gw, gh) = self.image_menu_geometry();
        c.draw_round_rect(gx, gy, gw, gh, 10, Color::rgb(250, 254, 255));
        c.draw_round_rect(gx, gy, gw, gh, 10, Color::rgba(176, 198, 210, 90));
        for (i, label) in Self::IMAGE_MENU_ITEMS.iter().enumerate() {
            let ry = gy + 5 + i as i32 * 30;
            c.draw_text(&self.regular, label, gx + 16, ry + 9, 13.0, INK);
        }
    }

    /// Returns true when the click landed on a menu item (so the caller consumes it).
    fn handle_image_menu_click(&mut self, _mx: i32, _my: i32, x: i32, y: i32) -> bool {
        let (gx, gy, gw, gh) = self.image_menu_geometry();
        if x < gx || x >= gx + gw || y < gy || y >= gy + gh {
            return false;
        }
        let row = ((y - gy - 5) / 30).clamp(0, Self::IMAGE_MENU_ITEMS.len() as i32 - 1);
        match row {
            0 => {
                if let Some(Viewer::Image(img)) = &self.viewer {
                    let path = img.path.clone();
                    self.status = if copy_image_to_system_clipboard(&path) {
                        "Image copied to clipboard".into()
                    } else {
                        "Copy failed (install xclip)".into()
                    };
                }
            }
            1 => self.zoom_image(1.25),
            2 => self.zoom_image(0.8),
            _ => self.zoom_image_to(1.0),
        }
        true
    }

    fn zoom_image(&mut self, factor: f32) {
        if let Some(Viewer::Image(img)) = &self.viewer {
            let z = (img.zoom * factor).clamp(0.1, 12.0);
            self.zoom_image_to(z);
        }
    }

    fn zoom_image_to(&mut self, zoom: f32) {
        let Some(Viewer::Image(img)) = &self.viewer else {
            return;
        };
        let path = img.path.clone();
        let (_, _, cw, ch) = self.content_rect();
        let mw = (cw - 40).max(50) as u32;
        let mh = (ch - 80).max(50) as u32;
        self.viewer = Some(Viewer::Image(ImageView::open_zoomed(&path, mw, mh, zoom)));
        self.status = format!("Zoom {}%", (zoom * 100.0).round() as i32);
    }

    // ------------------------------------------------------ drag-out (XDND source)

    fn send_client_message(&self, target: Window, type_: Atom, data: [u32; 5]) -> AnyResult<()> {
        let event = ClientMessageEvent::new(32, target, type_, data);
        self.conn
            .send_event(false, target, EventMask::NO_EVENT, event)?;
        Ok(())
    }

    /// XdndAware version advertised by `win`, if any.
    fn xdnd_aware_version(&self, win: Window) -> AnyResult<Option<u8>> {
        let reply = self
            .conn
            .get_property(false, win, self.xdnd.aware, AtomEnum::ATOM, 0, 1)?
            .reply();
        if let Ok(reply) = reply {
            if let Some(mut vals) = reply.value32() {
                if let Some(ver) = vals.next() {
                    return Ok(Some((ver as u8).min(5)));
                }
            }
        }
        Ok(None)
    }

    /// Deepest XDND-aware window under the given root coordinates.
    fn xdnd_target_at(&self, root_x: i16, root_y: i16) -> AnyResult<(Window, u8)> {
        let own_img = self.img_win.as_ref().map(|w| w.window);
        let mut win = self.root;
        let mut found = (0u32, 0u8);
        for _ in 0..16 {
            let trans = self
                .conn
                .translate_coordinates(self.root, win, root_x, root_y)?
                .reply()?;
            if win != self.root && win != self.window && Some(win) != own_img {
                if let Some(ver) = self.xdnd_aware_version(win)? {
                    found = (win, ver);
                }
            }
            if trans.child == x11rb::NONE {
                break;
            }
            win = trans.child;
        }
        Ok(found)
    }

    fn xdnd_enter(&self, target: Window, _ver: u8) -> AnyResult<()> {
        // Version 5 in the high byte; low bit 0 = we advertise <= 3 types inline.
        self.send_client_message(
            target,
            self.xdnd.enter,
            [self.window, 5 << 24, self.xdnd.uri_list, 0, 0],
        )
    }

    fn xdnd_position(&self, target: Window, root_x: i16, root_y: i16) -> AnyResult<()> {
        let pos = ((root_x as u32) << 16) | (root_y as u32 & 0xffff);
        self.send_client_message(
            target,
            self.xdnd.position,
            [self.window, 0, pos, 0, self.xdnd.action_copy],
        )
    }

    fn xdnd_leave(&self, target: Window) -> AnyResult<()> {
        self.send_client_message(target, self.xdnd.leave, [self.window, 0, 0, 0, 0])
    }

    fn xdnd_drop(&self, target: Window, time: u32) -> AnyResult<()> {
        self.send_client_message(target, self.xdnd.drop, [self.window, 0, time, 0, 0])
    }

    /// Handle pointer motion while a file drag is armed/active. Returns whether a redraw
    /// is needed.
    fn drag_out_motion(
        &mut self,
        event_x: i32,
        event_y: i32,
        root_x: i16,
        root_y: i16,
        time: u32,
    ) -> AnyResult<bool> {
        let Some(drag) = self.file_drag.as_ref() else {
            return Ok(false);
        };
        let src_win = drag.src_win;
        if !drag.started {
            let moved =
                (event_x - drag.start_x).abs() > 6 || (event_y - drag.start_y).abs() > 6;
            if !moved {
                return Ok(false);
            }
            self.conn
                .set_selection_owner(self.window, self.xdnd.selection, time)?;
            let _ = self
                .conn
                .grab_pointer(
                    false,
                    src_win,
                    EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                    x11rb::NONE,
                    x11rb::NONE,
                    time,
                )?
                .reply();
            if let Some(d) = self.file_drag.as_mut() {
                d.started = true;
            }
            self.status = "Dragging file to another app...".into();
        }
        let (target, ver) = self.xdnd_target_at(root_x, root_y)?;
        let prev = self.file_drag.as_ref().map(|d| d.target).unwrap_or(0);
        if target != prev {
            if prev != 0 {
                self.xdnd_leave(prev)?;
            }
            if target != 0 {
                self.xdnd_enter(target, ver)?;
            }
            if let Some(d) = self.file_drag.as_mut() {
                d.target = target;
                d.target_ver = ver;
                d.accepted = false;
            }
        }
        if target != 0 {
            self.xdnd_position(target, root_x, root_y)?;
        }
        self.conn.flush()?;
        Ok(false)
    }

    /// Handle button release: complete or cancel a drag-out.
    fn drag_out_release(&mut self, time: u32) -> AnyResult<()> {
        let Some(drag) = self.file_drag.as_ref() else {
            return Ok(());
        };
        if !drag.started {
            self.file_drag = None;
            return Ok(());
        }
        let target = drag.target;
        let accepted = drag.accepted;
        let _ = self.conn.ungrab_pointer(time);
        if target != 0 && accepted {
            self.xdnd_drop(target, time)?;
            self.status = "Dropped file".into();
            // Keep file_drag alive to answer the SelectionRequest / XdndFinished.
        } else {
            if target != 0 {
                self.xdnd_leave(target)?;
            }
            self.file_drag = None;
            self.status = "Drag cancelled".into();
        }
        self.conn.flush()?;
        Ok(())
    }

    fn handle_selection_request(&mut self, ev: &SelectionRequestEvent) -> AnyResult<()> {
        if ev.selection != self.xdnd.selection {
            return Ok(());
        }
        let mut property = ev.property;
        if let Some(drag) = self.file_drag.as_ref() {
            if ev.target == self.xdnd.uri_list {
                let uri = format!("file://{}\r\n", drag.path.to_string_lossy());
                self.conn.change_property8(
                    PropMode::REPLACE,
                    ev.requestor,
                    ev.property,
                    self.xdnd.uri_list,
                    uri.as_bytes(),
                )?;
            } else {
                property = x11rb::NONE;
            }
        } else {
            property = x11rb::NONE;
        }
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: ev.time,
            requestor: ev.requestor,
            selection: ev.selection,
            target: ev.target,
            property,
        };
        self.conn
            .send_event(false, ev.requestor, EventMask::NO_EVENT, notify)?;
        self.conn.flush()?;
        Ok(())
    }

    fn handle_xdnd_client_message(&mut self, ev: &ClientMessageEvent) -> bool {
        let data = ev.data.as_data32();
        if ev.type_ == self.xdnd.status {
            if let Some(drag) = self.file_drag.as_mut() {
                drag.accepted = data[1] & 1 != 0;
            }
            true
        } else if ev.type_ == self.xdnd.finished {
            self.file_drag = None;
            true
        } else {
            false
        }
    }

    fn draw_terminal(&self, c: &mut Canvas) {
        let (tx, ty, tw, th) = self.terminal_rect();
        c.draw_rect(tx, ty, tw, th, TERM_BG);
        // Tab bar
        c.draw_rect(tx, ty, tw, TAB_BAR_H, Color::rgb(236, 245, 250));
        c.draw_rect(
            tx,
            ty + TAB_BAR_H - 1,
            tw,
            1,
            Color::rgba(176, 198, 210, 120),
        );
        let tab_w = 148;
        for (idx, tab) in self.tabs.iter().enumerate() {
            let x = 8 + idx as i32 * tab_w;
            let active = idx == self.active_tab;
            c.draw_round_rect(
                x,
                ty + 4,
                tab_w - 6,
                TAB_BAR_H - 8,
                7,
                if active {
                    Color::rgba(116, 213, 198, 95)
                } else {
                    CARD
                },
            );
            let label = if tab.busy() {
                format!("{} *", compact(&tab.title, 12))
            } else {
                compact(&tab.title, 14)
            };
            c.draw_text(
                &self.regular,
                &label,
                x + 8,
                ty + 8,
                11.0,
                if active {
                    MINT_DARK
                } else {
                    SOFT_INK
                },
            );
            c.draw_text(&self.regular, "x", x + tab_w - 18, ty + 8, 11.0, MUTED);
        }
        // "+" new tab
        let plus_x = 8 + self.tabs.len() as i32 * tab_w;
        c.draw_round_rect(plus_x, ty + 4, 26, TAB_BAR_H - 8, 7, CARD);
        c.draw_text_center(&self.regular, "+", plus_x + 13, ty + 7, 13.0, MINT_DARK);
        // Hide button
        c.draw_text(&self.regular, "v", i32::from(self.width) - 24, ty + 8, 12.0, MUTED);

        // Grid
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let origin_y = ty + TAB_BAR_H + 6;
        for row in 0..tab.rows {
            let y = origin_y + row as i32 * CELL_H;
            if y + CELL_H > ty + th {
                break;
            }
            let Some(cells) = tab.display_row(row) else {
                continue;
            };
            for col in 0..cells.len() {
                if self.terminal_cell_selected(row, col) {
                    c.draw_rect(
                        10 + col as i32 * CELL_W,
                        y,
                        CELL_W,
                        CELL_H,
                        Color::rgba(73, 156, 231, 105),
                    );
                }
            }
            // Position every glyph on the PTY's fixed cell grid. Drawing a
            // whole run uses the font's fractional advance, which accumulates
            // an offset between the visible text and the grid cursor.
            for (col, cell) in cells.iter().enumerate() {
                if cell.ch != ' ' {
                    c.draw_text(
                        &self.mono,
                        &cell.ch.to_string(),
                        10 + col as i32 * CELL_W,
                        y,
                        13.0,
                        cell.fg,
                    );
                }
            }
        }
        if let Some((message, _)) = self.terminal_notice.as_ref() {
            let notice_w = 154;
            let notice_x = i32::from(self.width) - notice_w - 42;
            c.draw_round_rect(
                notice_x,
                ty + 4,
                notice_w,
                TAB_BAR_H - 8,
                7,
                Color::rgba(116, 213, 198, 118),
            );
            c.draw_text_center(
                &self.bold,
                message,
                notice_x + notice_w / 2,
                ty + 8,
                11.0,
                MINT_DARK,
            );
        }
        if tab.scrollback > 0 {
            let label = format!("up {} lines", tab.scrollback);
            c.draw_round_rect(
                i32::from(self.width) - 118,
                ty + th - 27,
                104,
                20,
                6,
                Color::rgba(116, 213, 198, 130),
            );
            c.draw_text_center(
                &self.bold,
                &label,
                i32::from(self.width) - 66,
                ty + th - 24,
                10.0,
                MINT_DARK,
            );
        }
        // Cursor
        if !tab.dead && self.focus == Focus::Terminal && tab.scrollback == 0 {
            let cx = 10 + tab.cur_x as i32 * CELL_W;
            let cy = origin_y + tab.cur_y as i32 * CELL_H;
            c.draw_rect(cx, cy + 1, CELL_W, CELL_H - 2, Color::rgba(29, 145, 137, 145));
        }
        if tab.dead {
            c.draw_text(
                &self.regular,
                "Shell exited - click + for a new tab",
                14,
                origin_y + 4,
                12.0,
                Color::rgb(200, 150, 150),
            );
        }
    }

    fn upload(&self, c: &Canvas) -> AnyResult<()> {
        let img = Image::new(
            c.width,
            c.height,
            ScanlinePad::Pad32,
            self.depth,
            BitsPerPixel::B32,
            ImageOrder::LsbFirst,
            Cow::Borrowed(&c.data),
        )?;
        img.put(&self.conn, self.window, self.gc, 0, 0)?;
        Ok(())
    }
}
