//! State and rendering for the standalone image viewer window that opens
//! to the right of the file list: zoom at pointer, pan, rotate, flip, crop.

use std::path::{Path, PathBuf};

use crate::canvas::{Canvas, Color, MINT_DARK};

/// Height of the toolbar at the top of the image window.
pub const IMG_TOP_BAR: i32 = 40;
/// Dark navy backdrop behind the image.
pub const IMG_BACKDROP: Color = Color::rgb(21, 39, 66);

pub struct ImgState {
    pub path: PathBuf,
    /// Current (possibly rotated/flipped/cropped) full-resolution image.
    pub img: image::RgbaImage,
    pub error: Option<String>,
    /// Absolute scale: screen pixels per image pixel.
    pub zoom: f32,
    /// Top-left corner of the drawn image, in window coordinates.
    pub origin: (f32, f32),
    /// True once the user zoomed manually (disables auto-refit on resize).
    pub user_zoomed: bool,
    pub menu_open: bool,
    /// Right-click context menu, anchored at (x, y).
    pub ctx_menu: Option<(i32, i32)>,
    pub crop_mode: bool,
    /// Crop selection drag: (start_x, start_y, cur_x, cur_y) window coords.
    pub crop_drag: Option<(i32, i32, i32, i32)>,
    /// Last pointer position while panning with the left button.
    pub panning: Option<(i32, i32)>,
    pub status: String,
}

impl ImgState {
    pub fn open(path: &Path) -> Self {
        let (img, error) = match image::open(path) {
            Ok(img) => (img.to_rgba8(), None),
            Err(err) => (image::RgbaImage::new(1, 1), Some(err.to_string())),
        };
        Self {
            path: path.to_path_buf(),
            img,
            error,
            zoom: 1.0,
            origin: (0.0, 0.0),
            user_zoomed: false,
            menu_open: false,
            ctx_menu: None,
            crop_mode: false,
            crop_drag: None,
            panning: None,
            status: String::new(),
        }
    }

    /// Fit and center the image in the view area below the toolbar.
    pub fn fit(&mut self, win_w: i32, win_h: i32) {
        let (iw, ih) = (self.img.width() as f32, self.img.height() as f32);
        let vw = win_w.max(1) as f32;
        let vh = (win_h - IMG_TOP_BAR).max(1) as f32;
        self.zoom = (vw / iw).min(vh / ih).min(1.0).max(0.001);
        let sw = iw * self.zoom;
        let sh = ih * self.zoom;
        self.origin = ((vw - sw) / 2.0, IMG_TOP_BAR as f32 + (vh - sh) / 2.0);
        self.user_zoomed = false;
    }

    /// Zoom by `factor`, keeping the image point under (mx, my) fixed.
    pub fn zoom_at(&mut self, mx: i32, my: i32, factor: f32) {
        let new_zoom = (self.zoom * factor).clamp(0.02, 40.0);
        let ratio = new_zoom / self.zoom;
        self.origin.0 = mx as f32 - (mx as f32 - self.origin.0) * ratio;
        self.origin.1 = my as f32 - (my as f32 - self.origin.1) * ratio;
        self.zoom = new_zoom;
        self.user_zoomed = true;
    }

    pub fn pan(&mut self, dx: i32, dy: i32) {
        self.origin.0 += dx as f32;
        self.origin.1 += dy as f32;
    }

    /// Whether the image (at current zoom) overflows the view area.
    pub fn overflows(&self, win_w: i32, win_h: i32) -> bool {
        let sw = self.img.width() as f32 * self.zoom;
        let sh = self.img.height() as f32 * self.zoom;
        sw > win_w as f32 || sh > (win_h - IMG_TOP_BAR) as f32
    }

    pub fn rotate90(&mut self, win_w: i32, win_h: i32) {
        self.img = image::imageops::rotate90(&self.img);
        self.fit(win_w, win_h);
        self.status = "Rotated 90".into();
    }

    pub fn flip_horizontal(&mut self) {
        image::imageops::flip_horizontal_in_place(&mut self.img);
        self.status = "Flipped horizontally".into();
    }

    pub fn flip_vertical(&mut self) {
        image::imageops::flip_vertical_in_place(&mut self.img);
        self.status = "Flipped vertically".into();
    }

    /// Apply the current crop selection (window coords) to the image.
    pub fn apply_crop(&mut self, win_w: i32, win_h: i32) {
        let Some((x0, y0, x1, y1)) = self.crop_drag.take() else {
            return;
        };
        let zoom = self.zoom.max(0.001);
        let sx0 = ((x0.min(x1) as f32 - self.origin.0) / zoom).floor().max(0.0) as u32;
        let sy0 = ((y0.min(y1) as f32 - self.origin.1) / zoom).floor().max(0.0) as u32;
        let sx1 = (((x0.max(x1)) as f32 - self.origin.0) / zoom)
            .ceil()
            .clamp(0.0, self.img.width() as f32) as u32;
        let sy1 = (((y0.max(y1)) as f32 - self.origin.1) / zoom)
            .ceil()
            .clamp(0.0, self.img.height() as f32) as u32;
        self.crop_mode = false;
        if sx1 <= sx0 + 1 || sy1 <= sy0 + 1 {
            self.status = "Crop selection too small".into();
            return;
        }
        self.img =
            image::imageops::crop_imm(&self.img, sx0, sy0, sx1 - sx0, sy1 - sy0).to_image();
        self.fit(win_w, win_h);
        self.status = format!("Cropped to {} x {} px", sx1 - sx0, sy1 - sy0);
    }

    /// Render the image area (nearest-neighbour sampling) into the canvas.
    pub fn render(&self, c: &mut Canvas) {
        let w = i32::from(c.width);
        let h = i32::from(c.height);
        c.draw_rect(0, IMG_TOP_BAR, w, h - IMG_TOP_BAR, IMG_BACKDROP);
        if self.error.is_some() {
            return;
        }
        let iw = self.img.width() as i32;
        let ih = self.img.height() as i32;
        let zoom = self.zoom.max(0.001);
        let col_map: Vec<i32> = (0..w)
            .map(|x| ((x as f32 - self.origin.0) / zoom).floor() as i32)
            .collect();
        for y in IMG_TOP_BAR..h {
            let sy = ((y as f32 - self.origin.1) / zoom).floor() as i32;
            if sy < 0 || sy >= ih {
                continue;
            }
            for x in 0..w {
                let sx = col_map[x as usize];
                if sx < 0 || sx >= iw {
                    continue;
                }
                let p = self.img.get_pixel(sx as u32, sy as u32);
                c.blend_pixel(x, y, Color::rgba(p[0], p[1], p[2], p[3]));
            }
        }
        // Crop selection overlay.
        if let Some((x0, y0, x1, y1)) = self.crop_drag {
            let rx = x0.min(x1);
            let ry = y0.min(y1);
            let rw = (x1 - x0).abs();
            let rh = (y1 - y0).abs();
            c.draw_rect(rx, ry, rw, rh, Color::rgba(116, 213, 198, 60));
            c.draw_rect(rx, ry, rw, 2, MINT_DARK);
            c.draw_rect(rx, ry + rh - 2, rw, 2, MINT_DARK);
            c.draw_rect(rx, ry, 2, rh, MINT_DARK);
            c.draw_rect(rx + rw - 2, ry, 2, rh, MINT_DARK);
        }
    }
}
