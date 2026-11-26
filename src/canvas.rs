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

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Color {
    pub(crate) r: u8,
    pub(crate) g: u8,
    pub(crate) b: u8,
    pub(crate) a: u8,
}

impl Color {
    pub(crate) const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub(crate) const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
}

pub(crate) const INK: Color = Color::rgb(32, 43, 54);
pub(crate) const MUTED: Color = Color::rgb(105, 118, 132);
pub(crate) const SOFT_INK: Color = Color::rgb(74, 88, 103);
pub(crate) const MINT_DARK: Color = Color::rgb(29, 145, 137);
pub(crate) const MINT_LIGHT: Color = Color::rgb(160, 238, 220);
pub(crate) const BLUE: Color = Color::rgb(73, 156, 231);
pub(crate) const BLUE_LIGHT: Color = Color::rgb(175, 218, 245);
pub(crate) const RED_LIGHT: Color = Color::rgb(255, 174, 184);
pub(crate) const RECORD_CAMERA_HEIGHT: i32 = 16;
/// Side of every square topbar icon tile (keyboard, clipboard, screenshot,
/// recording, display), in logical pixels.
pub(crate) const TOPBAR_ICON_SIZE: i32 = 22;
/// Side of the square topbar recording button; same as its neighbours.
pub(crate) const TOPBAR_RECORD_SIZE: i32 = TOPBAR_ICON_SIZE;
/// Very light red of the idle record dot.
pub(crate) const RECORD_DOT_LIGHT: Color = Color::rgb(248, 180, 180);
/// Fill of the recording button while capturing.
pub(crate) const RECORD_GREEN: Color = Color::rgb(86, 206, 120);
pub(crate) const CARD_LINE: Color = Color::rgba(198, 214, 224, 130);
pub(crate) const TOPBAR_ICON_SPACING: i32 = 33;
pub(crate) const TOPBAR_ICON_HIT_RADIUS: i32 = 18;
pub(crate) const CLIPBOARD_HISTORY_LIMIT: usize = 200;
pub(crate) const CLIPBOARD_MENU_VISIBLE_ROWS: usize = 10;
pub(crate) const CLIPBOARD_MENU_WIDTH: u16 = 420;
pub(crate) const CLIPBOARD_MENU_TEXT_ROW_HEIGHT: i32 = 58;
pub(crate) const CLIPBOARD_MENU_IMAGE_ROW_HEIGHT: i32 = 62;
pub(crate) const CLIPBOARD_MENU_IMAGE_PREVIEW_W: i32 = 184;
pub(crate) const CLIPBOARD_MENU_IMAGE_PREVIEW_H: i32 = 48;
pub(crate) const CLIPBOARD_MENU_NAV_Y: i32 = 8;
pub(crate) const CLIPBOARD_MENU_NAV_W: i32 = 36;
pub(crate) const CLIPBOARD_MENU_NAV_H: i32 = 28;
/// Top of the selection preview pill; the shortcut row and page nav sit above it.
pub(crate) const CLIPBOARD_MENU_PREVIEW_Y: i32 = 44;
/// Top of the first history row's background.
pub(crate) const CLIPBOARD_MENU_LIST_TOP: i32 = 80;
pub(crate) const DEFAULT_WORKSPACE_COUNT: usize = 2;
pub(crate) const MAX_WORKSPACE_COUNT: usize = 8;
pub(crate) const WORKSPACE_STRIDE: i32 = 34;
pub(crate) const WORKSPACE_SIZE: i32 = 24;
pub(crate) const FOLDER_DEFAULT_WIDTH: u16 = 330;
pub(crate) const FOLDER_DEFAULT_HEIGHT: u16 = 220;
pub(crate) const FOLDER_MIN_WIDTH: u16 = 260;
pub(crate) const FOLDER_MIN_HEIGHT: u16 = 160;
pub(crate) const TERMINAL_MIN_WIDTH: u16 = 260;
pub(crate) const TERMINAL_MIN_HEIGHT: u16 = 120;
pub(crate) const TERMINAL_DEFAULT_WIDTH: u16 = FOLDER_DEFAULT_WIDTH;

