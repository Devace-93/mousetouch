#!/usr/bin/env python3
"""Emulate a Magic Mouse 2 through /dev/uinput, for testing mousetouch on
machines (or VMs) without the real mouse.

The device looks like what hid-magicmouse exposes over evdev: a mouse with
REL_X/REL_Y, buttons, the wheel at both resolutions, and ABS_MT_* multitouch
(protocol B, 16 slots, the Magic Mouse 2 axis ranges and resolutions). The
daemon finds it the same way it finds the real mouse (name + ABS_MT axes);
no hidraw node exists, so the daemon uses its evdev fallback.

Standard library only; needs write access to /dev/uinput (the mousetouch udev
rule grants it to the logged-in user, otherwise run with sudo).

Scenarios:
  idle          create the device and keep it until Ctrl-C
  scroll        one finger, 25 mm forward; wheel events like the kernel driver
  scroll-back   same, backwards
  swipe-left    two fingers, 30 mm                (workspace swipe)
  swipe-right
  pinch         two fingers moving apart, 12 mm   (zoom)
  tap           two-finger tap
  double-tap    two-finger double tap             (overview on most desktops)
  click         left button click
  all           scroll, swipe-left, swipe-right, pinch, double-tap

Examples:
  tools/magicmouse-emu.py scroll
  tools/magicmouse-emu.py --name "Enrique's Mouse" idle     # renamed mouse (issue #20)
  tools/magicmouse-emu.py --bus usb --product 0323 all      # Magic Mouse USB-C
  tools/magicmouse-emu.py --hold 10 swipe-left              # keep device 10 s after
"""

import argparse
import ctypes
import fcntl
import os
import signal
import struct
import sys
import time

# --- linux/input-event-codes.h ------------------------------------------------
EV_SYN, EV_KEY, EV_REL, EV_ABS = 0, 1, 2, 3
SYN_REPORT = 0
BTN_LEFT, BTN_RIGHT = 0x110, 0x111
REL_X, REL_Y, REL_HWHEEL, REL_WHEEL = 0, 1, 6, 8
REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES = 11, 12
ABS_MT_SLOT, ABS_MT_TOUCH_MAJOR, ABS_MT_TOUCH_MINOR = 0x2F, 0x30, 0x31
ABS_MT_ORIENTATION, ABS_MT_POSITION_X, ABS_MT_POSITION_Y = 0x34, 0x35, 0x36
ABS_MT_TRACKING_ID = 0x39
BUS = {"usb": 0x03, "bluetooth": 0x05}

# --- hid-magicmouse.c, Magic Mouse 2 ------------------------------------------
MIN_X, MAX_X, RES_X = -1100, 1258, 26   # ~90 mm wide
MIN_Y, MAX_Y, RES_Y = -1589, 2047, 70   # ~52 mm deep
HI_RES_NOTCH = 120

# --- linux/uinput.h (ioctls) ----------------------------------------------------
_IOC_NONE, _IOC_WRITE = 0, 1


def _ioc(direction, typ, nr, size):
    return (direction << 30) | (size << 16) | (ord(typ) << 8) | nr


UI_DEV_CREATE = _ioc(_IOC_NONE, "U", 1, 0)
UI_DEV_DESTROY = _ioc(_IOC_NONE, "U", 2, 0)
UI_DEV_SETUP = _ioc(_IOC_WRITE, "U", 3, 92)    # struct uinput_setup
UI_ABS_SETUP = _ioc(_IOC_WRITE, "U", 4, 28)    # struct uinput_abs_setup
UI_SET_EVBIT = _ioc(_IOC_WRITE, "U", 100, 4)
UI_SET_KEYBIT = _ioc(_IOC_WRITE, "U", 101, 4)
UI_SET_RELBIT = _ioc(_IOC_WRITE, "U", 102, 4)
UI_SET_ABSBIT = _ioc(_IOC_WRITE, "U", 103, 4)


