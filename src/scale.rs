//! HiDPI support. Aurora lays everything out in logical pixels; the X server
//! works in physical pixels. `ScaledConn` wraps the connection and converts
//! geometry at the protocol boundary (requests up, events and replies down),
//! and `Canvas` renders its logical drawing at physical resolution, so text
//! and shapes stay sharp on high-density panels instead of being pixel-doubled.

use std::ops::Deref;
use std::sync::atomic::{AtomicU32, Ordering};

use x11rb::connection::Connection;
use x11rb::cookie::{Cookie, VoidCookie};
use x11rb::errors::{ConnectionError, ReplyError};
use x11rb::protocol::shape::{self, ConnectionExt as ShapeConnectionExt};
use x11rb::protocol::xproto::{
    ClipOrdering, ConfigureWindowAux, ConnectionExt as XprotoConnectionExt, CreateWindowAux,
    Drawable, Gcontext, GetGeometryReply, Pixmap, QueryPointerReply, Rectangle,
    TranslateCoordinatesReply, Visualid, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

static SCALE_BITS: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0

/// Logical-to-physical pixel ratio.
pub(crate) fn ui_scale() -> f32 {
    f32::from_bits(SCALE_BITS.load(Ordering::Relaxed))
}

/// Whole-number scale (GDK_SCALE): 1, 2 or 3.
pub(crate) fn gdk_scale() -> u32 {
    ui_scale().round().clamp(1.0, 3.0) as u32
}

pub(crate) fn set_ui_scale(scale: f32) {
    let scale = if scale.is_finite() { scale.clamp(1.0, 3.0) } else { 1.0 };
    SCALE_BITS.store(scale.to_bits(), Ordering::Relaxed);
}

/// Logical coordinate to physical.
pub(crate) fn px(v: i32) -> i32 {
    (v as f32 * ui_scale()).round() as i32
}

/// Logical length to physical, at least 1 when the input is positive.
pub(crate) fn px_len(v: u32) -> u32 {
    if v == 0 {
        0
    } else {
        ((v as f32 * ui_scale()).round() as u32).max(1)
    }
}

/// Physical span of a logical segment, so adjacent segments tile exactly.
pub(crate) fn px_span(pos: i32, len: i32) -> (i32, i32) {
    let a = px(pos);
    let b = px(pos + len);
    (a, (b - a).max(if len > 0 { 1 } else { 0 }))
}

/// Physical coordinate to logical.
pub(crate) fn lg(v: i32) -> i32 {
    (v as f32 / ui_scale()).floor() as i32
}

/// Physical length to logical.
pub(crate) fn lg_len(v: u32) -> u32 {
    if v == 0 {
        0
    } else {
        ((v as f32 / ui_scale()).round() as u32).max(1)
    }
}

fn clamp_i16(v: i32) -> i16 {
    v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

fn clamp_u16(v: u32) -> u16 {
    v.min(u32::from(u16::MAX)) as u16
}

fn lg16(v: i16) -> i16 {
    clamp_i16(lg(i32::from(v)))
}

fn lg_len16(v: u16) -> u16 {
    clamp_u16(lg_len(u32::from(v)))
}

pub(crate) fn scale_rect(r: &Rectangle) -> Rectangle {
    let (x, w) = px_span(i32::from(r.x), i32::from(r.width));
    let (y, h) = px_span(i32::from(r.y), i32::from(r.height));
    Rectangle { x: clamp_i16(x), y: clamp_i16(y), width: clamp_u16(w.max(0) as u32), height: clamp_u16(h.max(0) as u32) }
}

pub(crate) fn scale_configure_aux(aux: &ConfigureWindowAux) -> ConfigureWindowAux {
    let mut out = aux.clone();
    match (aux.x, aux.width) {
        (Some(x), Some(w)) => {
            let (px0, pw) = px_span(x, w as i32);
            out.x = Some(px0);
            out.width = Some(pw.max(1) as u32);
        }
        _ => {
            out.x = aux.x.map(px);
            out.width = aux.width.map(px_len);
        }
    }
    match (aux.y, aux.height) {
        (Some(y), Some(h)) => {
            let (py0, ph) = px_span(y, h as i32);
            out.y = Some(py0);
            out.height = Some(ph.max(1) as u32);
        }
        _ => {
            out.y = aux.y.map(px);
            out.height = aux.height.map(px_len);
        }
    }
    out.border_width = aux.border_width.map(px_len);
    out
}

/// Rewrites event geometry from physical to logical pixels.
pub(crate) fn scale_event(event: Event) -> Event {
    if ui_scale() == 1.0 {
        return event;
    }
    match event {
        Event::ButtonPress(mut e) => {
            e.root_x = lg16(e.root_x);
            e.root_y = lg16(e.root_y);
            e.event_x = lg16(e.event_x);
            e.event_y = lg16(e.event_y);
            Event::ButtonPress(e)
        }
        Event::ButtonRelease(mut e) => {
            e.root_x = lg16(e.root_x);
            e.root_y = lg16(e.root_y);
            e.event_x = lg16(e.event_x);
            e.event_y = lg16(e.event_y);
            Event::ButtonRelease(e)
        }
        Event::MotionNotify(mut e) => {
            e.root_x = lg16(e.root_x);
            e.root_y = lg16(e.root_y);
            e.event_x = lg16(e.event_x);
            e.event_y = lg16(e.event_y);
            Event::MotionNotify(e)
        }
        Event::EnterNotify(mut e) => {
            e.root_x = lg16(e.root_x);
            e.root_y = lg16(e.root_y);
            e.event_x = lg16(e.event_x);
            e.event_y = lg16(e.event_y);
            Event::EnterNotify(e)
        }
        Event::LeaveNotify(mut e) => {
            e.root_x = lg16(e.root_x);
            e.root_y = lg16(e.root_y);
            e.event_x = lg16(e.event_x);
            e.event_y = lg16(e.event_y);
            Event::LeaveNotify(e)
        }
        Event::ConfigureRequest(mut e) => {
            e.x = lg16(e.x);
            e.y = lg16(e.y);
            e.width = lg_len16(e.width);
            e.height = lg_len16(e.height);
            e.border_width = lg_len16(e.border_width);
            Event::ConfigureRequest(e)
        }
        Event::ConfigureNotify(mut e) => {
            e.x = lg16(e.x);
            e.y = lg16(e.y);
            e.width = lg_len16(e.width);
            e.height = lg_len16(e.height);
            e.border_width = lg_len16(e.border_width);
            Event::ConfigureNotify(e)
        }
        Event::CreateNotify(mut e) => {
            e.x = lg16(e.x);
            e.y = lg16(e.y);
            e.width = lg_len16(e.width);
            e.height = lg_len16(e.height);
            e.border_width = lg_len16(e.border_width);
            Event::CreateNotify(e)
        }
        Event::Expose(mut e) => {
            // Cover the damaged area fully after rounding.
            let x0 = lg(i32::from(e.x));
            let y0 = lg(i32::from(e.y));
            let x1 = ((i32::from(e.x) + i32::from(e.width)) as f32 / ui_scale()).ceil() as i32;
            let y1 = ((i32::from(e.y) + i32::from(e.height)) as f32 / ui_scale()).ceil() as i32;
            e.x = x0.max(0) as u16;
            e.y = y0.max(0) as u16;
            e.width = clamp_u16((x1 - x0).max(0) as u32);
            e.height = clamp_u16((y1 - y0).max(0) as u32);
            Event::Expose(e)
        }
        Event::GravityNotify(mut e) => {
            e.x = lg16(e.x);
            e.y = lg16(e.y);
            Event::GravityNotify(e)
        }
        Event::ReparentNotify(mut e) => {
            e.x = lg16(e.x);
            e.y = lg16(e.y);
            Event::ReparentNotify(e)
        }
        other => other,
    }
}

/// Cookie whose reply is converted to logical pixels.
pub(crate) struct ScaledCookie<'c, R> {
    inner: Cookie<'c, RustConnection, R>,
}

pub(crate) trait ScaleReply {
    fn to_logical(self) -> Self;
}

impl ScaleReply for GetGeometryReply {
    fn to_logical(mut self) -> Self {
        self.x = lg16(self.x);
        self.y = lg16(self.y);
        self.width = lg_len16(self.width);
        self.height = lg_len16(self.height);
        self.border_width = lg_len16(self.border_width);
        self
    }
}

impl ScaleReply for QueryPointerReply {
    fn to_logical(mut self) -> Self {
        self.root_x = lg16(self.root_x);
        self.root_y = lg16(self.root_y);
        self.win_x = lg16(self.win_x);
        self.win_y = lg16(self.win_y);
        self
    }
}

impl ScaleReply for TranslateCoordinatesReply {
    fn to_logical(mut self) -> Self {
        self.dst_x = lg16(self.dst_x);
        self.dst_y = lg16(self.dst_y);
        self
    }
}

impl<R> ScaledCookie<'_, R>
where
    R: ScaleReply + x11rb::x11_utils::TryParse,
{
    pub(crate) fn reply(self) -> Result<R, ReplyError> {
        self.inner.reply().map(ScaleReply::to_logical)
    }
}

/// The X connection as Aurora sees it: logical pixels everywhere. Inherent
/// methods shadow the x11rb extension-trait methods that carry geometry;
/// everything else derefs to the real connection unchanged.
pub(crate) struct ScaledConn(pub(crate) RustConnection);

impl Deref for ScaledConn {
    type Target = RustConnection;
    fn deref(&self) -> &RustConnection {
        &self.0
    }
}

#[allow(clippy::too_many_arguments)]
impl ScaledConn {
    pub(crate) fn configure_window<'c>(
        &'c self,
        window: Window,
        value_list: &ConfigureWindowAux,
    ) -> Result<VoidCookie<'c, RustConnection>, ConnectionError> {
        self.0.configure_window(window, &scale_configure_aux(value_list))
    }

    pub(crate) fn create_window<'c>(
        &'c self,
        depth: u8,
        wid: Window,
        parent: Window,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        class: WindowClass,
        visual: Visualid,
        value_list: &CreateWindowAux,
    ) -> Result<VoidCookie<'c, RustConnection>, ConnectionError> {
        let (px0, pw) = px_span(i32::from(x), i32::from(width));
        let (py0, ph) = px_span(i32::from(y), i32::from(height));
        self.0.create_window(
            depth,
            wid,
            parent,
            clamp_i16(px0),
            clamp_i16(py0),
            clamp_u16(pw.max(1) as u32),
            clamp_u16(ph.max(1) as u32),
            clamp_u16(px_len(u32::from(border_width))),
            class,
            visual,
            value_list,
        )
    }

    pub(crate) fn reparent_window(
        &self,
        window: Window,
        parent: Window,
        x: i16,
        y: i16,
    ) -> Result<VoidCookie<'_, RustConnection>, ConnectionError> {
        self.0.reparent_window(
            window,
            parent,
            clamp_i16(px(i32::from(x))),
            clamp_i16(px(i32::from(y))),
        )
    }

    pub(crate) fn create_pixmap(
        &self,
        depth: u8,
        pid: Pixmap,
        drawable: Drawable,
        width: u16,
        height: u16,
    ) -> Result<VoidCookie<'_, RustConnection>, ConnectionError> {
        self.0.create_pixmap(
            depth,
            pid,
            drawable,
            clamp_u16(px_len(u32::from(width))),
            clamp_u16(px_len(u32::from(height))),
        )
    }

    pub(crate) fn clear_area(
        &self,
        exposures: bool,
        window: Window,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> Result<VoidCookie<'_, RustConnection>, ConnectionError> {
        let r = scale_rect(&Rectangle { x, y, width, height });
        // A zero extent means "to the window edge" in X; keep it zero.
        let w = if width == 0 { 0 } else { r.width };
        let h = if height == 0 { 0 } else { r.height };
        self.0.clear_area(exposures, window, r.x, r.y, w, h)
    }

    pub(crate) fn copy_area(
        &self,
        src_drawable: Drawable,
        dst_drawable: Drawable,
        gc: Gcontext,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> Result<VoidCookie<'_, RustConnection>, ConnectionError> {
        let s = scale_rect(&Rectangle { x: src_x, y: src_y, width, height });
        let d = px(i32::from(dst_x));
        let e = px(i32::from(dst_y));
        self.0.copy_area(
            src_drawable,
            dst_drawable,
            gc,
            s.x,
            s.y,
            clamp_i16(d),
            clamp_i16(e),
            s.width,
            s.height,
        )
    }

    pub(crate) fn poly_fill_rectangle<'c>(
        &'c self,
        drawable: Drawable,
        gc: Gcontext,
        rectangles: &[Rectangle],
    ) -> Result<VoidCookie<'c, RustConnection>, ConnectionError> {
        let scaled: Vec<Rectangle> = rectangles.iter().map(scale_rect).collect();
        self.0.poly_fill_rectangle(drawable, gc, &scaled)
    }

    pub(crate) fn shape_rectangles<'c>(
        &'c self,
        operation: shape::SO,
        destination_kind: shape::SK,
        ordering: ClipOrdering,
        destination_window: Window,
        x_offset: i16,
        y_offset: i16,
        rectangles: &[Rectangle],
    ) -> Result<VoidCookie<'c, RustConnection>, ConnectionError> {
        let scaled: Vec<Rectangle> = rectangles.iter().map(scale_rect).collect();
        self.0.shape_rectangles(
            operation,
            destination_kind,
            ordering,
            destination_window,
            clamp_i16(px(i32::from(x_offset))),
            clamp_i16(px(i32::from(y_offset))),
            &scaled,
        )
    }

    pub(crate) fn get_geometry(
        &self,
        drawable: Drawable,
    ) -> Result<ScaledCookie<'_, GetGeometryReply>, ConnectionError> {
        Ok(ScaledCookie { inner: self.0.get_geometry(drawable)? })
    }

    pub(crate) fn query_pointer(
        &self,
        window: Window,
    ) -> Result<ScaledCookie<'_, QueryPointerReply>, ConnectionError> {
        Ok(ScaledCookie { inner: self.0.query_pointer(window)? })
    }

    pub(crate) fn translate_coordinates(
        &self,
        src_window: Window,
        dst_window: Window,
        src_x: i16,
        src_y: i16,
    ) -> Result<ScaledCookie<'_, TranslateCoordinatesReply>, ConnectionError> {
        Ok(ScaledCookie {
            inner: self.0.translate_coordinates(
                src_window,
                dst_window,
                clamp_i16(px(i32::from(src_x))),
                clamp_i16(px(i32::from(src_y))),
            )?,
        })
    }

    pub(crate) fn poll_for_event(&self) -> Result<Option<Event>, ConnectionError> {
        Ok(self.0.poll_for_event()?.map(scale_event))
    }
}
