//! Talk to a MouseTouch Bridge (a Raspberry Pi Pico 2 W running the
//! magicmouse-pico firmware) over its USB-CDC serial line.
//!
//! The card pairs the Magic Mouse over Bluetooth and re-exposes it to the host
//! over USB, so a mouse paired once works on any OS with no driver. On Linux we
//! can additionally put the card in `raw` mode and run our own gesture engine
//! on the fingers it streams (see `serve_bridge` in main.rs).
//!
//! This module mirrors `bluetooth.rs`: the CLI subcommands print the same
//! `step:`/`hint:`/`ok:` progress and the same per-device JSON, so the settings
//! app's existing `PairDialog`/`NearbyMice` drive it unchanged.
//!
//! No serial crate is used (the release is a static musl binary); the port is a
//! plain fd configured with libc termios, and DTR is asserted explicitly — the
//! firmware's CDC stays silent until it is.

use anyhow::{bail, Context, Result};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// USB ids of the Pico-based bridge (Raspberry Pi vendor, our composite PID).
const BRIDGE_VID: &str = "2e8a";
// 000e: composite CDC + mouse + keyboard + touchpad (current firmware).
// 000c: the earlier CDC + mouse + touchpad layout.
const BRIDGE_PIDS: &[&str] = &["000e", "000c"];
/// A bare board in BOOTSEL (mass storage) advertises one of these instead
/// (RP2350 / RP2040 boot ROM); used by `detect` to offer an "install firmware"
/// action for a card without the firmware.
const RP2_BOOTSEL_PIDS: &[&str] = &["000f", "0003"];

// ---- port discovery -------------------------------------------------------

/// The `/dev/ttyACMx` whose USB device is our bridge, if one is plugged in.
pub fn find_port() -> Option<PathBuf> {
    for entry in fs::read_dir("/sys/class/tty").ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_str().unwrap_or("");
        if !name.starts_with("ttyACM") {
            continue;
        }
        // /sys/class/tty/ttyACMx/device is the CDC interface; ../.. is the USB device.
        let dev = entry.path().join("device");
        let vid = read_usb_attr(&dev, "idVendor");
        let pid = read_usb_attr(&dev, "idProduct");
        if vid.as_deref() == Some(BRIDGE_VID) && pid.as_deref().is_some_and(|p| BRIDGE_PIDS.contains(&p)) {
            return Some(PathBuf::from("/dev").join(name));
        }
    }
    None
}

