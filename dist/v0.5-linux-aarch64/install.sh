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

# Match both the process name and its X server; :0 and :0.0 are the same
# server, while :11 must never be stopped when restarting :0.
wm_processes_for_display() {
    as_root sh -s -- "$BIN_NAME" "$RESTART_DISPLAY" "$1" <<'SELECT_WM'
set -eu
wm_name="$1"
target_display="$(printf '%s\n' "$2" | sed 's/\.[0-9][0-9]*$//')"
action="$3"
for proc_dir in /proc/[0-9]*; do
    [ -r "$proc_dir/comm" ] && [ -r "$proc_dir/environ" ] || continue
    IFS= read -r proc_name < "$proc_dir/comm" 2>/dev/null || continue
    [ "$proc_name" = "$wm_name" ] || continue
    proc_display="$(tr '\000' '\n' < "$proc_dir/environ" 2>/dev/null |
        sed -n 's/^DISPLAY=//p' | sed 's/\.[0-9][0-9]*$//')"
    [ "$proc_display" = "$target_display" ] || continue
    proc_pid="${proc_dir##*/}"
    case "$action" in
        list) printf '%s\n' "$proc_pid" ;;
        stop) kill -TERM "$proc_pid" 2>/dev/null || true ;;
        *) exit 1 ;;
    esac
done
SELECT_WM
}

if [ "$(id -u)" -ne 0 ]; then
    need_cmd sudo
fi

if [ -x "./$BIN_NAME" ] && [ -x "./aurora-files" ]; then
    wm_source="./$BIN_NAME"
    files_source="./aurora-files"
    echo "Using bundled release binaries."
else
    need_cmd cargo
    echo "Building $BIN_NAME and aurora-files release binaries..."
    cargo build --release --bins
    wm_source="target/release/$BIN_NAME"
    files_source="target/release/aurora-files"
fi

tmp_wrapper="$(mktemp)"
tmp_desktop="$(mktemp)"
trap 'rm -f "$tmp_wrapper" "$tmp_desktop"' EXIT

echo "Installing $BIN_PATH..."
as_root install -Dm755 "$wm_source" "$BIN_PATH"

echo "Installing aurora-files..."
as_root install -Dm755 "$files_source" "$PREFIX/bin/aurora-files"
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

if command -v import >/dev/null 2>&1; then
    DISPLAY="$RESTART_DISPLAY" import -window root "/tmp/aurora-before-install.png" 2>/dev/null || true
fi

if ! DISPLAY="$RESTART_DISPLAY" xdotool getdisplaygeometry >/dev/null 2>&1; then
    echo "No reachable X server on $RESTART_DISPLAY; installed files, but skipped restart."
    exit 0
fi

echo "Restarting Aurora WM on $RESTART_DISPLAY..."
wm_processes_for_display stop
restart_wait=0
while [ -n "$(wm_processes_for_display list)" ]; do
    if [ "$restart_wait" -ge 5 ]; then
        echo "Aurora WM on $RESTART_DISPLAY did not stop; skipped launching a second WM." >&2
        exit 1
    fi
    sleep 1
    restart_wait=$((restart_wait + 1))
done
setsid -f env DISPLAY="$RESTART_DISPLAY" "$SESSION_WRAPPER" >"/tmp/aurora-wm-${RESTART_DISPLAY#:}.log" 2>&1
sleep 2
DISPLAY="$RESTART_DISPLAY" xdotool getdisplaygeometry
DISPLAY="$RESTART_DISPLAY" xdotool keyup super keyup ctrl keyup alt keyup shift mouseup 1 mouseup 2 || true

echo "Installed LightDM session: $XSESSION_FILE"
