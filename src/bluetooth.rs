//! Bluetooth pairing repair for the Magic Mouse.
//!
//! When the mouse is paired with a Mac and comes back, BlueZ is often left
//! with a stale record (`Paired: yes`, `Bonded: no`): the mouse holds the
//! key it made with the Mac, bluetoothd rejects its HID connection as
//! "!bonded", and no input device ever appears. Forgetting the mouse in the
//! desktop's Bluetooth panel does not always clear that record. The fix is
//! always the same: remove the device, let it advertise again (switch it off
//! and on), pair, trust, connect. `mousetouch bt-check` reports the state and
//! `mousetouch bt-repair` runs the fix by name. `mousetouch bt-scan` lists
//! the mice that are switched on and not connected (as JSON lines) and
//! `mousetouch bt-pair ADDR` runs the same fix for one of them; the settings
//! app's "Nearby mice" section is built on those two.
//!
//! Everything goes through `bluetoothctl` (shipped with BlueZ), which also
//! supplies the pairing agent.

use anyhow::{anyhow, bail, Context, Result};
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
pub struct BtDevice {
    pub addr: String,
    pub name: String,
    pub paired: bool,
    pub bonded: bool,
    pub trusted: bool,
    pub connected: bool,
    /// Bluetooth class of device (`Class: 0x00002580`), when BlueZ has it.
    pub class: Option<u32>,
    /// BlueZ's icon name (`input-mouse`), when it has one.
    pub icon: Option<String>,
    /// `bluetooth:v004Cp0323d0314` once the device identified itself.
    pub modalias: Option<String>,
    /// Signal strength: present only while the device is advertising.
    pub rssi: Option<i32>,
}

impl BtDevice {
    /// Paired without a bond: bluetoothd will reject the mouse's connection.
    pub fn stale(&self) -> bool {
        self.paired && !self.bonded
    }

    /// A pointing device: BlueZ's icon, the class of device (major
    /// peripheral, minor pointing) or an Apple mouse modalias.
    pub fn is_mouse(&self) -> bool {
        if self.icon.as_deref() == Some("input-mouse") {
            return true;
        }
        if let Some(c) = self.class {
            if (c & 0x1f00) == 0x0500 && (c & 0x0080) != 0 {
                return true;
            }
        }
        self.modalias.as_deref().is_some_and(|m| {
            let m = m.to_ascii_lowercase();
            (m.contains("v004c") || m.contains("v05ac")) && ["p030d", "p0269", "p0323"].iter().any(|p| m.contains(p))
        })
    }

    /// Switched on and reachable, but not usable yet: advertising or still
    /// on record, and either not connected or connected without a bond.
    pub fn ready_to_connect(&self) -> bool {
        (self.rssi.is_some() || self.paired) && (!self.connected || self.stale())
    }

    /// `unpaired`, `stale` or `disconnected`, for scripts and the UI.
    pub fn state(&self) -> &'static str {
        if self.stale() {
            "stale"
        } else if self.paired {
            "disconnected"
        } else {
            "unpaired"
        }
    }

    fn json(&self) -> String {
        serde_json::json!({
            "addr": self.addr,
            "name": self.name,
            "paired": self.paired,
            "bonded": self.bonded,
            "trusted": self.trusted,
            "connected": self.connected,
            "rssi": self.rssi,
            "state": self.state(),
        })
        .to_string()
    }
}

/// Run bluetoothctl non-interactively and return its output without the
/// ANSI colour codes it prints even when not on a terminal.
fn bluetoothctl(args: &[&str]) -> Result<String> {
    let out = Command::new("bluetoothctl")
        .args(args)
        .output()
        .context("running bluetoothctl (is BlueZ installed?)")?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(strip_ansi(&text))
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // ESC [ ... final byte in 0x40..=0x7e
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&d) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// `bluetoothctl devices` -> (address, name).
pub fn devices() -> Result<Vec<(String, String)>> {
    let text = bluetoothctl(&["devices"])?;
    Ok(text
        .lines()
        .filter_map(|l| {
            let rest = l.trim().strip_prefix("Device ")?;
            let (addr, name) = rest.split_once(' ').unwrap_or((rest, ""));
            Some((addr.to_string(), name.trim().to_string()))
        })
        .collect())
}

