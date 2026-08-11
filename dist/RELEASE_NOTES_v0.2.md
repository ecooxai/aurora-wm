## Aurora WM v0.2

Aurora WM v0.2 focuses on a cleaner desktop shell, faster app navigation, richer file and image workflows, and a dock that stays out of the way of application windows.

### Highlights

- Reworked the dock into a compact 44 px-high strip with tighter icon spacing and much less unused margin.
- Made the dock container transparent between icons using X Shape bounding regions. Windows remain below the dock icons while showing through the gaps and rounded corners.
- Kept dock click targets, task icons, and overflow-menu placement aligned with the new compact geometry.
- Added typo-tolerant app search, expandable app categories, keyboard navigation, and camera/recorder launcher actions.
- Expanded the window title menu with a workspace picker so windows can be moved directly between workspaces.
- Improved Aurora Files with duplicate-tab prevention, a 20-tab cap, and more polished folder-tab behavior.
- Added image-viewer zoom controls, reset zoom, image metadata, a dedicated close control, clipboard copy, and XDND drag-out support to other applications.
- Refined desktop chrome, media/status UI, clipboard behavior, workspace handling, and window-management integration throughout the shell.

### Version and artifacts

- Package version: `0.2.0`
- Git tag: `v0.2`
- `aurora-wm-v0.2-linux-x86_64.tar.gz` targets glibc-based x86_64 Linux systems.
- `aurora-wm-v0.2-linux-aarch64.tar.gz` targets 64-bit ARM Linux systems (AArch64, glibc 2.30 or newer).
- `aurora-wm-v0.2-linux-armv7.tar.gz` targets 32-bit ARMv7 Linux systems using the hard-float ABI (glibc 2.34 or newer).
- Every archive contains stripped release builds of `aurora-wm` and `aurora-files`, plus the prebuilt-aware `install.sh` and Aurora Files desktop-entry assets.
- Use the accompanying `.sha256` file to verify the download.

### Install the binary build

```bash
tar -xzf aurora-wm-v0.2-linux-x86_64.tar.gz
cd v0.2-linux-x86_64
./install.sh
```

The bundled installer detects the prebuilt binaries and installs them without requiring Cargo. It also registers the Aurora WM X session and Aurora Files desktop entries. Set `NO_RESTART=1` if you do not want it to restart the test WM on display `:11`.

### Validation

- `cargo test --all-targets`
- `cargo build --release --bins`
- `cargo zigbuild --release --bins --target aarch64-unknown-linux-gnu`
- `cargo zigbuild --release --bins --target armv7-unknown-linux-gnueabihf`
- Runtime-tested on the project's X server at display `:11` with the compositor disabled.
- ARM ELF headers, dynamic loaders, and hard-float flags were validated after cross-compilation. ARM builds were not runtime-tested on physical ARM hardware.
