# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

MouseTouch is a Linux-only userspace driver (Rust daemon + Python GTK settings app) that turns the Apple Magic Mouse's multitouch surface into desktop gestures. It depends on evdev, uinput, hidraw, systemd and Wayland protocols, so it **does not build or run on Windows/macOS**. If the checkout is on a non-Linux machine, only static edits and `python -m py_compile` are possible locally; rely on CI (ubuntu-24.04) or a Linux VM for build and tests.

## Commands

```sh
cargo build --release                 # binary at target/release/mousetouch
cargo test --locked                   # all tests (unit tests live in each src/*.rs)
cargo test scroll::                   # one module's tests
cargo test <test_fn_name>             # a single test
python3 -m py_compile ui/mousetouch-settings   # the only check the settings app has
sh -n get.sh && bash -n install.sh uninstall.sh purge.sh   # installer syntax (CI runs these)

./target/release/mousetouch -v run    # daemon in the foreground with debug logs
./ui/mousetouch-settings              # settings app (never with sudo)
```

Some tests create virtual devices, so `/dev/uinput` must be writable (`sudo modprobe uinput && sudo chmod 666 /dev/uinput`, as CI does).

There is no clippy/rustfmt step in CI. The code uses long lines (well past 100 columns); do not reformat files wholesale.

