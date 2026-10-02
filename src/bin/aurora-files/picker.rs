//! Reusable Files picker and small file-operation prompts. See PICKER_PROTOCOL.md.
use super::*;

pub(super) const PICKER_FOOTER_H: i32 = 98;
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PickerMode { Open, Multiple, Save, Directory }
pub(super) struct Picker {
    pub mode: PickerMode,
    pub initial: PathBuf,
    pub title: String,
    pub name: String,
    pub filter: String,
    pub requester: Option<Window>,
    pub selected: HashSet<PathBuf>,
    pub overwrite: Option<PathBuf>,
    pub null_output: bool,
    pub all_files: bool,
    pub editing_name: bool,
}
impl Picker {
    fn new(mode: &str) -> AnyResult<Self> {
        Ok(Self {
            mode: match mode { "open" => PickerMode::Open, "multiple" => PickerMode::Multiple,
                "save" => PickerMode::Save, "directory" => PickerMode::Directory,
                _ => return Err("unknown picker mode".into()) },
            initial: home_dir(), title: match mode { "save" => "Save file", "directory" => "Select folder", "multiple" => "Choose files", _ => "Choose a file" }.into(), name: String::new(),
            filter: String::new(), requester: None, selected: HashSet::new(),
            overwrite: None, null_output: false, all_files: false, editing_name: false,
        })
    }
    pub fn from_args(args: &[String]) -> AnyResult<Option<Self>> {
        let mode = args.iter().find_map(|a| match a.as_str() {
            "--choose-file" => Some("open"), "--choose-files" => Some("multiple"),
            "--save-file" => Some("save"), "--choose-directory" => Some("directory"), _ => None });
        let Some(mode) = mode else { return Ok(None); };
        let mut picker = Self::new(mode)?;
        for (i, arg) in args.iter().enumerate() {
            if ["--path", "--title", "--filename", "--filter"].contains(&arg.as_str()) {
                let value = args.get(i + 1).ok_or("missing picker option value")?;
                match arg.as_str() { "--path" => picker.initial = PathBuf::from(value),
                    "--title" => picker.title = value.clone(), "--filename" => picker.name = value.clone(),
                    _ => picker.filter = value.clone() }
            }
        }
        picker.null_output = args.iter().any(|a| a == "--null");
        picker.editing_name = picker.mode == PickerMode::Save;
        Ok(Some(picker))
    }
    fn from_wire(bytes: &[u8], requester: Window) -> AnyResult<Self> {
        if bytes.len() > 65536 || bytes.last() != Some(&0) { return Err("invalid picker request size or terminator".into()); }
        let fields = std::str::from_utf8(bytes)?.trim_end_matches('\0').split('\0').collect::<Vec<_>>();
        if fields.len() < 2 || fields[0] != "1" { return Err("unsupported picker protocol".into()); }
        let mut picker = Self::new(fields[1])?;
        if let Some(path) = fields.get(2).filter(|p| !p.is_empty()) { picker.initial = PathBuf::from(path); }
        if let Some(title) = fields.get(3).filter(|p| !p.is_empty()) { picker.title = title.to_string(); }
        picker.name = fields.get(4).unwrap_or(&"").to_string();
        picker.filter = fields.get(5).unwrap_or(&"").to_string();
        picker.requester = Some(requester);
        picker.editing_name = picker.mode == PickerMode::Save;
        Ok(picker)
    }
    pub fn matches(&self, entry: &Entry) -> bool {
        entry.kind == FileKind::Directory || (self.mode != PickerMode::Directory &&
            (self.all_files || self.filter.trim().is_empty() || self.filter.split(';').any(|pattern| glob_matches(pattern.trim(), &entry.name))))
    }
    fn label(&self) -> &'static str {
        match self.mode { PickerMode::Save => if self.overwrite.is_some() { "Replace" } else { "Save" },
            PickerMode::Directory => "Select folder", _ => "Open" }
    }
}
/// Bounded wildcard matcher: *, ?; case-insensitive filenames, no shell execution.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let p = pattern.to_lowercase().chars().collect::<Vec<_>>();
    let n = name.to_lowercase().chars().collect::<Vec<_>>();
    let (mut i, mut j, mut star, mut retry) = (0, 0, None, 0);
    while j < n.len() {
        if i < p.len() && (p[i] == '?' || p[i] == n[j]) { i += 1; j += 1; }
        else if i < p.len() && p[i] == '*' { star = Some(i); i += 1; retry = j; }
        else if let Some(s) = star { retry += 1; j = retry; i = s + 1; }
        else { return false; }
    }
    while i < p.len() && p[i] == '*' { i += 1; }
    i == p.len()
}