pub fn info(addr: &str) -> Result<BtDevice> {
    let text = bluetoothctl(&["info", addr])?;
    if text.contains("not available") {
        bail!("device {addr} not known to BlueZ");
    }
    let mut d = BtDevice { addr: addr.to_string(), ..Default::default() };
    for line in text.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match key.trim() {
            "Name" => d.name = value.to_string(),
            "Alias" if d.name.is_empty() => d.name = value.to_string(),
            "Paired" => d.paired = value == "yes",
            "Bonded" => d.bonded = value == "yes",
            "Trusted" => d.trusted = value == "yes",
            "Connected" => d.connected = value == "yes",
            "Class" => d.class = value.split_whitespace().next().and_then(|h| u32::from_str_radix(h.trim_start_matches("0x"), 16).ok()),
            "Icon" => d.icon = Some(value.to_string()),
            "Modalias" => d.modalias = Some(value.to_string()),
            // `RSSI: 0xffffffd8 (-40)`: the decimal is in the parentheses.
            "RSSI" => d.rssi = value.rsplit('(').next().and_then(|v| v.trim_end_matches(')').trim().parse().ok()),
            _ => {}
        }
    }
    Ok(d)
}

/// Discover for `seconds`, then return every mouse that is switched on and
/// not connected (or connected without a bond), strongest signal first.
pub fn scan(seconds: u32) -> Result<Vec<BtDevice>> {
    bluetoothctl(&["--timeout", &seconds.to_string(), "scan", "on"])?;
    let mut out: Vec<BtDevice> = devices()?
        .into_iter()
        .filter_map(|(addr, _)| info(&addr).ok())
        .filter(|d| d.is_mouse() && d.ready_to_connect())
        .collect();
    out.sort_by_key(|d| std::cmp::Reverse(d.rssi.unwrap_or(i32::MIN)));
    Ok(out)
}

/// `bt-scan [SECONDS]`: one JSON object per line for each nearby mouse.
pub fn scan_cmd(seconds: u32) -> Result<()> {
    for d in scan(seconds)? {
        println!("{}", d.json());
    }
    Ok(())
}

/// Known devices whose name contains `name_match` (case-insensitive).
pub fn find(name_match: &str) -> Result<Vec<BtDevice>> {
    let needle = name_match.to_ascii_lowercase();
    let mut out = Vec::new();
    for (addr, name) in devices()? {
        if name.to_ascii_lowercase().contains(&needle) {
            out.push(info(&addr)?);
        }
    }
    Ok(out)
}

/// `bt-check NAME`: one line per matching device. Exit status: 0 = paired
/// and bonded, 3 = stale pairing (needs `bt-repair`), 4 = not paired or
/// not known.
pub fn check(name_match: &str) -> Result<()> {
    let devs = find(name_match)?;
    if devs.is_empty() {
        println!("none: no Bluetooth device named like \"{name_match}\"");
        std::process::exit(4);
    }
    let mut status = 0;
    for d in &devs {
        let state = if d.stale() {
            status = status.max(3);
            "stale"
        } else if d.paired && d.bonded {
            "ok"
        } else {
            status = status.max(4);
            "unpaired"
        };
        println!(
            "{state}: {} {} paired={} bonded={} trusted={} connected={}",
            d.addr, d.name, d.paired, d.bonded, d.trusted, d.connected
        );
    }
    std::process::exit(status);
}

fn remove(d: &BtDevice) -> Result<()> {
    println!("step: removing {} ({})", d.name, d.addr);
    let out = bluetoothctl(&["remove", &d.addr])?;
    if !out.contains("removed") {
        bail!("could not remove {}: {}", d.addr, out.trim());
    }
    Ok(())
}

