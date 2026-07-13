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
use crate::terminal_ui::*;
use crate::folder_actions::*;
use crate::media_ui::*;
use crate::system_apply::*;
use crate::layout::*;
use crate::pixels::*;
use crate::system::*;
use crate::textutil::*;
use crate::procutil::*;
use crate::files::*;

pub(crate) fn draw_card(c: &mut Canvas, x: i32, y: i32, w: i32, h: i32) {
    c.draw_round_rect(x, y, w, h, 12, Color::rgba(255, 255, 255, 184));
    c.draw_round_rect(x, y, w, h, 12, Color::rgba(214, 230, 237, 42));
    c.draw_rect(x + 12, y, w - 24, 1, CARD_LINE);
}

pub(crate) fn draw_metric_bar(
    c: &mut Canvas,
    font: &Font<'static>,
    x: i32,
    y: i32,
    name: &str,
    value: f32,
    suffix: &str,
) {
    let value_right = i32::from(c.width) - 40;
    let bar_x = x + 72;
    let bar_right = (value_right - 44).max(bar_x + 24);
    let bar_w = bar_right - bar_x;
    c.draw_text(font, name, x, y - 1, 12.0, MUTED);
    c.draw_round_rect(bar_x, y, bar_w, 8, 4, Color::rgba(211, 225, 232, 170));
    c.draw_round_rect(
        bar_x,
        y,
        (bar_w as f32 * (value / 100.0).clamp(0.0, 1.0)) as i32,
        8,
        4,
        Color::rgba(116, 213, 198, 210),
    );
    c.draw_text_right(
        font,
        &format!("{value:.0}{suffix}"),
        value_right,
        y - 6,
        12.0,
        INK,
    );
}

pub(crate) fn snap_auto_power_saver_minutes(minutes: u32) -> u32 {
    let minutes = minutes.clamp(
        AUTO_POWER_SAVER_MIN_MINUTES,
        AUTO_POWER_SAVER_MAX_MINUTES,
    );
    if minutes < AUTO_POWER_SAVER_STEP_MINUTES / 2 {
        return AUTO_POWER_SAVER_MIN_MINUTES;
    }
    (((minutes + AUTO_POWER_SAVER_STEP_MINUTES / 2) / AUTO_POWER_SAVER_STEP_MINUTES)
        * AUTO_POWER_SAVER_STEP_MINUTES)
        .min(AUTO_POWER_SAVER_MAX_MINUTES)
}

pub(crate) fn auto_power_saver_minutes_from_slider(x: i32, left: i32, width: i32) -> u32 {
    if width <= 0 || x <= left {
        return AUTO_POWER_SAVER_MIN_MINUTES;
    }
    if x >= left + width {
        return AUTO_POWER_SAVER_MAX_MINUTES;
    }
    let span = AUTO_POWER_SAVER_MAX_MINUTES - AUTO_POWER_SAVER_MIN_MINUTES;
    let raw = AUTO_POWER_SAVER_MIN_MINUTES
        + ((x - left) as u32 * span + width as u32 / 2) / width as u32;
    snap_auto_power_saver_minutes(raw)
}

pub(crate) fn auto_power_saver_slider_x(minutes: u32, left: i32, width: i32) -> i32 {
    let minutes = minutes.clamp(
        AUTO_POWER_SAVER_MIN_MINUTES,
        AUTO_POWER_SAVER_MAX_MINUTES,
    );
    left + ((minutes - AUTO_POWER_SAVER_MIN_MINUTES) as i64 * i64::from(width)
        / i64::from(AUTO_POWER_SAVER_MAX_MINUTES - AUTO_POWER_SAVER_MIN_MINUTES)) as i32
}

pub(crate) fn draw_info_row(c: &mut Canvas, font: &Font<'static>, x: i32, y: i32, key: &str, value: &str) {
    c.draw_text(font, key, x, y, 12.0, MUTED);
    c.draw_text(font, value, x + 62, y, 12.0, INK);
}

pub(crate) fn mask_has(mask: ConfigWindow, flag: ConfigWindow) -> bool {
    u16::from(mask) & u16::from(flag) != 0
}