class FakeMouse:
    def __init__(self, name, bus, vendor, product, version=0x0311):
        self.fd = os.open("/dev/uinput", os.O_WRONLY | os.O_NONBLOCK)
        f = self.fd
        for ev in (EV_SYN, EV_KEY, EV_REL, EV_ABS):
            fcntl.ioctl(f, UI_SET_EVBIT, ev)
        for k in (BTN_LEFT, BTN_RIGHT):
            fcntl.ioctl(f, UI_SET_KEYBIT, k)
        for r in (REL_X, REL_Y, REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES):
            fcntl.ioctl(f, UI_SET_RELBIT, r)
        abs_axes = [
            # code, min, max, fuzz, flat, resolution
            (ABS_MT_SLOT, 0, 15, 0, 0, 0),
            (ABS_MT_TRACKING_ID, 0, 65535, 0, 0, 0),
            (ABS_MT_POSITION_X, MIN_X, MAX_X, 4, 0, RES_X),
            (ABS_MT_POSITION_Y, MIN_Y, MAX_Y, 4, 0, RES_Y),
            (ABS_MT_TOUCH_MAJOR, 0, 255 << 2, 4, 0, 0),
            (ABS_MT_TOUCH_MINOR, 0, 255 << 2, 4, 0, 0),
            (ABS_MT_ORIENTATION, -31, 32, 1, 0, 0),
        ]
        for code, lo, hi, fuzz, flat, res in abs_axes:
            fcntl.ioctl(f, UI_SET_ABSBIT, code)
            # struct uinput_abs_setup { __u16 code; struct input_absinfo absinfo; }
            fcntl.ioctl(f, UI_ABS_SETUP, struct.pack("HxxiiiiiI", code, 0, lo, hi, fuzz, flat, res))
        # struct uinput_setup { struct input_id id; char name[80]; __u32 ff_effects_max; }
        setup = struct.pack("HHHH80sI", bus, vendor, product, version, name.encode()[:79], 0)
        fcntl.ioctl(f, UI_DEV_SETUP, setup)
        fcntl.ioctl(f, UI_DEV_CREATE)
        self.name = name

    def close(self):
        try:
            fcntl.ioctl(self.fd, UI_DEV_DESTROY)
        finally:
            os.close(self.fd)

    def emit(self, events):
        """events: list of (type, code, value); one SYN_REPORT is appended."""
        now = time.time()
        sec, usec = int(now), int((now - int(now)) * 1e6)
        buf = b"".join(struct.pack("llHHi", sec, usec, t, c, v) for t, c, v in events)
        buf += struct.pack("llHHi", sec, usec, EV_SYN, SYN_REPORT, 0)
        os.write(self.fd, buf)

    # -- multitouch helpers (protocol B) --
    def land(self, fingers):
        """fingers: list of (slot, tracking_id, x, y)."""
        ev = []
        for slot, tid, x, y in fingers:
            ev += [(EV_ABS, ABS_MT_SLOT, slot), (EV_ABS, ABS_MT_TRACKING_ID, tid),
                   (EV_ABS, ABS_MT_POSITION_X, x), (EV_ABS, ABS_MT_POSITION_Y, y),
                   (EV_ABS, ABS_MT_TOUCH_MAJOR, 400), (EV_ABS, ABS_MT_TOUCH_MINOR, 360)]
        self.emit(ev)

    def move(self, fingers):
        """fingers: list of (slot, x, y)."""
        ev = []
        for slot, x, y in fingers:
            ev += [(EV_ABS, ABS_MT_SLOT, slot), (EV_ABS, ABS_MT_POSITION_X, x), (EV_ABS, ABS_MT_POSITION_Y, y)]
        self.emit(ev)

    def lift(self, slots):
        ev = []
        for slot in slots:
            ev += [(EV_ABS, ABS_MT_SLOT, slot), (EV_ABS, ABS_MT_TRACKING_ID, -1)]
        self.emit(ev)


REPORT_MS = 11  # the mouse reports at ~90 Hz


def sleep_report(n=1):
    time.sleep(REPORT_MS * n / 1000)


def mm_x(mm):
    return int(round(mm * RES_X))


def mm_y(mm):
    return int(round(mm * RES_Y))


def scenario_scroll(dev, mm=25.0, backwards=False, bounce_mm=1.0, step_mm=0.25):
    """One finger travelling `mm` along Y. Wheel events are produced the way
    hid-magicmouse does it, from two separate counters:
      REL_WHEEL_HI_RES: 1/10 notch per ~0.13 mm, after 1.3 mm of travel
      REL_WHEEL:        one notch per ~1.4 mm, after 2.9 mm of travel
    The touch starts with a short bounce the other way (`bounce_mm`), like a
    fingertip rolling on the shell; mousetouch's direction debounce exists
    for that."""
    sign = -1 if backwards else 1       # forward = scroll up = wheel +1
    x, y0 = mm_x(-8), mm_y(5)
    dev.land([(0, 40, x, y0)])
    hi_acc = lo_acc = 0.0
    travel = 0.0
    path = [-bounce_mm * (i + 1) / 4 for i in range(4)]        # the bounce
    steps = int(mm / step_mm)
    path += [-bounce_mm + step_mm * (i + 1) for i in range(steps)]   # the real motion
    for p in path:
        sleep_report()
        y = y0 - mm_y(p) * sign
        ev = [(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_POSITION_Y, y)]
        d = p - travel
        travel = p
        hi_acc += d
        lo_acc += d
        if abs(travel) >= 1.3:
            n = int(hi_acc / 0.13)
            if n:
                ev.append((EV_REL, REL_WHEEL_HI_RES, n * 12 * sign))
                hi_acc -= n * 0.13
        if abs(travel) >= 2.9:
            n = int(lo_acc / 1.4)
            if n:
                ev.append((EV_REL, REL_WHEEL, n * sign))
                lo_acc -= n * 1.4
        dev.emit(ev)
    dev.lift([0])


