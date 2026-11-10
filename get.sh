#!/bin/sh
# mousetouch installer.
#
#   curl -fsSL https://raw.githubusercontent.com/Devace-93/mousetouch/main/get.sh | sh
#
# Picks the newest GitHub release, verifies the checksum, and installs the
# native package where there is one (.deb on apt systems, .rpm on dnf/zypper
# systems) or the static binary plus udev rule and user service elsewhere.
# Needs sudo for the system files. Environment knobs:
#   MOUSETOUCH_VERSION=v0.1.0   install a specific release instead of the latest
#   MOUSETOUCH_FORCE_TARBALL=1  skip the native package and use the tarball
#   MOUSETOUCH_DRY_RUN=1        print what would be done, change nothing
set -eu

REPO="Devace-93/mousetouch"
BASE_URL="${MOUSETOUCH_BASE_URL:-https://github.com/$REPO/releases/download}"
VERSION="${MOUSETOUCH_VERSION:-}"
PREFIX="${PREFIX:-/usr/local}"
DRY="${MOUSETOUCH_DRY_RUN:-}"

say() { printf '>> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
run() { if [ -n "$DRY" ]; then printf '   %s\n' "$*"; else "$@"; fi; }
sudo_run() { if [ "$(id -u)" -eq 0 ]; then run "$@"; else run sudo "$@"; fi; }
have() { command -v "$1" >/dev/null 2>&1; }

have curl || die "curl is required"
have tar || die "tar is required"

case "$(uname -m)" in
    x86_64|amd64) ARCH=x86_64; DEB_ARCH=amd64; RPM_ARCH=x86_64 ;;
    aarch64|arm64) ARCH=aarch64; DEB_ARCH=arm64; RPM_ARCH=aarch64 ;;
    *) die "unsupported architecture: $(uname -m)" ;;
esac
[ "$(uname -s)" = "Linux" ] || die "this installer is for Linux"

if [ -z "$VERSION" ]; then
    # The /releases/latest page redirects to /releases/tag/vX.Y.Z.
    VERSION=$(curl -fsSI "https://github.com/$REPO/releases/latest" | tr -d '\r' \
              | sed -n 's/^[Ll]ocation: .*\/tag\///p' | tail -n1)
    [ -n "$VERSION" ] || die "could not find the latest release (no network, or no release yet)"
fi
VER="${VERSION#v}"
say "mousetouch $VERSION ($ARCH)"

TMP=$(mktemp -d)
trap 'rm -rf "${TMP:?}"' EXIT
fetch() {
    say "downloading $1"
    curl -fsSL --retry 3 -o "$TMP/$1" "$BASE_URL/$VERSION/$1"
}
verify() {
    [ -f "$TMP/SHA256SUMS" ] || fetch SHA256SUMS
    if have sha256sum; then
        (cd "$TMP" && grep " $1\$" SHA256SUMS | sha256sum -c --quiet -) || die "checksum mismatch for $1"
        say "checksum ok"
    else
        say "sha256sum not available, skipping checksum verification"
    fi
}

enable_service() {
    if have systemctl && [ "$(id -u)" -ne 0 ]; then
        say "enabling the user service"
        run systemctl --user daemon-reload
        run systemctl --user enable --now mousetouch.service || true
    else
        say "enable the daemon for your user with: systemctl --user enable --now mousetouch"
    fi
}

if [ -z "${MOUSETOUCH_FORCE_TARBALL:-}" ] && have apt-get && have dpkg; then
    PKG="mousetouch_${VER}-1_${DEB_ARCH}.deb"
    fetch "$PKG"; verify "$PKG"
    say "installing with apt"
    sudo_run apt-get install -y "$TMP/$PKG"
    enable_service
elif [ -z "${MOUSETOUCH_FORCE_TARBALL:-}" ] && have rpm && { have dnf || have zypper; }; then
    PKG="mousetouch-${VER}-1.${RPM_ARCH}.rpm"
    fetch "$PKG"; verify "$PKG"
    if have dnf; then say "installing with dnf"; sudo_run dnf install -y "$TMP/$PKG"
    else say "installing with zypper"; sudo_run zypper --non-interactive install --allow-unsigned-rpm "$TMP/$PKG"; fi
    enable_service
else
    TARBALL="mousetouch-$VERSION-$ARCH-linux.tar.gz"
    fetch "$TARBALL"; verify "$TARBALL"
    tar xzf "$TMP/$TARBALL" -C "$TMP"
    SRC="$TMP/mousetouch-$VERSION-$ARCH-linux"
    say "installing to $PREFIX (sudo)"
    sudo_run install -Dm755 "$SRC/mousetouch" "$PREFIX/bin/mousetouch"
    sudo_run install -Dm755 "$SRC/mousetouch-settings" "$PREFIX/bin/mousetouch-settings"
    sudo_run install -Dm644 "$SRC/io.github.Devace_93.MouseTouch.desktop" "$PREFIX/share/applications/io.github.Devace_93.MouseTouch.desktop"
    sudo_run install -Dm644 "$SRC/io.github.Devace_93.MouseTouch.svg" "$PREFIX/share/icons/hicolor/scalable/apps/io.github.Devace_93.MouseTouch.svg"
    sudo_run install -Dm644 "$SRC/60-mousetouch.rules" /etc/udev/rules.d/60-mousetouch.rules
    sudo_run install -Dm644 "$SRC/mousetouch.conf" /etc/modules-load.d/mousetouch.conf
    sudo_run install -Dm644 "$SRC/mousetouch.service" /etc/systemd/user/mousetouch.service
    sudo_run modprobe uinput || true
    if have udevadm; then
        sudo_run udevadm control --reload-rules
        sudo_run udevadm trigger --subsystem-match=input --subsystem-match=hidraw --subsystem-match=misc
    fi
    enable_service
fi

cat <<MSG

Done. Swipe left/right with two fingers on the Magic Mouse to switch
workspaces; double-tap with two fingers to show them.
Settings app: mousetouch-settings (also in your app menu)
Config:       ~/.config/mousetouch/config.toml (reloaded on save)
Logs:         journalctl --user -fu mousetouch
If the mouse was already paired, re-pair it or run: sudo udevadm trigger
MSG