Without hardware: `mousetouch fake-mouse all` creates a fake Magic Mouse over uinput and plays a swipe, pinch and double tap through a running daemon (`MOUSETOUCH_FAKE_NAME` renames it so only a test config's `device_match` picks it up). `tools/magicmouse-emu.py` is a stdlib-only equivalent that also emulates scrolling and custom names/ids. Neither has a hidraw sibling, so they exercise only the evdev fallback path. `tools/VM-TESTING.md` is the per-distro manual checklist.

Useful diagnostics subcommands: `probe`, `watch`, `events`, `desktop`, `print-config`, `demo-swipe`, `key` (see `USAGE` in `src/main.rs`).

## Architecture

### Daemon data flow (`src/`)

`main.rs::run` is a 1 Hz supervisor loop: it hot-reloads the config when the file's mtime changes, rescans `/dev/input` (`discover.rs`), spawns one thread per newly found mouse (`serve_device`), and exits with code 75 when the binary on disk was replaced so the service manager starts the new version (`BinaryWatch`, only under systemd).

Each `serve_device` thread:

1. Creates a per-mouse `VirtualMouse`, then `EVIOCGRAB`s the real evdev node. Pointer motion, buttons and wheel are re-emitted through the virtual mouse; wheel events pass through `scroll.rs` (direction debounce, single hi-res counter, acceleration, momentum).
2. Gets finger positions from **hidraw** (`hidraw.rs`, raw HID report decoding following `hid-magicmouse.c`) when the node can be opened, otherwise from the driver's evdev multitouch events. `Engine::set_hidraw_mode(true)` makes the engine ignore evdev ABS events. The hidraw path exists because the common DKMS driver build reports every finger in slot 0.
3. `poll`s both fds with a timeout from `Engine::next_deadline()` and calls `Engine::tick()` every iteration; timers (tap windows, momentum) are driven this way, not by separate threads.

`engine.rs` holds all gesture recognition for one mouse. Its outputs:

- **Workspace swipe / pinch**: replayed as synthetic contacts on the single shared `VirtualTouchpad` (`virt.rs`), so the compositor runs its own finger-following animation. The number of synthetic fingers (3 on GNOME, 4 on COSMIC/KDE) and the swipe axis come from desktop detection.
- **Taps and discrete swipes**: matched against `[[gesture]]` rules and fired as key combos (shared `VirtualKeyboard`), virtual mouse buttons, or shell commands. Rules with `app = "..."` are scoped by the focused app id and win over the workspace swipe.

State shared across device threads: `Arc<RwLock<Config>>`, `engine::Shared` (touchpad + keyboard + toggle states, behind a `Mutex`), the desktop settings handle, the focus handle, and the `Monitor`.

Supporting modules, each on its own thread:

- `desktop_settings.rs` reads and watches the desktop's own settings (GNOME/Pantheon dconf, COSMIC RON files, KDE INI) for swipe finger count, natural scrolling, overview/app-library shortcuts and workspace axis. Adding support for a desktop mostly means extending this file.
- `focus.rs` tracks the focused app id over Wayland (`zwlr_foreign_toplevel_manager_v1`, or `ext_foreign_toplevel_list_v1` + `zcosmic_toplevel_info_v1`). GNOME exposes neither, so `app` rules never match there.
- `monitor.rs` republishes what the daemon sees as newline-delimited JSON on `$XDG_RUNTIME_DIR/mousetouch.sock`. Each event carries a `dev` number per mouse. This stream is the contract with the settings app's live view; changing `monitor::Event` means updating `ui/mousetouch-settings` too.
- `bluetooth.rs` wraps `bluetoothctl` for `bt-check` / `bt-repair` / `bt-scan` / `bt-pair`; the settings app's *Nearby mice* section shells out to the last two.
- `desktop.rs` writes a user `.desktop` entry for installs outside `XDG_DATA_DIRS` (Homebrew).

### Config (`config.rs`)

TOML at `~/.config/mousetouch/config.toml`, written by both the daemon (`load_or_create`) and the settings app, hot-reloaded by the daemon. `CONFIG_VERSION` + `migrate()` handle files from older releases (a missing `version` key reads as 0). When adding or changing a config field, update: the serde model and its defaults, `migrate()` if old files need different handling, the settings app (which reads/writes the same file independently in Python), and the TOML reference in `README.md`.

### Settings app (`ui/mousetouch-settings`)

One ~4000-line extensionless Python file (GTK4 + libadwaita), installed as-is to `/usr/bin`. It talks to the daemon only through: the config file, the monitor socket, and subprocess calls to the `mousetouch` binary (`desktop`, `--version`, `bt-scan`, `bt-pair`) and `systemctl --user`. UI-only preferences (theme, language) go to `ui.toml`, which the daemon never reads.

Translations are inline: `CATALOG` maps base language -> {English source string -> translation}, looked up by `_()`. The locale list (`LANGS`, 34 locales) is deliberately the same as `site/i18n.js`; user-visible strings added to the app or the site need entries in the respective catalogs.

### Site (`site/`)

Static landing page for <https://mouse.3m4.net>, deployed by the `Pages` workflow on pushes to `main` that touch `site/`. Install snippets in `site/index.html` and `README.md` carry concrete release versions that `scripts/set-release-version.py` rewrites; don't hand-edit those version numbers.

## Packaging and releases

Read `packaging/RELEASING.md` before touching anything release-related. Key points:

- A release is a `vX.Y.Z` tag that must equal `version` in `Cargo.toml`; the `Release` workflow builds static musl binaries (x86_64, aarch64), tarballs, `.deb` (`cargo deb`), `.rpm` (`cargo generate-rpm`), the Homebrew formula, and then opens and merges a PR that bumps the versions in README/site.
- The list of installed files is duplicated across channels: `[package.metadata.deb]` and `[package.metadata.generate-rpm]` in `Cargo.toml`, the tarball staging step in `release.yml`, `get.sh`, `install.sh`, `uninstall.sh`, `purge.sh`, `packaging/aur/PKGBUILD`, `packaging/rpm/mousetouch.spec`, `packaging/homebrew/*`, and `flake.nix`. Adding or renaming an installed file (udev rule, unit, desktop file, icon) means updating all of them.
- CI skips runs for changes limited to `README.md`, `site/**`, `packaging/RELEASING.md` and `tools/*.md`.

## Workflow conventions

Changes land on `main` through pull requests from topic branches (see `git log`); commit subjects for non-code areas are prefixed with the area (`Site: ...`, `README: ...`). Demo recordings attached to PRs are kept under `docs/pr/`.