/// `bt-repair NAME [TIMEOUT_S]`: remove every matching device, wait for the
/// mouse to advertise again, then pair, trust and connect. Progress goes to
/// stdout one line at a time (`step:`, `hint:`, `ok:`), which the settings
/// app shows as it happens.
pub fn repair(name_match: &str, timeout: Duration) -> Result<()> {
    let known = find(name_match)?;
    for d in &known {
        remove(d)?;
    }
    if known.is_empty() {
        println!("step: nothing to remove");
    }

    println!("hint: switch the mouse off and on so it advertises again");
    let needle = name_match.to_ascii_lowercase();
    let addr = wait_for(timeout, &format!("\"{name_match}\""), |(_, n)| n.to_ascii_lowercase().contains(&needle))?;
    pair_trust_connect(&addr)
}

/// `bt-pair ADDR [TIMEOUT_S]`: the same fix for one known address (what the
/// settings app runs for a mouse picked from "Nearby mice"): drop whatever
/// record BlueZ has, wait for the mouse, pair, trust, connect.
pub fn pair(addr: &str, timeout: Duration) -> Result<()> {
    match info(addr) {
        Ok(d) if d.paired || d.connected => remove(&d)?,
        Ok(_) => {
            // Only a discovery entry: removing it too makes the pairing start
            // clean, and the mouse is still advertising.
            println!("step: not paired before");
            let _ = bluetoothctl(&["remove", addr]);
        }
        Err(_) => println!("step: not known to BlueZ yet"),
    }
    println!("hint: keep the mouse switched on; switch it off and on if it does not show up");
    let wanted = addr.to_ascii_uppercase();
    let addr = wait_for(timeout, addr, |(a, _)| a.eq_ignore_ascii_case(&wanted))?;
    pair_trust_connect(&addr)
}

/// Scan in 6 s rounds until a device matching `pick` is listed, or `timeout`.
fn wait_for(timeout: Duration, what: &str, pick: impl Fn(&(String, String)) -> bool) -> Result<String> {
    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            bail!("the mouse did not show up within {} s; switch it off and on and run again", timeout.as_secs());
        }
        println!("step: scanning for {what} ...");
        bluetoothctl(&["--timeout", "6", "scan", "on"])?;
        if let Some((addr, name)) = devices()?.into_iter().find(|d| pick(d)) {
            println!("step: found {name} ({addr})");
            return Ok(addr);
        }
    }
}