#[derive(Clone, Copy)]
pub(super) enum EditKind { Location, NewFolder, Rename, Trash }
pub(super) struct EditPrompt { kind: EditKind, text: String, replace: bool, source: Option<PathBuf>, directory: PathBuf }

impl App {
    pub(super) fn begin_picker(&mut self, picker: Picker) -> AnyResult<()> {
        let path = std::fs::canonicalize(&picker.initial).unwrap_or_else(|_| home_dir());
        let path = if path.is_dir() { path } else { path.parent().unwrap_or(Path::new("/")).to_path_buf() };
        self.conn.change_property8(PropMode::REPLACE, self.window, AtomEnum::WM_NAME,
            AtomEnum::STRING, picker.title.as_bytes())?;
        let title_atom = self.conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
        self.conn.change_property8(PropMode::REPLACE, self.window, title_atom,
            self.utf8_string_atom, picker.title.as_bytes())?;
        self.picker = Some(picker);
        self.terminal_visible = false;
        self.focus = Focus::Files;
        self.edit_prompt = None;
        self.show_hidden = false;
        self.navigate(&path);
        self.conn.map_window(self.window)?;
        self.conn.configure_window(self.window, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
        self.conn.flush()?;
        Ok(())
    }
    fn write_picker_result(&self, requester: Window, fields: &[String]) -> AnyResult<()> {
        let atom = self.conn.intern_atom(false, b"_AURORA_FILE_PICKER_RESULT")?.reply()?.atom;
        // TinyX's setup limit includes the ChangeProperty request header. Keep
        // a generous header allowance and stay below Firefox's read limit.
        let limit = (usize::from(self.conn.setup().maximum_request_length) * 4)
            .saturating_sub(64).min(256 * 1024 - 64);
        let bytes = encode_picker_result(fields, limit);
        self.conn.change_property8(PropMode::REPLACE, requester, atom, self.utf8_string_atom, &bytes)?.check()?;
        self.conn.flush()?;
        Ok(())
    }
    pub(super) fn receive_picker(&mut self, requester: Window, parent: Window) {
        let result = (|| -> AnyResult<()> {
            if requester == 0 || requester == self.root || requester == self.window { return Err("invalid requester".into()); }
            if self.picker.is_some() { return Err("file picker is busy".into()); }
            let atom = self.conn.intern_atom(false, b"_AURORA_FILE_PICKER_REQUEST")?.reply()?.atom;
            let prop = self.conn.get_property(false, requester, atom, self.utf8_string_atom, 0, 16385)?.reply()?;
            if prop.bytes_after != 0 || prop.format != 8 { return Err("invalid picker request".into()); }
            let picker = Picker::from_wire(&prop.value, requester)?;
            self.conn.change_window_attributes(requester, &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY))?.check()?;
            let atom = self.conn.intern_atom(false, b"WM_TRANSIENT_FOR")?.reply()?.atom;
            if parent != 0 {
                self.conn.change_property32(PropMode::REPLACE, self.window, atom, AtomEnum::WINDOW, &[parent])?;
            } else { self.conn.delete_property(self.window, atom)?; }
            self.begin_picker(picker)
        })();
        if let Err(err) = result { let _ = self.write_picker_result(requester, &["error".into(), err.to_string()]); }
    }
    pub(super) fn finish_picker(&mut self, paths: Option<Vec<PathBuf>>) {
        let Some(picker) = self.picker.take() else { return; };
        if let Some(requester) = picker.requester {
            let mut fields = vec![if paths.is_some() { "accept".into() } else { "cancel".into() }];
            if let Some(paths) = &paths { fields.extend(paths.iter().map(|p| p.to_string_lossy().into_owned())); }
            let _ = self.write_picker_result(requester, &fields);
        } else if let Some(paths) = &paths {
            let mut stdout = std::io::stdout().lock();
            for path in paths {
                use std::os::unix::ffi::OsStrExt;
                let _ = stdout.write_all(path.as_os_str().as_bytes());
                let _ = stdout.write_all(if picker.null_output { b"\0" } else { b"\n" });
            }
            let _ = stdout.flush();
        } else if !self.picker_service { std::process::exit(1); }
        let _ = self.conn.unmap_window(self.window);
        let _ = self.conn.flush();
        self.edit_prompt = None;
        self.picker_done = !self.picker_service;
    }
    pub(super) fn accept_picker(&mut self) {
        let Some(picker) = self.picker.as_ref() else { return; };
        let selected = self.selected.and_then(|i| self.entries.get(i)).map(|e| e.path.clone());
        let paths = match picker.mode {
            PickerMode::Directory => vec![selected.filter(|p| p.is_dir()).unwrap_or_else(|| self.cwd.clone())],
            PickerMode::Save => {
                let name = picker.name.trim();
                if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
                    self.status = "Enter a filename without slashes".into(); return;
                }
                let target = self.cwd.join(name);
                if target.is_dir() { self.status = "That name belongs to a folder".into(); return; }
                if std::fs::symlink_metadata(&target).is_ok() && picker.overwrite.as_ref() != Some(&target) {
                    self.status = "File exists. Click Replace again to confirm.".into();
                    self.picker.as_mut().unwrap().overwrite = Some(target); return;
                }
                vec![target]
            }
            PickerMode::Multiple => {
                let mut paths = picker.selected.iter().filter(|p| p.is_file()).cloned().collect::<Vec<_>>();
                paths.sort(); paths
            }
            PickerMode::Open => selected.filter(|p| p.is_file()).into_iter().collect(),
        };
        if paths.is_empty() { self.status = "Select a file first".into(); return; }
        if picker.mode != PickerMode::Save && paths.iter().any(|p| !p.exists()) {
            self.status = "Selection no longer exists; refresh and try again".into(); return;
        }
        let paths = paths.into_iter().map(|p| std::fs::canonicalize(&p).unwrap_or(p)).collect();
        self.finish_picker(Some(paths));
    }
    pub(super) fn picker_click(&mut self, x: i32, y: i32, button: u8, state: u16) -> bool {
        let footer = i32::from(self.height) - PICKER_FOOTER_H;
        if y >= footer {
            if button == 1 {
                if y >= footer + 54 {
                    if x >= i32::from(self.width) - 132 { self.accept_picker(); }
                    else if x >= i32::from(self.width) - 246 { self.finish_picker(None); }
                    else { self.start_edit(EditKind::NewFolder); }
                } else if y >= footer + 30 {
                    let p = self.picker.as_mut().unwrap(); p.all_files = !p.all_files;
                    self.picker.as_mut().unwrap().selected.clear();
                    self.refresh_entries(); self.selected = None; self.scroll = 0;
                } else if self.picker.as_ref().is_some_and(|p| p.mode == PickerMode::Save) {
                    self.picker.as_mut().unwrap().editing_name = true;
                }
            }
            return true;
        }
        if self.sort_open { return false; }
        if y < HEADER_H {
            if button != 1 { return true; }
            if y >= 49 { self.start_edit(EditKind::Location); return true; }
            if (56..=86).contains(&x) { if let Some(path) = self.cwd.parent().map(Path::to_path_buf) { self.navigate(&path); } return true; }
            if (132..=162).contains(&x) { self.show_hidden = !self.show_hidden; self.refresh_entries(); return true; }
            if x >= i32::from(self.width) - 50 { self.start_edit(EditKind::NewFolder); return true; }
            return false;
        }
        if x < SIDEBAR_W || button == 4 || button == 5 { return false; }
        if self.sort_open { return false; }
        if button != 1 { return true; }
        let row = (y - HEADER_H - 4) / ROW_H;
        let idx = self.scroll + row.max(0) as usize;
        let Some(entry) = self.entries.get(idx).cloned() else { return true; };
        let double = self.last_click.is_some_and(|(last, t)| last == idx && t.elapsed() < Duration::from_millis(450));
        let ctrl = state & u16::from(KeyButMask::CONTROL) != 0;
        let shift = state & u16::from(KeyButMask::SHIFT) != 0;
        let previous = self.selected;
        self.selected = Some(idx);
        self.last_click = Some((idx, Instant::now()));
        let picker = self.picker.as_mut().unwrap();
        picker.editing_name = false;
        picker.overwrite = None;
        if picker.mode == PickerMode::Multiple {
            if shift {
                let anchor = previous.unwrap_or(idx);
                for e in &self.entries[anchor.min(idx)..=anchor.max(idx)] {
                    if e.path.is_file() { picker.selected.insert(e.path.clone()); }
                }
            } else if ctrl {
                if !picker.selected.remove(&entry.path) && entry.path.is_file() { picker.selected.insert(entry.path.clone()); }
            } else {
                picker.selected.clear();
                if entry.path.is_file() { picker.selected.insert(entry.path.clone()); }
            }
        }
        if picker.mode == PickerMode::Save && entry.path.is_file() { picker.name = entry.name.clone(); }
        self.status = if picker.mode == PickerMode::Multiple { format!("{} files selected — Ctrl/Shift click to add", picker.selected.len()) } else { entry.name.clone() };
        if double && !ctrl && !shift {
            if entry.kind == FileKind::Directory { self.navigate(&entry.path); } else { self.accept_picker(); }
        }
        true
    }
    pub(super) fn picker_key(&mut self, key: u32, ctrl: bool, shift: bool) {
        if key == 0xff1b { self.finish_picker(None); return; }
        if ctrl && matches!(key, 0x6c | 0x4c) { self.start_edit(EditKind::Location); return; }
        if ctrl && shift && matches!(key, 0x6e | 0x4e) { self.start_edit(EditKind::NewFolder); return; }
        if ctrl && matches!(key, 0x68 | 0x48) { self.show_hidden = !self.show_hidden; self.refresh_entries(); return; }
        if key == 0xffc2 { self.refresh_entries(); return; }
        if key == 0xff09 { if let Some(p) = &mut self.picker { p.editing_name = !p.editing_name; } return; }
        if self.picker.as_ref().is_some_and(|p| p.editing_name && p.mode == PickerMode::Save) {
            let p = self.picker.as_mut().unwrap();
            if ctrl && matches!(key, 0x61 | 0x41) { p.name.clear(); }
            else if key == 0xff08 { p.name.pop(); }
            else if key == 0xff0d { self.accept_picker(); return; }
            else if !ctrl { if let Some(c) = keysym_char(key) { p.name.push(c); } }
            p.overwrite = None; return;
        }
        if ctrl && matches!(key, 0x61 | 0x41) {
            if let Some(p) = &mut self.picker { if p.mode == PickerMode::Multiple { p.selected = self.entries.iter().filter(|e| e.path.is_file()).map(|e| e.path.clone()).collect(); } }
            return;
        }
        match key {
            0xff08 => if let Some(path) = self.cwd.parent().map(Path::to_path_buf) { self.navigate(&path); },
            0xff0d => if ctrl { self.accept_picker(); } else if let Some(i) = self.selected { self.open_entry(i); } else { self.accept_picker(); },
            0xff52 | 0xff54 | 0xff50 | 0xff57 | 0xff55 | 0xff56 => {
                if self.entries.is_empty() { return; }
                let old = self.selected.unwrap_or(0);
                let idx = match key { 0xff52 => old.saturating_sub(1), 0xff54 => old + 1, 0xff50 => 0,
                    0xff57 => self.entries.len()-1, 0xff55 => old.saturating_sub(self.visible_rows()), _ => old + self.visible_rows() }.min(self.entries.len()-1);
                self.selected = Some(idx);
                self.scroll = self.scroll.min(idx);
                if idx >= self.scroll + self.visible_rows().max(1) { self.scroll = idx + 1 - self.visible_rows().max(1); }
                if let Some(p) = &mut self.picker { if p.mode == PickerMode::Multiple && !ctrl {
                    if !shift { p.selected.clear(); }
                    for entry in &self.entries[if shift { old.min(idx) } else { idx }..=if shift { old.max(idx) } else { idx }] {
                        if entry.path.is_file() { p.selected.insert(entry.path.clone()); }
                    }
                } }
            }
            0x20 => if let Some(path) = self.selected.and_then(|i| self.entries.get(i)).map(|e| e.path.clone()) {
                if let Some(p) = &mut self.picker { if p.mode == PickerMode::Multiple && !p.selected.remove(&path) && path.is_file() { p.selected.insert(path); } }
            },
            _ => {}
        }
    }
    pub(super) fn draw_picker_footer(&self, c: &mut Canvas) {
        let Some(p) = &self.picker else { return; };
        let w = i32::from(self.width);
        let y = i32::from(self.height) - PICKER_FOOTER_H;
        c.draw_rect(0, y, w, PICKER_FOOTER_H, Color::rgb(236, 245, 250));
        let text = if p.mode == PickerMode::Save { format!("Name: {}{}", p.name, if p.editing_name { "|" } else { "" }) }
            else if !self.status.is_empty() { self.status.clone() }
            else { format!("{}  {}", p.title, if p.mode == PickerMode::Multiple { "(Ctrl/Shift click)" } else { "" }) };
        c.draw_text(&self.regular, &compact(&text, ((w-32)/7) as usize), 16, y+10, 13.0, INK);
        let filter = if p.all_files || p.filter.is_empty() { "All files (click to switch filter)" } else { &p.filter };
        let detail = if p.mode == PickerMode::Save && !self.status.is_empty() { &self.status } else { filter };
        c.draw_text(&self.regular, &compact(detail, ((w-32)/6) as usize), 16, y+33, 11.0, MUTED);
        for (x, width, label) in [(16, 116, "New folder"), (w-246, 104, "Cancel"), (w-132, 116, p.label())] {
            c.draw_round_rect(x, y+56, width, 32, 8, if x == w-132 { Color::rgb(164, 228, 214) } else { CARD });
            c.draw_text(&self.bold, label, x+10, y+64, 12.0, INK);
        }
    }
    pub(super) fn start_edit(&mut self, kind: EditKind) {
        let text = match kind { EditKind::Location => self.cwd.to_string_lossy().into_owned(),
            EditKind::NewFolder => String::new(), EditKind::Rename | EditKind::Trash => {
                let Some(e) = self.selected.and_then(|i| self.entries.get(i)) else { self.status = "Select an item to rename".into(); return; };
                if e.path.parent() != Some(self.cwd.as_path()) { return; } e.name.clone()
            } };
        self.edit_prompt = Some(EditPrompt { kind, text, replace: true, source: self.selected.and_then(|i| self.entries.get(i)).map(|e| e.path.clone()), directory: self.cwd.clone() });
    }
    pub(super) fn edit_prompt_key(&mut self, key: u32, ctrl: bool) {
        if key == 0xff1b { self.edit_prompt = None; return; }
        if key == 0xff0d { self.apply_edit_prompt(); return; }
        let Some(prompt) = &mut self.edit_prompt else { return; };
        if matches!(prompt.kind, EditKind::Trash) { return; }
        if ctrl && matches!(key, 0x61 | 0x41) { prompt.replace = true; return; }
        if key == 0xff08 { if prompt.replace { prompt.text.clear(); } else { prompt.text.pop(); } prompt.replace = false; }
        else if !ctrl { if let Some(c) = keysym_char(key) { if prompt.replace { prompt.text.clear(); } prompt.replace = false; prompt.text.push(c); } }
    }
    pub(super) fn apply_edit_prompt(&mut self) {
        let Some(prompt) = self.edit_prompt.take() else { return; };
        if matches!(prompt.kind, EditKind::Location) {
            let path = if prompt.text == "~" { home_dir() } else if let Some(p) = prompt.text.strip_prefix("~/") { home_dir().join(p) } else { prompt.directory.join(&prompt.text) };
            self.navigate(&path); return;
        }
        if matches!(prompt.kind, EditKind::Trash) {
            let Some(source) = prompt.source else { return; };
            match move_to_trash(&source) {
                Ok(()) => { self.refresh_entries(); self.selected = None; self.places = places(); self.status = "Moved to Trash — files can be recovered from the Trash folder".into(); }
                Err(err) => self.status = format!("Could not move to Trash: {err}"),
            }
            return;
        }
        if prompt.text.is_empty() || prompt.text == "." || prompt.text == ".." || prompt.text.contains('/') || prompt.text.contains('\0') {
            self.status = "Enter a name without slashes".into(); return;
        }
        let dst = prompt.directory.join(&prompt.text);
        if std::fs::symlink_metadata(&dst).is_ok() { self.status = "An item with that name already exists".into(); return; }
        let result = match prompt.kind {
            EditKind::NewFolder => std::fs::create_dir(&dst),
            EditKind::Rename => match prompt.source {
                Some(path) => std::fs::rename(&path, &dst), None => return },
            EditKind::Location | EditKind::Trash => unreachable!(),
        };
        match result { Ok(()) => { self.refresh_entries(); self.selected = self.entries.iter().position(|e| e.path == dst); self.status = "Done".into(); },
            Err(err) => self.status = format!("Could not change folder: {err}") }
    }
    pub(super) fn draw_edit_prompt(&self, c: &mut Canvas) {
        let Some(prompt) = &self.edit_prompt else { return; };
        let w = i32::from(self.width);
        let y = i32::from(self.height) / 2 - 48;
        c.draw_round_rect(12, y, w-24, 138, 12, Color::rgb(226, 242, 244));
        let label = match prompt.kind { EditKind::Location => "Go to folder", EditKind::NewFolder => "New folder", EditKind::Rename => "Rename", EditKind::Trash => "Move to Trash? (recoverable)" };
        c.draw_text(&self.bold, label, 28, y+12, 15.0, INK);
        c.draw_round_rect(24, y+40, w-48, 32, 5, CARD);
        c.draw_text(&self.regular, &compact(&format!("{}|", prompt.text), ((w-64)/7) as usize), 30, y+48, 13.0, INK);
        c.draw_round_rect(24, y+90, w/2-34, 36, 6, CARD);
        c.draw_round_rect(w/2+10, y+90, w/2-34, 36, 6, Color::rgb(164, 228, 214));
        c.draw_text(&self.bold, "Cancel (Esc)", 28, y+98, 13.0, MUTED);
        c.draw_text(&self.bold, "OK (Enter)", w/2+10, y+98, 13.0, MINT_DARK);
    }
}
fn encode_picker_result(fields: &[String], limit: usize) -> Vec<u8> {
    let size = fields.iter().try_fold(0usize, |size, field| size.checked_add(field.len())?.checked_add(1));
    if size.is_none_or(|size| size > limit) {
        return b"error\0Too many selected paths; select fewer files.\0".to_vec();
    }
    fields.iter().flat_map(|s| s.as_bytes().iter().copied().chain(Some(0))).collect()
}

