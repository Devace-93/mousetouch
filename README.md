# <img src="https://raw.githubusercontent.com/Devace-93/mousetouch/main/packaging/io.github.Devace_93.MouseTouch.svg" alt="" width="40" align="top"> mousetouch

Native-feeling multitouch gestures for the Apple Magic Mouse on Linux
(Wayland), implemented as a small userspace driver.

Website: <https://mouse.3m4.net>

> MouseTouch is an independent open-source project. It is not affiliated with,
> endorsed by or sponsored by Apple Inc. "Magic Mouse" is a trademark of Apple
> Inc., used here only to describe the hardware the driver works with.

Out of the box:

| Gesture on the mouse       | Result                                                        |
|----------------------------|---------------------------------------------------------------|
| 2-finger swipe left/right  | Switch workspace, animated and following your fingers         |
| 2-finger pinch             | Native pinch zoom in apps that support it (browsers, viewers) |
| 2-finger double tap        | Show the workspaces (`Super+W` on COSMIC, `Super` on GNOME)   |
| 3-finger tap               | Show all applications (`Super+A` app library / app grid)      |
| 1-finger scroll, clicks    | Unchanged, except the backwards jump a scroll often starts with is filtered out |

Extra taps and swipes (3 fingers, vertical swipes, ...) can be mapped to
shortcuts, mouse buttons or commands from the settings app, optionally only
for a given application (e.g. 2-finger swipes = back/forward in the browser).

Coming from a Mac? The settings app's **Nearby mice** section lists the mouse
as soon as it is switched on and pairs it with one click, running the
Bluetooth repair BlueZ needs after a macOS pairing (the desktop's own pairing
usually leaves the mouse *paired but not bonded*, and it never connects).

## How it works

The Magic Mouse reports every finger on its surface in its HID reports, but
desktops ignore that because the device is a *mouse*, not a touchpad. This
daemon:

1. Grabs the mouse's evdev node and re-emits pointer motion, buttons and
   wheel events through a virtual mouse, so everything keeps working. The
   first wheel events of each touch are held until the net movement clearly
   points one way, which removes the short backwards jump the kernel driver
   produces while the fingertip settles on the shell.
2. Decodes the finger positions from the raw HID reports (`/dev/hidrawN`,
   same layout the kernel driver uses). It falls back to the driver's evdev
   multitouch events when hidraw is not accessible; note that the popular DKMS
   build of `hid-magicmouse` (4.18+magicmouse2) reports every finger in slot 0,
   so the hidraw path is what makes gestures reliable there.
3. Tracks the fingers on the surface. When two fingers travel sideways it puts
   **four synthetic fingers** on a virtual touchpad and replays the motion.
   The compositor sees an ordinary 4-finger touchpad swipe and runs its own
   workspace animation: it follows the fingers, pauses when they pause, goes
   back when they go back, and uses the release velocity to settle.
   When two fingers move apart or together it replays **two contacts
   spreading** instead, which libinput reports as a pinch and the compositor
   forwards to the focused app as native pinch zoom.
4. Recognises taps and discrete swipes and fires configurable actions through
   a virtual keyboard / the virtual mouse / a shell command. Rules can be
   scoped to the focused application; the app id comes from the compositor's
   toplevel protocols (wlr-foreign-toplevel-management on KDE/Sway/Hyprland,
   ext-foreign-toplevel-list + cosmic-toplevel-info on COSMIC; GNOME exposes
   neither, so there `app` rules are simply never matched).

Nothing is patched in the kernel or the compositor, so it survives updates
and works on any compositor with libinput touchpad gestures (COSMIC, GNOME,
KDE, niri, ...).

## Install

Requirements: `/dev/uinput` (standard), the `hid-magicmouse` kernel module
(in-tree; the DKMS version also works), systemd. The settings app also needs
`python3-gi` with its cairo bridge, GTK 4 and libadwaita (`python3-gi
python3-gi-cairo gir1.2-gtk-4.0 gir1.2-adw-1` on Debian/Ubuntu/Pop!_OS).

**One line, any distro** (picks the `.deb`/`.rpm` or the static binary, verifies
the checksum, needs sudo):

```sh
curl -fsSL https://raw.githubusercontent.com/Devace-93/mousetouch/main/get.sh | sh
```

