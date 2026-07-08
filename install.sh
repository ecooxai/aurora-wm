#!/bin/sh
set -eu

cd "$(dirname "$0")"

BIN_NAME="${BIN_NAME:-aurora-wm}"
PREFIX="${PREFIX:-/usr/local}"
BIN_PATH="${BIN_PATH:-$PREFIX/bin/$BIN_NAME}"
SESSION_WRAPPER="${SESSION_WRAPPER:-/usr/bin/aurora-wm-session}"
XSESSION_FILE="${XSESSION_FILE:-/usr/share/xsessions/aurora-wm.desktop}"
RESTART_DISPLAY="${RESTART_DISPLAY:-:11}"
NO_RESTART="${NO_RESTART:-0}"

as_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo "$@"
    fi
}

need_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "install.sh: missing required command: $1" >&2
        exit 1
    fi
}

need_cmd cargo
if [ "$(id -u)" -ne 0 ]; then
    need_cmd sudo
fi

tmp_wrapper="$(mktemp)"
tmp_desktop="$(mktemp)"
trap 'rm -f "$tmp_wrapper" "$tmp_desktop"' EXIT

echo "Building $BIN_NAME and aurora-files release binaries..."
cargo build --release --bins

echo "Installing $BIN_PATH..."
as_root install -Dm755 "target/release/$BIN_NAME" "$BIN_PATH"

echo "Installing aurora-files..."
as_root install -Dm755 "target/release/aurora-files" "$PREFIX/bin/aurora-files"
as_root install -Dm644 assets/aurora-files.desktop /usr/share/applications/aurora-files.desktop
as_root install -Dm644 assets/aurora-files-terminal.desktop /usr/share/applications/aurora-files-terminal.desktop
if command -v update-desktop-database >/dev/null 2>&1; then
    as_root update-desktop-database /usr/share/applications || true
fi
# Register Aurora Files as the default file manager for the current user.
if command -v xdg-mime >/dev/null 2>&1; then
    xdg-mime default aurora-files.desktop inode/directory || true
fi

cat >"$tmp_wrapper" <<EOF
#!/bin/sh

if test -n "\$1"; then
    echo "Syntax: aurora-wm-session"
    echo
    echo "See the aurora-wm-session(1) manpage for help."
    exit 1
fi

# Clean up after display managers that may leave desktop metadata on root.
xprop -root -remove _NET_NUMBER_OF_DESKTOPS \\
      -remove _NET_DESKTOP_NAMES \\
      -remove _NET_CURRENT_DESKTOP 2>/dev/null

# Set up the environment.
A="/etc/xdg/aurora-wm/environment"
test -r "\$A" && . "\$A"
A="\${XDG_CONFIG_HOME:-"\$HOME/.config"}/aurora-wm/environment"
test -r "\$A" && . "\$A"

exec "$BIN_PATH" "\$@"
EOF

echo "Installing $SESSION_WRAPPER..."
as_root install -Dm755 "$tmp_wrapper" "$SESSION_WRAPPER"

cat >"$tmp_desktop" <<EOF
[Desktop Entry]
Name=Aurora WM
Comment=Log in using the Aurora window manager
Exec=$SESSION_WRAPPER
TryExec=$SESSION_WRAPPER
Icon=window-manager
Type=Application
EOF

echo "Installing $XSESSION_FILE..."
as_root install -Dm644 "$tmp_desktop" "$XSESSION_FILE"

if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$XSESSION_FILE"
fi

if [ "$NO_RESTART" = "1" ]; then
    echo "Skipping restart because NO_RESTART=1."
    exit 0
fi

if ! command -v xdotool >/dev/null 2>&1; then
    echo "xdotool not found; installed files, but skipped restart on $RESTART_DISPLAY."
    exit 0
fi

if ! DISPLAY="$RESTART_DISPLAY" xdotool getdisplaygeometry >/dev/null 2>&1; then
    echo "No reachable X server on $RESTART_DISPLAY; installed files, but skipped restart."
    exit 0
fi

if command -v import >/dev/null 2>&1; then
    DISPLAY="$RESTART_DISPLAY" import -window root "/tmp/aurora-before-install.png" 2>/dev/null || true
fi

echo "Restarting Aurora WM on $RESTART_DISPLAY..."
as_root pkill -x "$BIN_NAME" 2>/dev/null || true
sleep 1
setsid -f env DISPLAY="$RESTART_DISPLAY" "$SESSION_WRAPPER" >"/tmp/aurora-wm-${RESTART_DISPLAY#:}.log" 2>&1
sleep 2
DISPLAY="$RESTART_DISPLAY" xdotool getdisplaygeometry
DISPLAY="$RESTART_DISPLAY" xdotool keyup super keyup ctrl keyup alt keyup shift mouseup 1 mouseup 2 || true

echo "Installed LightDM session: $XSESSION_FILE"