/// A drawing surface with a logical coordinate system (`width` x `height`)
/// backed by a physical pixel buffer (`pw` x `ph`, BGRX) at the global UI
/// scale. All drawing calls take logical coordinates; shapes and text are
/// rasterised at physical resolution so they stay sharp on HiDPI panels.
#[derive(Clone)]
pub(crate) struct Canvas {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) pw: u16,
    pub(crate) ph: u16,
    pub(crate) s: f32,
    pub(crate) data: Vec<u8>,
}

impl Canvas {
    pub(crate) fn new(width: u16, height: u16, color: Color) -> Self {
        let s = crate::scale::ui_scale();
        let pw = crate::scale::px_len(u32::from(width)).min(u32::from(u16::MAX)) as u16;
        let ph = crate::scale::px_len(u32::from(height)).min(u32::from(u16::MAX)) as u16;
        let mut data = vec![0; usize::from(pw) * usize::from(ph) * 4];
        for px in data.chunks_exact_mut(4) {
            px[0] = color.b;
            px[1] = color.g;
            px[2] = color.r;
            px[3] = 0;
        }
        Self {
            width,
            height,
            pw,
            ph,
            s,
            data,
        }
    }

    /// Crops a region of the screen-sized wallpaper buffer. The buffer is at
    /// physical resolution (`px_len(screen_width)` pixels per row).
    pub(crate) fn from_wallpaper_crop(
        wallpaper: &[u8],
        screen_width: u16,
        x: i32,
        y: i32,
        width: u16,
        height: u16,
    ) -> Self {
        let mut canvas = Self::new(width, height, Color::rgb(238, 247, 252));
        let spw = crate::scale::px_len(u32::from(screen_width)) as usize;
        let (x0, y0) = (canvas.px(x), canvas.px(y));
        let row_bytes = usize::from(canvas.pw) * 4;
        for yy in 0..i32::from(canvas.ph) {
            let sy = y0 + yy;
            if sy < 0 {
                continue;
            }
            // Copy the visible run of this row in one go.
            let start_x = (-x0).max(0);
            let end_x = i32::from(canvas.pw).min(spw as i32 - x0);
            if end_x <= start_x {
                continue;
            }
            let src = (sy as usize * spw + (x0 + start_x) as usize) * 4;
            let len = (end_x - start_x) as usize * 4;
            let dst = yy as usize * row_bytes + start_x as usize * 4;
            if src + len <= wallpaper.len() {
                canvas.data[dst..dst + len].copy_from_slice(&wallpaper[src..src + len]);
            }
        }
        canvas
    }

    /// Logical coordinate to this canvas's physical pixel grid.
    #[inline]
    pub(crate) fn px(&self, v: i32) -> i32 {
        (v as f32 * self.s).round() as i32
    }

    /// Logical pixel-index coordinate to the physical continuous position of
    /// that pixel's centre, expressed as a physical pixel-index coordinate.
    #[inline]
    fn pc(&self, v: f32) -> f32 {
        (v + 0.5) * self.s - 0.5
    }

    #[inline]
    fn pidx(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x >= i32::from(self.pw) || y >= i32::from(self.ph) {
            return None;
        }
        Some((y as usize * usize::from(self.pw) + x as usize) * 4)
    }

