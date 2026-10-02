//! Recording control gestures and the maximum-duration menu.
use crate::canvas::*;
use crate::draw_helpers::*;
use crate::model::*;
use crate::*;

pub(crate) const RECORDING_HOLD_DELAY: Duration = Duration::from_millis(600);
const DURATION_OPTIONS: [(u64, &str); 5] = [
    (30 * 60, "30 minutes"),
    (60 * 60, "1 hour"),
    (4 * 60 * 60, "4 hours"),
    (8 * 60 * 60, "8 hours"),
    (24 * 60 * 60, "24 hours"),
];

pub(crate) struct PendingRecordingButton {
    pub(crate) pressed_at: Instant,
    pub(crate) menu_opened: bool,
}

fn duration_label(duration: Duration) -> String {
    DURATION_OPTIONS
        .iter()
        .find(|(seconds, _)| *seconds == duration.as_secs())
        .map(|(_, label)| (*label).to_string())
        .unwrap_or_else(|| format!("{} minutes", duration.as_secs() / 60))
}

fn elapsed_label(seconds: u64) -> String {
    if seconds < 3600 {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    } else {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    }
}

impl Aurora {
    pub(crate) fn recording_button_contains(&self, x: i32, y: i32) -> bool {
        let controls = self.topbar_controls();
        (controls.recording_x - controls.recording_half_width
            ..=controls.recording_x + controls.recording_half_width)
            .contains(&x)
            && (0..i32::from(TOPBAR_HEIGHT)).contains(&y)
    }

    pub(crate) fn recording_tooltip(&self) -> String {
        let action = self
            .recording
            .as_ref()
            .map(|state| state.label())
            .unwrap_or("Record screen with system audio and microphone");
        let limit = self
            .recording
            .as_ref()
            .and_then(|state| state.duration_limit())
            .unwrap_or(self.recording_max_duration);
        format!(
            "{action} · Max {} · Hold for duration",
            duration_label(limit)
        )
    }

    pub(crate) fn draw_topbar_recording_icon(&self, c: &mut Canvas, controls: &TopbarControls) {
        let Some(state) = self.recording.as_ref() else {
            draw_record_icon(c, controls.recording_x, 20, BLUE_LIGHT);
            return;
        };
        // The timer is part of the camcorder body. Keep the control wide for all
        // active stages so a press never moves while preparation finishes.
        let x = controls.recording_x - controls.recording_half_width + 4;
        let width = controls.recording_half_width * 2 - 22;
        let color = if state.is_recording() { RED_LIGHT } else { BLUE_LIGHT };
        draw_record_camera(c, x, 20, width, color);
        let label = state
            .elapsed()
            .map(|elapsed| elapsed_label(elapsed.as_secs()))
            .unwrap_or_else(|| match state {
                crate::screen_record::RecordingState::Preparing { .. } => "…".to_string(),
                crate::screen_record::RecordingState::Countdown { shown, .. } => shown.to_string(),
                _ => "Saving".to_string(),
            });
        // Center the visible glyph bounds, including descenders in "Saving",
        // so the 12px text stays inside the shared 16px camera height.
        let scale = Scale::uniform(12.0);
        let ascent = self.bold.v_metrics(scale).ascent;
        let bounds = self
            .bold
            .layout(&label, scale, point(0.0, ascent))
            .filter_map(|glyph| glyph.pixel_bounding_box())
            .fold(None::<(i32, i32)>, |bounds, bbox| {
                Some(match bounds {
                    Some((top, bottom)) => (top.min(bbox.min.y), bottom.max(bbox.max.y)),
                    None => (bbox.min.y, bbox.max.y),
                })
            });
        let label_y = bounds
            .map(|(top, bottom)| 20 - (top + bottom) / 2)
            .unwrap_or(14);
        c.draw_text_center(&self.bold, &label, x + width / 2, label_y, 12.0, INK);
    }

    pub(crate) fn press_recording_button(&mut self) -> AnyResult<()> {
        self.hide_tooltip()?;
        self.pending_recording_button = Some(PendingRecordingButton {
            pressed_at: Instant::now(),
            menu_opened: false,
        });
        self.conn
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
            .reply()?;
        Ok(())
    }

    pub(crate) fn poll_recording_button(&mut self) -> AnyResult<bool> {
        if !self
            .pending_recording_button
            .as_ref()
            .is_some_and(|pending| {
                !pending.menu_opened && pending.pressed_at.elapsed() >= RECORDING_HOLD_DELAY
            })
        {
            return Ok(false);
        }
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        if !self.recording_button_contains(i32::from(pointer.root_x), i32::from(pointer.root_y)) {
            // Cancel an outside hold while retaining the release route.
            if let Some(pending) = self.pending_recording_button.as_mut() {
                pending.menu_opened = true;
            }
            return Ok(false);
        }
        if let Some(pending) = self.pending_recording_button.as_mut() {
            pending.menu_opened = true;
        }
        self.show_recording_menu()?;
        Ok(true)
    }

