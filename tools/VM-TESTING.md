# Testing mousetouch across distros in VMs

No USB sticks needed: everything the daemon touches (`/dev/uinput`, udev, a
systemd user session, a Wayland compositor) exists inside a VM. The only
thing a VM cannot have for free is the mouse itself; see *Getting a mouse
into the VM*.

## Host setup (Pop!_OS)

```sh
sudo apt install virt-manager qemu-system-x86 ovmf
sudo usermod -aG libvirt "$USER"   # log out and in again
```

Per VM (virt-manager → New virtual machine):

- 2–4 vCPU, 4 GB RAM, 25 GB disk, firmware UEFI (OVMF).
- Video: **Virtio** with **3D acceleration** on; Display: Spice with OpenGL
  on. Without this GNOME/KDE Wayland sessions are unusable.
- Install the desktop, log in once, then **take a snapshot** named
  `fresh`. Every test run starts by reverting to it.

## Distro matrix

| VM | Why | Install channel to test |
|----|-----|-------------------------|
| Zorin OS 17 (GNOME) | issue #20 reporter | `get.sh`, `.deb` |
| Ubuntu 24.04 (GNOME) | most Magic Mouse users | `.deb`, `get.sh` |
| Fedora Workstation | rpm path, SELinux | `.rpm` |
| Arch + KDE Plasma | AUR package, KDE focus tracking | `PKGBUILD` from `packaging/aur` |
| NixOS (optional) | flake module | `services.mousetouch` |
| Pop!_OS 24.04 (COSMIC) | control, same as the dev machine | `get.sh` |

## Getting a mouse into the VM

Pick one:

1. **Real mouse, Bluetooth dongle passthrough.** The Magic Mouse cannot be
   used while cabled, so pass a Bluetooth *controller* instead: a cheap USB
   dongle, or the laptop's internal Bluetooth (usually a USB device; the host
   loses Bluetooth while the VM owns it). virt-manager → Add Hardware → USB
   Host Device. Pair the mouse inside the guest with its Bluetooth settings.
   This is the only way to test the hidraw path and real scroll feel.
2. **Emulator** (`tools/magicmouse-emu.py`). Stdlib Python, creates a fake
   Magic Mouse 2 through `/dev/uinput` and plays gestures. Covers detection,
   permissions, name matching, the scroll shaper, taps, swipes and pinch
   through the daemon's evdev fallback. Does not cover hidraw decoding or
   Bluetooth pairing. Copy it into the guest with `scp`, or via a shared
   folder.

## Checklist per VM

Revert to `fresh`, then:

### 1. Install

- [ ] Install through the channel in the matrix. Note the exact command.
- [ ] `mousetouch --version` prints the release.
- [ ] `systemctl --user status mousetouch` is `active (running)`.
- [ ] Udev rule present: `ls /etc/udev/rules.d/60-mousetouch.rules` or
      `/usr/lib/udev/rules.d/60-mousetouch.rules`.
- [ ] `getfacl /dev/uinput` shows `user:<you>:rw-` (uaccess applied).
- [ ] App menu shows *MouseTouch Settings* with the mouse icon; it launches.
- [ ] `~/.config/mousetouch/config.toml` was created by the daemon, owned by
      you, not root.

### 2. Detection (emulator)

```sh
python3 magicmouse-emu.py idle          # leave running in a terminal
mousetouch probe                        # in another one
```

- [ ] `probe` lists `/dev/input/eventN: Magic Mouse`, ~90.7 × 51.9 mm.
- [ ] `getfacl /dev/input/eventN` shows `user:<you>:rw-`.
- [ ] Settings app shows the device as connected (Live view works).
- [ ] `mousetouch events` prints `{"t":"device","name":"Magic Mouse",...,"source":"evdev"}`.

Issue #20 reproductions (expected to fail today; a fix should make them pass
or at least report *why*):

- [ ] **Paired before the rule existed.** Start the emulator *before*
      installing mousetouch, then install. Does `probe` see it? If not,
      does `sudo udevadm trigger --subsystem-match=input` fix it without
      reconnecting?