    /// Blends one physical pixel.
    #[inline]
    pub(crate) fn blend_phys(&mut self, x: i32, y: i32, color: Color) {
        let Some(i) = self.pidx(x, y) else {
            return;
        };
        if color.a == 255 {
            self.data[i] = color.b;
            self.data[i + 1] = color.g;
            self.data[i + 2] = color.r;
            self.data[i + 3] = 0;
            return;
        }
        if color.a == 0 {
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

    /// Blends one logical pixel (a block of physical pixels). Used by image
    /// painters that walk source pixels in logical space.
    pub(crate) fn blend_pixel(&mut self, x: i32, y: i32, color: Color) {
        if self.s == 1.0 {
            self.blend_phys(x, y, color);
            return;
        }
        let (x0, x1) = (self.px(x), self.px(x + 1));
        let (y0, y1) = (self.px(y), self.px(y + 1));
        for yy in y0..y1.max(y0 + 1) {
            for xx in x0..x1.max(x0 + 1) {
                self.blend_phys(xx, yy, color);
            }
        }
    }

    /// Writes a raw BGRA logical pixel without blending.
    pub(crate) fn set_pixel_raw(&mut self, x: i32, y: i32, bgra: [u8; 4]) {
        let (x0, x1) = (self.px(x), self.px(x + 1));
        let (y0, y1) = (self.px(y), self.px(y + 1));
        for yy in y0..y1.max(y0 + 1) {
            for xx in x0..x1.max(x0 + 1) {
                if let Some(i) = self.pidx(xx, yy) {
                    self.data[i..i + 4].copy_from_slice(&bgra);
                }
            }
        }
    }

    /// Paints an RGBA image into the logical rectangle (x, y, w, h), sampling
    /// the source at physical resolution so photos and icons stay detailed.
    pub(crate) fn paint_rgba_image(
        &mut self,
        src: &[u8],
        src_w: u32,
        src_h: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        opaque: bool,
    ) {
        if w <= 0 || h <= 0 || src_w == 0 || src_h == 0 {
            return;
        }
        let (x0, pw) = crate::scale::px_span(x, w);
        let (y0, ph) = crate::scale::px_span(y, h);
        for yy in 0..ph {
            let sy = ((yy as u64 * u64::from(src_h)) / ph.max(1) as u64).min(u64::from(src_h) - 1) as usize;
            for xx in 0..pw {
                let sx = ((xx as u64 * u64::from(src_w)) / pw.max(1) as u64).min(u64::from(src_w) - 1) as usize;
                let i = (sy * src_w as usize + sx) * 4;
                if i + 3 >= src.len() {
                    continue;
                }
                let a = if opaque { 255 } else { src[i + 3] };
                self.blend_phys(x0 + xx, y0 + yy, Color::rgba(src[i], src[i + 1], src[i + 2], a));
            }
        }
    }

    /// Calls `coverage(fx, fy)` for every physical pixel inside the logical
    /// box, with (fx, fy) in logical pixel-index space, and blends `color` by
    /// the returned coverage. Lets custom shapes rasterise at full resolution.
    pub(crate) fn shade<F: Fn(f32, f32) -> f32>(
        &mut self,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        color: Color,
        coverage: F,
    ) {
        let s = self.s;
        let (px0, px1) = (self.px(x0).max(0), self.px(x1).min(i32::from(self.pw)));
        let (py0, py1) = (self.px(y0).max(0), self.px(y1).min(i32::from(self.ph)));
        for py in py0..py1 {
            let fy = (py as f32 + 0.5) / s - 0.5;
            for px in px0..px1 {
                let fx = (px as f32 + 0.5) / s - 0.5;
                let cov = coverage(fx, fy);
                if cov > 0.0 {
                    let mut c = color;
                    c.a = (f32::from(color.a) * cov.min(1.0)).round() as u8;
                    self.blend_phys(px, py, c);
                }
            }
        }
    }

    pub(crate) fn draw_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color) {
        if w <= 0 || h <= 0 {
            return;
        }
        let (px0, pw) = crate::scale::px_span(x, w);
        let (py0, ph) = crate::scale::px_span(y, h);
        let x0 = px0.max(0);
        let y0 = py0.max(0);
        let x1 = (px0 + pw).min(i32::from(self.pw));
        let y1 = (py0 + ph).min(i32::from(self.ph));
        for yy in y0..y1 {
            for xx in x0..x1 {
                self.blend_phys(xx, yy, color);
            }
        }
    }

    pub(crate) fn draw_round_rect(&mut self, x: i32, y: i32, w: i32, h: i32, radius: i32, color: Color) {
        if w <= 0 || h <= 0 {
            return;
        }
        let (x, w) = crate::scale::px_span(x, w);
        let (y, h) = crate::scale::px_span(y, h);
        let radius = self.px(radius);
        let r = radius.max(0).min(w / 2).min(h / 2);
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(i32::from(self.pw));
        let y1 = (y + h).min(i32::from(self.ph));
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
                    self.blend_phys(xx, yy, blended);
                }
            }
        }
    }

    pub(crate) fn draw_circle(&mut self, cx: i32, cy: i32, radius: i32, color: Color) {
        let fcx = self.pc(cx as f32);
        let fcy = self.pc(cy as f32);
        let r = radius as f32 * self.s;
        let x_start = ((fcx - r - 1.0).floor() as i32).max(0);
        let x_end = ((fcx + r + 1.0).ceil() as i32).min(i32::from(self.pw) - 1);
        let y_start = ((fcy - r - 1.0).floor() as i32).max(0);
        let y_end = ((fcy + r + 1.0).ceil() as i32).min(i32::from(self.ph) - 1);

        for y in y_start..=y_end {
            for x in x_start..=x_end {
                let dx = x as f32 - fcx;
                let dy = y as f32 - fcy;
                let d = (dx * dx + dy * dy).sqrt();

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
                    self.blend_phys(x, y, blended);
                }
            }
        }
    }

    pub(crate) fn draw_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, thickness: i32, color: Color) {
        let (fx0, fy0) = (self.pc(x0 as f32), self.pc(y0 as f32));
        let (fx1, fy1) = (self.pc(x1 as f32), self.pc(y1 as f32));
        let t_phys = thickness as f32 * self.s;
        let pad = t_phys + 2.0;
        let x_start = ((fx0.min(fx1) - pad).floor() as i32).max(0);
        let x_end = ((fx0.max(fx1) + pad).ceil() as i32).min(i32::from(self.pw));
        let y_start = ((fy0.min(fy1) - pad).floor() as i32).max(0);
        let y_end = ((fy0.max(fy1) + pad).ceil() as i32).min(i32::from(self.ph));

        let dx = fx1 - fx0;
        let dy = fy1 - fy0;
        let len2 = dx * dx + dy * dy;

        let r = t_phys / 2.0;

        for y in y_start..y_end {
            for x in x_start..x_end {
                let t = if len2 == 0.0 {
                    0.0
                } else {
                    (((x as f32 - fx0) * dx + (y as f32 - fy0) * dy) / len2).clamp(0.0, 1.0)
                };
                let proj_x = fx0 + t * dx;
                let proj_y = fy0 + t * dy;
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
                    self.blend_phys(x, y, blended);
                }
            }
        }
    }

    pub(crate) fn draw_text(
        &mut self,
        font: &Font<'static>,
        text: &str,
        x: i32,
        y: i32,
        size: f32,
        color: Color,
    ) {
        let scale = Scale::uniform(size * self.s);
        let metrics = font.v_metrics(scale);
        let glyphs: Vec<_> = font
            .layout(
                text,
                scale,
                point(x as f32 * self.s, y as f32 * self.s + metrics.ascent),
            )
            .collect();
        for glyph in glyphs {
            if let Some(bb) = glyph.pixel_bounding_box() {
                glyph.draw(|gx, gy, v| {
                    let alpha = (v * f32::from(color.a)).round().clamp(0.0, 255.0) as u8;
                    self.blend_phys(
                        bb.min.x + gx as i32,
                        bb.min.y + gy as i32,
                        Color { a: alpha, ..color },
                    );
                });
            }
        }
    }

    pub(crate) fn draw_text_center(
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

    pub(crate) fn draw_text_right(
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

pub(crate) fn measure_text(font: &Font<'static>, text: &str, size: f32) -> i32 {
    let scale = Scale::uniform(size);
    let mut width = 0.0f32;
    for glyph in font.layout(text, scale, point(0.0, 0.0)) {
        let advance = glyph.position().x + glyph.unpositioned().h_metrics().advance_width;
        width = width.max(advance);
    }
    width.ceil() as i32
}

pub(crate) fn point_in_rect(px: i32, py: i32, x: i32, y: i32, w: i32, h: i32) -> bool {
    px >= x && px < x + w && py >= y && py < y + h
}