    pub(crate) fn release_recording_button(&mut self, ev: ButtonReleaseEvent) -> AnyResult<()> {
        if ev.detail != 1 {
            return Ok(());
        }
        let Some(pending) = self.pending_recording_button.take() else {
            return Ok(());
        };
        self.conn.ungrab_pointer(CURRENT_TIME)?;
        if pending.menu_opened {
            return Ok(());
        }
        if !self.recording_button_contains(i32::from(ev.root_x), i32::from(ev.root_y)) {
            return Ok(());
        }
        if pending.pressed_at.elapsed() >= RECORDING_HOLD_DELAY {
            self.show_recording_menu()?;
        } else {
            self.toggle_screen_recording()?;
        }
        Ok(())
    }

    fn recording_menu_geometry(&self) -> (i16, i16, u16, u16) {
        let width = 260.min(self.screen_width);
        let x = (self.topbar_controls().recording_x - i32::from(width) / 2)
            .clamp(0, i32::from(self.screen_width.saturating_sub(width)));
        (x as i16, TOPBAR_HEIGHT as i16 + 4, width, 272)
    }

    pub(crate) fn show_recording_menu(&mut self) -> AnyResult<()> {
        self.hide_tooltip()?;
        self.hide_app_menu()?;
        self.hide_aurora_menu()?;
        self.hide_clipboard_menu()?;
        self.recording_menu_visible = true;
        let (x, y, w, h) = self.recording_menu_geometry();
        self.conn.configure_window(
            self.ui.recording_menu,
            &ConfigureWindowAux::new()
                .x(i32::from(x))
                .y(i32::from(y))
                .width(u32::from(w))
                .height(u32::from(h))
                .stack_mode(StackMode::ABOVE),
        )?;
        self.conn.map_window(self.ui.recording_menu)?;
        self.conn
            .grab_keyboard(
                false,
                self.ui.recording_menu,
                CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )?
            .reply()?;
        self.redraw_recording_menu()?;
        self.conn.flush()?;
        Ok(())
    }

    pub(crate) fn hide_recording_menu(&mut self) -> AnyResult<()> {
        if self.recording_menu_visible {
            self.recording_menu_visible = false;
            self.conn.unmap_window(self.ui.recording_menu)?;
            self.conn.ungrab_keyboard(CURRENT_TIME)?;
        }
        Ok(())
    }

    pub(crate) fn redraw_recording_menu(&self) -> AnyResult<()> {
        if !self.recording_menu_visible {
            return Ok(());
        }
        let (_, _, w, h) = self.recording_menu_geometry();
        let mut c = Canvas::new(w, h, Color::rgb(244, 250, 254));
        c.draw_round_rect(
            0,
            0,
            i32::from(w),
            i32::from(h),
            12,
            Color::rgb(244, 250, 254),
        );
        c.draw_text(&self.bold, "Maximum recording time", 16, 13, 14.0, INK);
        for (index, (seconds, label)) in DURATION_OPTIONS.iter().enumerate() {
            let y = 48 + index as i32 * 34;
            let selected = self.recording_max_duration.as_secs() == *seconds;
            if selected {
                c.draw_round_rect(8, y, i32::from(w) - 16, 31, 7, BLUE_LIGHT);
            }
            c.draw_text(&self.regular, label, 18, y + 5, 14.0, INK);
            if selected {
                draw_round_line(
                    &mut c,
                    i32::from(w) - 35,
                    y + 16,
                    i32::from(w) - 30,
                    y + 21,
                    2,
                    BLUE,
                );
                draw_round_line(
                    &mut c,
                    i32::from(w) - 30,
                    y + 21,
                    i32::from(w) - 21,
                    y + 11,
                    2,
                    BLUE,
                );
            }
        }
        c.draw_text(
            &self.regular,
            if self.recording.is_some() {
                "Applies to the next recording"
            } else {
                "Stops and saves automatically"
            },
            16,
            231,
            12.0,
            MUTED,
        );
        self.upload_canvas(self.ui.recording_menu, &c)
    }

    pub(crate) fn route_recording_menu_press(&mut self, ev: ButtonPressEvent) -> AnyResult<bool> {
        if !self.recording_menu_visible {
            return Ok(false);
        }
        let (mx, my, mw, mh) = self.recording_menu_geometry();
        let x = i32::from(ev.root_x) - i32::from(mx);
        let y = i32::from(ev.root_y) - i32::from(my);
        if (0..i32::from(mw)).contains(&x) && (0..i32::from(mh)).contains(&y) {
            if ev.event == self.root {
                self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
            }
            if ev.detail == 1 && y >= 48 {
                let row = (y - 48) / 34;
                if let Some((seconds, _)) = DURATION_OPTIONS.get(row as usize) {
                    self.recording_max_duration = Duration::from_secs(*seconds);
                    self.hide_recording_menu()?;
                    self.redraw_topbar()?;
                }
            }
            return Ok(true);
        }
        self.hide_recording_menu()?;
        if self.recording_button_contains(i32::from(ev.root_x), i32::from(ev.root_y)) {
            if ev.event == self.root {
                self.conn.allow_events(Allow::ASYNC_POINTER, ev.time)?;
            }
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_timer_remains_readable_across_hour_boundaries() {
        assert_eq!(elapsed_label(0), "00:00");
        assert_eq!(elapsed_label(3599), "59:59");
        assert_eq!(elapsed_label(3600), "1:00:00");
        assert_eq!(elapsed_label(24 * 3600), "24:00:00");
    }
}
