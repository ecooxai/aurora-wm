//! Floating on-screen touch keyboard.
//!
//! An override-redirect window that never takes the input focus: key presses
//! are synthesized with XTEST, so they reach whichever window has the focus
//! (client applications and Aurora's own Files/terminal alike). The topbar
//! keyboard icon, `aurora-wm --toggle-keyboard` and the hide key toggle it;
//! the grip bar along its top edge drags it anywhere on screen.

use std::time::{Duration, Instant};

use rusttype::Font;
use x11rb::CURRENT_TIME;
use x11rb::connection::Connection;
use x11rb::protocol::shape::{self, ConnectionExt as ShapeConnectionExt};
use x11rb::protocol::xproto::ConnectionExt as XprotoConnectionExt;
use x11rb::protocol::xproto::*;
use x11rb::protocol::xtest::ConnectionExt as XtestConnectionExt;
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;

use crate::canvas::*;
use crate::draw_helpers::rounded_rect_shape_rects;
use crate::model::Aurora;
use crate::{AnyResult, DOCK_BOTTOM_MARGIN, DOCK_HEIGHT, TOPBAR_HEIGHT};

const COLS_Q: i32 = 60; // every row is 15 key units wide, in quarter units
const PAD: i32 = 8;
const GAP: i32 = 5;
const GRIP_H: i32 = 26;
const RADIUS: i32 = 14;
const REPEAT_DELAY: Duration = Duration::from_millis(420);
const REPEAT_RATE: Duration = Duration::from_millis(65);
const DOUBLE_TAP: Duration = Duration::from_millis(350);

const PANEL: Color = Color::rgb(236, 241, 245);
const KEY: Color = Color::rgb(255, 255, 255);
const KEY_FN: Color = Color::rgb(214, 223, 230);
const KEY_DOWN: Color = Color::rgb(160, 238, 220);
const KEY_LATCHED: Color = Color::rgb(116, 213, 198);
const KEY_SHADOW: Color = Color::rgba(32, 43, 54, 40);
const KEY_TEXT: Color = Color::rgb(32, 43, 54);
const KEY_HINT: Color = Color::rgb(120, 134, 148);

const XK_BACKSPACE: u32 = 0xff08;
const XK_TAB: u32 = 0xff09;
const XK_RETURN: u32 = 0xff0d;
const XK_ESCAPE: u32 = 0xff1b;
const XK_LEFT: u32 = 0xff51;
const XK_UP: u32 = 0xff52;
const XK_RIGHT: u32 = 0xff53;
const XK_DOWN: u32 = 0xff54;
const XK_SHIFT_L: u32 = 0xffe1;
const XK_CONTROL_L: u32 = 0xffe3;
const XK_ALT_L: u32 = 0xffe9;
const XK_SUPER_L: u32 = 0xffeb;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Act {
    /// A printable key: (normal, shifted).
    Char(char, char),
    /// A named non-printing key.
    Key(u32),
    Shift,
    Ctrl,
    Alt,
    Super,
    Hide,
}

/// (action, width in quarter key units)
type Row = &'static [(Act, i32)];

const fn c(a: char, b: char) -> (Act, i32) {
    (Act::Char(a, b), 4)
}

