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

impl Aurora {
    pub(crate) fn new(
        conn: RustConnection,
        display: String,
        screen: &Screen,
        screen_num: usize,
        compositor_override: Option<bool>,
    ) -> AnyResult<Self> {
        let gc = conn.generate_id()?;
        conn.create_gc(
            gc,
            screen.root,
            &CreateGCAux::new()
                .graphics_exposures(0)
                .foreground(screen.black_pixel)
                .background(screen.white_pixel),
        )?;
        let cursor = create_pointer_cursor(&conn, screen.root)?;
        conn.change_window_attributes(
            screen.root,
            &ChangeWindowAttributesAux::new().cursor(cursor),
        )?;

        let regular = Font::try_from_bytes(FONT_REGULAR).ok_or("failed to load regular font")?;
        let bold = Font::try_from_bytes(FONT_BOLD).ok_or("failed to load bold font")?;
        let terminal_regular = Font::try_from_bytes(FONT_TERMINAL_REGULAR)
            .ok_or("failed to load terminal regular font")?;
        let terminal_bold =
            Font::try_from_bytes(FONT_TERMINAL_BOLD).ok_or("failed to load terminal bold font")?;
        let wallpaper_pixels = render_wallpaper_pixels(
            WALLPAPERS[0].bytes,
            screen.width_in_pixels,
            screen.height_in_pixels,
        )?;
        let (clipboard_image_preview_tx, clipboard_image_preview_rx) = mpsc::channel();
        let mut wallpaper_cache = vec![None; WALLPAPERS.len()];
        wallpaper_cache[0] = Some(wallpaper_pixels.clone());
        let wallpaper_previews = vec![None; WALLPAPERS.len()];
        let mut settings = SettingsState::default();
        if let Some(enabled) = compositor_override {
            settings.compositor_enabled = enabled;
            save_app_commands(&settings)?;
        }
        let compositor_active = if settings.compositor_enabled {
            init_light_compositor(&conn, screen.root)
        } else {
            eprintln!("aurora-wm: light compositor disabled by setting");
            false
        };
        let shape_supported = conn
            .extension_information(shape::X11_EXTENSION_NAME)?
            .is_some();
        let ui = UiWindows {
            topbar: conn.generate_id()?,
            dock: conn.generate_id()?,
            settings: conn.generate_id()?,
            folder: conn.generate_id()?,
            folder_terminal: conn.generate_id()?,
            screenshot_overlay: conn.generate_id()?,
            app_menu: conn.generate_id()?,
            aurora_menu: conn.generate_id()?,
            clipboard_menu: conn.generate_id()?,
            media: [
                conn.generate_id()?,
                conn.generate_id()?,
                conn.generate_id()?,
                conn.generate_id()?,
                conn.generate_id()?,
            ],
            dock_more_menu: conn.generate_id()?,
            title_menu: conn.generate_id()?,
            confirm_dialog: conn.generate_id()?,
            tooltip: conn.generate_id()?,
        };
        let mut sampler = SystemSampler::new();
        let metrics = sampler.sample();
        let (settings_data_tx, settings_data_rx) = mpsc::channel();
        let (terminal_apps, browser_apps, photo_apps, video_apps) = discover_installed_apps();
        let display_modes =
            read_display_modes(&display, screen.width_in_pixels, screen.height_in_pixels);
        let workspace_ui = (0..DEFAULT_WORKSPACE_COUNT)
            .map(|_| WorkspaceUiState::new(screen.height_in_pixels))
            .collect();
        let wm_s_atom = conn
            .intern_atom(false, format!("WM_S{}", screen_num).as_bytes())?
            .reply()?
            .atom;
        let mut app = Self {
            conn,
            display,
            root: screen.root,
            depth: screen.root_depth,
            visual: screen.root_visual,
            gc,
            cursor,
            screen_width: screen.width_in_pixels,
            screen_height: screen.height_in_pixels,
            wallpaper_index: 0,
            wallpaper_pixels,
            wallpaper_cache,
            wallpaper_previews,
            wallpaper_pixmap: None,
            compositor_active,
            shape_supported,
            ui,
            regular,
            bold,
            terminal_regular,
            terminal_bold,
            settings,
            terminal_apps,
            browser_apps,
            photo_apps,
            video_apps,
            display_modes,
            sampler,
            metrics,
            clients: HashMap::new(),
            workspace_count: DEFAULT_WORKSPACE_COUNT,
            active_workspace: 0,
            workspace_ui,
            active_client: None,
            drag: None,
            pending_resize: None,
            pending_ui_resize: None,
            ui_resize: None,
            pending_client_drag: None,
            title_hover: None,
            ignored_unmaps: Vec::new(),
            settings_visible: true,
            settings_front: false,
            folder_mode: FolderMode::Home,
            folder_entries: folder_entries_for(FolderMode::Home, FolderSort::Name),
            folder_places: place_entries(),
            folder_path: folder_path_for(FolderMode::Home),
            folder_selected: None,
            folder_scroll: 0,
            folder_front: false,
            folder_more_open: false,
            folder_sort_open: false,
            folder_sort: FolderSort::Name,
            folder_width: FOLDER_DEFAULT_WIDTH,
            folder_height: (screen.height_in_pixels as f32 * 0.5) as u16,
            folder_terminal_width: TERMINAL_DEFAULT_WIDTH,
            folder_terminal_height: (screen.height_in_pixels as f32 * 0.4) as u16,
            folder_terminal: FolderTerminal::new(folder_path_for(FolderMode::Home)),
            media: None,
            media_slots: vec![None; MEDIA_SLOT_COUNT],
            media_front: false,
            media_front_slot: None,
            media_text_selection: None,
            media_text_selecting: false,
            media_text_selection_redraw_at: None,
            media_text_live_rects: Vec::new(),
            media_context_open: None,
            media_trash_prompt: None,
            app_menu_visible: false,
            app_menu_more: false,
            app_menu_scroll: 0,
            app_menu_query: String::new(),
            app_menu_expanded_categories: HashSet::new(),
            dock_more_visible: false,
            aurora_menu_visible: false,
            aurora_menu_about: false,
            aurora_menu_restart_confirm: false,
            clipboard_menu_visible: false,
            clipboard_history: read_clipboard_history_store(),
            clipboard_history_page: 0,
            clipboard_image_previews: HashMap::new(),
            clipboard_image_preview_pending: HashSet::new(),
            clipboard_image_preview_tx,
            clipboard_image_preview_rx,
            clipboard_poll_rx: None,
            last_clipboard_poll: Instant::now(),
            clipboard_watch_supported: false,
            clipboard_dirty: true,
            last_seen_clipboard_text: None,
            last_seen_clipboard_image_sig: None,
            command_paste_armed: false,
            wm_s_atom,
            folder_context_open: false,
            folder_context_pos: (0, 0),
            folder_clipboard: None,
            folder_info: None,
            folder_terminal_selection: None,
            folder_terminal_selecting: false,
            folder_terminal_live_rects: Vec::new(),
            folder_drag: None,
            folder_press: None,
            xdnd_source: None,
            dock_last_click: None,
            icon_cache: HashMap::new(),
            last_clock_label: format_clock(),
            last_tick: Instant::now(),
            last_media_tick: Instant::now(),
            last_pointer_pos: None,
            last_pointer_activity: Instant::now(),
            pending_auto_power_saver_apply: None,
            screenshot_mode: false,
            screenshot_selection: None,
            screenshot_base: None,
            screenshot_live_rect: None,
            pending_screenshot_button: None,
            topbar_notice: None,
            ffplay_process: None,
            pending_window_nudges: Vec::new(),
            wifi_refresh_rx: None,
            settings_cache: SettingsDataCache::default(),
            settings_data_tx,
            settings_data_rx,
            settings_data_pending: 0,
            settings_hidden_at: None,
            focus_history: Vec::new(),
            alt_tab_index: 0,
            alt_tab_windows: Vec::new(),
            choose_file_mode: false,
            title_menu_open: None,
            title_menu_workspaces: false,
            confirm_close: None,
            tooltip_shown: None,
        };
        app.apply_sleep_timeout();
        if app.settings.auto_power_saver_enabled && app.settings.auto_power_saver_minutes > 0 {
            let _ = touch_notidle_marker();
            let _ = app.set_power_mode(PowerMode::Performance);
        }
        let _ = save_app_commands(&app.settings);
        app.create_ui_windows()?;
        app.request_settings_data(app.settings.tab);
        if let Err(err) = app.init_clipboard_watcher() {
            eprintln!("aurora-wm: clipboard watcher unavailable, using polling: {err}");
        }
        Ok(app)
    }