**Packages** from the [releases page](https://github.com/Devace-93/mousetouch/releases):

| Distro | Command |
|---|---|
| Debian, Ubuntu, Pop!_OS | `curl -LO https://github.com/Devace-93/mousetouch/releases/download/v0.1.10/mousetouch_0.1.10-1_amd64.deb && sudo apt install ./mousetouch_0.1.10-1_amd64.deb` (latest; `_arm64` for ARM, or any `./mousetouch_<version>_amd64.deb` you downloaded) |
| Fedora, openSUSE | `curl -LO https://github.com/Devace-93/mousetouch/releases/download/v0.1.10/mousetouch-0.1.10-1.x86_64.rpm && sudo dnf install ./mousetouch-0.1.10-1.x86_64.rpm` (latest; `.aarch64` for ARM, or any `./mousetouch-<version>.x86_64.rpm`) |
| Arch | not on the AUR yet (registration was closed when 0.1.7 shipped); build the package from the repo: `cd packaging/aur && makepkg -si` |
| Homebrew on Linux | `brew trust Devace-93/tap` once (recent Homebrew asks this for third-party taps), then `brew install Devace-93/tap/mousetouch` and follow the printed caveats for the udev rule; the daemon adds the settings app to the app menu on its first start. After `brew upgrade` the running daemon notices the new binary and restarts itself (0.1.10 and later; before that, `brew services restart mousetouch`) |
| NixOS / Nix | `nix profile install github:Devace-93/mousetouch`, or import `nixosModules.default` and set `services.mousetouch.enable = true` |
| Rust toolchain | `cargo install mousetouch` (binary only; install the udev rule from `packaging/` by hand) |
| Any, static tarball | `mousetouch-<version>-<arch>-linux.tar.gz` contains the binary, the settings app and the files `get.sh` installs |

The `.deb` and `.rpm` enable the per-user service for everyone and start it
for users logged in at install time; other methods print the
`systemctl --user enable --now mousetouch` step.

**From source:**

```sh
git clone https://github.com/Devace-93/mousetouch
cd mousetouch
./install.sh
```

`install.sh` builds the binary, installs it to `/usr/local/bin`, adds a udev
rule granting the logged-in user access to the mouse's input and hidraw nodes
and to `/dev/uinput`, installs the settings app, and enables a per-user systemd
service. Remove everything with `./uninstall.sh`. To wipe every trace from a
machine before testing a fresh install (any install method, services, menu
entries, config; `--keep-config` keeps your settings, `--dry-run` only lists),
run `./purge.sh`.

If the Magic Mouse was paired before the udev rule was installed, unplug/re-
pair it or run `sudo udevadm trigger` once.

### About DKMS drivers

MouseTouch only needs the `hid-magicmouse` driver that ships inside the
Linux kernel (5.15 or newer; the 2024 USB-C mouse needs 6.16 or newer).
Third-party DKMS builds of that driver (`hid-magicmouse-dkms`,
`magicmouse-hid`, ...) are recompiled on every kernel update and replace the
built-in module. The common `4.18+magicmouse2` build reports every finger in
slot 0 and does not know the USB-C model, so that mouse lands on
`hid-generic` with no gestures at all. If you installed one for an older
kernel, remove it and reconnect the mouse:

```sh
dkms status | grep -i magic                                   # is one installed?
sudo dkms remove hid-magicmouse-dkms/4.18+magicmouse2 --all   # use the name/version printed
sudo depmod -a && sudo modprobe -r hid_magicmouse; sudo modprobe hid_magicmouse
```

## Configure

Run **MouseTouch** from the app menu, or
`mousetouch-settings`. Settings are saved to
`~/.config/mousetouch/config.toml` and the daemon reloads them
instantly.

The app follows the desktop's light/dark theme and language by default. Both
can be overridden in its *Appearance* group (34 locales, the same list as the
3m4.net pages); the choice is stored in `~/.config/mousetouch/ui.toml`, which
the daemon never reads.

- **Direction** — *Natural* (macOS-like, swipe left = next workspace) or
  *Inverted*. The daemon accounts for your desktop's touchpad natural-scroll
  setting automatically.
- **Sensitivity** — finger travel per workspace.
- **Desktop gesture fingers** — 4 for COSMIC/KDE, 3 for GNOME.
- **Pinch zoom** — on/off and zoom aggressiveness.
- **Gestures** — add rows like *Tap · 3 fingers → Click middle* or
  *Swipe · 2 fingers · right · only in firefox → alt+left*.

### Live view

The top of the settings window draws the mouse as the daemon sees it: every
finger with its number, where it landed and the path it took, the workspace
swipe or pinch being replayed (arrow with the travel in mm, or the zoom
factor), scroll notches, clicks, and a badge for each tap, double tap or
swipe with the rule it ran (or *no rule*). Touches that were not counted as
taps say why (*moved too far*, *lifted while moving*, ...), which is the
quickest way to tune the *Tap timing* values.

The window-with-arrow button opens the same view in its own window with an
event log, handy next to the app you are testing (use your desktop's window
menu to keep it on every workspace: COSMIC *Sticky*, GNOME *Always on
Visible Workspace*, KDE *All Desktops*).

With more than one Magic Mouse attached, the drop-down next to the view
picks which one to follow, or *All mice*: then each mouse draws in its own
colours (blue-first and pink-first palettes) and badges and log lines carry
its name. Every event in the stream has a `dev` number for this.

The data comes from the daemon's event stream on
`$XDG_RUNTIME_DIR/mousetouch.sock` (newline-delimited JSON, see
`src/monitor.rs`); `mousetouch events` prints it in a terminal.

### Upgrading

When the daemon runs as a service and the `mousetouch` binary on disk is
replaced (a package upgrade, `brew upgrade`, `install.sh`), it exits so the
service starts the new version; nothing to restart by hand. An open
settings window notices too and shows a banner to reopen it (or to restart
a daemon that is still on the old version when it is not run as a service).

The same options exist in the TOML file:


- **Direction** — *Natural* (macOS-like, swipe left = next workspace) or
  *Inverted*. The daemon accounts for your desktop's touchpad natural-scroll
  setting automatically.
- **Sensitivity** — finger travel per workspace.
- **Desktop gesture fingers** — 4 for COSMIC/KDE, 3 for GNOME.
- **Gestures** — add rows like *Tap · 3 fingers → Click middle* or
  *Swipe · 2 fingers · up → super+w*.

```toml
[workspace_swipe]
enabled = true
fingers = 2
direction = "natural"   # or "inverted"
sensitivity = 0.4
synthetic_fingers = 4   # 3 on GNOME
axis = "horizontal"

[[gesture]]
kind = "double_tap"     # swipe | tap | double_tap
fingers = 2
action = { type = "key", combo = "super+w" }
# gesture = []          # instead of any [[gesture]]: no gestures at all
                        # (a file without the key gets the two defaults)

[pinch]
enabled = true
sensitivity = 1.5       # zoom aggressiveness

# One-finger scrolling. The kernel driver begins scrolling after ~1.3 mm of
# travel, and a fingertip landing on the curved shell rolls about that far
# before the hand moves, so a scroll often started with a jump the wrong way:
# the daemon holds the first wheel events of each touch until the net movement
# is clearly one way (120 units = one notch) and drops the rest. It also emits
# wheel notches and high-resolution steps from one counter, so scrolling feels
# the same on every connection (the driver's two counters disagree, and a flick
# on the first scroll could switch libinput into a different mode for good).
# Fast movement scrolls further (acceleration) and a finger that lifts while
# moving leaves the page coasting (momentum), the way macOS does it. Apple
# publishes neither curve for the Magic Mouse; the decay is their documented
# scroll-view deceleration rate, the acceleration ramp is ours.
[scroll]
direction_debounce = true
debounce_units = 40     # raise if a scroll still starts backwards, lower if it starts late
mode = "smooth"         # "smooth": every 1/10 step; "notches": whole wheel clicks, classic feel
speed = 1.0             # multiplier on scroll distance
acceleration = 0.5      # extra gain at a hard flick (1 m/s): 0 = none, 0.5 = 1.5x, 1 = 2x
momentum = true         # keep scrolling after the finger lifts while moving; touching stops it
momentum_decay = 0.998  # speed kept per millisecond (Apple's rate; 0.99 stops in half a second)

# Optional macOS-style smart zoom (off by default: a one-finger double tap is
# easy to trigger by accident). Each double tap sends the next step, wrapping
# around; browsers step 100 -> 110 -> 125 -> 150 -> 175 -> 200 per Ctrl+KP_Plus.
[[gesture]]
kind = "double_tap"
fingers = 1
action = { type = "cycle", steps = ["ctrl+kpplus,ctrl+kpplus", "ctrl+kpplus,ctrl+kpplus", "ctrl+kpplus", "ctrl+0"] }

# Alternative: a plain toggle that magnifies the whole screen with COSMIC's
# accessibility zoom instead of the app's own zoom.
# action = { type = "toggle", on = "super+equal", off = "super+minus" }

[[gesture]]
kind = "swipe"
fingers = 3
direction = "up"
action = { type = "command", command = "notify-send 'three fingers up'" }

# Per-application: in Firefox a 2-finger swipe goes back/forward instead of
# switching workspaces (app-scoped rules win over the workspace swipe).
[[gesture]]
kind = "swipe"
fingers = 2
direction = "right"
app = "firefox"
action = { type = "key", combo = "alt+left" }

[[gesture]]
kind = "swipe"
fingers = 2
direction = "left"
app = "firefox"
action = { type = "key", combo = "alt+right" }
```

On GNOME set `axis = "both"` under `[workspace_swipe]` (with
`synthetic_fingers = 3`): a horizontal 2-finger swipe switches workspaces and a
vertical one opens the overview, both with GNOME's own animation.

## Troubleshooting

```sh
mousetouch probe          # is the mouse seen, with touch axes?
mousetouch watch          # live finger positions (daemon stopped)
mousetouch events         # the running daemon's event stream as JSON lines
mousetouch demo-swipe left 40   # replay a synthetic 4-finger swipe
mousetouch bt-check       # Bluetooth pairing healthy? (3 = stale after a Mac pairing)
mousetouch bt-repair      # remove the mouse from BlueZ and pair it again
mousetouch bt-scan        # mice switched on and not connected, as JSON lines
mousetouch bt-pair ADDR   # what Settings > Nearby mice runs for one of them
journalctl --user -fu mousetouch
journalctl -u bluetooth   # what bluetoothd thinks of the mouse
```

Known cases, in the order they tend to happen to someone coming from a Mac:

- *The mouse does not show up in the Bluetooth list*: a Magic Mouse only
  advertises while no other computer holds it. Forget it on the Mac (or turn
  the Mac's Bluetooth off), unplug the USB-C cable (over the cable it works
  as a USB mouse and turns Bluetooth off), then switch the mouse off, wait a
  few seconds and switch it on; the LED blinks while it looks for a host.
- *The desktop's Bluetooth panel does not list the mouse at all*, though it is
  switched on: some panels hide devices they cannot classify. **Nearby mice**
  in the settings app talks to BlueZ directly, lists every mouse that is
  advertising and pairs it from there (a second Magic Mouse 2 that COSMIC's
  panel never showed paired fine this way).
- *The mouse pairs but never connects* (typical right after a Mac, even when
  it was forgotten on both sides): BlueZ keeps a record that is *paired* but
  not *bonded* and rejects the mouse (`journalctl -u bluetooth` says
  "Rejected connection from !bonded device"; `bluetoothctl info` shows
  `Paired: yes`, `Bonded: no`). Open the settings app, find the mouse under
  **Nearby mice** and press **Connect**: it removes the stale record, waits
  for the mouse, pairs, trusts and connects. The same from a terminal:
  `mousetouch bt-pair <address>` (addresses from `mousetouch bt-scan`) or
  `mousetouch bt-repair`, which finds the mouse by name; `mousetouch bt-check`
  reports the state (exit 3 = stale).

- *No device found*: the mouse must be connected over **Bluetooth** (the
  cable only charges on Linux) and bound to `hid-magicmouse`. `mousetouch
  probe` names every node that looks like the mouse and cannot be used, with
  the reason: *permission denied* (udev rule missing or not applied yet:
  install it, then re-pair the mouse or `sudo udevadm trigger`) or *no
  multitouch axes* (bound to `hid-generic`, see the DKMS note). The mouse is
  recognised by its vendor/product ids, so renaming it on macOS is fine.
- *Magic Mouse (USB-C, 2024) moves the pointer but has no gestures*: that
  model (product `0323`) is supported by the in-tree driver from Linux 6.16.
  If a DKMS `hid-magicmouse` (e.g. `4.18+magicmouse2`) is installed it shadows
  the in-tree module and does not know the new id, so the kernel binds the
  mouse to `hid-generic` (`journalctl -k | grep -i "magic mouse"` shows
  `hid-generic` instead of `magicmouse`). Remove it: `sudo dkms remove
  hid-magicmouse-dkms/4.18+magicmouse2 --all`, then `sudo depmod -a`,
  `sudo modprobe -r hid_magicmouse; sudo modprobe hid_magicmouse`, and switch
  the mouse off and on.
- *The mouse was renamed on macOS*: fine, the daemon and the udev rule
  recognise it by vendor/product id, whatever it is called.
- *Scrolling is choppy, or too fast in terminals*: since 0.1.6 the default is
  *Smooth steps* with acceleration and momentum (see `[scroll]` above). A
  config written by an older version keeps `mode = "notches"`; switch it in
  Settings → Scrolling. Terminals and a few toolkits only understand whole
  notches and scroll faster in smooth mode: pick *Whole notches* there.
- *Swipe goes the wrong way*: flip **Direction**, or set *Desktop touchpad
  natural scrolling* explicitly.
- *Nothing happens on swipe*: check the desktop handles 4-finger (or
  3-finger) touchpad swipes and that it does not disable touchpads while a
  mouse is connected.
- *Permission denied*: make sure the udev rule is installed (`install.sh`) and
  re-pair the mouse or run `sudo udevadm trigger` so it applies to a mouse
  that was already connected; the daemon logs this once per node.
- *Pinch zooms the wrong amount*: adjust `pinch.sensitivity` (1.0 = touchpad
  parity).
- *`app` rules never fire*: run with `-v` and look for "focus tracking via";
  the compositor must expose one of the toplevel protocols listed above.

## Tested on

Mice and systems MouseTouch has actually been run on; the full table with
firmware, adapters and install methods is at <https://mouse.3m4.net/tested/>.
Rows marked *reported* come from users and have not been reproduced yet.

| Mouse                              | Ids         | Driver                          | Status   | Notes |
|------------------------------------|-------------|---------------------------------|----------|-------|
| Apple Magic Mouse (USB-C, 2024)    | `004c:0323` | `hid-magicmouse` (Linux 6.16+)  | works    | Renamed on a Mac; paired again from *Nearby mice* after the desktop panel left it half paired |
| Apple Magic Mouse 2 (Lightning)    | `004c:0269` | `hid-magicmouse` (Linux 5.15+)  | works    | Not listed by the desktop's Bluetooth panel; *Nearby mice* found and paired it. Both mice work at once |
| Apple Magic Mouse 2 (Lightning)    | `004c:0269` | `hid-magicmouse`                | reported | Zorin OS on a 2017 iMac, issue #20; detection by id shipped in 0.1.5, confirmation pending |

| System                                   | Desktop · session   | Kernel · BlueZ      | Bluetooth · machine                                  | Install                                | Status   |
|------------------------------------------|---------------------|---------------------|------------------------------------------------------|----------------------------------------|----------|
| Pop!_OS 24.04 LTS                        | COSMIC · Wayland    | 6.17.9 · 5.72       | Qualcomm QCA61x4 (USB) · Lenovo IdeaPad Gaming 3     | one-line (.deb), Homebrew, `install.sh` | works    |
| Zorin OS 17                              | GNOME · Wayland     | —                   | built-in · iMac (2017)                               | one-line (.deb)                        | reported |

Tested somewhere else? Open an issue with the mouse ids (`mousetouch probe`),
distro, desktop, `uname -r`, `bluetoothctl --version` and how you installed.

## Development

```sh
cargo build --release
./target/release/mousetouch -v run
./ui/mousetouch-settings
```

Layout: `src/main.rs` (device discovery, event loop), `src/engine.rs`
(gesture recognition), `src/hidraw.rs` (HID report decoding), `src/focus.rs`
(focused-app tracking over Wayland), `src/virt.rs` (uinput devices),
`src/monitor.rs` (event stream for the live view), `src/bluetooth.rs`
(pairing check and repair through bluetoothctl), `src/config.rs`, `ui/` (GTK4/libadwaita
settings with the live view), `packaging/` (systemd unit, udev rule, desktop
file).

To exercise the daemon without hardware: `mousetouch fake-mouse all` creates
a fake Magic Mouse over uinput and plays a swipe, a pinch and a double tap
through it (`MOUSETOUCH_FAKE_NAME` renames it so only a test config matches).
`tools/magicmouse-emu.py` does the same without building the project and
also emulates scrolling, custom device names and IDs; `tools/VM-TESTING.md`
is the checklist for trying a release on other distros in VMs.

## Related work

Other people have attacked the same problem; worth a look to compare approaches:

- [Salacfrantisek/magic-mouse-wayland-gestures](https://github.com/Salacfrantisek/magic-mouse-wayland-gestures)
  independently uses the same idea (hidraw in, gesture-only virtual touchpad
  out) for GNOME, and adds one-finger scroll shaping and two-finger pinch zoom.
- [brenoperucchi/mousetouch](https://github.com/brenoperucchi/mousetouch)
  reads hidraw and maps two-finger swipes to keystrokes (`wtype`).
- [isomoes/mouse-gesture](https://github.com/isomoes/mouse-gesture) is a Rust
  evdev recognizer for the USB-C model with `xdotool` actions (X11).
- [entrope/linux-magicmouse](https://github.com/entrope/linux-magicmouse) is the
  origin of the `hid-magicmouse` kernel driver.

What this project adds on top: compositor-native, finger-following animation
that is not tied to GNOME (configurable 3/4 synthetic fingers), a
driver-independent hidraw decoder with an evdev fallback, a generic rule engine
(tap / double tap / swipe x N fingers -> key, button or command), hot-reloaded
configuration and a settings UI.

## License

MIT
