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

impl Aurora {
    pub(crate) fn toggle_screenshot_mode(&mut self) -> AnyResult<()> {
        if self.screenshot_mode {
            self.capture_screenshot(None)?;
        } else {
            self.screenshot_mode = true;
            self.screenshot_selection = None;
            self.screenshot_base = capture_screen_preview(&self.conn, self.root);
            self.set_topbar_notice(
                "Drag to pick area. Hold camera 2s for full screen.",
                Duration::from_secs(4),
            )?;
            self.conn.configure_window(
                self.ui.screenshot_overlay,
                &ConfigureWindowAux::new()
                    .x(0)
                    .y(0)
                    .width(u32::from(self.screen_width))
                    .height(u32::from(self.screen_height))
                    .stack_mode(StackMode::ABOVE),
            )?;
            self.conn.map_window(self.ui.screenshot_overlay)?;
            self.redraw_screenshot_overlay()?;
            self.raise_ui()?;
        }
        Ok(())
    }

    pub(crate) fn start_screenshot_selection(&mut self, root_x: i16, root_y: i16) -> AnyResult<()> {
        self.erase_screenshot_live_rect()?;
        self.screenshot_selection = Some(ScreenshotSelection {
            start_x: root_x,
            start_y: root_y,
            current_x: root_x,
            current_y: root_y,
        });
        let _ = self
            .conn
            .grab_pointer(
                false,
                self.root,
                EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                x11rb::NONE,
                self.cursor,
                CURRENT_TIME,
            )?
            .reply();
        self.update_screenshot_live_rect()?;
        Ok(())
    }

    pub(crate) fn finish_screenshot_selection(&mut self, root_x: i16, root_y: i16) -> AnyResult<()> {
        self.erase_screenshot_live_rect()?;
        let Some(selection) = self.screenshot_selection.take() else {
            return Ok(());
        };
        let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        let x1 = i32::from(selection.start_x).min(i32::from(root_x)).max(0);
        let y1 = i32::from(selection.start_y).min(i32::from(root_y)).max(0);
        let x2 = i32::from(selection.start_x)
            .max(i32::from(root_x))
            .min(i32::from(self.screen_width));
        let y2 = i32::from(selection.start_y)
            .max(i32::from(root_y))
            .min(i32::from(self.screen_height));
        if x2 - x1 >= 8 && y2 - y1 >= 8 {
            self.capture_screenshot(Some((x1, y1, x2 - x1, y2 - y1)))?;
        } else {
            self.screenshot_mode = false;
            self.screenshot_base = None;
            self.conn.unmap_window(self.ui.screenshot_overlay)?;
            self.set_topbar_notice("Screenshot cancelled", Duration::from_secs(2))?;
        }
        Ok(())
    }

    pub(crate) fn erase_screenshot_live_rect(&mut self) -> AnyResult<()> {
        if let Some((x, y, w, h)) = self.screenshot_live_rect.take() {
            let rects = selection_border_rects(x, y, w, h);
            self.draw_xor_rects(self.ui.screenshot_overlay, &rects)?;
        }
        Ok(())
    }

    pub(crate) fn update_screenshot_live_rect(&mut self) -> AnyResult<()> {
        let Some(selection) = self.screenshot_selection else {
            return Ok(());
        };
        let x1 = i32::from(selection.start_x)
            .min(i32::from(selection.current_x))
            .max(0);
        let y1 = i32::from(selection.start_y)
            .min(i32::from(selection.current_y))
            .max(0);
        let x2 = i32::from(selection.start_x)
            .max(i32::from(selection.current_x))
            .min(i32::from(self.screen_width));
        let y2 = i32::from(selection.start_y)
            .max(i32::from(selection.current_y))
            .min(i32::from(self.screen_height));
        let w = (x2 - x1).max(10) as u16;
        let h = (y2 - y1).max(10) as u16;
        let x = (x1.min(i32::from(self.screen_width.saturating_sub(w)))) as i16;
        let y = (y1.min(i32::from(self.screen_height.saturating_sub(h)))) as i16;
        let next = (x, y, w, h);
        if self.screenshot_live_rect == Some(next) {
            return Ok(());
        }
        self.erase_screenshot_live_rect()?;
        self.screenshot_live_rect = Some(next);
        let rects = selection_border_rects(x, y, w, h);
        self.draw_xor_rects(self.ui.screenshot_overlay, &rects)?;
        Ok(())
    }

