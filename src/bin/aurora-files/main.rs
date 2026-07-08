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
use std::path::{Path, PathBuf};
use std::process::Command;
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

const HEADER_H: i32 = 52;
const SIDEBAR_W: i32 = 172;
const TAB_BAR_H: i32 = 30;
const ROW_H: i32 = 34;
const CELL_W: i32 = 8;
const CELL_H: i32 = 17;

type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Files,
    Terminal,
    Editor,
}

struct App {
    conn: RustConnection,
    display: String,
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

    cwd: PathBuf,
    entries: Vec<Entry>,
    places: Vec<fsmodel::Place>,
    selected: Option<usize>,
    last_click: Option<(usize, Instant)>,
    scroll: usize,
    show_hidden: bool,
    status: String,

    terminal_visible: bool,
    terminal_h: i32,
    tabs: Vec<Tab>,
    active_tab: usize,

    viewer: Option<Viewer>,
    focus: Focus,
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

fn run(args: &[String]) -> AnyResult<()> {
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
    let (conn, screen_num) = RustConnection::connect(None)?;
    let screen = conn.setup().roots[screen_num].clone();
    let window = conn.generate_id()?;
    let width: u16 = 1020.min(screen.width_in_pixels.saturating_sub(60));
    let height: u16 = 680.min(screen.height_in_pixels.saturating_sub(80));
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
    let title = "Aurora Files";
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

    let mut app = App {
        conn,
        display,
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
        entries: list_dir(&start_dir, false),
        places: places(),
        cwd: start_dir,
        selected: None,
        last_click: None,
        scroll: 0,
        show_hidden: false,
        status: String::new(),
        terminal_visible: true,
        terminal_h: 236,
        tabs: Vec::new(),
        active_tab: 0,
        viewer: None,
        focus: if args.iter().any(|a| a == "--terminal") {
            Focus::Terminal
        } else {
            Focus::Files
        },
    };
    if !start_path.is_dir() {
        app.open_file_by_path(&start_path.clone());
    }
    app.new_tab();
    app.event_loop()
}

impl App {
    // ------------------------------------------------------------ layout

    fn terminal_rect(&self) -> (i32, i32, i32, i32) {
        let h = if self.terminal_visible { self.terminal_h } else { 0 };
        (0, i32::from(self.height) - h, i32::from(self.width), h)
    }

    fn content_rect(&self) -> (i32, i32, i32, i32) {
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
    }

    // ------------------------------------------------------------ navigation