def scenario_swipe(dev, direction, mm=30.0):
    sign = -1 if direction == "left" else 1
    x0, y = mm_x(-15 if sign > 0 else 15), 0
    dev.land([(0, 10, x0, y), (1, 11, x0 + mm_x(12), y)])
    steps = int(mm)
    for i in range(1, steps + 1):
        sleep_report()
        dx = mm_x(i) * sign
        dev.move([(0, x0 + dx, y), (1, x0 + mm_x(12) + dx, y)])
    dev.lift([0, 1])


def scenario_pinch(dev, mm=12.0, out=True):
    x0, y = mm_x(-6), 0
    dev.land([(0, 10, x0, y), (1, 11, x0 + mm_x(12), y)])
    steps = int(mm)
    for i in range(1, steps + 1):
        sleep_report()
        d = mm_x(i / 2) * (1 if out else -1)
        dev.move([(0, x0 - d, y), (1, x0 + mm_x(12) + d, y)])
    dev.lift([0, 1])


def scenario_tap(dev, times=1):
    for i in range(times):
        if i:
            time.sleep(0.12)
        dev.land([(0, 10, mm_x(-6), 0), (1, 11, mm_x(6), 0)])
        time.sleep(0.08)
        dev.lift([0, 1])


def scenario_click(dev):
    dev.emit([(EV_KEY, BTN_LEFT, 1)])
    time.sleep(0.08)
    dev.emit([(EV_KEY, BTN_LEFT, 0)])


def run_scenario(dev, name):
    print(f"-> {name}")
    if name == "scroll":
        scenario_scroll(dev)
    elif name == "flick":
        # 3 mm per report is ~270 mm/s: fast enough for mousetouch's
        # acceleration and momentum to kick in.
        scenario_scroll(dev, mm=45.0, step_mm=3.0)
    elif name == "scroll-back":
        scenario_scroll(dev, backwards=True)
    elif name in ("swipe-left", "swipe-right"):
        scenario_swipe(dev, name.split("-")[1])
    elif name == "pinch":
        scenario_pinch(dev)
    elif name == "tap":
        scenario_tap(dev)
    elif name == "double-tap":
        scenario_tap(dev, 2)
    elif name == "click":
        scenario_click(dev)
    else:
        sys.exit(f"unknown scenario {name}")


SCENARIOS = ["idle", "scroll", "scroll-back", "flick", "swipe-left", "swipe-right", "pinch", "tap", "double-tap", "click", "all"]
ALL = ["scroll", "swipe-left", "swipe-right", "pinch", "double-tap"]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("scenario", nargs="*", default=["all"], choices=SCENARIOS)
    ap.add_argument("--name", default="Magic Mouse", help="evdev device name (default: %(default)s)")
    ap.add_argument("--bus", choices=BUS, default="bluetooth")
    ap.add_argument("--vendor", default=None, help="hex, default 004c (Bluetooth) / 05ac (USB)")
    ap.add_argument("--product", default="0269", help="hex: 030d, 0269 (default), 0323")
    ap.add_argument("--settle", type=float, default=2.5, help="seconds to wait for the daemon to attach")
    ap.add_argument("--hold", type=float, default=1.0, help="seconds to keep the device after the last scenario")
    ap.add_argument("--repeat", type=int, default=1)
    ap.add_argument("--gap", type=float, default=0.9, help="seconds between scenarios")
    a = ap.parse_args()
    vendor = int(a.vendor or ("004c" if a.bus == "bluetooth" else "05ac"), 16)
    product = int(a.product, 16)

    try:
        dev = FakeMouse(a.name, BUS[a.bus], vendor, product)
    except PermissionError:
        sys.exit("cannot open /dev/uinput: install the mousetouch udev rule (or run with sudo)")
    node = None
    for d in sorted(os.listdir("/sys/class/input")):
        p = f"/sys/class/input/{d}/device/name"
        if d.startswith("event") and os.path.exists(p) and open(p).read().strip() == a.name:
            node = f"/dev/input/{d}"
    print(f"created {a.name!r} bus={a.bus} vendor={vendor:04x} product={product:04x} at {node or '?'}")

    stop = False

    def on_sig(*_):
        nonlocal stop
        stop = True

    signal.signal(signal.SIGINT, on_sig)
    signal.signal(signal.SIGTERM, on_sig)
    try:
        scenarios = a.scenario
        if scenarios == ["idle"]:
            print("idle; Ctrl-C to remove the device")
            while not stop:
                time.sleep(0.2)
            return
        if "all" in scenarios:
            scenarios = ALL
        print(f"waiting {a.settle}s for the daemon to attach")
        time.sleep(a.settle)
        for r in range(a.repeat):
            for i, s in enumerate(scenarios):
                if stop:
                    return
                if i or r:
                    time.sleep(a.gap)
                run_scenario(dev, s)
        time.sleep(a.hold)
    finally:
        dev.close()
        print("device removed")


if __name__ == "__main__":
    main()