pub(crate) fn hover_title_button(x: i16, y: i16) -> Option<TitleButton> {
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

pub(crate) fn resize_corner_edges_for_frame(
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

#[cfg(test)]
mod power_slider_tests {
    use super::*;

    #[test]
    fn slider_covers_endpoints_and_uses_fifty_minute_steps() {
        assert_eq!(auto_power_saver_minutes_from_slider(10, 10, 100), 1);
        assert_eq!(auto_power_saver_minutes_from_slider(110, 10, 100), 1000);
        let middle = auto_power_saver_minutes_from_slider(60, 10, 100);
        assert_eq!(middle % AUTO_POWER_SAVER_STEP_MINUTES, 0);
        assert_eq!(auto_power_saver_slider_x(1, 10, 100), 10);
        assert_eq!(auto_power_saver_slider_x(1000, 10, 100), 110);
    }
}

pub(crate) fn resize_corner_edges_for_client(info: &ClientInfo, x: i16, y: i16) -> Option<ResizeEdges> {
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

pub(crate) fn resize_side_hint_for_frame(info: &ClientInfo, x: i16) -> bool {
    let width = i16::try_from(info.width).unwrap_or(i16::MAX);
    x <= RESIZE_EDGE || x >= width - RESIZE_EDGE
}

pub(crate) fn resize_side_hint_for_client(info: &ClientInfo, x: i16) -> bool {
    let width = i16::try_from(info.width).unwrap_or(i16::MAX);
    x <= RESIZE_EDGE || x >= width - RESIZE_EDGE
}

pub(crate) fn terminal_default_height_for(folder_height: u16, screen_height: u16) -> u16 {
    let y = i32::from(TOPBAR_HEIGHT) + 26 + i32::from(folder_height) + 8;
    i32::from(screen_height)
        .saturating_sub(y + 50)
        .max(i32::from(TERMINAL_MIN_HEIGHT)) as u16
}

pub(crate) fn client_uses_own_chrome(class: &str, title: &str) -> bool {
    let text = format!("{} {}", class, title.to_ascii_lowercase());
    ["firefox", "chromium", "google-chrome", "brave", "vivaldi"]
        .iter()
        .any(|needle| text.contains(needle))
}

pub(crate) fn client_is_ffplay(class: &str, title: &str) -> bool {
    let text = format!("{} {}", class, title.to_ascii_lowercase());
    text.contains("ffplay") || text.contains("aurora ffplay")
}

pub(crate) fn rounded_top_shape_rects(width: u16, height: u16, radius: i32) -> Vec<Rectangle> {
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

pub(crate) fn rounded_rect_shape_rects(
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    radius: i32,
) -> Vec<Rectangle> {
    let width_i = i32::from(width);
    let height_i = i32::from(height);
    let r = radius.max(0).min(width_i / 2).min(height_i / 2);
    if r == 0 {
        return vec![Rectangle { x, y, width, height }];
    }

    let mut rects = Vec::with_capacity(height as usize);
    for row in 0..height_i {
        let corner_y = if row < r {
            r - row - 1
        } else if row >= height_i - r {
            row - (height_i - r)
        } else {
            0
        };
        let inset = if corner_y == 0 {
            0
        } else {
            r - ((r * r - corner_y * corner_y) as f64).sqrt().round() as i32
        };
        rects.push(Rectangle {
            x: x.saturating_add(inset as i16),
            y: y.saturating_add(row as i16),
            width: (width_i - inset * 2).max(1) as u16,
            height: 1,
        });
    }
    rects
}

pub(crate) fn create_pointer_cursor(conn: &RustConnection, root: Window) -> AnyResult<Cursor> {
    create_standard_left_ptr_cursor(conn).or_else(|_| create_pixmap_pointer_cursor(conn, root))
}

pub(crate) fn create_standard_left_ptr_cursor(conn: &RustConnection) -> AnyResult<Cursor> {
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

pub(crate) fn create_pixmap_pointer_cursor(conn: &RustConnection, root: Window) -> AnyResult<Cursor> {
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

pub(crate) fn draw_workspace_icon(c: &mut Canvas, x: i32, y: i32, active: bool) {
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

pub(crate) fn draw_add_workspace_icon(c: &mut Canvas, x: i32, cy: i32) {
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

pub(crate) fn draw_sparkline(c: &mut Canvas, x: i32, y: i32, w: i32, h: i32, value: f64, color: Color) {
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

pub(crate) fn draw_sidebar_icon(c: &mut Canvas, idx: usize, cx: i32, cy: i32, color: Color) {
    match idx {
        0 => draw_sidebar_display_icon(c, cx, cy, color),
        1 => draw_power_icon(c, cx, cy, color),
        2 => draw_sidebar_wallpaper_icon(c, cx, cy, color),
        3 => draw_sidebar_audio_icon(c, cx, cy, color),
        4 => draw_sidebar_network_icon(c, cx, cy, color),
        5 => draw_sidebar_bluetooth_icon(c, cx, cy, color),
        6 => draw_sidebar_startup_icon(c, cx, cy, color),
        7 => draw_sidebar_apps_icon(c, cx, cy, color),
        8 => draw_sidebar_keyboard_icon(c, cx, cy, color),
        _ => draw_sidebar_about_icon(c, cx, cy, color),
    }
}

pub(crate) fn draw_round_line(
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

pub(crate) fn draw_arc(
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

pub(crate) fn draw_sidebar_tile(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_sidebar_display_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_sidebar_wallpaper_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_sidebar_audio_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_sidebar_tile(c, cx, cy, color);
    draw_speaker_icon_small(c, cx, cy, color);
}

pub(crate) fn draw_sidebar_network_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_sidebar_tile(c, cx, cy, color);
    draw_wifi_icon_small(c, cx, cy, color);
}

pub(crate) fn draw_sidebar_bluetooth_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_sidebar_startup_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_sidebar_apps_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_sidebar_keyboard_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_sidebar_tile(c, cx, cy, color);
    let is_topbar = color == MINT_LIGHT;
    let base_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(60, 75, 96) };
    let accent_color = if is_topbar { Color::rgb(175, 218, 245) } else { Color::rgb(82, 196, 180) };
    c.draw_round_rect(cx - 10, cy - 7, 20, 14, 3, base_color);
    for row in 0..2 {
        for col in 0..4 {
            c.draw_rect(cx - 7 + col * 4, cy - 4 + row * 4, 2, 2, accent_color);
        }
    }
    c.draw_rect(cx - 5, cy + 3, 10, 2, accent_color);
}

pub(crate) fn draw_sidebar_about_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_dock_icon(c: &mut Canvas, idx: usize, cx: i32, cy: i32) {
    match idx {
        0 => draw_apps_icon(c, cx, cy, BLUE),
        1 => draw_picture_icon(c, cx, cy, MINT_DARK),
        2 => draw_music_icon(c, cx, cy, MINT_DARK),
        3 => draw_play_icon(c, cx, cy, BLUE),
        _ => draw_gear_icon(c, cx, cy, SOFT_INK),
    }
}

pub(crate) fn draw_apps_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    for row in 0..2 {
        for col in 0..2 {
            let x = cx - 11 + col * 14;
            let y = cy - 11 + row * 14;
            c.draw_round_rect(x, y, 9, 9, 3, Color::rgba(color.r, color.g, color.b, 48));
            c.draw_round_rect(x + 2, y + 2, 5, 5, 2, color);
        }
    }
}

pub(crate) fn draw_client_icon(c: &mut Canvas, cx: i32, cy: i32, active: bool) {
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

pub(crate) fn draw_text_file_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_client_task_icon(
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

pub(crate) fn draw_file_kind_icon(c: &mut Canvas, kind: FileKind, cx: i32, cy: i32) {
    match kind {
        FileKind::Directory => draw_folder_icon(c, cx, cy, MINT_DARK),
        FileKind::Text => draw_text_file_icon(c, cx, cy, SOFT_INK),
        FileKind::Image => draw_picture_icon(c, cx, cy, MINT_DARK),
        FileKind::Audio => draw_music_icon(c, cx, cy, MINT_DARK),
        FileKind::Video => draw_play_icon(c, cx, cy, BLUE),
        FileKind::Other => draw_client_icon(c, cx, cy, true),
    }
}

pub(crate) fn file_kind_label(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Directory => "Folder",
        FileKind::Text => "Text file",
        FileKind::Image => "Image file",
        FileKind::Audio => "Audio file",
        FileKind::Video => "Video file",
        FileKind::Other => "Open file",
    }
}

pub(crate) fn draw_launcher_icon(c: &mut Canvas, idx: usize, cx: i32, cy: i32) {
    match idx {
        0 => draw_play_icon(c, cx, cy, BLUE),
        1 => draw_globe_icon(c, cx, cy, BLUE),
        2 => draw_camera_icon(c, cx, cy, MINT_DARK),
        3 => draw_record_icon(c, cx, cy, Color::rgb(206, 76, 91)),
        4 => draw_gear_icon(c, cx, cy, SOFT_INK),
        _ => draw_more_icon(c, cx, cy, SOFT_INK),
    }
}

pub(crate) fn draw_search_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.draw_circle(cx - 2, cy - 2, 6, color);
    c.draw_circle(cx - 2, cy - 2, 4, Color::rgba(248, 253, 255, 255));
    draw_round_line(c, cx + 2, cy + 2, cx + 7, cy + 7, 2, color);
}

pub(crate) fn draw_catalog_chevron(
    c: &mut Canvas,
    cx: i32,
    cy: i32,
    expanded: bool,
    color: Color,
) {
    if expanded {
        draw_round_line(c, cx - 4, cy - 2, cx, cy + 2, 2, color);
        draw_round_line(c, cx, cy + 2, cx + 4, cy - 2, 2, color);
    } else {
        draw_round_line(c, cx - 2, cy - 4, cx + 2, cy, 2, color);
        draw_round_line(c, cx + 2, cy, cx - 2, cy + 4, 2, color);
    }
}

pub(crate) fn draw_camera_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.draw_round_rect(cx - 11, cy - 7, 22, 15, 5, Color::rgba(color.r, color.g, color.b, 55));
    c.draw_round_rect(cx - 5, cy - 11, 10, 5, 2, color);
    c.draw_circle(cx, cy, 6, color);
    c.draw_circle(cx, cy, 3, Color::rgba(248, 253, 255, 255));
}

pub(crate) fn draw_record_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.draw_circle(cx, cy, 11, Color::rgba(color.r, color.g, color.b, 40));
    c.draw_circle(cx, cy, 6, color);
}

pub(crate) fn draw_folder_icon(c: &mut Canvas, cx: i32, cy: i32, _color: Color) {
    c.draw_round_rect(
        cx - 12,
        cy - 12,
        24,
        24,
        6,
        Color::rgb(175, 218, 245), // Simple beautiful light blue square
    );
}

pub(crate) fn draw_home_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_more_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.draw_circle(cx - 7, cy, 2, color);
    c.draw_circle(cx, cy, 2, color);
    c.draw_circle(cx + 7, cy, 2, color);
}

pub(crate) fn draw_sort_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.draw_line(cx - 8, cy - 6, cx + 7, cy - 6, 2, color);
    c.draw_line(cx - 8, cy, cx + 3, cy, 2, color);
    c.draw_line(cx - 8, cy + 6, cx - 1, cy + 6, 2, color);
}

pub(crate) fn draw_terminal_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    draw_round_line(c, cx - 8, cy - 5, cx - 3, cy, 2, color);
    draw_round_line(c, cx - 8, cy + 5, cx - 3, cy, 2, color);
    c.draw_line(cx + 1, cy + 5, cx + 8, cy + 5, 2, color);
}

pub(crate) fn draw_screenshot_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_clipboard_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_copy_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_edit_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_save_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.draw_round_rect(cx - 8, cy - 8, 16, 16, 3, color);
    c.draw_rect(cx + 3, cy - 8, 5, 5, Color::rgba(255, 255, 255, 180));
    c.draw_round_rect(cx - 5, cy + 1, 10, 5, 2, Color::rgba(255, 255, 255, 210));
}

pub(crate) fn draw_picture_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_globe_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.draw_circle(cx, cy, 12, Color::rgba(color.r, color.g, color.b, 45));
    c.draw_circle(cx, cy, 10, Color::rgba(255, 255, 255, 80));
    c.draw_line(cx - 10, cy, cx + 10, cy, 2, color);
    c.draw_line(cx, cy - 10, cx, cy + 10, 2, color);
    c.draw_line(cx - 7, cy - 7, cx + 7, cy - 7, 1, color);
    c.draw_line(cx - 7, cy + 7, cx + 7, cy + 7, 1, color);
}

pub(crate) fn draw_music_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_play_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_gear_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_power_icon(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_wifi_icon_small(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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

pub(crate) fn draw_speaker_icon_small(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
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