    pub(crate) fn run_loop(&mut self) -> AnyResult<()> {
        let trace_events = env::var_os("AURORA_TRACE_EVENTS").is_some();
        let mut trace_counts: HashMap<&'static str, usize> = HashMap::new();
        let mut next_trace_log = Instant::now() + Duration::from_secs(1);
        let mut next_pointer_poll = Instant::now();
        loop {
            let mut handled_event = false;
            let mut pending_motion = None;
            while let Some(event) = self.conn.poll_for_event()? {
                if trace_events {
                    *trace_counts.entry(event_name(&event)).or_default() += 1;
                }
                if let Event::MotionNotify(ev) = event {
                    pending_motion = Some(ev);
                } else {
                    handled_event = true;
                    if let Some(ev) = pending_motion.take() {
                        handled_event |= self.handle_motion_notify(ev)?;
                    }
                    self.handle_event(event)?;
                }
            }
            if let Some(ev) = pending_motion.take() {
                handled_event |= self.handle_motion_notify(ev)?;
            }

            if self.folder_terminal.visible && self.poll_folder_terminal()? {
                handled_event = true;
            }
            if self.folder_terminal.visible && self.sync_folder_to_terminal_cwd()? {
                handled_event = true;
            }

            if let Some(pending) = self.pending_resize {
                if pending.pressed_at.elapsed() >= Duration::from_secs(2) {
                    self.pending_resize = None;
                    self.start_resize(
                        pending.client,
                        pending.root_x,
                        pending.root_y,
                        pending.edges,
                    )?;
                }
            }

            if let Some(pending) = self.pending_ui_resize {
                if pending.pressed_at.elapsed() >= Duration::from_secs(1) {
                    self.pending_ui_resize = None;
                    self.start_ui_resize(pending)?;
                }
            }

            let needs_pointer_poll = self.pending_resize.is_some()
                || self.pending_ui_resize.is_some()
                || self.pending_client_drag.is_some()
                || self.settings.auto_power_saver_slider_dragging
                || self.drag.is_some()
                || self.ui_resize.is_some()
                || self.pending_screenshot_button.is_some();
            let now = Instant::now();
            let pointer = if needs_pointer_poll && now >= next_pointer_poll {
                let interval = if self.ui_resize.is_some() {
                    COMPOSITED_MOVE_INTERVAL
                } else if self
                    .drag
                    .is_some_and(|drag| matches!(drag.kind, DragKind::Move))
                    && !self.compositor_active
                {
                    NON_COMPOSITED_MOVE_INTERVAL
                } else if self.drag.is_some() {
                    COMPOSITED_MOVE_INTERVAL
                } else {
                    Duration::from_millis(50)
                };
                next_pointer_poll = now + interval;
                Some(self.conn.query_pointer(self.root)?.reply()?)
            } else {
                None
            };

            if let (Some(pending), Some(pointer)) = (self.pending_resize, pointer.as_ref()) {
                let button_down = u16::from(pointer.mask) & u16::from(KeyButMask::BUTTON1) != 0;
                if !button_down || pending.pressed_at.elapsed() >= Duration::from_secs(5) {
                    self.pending_resize = None;
                    let _ = self.conn.ungrab_pointer(CURRENT_TIME);
                }
            }

            if let (Some(pending), Some(pointer)) = (self.pending_ui_resize, pointer.as_ref()) {
                let button_down = u16::from(pointer.mask) & u16::from(KeyButMask::BUTTON1) != 0;
                if !button_down || pending.pressed_at.elapsed() >= Duration::from_secs(5) {
                    self.pending_ui_resize = None;
                    let _ = self.conn.ungrab_pointer(CURRENT_TIME);
                }
            }

            if let (Some(pending), Some(pointer)) = (self.pending_client_drag, pointer.as_ref()) {
                let button_down = u16::from(pointer.mask) & u16::from(KeyButMask::BUTTON1) != 0;
                if !button_down {
                    self.pending_client_drag = None;
                } else {
                    let moved = (i32::from(pointer.root_x) - i32::from(pending.root_x)).abs() > 4
                        || (i32::from(pointer.root_y) - i32::from(pending.root_y)).abs() > 4;
                    if moved && self.drag.is_none() {
                        self.pending_client_drag = None;
                        self.start_drag(pending.client, pointer.root_x, pointer.root_y)?;
                    } else if pending.pressed_at.elapsed() >= Duration::from_secs(2) {
                        self.pending_client_drag = None;
                    }
                }
            }
            if self.settings.auto_power_saver_slider_dragging {
                if let Some(pointer) = pointer.as_ref() {
                    let button_down =
                        u16::from(pointer.mask) & u16::from(KeyButMask::BUTTON1) != 0;
                    if button_down {
                        let (settings_x, _, _, _) = self.settings_geometry();
                        self.set_auto_power_saver_from_slider(
                            i32::from(pointer.root_x) - i32::from(settings_x),
                        )?;
                    } else {
                        self.settings.auto_power_saver_slider_dragging = false;
                        self.pending_auto_power_saver_apply = None;
                        self.apply_auto_power_saver_setting()?;
                        self.redraw_settings()?;
                    }
                    handled_event = true;
                }
            }
            if let Some(pointer) = pointer.as_ref().filter(|_| self.drag.is_some()) {
                let button_down = u16::from(pointer.mask) & u16::from(KeyButMask::BUTTON1) != 0;
                if button_down {
                    handled_event |= self.update_drag_position(pointer.root_x, pointer.root_y)?;
                } else {
                    self.drag = None;
                    let _ = self.conn.ungrab_pointer(CURRENT_TIME);
                }
            }
            if let Some(pointer) = pointer.as_ref().filter(|_| self.ui_resize.is_some()) {
                let button_down = u16::from(pointer.mask) & u16::from(KeyButMask::BUTTON1) != 0;
                if button_down {
                    handled_event |= self.update_ui_resize(pointer.root_x, pointer.root_y)?;
                } else {
                    self.end_drag()?;
                }
            }

            if let (Some(pending), Some(pointer)) =
                (self.pending_screenshot_button, pointer.as_ref())
            {
                let button_down = u16::from(pointer.mask) & u16::from(KeyButMask::BUTTON1) != 0;
                if button_down && pending.pressed_at.elapsed() >= Duration::from_secs(2) {
                    self.pending_screenshot_button = None;
                    self.capture_screenshot(None)?;
                    handled_event = true;
                } else if !button_down {
                    self.pending_screenshot_button = None;
                }
            }

            if self
                .topbar_notice
                .as_ref()
                .is_some_and(|(_, until)| Instant::now() >= *until)
            {
                self.topbar_notice = None;
                self.redraw_topbar()?;
                handled_event = true;
            }

            if handled_event {
                self.conn.flush()?;
            }

            if self.has_playing_internal_media()
                && self.last_media_tick.elapsed() >= Duration::from_millis(250)
            {
                self.last_media_tick = Instant::now();
                if self.advance_internal_media()? {
                    self.conn.flush()?;
                }
            }

            if self.process_pending_window_nudges()? {
                handled_event = true;
            }
            if self.poll_settings_data()? {
                handled_event = true;
            }
            self.reap_ffplay_process();

            if self
                .pending_auto_power_saver_apply
                .is_some_and(|at| Instant::now() >= at)
            {
                self.pending_auto_power_saver_apply = None;
                if self.apply_auto_power_saver_setting()? {
                    self.conn.flush()?;
                }
            }

            let interactive = self.drag.is_some()
                || self.ui_resize.is_some()
                || self.pending_resize.is_some()
                || self.pending_ui_resize.is_some()
                || self.pending_client_drag.is_some()
                || self.settings.auto_power_saver_slider_dragging
                || self.pending_screenshot_button.is_some();

            if !interactive && self.last_tick.elapsed() >= IDLE_CHECK_INTERVAL {
                self.last_tick = Instant::now();
                let mut idle_changed = false;

                if self.poll_wifi_refresh()? {
                    idle_changed = true;
                }
                if self.poll_clipboard_history()? {
                    idle_changed = true;
                }
                if self.poll_clipboard_image_previews()? {
                    idle_changed = true;
                }
                if self.refresh_folder_entries() {
                    self.redraw_folder()?;
                    idle_changed = true;
                }
                if self.sync_current_power_mode()? {
                    idle_changed = true;
                }
                if self.settings.auto_power_saver_enabled
                    && self.settings.auto_power_saver_minutes > 0
                    && self.update_auto_power_saver()?
                {
                    idle_changed = true;
                }

                let clock_label = format_clock();
                let clock_changed = clock_label != self.last_clock_label;
                let metrics_visible = self.settings_visible
                    && matches!(self.settings.tab, SettingsTab::Power | SettingsTab::About);
                if clock_changed || metrics_visible {
                    self.metrics = self.sampler.sample();
                    self.metrics.gpu_usage = self
                        .settings_cache
                        .gpu_usage
                        .clone()
                        .unwrap_or_default();
                }
                if clock_changed {
                    self.last_clock_label = clock_label;
                    self.redraw_topbar()?;
                    idle_changed = true;
                }
                if self.settings_visible {
                    // Kick off async refreshes for the visible tab; results
                    // arrive via poll_settings_data without blocking drawing.
                    self.request_settings_data(self.settings.tab);
                    if matches!(
                        self.settings.tab,
                        SettingsTab::Network | SettingsTab::Power | SettingsTab::About
                    ) {
                        self.redraw_settings()?;
                        idle_changed = true;
                    }
                } else if self
                    .settings_hidden_at
                    .is_some_and(|at| at.elapsed() >= Duration::from_secs(10))
                {
                    // Settings hidden for a while: drop its cached data so it
                    // costs nothing until it is opened again.
                    self.settings_hidden_at = None;
                    self.settings_cache = SettingsDataCache::default();
                    self.wifi_refresh_rx = None;
                }
                if idle_changed {
                    self.conn.flush()?;
                }
            }

            if trace_events && Instant::now() >= next_trace_log {
                if !trace_counts.is_empty() {
                    eprintln!("event counts: {:?}", trace_counts);
                    trace_counts.clear();
                }
                next_trace_log = Instant::now() + Duration::from_secs(1);
            }

            wait_for_x_event_or_timeout(
                &self.conn,
                self.loop_wait_timeout(handled_event, needs_pointer_poll, next_pointer_poll),
            );
        }
    }

