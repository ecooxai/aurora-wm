//! Tabbed terminal: PTY management plus a compact VT100/ANSI emulator.
//!
//! Each tab owns its own shell process and screen grid. When the user
//! navigates to a folder while the active tab is running a foreground
//! program, a new tab is opened instead of disturbing it.

use std::ffi::CString;
use std::io::Read;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};

use crate::canvas::Color;

pub const TERM_FG: Color = Color::rgb(32, 43, 54);
pub const TERM_BG: Color = Color::rgb(247, 252, 255);

#[derive(Clone, Copy)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bold: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: TERM_FG,
            bold: false,
        }
    }
}

pub struct Tab {
    pub pty: RawFd,
    pub pid: libc::pid_t,
    pub cols: usize,
    pub rows: usize,
    pub grid: Vec<Vec<Cell>>,
    pub cur_x: usize,
    pub cur_y: usize,
    pub saved_cursor: (usize, usize),
    pub scroll_top: usize,
    pub scroll_bottom: usize,
    pub fg: Color,
    pub bold: bool,
    pub esc: EscState,
    pub esc_buf: String,
    pub title: String,
    pub cwd: PathBuf,
    pub dead: bool,
    pub wrap_pending: bool,
    pub bracketed_paste: bool,
    pub mouse_enabled: bool,
}

pub enum EscState {
    None,
    Esc,
    Csi,
    Osc,
}

pub fn ansi_color(idx: usize, bright: bool) -> Color {
    let base: [(u8, u8, u8); 8] = [
        (32, 43, 54),
        (190, 58, 66),
        (35, 132, 86),
        (156, 112, 18),
        (45, 105, 185),
        (145, 76, 165),
        (28, 126, 135),
        (92, 105, 118),
    ];
    let (r, g, b) = base[idx.min(7)];
    if bright {
        Color::rgb(
            r.saturating_add(25),
            g.saturating_add(25),
            b.saturating_add(25),
        )
    } else {
        Color::rgb(r, g, b)
    }
}

fn color_256(v: usize) -> Color {
    if v < 8 {
        ansi_color(v, false)
    } else if v < 16 {
        ansi_color(v - 8, true)
    } else if v < 232 {
        let v = v - 16;
        let scale = |c: usize| if c == 0 { 0u8 } else { (55 + c * 40) as u8 };
        Color::rgb(scale(v / 36), scale((v / 6) % 6), scale(v % 6))
    } else {
        let g = (8 + (v.saturating_sub(232)) * 10) as u8;
        Color::rgb(g, g, g)
    }
}

