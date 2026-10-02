# AuroraWM vendor snapshot

This directory is a source-only snapshot copied from `/home/a/project/aurorawm`.

- Source revision: `86a97cd910b285e30fc4b2d12497c9288eede4de`
- Source checkout status: clean at copy time
- Copied paths: `Cargo.toml`, `Cargo.lock`, and `src/`
- Source files copied: 35 files under `src/`

The source checkout has no `patches/`, README, or license file to copy. Generated
and release material was intentionally omitted, including `.git/`, `target/`,
`dist/`, binaries, screenshots, fonts, wallpapers, and install artifacts.

## Local guest Files changes (2026-09-26)

The standalone app under `src/bin/aurora-files/` now includes file-picker CLI
modes and an X11 picker service, location/new-folder/rename prompts, recursive
copy/cut/paste, and recoverable Trash. See `PICKER_PROTOCOL.md` for its protocol
and controls. The WM source and deployed WM binary were not replaced.

The Wasm-only PTY path follows `woiceatus/aurora-wm-wasm` revision
`c2cd4a69d565cdb14d9f4b33d6cb6fdc419f6398`: openpty + posix_spawnp and `/bin/sh`,
while native builds retain forkpty. `../../tools/build-aurora-files-guest.sh`
compiles the app for `wasm32-unknown-linux-musl` in a scratch overlay, verifies
its Linux imports/entrypoint, and leaves rootfs deployment to the explicit
archive overlay tool. It uses external Noto font files because this vendor
snapshot still omits generated assets/fonts. The build script documents
its toolchain paths and environment overrides.
