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
mod term;
mod viewer;

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use canvas::*;
use fsmodel::*;
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
        let mut a = |name: &[u8]| -> AnyResult<Atom> {
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
    regular: Font<'static>,
    bold: Font<'static>,
    mono: Font<'static>,
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
        regular: Font::try_from_bytes(FONT_REGULAR).ok_or("font")?,
        bold: Font::try_from_bytes(FONT_BOLD).ok_or("font")?,
        mono: Font::try_from_bytes(FONT_MONO).ok_or("font")?,
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
            self.entries.insert(
                0,
                Entry {
                    name: "..".into(),
                    path: parent,
                    kind: FileKind::Directory,
                    size: 0,
                    modified: std::time::SystemTime::UNIX_EPOCH,
                },
            );
        }
    }

    fn navigate(&mut self, path: &Path) {
        if !path.is_dir() {
            return;
        }
        self.cwd = path.to_path_buf();
        self.refresh_entries();
        self.selected = None;
        self.scroll = 0;
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

    fn open_file_by_path(&mut self, path: &Path) {
        let (_, _, cw, ch) = self.content_rect();
        let kind = file_kind_for(path);
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
        for row in start.0..=end.0.min(tab.grid.len().saturating_sub(1)) {
            let cells = &tab.grid[row];
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
                match event {
                    Event::Expose(ev) if ev.count == 0 => needs_draw = true,
                    Event::ConfigureNotify(ev) => {
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
                        self.on_click(
                            i32::from(ev.event_x),
                            i32::from(ev.event_y),
                            ev.detail,
                            u16::from(ev.state),
                        );
                        needs_draw = true;
                    }
                    Event::ButtonRelease(ev) => {
                        if self.file_drag.is_some() {
                            self.drag_out_release(ev.time)?;
                        }
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
                    Event::KeyPress(ev) => {
                        if self.on_key(ev)? {
                            return Ok(());
                        }
                        needs_draw = true;
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
                            self.viewer_close();
                            return Ok(());
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
            let menu_x = i32::from(self.width) - 214;
            if x >= menu_x + 8 && x <= menu_x + 186 && y >= 86 && y <= 116 {
                self.show_hidden = !self.show_hidden;
                self.more_open = false;
                self.refresh_entries();
                self.selected = None;
                self.scroll = 0;
                self.status = if self.show_hidden {
                    "Showing hidden files".into()
                } else {
                    "Hidden files are hidden".into()
                };
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
                        let col = ((x - cx - 18) / 8).max(0) as usize;
                        text.cursor = (line, col.min(text.lines[line].chars().count()));
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
                let is_parent = self.entries.get(idx).is_some_and(|entry| {
                    entry.name == ".."
                        && entry.kind == FileKind::Directory
                        && self.cwd.parent().is_some_and(|parent| parent == entry.path)
                });
                if is_parent {
                    self.open_entry(idx);
                    return;
                }
                let now = Instant::now();
                let double = self
                    .last_click
                    .is_some_and(|(i, at)| i == idx && now.duration_since(at).as_millis() < 450);
                self.last_click = Some((idx, now));
                if double || self.selected == Some(idx) {
                    self.open_entry(idx);
                } else {
                    self.selected = Some(idx);
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
                    if ctrl && matches!(keysym, 0x76 | 0x56) {
                        if let Some(text) = read_text_from_system_clipboard() {
                            self.clear_terminal_selection();
                            if let Some(tab) = self.tabs.get(self.active_tab) {
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
                        0x20..=0x7e => {
                            let ch = keysym as u8;
                            if ctrl {
                                vec![ch.to_ascii_uppercase().wrapping_sub(b'@') & 0x1f]
                            } else {
                                vec![ch]
                            }
                        }
                        _ => Vec::new(),
                    };
                    if !bytes.is_empty() {
                        self.clear_terminal_selection();
                        if let Some(tab) = self.tabs.get(self.active_tab) {
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
            let menu_x = i32::from(self.width) - 214;
            let menu_y = 54;
            c.draw_round_rect(
                menu_x,
                menu_y,
                194,
                72,
                12,
                Color::rgba(250, 254, 255, 242),
            );
            c.draw_text(&self.bold, "View", menu_x + 14, menu_y + 12, 14.0, INK);
            c.draw_round_rect(
                menu_x + 8,
                menu_y + 32,
                178,
                30,
                7,
                Color::rgba(234, 246, 249, 150),
            );
            c.draw_text(
                &self.regular,
                if self.show_hidden {
                    "Hide hidden files"
                } else {
                    "Show hidden files"
                },
                menu_x + 18,
                menu_y + 39,
                12.0,
                INK,
            );
            if self.show_hidden {
                c.draw_circle(menu_x + 172, menu_y + 47, 4, MINT_DARK);
            }
        }
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
                for (row, line) in text.lines.iter().skip(text.scroll).take(visible).enumerate() {
                    let y = cy + 44 + row as i32 * 18;
                    let line_idx = text.scroll + row;
                    if text.editable && line_idx == text.cursor.0 && self.focus == Focus::Editor {
                        c.draw_rect(cx + 12, y - 2, cw - 26, 18, Color::rgba(116, 213, 198, 40));
                        let cursor_x = cx + 18 + 8 * text.cursor.1.min(line.chars().count()) as i32;
                        c.draw_rect(cursor_x, y - 2, 2, 17, MINT_DARK);
                    }
                    c.draw_text(&self.mono, &compact(line, ((cw - 40) / 8) as usize), cx + 18, y, 13.0, INK);
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
        let mut win = self.root;
        let mut found = (0u32, 0u8);
        for _ in 0..16 {
            let trans = self
                .conn
                .translate_coordinates(self.root, win, root_x, root_y)?
                .reply()?;
            if win != self.root && win != self.window {
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
                    self.window,
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
        for (row, cells) in tab.grid.iter().enumerate() {
            let y = origin_y + row as i32 * CELL_H;
            if y + CELL_H > ty + th {
                break;
            }
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
        // Cursor
        if !tab.dead && self.focus == Focus::Terminal {
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
