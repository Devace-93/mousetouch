//! Finding the mouse under `/dev/input`.
//!
//! A Magic Mouse is recognised by its vendor/product ids (Apple over
//! Bluetooth or USB; Magic Mouse, Magic Mouse 2, Magic Mouse 2 USB-C), so a
//! mouse renamed from macOS is still found; the config's `device_match` adds
//! a name match on top for anything else. Nodes that look like the mouse but
//! cannot be used are reported instead of skipped silently: a node the user
//! may not open (udev rule missing or not yet applied) and a node without
//! multitouch axes (the mouse is bound to `hid-generic`, typically because an
//! old DKMS `hid-magicmouse` shadows the in-tree driver).

use crate::virt::VIRTUAL_SUFFIX;
use evdev::{AbsoluteAxisCode, Device};
use std::path::{Path, PathBuf};

/// Apple's vendor id over Bluetooth and over USB.
const APPLE_VENDORS: [u16; 2] = [0x004c, 0x05ac];
/// Magic Mouse (030d), Magic Mouse 2 (0269), Magic Mouse 2 USB-C (0323).
const MAGIC_MOUSE_PRODUCTS: [u16; 3] = [0x030d, 0x0269, 0x0323];

pub fn is_magic_mouse(vendor: u16, product: u16) -> bool {
    APPLE_VENDORS.contains(&vendor) && MAGIC_MOUSE_PRODUCTS.contains(&product)
}

#[derive(Debug)]
pub enum Problem {
    /// The node cannot be opened by this user.
    Denied,
    /// Opened fine, but the driver exposes no `ABS_MT_*` axes.
    NoTouchAxes,
}

/// A node that looks like the mouse but cannot be used, with why.
#[derive(Debug)]
pub struct Unusable {
    pub path: PathBuf,
    pub name: String,
    pub vendor: u16,
    pub product: u16,
    pub problem: Problem,
}

impl Unusable {
    /// One line for the log or `probe`, with what to do about it.
    pub fn describe(&self) -> String {
        let who = format!("{} ({}, {:04x}:{:04x})", self.path.display(), self.name, self.vendor, self.product);
        match self.problem {
            Problem::Denied => format!(
                "{who}: permission denied; install the udev rule (packaging/60-mousetouch.rules), then re-pair the mouse or run `sudo udevadm trigger`"
            ),
            Problem::NoTouchAxes => format!(
                "{who}: no multitouch axes, so it is not bound to hid-magicmouse (hid-generic?); an old DKMS hid-magicmouse shadows the in-tree driver, see the README"
            ),
        }
    }
}

#[derive(Debug, Default)]
pub struct Scan {
    /// Usable devices, sorted by path.
    pub devices: Vec<(PathBuf, Device)>,
    pub unusable: Vec<Unusable>,
}

/// Identity of an event node read from sysfs, which needs no permission on
/// the node itself.
struct SysfsId {
    name: String,
    vendor: u16,
    product: u16,
}

fn sysfs_id(event: &str) -> Option<SysfsId> {
    let base = PathBuf::from("/sys/class/input").join(event).join("device");
    let name = std::fs::read_to_string(base.join("name")).ok()?.trim().to_string();
    let vendor = parse_hex(&std::fs::read_to_string(base.join("id/vendor")).ok()?)?;
    let product = parse_hex(&std::fs::read_to_string(base.join("id/product")).ok()?)?;
    Some(SysfsId { name, vendor, product })
}

fn parse_hex(s: &str) -> Option<u16> {
    u16::from_str_radix(s.trim(), 16).ok()
}

fn has_touch_axes(d: &Device) -> bool {
    d.supported_absolute_axes().map_or(false, |a| a.contains(AbsoluteAxisCode::ABS_MT_POSITION_X))
}

/// Look at every `/dev/input/event*`; keep the ones that are a Magic Mouse
/// by id or whose name contains `device_match`, and say why a candidate
/// could not be used.
pub fn scan(device_match: &str) -> Scan {
    let mut out = Scan::default();
    let Ok(entries) = std::fs::read_dir("/dev/input") else { return out };
    let mut events: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            name.starts_with("event").then(|| (name, e.path()))
        })
        .collect();
    events.sort();
    for (event, path) in events {
        match sysfs_id(&event) {
            Some(id) => {
                if !is_candidate(&id.name, id.vendor, id.product, device_match) {
                    continue;
                }
                match judge(path, id, Device::open(&path_for_open(&event))) {
                    Ok(dev) => out.devices.push(dev),
                    Err(Some(u)) => out.unusable.push(u),
                    Err(None) => {}
                }
            }
            // No sysfs (containers, odd setups): judge the node by itself.
            None => match Device::open(&path) {
                Ok(d) => {
                    let name = d.name().unwrap_or("").to_string();
                    let id = d.input_id();
                    if is_candidate(&name, id.vendor(), id.product(), device_match) && has_touch_axes(&d) {
                        out.devices.push((path, d));
                    }
                }
                Err(e) => log::debug!("{}: {e}", path.display()),
            },
        }
    }
    out.devices.sort_by_key(|(p, _)| event_number(p));
    out.unusable.sort_by_key(|u| event_number(&u.path));
    out
}

