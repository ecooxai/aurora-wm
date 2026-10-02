## Aurora WM v0.5.1

Aurora Files now uses one light-blue folder icon for launching and switching to the running app. Recording uses a light-red video-camera icon with an elapsed timer inside its body.

### Changes

- Light-blue folder icons throughout the dock, merged topbar, Files rows, sidebar, and folder-tabs toolbar. A running Files app reuses the pinned folder icon with a black background instead of adding another task icon.
- Larger Aurora and workspace controls align with the app icons in the topbar.
- All apps scrolling reuses a shared application catalog; background change detection avoids rescanning desktop files during menu interaction.
- A readable recording timer updates once per second inside the light-red camcorder icon. Only its width expands; its height stays the same as the idle icon, without overlapping screenshot or display buttons.
- Recordings stop and save automatically after 30 minutes by default. Hold the recording button for 600 milliseconds to choose 30 minutes, 1 hour, 4 hours, 8 hours, or 24 hours. A brief click starts or stops recording. Duration changes during a recording apply to the next recording.
- The chosen limit is frozen for each recording, survives GPU-to-software fallback, and is enforced by both the WM and FFmpeg.
- Double-clicking a video or audio file opens a separate 500 × 500 player beside Aurora Files while the file list and terminal remain available. Video aspect ratio is preserved inside the square. Smaller screens use a smaller square that fits. mpv is preferred, with ffplay fallback; audio uses player controls/cover art or a visible waveform. Failed playback shows a notice in Files.
- Retains native-resolution 24 FPS recording at a 3 Mbps video target, system and microphone audio, GPU preference, and the missing-FFmpeg notice.

### Installation

Both Linux x86_64 and AArch64 downloads contain aurora-wm, aurora-files, install.sh, desktop entries, and picker protocol documentation. Check the accompanying SHA-256 file before installation.

```sh
tar -xzf aurora-wm-v0.5.1-linux-x86_64.tar.gz
cd v0.5.1-linux-x86_64
RESTART_DISPLAY=:0 ./install.sh
```

The default restart display is :11. NO_RESTART=1 installs without replacing a WM. X servers and application clients are preserved. Existing Aurora Files processes retain their old code until reopened.

Recording requires FFmpeg, ffprobe, pactl, and a PulseAudio-compatible service with an output monitor and microphone. Video playback requires mpv or ffplay.

### Validation

- All 30 automated tests passed. Native x86_64 and AArch64 release builds succeeded; ARM was cross-built, without a physical ARM GUI test.
- GUI checks on display :11 verified light-red recording controls with constant 16px height, the timer and duration menu, pinned Files focus across workspaces, merged and bottom dock layouts, All apps scrolling, and separate 500 × 500 audio/video players that preserve the file list.
- Two full 120-second checks sampled WM CPU and resident memory plus swap once per second. Every sample stayed below 5% of one CPU core and 50 MB: display :11 peaked at 4.000086% CPU and 14,467,072 bytes while recording and scrolling All apps; the installed :0 session peaked at 2.000002% CPU and 20,881,408 bytes. Raw samples are in PERFORMANCE_v0.5.1.csv. These are measured results for the tested workloads.
- VAAPI recording preserved the 1280 × 800 screen resolution at 24 FPS, with a measured 2,993,122 bps video rate and AAC audio mixing the output monitor and microphone. During a separate 120-second encoder measurement, FFmpeg averaged 15.34% of one core (1.92% across this machine’s eight logical CPUs), peaking at 19.00% of one core. Encoder samples are in ENCODER_PERFORMANCE_v0.5.1.csv.
- Installed both native binaries and replaced only the WM on :0, preserving all 12 connected client processes, eight application windows, and both X servers.
- System touchpad settings persist half-speed scrolling (ScrollPixelDistance 30, previously 15) and faster pointer movement (AccelSpeed 0.25).
