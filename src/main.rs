//! mousetouch: userspace driver that adds desktop gestures to the
//! Apple Magic Mouse on Linux.
//!
//! The daemon grabs the mouse's evdev node, forwards normal pointer events
//! through a virtual mouse, and replays multi-finger motion on a virtual
//! touchpad so the compositor's native swipe gestures (with their animation
//! and finger tracking) drive workspace switching. Taps and discrete swipes
//! map to keyboard shortcuts, buttons or commands.

mod bluetooth;
mod config;
mod desktop;
mod discover;
mod engine;
mod focus;
mod hidraw;
mod monitor;
mod scroll;
mod virt;

use anyhow::{Context, Result};
use config::Config;
use engine::{AxisInfo, Engine, Shared};
use evdev::{AbsoluteAxisCode, Device};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};
use virt::{VirtualKeyboard, VirtualMouse, VirtualTouchpad};

const USAGE: &str = "\
mousetouch [OPTIONS] [COMMAND]

Commands:
  run                 Run the daemon (default)
  probe               List matching devices and their touch calibration
  watch               Like probe, then print live finger positions (Ctrl-C to stop)
  events              Print the running daemon's event stream (fingers, gestures,
                      scroll, buttons) as JSON lines; what the settings app draws
  bt-check [NAME]     Is the mouse's Bluetooth pairing healthy? (exit 3 = stale
                      pairing after it was paired with a Mac, 4 = not paired)
  bt-repair [NAME] [SECONDS]
                      Remove the mouse from BlueZ and pair it again (asks you
                      to switch it off and on). NAME = part of its Bluetooth
                      name, default the config's device_match
  bt-scan [SECONDS]   Discover for SECONDS (default 6) and list the mice that
                      are switched on and not connected, one JSON object per
                      line (addr, name, paired, bonded, rssi, state)
  bt-pair ADDR [SECONDS]
                      Remove whatever BlueZ knows about that mouse, then pair,
                      trust and connect it (what Settings > Nearby mice runs)
  demo-swipe DIR [MM] Replay a synthetic swipe on the virtual touchpad
                      (DIR = left|right|up|down, MM = travel, default 40)
  print-config        Print the effective configuration as TOML
  key COMBO           Press a key combo through a virtual keyboard (testing),
                      e.g. key ctrl+kpplus  or  key 'ctrl+equal,ctrl+equal'
  fake-mouse [WHAT]   Create a fake Magic Mouse via uinput and play gestures
                      through it (testing). WHAT = all (swipe, pinch, 2-finger
                      double tap) or double-tap-1 [N times]

Options:
  -c, --config PATH   Config file (default: ~/.config/mousetouch/config.toml)
  -v, --verbose       Debug logging
  -V, --version       Print the version
  -h, --help          Show this help";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut config_path = config::default_path();
    let mut verbose = false;
    let mut cmd = String::from("run");
    let mut rest: Vec<String> = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" | "--config" => config_path = PathBuf::from(args.next().context("--config needs a path")?),
            "-v" | "--verbose" => verbose = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("mousetouch {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other if other.starts_with('-') => anyhow::bail!("unknown option {other}\n{USAGE}"),
            other => {
                cmd = other.to_string();
                rest = args.by_ref().collect();
            }
        }
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(if verbose { "debug" } else { "info" }))
        .format_timestamp_millis()
        .init();

    match cmd.as_str() {
        "run" => run(&config_path),
        "probe" => probe(&config_path, false),
        "watch" => probe(&config_path, true),
        "events" => monitor::print_events(),
        "bt-check" | "bt-repair" => {
            let name = match rest.first() {
                Some(n) => n.clone(),
                None => config::load(&config_path).map(|c| c.general.device_match).unwrap_or_else(|_| config::General::default().device_match),
            };
            if cmd == "bt-check" {
                bluetooth::check(&name)
            } else {
                let secs = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(90);
                bluetooth::repair(&name, Duration::from_secs(secs))
            }
        }
        "bt-scan" => bluetooth::scan_cmd(rest.first().and_then(|s| s.parse().ok()).unwrap_or(6)),
        "bt-pair" => {
            let Some(addr) = rest.first() else { anyhow::bail!("bt-pair needs the mouse's Bluetooth address (see bt-scan)") };
            let secs = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(60);
            bluetooth::pair(addr, Duration::from_secs(secs))
        }
        "demo-swipe" => demo_swipe(&rest),
        "fake-mouse" => fake_mouse(
            rest.first().map(String::as_str).unwrap_or("all"),
            rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(1),
        ),
        "key" => {
            let combo = rest.first().context("key needs a combo, e.g. ctrl+kpplus")?;
            let mut kb = VirtualKeyboard::new()?;
            thread::sleep(Duration::from_millis(800)); // let the compositor add the device
            kb.tap_combo(combo)?;
            thread::sleep(Duration::from_millis(200));
            Ok(())
        }
        "print-config" => {
            let cfg = config::load_or_create(&config_path)?;
            print!("{}", config::to_toml(&cfg)?);
            Ok(())
        }
        other => anyhow::bail!("unknown command {other}\n{USAGE}"),
    }
}

