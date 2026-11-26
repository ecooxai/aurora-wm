## Aurora WM v0.6

Aurora now works well on touch tablets. This release adds HiDPI scaling, an on-screen keyboard, press-and-hold menus, and title bars you can drag from anywhere. Settings are redesigned. A new portal backend lets browsers use Aurora Files for open and save dialogs. A WebAssembly build of Aurora Files is also included.

### Changes

**Touch and windows**
- Title bars: the close, minimise and maximise buttons and the title text act when you release. Press and drag on any of them to move the window instead. Tapping the title opens the window menu, and tapping it again closes it.
- Press and hold a workspace in the topbar for a menu: new workspace, or close the workspace (with confirmation). The separate new-workspace button is gone.
- Press and hold a running app for a menu: force close, restart, or set CPU priority to Low, Normal or High. Each app keeps its priority (stored by WM_CLASS) across app and WM restarts. High needs the `aurora-renice` helper, which install.sh installs with `cap_sys_nice`.
- A floating on-screen keyboard sends keys with XTEST and never takes focus. Open it from the topbar keyboard icon or with `aurora-wm --toggle-keyboard`.

**Display**
- HiDPI: everything is laid out in logical pixels and drawn at full resolution. The 1×, 2× and 3× options restart the WM in place with a matching `GDK_SCALE`. Your windows stay open.
- `aurora-wm --restart-wm` re-executes the WM on the same X server. Your windows are kept and picked up again, even if the binary was replaced while it was running.
- Rotation setting: Auto, Off, 90°, 180° or 270°.
- Screen sleep turns off the backlight, and waking always turns it back on.

**Topbar**
- The clock shows only the time. Tap it to show the full date.
- Clipboard manager redesign:
  - A lighter look.
  - Icon shortcuts that send copy, select all, undo and redo to the focused window.
  - A preview of the current X selection, which is not added to history.
- Press and hold the clipboard icon to paste: Ctrl+Shift+V for text, Ctrl+V when the newest item is an image.

**Settings**
- About is now at the bottom of the sidebar.
- The Dock tab is now Multitask. It has the dock-merge switch plus switches to show or hide each topbar icon: keyboard, clipboard, screenshot, recording, display, audio and Wi-Fi.
- The Network tab shows total download and upload speed with sparklines, and the speed of each interface on its row.
- Session actions (log out, soft reboot, reboot, suspend, shut down) and a full network restart. Each asks for confirmation first and shows the exact command it will run.

**Aurora Files and file dialogs**
- Aurora Files follows `GDK_SCALE` and draws text, icons and images sharply on HiDPI panels.
- New xdg-desktop-portal FileChooser backend (`portal/`). Browsers without GTK or Qt, such as Chromium, use the Aurora Files picker to open files, save files and choose a download folder.

### Downloads

- `aurora-wm-v0.6-linux-x86_64.tar.gz` and `aurora-wm-v0.6-linux-aarch64.tar.gz` contain:
  - aurora-wm, aurora-files and aurora-renice
  - install.sh
  - desktop entries
  - the picker protocol document
  - the `portal/` file-chooser backend
- The AArch64 binaries need glibc 2.30 or newer.
- `aurora-wm-v0.6-wasm32-linux-musl.tar.gz` contains only Aurora Files, built for the `wasm32-unknown-linux-musl` Linux guest ABI. It is a module that imports `linux.syscall` and exports `_start`. The window manager itself needs a native X11 host, so it has no wasm build.

Check each archive against its SHA-256 file before installing.

### Installation

```sh
tar -xzf aurora-wm-v0.6-linux-x86_64.tar.gz
cd v0.6-linux-x86_64
RESTART_DISPLAY=:0 ./install.sh
```

The default restart display is :11. NO_RESTART=1 installs without replacing a running WM.

Optional browser file dialogs, installed manually:
1. Copy `portal/aurora-portal-filechooser` to `/usr/local/libexec/`.
2. Copy `portal/aurora.portal` to `/usr/share/xdg-desktop-portal/portals/`.
3. Copy the `.service` file to `/usr/share/dbus-1/services/`.
4. Copy `portal/portals.conf` to `~/.config/xdg-desktop-portal/`.

### Validation

- All 30 automated tests passed.
- Release builds succeeded:
  - x86_64: native build.
  - AArch64: cross-built with cargo-zigbuild against glibc 2.30.
  - wasm32-unknown-linux-musl: Aurora Files only. The wasm build was checked with `wasm-validate`, and its Linux guest imports and entry point were checked. It was not run in a guest.
- GUI checks on X display :11 covered:
  - Title bars: click versus drag on the title and the buttons, the menu toggle, and drag while the menu is open.
  - Maximise and restore, and close.
  - The Multitask icon switches and the compacting topbar.
  - The Network speeds.
  - The time-only clock and the date toggle.
  - The clipboard menu with its selection preview.
- The AArch64 WM, built from this source, was installed on an iPad running Linux. It was restarted in place with `--restart-wm` while Xorg and all open applications kept running.