fn path_for_open(event: &str) -> PathBuf {
    PathBuf::from("/dev/input").join(event)
}

/// `/dev/input/event10` sorts after `event8`.
fn event_number(path: &Path) -> u32 {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("event"))
        .and_then(|n| n.parse().ok())
        .unwrap_or(u32::MAX)
}

/// Decide what an opened (or not) candidate node is: usable, unusable for a
/// reason worth telling the user, or (`Err(None)`) not worth mentioning.
fn judge(path: PathBuf, id: SysfsId, opened: std::io::Result<Device>) -> Result<(PathBuf, Device), Option<Unusable>> {
    match opened {
        Ok(d) if has_touch_axes(&d) => Ok((path, d)),
        Ok(_) => Err(Some(Unusable { path, name: id.name, vendor: id.vendor, product: id.product, problem: Problem::NoTouchAxes })),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(Some(Unusable { path, name: id.name, vendor: id.vendor, product: id.product, problem: Problem::Denied }))
        }
        Err(e) => {
            log::debug!("{}: {e}", path.display());
            Err(None)
        }
    }
}

fn is_candidate(name: &str, vendor: u16, product: u16, device_match: &str) -> bool {
    if name.contains(VIRTUAL_SUFFIX) {
        return false;
    }
    is_magic_mouse(vendor, product) || (!device_match.is_empty() && name.contains(device_match))
}

/// The file the kernel exposes the node's identity in, for callers that want
/// to show it (`probe`).
#[allow(dead_code)]
pub fn sysfs_dir(path: &Path) -> Option<PathBuf> {
    let event = path.file_name()?.to_str()?;
    Some(PathBuf::from("/sys/class/input").join(event).join("device"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_mice_by_id() {
        assert!(is_magic_mouse(0x004c, 0x0269));
        assert!(is_magic_mouse(0x05ac, 0x0323));
        assert!(is_magic_mouse(0x004c, 0x030d));
        assert!(!is_magic_mouse(0x004c, 0x0265), "Magic Trackpad 2 is not a mouse");
        assert!(!is_magic_mouse(0x046d, 0x0269));
    }

    #[test]
    fn candidates_by_id_or_name_but_never_our_own_virtual_devices() {
        assert!(is_candidate("Enrique's Mouse", 0x004c, 0x0269, "Magic Mouse"), "renamed mouse found by id");
        assert!(is_candidate("Fake Magic Mouse", 0x1234, 0x0001, "Magic Mouse"), "name match still works");
        assert!(!is_candidate("Logitech M720", 0x046d, 0xb015, "Magic Mouse"));
        assert!(!is_candidate("Magic Mouse (mousetouch)", 0x004c, 0x0269, "Magic Mouse"), "our virtual mouse");
        assert!(!is_candidate("Anything", 0x1234, 0x0001, ""));
    }

    #[test]
    fn a_node_we_may_not_open_is_reported_with_a_hint() {
        let id = SysfsId { name: "Enrique's Mouse".into(), vendor: 0x004c, product: 0x0269 };
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        match judge(PathBuf::from("/dev/input/event10"), id, Err(denied)) {
            Err(Some(u)) => {
                assert!(matches!(u.problem, Problem::Denied));
                let text = u.describe();
                assert!(text.contains("event10") && text.contains("Enrique's Mouse") && text.contains("004c:0269"), "{text}");
                assert!(text.contains("udev"), "{text}");
            }
            other => panic!("expected a denied report, got {other:?}"),
        }
        // A vanished node (ENOENT between listing and opening) is not news.
        let id = SysfsId { name: "Magic Mouse".into(), vendor: 0x004c, product: 0x0323 };
        assert!(matches!(judge(PathBuf::from("/dev/input/event9"), id, Err(std::io::Error::from(std::io::ErrorKind::NotFound))), Err(None)));
    }

    #[test]
    fn event_nodes_sort_numerically() {
        let mut v = vec![PathBuf::from("/dev/input/event10"), PathBuf::from("/dev/input/event8"), PathBuf::from("/dev/input/event9")];
        v.sort_by_key(|p| event_number(p));
        assert_eq!(v[0].file_name().unwrap(), "event8");
        assert_eq!(v[2].file_name().unwrap(), "event10");
    }

    #[test]
    fn sysfs_hex() {
        assert_eq!(parse_hex("004c\n"), Some(0x4c));
        assert_eq!(parse_hex("0323"), Some(0x323));
        assert_eq!(parse_hex("zz"), None);
    }
}