    pub(crate) fn has_playing_internal_media(&self) -> bool {
        self.media_slots.iter().any(|slot| {
            slot.as_ref().is_some_and(|media| {
                media.playing && matches!(media.entry.kind, FileKind::Audio | FileKind::Video)
            })
        })
    }

    pub(crate) fn loop_wait_timeout(
        &self,
        handled_event: bool,
        needs_pointer_poll: bool,
        next_pointer_poll: Instant,
    ) -> Duration {
        let now = Instant::now();
        let interactive = self.drag.is_some()
            || self.ui_resize.is_some()
            || self.pending_resize.is_some()
            || self.pending_ui_resize.is_some()
            || self.pending_client_drag.is_some()
            || self.settings.auto_power_saver_slider_dragging
            || self.pending_screenshot_button.is_some();
        let mut timeout = if handled_event {
            if interactive {
                Duration::from_millis(1)
            } else {
                Duration::from_millis(4)
            }
        } else if interactive {
            Duration::from_millis(16)
        } else {
            IDLE_CHECK_INTERVAL
        };

        if needs_pointer_poll {
            timeout = timeout.min(next_pointer_poll.saturating_duration_since(now));
        }
        if let Some(pending) = self.pending_resize {
            timeout = timeout
                .min((pending.pressed_at + Duration::from_secs(2)).saturating_duration_since(now));
        }
        if let Some(pending) = self.pending_ui_resize {
            timeout = timeout
                .min((pending.pressed_at + Duration::from_secs(1)).saturating_duration_since(now));
        }
        if let Some(pending) = self.pending_client_drag {
            timeout = timeout.min(
                (pending.pressed_at + Duration::from_millis(500)).saturating_duration_since(now),
            );
        }
        if let Some(pending) = self.pending_screenshot_button {
            timeout = timeout
                .min((pending.pressed_at + Duration::from_secs(2)).saturating_duration_since(now));
        }
        if let Some((_, until)) = self.topbar_notice.as_ref() {
            timeout = timeout.min((*until).saturating_duration_since(now));
        }
        if let Some(at) = self.pending_auto_power_saver_apply {
            timeout = timeout.min(at.saturating_duration_since(now));
        }
        if self.has_playing_internal_media() {
            timeout = timeout.min(
                (self.last_media_tick + Duration::from_millis(250)).saturating_duration_since(now),
            );
        }
        if self.settings_data_pending != 0 {
            // Background settings loaders are running; wake soon so their
            // results are applied as they arrive.
            timeout = timeout.min(Duration::from_millis(60));
        }
        if !interactive {
            timeout =
                timeout.min((self.last_tick + IDLE_CHECK_INTERVAL).saturating_duration_since(now));
        }
        timeout
    }