const ROWS: [Row; 5] = [
    &[
        c('`', '~'), c('1', '!'), c('2', '@'), c('3', '#'), c('4', '$'), c('5', '%'),
        c('6', '^'), c('7', '&'), c('8', '*'), c('9', '('), c('0', ')'), c('-', '_'),
        c('=', '+'), (Act::Key(XK_BACKSPACE), 8),
    ],
    &[
        (Act::Key(XK_TAB), 6), c('q', 'Q'), c('w', 'W'), c('e', 'E'), c('r', 'R'),
        c('t', 'T'), c('y', 'Y'), c('u', 'U'), c('i', 'I'), c('o', 'O'), c('p', 'P'),
        c('[', '{'), c(']', '}'), (Act::Char('\\', '|'), 6),
    ],
    &[
        (Act::Key(XK_ESCAPE), 7), c('a', 'A'), c('s', 'S'), c('d', 'D'), c('f', 'F'),
        c('g', 'G'), c('h', 'H'), c('j', 'J'), c('k', 'K'), c('l', 'L'), c(';', ':'),
        c('\'', '"'), (Act::Key(XK_RETURN), 9),
    ],
    &[
        (Act::Shift, 9), c('z', 'Z'), c('x', 'X'), c('c', 'C'), c('v', 'V'), c('b', 'B'),
        c('n', 'N'), c('m', 'M'), c(',', '<'), c('.', '>'), c('/', '?'),
        (Act::Key(XK_UP), 4), (Act::Shift, 7),
    ],
    &[
        (Act::Ctrl, 6), (Act::Alt, 5), (Act::Super, 5), (Act::Char(' ', ' '), 24),
        (Act::Key(XK_LEFT), 4), (Act::Key(XK_DOWN), 4), (Act::Key(XK_RIGHT), 4),
        (Act::Hide, 8),
    ],
];

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Latch {
    #[default]
    Off,
    Once,
    Locked,
}

#[derive(Clone, Copy)]
struct KeyRect {
    act: Act,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

#[derive(Default)]
pub(crate) struct TouchKeyboard {
    window: Option<Window>,
    pub(crate) visible: bool,
    pos: Option<(i16, i16)>,
    size: (u16, u16),
    shift: Latch,
    ctrl: bool,
    alt: bool,
    sup: bool,
    last_shift_tap: Option<Instant>,
    pressed: Option<usize>,
    drag: Option<(i16, i16, i16, i16)>,
    pub(crate) repeat_at: Option<Instant>,
    /// Cached keyboard mapping: (min keycode, keysyms per keycode, keysyms).
    keymap: Option<(u8, usize, Vec<u32>)>,
    /// Rendered keyboard for the current latch state.
    cache: Option<Canvas>,
}

impl TouchKeyboard {
    pub(crate) fn window_id(&self) -> Option<Window> {
        self.window
    }

    pub(crate) fn is_window(&self, window: Window) -> bool {
        self.visible && self.window == Some(window)
    }
}

fn layout(w: i32, h: i32) -> Vec<KeyRect> {
    let inner_w = w - 2 * PAD;
    let rows = ROWS.len() as i32;
    let top = PAD + GRIP_H;
    let row_h = (h - top - PAD + GAP) / rows;
    let mut keys = Vec::with_capacity(70);
    for (r, row) in ROWS.iter().enumerate() {
        let y = top + r as i32 * row_h;
        let mut q = 0;
        for &(act, wq) in row.iter() {
            let x0 = PAD + q * inner_w / COLS_Q;
            let x1 = PAD + (q + wq) * inner_w / COLS_Q;
            keys.push(KeyRect { act, x: x0, y, w: x1 - x0 - GAP, h: row_h - GAP });
            q += wq;
        }
    }
    keys
}

fn repeats(act: Act) -> bool {
    matches!(
        act,
        Act::Key(XK_BACKSPACE | XK_LEFT | XK_RIGHT | XK_UP | XK_DOWN) | Act::Char(' ', _)
    )
}

impl Aurora {
    fn osk_size(&self) -> (u16, u16) {
        let sw = i32::from(self.screen_width);
        let w = (sw - 16).clamp(300, 780);
        // Keys about as tall as they are wide, but never huge on wide screens.
        let unit = (w - 2 * PAD) * 4 / COLS_Q;
        let row_h = unit.clamp(38, 52) + GAP;
        let h = PAD + GRIP_H + row_h * ROWS.len() as i32 - GAP + PAD;
        (w as u16, h as u16)
    }

