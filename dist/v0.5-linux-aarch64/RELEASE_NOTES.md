## Aurora WM v0.5

Aurora Files now brings reused text and image viewers into the current workspace when opened. The dock, start menu, and screen recording controls have also been improved.

### Changes

- New windows honor their requested workspace and preserve focus when they open on a background workspace. Sticky windows become visible immediately.
- Replaced the Pictures, Music, and Videos dock shortcuts with one Files launcher. Running apps have a dark background; the focused app has a white background.
- Added a Dock settings tab with a persistent option to merge the dock into the topbar. Files and running apps appear beside the workspace controls, with icons blending into the bar and a dark background only for the active window. The bottom dock is hidden, and the clock moves just before the clipboard button. A narrow screen falls back to the bottom dock when the topbar cannot fit the launchers.
- Clicking the Aurora name or leftmost logo opens the start menu.
- All Apps supports wheel scrolling across the menu and keyboard scrolling without dismissing the menu. Categories begin expanded.
- Added a recording button immediately after the screenshot button. A centered 3, 2, 1 countdown precedes full-screen recording with system audio and microphone audio. Click again to stop and open the saved recording folder in Aurora Files.
- Recordings preserve screen resolution, default to 24 FPS, and use a 3 Mbps video target. Files are saved as MP4 under `~/Desktop/screenrecording` with unique names.
- Recording probes working VAAPI, NVENC, and QSV hardware encoders at native screen dimensions, preferring GPU encoding and falling back to a fast, two-thread software encoder. If FFmpeg is missing, a notice below the recording button asks the user to install it.
- Shared wallpaper buffers and a bounded cache reduce WM memory use.
- Installer restarts target only the requested X display's Aurora WM process, preserving other applications, other displays, and the X server.

### Recording requirements

Install FFmpeg with `libx264`, `x11grab`, and PulseAudio input support, plus `ffprobe` and `pactl`. Recording uses the current output's monitor and the default microphone through PulseAudio or PipeWire's PulseAudio service. A clear notice appears if either audio source is unavailable.

### Downloads and installation

Each Linux x86_64 or AArch64 archive includes `aurora-wm`, `aurora-files`, `install.sh`, desktop entries, and picker protocol documentation. Verify the archive using its accompanying `.sha256` file.

```sh
tar -xzf aurora-wm-v0.5-linux-x86_64.tar.gz
cd v0.5-linux-x86_64
RESTART_DISPLAY=:0 ./install.sh
```

The default restart display is `:11`. Use `NO_RESTART=1` to install without restarting a WM. The installer never restarts the X server or closes application clients.

### Validation

- `cargo test --locked`: 19 tests passed.
- Native release and AArch64 cross-compilation for both binaries.
- GUI regression tests on display `:11` for viewer reuse, dock modes, start-menu scrolling, and recording start/stop.
- Native 1280×800 and odd-size 1279×799 captures verified at 24 FPS with H.264 video and AAC audio. ARM binaries are cross-compiled, without a physical ARM runtime test.
- VAAPI recording with system and microphone audio verified on this system; missing-FFmpeg handling is tested separately.

Performance checks sample the WM once per second for 120 seconds, using CPU percentages relative to one core and a conservative memory total of RSS plus swap. FFmpeg runs as a separate process and is measured separately from the WM.

| Workload | Samples | Peak WM CPU | Peak WM memory | Result |
| --- | ---: | ---: | ---: | --- |
| `:11`, 1280×800, idle | 120 | 1.00% | 14.64 MB | All samples passed |
| `:11`, menus, workspace switches, scrolling, and GPU recording | 120 | 3.01% | 14.77 MB | All samples passed |
| Installed WM on `:0`, 1920×1080, idle | 120 | 1.01% | 19.83 MB | All samples passed |

Every sampled WM interval remained below 5% CPU and 50 MB memory. Raw samples are in [`dist/PERFORMANCE_v0.5.csv`](https://github.com/ecooxai/aurora-wm/blob/v0.5/dist/PERFORMANCE_v0.5.csv). These results cover the tested resolutions and workloads.

During the 120-second recording test, the separate VAAPI FFmpeg process averaged 16.72% of one CPU core (about 2.09% of this eight-thread system's total CPU capacity), with a 27.00% one-core peak. The saved recording was checked for native dimensions, 24 FPS, H.264, and AAC.

Local installation preserved all 18 existing application processes and 22 application windows, both X servers, and the running WM on `:11`. The system touchpad now uses half the previous scroll speed and an acceleration setting of 0.25, persisted for future X sessions.