fn axis_info(dev: &Device, code: AbsoluteAxisCode, fallback_res: f64) -> Result<AxisInfo> {
    let abs = dev.get_abs_state()?;
    let a = abs[code.0 as usize];
    let res = if a.resolution > 0 { a.resolution as f64 } else { fallback_res };
    Ok(AxisInfo { min: a.minimum, max: a.maximum, res })
}

fn run(config_path: &Path) -> Result<()> {
    let cfg = Arc::new(RwLock::new(config::load_or_create(config_path)?));
    log::info!("config: {}", config_path.display());
    let shared = Arc::new(Mutex::new(Shared {
        touchpad: VirtualTouchpad::new()?,
        keyboard: VirtualKeyboard::new()?,
        toggles: HashMap::new(),
    }));
    let compositor_natural = config::detect_compositor_natural_scroll();
    log::info!("compositor touchpad natural scroll: {compositor_natural}");
    let focus = focus::spawn();
    let monitor = monitor::Monitor::start();
    // Homebrew installs are outside the desktop's search path; add the
    // settings app to the app menu from here (no-op elsewhere).
    desktop::ensure_app_menu_entry();

    let active: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
    // Device number stamped on the event stream, so the settings app can
    // tell two mice apart; never reused while the daemon runs.
    let mut next_dev: u32 = 0;
    let mut last_mtime = std::fs::metadata(config_path).and_then(|m| m.modified()).ok();
    // Nodes already reported as unusable, so the warning is logged once.
    let mut warned: HashSet<PathBuf> = HashSet::new();

    loop {
        // Hot-reload the config when the file changes.
        let mtime = std::fs::metadata(config_path).and_then(|m| m.modified()).ok();
        if mtime != last_mtime {
            last_mtime = mtime;
            match config::load(config_path) {
                Ok(new) => {
                    *cfg.write().unwrap() = new;
                    log::info!("config reloaded");
                }
                Err(e) => log::error!("config not reloaded: {e:#}"),
            }
        }

        let device_match = cfg.read().unwrap().general.device_match.clone();
        let scan = discover::scan(&device_match);
        warned.retain(|p| scan.unusable.iter().any(|u| &u.path == p));
        for u in &scan.unusable {
            if warned.insert(u.path.clone()) {
                log::warn!("{}", u.describe());
            }
        }
        for (path, dev) in scan.devices {
            if active.lock().unwrap().contains(&path) {
                continue;
            }
            active.lock().unwrap().insert(path.clone());
            let cfg = Arc::clone(&cfg);
            let shared = Arc::clone(&shared);
            let active = Arc::clone(&active);
            let focus = Arc::clone(&focus);
            next_dev += 1;
            let monitor = monitor.for_device(next_dev);
            thread::spawn(move || {
                let name = dev.name().unwrap_or("?").to_string();
                log::info!("attached {} ({})", name, path.display());
                if let Err(e) = serve_device(&path, dev, cfg, shared, compositor_natural, focus, monitor.clone()) {
                    log::warn!("{} detached: {e:#}", name);
                } else {
                    log::info!("{} detached", name);
                }
                monitor.set_device(None);
                active.lock().unwrap().remove(&path);
            });
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn serve_device(
    path: &Path,
    mut dev: Device,
    cfg: Arc<RwLock<Config>>,
    shared: Arc<Mutex<Shared>>,
    compositor_natural: bool,
    focus: focus::Focus,
    monitor: monitor::Monitor,
) -> Result<()> {
    // Magic Mouse 2 geometry from hid-magicmouse, used only if the driver
    // reports no resolution.
    let ax = axis_info(&dev, AbsoluteAxisCode::ABS_MT_POSITION_X, 26.0)?;
    let ay = axis_info(&dev, AbsoluteAxisCode::ABS_MT_POSITION_Y, 70.0)?;
    log::debug!("touch axes: x={ax:?} y={ay:?}");
    let mouse = VirtualMouse::new(dev.name().unwrap_or("Magic Mouse"), dev.input_id())?;
    // Give the compositor a moment to pick up the virtual mouse before the
    // real one goes silent.
    thread::sleep(Duration::from_millis(300));
    dev.grab().context("grabbing device (EVIOCGRAB)")?;
    dev.set_nonblocking(true)?;

    let mut engine = Engine::new(cfg, shared, mouse, ax, ay, compositor_natural, focus, monitor.clone());

    // Prefer raw HID reports for touch data (see hidraw.rs for why).
    let mut raw: Option<std::fs::File> = None;
    let mut source = "evdev";
    match hidraw::hidraw_for_event(path) {
        Some(raw_path) => match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(&raw_path) {
            Ok(f) => {
                log::info!("touch data from {}", raw_path.display());
                engine.set_hidraw_mode(true);
                raw = Some(f);
                source = "hidraw";
            }
            Err(e) => log::warn!(
                "cannot open {} ({e}); falling back to evdev multitouch, which is broken with the DKMS hid-magicmouse. Install the udev rule from packaging/.",
                raw_path.display()
            ),
        },
        None => log::warn!("no hidraw node found for {}; using evdev multitouch", path.display()),
    }
    monitor.set_device(Some(monitor::Event::Device {
        name: dev.name().unwrap_or("Magic Mouse").to_string(),
        w_mm: (ax.max - ax.min) as f64 / ax.res,
        h_mm: (ay.max - ay.min) as f64 / ay.res,
        source,
    }));

    let fd = dev.as_raw_fd();
    let raw_fd = raw.as_ref().map(|f| f.as_raw_fd()).unwrap_or(-1);
    let mut report = [0u8; 256];
    let mut touches = Vec::with_capacity(8);
    loop {
        let timeout_ms = match engine.next_deadline() {
            Some(d) => d.saturating_duration_since(Instant::now()).as_micros().div_ceil(1000).min(1000) as i32,
            None => 1000,
        };
        let mut pfds = [
            libc::pollfd { fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: raw_fd, events: libc::POLLIN, revents: 0 },
        ];
        let n = if raw_fd >= 0 { 2 } else { 1 };
        let r = unsafe { libc::poll(pfds.as_mut_ptr(), n, timeout_ms) };
        if r < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }
        if r > 0 {
            if pfds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                anyhow::bail!("device went away");
            }
            if pfds[0].revents & libc::POLLIN != 0 {
                match dev.fetch_events() {
                    Ok(events) => {
                        for ev in events {
                            engine.handle(ev)?;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
            }
            if n == 2 && pfds[1].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                anyhow::bail!("hidraw device went away");
            }
            if n == 2 && pfds[1].revents & libc::POLLIN != 0 {
                let f = raw.as_mut().unwrap();
                loop {
                    match f.read(&mut report) {
                        Ok(0) => break,
                        Ok(len) => {
                            if hidraw::parse_report(&report[..len], &mut touches) {
                                log::trace!("hid report: {touches:?}");
                                engine.handle_touch_frame(&touches)?;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }
        engine.tick(Instant::now())?;
    }
}

fn probe(config_path: &Path, watch: bool) -> Result<()> {
    let cfg = config::load_or_create(config_path)?;
    let scan = discover::scan(&cfg.general.device_match);
    for u in &scan.unusable {
        println!("unusable: {}", u.describe());
    }
    let devices = scan.devices;
    if devices.is_empty() {
        if scan.unusable.is_empty() {
            println!("no Magic Mouse found (Apple vendor id with product 030d/0269/0323, or a name containing \"{}\")", cfg.general.device_match);
            println!("(is the mouse connected over Bluetooth and bound to hid-magicmouse? `journalctl -k | grep -i \"magic mouse\"`)");
        }
        return Ok(());
    }
    for (path, dev) in &devices {
        let id = dev.input_id();
        println!("{}: {}", path.display(), dev.name().unwrap_or("?"));
        println!("  bus={:?} vendor={:04x} product={:04x} version={:04x}", id.bus_type(), id.vendor(), id.product(), id.version());
        for code in [AbsoluteAxisCode::ABS_MT_POSITION_X, AbsoluteAxisCode::ABS_MT_POSITION_Y] {
            let a = axis_info(dev, code, 0.0)?;
            let mm = if a.res > 0.0 { (a.max - a.min) as f64 / a.res } else { f64::NAN };
            println!("  {code:?}: {}..{} res={} units/mm (~{mm:.1} mm)", a.min, a.max, a.res);
        }
    }
    if !watch {
        return Ok(());
    }
    let (path, mut dev) = devices.into_iter().next().unwrap();
    let ax = axis_info(&dev, AbsoluteAxisCode::ABS_MT_POSITION_X, 26.0)?;
    let ay = axis_info(&dev, AbsoluteAxisCode::ABS_MT_POSITION_Y, 70.0)?;
    println!("watching {} (not grabbed) ... Ctrl-C to stop", path.display());
    let mut slots: [Option<(i32, i32)>; 16] = [None; 16];
    let mut slot = 0usize;
    loop {
        for ev in dev.fetch_events()? {
            match ev.event_type() {
                evdev::EventType::ABSOLUTE => match AbsoluteAxisCode(ev.code()) {
                    AbsoluteAxisCode::ABS_MT_SLOT => slot = (ev.value().max(0) as usize).min(15),
                    AbsoluteAxisCode::ABS_MT_TRACKING_ID => {
                        slots[slot] = if ev.value() < 0 { None } else { Some((0, 0)) };
                    }
                    AbsoluteAxisCode::ABS_MT_POSITION_X => {
                        if let Some(p) = &mut slots[slot] {
                            p.0 = ev.value();
                        }
                    }
                    AbsoluteAxisCode::ABS_MT_POSITION_Y => {
                        if let Some(p) = &mut slots[slot] {
                            p.1 = ev.value();
                        }
                    }
                    _ => {}
                },
                evdev::EventType::SYNCHRONIZATION if ev.code() == 0 => {
                    let fingers: Vec<String> = slots
                        .iter()
                        .flatten()
                        .map(|(x, y)| format!("({:5.1},{:5.1})mm", (x - ax.min) as f64 / ax.res, (y - ay.min) as f64 / ay.res))
                        .collect();
                    if !fingers.is_empty() {
                        println!("{} finger(s): {}", fingers.len(), fingers.join(" "));
                    }
                }
                _ => {}
            }
        }
    }
}

fn demo_swipe(args: &[String]) -> Result<()> {
    let dir = args.first().map(String::as_str).unwrap_or("left");
    let mm: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(40.0);
    let fingers: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4);
    let (ux, uy) = match dir {
        "left" => (-1.0, 0.0),
        "right" => (1.0, 0.0),
        "up" => (0.0, -1.0),
        "down" => (0.0, 1.0),
        other => anyhow::bail!("bad direction {other}"),
    };
    let mut pad = VirtualTouchpad::new()?;
    thread::sleep(Duration::from_millis(1500)); // let the compositor add the device
    println!("swiping {dir} {mm} mm with {fingers} fingers");
    pad.begin(fingers)?;
    let steps = 40;
    for _ in 0..steps {
        pad.move_by(ux * mm / steps as f64, uy * mm / steps as f64)?;
        thread::sleep(Duration::from_millis(10));
    }
    pad.end()?;
    thread::sleep(Duration::from_millis(300));
    Ok(())
}

/// Test helper: emulate the evdev stream of a Magic Mouse 2 as produced by
/// hid-magicmouse, so the daemon can be exercised without touching hardware.
fn fake_mouse(what: &str, repeat: usize) -> Result<()> {
    use evdev::uinput::VirtualDevice;
    use evdev::{AbsInfo, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode, RelativeAxisCode, UinputAbsSetup};
    let mut keys = AttributeSet::<KeyCode>::new();
    keys.insert(KeyCode::BTN_LEFT);
    keys.insert(KeyCode::BTN_RIGHT);
    let mut rels = AttributeSet::<RelativeAxisCode>::new();
    rels.insert(RelativeAxisCode::REL_X);
    rels.insert(RelativeAxisCode::REL_Y);
    rels.insert(RelativeAxisCode::REL_WHEEL);
    // MOUSETOUCH_FAKE_NAME lets tests pick a name that only a test config
    // matches, so another daemon on the machine leaves the fake mouse alone.
    let name = std::env::var("MOUSETOUCH_FAKE_NAME").unwrap_or_else(|_| "Fake Magic Mouse".into());
    let mut dev = VirtualDevice::builder()?
        .name(&name)
        .input_id(InputId::new(BusType::BUS_BLUETOOTH, 0x004c, 0x0269, 0x0311))
        .with_keys(&keys)?
        .with_relative_axes(&rels)?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_MT_SLOT, AbsInfo::new(0, 0, 15, 0, 0, 0)))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_MT_TRACKING_ID, AbsInfo::new(0, 0, 65535, 0, 0, 0)))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_MT_POSITION_X, AbsInfo::new(0, -1100, 1258, 4, 0, 26)))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_MT_POSITION_Y, AbsInfo::new(0, -1589, 2047, 4, 0, 70)))?
        .build()?;
    let a = |code: AbsoluteAxisCode, v: i32| InputEvent::new(EventType::ABSOLUTE.0, code.0, v);
    let syn = InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0);
    println!("fake mouse created; waiting for the daemon to attach");
    thread::sleep(Duration::from_millis(2500));

    if what == "double-tap-1" {
        // One finger, two quick taps with the drift a real fingertip shows,
        // repeated `repeat` times with a pause in between.
        for r in 0..repeat {
            if r > 0 {
                thread::sleep(Duration::from_millis(900));
            }
            println!("1-finger double tap");
        for _ in 0..2 {
            dev.emit(&[
                a(AbsoluteAxisCode::ABS_MT_SLOT, 0),
                a(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 20),
                a(AbsoluteAxisCode::ABS_MT_POSITION_X, -244),
                a(AbsoluteAxisCode::ABS_MT_POSITION_Y, -278),
                syn,
            ])?;
            for step in 1..=8 {
                thread::sleep(Duration::from_millis(11));
                dev.emit(&[a(AbsoluteAxisCode::ABS_MT_POSITION_Y, -278 - 60 * step), syn])?;
            }
            // The fingertip settles for a couple of reports before it leaves.
            for step_settle in 1..=2 {
                thread::sleep(Duration::from_millis(11));
                dev.emit(&[a(AbsoluteAxisCode::ABS_MT_POSITION_X, -244 + 10 * (step_settle as i32)), syn])?;
            }
            dev.emit(&[a(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1), syn])?;
            thread::sleep(Duration::from_millis(150));
        }
        }
        thread::sleep(Duration::from_millis(600));
        return Ok(());
    }

    let land = |dev: &mut VirtualDevice, x0: i32| -> Result<()> {
        dev.emit(&[
            a(AbsoluteAxisCode::ABS_MT_SLOT, 0),
            a(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 10),
            a(AbsoluteAxisCode::ABS_MT_POSITION_X, x0),
            a(AbsoluteAxisCode::ABS_MT_POSITION_Y, 0),
            a(AbsoluteAxisCode::ABS_MT_SLOT, 1),
            a(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 11),
            a(AbsoluteAxisCode::ABS_MT_POSITION_X, x0 + 300),
            a(AbsoluteAxisCode::ABS_MT_POSITION_Y, 0),
            syn,
        ])?;
        Ok(())
    };
    let lift = |dev: &mut VirtualDevice| -> Result<()> {
        dev.emit(&[
            a(AbsoluteAxisCode::ABS_MT_SLOT, 0),
            a(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1),
            a(AbsoluteAxisCode::ABS_MT_SLOT, 1),
            a(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1),
            syn,
        ])?;
        Ok(())
    };

    println!("2-finger swipe left, 30 mm");
    land(&mut dev, 400)?;
    for step in 1..=30 {
        thread::sleep(Duration::from_millis(12));
        dev.emit(&[
            a(AbsoluteAxisCode::ABS_MT_SLOT, 0),
            a(AbsoluteAxisCode::ABS_MT_POSITION_X, 400 - 26 * step),
            a(AbsoluteAxisCode::ABS_MT_SLOT, 1),
            a(AbsoluteAxisCode::ABS_MT_POSITION_X, 700 - 26 * step),
            syn,
        ])?;
    }
    lift(&mut dev)?;
    thread::sleep(Duration::from_millis(800));

    println!("2-finger pinch out, 12 mm");
    land(&mut dev, 0)?;
    for step in 1..=12 {
        thread::sleep(Duration::from_millis(12));
        dev.emit(&[
            a(AbsoluteAxisCode::ABS_MT_SLOT, 0),
            a(AbsoluteAxisCode::ABS_MT_POSITION_X, -13 * step),
            a(AbsoluteAxisCode::ABS_MT_SLOT, 1),
            a(AbsoluteAxisCode::ABS_MT_POSITION_X, 300 + 13 * step),
            syn,
        ])?;
    }
    lift(&mut dev)?;
    thread::sleep(Duration::from_millis(800));

    println!("2-finger double tap");
    for _ in 0..2 {
        land(&mut dev, 0)?;
        thread::sleep(Duration::from_millis(80));
        lift(&mut dev)?;
        thread::sleep(Duration::from_millis(120));
    }
    thread::sleep(Duration::from_millis(600));
    println!("done");
    Ok(())
}
