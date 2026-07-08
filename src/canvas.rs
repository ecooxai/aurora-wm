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
pub(crate) const CLIPBOARD_MENU_PREV_X: i32 = 118;
pub(crate) const CLIPBOARD_MENU_NEXT_X: i32 = 158;
pub(crate) const DEFAULT_WORKSPACE_COUNT: usize = 2;
pub(crate) const MAX_WORKSPACE_COUNT: usize = 8;
pub(crate) const WORKSPACE_STRIDE: i32 = 27;
pub(crate) const WORKSPACE_SIZE: i32 = 18;
pub(crate) const FOLDER_DEFAULT_WIDTH: u16 = 330;
pub(crate) const FOLDER_DEFAULT_HEIGHT: u16 = 220;
pub(crate) const FOLDER_MIN_WIDTH: u16 = 260;
pub(crate) const FOLDER_MIN_HEIGHT: u16 = 160;
pub(crate) const TERMINAL_MIN_WIDTH: u16 = 260;
pub(crate) const TERMINAL_MIN_HEIGHT: u16 = 120;
pub(crate) const TERMINAL_DEFAULT_WIDTH: u16 = FOLDER_DEFAULT_WIDTH;

#[derive(Clone)]
pub(crate) struct Canvas {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) data: Vec<u8>,
}

impl Canvas {
    pub(crate) fn new(width: u16, height: u16, color: Color) -> Self {
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

    pub(crate) fn from_wallpaper_crop(
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

    pub(crate) fn idx(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x >= i32::from(self.width) || y >= i32::from(self.height) {
            return None;
        }
        Some((usize::try_from(y).ok()? * usize::from(self.width) + usize::try_from(x).ok()?) * 4)
    }

    pub(crate) fn blend_pixel(&mut self, x: i32, y: i32, color: Color) {
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

    pub(crate) fn draw_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color) {
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

    pub(crate) fn draw_round_rect(&mut self, x: i32, y: i32, w: i32, h: i32, radius: i32, color: Color) {
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

    pub(crate) fn draw_circle(&mut self, cx: i32, cy: i32, radius: i32, color: Color) {
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

    pub(crate) fn draw_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, thickness: i32, color: Color) {
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

    pub(crate) fn draw_text(
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