/// Walk up from a tty `device` symlink to the USB device dir and read `attr`
/// (idVendor/idProduct live on the device, not the interface).
fn read_usb_attr(iface_dev: &std::path::Path, attr: &str) -> Option<String> {
    let mut dir = fs::canonicalize(iface_dev).ok()?;
    for _ in 0..4 {
        let p = dir.join(attr);
        if let Ok(s) = fs::read_to_string(&p) {
            return Some(s.trim().to_ascii_lowercase());
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

/// Is any RP2350 board present in BOOTSEL (no firmware)? Returns its mount-less
/// presence so the app can offer to flash the bridge firmware (req 4).
pub fn bootsel_present() -> bool {
    let Ok(dir) = fs::read_dir("/sys/bus/usb/devices") else { return false };
    for entry in dir.flatten() {
        let p = entry.path();
        let vid = fs::read_to_string(p.join("idVendor")).ok();
        let pid = fs::read_to_string(p.join("idProduct")).ok();
        if vid.as_deref().map(|s| s.trim().to_ascii_lowercase()) == Some(BRIDGE_VID.to_string())
            && pid.as_deref().map(|s| s.trim().to_ascii_lowercase()).is_some_and(|p| RP2_BOOTSEL_PIDS.contains(&p.as_str()))
        {
            return true;
        }
    }
    false
}

// ---- serial line transport ------------------------------------------------

/// An open bridge serial connection configured for raw 115200 8N1 with DTR set.
pub struct Bridge {
    file: std::fs::File,
    buf: String,
}

impl Bridge {
    /// Open the auto-detected bridge port.
    pub fn open() -> Result<Bridge> {
        let port = find_port().context("no MouseTouch Bridge found (is the card plugged in?)")?;
        Bridge::open_path(&port)
    }

    pub fn open_path(port: &std::path::Path) -> Result<Bridge> {
        // SAFETY: standard libc open + termios setup on a character device.
        let fd: RawFd = unsafe {
            let cpath = std::ffi::CString::new(port.as_os_str().as_bytes())?;
            let fd = libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
            if fd < 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| format!("opening {}", port.display()));
            }
            configure_tty(fd).with_context(|| format!("configuring {}", port.display()))?;
            fd
        };
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let mut b = Bridge { file, buf: String::new() };
        // Drain any backlog (the firmware streams TOUCH lines when a mouse is on).
        let _ = b.drain(Duration::from_millis(150));
        Ok(b)
    }

    /// Send one command line (newline-terminated).
    pub fn send(&mut self, cmd: &str) -> Result<()> {
        self.file.write_all(cmd.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        Ok(())
    }

    /// Read the next newline-delimited line, waiting up to `deadline`.
    pub fn read_line(&mut self, deadline: Instant) -> Result<Option<String>> {
        loop {
            if let Some(i) = self.buf.find('\n') {
                let line: String = self.buf.drain(..=i).collect();
                return Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            let mut tmp = [0u8; 256];
            match self.file.read(&mut tmp) {
                Ok(0) => std::thread::sleep(Duration::from_millis(10)),
                Ok(n) => self.buf.push_str(&String::from_utf8_lossy(&tmp[..n])),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Read lines until `pred` returns Some, or the timeout elapses.
    fn read_until<T>(&mut self, timeout: Duration, mut pred: impl FnMut(&str) -> Option<T>) -> Result<Option<T>> {
        let deadline = Instant::now() + timeout;
        while let Some(line) = self.read_line(deadline)? {
            if let Some(v) = pred(&line) {
                return Ok(Some(v));
            }
        }
        Ok(None)
    }

    fn drain(&mut self, dur: Duration) -> Result<()> {
        let deadline = Instant::now() + dur;
        while self.read_line(deadline)?.is_some() {}
        self.buf.clear();
        Ok(())
    }
}

/// Configure an open fd as raw 115200 8N1 with DTR/RTS asserted and 0.2s reads.
unsafe fn configure_tty(fd: RawFd) -> Result<()> {
    let mut tio: libc::termios = std::mem::zeroed();
    if libc::tcgetattr(fd, &mut tio) != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    libc::cfmakeraw(&mut tio);
    libc::cfsetispeed(&mut tio, libc::B115200);
    libc::cfsetospeed(&mut tio, libc::B115200);
    tio.c_cflag |= libc::CLOCAL | libc::CREAD;
    tio.c_cc[libc::VMIN] = 0;
    tio.c_cc[libc::VTIME] = 2; // 0.2s read timeout
    if libc::tcsetattr(fd, libc::TCSANOW, &tio) != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // Assert DTR + RTS, or the firmware's USB-CDC never reports "connected" and
    // host->device writes stall. pyserial does this by default; we must too.
    let mut status: libc::c_int = 0;
    if libc::ioctl(fd, libc::TIOCMGET, &mut status) == 0 {
        status |= libc::TIOCM_DTR | libc::TIOCM_RTS;
        libc::ioctl(fd, libc::TIOCMSET, &status);
    }
    Ok(())
}

// ---- status ---------------------------------------------------------------

/// Parsed `STATUS ver=.. mode=.. connected=.. local=.. mouse=..` line.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub version: String,
    pub mode: String,
    pub connected: bool,
    pub local: String,
    pub mouse: String,
}

fn parse_status(line: &str) -> Option<Status> {
    let rest = line.strip_prefix("STATUS ")?;
    let mut s = Status::default();
    for tok in rest.split_whitespace() {
        let Some((k, v)) = tok.split_once('=') else { continue };
        match k {
            "ver" => s.version = v.to_string(),
            "mode" => s.mode = v.to_string(),
            "connected" => s.connected = v == "1",
            "local" => s.local = v.to_string(),
            "mouse" => s.mouse = v.to_string(),
            _ => {}
        }
    }
    Some(s)
}

/// Query the card's status (also the presence probe).
pub fn status() -> Result<Status> {
    let mut b = Bridge::open()?;
    b.send("status")?;
    b.read_until(Duration::from_secs(2), parse_status)?
        .context("no STATUS reply from the bridge")
}

/// `bridge-status`: print the STATUS as JSON (what the app polls to decide
/// whether a card is present and what it is connected to).
pub fn status_cmd() -> Result<()> {
    match status() {
        Ok(s) => {
            println!(
                "{}",
                serde_json::json!({
                    "present": true,
                    "version": s.version,
                    "mode": s.mode,
                    "connected": s.connected,
                    "local": s.local,
                    "mouse": s.mouse,
                })
            );
            Ok(())
        }
        Err(_) => {
            println!("{}", serde_json::json!({ "present": false, "bootsel": bootsel_present() }));
            Ok(())
        }
    }
}

// ---- scan -----------------------------------------------------------------

/// A mouse the card found over Bluetooth. Shaped like `bluetooth::BtDevice::json`
/// so the settings app's device list renders it identically.
fn dev_json(addr: &str, name: &str, rssi: i32, connected_mouse: &str) -> String {
    let connected = addr.eq_ignore_ascii_case(connected_mouse);
    serde_json::json!({
        "addr": addr,
        "name": if name == "-" { "" } else { name },
        "paired": connected,
        "bonded": connected,
        "trusted": connected,
        "connected": connected,
        "rssi": rssi,
        "state": if connected { "ok" } else { "unpaired" },
        "via": "bridge",
    })
    .to_string()
}

/// `bridge-scan [SECONDS]`: ask the card to inquire, print one JSON per mouse.
pub fn scan_cmd(_seconds: u32) -> Result<()> {
    let mut b = Bridge::open()?;
    // Know which mouse (if any) is already connected, to mark it in the list.
    b.send("status")?;
    let connected_mouse = b
        .read_until(Duration::from_secs(2), parse_status)?
        .map(|s| s.mouse)
        .unwrap_or_default();

    b.send("scan")?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while let Some(line) = b.read_line(deadline)? {
        if line == "SCAN_DONE" {
            break;
        }
        if let Some(rest) = line.strip_prefix("DEV ") {
            // DEV <addr> <cod:06x> <rssi> <name...>
            let mut it = rest.splitn(4, ' ');
            let (Some(addr), Some(_cod), Some(rssi), name) = (it.next(), it.next(), it.next(), it.next()) else {
                continue;
            };
            // Only list devices that look like a mouse (class of device 0x05 peripheral,
            // or an Apple name), to match the native scan's filtering.
            let rssi: i32 = rssi.parse().unwrap_or(0);
            let name = name.unwrap_or("-");
            println!("{}", dev_json(addr, name, rssi, &connected_mouse));
        }
    }
    Ok(())
}

// ---- pair -----------------------------------------------------------------

/// `bridge-pair ADDR [TIMEOUT_S]`: tell the card to pair + enable multitouch.
/// Emits the same `step:`/`hint:`/`ok:` progress the native bt-pair does, so the
/// settings app's PairDialog shows it unchanged.
pub fn pair(addr: &str, timeout: Duration) -> Result<()> {
    let mut b = Bridge::open()?;
    println!("step: asking the bridge to connect to {addr}");
    b.send(&format!("pair {addr}"))?;
    println!("hint: keep the mouse switched on; switch it off and on if it does not connect");

    let deadline = Instant::now() + timeout;
    let mut connected = false;
    while let Some(line) = b.read_line(deadline)? {
        if let Some(rest) = line.strip_prefix("ERR ") {
            bail!("bridge error: {rest}");
        } else if line.starts_with("CONNECTING") {
            println!("step: connecting over Bluetooth");
        } else if line.starts_with("CONNECTED") {
            connected = true;
            println!("step: connected, enabling multitouch");
        } else if line == "MT_ON" {
            println!("ok: paired and multitouch enabled");
            return Ok(());
        }
    }
    if connected {
        // Connected but the MT_ON confirmation did not arrive in time; the mouse
        // still works, so treat it as success with a note.
        println!("ok: connected (multitouch confirmation timed out)");
        return Ok(());
    }
    bail!("timed out waiting for the bridge to connect to {addr}");
}

// ---- mode / profile -------------------------------------------------------

/// `bridge-mode ptp|raw`.
pub fn mode_cmd(mode: &str) -> Result<()> {
    if mode != "ptp" && mode != "raw" {
        bail!("mode must be 'ptp' or 'raw'");
    }
    let mut b = Bridge::open()?;
    b.send(&format!("mode {mode}"))?;
    match b.read_until(Duration::from_secs(2), |l| l.strip_prefix("MODE ").map(str::to_string))? {
        Some(m) => {
            println!("ok: mode {m}");
            Ok(())
        }
        None => bail!("no MODE reply from the bridge"),
    }
}

/// Set the card's forwarding mode on an already-open connection (used by the
/// daemon when it takes over a bridge in `raw` mode).
pub fn set_mode(b: &mut Bridge, mode: &str) -> Result<()> {
    b.send(&format!("mode {mode}"))?;
    b.read_until(Duration::from_secs(2), |l| l.strip_prefix("MODE ").map(|_| ()))?
        .context("no MODE reply")?;
    Ok(())
}

/// `bridge-profile-get ADDR`: print the stored 12-byte calibration as hex, or
/// `none`. The host keeps the gesture config; this is the card-side blob.
pub fn profile_get_cmd(addr: &str) -> Result<()> {
    let mut b = Bridge::open()?;
    b.send(&format!("profile get {addr}"))?;
    let want = addr.to_ascii_uppercase();
    let line = b
        .read_until(Duration::from_secs(2), |l| {
            l.strip_prefix("PROFILE ").filter(|r| r.to_ascii_uppercase().starts_with(&want)).map(str::to_string)
        })?
        .context("no PROFILE reply")?;
    // "<addr> <hex|none>"
    let blob = line.split_whitespace().nth(1).unwrap_or("none");
    println!("{}", serde_json::json!({ "addr": addr, "blob": blob }));
    Ok(())
}

/// `bridge-profile-set ADDR HEX`: store the card-side calibration blob.
pub fn profile_set_cmd(addr: &str, hex: &str) -> Result<()> {
    let mut b = Bridge::open()?;
    b.send(&format!("profile set {addr} {hex}"))?;
    match b.read_until(Duration::from_secs(2), |l| {
        if let Some(rest) = l.strip_prefix("PROFILE_OK ") {
            Some(Ok(rest.to_string()))
        } else if let Some(rest) = l.strip_prefix("ERR ") {
            Some(Err(rest.to_string()))
        } else {
            None
        }
    })? {
        Some(Ok(_)) => {
            println!("ok: profile stored for {addr}");
            Ok(())
        }
        Some(Err(e)) => bail!("bridge error: {e}"),
        None => bail!("no reply from the bridge"),
    }
}