fn keysym_char(key: u32) -> Option<char> {
    let code = if key & 0xff00_0000 == 0x0100_0000 { key & 0x00ff_ffff } else { key };
    char::from_u32(code).filter(|c| !c.is_control() && (code < 0xff00 || key & 0xff00_0000 == 0x0100_0000))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn result_size_is_bounded_without_partial_selection() {
        let fields = vec!["accept".into(), "/one".into()];
        assert_eq!(encode_picker_result(&fields, 12), b"accept\0/one\0");
        assert!(encode_picker_result(&fields, 11).starts_with(b"error\0"));
        let large = vec!["accept".into(), "x".repeat(262144)];
        assert!(encode_picker_result(&large, 262080).len() < 128);
    }
    #[test] fn glob_patterns() { assert!(glob_matches("*.PNG", "photo.png")); assert!(glob_matches("a?c*", "abc.txt")); assert!(!glob_matches("*.png", "photo.jpg")); assert!(glob_matches("*", "notes")); }
    #[test] fn request_preserves_spaces_and_empty_fields() {
        let p = Picker::from_wire(b"1\0multiple\0/home/admin/My Files\0Upload files\0\0*.png;*.jpg\0", 42).unwrap();
        assert_eq!(p.initial, PathBuf::from("/home/admin/My Files")); assert_eq!(p.filter, "*.png;*.jpg"); assert_eq!(p.requester, Some(42));
    }
    #[test] fn invalid_protocol_is_rejected() { assert!(Picker::from_wire(b"2\0open\0", 42).is_err()); assert!(Picker::from_wire(b"1\0open", 42).is_err()); }
}