    pub(crate) fn capture_screenshot(&mut self, rect: Option<(i32, i32, i32, i32)>) -> AnyResult<()> {
        self.screenshot_mode = false;
        self.screenshot_selection = None;
        self.screenshot_base = None;
        self.screenshot_live_rect = None;
        let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        self.conn.unmap_window(self.ui.screenshot_overlay)?;
        self.conn.flush()?;
        let desktop = home_dir().join("Desktop");
        let _ = fs::create_dir_all(&desktop);
        let path = desktop.join(format!(
            "Aurora Screenshot {}.png",
            OffsetDateTime::now_utc().unix_timestamp()
        ));
        let (x, y, w, h) = rect.unwrap_or((
            0,
            0,
            i32::from(self.screen_width),
            i32::from(self.screen_height),
        ));
        let Ok((pixels, width, height)) = capture_root_rgba(
            &self.conn,
            self.root,
            x as i16,
            y as i16,
            w.max(1) as u16,
            h.max(1) as u16,
        ) else {
            self.set_topbar_notice("Screenshot failed", Duration::from_secs(3))?;
            return Ok(());
        };
        if image::save_buffer_with_format(
            &path,
            &pixels,
            width,
            height,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .is_err()
        {
            self.set_topbar_notice("Screenshot failed", Duration::from_secs(3))?;
            return Ok(());
        }
        copy_image_to_clipboard(&path);
        self.last_seen_clipboard_image_sig = clipboard_file_image_signature(&path);
        self.remember_clipboard_item(ClipboardItem::Image(path.clone()));
        self.set_topbar_notice(
            "Screenshot saved and copied to clipboard",
            Duration::from_secs(3),
        )?;
        if self.launch_screenshot_viewer(&path) {
            return Ok(());
        }
        self.open_media(FolderEntry {
            name: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Screenshot.png")
                .to_string(),
            path,
            kind: FileKind::Image,
        })?;
        if let Some(media) = self.media_slots.get_mut(0).and_then(|m| m.as_mut()) {
            media.notice = Some("Copied image to clipboard".to_string());
        }
        self.redraw_media_slot(0)?;
        Ok(())
    }

    pub(crate) fn redraw_screenshot_overlay(&self) -> AnyResult<()> {
        let mut c = self
            .screenshot_base
            .as_ref()
            .map(|preview| canvas_from_preview(preview, self.screen_width, self.screen_height))
            .unwrap_or_else(|| {
                Canvas::from_wallpaper_crop(
                    &self.wallpaper_pixels,
                    self.screen_width,
                    0,
                    0,
                    self.screen_width,
                    self.screen_height,
                )
            });
        c.draw_rect(
            0,
            0,
            i32::from(self.screen_width),
            i32::from(self.screen_height),
            Color::rgba(0, 0, 0, 128),
        );
        if let Some(selection) = self.screenshot_selection {
            let x1 = i32::from(selection.start_x)
                .min(i32::from(selection.current_x))
                .max(0);
            let y1 = i32::from(selection.start_y)
                .min(i32::from(selection.current_y))
                .max(0);
            let x2 = i32::from(selection.start_x)
                .max(i32::from(selection.current_x))
                .min(i32::from(self.screen_width));
            let y2 = i32::from(selection.start_y)
                .max(i32::from(selection.current_y))
                .min(i32::from(self.screen_height));
            let w = x2 - x1;
            let h = y2 - y1;
            if w > 0 && h > 0 {
                if let Some(base) = self.screenshot_base.as_ref() {
                    paint_preview_region(&mut c, base, x1, y1, w, h);
                }
                c.draw_rect(x1, y1, w, 2, MINT_LIGHT);
                c.draw_rect(x1, y2 - 2, w, 2, MINT_LIGHT);
                c.draw_rect(x1, y1, 2, h, MINT_LIGHT);
                c.draw_rect(x2 - 2, y1, 2, h, MINT_LIGHT);
            } else {
                c.draw_rect(x1 - 5, y1 - 5, 10, 2, MINT_LIGHT);
                c.draw_rect(x1 - 5, y1 + 4, 10, 2, MINT_LIGHT);
                c.draw_rect(x1 - 5, y1 - 5, 2, 10, MINT_LIGHT);
                c.draw_rect(x1 + 4, y1 - 5, 2, 10, MINT_LIGHT);
            }
        }
        self.upload_canvas(self.ui.screenshot_overlay, &c)
    }

}