    pub(crate) fn update_auto_power_saver(&mut self) -> AnyResult<bool> {
        self.mark_current_display_activity()?;
        let threshold =
            Duration::from_secs(u64::from(self.settings.auto_power_saver_minutes.max(1)) * 60);
        let idle_long_enough = notidle_marker_age().is_none_or(|age| age > threshold);
        if !idle_long_enough && self.settings.power_mode != PowerMode::Performance {
            self.set_power_mode(PowerMode::Performance)?;
            if self.settings_visible && self.settings.tab == SettingsTab::Power {
                self.redraw_settings()?;
            }
            return Ok(true);
        }
        if idle_long_enough && self.settings.power_mode != PowerMode::Saver {
            self.set_power_mode(PowerMode::Saver)?;
            if self.settings_visible && self.settings.tab == SettingsTab::Power {
                self.redraw_settings()?;
            }
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) fn mark_current_display_activity(&mut self) -> AnyResult<()> {
        let active_window_ms = (IDLE_CHECK_INTERVAL + Duration::from_millis(250)).as_millis();
        if let Ok(cookie) = self.conn.screensaver_query_info(self.root) {
            if let Ok(info) = cookie.reply() {
                if u128::from(info.ms_since_user_input) <= active_window_ms {
                    touch_notidle_marker()?;
                }
                return Ok(());
            }
        }

        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        let pos = (pointer.root_x, pointer.root_y);
        let moved = self.last_pointer_pos.is_none_or(|last| last != pos);
        self.last_pointer_pos = Some(pos);
        if moved {
            self.last_pointer_activity = Instant::now();
        }
        if moved
            || self
                .last_pointer_activity
                .elapsed()
                .saturating_sub(IDLE_CHECK_INTERVAL)
                < Duration::from_millis(250)
        {
            touch_notidle_marker()?;
        }
        Ok(())
    }

    pub(crate) fn sync_current_power_mode(&mut self) -> AnyResult<bool> {
        let Some(mode) = current_power_mode_cached_or_refresh() else {
            return Ok(false);
        };
        if mode == self.settings.power_mode {
            return Ok(false);
        }
        self.settings.power_mode = mode;
        Ok(true)
    }

    pub(crate) fn apply_auto_power_saver_setting(&mut self) -> AnyResult<bool> {
        self.settings.auto_power_saver_minutes = self
            .settings
            .auto_power_saver_input
            .trim()
            .parse::<u32>()
            .unwrap_or(self.settings.auto_power_saver_minutes)
            .clamp(AUTO_POWER_SAVER_MIN_MINUTES, AUTO_POWER_SAVER_MAX_MINUTES);
        self.settings.auto_power_saver_input = self.settings.auto_power_saver_minutes.to_string();
        self.last_pointer_activity = Instant::now();
        self.last_pointer_pos = None;
        if self.settings.auto_power_saver_enabled {
            touch_notidle_marker()?;
            self.set_power_mode(PowerMode::Performance)?;
        }
        save_app_commands(&self.settings)?;
        Ok(true)
    }

    pub(crate) fn init_clipboard_watcher(&mut self) -> AnyResult<()> {
        if self
            .conn
            .extension_information(xfixes::X11_EXTENSION_NAME)?
            .is_none()
        {
            return Ok(());
        }
        self.conn.xfixes_query_version(5, 0)?.reply()?;
        let clipboard = self.atom(b"CLIPBOARD")?;
        let mask = xfixes::SelectionEventMask::SET_SELECTION_OWNER
            | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
            | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE;
        self.conn
            .xfixes_select_selection_input(self.root, clipboard, mask)?
            .check()?;
        self.clipboard_watch_supported = true;
        self.clipboard_dirty = true;
        Ok(())
    }

    pub(crate) fn create_ui_windows(&mut self) -> AnyResult<()> {
        self.grab_root_button1()?;
        self.grab_alt_tab()?;
        self.grab_workspace_keys()?;
        self.grab_command_paste()?;
        self.grab_configured_shortcuts()?;
        self.publish_ewmh_support()?;
        self.create_extra_ui_windows()?;
        let top_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::LEAVE_WINDOW,
            )
            .cursor(self.cursor)
            .background_pixel(0)
            .backing_store(BackingStore::WHEN_MAPPED);
        self.conn.create_window(
            self.depth,
            self.ui.topbar,
            self.root,
            0,
            0,
            self.screen_width,
            TOPBAR_HEIGHT,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &top_aux,
        )?;

        let dock = self.dock_geometry();
        let dock_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS)
            .cursor(self.cursor)
            .background_pixel(0)
            .backing_store(BackingStore::WHEN_MAPPED);
        self.conn.create_window(
            self.depth,
            self.ui.dock,
            self.root,
            dock.0,
            dock.1,
            dock.2,
            dock.3,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &dock_aux,
        )?;