pub fn spawn_pty(cwd: &Path, cols: usize, rows: usize) -> Option<(RawFd, libc::pid_t)> {
    let mut master: RawFd = -1;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    ws.ws_col = cols as u16;
    ws.ws_row = rows as u16;
    let pid = unsafe { libc::forkpty(&mut master, std::ptr::null_mut(), std::ptr::null(), &ws) };
    if pid < 0 {
        return None;
    }
    if pid == 0 {
        // Child: exec the user's shell in the requested directory.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        let _ = std::env::set_current_dir(cwd);
        unsafe {
            std::env::set_var("TERM", "xterm-256color");
        }
        let c_shell = CString::new(shell).unwrap_or_else(|_| CString::new("/bin/sh").unwrap());
        unsafe {
            libc::execlp(
                c_shell.as_ptr(),
                c_shell.as_ptr(),
                std::ptr::null::<libc::c_char>(),
            );
            libc::_exit(1);
        }
    }
    unsafe {
        let flags = libc::fcntl(master, libc::F_GETFL);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    Some((master, pid))
}

impl Tab {
    pub fn new(cwd: PathBuf, cols: usize, rows: usize) -> Option<Self> {
        let (pty, pid) = spawn_pty(&cwd, cols, rows)?;
        Some(Self {
            pty,
            pid,
            cols,
            rows,
            grid: vec![vec![Cell::default(); cols]; rows],
            cur_x: 0,
            cur_y: 0,
            saved_cursor: (0, 0),
            scroll_top: 0,
            scroll_bottom: rows - 1,
            fg: TERM_FG,
            bold: false,
            esc: EscState::None,
            esc_buf: String::new(),
            title: cwd
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "shell".into()),
            cwd,
            dead: false,
            wrap_pending: false,
            bracketed_paste: false,
            mouse_enabled: false,
        })
    }

    /// True when a foreground program other than the shell is running.
    pub fn busy(&self) -> bool {
        if self.dead {
            return false;
        }
        let fg = unsafe { libc::tcgetpgrp(self.pty) };
        fg > 0 && fg != self.pid
    }

    pub fn write_input(&self, data: &[u8]) {
        if self.dead {
            return;
        }
        unsafe {
            let _ = libc::write(self.pty, data.as_ptr().cast(), data.len());
        }
    }

    pub fn send_cd(&self, path: &Path) {
        let quoted = format!(
            " cd '{}'\n",
            path.to_string_lossy().replace('\'', r"'\''")
        );
        self.write_input(quoted.as_bytes());
    }

    /// Drain PTY output. Returns true when anything changed.
    pub fn poll(&mut self) -> bool {
        if self.dead {
            return false;
        }
        let mut changed = false;
        let mut buf = [0u8; 16384];
        loop {
            let n = unsafe { libc::read(self.pty, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                let text = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
                for ch in text.chars() {
                    self.feed(ch);
                }
                changed = true;
                if (n as usize) < buf.len() {
                    break;
                }
            } else {
                if n == 0 {
                    self.dead = true;
                    changed = true;
                }
                break;
            }
        }
        let mut status = 0;
        if unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) } == self.pid {
            self.dead = true;
            changed = true;
        }
        changed
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows || cols < 4 || rows < 2 {
            return;
        }
        let mut grid = vec![vec![Cell::default(); cols]; rows];
        for (y, row) in self.grid.iter().rev().take(rows).enumerate() {
            let ny = rows - 1 - y;
            for (x, cell) in row.iter().take(cols).enumerate() {
                grid[ny][x] = *cell;
            }
        }
        self.grid = grid;
        self.cols = cols;
        self.rows = rows;
        self.cur_x = self.cur_x.min(cols - 1);
        self.cur_y = self.cur_y.min(rows - 1);
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        let ws = libc::winsize {
            ws_col: cols as u16,
            ws_row: rows as u16,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(self.pty, libc::TIOCSWINSZ, &ws);
        }
    }

    fn scroll_up(&mut self) {
        self.grid[self.scroll_top..=self.scroll_bottom].rotate_left(1);
        self.grid[self.scroll_bottom] = vec![Cell::default(); self.cols];
    }

    fn scroll_down(&mut self) {
        self.grid[self.scroll_top..=self.scroll_bottom].rotate_right(1);
        self.grid[self.scroll_top] = vec![Cell::default(); self.cols];
    }

    fn linefeed(&mut self) {
        if self.cur_y == self.scroll_bottom {
            self.scroll_up();
        } else if self.cur_y + 1 < self.rows {
            self.cur_y += 1;
        }
    }

    fn put_char(&mut self, ch: char) {
        if self.wrap_pending {
            self.wrap_pending = false;
            self.cur_x = 0;
            self.linefeed();
        }
        if self.cur_x >= self.cols {
            self.cur_x = self.cols - 1;
        }
        self.grid[self.cur_y][self.cur_x] = Cell {
            ch,
            fg: self.fg,
            bold: self.bold,
        };
        if self.cur_x + 1 == self.cols {
            self.wrap_pending = true;
        } else {
            self.cur_x += 1;
        }
    }

    pub fn feed(&mut self, ch: char) {
        match self.esc {
            EscState::None => match ch {
                '\x1b' => self.esc = EscState::Esc,
                '\n' => self.linefeed(),
                '\r' => {
                    self.cur_x = 0;
                    self.wrap_pending = false;
                }
                '\x08' => {
                    self.cur_x = self.cur_x.saturating_sub(1);
                    self.wrap_pending = false;
                }
                '\t' => {
                    let next = ((self.cur_x / 8) + 1) * 8;
                    self.cur_x = next.min(self.cols - 1);
                }
                '\x07' => {}
                ch if !ch.is_control() => self.put_char(ch),
                _ => {}
            },
            EscState::Esc => {
                match ch {
                    '[' => {
                        self.esc = EscState::Csi;
                        self.esc_buf.clear();
                        return;
                    }
                    ']' => {
                        self.esc = EscState::Osc;
                        self.esc_buf.clear();
                        return;
                    }
                    'M' => {
                        // Reverse index
                        if self.cur_y == self.scroll_top {
                            self.scroll_down();
                        } else {
                            self.cur_y = self.cur_y.saturating_sub(1);
                        }
                    }
                    '7' => self.saved_cursor = (self.cur_x, self.cur_y),
                    '8' => {
                        self.cur_x = self.saved_cursor.0.min(self.cols - 1);
                        self.cur_y = self.saved_cursor.1.min(self.rows - 1);
                    }
                    '(' | ')' | '#' => {
                        // Consume one more byte (charset designators etc.)
                        self.esc = EscState::Osc; // reuse: swallow until terminator-ish
                        self.esc_buf.clear();
                        self.esc_buf.push('\u{0}');
                        return;
                    }
                    _ => {}
                }
                self.esc = EscState::None;
            }
            EscState::Csi => {
                if ch.is_ascii_alphabetic() || ch == '@' || ch == '`' {
                    let buf = std::mem::take(&mut self.esc_buf);
                    self.esc = EscState::None;
                    self.apply_csi(&buf, ch);
                } else if self.esc_buf.len() < 64 {
                    self.esc_buf.push(ch);
                } else {
                    self.esc = EscState::None;
                }
            }
            EscState::Osc => {
                // Charset-designator swallow mode: single byte.
                if self.esc_buf.starts_with('\u{0}') {
                    self.esc = EscState::None;
                    self.esc_buf.clear();
                    return;
                }
                if ch == '\x07' {
                    let buf = std::mem::take(&mut self.esc_buf);
                    if let Some(title) = buf
                        .strip_prefix("0;")
                        .or_else(|| buf.strip_prefix("2;"))
                    {
                        self.title = title.chars().take(24).collect();
                        // Track shell cwd via OSC 7 or title heuristics.
                    }
                    if let Some(uri) = buf.strip_prefix("7;file://") {
                        if let Some(idx) = uri.find('/') {
                            self.cwd = PathBuf::from(&uri[idx..]);
                        }
                    }
                    self.esc = EscState::None;
                } else if ch == '\x1b' {
                    self.esc = EscState::None;
                    self.esc_buf.clear();
                } else if self.esc_buf.len() < 256 {
                    self.esc_buf.push(ch);
                }
            }
        }
    }

    fn csi_params(buf: &str) -> Vec<usize> {
        buf.trim_start_matches(['?', '>'])
            .split(';')
            .map(|p| p.parse::<usize>().unwrap_or(0))
            .collect()
    }

    fn apply_csi(&mut self, buf: &str, action: char) {
        let params = Self::csi_params(buf);
        let p0 = params.first().copied().unwrap_or(0);
        let p1 = params.get(1).copied().unwrap_or(0);
        self.wrap_pending = false;
        match action {
            'A' => self.cur_y = self.cur_y.saturating_sub(p0.max(1)).max(self.scroll_top),
            'B' | 'e' => self.cur_y = (self.cur_y + p0.max(1)).min(self.rows - 1),
            'C' | 'a' => self.cur_x = (self.cur_x + p0.max(1)).min(self.cols - 1),
            'D' => self.cur_x = self.cur_x.saturating_sub(p0.max(1)),
            'E' => {
                self.cur_x = 0;
                self.cur_y = (self.cur_y + p0.max(1)).min(self.rows - 1);
            }
            'F' => {
                self.cur_x = 0;
                self.cur_y = self.cur_y.saturating_sub(p0.max(1));
            }
            'G' | '`' => self.cur_x = p0.max(1).min(self.cols) - 1,
            'H' | 'f' => {
                self.cur_y = p0.max(1).min(self.rows) - 1;
                self.cur_x = p1.max(1).min(self.cols) - 1;
            }
            'd' => self.cur_y = p0.max(1).min(self.rows) - 1,
            'J' => {
                let (sy, ey) = match p0 {
                    0 => (self.cur_y, self.rows - 1),
                    1 => (0, self.cur_y),
                    _ => (0, self.rows - 1),
                };
                for y in sy..=ey {
                    let (sx, ex) = if p0 == 0 && y == self.cur_y {
                        (self.cur_x, self.cols - 1)
                    } else if p0 == 1 && y == self.cur_y {
                        (0, self.cur_x)
                    } else {
                        (0, self.cols - 1)
                    };
                    for x in sx..=ex {
                        self.grid[y][x] = Cell::default();
                    }
                }
            }
            'K' => {
                let (sx, ex) = match p0 {
                    0 => (self.cur_x, self.cols - 1),
                    1 => (0, self.cur_x),
                    _ => (0, self.cols - 1),
                };
                for x in sx..=ex.min(self.cols - 1) {
                    self.grid[self.cur_y][x] = Cell::default();
                }
            }
            'L' => {
                let n = p0.max(1).min(self.scroll_bottom + 1 - self.cur_y);
                for _ in 0..n {
                    self.grid[self.cur_y..=self.scroll_bottom].rotate_right(1);
                    self.grid[self.cur_y] = vec![Cell::default(); self.cols];
                }
            }
            'M' => {
                let n = p0.max(1).min(self.scroll_bottom + 1 - self.cur_y);
                for _ in 0..n {
                    self.grid[self.cur_y..=self.scroll_bottom].rotate_left(1);
                    self.grid[self.scroll_bottom] = vec![Cell::default(); self.cols];
                }
            }
            'P' => {
                let n = p0.max(1).min(self.cols - self.cur_x);
                self.grid[self.cur_y].drain(self.cur_x..self.cur_x + n);
                self.grid[self.cur_y]
                    .extend(std::iter::repeat_with(Cell::default).take(n));
            }
            '@' => {
                let n = p0.max(1).min(self.cols - self.cur_x);
                for _ in 0..n {
                    self.grid[self.cur_y].insert(self.cur_x, Cell::default());
                    self.grid[self.cur_y].truncate(self.cols);
                }
            }
            'X' => {
                let n = p0.max(1).min(self.cols - self.cur_x);
                for x in self.cur_x..self.cur_x + n {
                    self.grid[self.cur_y][x] = Cell::default();
                }
            }
            'S' => {
                for _ in 0..p0.max(1) {
                    self.scroll_up();
                }
            }
            'T' => {
                for _ in 0..p0.max(1) {
                    self.scroll_down();
                }
            }
            'r' => {
                let top = p0.max(1) - 1;
                let bottom = if p1 == 0 { self.rows } else { p1 }.min(self.rows) - 1;
                if top < bottom {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                    self.cur_x = 0;
                    self.cur_y = self.scroll_top;
                }
            }
            's' => self.saved_cursor = (self.cur_x, self.cur_y),
            'u' => {
                self.cur_x = self.saved_cursor.0.min(self.cols - 1);
                self.cur_y = self.saved_cursor.1.min(self.rows - 1);
            }
            'm' => self.apply_sgr(&params),
            'h' | 'l' => {
                let enabled = action == 'h';
                if buf.starts_with('?') && matches!(p0, 9 | 1000 | 1002 | 1003 | 1006) {
                    self.mouse_enabled = enabled;
                } else if buf.starts_with('?') && p0 == 2004 {
                    self.bracketed_paste = enabled;
                }
                if buf.starts_with('?') && (p0 == 1049 || p0 == 47 || p0 == 1047) {
                    for row in self.grid.iter_mut() {
                        for cell in row.iter_mut() {
                            *cell = Cell::default();
                        }
                    }
                    self.cur_x = 0;
                    self.cur_y = 0;
                }
            }
            _ => {}
        }
    }

    fn apply_sgr(&mut self, params: &[usize]) {
        let mut i = 0;
        if params.is_empty() {
            self.fg = TERM_FG;
            self.bold = false;
        }
        while i < params.len() {
            match params[i] {
                0 => {
                    self.fg = TERM_FG;
                    self.bold = false;
                }
                1 => self.bold = true,
                22 => self.bold = false,
                30..=37 => self.fg = ansi_color(params[i] - 30, false),
                39 => self.fg = TERM_FG,
                90..=97 => self.fg = ansi_color(params[i] - 90, true),
                38 => {
                    if params.get(i + 1) == Some(&5) {
                        if let Some(&v) = params.get(i + 2) {
                            self.fg = color_256(v);
                        }
                        i += 2;
                    } else if params.get(i + 1) == Some(&2) {
                        if let (Some(&r), Some(&g), Some(&b)) =
                            (params.get(i + 2), params.get(i + 3), params.get(i + 4))
                        {
                            self.fg = Color::rgb(r as u8, g as u8, b as u8);
                        }
                        i += 4;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        unsafe {
            if !self.dead {
                libc::kill(self.pid, libc::SIGHUP);
            }
            libc::close(self.pty);
        }
    }
}

/// Read any pending data without blocking (poll with 0 timeout).
pub fn fd_readable(fd: RawFd) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut pfd, 1, 0) > 0 && pfd.revents & libc::POLLIN != 0 }
}
