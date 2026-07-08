//! Minimal software canvas shared by the Aurora apps (BGRA, X11 ZPixmap).

use rusttype::{Font, Scale, point};

#[derive(Clone, Copy, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
}

pub const INK: Color = Color::rgb(32, 43, 54);
pub const MUTED: Color = Color::rgb(105, 118, 132);
pub const SOFT_INK: Color = Color::rgb(74, 88, 103);
pub const MINT_DARK: Color = Color::rgb(29, 145, 137);
pub const BLUE: Color = Color::rgb(73, 156, 231);
pub const PAPER: Color = Color::rgb(247, 252, 255);
pub const CARD: Color = Color::rgba(255, 255, 255, 150);

pub struct Canvas {
    pub width: u16,
    pub height: u16,
    pub data: Vec<u8>,
}

impl Canvas {
    pub fn new(width: u16, height: u16, color: Color) -> Self {
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

    fn idx(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x >= i32::from(self.width) || y >= i32::from(self.height) {
            return None;
        }
        Some((y as usize * usize::from(self.width) + x as usize) * 4)
    }

    pub fn blend_pixel(&mut self, x: i32, y: i32, color: Color) {
        let Some(i) = self.idx(x, y) else {
            return;
        };
        if color.a == 255 {
            self.data[i] = color.b;
            self.data[i + 1] = color.g;
            self.data[i + 2] = color.r;
            return;
        }
        let alpha = u32::from(color.a);
        let inv = 255 - alpha;
        self.data[i] = ((u32::from(color.b) * alpha + u32::from(self.data[i]) * inv) / 255) as u8;
        self.data[i + 1] =
            ((u32::from(color.g) * alpha + u32::from(self.data[i + 1]) * inv) / 255) as u8;
        self.data[i + 2] =
            ((u32::from(color.r) * alpha + u32::from(self.data[i + 2]) * inv) / 255) as u8;
    }

    pub fn draw_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color) {
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

    pub fn draw_round_rect(&mut self, x: i32, y: i32, w: i32, h: i32, radius: i32, color: Color) {
        if w <= 0 || h <= 0 {
            return;
        }
        let r = radius.max(0).min(w / 2).min(h / 2);
        let rf = r as f32;
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(i32::from(self.width));
        let y1 = (y + h).min(i32::from(self.height));
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
                    if cx == xx || cy == yy {
                        1.0
                    } else {
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

    pub fn draw_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, thickness: i32, color: Color) {
        let x_start = (x0.min(x1) - thickness - 2).max(0);
        let x_end = (x0.max(x1) + thickness + 2).min(i32::from(self.width));
        let y_start = (y0.min(y1) - thickness - 2).max(0);
        let y_end = (y0.max(y1) + thickness + 2).min(i32::from(self.height));
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
                let px = x0 as f32 + t * dx;
                let py = y0 as f32 + t * dy;
                let d = ((x as f32 - px).powi(2) + (y as f32 - py).powi(2)).sqrt();
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

    pub fn draw_circle(&mut self, cx: i32, cy: i32, radius: i32, color: Color) {
        let r = radius as f32;
        for y in (cy - radius - 1).max(0)..=(cy + radius + 1).min(i32::from(self.height) - 1) {
            for x in (cx - radius - 1).max(0)..=(cx + radius + 1).min(i32::from(self.width) - 1) {
                let d = (((x - cx).pow(2) + (y - cy).pow(2)) as f32).sqrt();
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

    pub fn draw_text(
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
        for glyph in font.layout(text, scale, point(x as f32, y as f32 + metrics.ascent)) {
            if let Some(bb) = glyph.pixel_bounding_box() {
                glyph.draw(|gx, gy, v| {
                    let alpha = (v * f32::from(color.a)).round().clamp(0.0, 255.0) as u8;
                    self.blend_pixel(
                        bb.min.x + gx as i32,
                        bb.min.y + gy as i32,
                        Color { a: alpha, ..color },
                    );
                });
            }
        }
    }

    pub fn draw_text_center(
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

    /// Paint RGBA pixels (e.g. decoded images) onto the canvas.
    pub fn paint_rgba(&mut self, pixels: &[u8], x: i32, y: i32, w: i32, h: i32) {
        for yy in 0..h {
            for xx in 0..w {
                let idx = ((yy * w + xx) * 4) as usize;
                if idx + 3 < pixels.len() {
                    self.blend_pixel(
                        x + xx,
                        y + yy,
                        Color::rgba(pixels[idx], pixels[idx + 1], pixels[idx + 2], pixels[idx + 3]),
                    );
                }
            }
        }
    }
}

pub fn measure_text(font: &Font<'static>, text: &str, size: f32) -> i32 {
    let scale = Scale::uniform(size);
    let mut width = 0.0f32;
    for glyph in font.layout(text, scale, point(0.0, 0.0)) {
        let advance = glyph.position().x + glyph.unpositioned().h_metrics().advance_width;
        width = width.max(advance);
    }
    width.ceil() as i32
}

pub fn compact(value: &str, max_chars: usize) -> String {
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

pub fn compact_path(path: &std::path::Path, max_chars: usize) -> String {
    let text = path.to_string_lossy();
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let tail: String = text
            .chars()
            .rev()
            .take(max_chars.saturating_sub(3))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("...{tail}")
    }
}