    fn osk_clamped_pos(&self, x: i32, y: i32) -> (i16, i16) {
        let (w, h) = self.osk.size;
        let max_x = (i32::from(self.screen_width) - i32::from(w)).max(0);
        let max_y = (i32::from(self.screen_height) - i32::from(h)).max(i32::from(TOPBAR_HEIGHT));
        (x.clamp(0, max_x) as i16, y.clamp(i32::from(TOPBAR_HEIGHT), max_y) as i16)
    }

    pub(crate) fn toggle_touch_keyboard(&mut self) -> AnyResult<()> {
        if self.osk.visible {
            self.hide_touch_keyboard()
        } else {
            self.show_touch_keyboard()
        }
    }

    pub(crate) fn show_touch_keyboard(&mut self) -> AnyResult<()> {
        let size = self.osk_size();
        if size != self.osk.size {
            self.osk.size = size;
            self.osk.cache = None;
        }
        let (w, h) = size;
        let pos = match self.osk.pos {
            Some((x, y)) => self.osk_clamped_pos(i32::from(x), i32::from(y)),
            None => self.osk_clamped_pos(
                (i32::from(self.screen_width) - i32::from(w)) / 2,
                i32::from(self.screen_height)
                    - i32::from(h)
                    - i32::from(DOCK_HEIGHT + DOCK_BOTTOM_MARGIN)
                    - 10,
            ),
        };
        self.osk.pos = Some(pos);
        let window = match self.osk.window {
            Some(window) => window,
            None => {
                let window = self.conn.generate_id()?;
                let aux = CreateWindowAux::new()
                    .override_redirect(1)
                    .event_mask(
                        EventMask::EXPOSURE
                            | EventMask::BUTTON_PRESS
                            | EventMask::BUTTON_RELEASE
                            | EventMask::BUTTON1_MOTION,
                    )
                    .cursor(self.cursor)
                    .background_pixel(0)
                    .backing_store(BackingStore::WHEN_MAPPED);
                self.conn.create_window(
                    self.depth,
                    window,
                    self.root,
                    pos.0,
                    pos.1,
                    w,
                    h,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    self.visual,
                    &aux,
                )?;
                // Never a focus target, whatever a window manager would think.
                let hints = [1u32, 0, 0, 0, 0, 0, 0, 0, 0];
                self.conn.change_property32(
                    PropMode::REPLACE,
                    window,
                    AtomEnum::WM_HINTS,
                    AtomEnum::WM_HINTS,
                    &hints,
                )?;
                self.conn.change_property8(
                    PropMode::REPLACE,
                    window,
                    AtomEnum::WM_NAME,
                    AtomEnum::STRING,
                    b"Aurora Keyboard",
                )?;
                self.osk.window = Some(window);
                window
            }
        };
        self.conn.configure_window(
            window,
            &ConfigureWindowAux::new()
                .x(i32::from(pos.0))
                .y(i32::from(pos.1))
                .width(u32::from(w))
                .height(u32::from(h))
                .stack_mode(StackMode::ABOVE),
        )?;
        let rects = rounded_rect_shape_rects(0, 0, w, h, RADIUS);
        self.conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::BOUNDING,
            ClipOrdering::UNSORTED,
            window,
            0,
            0,
            &rects,
        )?;
        self.osk.keymap = None;
        self.osk.visible = true;
        self.osk.pressed = None;
        self.osk.repeat_at = None;
        self.conn.map_window(window)?;
        self.conn.sync()?;
        self.redraw_touch_keyboard()?;
        self.redraw_topbar()?;
        self.conn.flush()?;
        Ok(())
    }

    pub(crate) fn hide_touch_keyboard(&mut self) -> AnyResult<()> {
        if !self.osk.visible {
            return Ok(());
        }
        self.osk_release_modifiers()?;
        self.osk.visible = false;
        self.osk.pressed = None;
        self.osk.drag = None;
        self.osk.repeat_at = None;
        self.osk.cache = None;
        if let Some(window) = self.osk.window {
            self.conn.unmap_window(window)?;
        }
        self.redraw_topbar()?;
        self.conn.flush()?;
        Ok(())
    }