        let settings = self.settings_geometry();
        let settings_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::KEY_PRESS,
            )
            .cursor(self.cursor)
            .background_pixel(0)
            .bit_gravity(Gravity::NORTH_WEST)
            .backing_store(BackingStore::WHEN_MAPPED)
            .save_under(1);
        self.conn.create_window(
            self.depth,
            self.ui.settings,
            self.root,
            settings.0,
            settings.1,
            settings.2,
            settings.3,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &settings_aux,
        )?;

        let folder = self.folder_geometry();
        let folder_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION,
            )
            .cursor(self.cursor)
            .background_pixel(0)
            .bit_gravity(Gravity::NORTH_WEST)
            .backing_store(BackingStore::WHEN_MAPPED)
            .save_under(1);
        self.conn.create_window(
            self.depth,
            self.ui.folder,
            self.root,
            folder.0,
            folder.1,
            folder.2,
            folder.3,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &folder_aux,
        )?;
        self.init_folder_dnd()?;

        let terminal = self.folder_terminal_geometry();
        let terminal_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::KEY_PRESS,
            )
            .cursor(self.cursor)
            .background_pixel(0)
            .bit_gravity(Gravity::NORTH_WEST)
            .backing_store(BackingStore::WHEN_MAPPED)
            .save_under(1);
        self.conn.create_window(
            self.depth,
            self.ui.folder_terminal,
            self.root,
            terminal.0,
            terminal.1,
            terminal.2,
            terminal.3,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &terminal_aux,
        )?;

        let overlay_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION,
            )
            .cursor(self.cursor)
            .background_pixel(0)
            .backing_store(BackingStore::WHEN_MAPPED);
        self.conn.create_window(
            self.depth,
            self.ui.screenshot_overlay,
            self.root,
            0,
            0,
            self.screen_width,
            self.screen_height,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &overlay_aux,
        )?;

        let menu = self.app_menu_geometry();
        let menu_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::KEY_PRESS)
            .cursor(self.cursor)
            .background_pixel(0)
            .backing_store(BackingStore::WHEN_MAPPED);
        self.conn.create_window(
            self.depth,
            self.ui.app_menu,
            self.root,
            menu.0,
            menu.1,
            menu.2,
            menu.3,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &menu_aux,
        )?;

        let more_menu = self.dock_more_menu_geometry();
        let more_menu_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS)
            .cursor(self.cursor)
            .background_pixel(0)
            .backing_store(BackingStore::WHEN_MAPPED);
        self.conn.create_window(
            self.depth,
            self.ui.dock_more_menu,
            self.root,
            more_menu.0,
            more_menu.1,
            more_menu.2,
            more_menu.3,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &more_menu_aux,
        )?;

        let aurora_menu = self.aurora_menu_geometry();
        let aurora_menu_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS)
            .cursor(self.cursor)
            .background_pixel(0)
            .backing_store(BackingStore::WHEN_MAPPED);
        self.conn.create_window(
            self.depth,
            self.ui.aurora_menu,
            self.root,
            aurora_menu.0,
            aurora_menu.1,
            aurora_menu.2,
            aurora_menu.3,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &aurora_menu_aux,
        )?;

        let clipboard_menu = self.clipboard_menu_geometry();
        let clipboard_menu_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS)
            .cursor(self.cursor)
            .background_pixel(0)
            .backing_store(BackingStore::WHEN_MAPPED);
        self.conn.create_window(
            self.depth,
            self.ui.clipboard_menu,
            self.root,
            clipboard_menu.0,
            clipboard_menu.1,
            clipboard_menu.2,
            clipboard_menu.3,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &clipboard_menu_aux,
        )?;

        let media_aux = CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::KEY_PRESS,
            )
            .cursor(self.cursor)
            .background_pixel(0)
            .backing_store(BackingStore::WHEN_MAPPED);
        for (idx, window) in self.ui.media.iter().copied().enumerate() {
            let media = self.media_geometry(idx);
            self.conn.create_window(
                self.depth,
                window,
                self.root,
                media.0,
                media.1,
                media.2,
                media.3,
                0,
                WindowClass::INPUT_OUTPUT,
                self.visual,
                &media_aux,
            )?;
        }

        self.conn.map_window(self.ui.topbar)?;
        self.conn.map_window(self.ui.dock)?;
        self.conn.map_window(self.ui.settings)?;

        // Initialize EWMH desktops on the root window
        if let Ok(num_atom) = self.atom(b"_NET_NUMBER_OF_DESKTOPS") {
            if let Ok(cardinal_atom) = self.atom(b"CARDINAL") {
                let _ = self.conn.change_property32(
                    PropMode::REPLACE,
                    self.root,
                    num_atom,
                    cardinal_atom,
                    &[self.workspace_count as u32],
                );
            }
        }
        if let Ok(cur_atom) = self.atom(b"_NET_CURRENT_DESKTOP") {
            if let Ok(cardinal_atom) = self.atom(b"CARDINAL") {
                let _ = self.conn.change_property32(
                    PropMode::REPLACE,
                    self.root,
                    cur_atom,
                    cardinal_atom,
                    &[self.active_workspace as u32],
                );
            }
        }

        self.install_pointer_cursor()?;
        self.raise_ui()?;
        self.conn.flush()?;
        Ok(())
    }

    pub(crate) fn install_pointer_cursor(&self) -> AnyResult<()> {
        let mut windows = vec![
            self.root,
            self.ui.topbar,
            self.ui.dock,
            self.ui.settings,
            self.ui.folder,
            self.ui.folder_terminal,
            self.ui.app_menu,
            self.ui.aurora_menu,
            self.ui.clipboard_menu,
            self.ui.dock_more_menu,
        ];
        windows.extend(self.ui.media);
        windows.extend(self.clients.values().map(|info| info.frame));

        for window in windows {
            self.conn.change_window_attributes(
                window,
                &ChangeWindowAttributesAux::new().cursor(self.cursor),
            )?;
        }
        if self
            .conn
            .extension_information(xfixes::X11_EXTENSION_NAME)?
            .is_some()
        {
            let _ = self.conn.xfixes_show_cursor(self.root);
        }
        Ok(())
    }

    pub(crate) fn init_folder_dnd(&self) -> AnyResult<()> {
        let xdnd_aware = self.atom(b"XdndAware")?;
        let atom_type = self.atom(b"ATOM")?;
        self.conn.change_property32(
            PropMode::REPLACE,
            self.ui.folder,
            xdnd_aware,
            atom_type,
            &[5],
        )?;
        Ok(())
    }

    pub(crate) fn scan_existing_windows(&mut self) -> AnyResult<()> {
        let reply = self.conn.query_tree(self.root)?.reply()?;
        for window in reply.children {
            if let Err(err) = self.adopt_mapped_root_window(window) {
                eprintln!("aurora-wm: failed to adopt existing window {window}: {err}");
            }
        }
        Ok(())
    }

    pub(crate) fn adopt_mapped_root_window(&mut self, window: Window) -> AnyResult<()> {
        if self.is_ui_window(window) || self.client_key_for(window).is_some() {
            return Ok(());
        }
        let attr = self.conn.get_window_attributes(window)?.reply()?;
        if attr.override_redirect && attr.map_state != MapState::UNMAPPED {
            let _ = self.conn.change_window_attributes(
                window,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            );
            let _ = self.suppress_uncomposited_cursor_overlay(window);
        } else if attr.map_state != MapState::UNMAPPED {
            self.manage_window(window)?;
        }
        Ok(())
    }

}
