## Aurora WM v0.3

Aurora WM v0.3 expands Aurora Files into a richer file, text, image, and terminal workspace while refining clipboard and power controls across the desktop.

### Highlights

- Added compact and maximized side-window previews for images and text files.
- Added image rotation, flipping, cropping, zooming, panning, clipboard copy, context menus, and drag-out support.
- Added an editable text window with cursor navigation, selection, clipboard copy, save controls, and read-only fallback.
- Added automatic folder refresh, newly-created-item ordering, persistent pinned folders, hidden-file controls, and file copy/paste actions.
- Added terminal scrollback with a 50,000-line history cap, selection-aware display, and corrected Ctrl+V / Ctrl+Shift+V behavior.
- Improved desktop-wide Super+V clipboard injection by selecting native terminal paste shortcuts where needed.
- Added a stepped automatic power-saver slider and improved settings interaction handling.
- Refined window focus, title menus, geometry, workspace behavior, drawing, and process integration throughout the shell.

### Version and artifacts

- Package version: `0.3.0`
- Git tag: `v0.3`
- `aurora-wm-v0.3-linux-x86_64.tar.gz` targets 64-bit x86 Linux systems.
- `aurora-wm-v0.3-linux-aarch64.tar.gz` targets 64-bit ARM Linux systems.
- Each archive contains stripped release builds of `aurora-wm` and `aurora-files`, the prebuilt-aware `install.sh`, and Aurora Files desktop-entry assets.
- Use the accompanying `.sha256` file to verify each download.

### Install the binary build

```bash
tar -xzf aurora-wm-v0.3-linux-x86_64.tar.gz
cd v0.3-linux-x86_64
./install.sh
```

The installer detects the bundled binaries and installs them without requiring Cargo. It also registers the Aurora WM X session and Aurora Files desktop entries. Set `NO_RESTART=1` to skip restarting the test WM on display `:11`.

### Validation

- `cargo test --locked` (9 tests passed)
- `cargo build --release --locked --bins`
- `cargo zigbuild --release --locked --target aarch64-unknown-linux-gnu --bins`
- Verified x86_64 and AArch64 ELF headers and archive layouts.
- Runtime validation is performed on the project's X server at display `:11`.
- The AArch64 build was cross-compiled and was not run on physical ARM hardware.
