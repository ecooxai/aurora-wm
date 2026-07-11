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
    pub(crate) fn redraw_folder_terminal(&self) -> AnyResult<()> {
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

    pub(crate) fn folder_terminal_display_rows(&self, visible_rows: usize) -> Vec<String> {
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

    pub(crate) fn folder_terminal_font_size(&self) -> f32 {
        13.0 + f32::from(self.folder_terminal.zoom)
    }

    pub(crate) fn folder_terminal_cell_w(&self) -> i32 {
        measure_text(
            &self.terminal_regular,
            "A",
            self.folder_terminal_font_size(),
        )
        .max(6)
    }

    pub(crate) fn folder_terminal_cell_h(&self) -> i32 {
        (FOLDER_TERMINAL_CELL_H + i32::from(self.folder_terminal.zoom) * 2).max(12)
    }

    pub(crate) fn handle_folder_terminal_key(&mut self, ev: KeyPressEvent) -> AnyResult<()> {
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

    pub(crate) fn handle_folder_terminal_scroll(&mut self, button: u8) -> AnyResult<()> {
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

    pub(crate) fn handle_folder_terminal_click(&mut self, x: i32, y: i32) -> AnyResult<()> {
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

    pub(crate) fn handle_folder_terminal_motion(
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

    pub(crate) fn handle_folder_terminal_release(&mut self) -> AnyResult<()> {
        self.folder_terminal_selecting = false;
        self.erase_folder_terminal_live_selection()?;
        if let Some(selection) = self.folder_terminal_selection {
            let rows = self.folder_terminal_display_rows(self.folder_terminal.rows);
            let text = selected_terminal_text(selection, &rows);
            if text.chars().count() >= 3 {
                copy_text_to_clipboard(&text);
            }
        }
        self.redraw_folder_terminal()
    }

    pub(crate) fn erase_folder_terminal_live_selection(&mut self) -> AnyResult<()> {
        if self.folder_terminal_live_rects.is_empty() {
            return Ok(());
        }
        let rects = std::mem::take(&mut self.folder_terminal_live_rects);
        self.draw_xor_rects(self.ui.folder_terminal, &rects)?;
        Ok(())
    }

    pub(crate) fn update_folder_terminal_live_selection(&mut self) -> AnyResult<()> {
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

    pub(crate) fn ensure_folder_terminal_pty(&mut self) {
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

    pub(crate) fn sync_folder_terminal_size(&mut self) {
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

    pub(crate) fn resize_folder_terminal_screen(&mut self, cols: usize, rows: usize) {
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

    pub(crate) fn resize_folder_terminal_pty(&mut self) {
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

    pub(crate) fn write_folder_terminal(&mut self, bytes: &[u8]) {
        if let Some(fd) = self.folder_terminal.master_fd {
            unsafe {
                let _ = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
            }
        }
    }

    pub(crate) fn poll_folder_terminal(&mut self) -> AnyResult<bool> {
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

    pub(crate) fn draw_terminal_message(&mut self, message: &str) {
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

    pub(crate) fn feed_folder_terminal(&mut self, text: &str) {
        for ch in text.chars() {
            self.feed_terminal_char(ch);
        }
    }

    pub(crate) fn feed_terminal_char(&mut self, ch: char) {
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

    pub(crate) fn feed_terminal_escape(&mut self, ch: char) {
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

    pub(crate) fn apply_terminal_csi(&mut self, body: &str) {
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

    pub(crate) fn save_terminal_cursor(&mut self) {
        self.folder_terminal.saved_cursor_x = self.folder_terminal.cursor_x;
        self.folder_terminal.saved_cursor_y = self.folder_terminal.cursor_y;
        self.folder_terminal.saved_line_drawing = self.folder_terminal.line_drawing;
    }

    pub(crate) fn restore_terminal_cursor(&mut self) {
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

    pub(crate) fn reset_terminal_emulation(&mut self) {
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

    pub(crate) fn set_terminal_alt_screen(&mut self, enabled: bool, save_cursor: bool) {
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

    pub(crate) fn terminal_insert_blanks(&mut self, count: usize) {
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

    pub(crate) fn terminal_insert_lines(&mut self, count: usize) {
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

    pub(crate) fn terminal_delete_lines(&mut self, count: usize) {
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

    pub(crate) fn terminal_scroll_up(&mut self, count: usize) {
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

    pub(crate) fn terminal_scroll_down(&mut self, count: usize) {
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

    pub(crate) fn terminal_reverse_index(&mut self) {
        if self.folder_terminal.cursor_y == self.folder_terminal.scroll_top {
            self.terminal_scroll_down(1);
        } else {
            self.folder_terminal.cursor_y = self.folder_terminal.cursor_y.saturating_sub(1);
        }
    }

    pub(crate) fn terminal_put_char(&mut self, ch: char) {
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

    pub(crate) fn terminal_linefeed(&mut self) {
        if self.folder_terminal.cursor_y >= self.folder_terminal.scroll_bottom {
            self.terminal_scroll_up(1);
        } else {
            self.folder_terminal.cursor_y += 1;
        }
    }

    pub(crate) fn terminal_newline(&mut self) {
        self.folder_terminal.cursor_x = 0;
        self.terminal_linefeed();
    }

}