    fn navigate(&mut self, path: &Path) {
        if !path.is_dir() {
            return;
        }
        self.cwd = path.to_path_buf();
        self.entries = list_dir(&self.cwd, self.show_hidden);
        self.selected = None;
        self.scroll = 0;
        self.viewer_close();
        self.terminal_cd(&path.to_path_buf());
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

    // ------------------------------------------------------------ event loop

    fn event_loop(&mut self) -> AnyResult<()> {
        let mut last_draw = Instant::now();
        let mut needs_draw = true;
        loop {
            while let Some(event) = self.conn.poll_for_event()? {
                match event {
                    Event::Expose(ev) if ev.count == 0 => needs_draw = true,
                    Event::ConfigureNotify(ev) => {
                        if ev.width != self.width || ev.height != self.height {
                            self.width = ev.width;
                            self.height = ev.height;
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
                    Event::ButtonRelease(_) => {
                        if let Some(Viewer::Model(model)) = self.viewer.as_mut() {
                            model.dragging = None;
                        }
                    }
                    Event::MotionNotify(ev) => {
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
                    Event::ClientMessage(ev) => {
                        if ev.data.as_data32()[0] == self.wm_delete {
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
            if term_changed {
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
        let (tx, ty, tw, th) = self.terminal_rect();
        let _ = (tx, tw);
        // Terminal area
        if self.terminal_visible && y >= ty && th > 0 {
            self.focus = Focus::Terminal;
            let rel_y = y - ty;
            if rel_y < TAB_BAR_H {
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
            }
            return;
        }
        // Header
        if y < HEADER_H {
            if x < 44 {
                // Back / up one level
                if self.viewer.is_some() {
                    self.viewer_close();
                } else if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
                    self.navigate(&parent);
                }
            } else if x >= i32::from(self.width) - 44 {
                self.terminal_visible = !self.terminal_visible;
                let (cols, rows) = self.term_grid_size();
                for tab in &mut self.tabs {
                    tab.resize(cols, rows);
                }
            } else if x >= i32::from(self.width) - 88 && x < i32::from(self.width) - 44 {
                self.show_hidden = !self.show_hidden;
                self.entries = list_dir(&self.cwd, self.show_hidden);
                self.scroll = 0;
            }
            return;
        }
        // Sidebar
        if x < SIDEBAR_W {
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
        // Viewer interactions
        let (cx, cy, cw, _ch) = self.content_rect();
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
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    if ctrl && (keysym == 't' as u32 || keysym == 'T' as u32) && shift {
                        self.new_tab();
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
                        tab.write_input(&bytes);
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
        self.upload(&c)?;
        Ok(())
    }

    fn draw_header(&self, c: &mut Canvas) {
        c.draw_rect(0, 0, i32::from(self.width), HEADER_H, Color::rgb(236, 245, 250));
        c.draw_rect(0, HEADER_H - 1, i32::from(self.width), 1, Color::rgba(176, 198, 210, 120));
        // Back button
        c.draw_round_rect(8, 10, 32, 32, 9, CARD);
        c.draw_line(28, 18, 18, 26, 2, MINT_DARK);
        c.draw_line(18, 26, 28, 34, 2, MINT_DARK);
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
        c.draw_text(&self.bold, &label, 52, 15, 15.0, INK);
        // Hidden-files toggle
        c.draw_round_rect(i32::from(self.width) - 86, 10, 38, 32, 9, CARD);
        c.draw_text_center(
            &self.bold,
            if self.show_hidden { ".*" } else { ".x" },
            i32::from(self.width) - 67,
            17,
            13.0,
            if self.show_hidden { MINT_DARK } else { MUTED },
        );
        // Terminal toggle
        c.draw_round_rect(i32::from(self.width) - 42, 10, 34, 32, 9, CARD);
        c.draw_text_center(&self.bold, ">_", i32::from(self.width) - 25, 17, 13.0,
            if self.terminal_visible { MINT_DARK } else { MUTED });
        if !self.status.is_empty() {
            c.draw_text(&self.regular, &compact(&self.status, 52), 52, 33, 11.0, MUTED);
        }
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
            c.draw_text(&self.bold, &compact(&entry.name, 52), cx + 46, y + 3, 13.0, INK);
            let info = if entry.kind == FileKind::Directory {
                entry.kind.label().to_string()
            } else {
                format!("{} - {}", entry.kind.label(), format_size(entry.size))
            };
            c.draw_text(&self.regular, &info, cx + cw - 200, y + 6, 11.0, MUTED);
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
                } else {
                    let x = cx + (cw - img.width as i32) / 2;
                    let y = cy + 40 + (ch - 60 - img.height as i32).max(0) / 2;
                    c.draw_rect(x - 2, y - 2, img.width as i32 + 4, img.height as i32 + 4, Color::rgba(23, 34, 42, 30));
                    c.paint_rgba(&img.pixels, x, y, img.width as i32, img.height as i32);
                    c.draw_text(
                        &self.regular,
                        &format!("{}x{}", img.width, img.height),
                        cx + 18,
                        cy + 10,
                        12.0,
                        MUTED,
                    );
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
    }

    fn draw_terminal(&self, c: &mut Canvas) {
        let (tx, ty, tw, th) = self.terminal_rect();
        c.draw_rect(tx, ty, tw, th, TERM_BG);
        // Tab bar
        c.draw_rect(tx, ty, tw, TAB_BAR_H, Color::rgb(18, 27, 36));
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
                    Color::rgba(116, 213, 198, 60)
                } else {
                    Color::rgba(255, 255, 255, 18)
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
                    Color::rgb(160, 238, 220)
                } else {
                    Color::rgb(180, 196, 208)
                },
            );
            c.draw_text(&self.regular, "x", x + tab_w - 18, ty + 8, 11.0, Color::rgb(150, 165, 178));
        }
        // "+" new tab
        let plus_x = 8 + self.tabs.len() as i32 * tab_w;
        c.draw_round_rect(plus_x, ty + 4, 26, TAB_BAR_H - 8, 7, Color::rgba(255, 255, 255, 22));
        c.draw_text_center(&self.regular, "+", plus_x + 13, ty + 7, 13.0, Color::rgb(180, 196, 208));
        // Hide button
        c.draw_text(&self.regular, "v", i32::from(self.width) - 24, ty + 8, 12.0, Color::rgb(150, 165, 178));

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
            // Batch runs of same style for fewer draw_text calls.
            let mut run = String::new();
            let mut run_start = 0usize;
            let mut run_fg = cells.first().map(|c| c.fg).unwrap_or(term::TERM_FG);
            for (col, cell) in cells.iter().enumerate() {
                if cell.fg != run_fg && !run.trim().is_empty() {
                    c.draw_text(&self.mono, &run, 10 + run_start as i32 * CELL_W, y, 13.0, run_fg);
                    run.clear();
                    run_start = col;
                    run_fg = cell.fg;
                } else if cell.fg != run_fg {
                    run.clear();
                    run_start = col;
                    run_fg = cell.fg;
                }
                run.push(cell.ch);
            }
            if !run.trim().is_empty() {
                c.draw_text(&self.mono, &run, 10 + run_start as i32 * CELL_W, y, 13.0, run_fg);
            }
        }
        // Cursor
        if !tab.dead && self.focus == Focus::Terminal {
            let cx = 10 + tab.cur_x as i32 * CELL_W;
            let cy = origin_y + tab.cur_y as i32 * CELL_H;
            c.draw_rect(cx, cy + 1, CELL_W, CELL_H - 2, Color::rgba(160, 238, 220, 150));
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