    pub(crate) fn raise_touch_keyboard(&self) -> AnyResult<()> {
        if let (true, Some(window)) = (self.osk.visible, self.osk.window) {
            self.conn
                .configure_window(window, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
        }
        Ok(())
    }

    /// Whether a root-relative point falls on the visible keyboard.
    pub(crate) fn touch_keyboard_contains(&self, root_x: i16, root_y: i16) -> bool {
        let (Some((x, y)), true) = (self.osk.pos, self.osk.visible) else {
            return false;
        };
        let (w, h) = self.osk.size;
        point_in_rect(
            i32::from(root_x),
            i32::from(root_y),
            i32::from(x),
            i32::from(y),
            i32::from(w),
            i32::from(h),
        )
    }

    // ------------------------------------------------------------- drawing

    fn osk_label(&self, act: Act) -> (String, Option<String>) {
        let shifted = self.osk.shift != Latch::Off;
        match act {
            Act::Char(' ', _) => (String::new(), None),
            Act::Char(a, b) if a.is_ascii_alphabetic() => {
                ((if shifted { b } else { a }).to_string(), None)
            }
            Act::Char(a, b) => {
                if shifted {
                    (b.to_string(), Some(a.to_string()))
                } else {
                    (a.to_string(), Some(b.to_string()))
                }
            }
            Act::Key(XK_TAB) => ("Tab".into(), None),
            Act::Key(XK_ESCAPE) => ("Esc".into(), None),
            Act::Ctrl => ("Ctrl".into(), None),
            Act::Alt => ("Alt".into(), None),
            Act::Super => ("Super".into(), None),
            _ => (String::new(), None),
        }
    }

    fn osk_key_color(&self, act: Act, pressed: bool) -> Color {
        if pressed {
            return KEY_DOWN;
        }
        let latched = match act {
            Act::Shift => self.osk.shift != Latch::Off,
            Act::Ctrl => self.osk.ctrl,
            Act::Alt => self.osk.alt,
            Act::Super => self.osk.sup,
            _ => false,
        };
        if latched {
            KEY_LATCHED
        } else if matches!(act, Act::Char(..)) {
            KEY
        } else {
            KEY_FN
        }
    }

    fn osk_draw_key(&self, cv: &mut Canvas, k: &KeyRect, ox: i32, oy: i32, pressed: bool) {
        let (x, y, w, h) = (k.x - ox, k.y - oy, k.w, k.h);
        cv.draw_round_rect(x, y + 1, w, h, 7, KEY_SHADOW);
        cv.draw_round_rect(x, y, w, h, 7, self.osk_key_color(k.act, pressed));
        let cx = x + w / 2;
        let cy = y + h / 2;
        let ink = KEY_TEXT;
        match k.act {
            Act::Key(XK_BACKSPACE) => {
                // An arrow-shaped tag with a cross.
                let l = cx - 13;
                let r = cx + 12;
                cv.draw_line(l, cy, l + 8, cy - 8, 2, ink);
                cv.draw_line(l, cy, l + 8, cy + 8, 2, ink);
                cv.draw_line(l + 8, cy - 8, r, cy - 8, 2, ink);
                cv.draw_line(l + 8, cy + 8, r, cy + 8, 2, ink);
                cv.draw_line(r, cy - 8, r, cy + 8, 2, ink);
                cv.draw_line(cx - 2, cy - 4, cx + 6, cy + 4, 2, ink);
                cv.draw_line(cx - 2, cy + 4, cx + 6, cy - 4, 2, ink);
            }
            Act::Key(XK_RETURN) => {
                cv.draw_line(cx + 10, cy - 8, cx + 10, cy + 3, 2, ink);
                cv.draw_line(cx + 10, cy + 3, cx - 10, cy + 3, 2, ink);
                cv.draw_line(cx - 10, cy + 3, cx - 4, cy - 3, 2, ink);
                cv.draw_line(cx - 10, cy + 3, cx - 4, cy + 9, 2, ink);
            }
            Act::Shift => {
                let filled = self.osk.shift == Latch::Locked;
                cv.draw_line(cx, cy - 10, cx - 9, cy, 2, ink);
                cv.draw_line(cx, cy - 10, cx + 9, cy, 2, ink);
                cv.draw_line(cx - 9, cy, cx - 4, cy, 2, ink);
                cv.draw_line(cx + 9, cy, cx + 4, cy, 2, ink);
                cv.draw_line(cx - 4, cy, cx - 4, cy + 8, 2, ink);
                cv.draw_line(cx + 4, cy, cx + 4, cy + 8, 2, ink);
                cv.draw_line(cx - 4, cy + 8, cx + 4, cy + 8, 2, ink);
                if filled {
                    cv.draw_rect(cx - 4, cy + 11, 9, 2, ink);
                }
            }
            Act::Key(dir @ (XK_LEFT | XK_RIGHT | XK_UP | XK_DOWN)) => {
                let (dx, dy) = match dir {
                    XK_LEFT => (-1, 0),
                    XK_RIGHT => (1, 0),
                    XK_UP => (0, -1),
                    _ => (0, 1),
                };
                let tip = (cx + dx * 7, cy + dy * 7);
                let a = (cx - dx * 4 + dy * 7, cy - dy * 4 + dx * 7);
                let b = (cx - dx * 4 - dy * 7, cy - dy * 4 - dx * 7);
                cv.draw_line(tip.0, tip.1, a.0, a.1, 3, ink);
                cv.draw_line(tip.0, tip.1, b.0, b.1, 3, ink);
            }
            Act::Hide => {
                // A small keyboard over a down chevron.
                cv.draw_round_rect(cx - 13, cy - 11, 26, 15, 3, ink);
                for i in 0..4 {
                    cv.draw_rect(cx - 9 + i * 5, cy - 8, 3, 3, KEY_FN);
                }
                cv.draw_rect(cx - 7, cy - 2, 14, 2, KEY_FN);
                cv.draw_line(cx - 6, cy + 7, cx, cy + 12, 2, ink);
                cv.draw_line(cx + 6, cy + 7, cx, cy + 12, 2, ink);
            }
            Act::Char(' ', _) => {
                cv.draw_rect(cx - 30, cy + 6, 60, 2, KEY_HINT);
            }
            act => {
                let (label, hint) = self.osk_label(act);
                let alpha = matches!(act, Act::Char(a, _) if a.is_ascii_alphabetic());
                let size = if label.chars().count() > 1 {
                    15.0
                } else if alpha {
                    22.0
                } else {
                    20.0
                };
                let font: &Font<'static> = if label.chars().count() > 1 { &self.bold } else { &self.regular };
                let th = (size * 0.72) as i32;
                let ty = cy - th / 2 - (size * 0.28) as i32;
                cv.draw_text_center(font, &label, cx, ty, size, ink);
                if let Some(hint) = hint {
                    cv.draw_text_right(&self.regular, &hint, x + w - 5, y + 2, 12.0, KEY_HINT);
                }
            }
        }
    }

    fn osk_render(&self) -> Canvas {
        let (w, h) = self.osk.size;
        let mut cv = Canvas::new(w, h, PANEL);
        // Grip: a pill in the middle of the top strip.
        let gw = 54;
        cv.draw_round_rect(i32::from(w) / 2 - gw / 2, PAD + 7, gw, 6, 3, Color::rgb(176, 189, 200));
        for k in layout(i32::from(w), i32::from(h)).iter() {
            self.osk_draw_key(&mut cv, k, 0, 0, false);
        }
        cv
    }

    pub(crate) fn redraw_touch_keyboard(&mut self) -> AnyResult<()> {
        let Some(window) = self.osk.window.filter(|_| self.osk.visible) else {
            return Ok(());
        };
        if self.osk.cache.is_none() {
            self.osk.cache = Some(self.osk_render());
        }
        if let Some(cache) = self.osk.cache.as_ref() {
            self.upload_canvas(window, cache)?;
        }
        if let Some(idx) = self.osk.pressed {
            self.osk_redraw_key(idx, true)?;
        }
        Ok(())
    }

    /// Repaint one key in place (press feedback without a full redraw).
    fn osk_redraw_key(&self, idx: usize, pressed: bool) -> AnyResult<()> {
        let Some(window) = self.osk.window else { return Ok(()) };
        let (w, h) = self.osk.size;
        let keys = layout(i32::from(w), i32::from(h));
        let Some(k) = keys.get(idx) else { return Ok(()) };
        let (bx, by, bw, bh) = (k.x - 1, k.y - 1, k.w + 2, k.h + 3);
        let mut cv = Canvas::new(bw as u16, bh as u16, PANEL);
        self.osk_draw_key(&mut cv, k, bx, by, pressed);
        self.upload_canvas_at(window, &cv, bx, by)
    }

    // ------------------------------------------------------------- input

    pub(crate) fn handle_touch_keyboard_press(&mut self, ev: ButtonPressEvent) -> AnyResult<()> {
        if ev.detail != 1 {
            return Ok(());
        }
        let (x, y) = (i32::from(ev.event_x), i32::from(ev.event_y));
        let (w, h) = self.osk.size;
        if y < PAD + GRIP_H - GAP {
            let (wx, wy) = self.osk.pos.unwrap_or((0, 0));
            self.osk.drag = Some((ev.root_x, ev.root_y, wx, wy));
            return Ok(());
        }
        let keys = layout(i32::from(w), i32::from(h));
        // Gaps belong to the nearest key, so a fat finger never lands on nothing.
        let Some((idx, k)) = keys
            .iter()
            .enumerate()
            .filter(|(_, k)| y >= k.y - GAP && y < k.y + k.h + GAP)
            .min_by_key(|(_, k)| {
                let dx = (x - (k.x + k.w / 2)).abs() - k.w / 2;
                let dy = (y - (k.y + k.h / 2)).abs() - k.h / 2;
                dx.max(0) + dy.max(0)
            })
        else {
            return Ok(());
        };
        let act = k.act;
        if let Some(prev) = self.osk.pressed.take() {
            self.osk_redraw_key(prev, false)?;
        }
        self.osk.pressed = Some(idx);
        self.osk_redraw_key(idx, true)?;
        self.conn.flush()?;
        match act {
            Act::Shift => {
                let now = Instant::now();
                let double = self.osk.last_shift_tap.is_some_and(|t| now - t < DOUBLE_TAP);
                self.osk.shift = match (self.osk.shift, double) {
                    (Latch::Off, _) => Latch::Once,
                    (Latch::Once, true) => Latch::Locked,
                    _ => Latch::Off,
                };
                self.osk.last_shift_tap = Some(now);
                self.osk_relatch()?;
            }
            Act::Ctrl => {
                self.osk.ctrl = !self.osk.ctrl;
                self.osk_relatch()?;
            }
            Act::Alt => {
                self.osk.alt = !self.osk.alt;
                self.osk_relatch()?;
            }
            Act::Super => {
                self.osk.sup = !self.osk.sup;
                self.osk_relatch()?;
            }
            Act::Hide => {}
            act => {
                self.osk_send(act)?;
                if repeats(act) {
                    self.osk.repeat_at = Some(Instant::now() + REPEAT_DELAY);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn handle_touch_keyboard_release(&mut self, _ev: ButtonReleaseEvent) -> AnyResult<()> {
        self.osk.repeat_at = None;
        if self.osk.drag.take().is_some() {
            return Ok(());
        }
        let Some(idx) = self.osk.pressed.take() else { return Ok(()) };
        let (w, h) = self.osk.size;
        let act = layout(i32::from(w), i32::from(h)).get(idx).map(|k| k.act);
        if act == Some(Act::Hide) {
            return self.hide_touch_keyboard();
        }
        self.osk_redraw_key(idx, false)?;
        self.conn.flush()?;
        Ok(())
    }

    pub(crate) fn handle_touch_keyboard_motion(&mut self, ev: MotionNotifyEvent) -> AnyResult<()> {
        let Some((rx, ry, wx, wy)) = self.osk.drag else { return Ok(()) };
        let nx = i32::from(wx) + i32::from(ev.root_x) - i32::from(rx);
        let ny = i32::from(wy) + i32::from(ev.root_y) - i32::from(ry);
        let pos = self.osk_clamped_pos(nx, ny);
        if Some(pos) != self.osk.pos {
            self.osk.pos = Some(pos);
            if let Some(window) = self.osk.window {
                self.conn.configure_window(
                    window,
                    &ConfigureWindowAux::new().x(i32::from(pos.0)).y(i32::from(pos.1)),
                )?;
                self.conn.flush()?;
            }
        }
        Ok(())
    }

    /// Key repeat for held Backspace, arrows and Space. Returns true when it acted.
    pub(crate) fn touch_keyboard_tick(&mut self) -> AnyResult<bool> {
        let Some(at) = self.osk.repeat_at else { return Ok(false) };
        if Instant::now() < at {
            return Ok(false);
        }
        // A lost release must not repeat forever.
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        if u16::from(pointer.mask) & u16::from(KeyButMask::BUTTON1) == 0 {
            self.osk.repeat_at = None;
            if let Some(idx) = self.osk.pressed.take() {
                self.osk_redraw_key(idx, false)?;
            }
            return Ok(true);
        }
        let (w, h) = self.osk.size;
        let act = self
            .osk
            .pressed
            .and_then(|idx| layout(i32::from(w), i32::from(h)).get(idx).map(|k| k.act));
        match act {
            Some(act) if repeats(act) => {
                self.osk_send(act)?;
                self.osk.repeat_at = Some(Instant::now() + REPEAT_RATE);
            }
            _ => self.osk.repeat_at = None,
        }
        Ok(true)
    }

    fn osk_relatch(&mut self) -> AnyResult<()> {
        self.osk.cache = None;
        self.redraw_touch_keyboard()?;
        self.conn.flush()?;
        Ok(())
    }

    fn osk_release_modifiers(&mut self) -> AnyResult<()> {
        let changed = self.osk.ctrl || self.osk.alt || self.osk.sup || self.osk.shift != Latch::Off;
        self.osk.ctrl = false;
        self.osk.alt = false;
        self.osk.sup = false;
        self.osk.shift = Latch::Off;
        if changed {
            self.osk.cache = None;
        }
        Ok(())
    }

    // ------------------------------------------------------------- XTEST

    fn osk_keymap(&mut self) -> AnyResult<&(u8, usize, Vec<u32>)> {
        if self.osk.keymap.is_none() {
            let setup = self.conn.setup();
            let min = setup.min_keycode;
            let count = setup.max_keycode.saturating_sub(min).saturating_add(1);
            let mapping = self.conn.get_keyboard_mapping(min, count)?.reply()?;
            self.osk.keymap = Some((min, usize::from(mapping.keysyms_per_keycode), mapping.keysyms));
        }
        Ok(self.osk.keymap.as_ref().unwrap())
    }

    /// The keycode for a keysym and whether Shift selects it (column 1).
    fn osk_lookup(&mut self, keysym: u32) -> AnyResult<Option<(u8, bool)>> {
        let (min, per, syms) = self.osk_keymap()?;
        let (min, per) = (*min, (*per).max(1));
        let mut found = None;
        for (i, chunk) in syms.chunks(per).enumerate() {
            if chunk.first() == Some(&keysym) {
                found = Some((min.saturating_add(i as u8), false));
                break;
            }
            if found.is_none() && chunk.get(1) == Some(&keysym) {
                found = Some((min.saturating_add(i as u8), true));
            }
        }
        if found.is_some() {
            return Ok(found);
        }
        // Not in the layout: borrow an unused keycode for it, as xdotool does.
        let spare = syms
            .chunks(per)
            .enumerate()
            .rev()
            .find(|(_, chunk)| chunk.iter().all(|&s| s == 0))
            .map(|(i, _)| min.saturating_add(i as u8));
        let Some(code) = spare else { return Ok(None) };
        let mut row = vec![0u32; per];
        row[0] = keysym;
        if per > 1 {
            row[1] = keysym;
        }
        self.conn.change_keyboard_mapping(1, code, per as u8, &row)?;
        self.conn.sync()?;
        self.osk.keymap = None;
        Ok(Some((code, false)))
    }

    fn osk_fake(&self, press: bool, code: u8) -> AnyResult<()> {
        let kind = if press { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT };
        self.conn.xtest_fake_input(kind, code, CURRENT_TIME, self.root, 0, 0, 0)?;
        Ok(())
    }

    fn osk_send(&mut self, act: Act) -> AnyResult<()> {
        let shifted = self.osk.shift != Latch::Off;
        let (keysym, extra_shift) = match act {
            Act::Char(a, b) => {
                let ch = if shifted { b } else { a };
                (char_keysym(ch), false)
            }
            Act::Key(sym) => (sym, shifted),
            _ => return Ok(()),
        };
        let Some((code, needs_shift)) = self.osk_lookup(keysym)? else {
            return Ok(());
        };
        let mut mods = Vec::new();
        if self.osk.ctrl {
            mods.push(XK_CONTROL_L);
        }
        if self.osk.alt {
            mods.push(XK_ALT_L);
        }
        if self.osk.sup {
            mods.push(XK_SUPER_L);
        }
        if needs_shift || extra_shift {
            mods.push(XK_SHIFT_L);
        }
        let mut mod_codes = Vec::new();
        for m in mods {
            if let Some((mc, _)) = self.osk_lookup(m)? {
                mod_codes.push(mc);
            }
        }
        for &mc in &mod_codes {
            self.osk_fake(true, mc)?;
        }
        self.osk_fake(true, code)?;
        self.osk_fake(false, code)?;
        for &mc in mod_codes.iter().rev() {
            self.osk_fake(false, mc)?;
        }
        // One-shot latches end after a real key.
        let relatch = self.osk.ctrl || self.osk.alt || self.osk.sup || self.osk.shift == Latch::Once;
        self.osk.ctrl = false;
        self.osk.alt = false;
        self.osk.sup = false;
        if self.osk.shift == Latch::Once {
            self.osk.shift = Latch::Off;
        }
        if relatch {
            self.osk.cache = None;
            self.redraw_touch_keyboard()?;
        }
        self.conn.flush()?;
        Ok(())
    }
}

fn char_keysym(ch: char) -> u32 {
    match ch {
        ' ' => 0x20,
        c if (c as u32) < 0x100 => c as u32,
        c => 0x0100_0000 | c as u32,
    }
}

/// The topbar's keyboard toggle icon.
pub(crate) fn draw_keyboard_icon(c: &mut Canvas, cx: i32, cy: i32, active: bool) {
    let body = if active { MINT_LIGHT } else { Color::rgb(175, 218, 245) };
    let keys = Color::rgba(23, 34, 42, 150);
    let s = TOPBAR_ICON_SIZE;
    if active {
        c.draw_round_rect(cx - s / 2 - 4, cy - s / 2 - 4, s + 8, s + 8, 8, Color::rgba(160, 238, 220, 60));
    }
    // Square tile like the other topbar icons: two rows of keys + space bar.
    c.draw_round_rect(cx - s / 2, cy - s / 2, s, s, 5, body);
    for row in 0..2 {
        for col in 0..4 {
            c.draw_rect(cx - 9 + col * 5, cy - 6 + row * 5, 3, 3, keys);
        }
    }
    c.draw_rect(cx - 6, cy + 4, 12, 3, keys);
}
