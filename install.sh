#!/usr/bin/env bash
# Install mousetouch system-wide (binary, udev rule, settings UI)
# and enable the per-user background service.
set -euo pipefail
cd "$(dirname "$0")"

PREFIX=${PREFIX:-/usr/local}
BIN=target/release/mousetouch

need() { command -v "$1" >/dev/null 2>&1 || { echo "missing: $1" >&2; exit 1; }; }

if [[ ! -x $BIN ]]; then
  need cargo
  echo ">> building release binary"
  cargo build --release
fi

echo ">> installing to $PREFIX (sudo)"
sudo install -Dm755 "$BIN" "$PREFIX/bin/mousetouch"
sudo install -Dm755 ui/mousetouch-settings "$PREFIX/bin/mousetouch-settings"
sudo install -Dm644 packaging/io.github.Devace_93.MouseTouch.desktop "$PREFIX/share/applications/io.github.Devace_93.MouseTouch.desktop"
sudo install -Dm644 packaging/io.github.Devace_93.MouseTouch.svg "$PREFIX/share/icons/hicolor/scalable/apps/io.github.Devace_93.MouseTouch.svg"
sudo install -Dm644 packaging/60-mousetouch.rules /etc/udev/rules.d/60-mousetouch.rules
sudo install -Dm644 packaging/mousetouch.conf /etc/modules-load.d/mousetouch.conf
sudo install -Dm644 packaging/mousetouch.service /etc/systemd/user/mousetouch.service
sudo modprobe uinput || true
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=input --subsystem-match=hidraw --subsystem-match=misc
command -v update-desktop-database >/dev/null && sudo update-desktop-database "$PREFIX/share/applications" || true
command -v gtk-update-icon-cache >/dev/null && sudo gtk-update-icon-cache -q "$PREFIX/share/icons/hicolor" || true

echo ">> enabling user service"
pkill -f '^[^ ]*mousetouch run' 2>/dev/null || true   # stop any manually started instance
systemctl --user daemon-reload
systemctl --user enable --now mousetouch.service
sleep 1
systemctl --user --no-pager --lines=5 status mousetouch.service || true

cat <<MSG

Done. Swipe left/right with two fingers on the Magic Mouse to switch
workspaces; double-tap with two fingers to show them.
Settings UI: mousetouch-settings   (also in your app menu)
Logs:        journalctl --user -fu mousetouch
MSG