fn pair_trust_connect(addr: &str) -> Result<()> {
    let addr = addr.to_string();
    let mut last_err = String::new();
    for attempt in 1..=3 {
        println!("step: pairing (attempt {attempt})");
        let out = bluetoothctl(&["pair", &addr])?;
        if out.contains("Pairing successful") || out.contains("Already paired") {
            last_err.clear();
            break;
        }
        last_err = out.lines().filter(|l| l.contains("Failed") || l.contains("Error")).last().unwrap_or("pairing failed").trim().to_string();
        std::thread::sleep(Duration::from_secs(2));
    }
    if !last_err.is_empty() {
        bail!("{last_err}");
    }
    println!("step: trusting");
    bluetoothctl(&["trust", &addr])?;
    println!("step: connecting");
    let out = bluetoothctl(&["connect", &addr])?;
    if !out.contains("Connection successful") {
        // The mouse often connects by itself right after pairing.
        std::thread::sleep(Duration::from_secs(2));
    }
    let d = info(&addr)?;
    if !d.bonded {
        return Err(anyhow!("paired but still not bonded; remove the mouse on the other computer too and try again"));
    }
    println!("ok: {} paired={} bonded={} trusted={} connected={}", d.name, d.paired, d.bonded, d.trusted, d.connected);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_colour_codes() {
        assert_eq!(strip_ansi("[\x1b[0;93mCHG\x1b[0m] Device AA Paired: yes"), "[CHG] Device AA Paired: yes");
    }

    fn parse(text: &str) -> BtDevice {
        let mut d = BtDevice::default();
        for line in text.lines() {
            let line = line.trim();
            let Some((key, value)) = line.split_once(':') else { continue };
            let value = value.trim();
            match key.trim() {
                "Name" => d.name = value.to_string(),
                "Paired" => d.paired = value == "yes",
                "Bonded" => d.bonded = value == "yes",
                "Connected" => d.connected = value == "yes",
                "Class" => d.class = value.split_whitespace().next().and_then(|h| u32::from_str_radix(h.trim_start_matches("0x"), 16).ok()),
                "Icon" => d.icon = Some(value.to_string()),
                "Modalias" => d.modalias = Some(value.to_string()),
                "RSSI" => d.rssi = value.rsplit('(').next().and_then(|v| v.trim_end_matches(')').trim().parse().ok()),
                _ => {}
            }
        }
        d
    }

    #[test]
    fn a_mouse_back_from_a_mac_is_a_stale_nearby_mouse() {
        // What `bluetoothctl info` printed for Enrique's renamed USB-C mouse
        // after pairing from the desktop panel: paired, not bonded, and
        // "connected" for the instant before bluetoothd rejects it.
        let d = parse(
            "\tName: Black  raton\n\tClass: 0x00002580 (9600)\n\tIcon: input-mouse\n\tPaired: yes\n\tBonded: no\n\tTrusted: no\n\tConnected: yes\n\tModalias: bluetooth:v004Cp0323d0314\n\tRSSI: 0xffffffd8 (-40)\n",
        );
        assert!(d.is_mouse());
        assert!(d.stale());
        assert!(d.ready_to_connect(), "connected without a bond still needs the fix");
        assert_eq!(d.state(), "stale");
        assert_eq!(d.rssi, Some(-40));
        assert_eq!(d.class, Some(0x2580));
        assert!(d.json().contains("\"state\":\"stale\""));
    }

    #[test]
    fn mice_are_told_apart_from_other_devices() {
        let speaker = parse("\tName: LE-Bocinita\n\tClass: 0x00240414 (2360340)\n\tIcon: audio-headset\n\tPaired: no\n\tRSSI: 0xffffffc0 (-64)\n");
        assert!(!speaker.is_mouse());
        // Class only (no icon yet): peripheral + pointing bit.
        let by_class = parse("\tName: Mouse\n\tClass: 0x00000580 (1408)\n\tPaired: no\n\tRSSI: 0xffffffd0 (-48)\n");
        assert!(by_class.is_mouse() && by_class.ready_to_connect() && by_class.state() == "unpaired");
        // Apple modalias alone (class missing on some adapters).
        let by_modalias = parse("\tName: X\n\tModalias: bluetooth:v004Cp0269d0100\n\tPaired: yes\n\tBonded: yes\n\tConnected: no\n");
        assert!(by_modalias.is_mouse() && by_modalias.ready_to_connect() && by_modalias.state() == "disconnected");
        // A healthy connected mouse is not "nearby": nothing to do.
        let fine = parse("\tName: Magic Mouse\n\tIcon: input-mouse\n\tPaired: yes\n\tBonded: yes\n\tConnected: yes\n");
        assert!(fine.is_mouse() && !fine.ready_to_connect());
        // A mouse BlueZ remembers but that is switched off (no RSSI, paired)
        // is listed as disconnected; an unpaired one that stopped
        // advertising is not.
        let off = parse("\tName: Magic Mouse\n\tIcon: input-mouse\n\tPaired: yes\n\tBonded: yes\n\tConnected: no\n");
        assert!(off.ready_to_connect());
        let gone = parse("\tName: Mouse\n\tIcon: input-mouse\n\tPaired: no\n\tConnected: no\n");
        assert!(!gone.ready_to_connect());
    }
}
