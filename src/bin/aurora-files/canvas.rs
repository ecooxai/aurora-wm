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
pub const BLUE_LIGHT: Color = Color::rgb(175, 218, 245);
pub const PAPER: Color = Color::rgb(247, 252, 255);
pub const CARD: Color = Color::rgba(255, 255, 255, 150);

/// A drawing surface with a logical coordinate system (`width` x `height`)
/// backed by a physical pixel buffer (`pw` x `ph`, BGRX) at the global UI
/// scale. All drawing calls take logical coordinates; shapes and text are
/// rasterised at physical resolution so they stay sharp on HiDPI panels.
#[derive(Clone)]
#[allow(dead_code)]
pub struct Canvas {
    pub width: u16,
    pub height: u16,
    pub pw: u16,
    pub ph: u16,
    pub s: f32,
    pub data: Vec<u8>,
}

#[allow(dead_code)]
impl Canvas {
    pub fn new(width: u16, height: u16, color: Color) -> Self {
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

    /// Logical coordinate to this canvas's physical pixel grid.
    #[inline]
    pub fn px(&self, v: i32) -> i32 {
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
    pub fn blend_phys(&mut self, x: i32, y: i32, color: Color) {
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
    pub fn blend_pixel(&mut self, x: i32, y: i32, color: Color) {
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
    pub fn set_pixel_raw(&mut self, x: i32, y: i32, bgra: [u8; 4]) {
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
    pub fn paint_rgba_image(
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
    pub fn shade<F: Fn(f32, f32) -> f32>(
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

    pub fn draw_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color) {
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

    pub fn draw_round_rect(&mut self, x: i32, y: i32, w: i32, h: i32, radius: i32, color: Color) {
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

    pub fn draw_circle(&mut self, cx: i32, cy: i32, radius: i32, color: Color) {
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

    pub fn draw_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, thickness: i32, color: Color) {
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

    pub fn draw_text(
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
        let (pw, ph) = (crate::scale::px_len(w.max(0) as u32), crate::scale::px_len(h.max(0) as u32));
        if self.s != 1.0 && pixels.len() == (pw * ph * 4) as usize {
            self.paint_rgba_image(pixels, pw, ph, x, y, w, h, false);
            return;
        }
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
    let mut chars = value.chars();
    let mut visible = chars
        .by_ref()
        .take(max_chars.saturating_add(1))
        .collect::<String>();
    if visible.chars().count() <= max_chars {
        return visible;
    }
    visible = visible
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect();
    visible.push_str("...");
    visible
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