- [ ] **Renamed mouse.** `magicmouse-emu.py --name "Someone's Mouse" idle`.
      `probe` says *no device matching "Magic Mouse"* and the node has no
      ACL. With the real mouse this is what a mouse renamed on macOS looks like.
- [ ] **Permission denied is reported as "no device".** With the emulator
      running as root (`sudo`) and the udev rule removed, `probe` as the user
      must not say "no device found" when the real reason is EACCES.
- [ ] **No sudo needed.** Everything above as the regular user; never run
      the Settings app with `sudo` (it would write `/root/.config/...`).

### 3. Gestures (emulator, with a desktop session open)

```sh
python3 magicmouse-emu.py all
```

Watch `mousetouch events` in a terminal and the desktop:

- [ ] `scroll`: `{"t":"scroll","dy":...}` lines; the window under the
      pointer scrolls up. With `scroll.mode = "notches"` every line is a
      multiple of 120; with `"smooth"` they are 12-unit steps.
- [ ] `swipe-left` / `swipe-right`: workspace changes with the desktop's
      own animation (GNOME: 3 synthetic fingers; COSMIC: 4).
- [ ] `pinch`: the focused app zooms (test in a browser or image viewer).
- [ ] `double-tap`: overview opens (`super+w` on COSMIC/KDE, `super` on GNOME).
- [ ] `click`: a normal left click arrives through the virtual mouse.

### 4. Scroll feel (real mouse only, issue #21)

In LibreOffice Calc and a browser, compare:

- [ ] Stock driver (daemon stopped: `systemctl --user stop mousetouch`).
- [ ] mousetouch, `mode = "notches"` (default today).
- [ ] mousetouch, `mode = "smooth"`, `debounce_units = 40`.

Note which feels closest to stock. The reporter's "momentum" is probably the
high-resolution steps that `notches` collapses.

### 5. Settings app

- [ ] Follows the system theme and language; manual override sticks across
      restarts (`~/.config/mousetouch/ui.toml`).
- [ ] Changing a setting is picked up by the daemon without restart
      (journal: `config reloaded`).
- [ ] Restart button restarts the service.

### 6. Remove

- [ ] `uninstall.sh` / package removal leaves no unit, rule, binary or icon:
      `purge.sh` reports nothing left.
- [ ] Revert the VM to `fresh`.

## Recording the site demo

`site/demo.webm` / `site/demo.gif` were recorded in a Pop!_OS 24.04 VM
(COSMIC) with no real mouse: `mousetouch fake-mouse demo` creates an
emulated Magic Mouse and plays a slow, eased sequence through the running
daemon (swipe to the next workspace, pinch out and in, double tap twice,
swipe back), so the settings app's live view and the desktop react exactly
as with the real device.

Scene: the detached live view (Live view → pop-out button) made *sticky*
(window menu → Sticky window) on the right, a file manager on one
workspace and Firefox on the next one (pinch zoom needs an app that
handles touchpad pinch; the pointer sits over the page). Frames come from
QEMU's QMP `screendump` (~30 fps, written to `/dev/shm` and shrunk to JPEG
as they arrive), then resampled to 30 fps VP9 and a 12 fps GIF, cropped
above the dock.

## Recording results

One row per VM in the PR or issue that triggered the run:

| VM | Channel | Version | Install | Detect | Gestures | Scroll | Notes |
|----|---------|---------|---------|--------|----------|--------|-------|
| Zorin 17 | get.sh | 0.1.3 | ✅ | ❌ #20 | | | ACL missing until re-pair |

## Known limits

- The emulator's node has no hidraw sibling, so the daemon logs
  `no hidraw node found ...; using evdev multitouch`. Expected.
- `fake-mouse` inside the `mousetouch` binary does a subset of this (swipe,
  pinch, double tap) and is what the Rust tests use; the Python emulator adds
  scrolling, custom names/IDs and works without building the project.
