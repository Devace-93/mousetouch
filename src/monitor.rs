//! Live event stream for the settings app's touch preview.
//!
//! The daemon grabs the mouse, so nothing else can watch the fingers; it
//! therefore republishes what it sees on a Unix socket
//! (`$XDG_RUNTIME_DIR/mousetouch.sock`) as newline-delimited JSON. Any number
//! of clients may connect; the settings app draws the mouse from it and
//! `mousetouch events` prints it. Nothing is serialised while no client is
//! connected, and a client that stops reading is dropped rather than letting
//! it stall the input thread.

use serde::Serialize;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// One finger on the surface, in millimetres from the top-left of the touch
/// area (`x0`, `y0` = where it landed).
#[derive(Debug, Clone, Serialize)]
pub struct Finger {
    pub id: i32,
    pub x: f64,
    pub y: f64,
    pub x0: f64,
    pub y0: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Event {
    /// First line every client gets: which daemon it is talking to.
    Hello { version: &'static str },
    /// A mouse was attached (also replayed to every new client). Every event
    /// of a mouse carries `dev`, a small number identifying it while it stays
    /// attached, so a client can tell two mice apart.
    Device { name: String, w_mm: f64, h_mm: f64, source: &'static str },
    /// The mouse went away.
    DeviceLost,
    /// The complete set of contacts right now, plus what the engine makes of
    /// them. `state`: idle | armed | workspace | pinch | done. `pad_dx/dy` is
    /// the travel forwarded to the virtual touchpad so far (mm); `scale` the
    /// pinch ratio replayed.
    Frame {
        ms: u64,
        fingers: Vec<Finger>,
        state: &'static str,
        max_fingers: usize,
        travel_mm: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        pad_dx: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pad_dy: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        scale: Option<f64>,
    },
    /// A continuous gesture started being replayed on the virtual touchpad.
    GestureBegin { ms: u64, gesture: &'static str, fingers: usize, #[serde(skip_serializing_if = "Option::is_none")] dir: Option<&'static str> },
    GestureEnd { ms: u64, gesture: &'static str },
    /// A discrete gesture was recognised; `matched` says whether a rule ran.
    Rule {
        ms: u64,
        kind: &'static str,
        fingers: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        dir: Option<&'static str>,
        matched: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        action: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        app: Option<String>,
    },
    /// All fingers lifted; the verdict on whether that touch was a tap.
    TouchEnd {
        ms: u64,
        fingers: usize,
        duration_ms: u64,
        travel_mm: f64,
        lift_speed: f64,
        tap: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<&'static str>,
    },
    /// Wheel movement released to the virtual mouse, in high-resolution units
    /// (120 = one notch). `dx` is the horizontal wheel.
    Scroll { ms: u64, dx: i32, dy: i32 },
    /// A physical button went down or up.
    Button { ms: u64, button: &'static str, down: bool },
}

struct Inner {
    clients: Mutex<Vec<UnixStream>>,
    has_clients: AtomicBool,
    /// The `Device` line of every attached mouse, by `dev`, replayed to
    /// clients that connect later.
    devices: Mutex<Vec<(u32, String)>>,
    epoch: Instant,
}

/// Cheap handle; cloning shares the client list. `for_device` gives the
/// handle a device number that is stamped on everything it sends.
#[derive(Clone)]
pub struct Monitor {
    inner: Arc<Inner>,
    dev: u32,
}

pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute()) {
        Some(dir) => dir.join("mousetouch.sock"),
        None => PathBuf::from(format!("/tmp/mousetouch-{}.sock", unsafe { libc::getuid() })),
    }
}

impl Monitor {
    fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                clients: Mutex::new(Vec::new()),
                has_clients: AtomicBool::new(false),
                devices: Mutex::new(Vec::new()),
                epoch: Instant::now(),
            }),
            dev: 0,
        }
    }

    /// A handle for one attached mouse: its events carry `"dev": id`.
    pub fn for_device(&self, id: u32) -> Self {
        Self { inner: Arc::clone(&self.inner), dev: id }
    }

    /// A monitor nobody can connect to (tests, one-off commands).
    #[allow(dead_code)]
    pub fn disabled() -> Self {
        Self::new()
    }

    /// Bind the socket and accept clients on a background thread. Failing
    /// to bind only disables the stream; the daemon keeps running.
    pub fn start() -> Self {
        Self::start_at(&socket_path())
    }

    pub fn start_at(path: &std::path::Path) -> Self {
        let m = Self::new();
        let _ = std::fs::remove_file(path);
        match UnixListener::bind(path) {
            Ok(listener) => {
                log::info!("event stream on {}", path.display());
                let inner = Arc::clone(&m.inner);
                std::thread::Builder::new()
                    .name("monitor-accept".into())
                    .spawn(move || {
                        for stream in listener.incoming().flatten() {
                            if stream.set_nonblocking(true).is_err() {
                                continue;
                            }
                            let mut stream = stream;
                            if let Ok(hello) = serde_json::to_string(&Event::Hello { version: env!("CARGO_PKG_VERSION") }) {
                                let _ = stream.write_all((hello + "\n").as_bytes());
                            }
                            for (_, line) in inner.devices.lock().unwrap().iter() {
                                let _ = stream.write_all(line.as_bytes());
                            }
                            inner.clients.lock().unwrap().push(stream);
                            inner.has_clients.store(true, Ordering::Relaxed);
                            log::debug!("event stream: client connected");
                        }
                    })
                    .expect("spawn monitor thread");
            }
            Err(e) => log::warn!("event stream disabled: cannot bind {}: {e}", path.display()),
        }
        m
    }

    /// Milliseconds since the daemon started (the `ms` field of every event).
    pub fn now_ms(&self) -> u64 {
        self.inner.epoch.elapsed().as_millis() as u64
    }

    /// True while at least one client is connected. Callers skip building
    /// events otherwise, so the stream costs nothing when unused.
    pub fn active(&self) -> bool {
        self.inner.has_clients.load(Ordering::Relaxed)
    }

    /// One JSON line for `ev`, with this handle's device number added.
    fn line(&self, ev: &Event) -> Option<String> {
        let mut value = serde_json::to_value(ev).ok()?;
        if self.dev != 0 {
            value["dev"] = self.dev.into();
        }
        let mut line = serde_json::to_string(&value).ok()?;
        line.push('\n');
        Some(line)
    }

    /// Remember this handle's device (sent to clients that connect later)
    /// and announce it to the current ones; `None` means it went away.
    pub fn set_device(&self, ev: Option<Event>) {
        let mut devices = self.inner.devices.lock().unwrap();
        devices.retain(|(id, _)| *id != self.dev);
        if let Some(line) = ev.as_ref().and_then(|e| self.line(e)) {
            devices.push((self.dev, line));
        }
        drop(devices);
        match ev {
            Some(ev) => self.send(&ev),
            None => self.send(&Event::DeviceLost),
        }
    }

    pub fn send(&self, ev: &Event) {
        if !self.active() {
            return;
        }
        let Some(line) = self.line(ev) else { return };
        let mut clients = self.inner.clients.lock().unwrap();
        clients.retain_mut(|c| match c.write_all(line.as_bytes()) {
            Ok(()) => true,
            Err(e) => {
                log::debug!("event stream: client dropped ({e})");
                false
            }
        });
        if clients.is_empty() {
            self.inner.has_clients.store(false, Ordering::Relaxed);
        }
    }
}

/// `mousetouch events`: print the stream until the daemon goes away.
pub fn print_events() -> anyhow::Result<()> {
    use std::io::{BufRead, BufReader};
    let path = socket_path();
    let stream = UnixStream::connect(&path).map_err(|e| anyhow::anyhow!("connecting to {} ({e}); is the daemon running?", path.display()))?;
    let reader = BufReader::new(stream);
    let mut out = std::io::stdout().lock();
    for line in reader.lines() {
        let line = line?;
        writeln!(out, "{line}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    #[test]
    fn clients_get_the_device_then_events_as_json_lines() {
        let dir = std::env::temp_dir().join(format!("mousetouch-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.sock");
        let m = Monitor::start_at(&path);
        assert!(!m.active());
        // Nobody listening: sending is a no-op, not an error.
        m.send(&Event::Scroll { ms: 1, dx: 0, dy: 120 });
        m.set_device(Some(Event::Device { name: "Magic Mouse".into(), w_mm: 90.0, h_mm: 52.0, source: "hidraw" }));

        let client = UnixStream::connect(&path).unwrap();
        // The accept thread needs a moment to register the client.
        for _ in 0..200 {
            if m.active() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(m.active());
        m.send(&Event::Frame {
            ms: 42,
            fingers: vec![Finger { id: 3, x: 10.0, y: 20.0, x0: 10.0, y0: 20.0 }],
            state: "armed",
            max_fingers: 1,
            travel_mm: 0.0,
            pad_dx: None,
            pad_dy: None,
            scale: None,
        });
        m.send(&Event::Rule { ms: 43, kind: "tap", fingers: 3, dir: None, matched: true, action: Some("super+a".into()), app: None });

        let mut lines = BufReader::new(client).lines();
        let hello: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!((hello["t"].as_str(), hello["version"].as_str()), (Some("hello"), Some(env!("CARGO_PKG_VERSION"))));
        let dev: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!(dev["t"], "device");
        assert_eq!(dev["w_mm"], 90.0);
        let frame: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!(frame["t"], "frame");
        assert_eq!(frame["fingers"][0]["id"], 3);
        assert!(frame.get("pad_dx").is_none(), "absent optionals are skipped");
        let rule: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!(rule["action"], "super+a");

        drop(lines);
        // A gone client is dropped on the next send and the stream goes idle.
        for _ in 0..50 {
            m.send(&Event::DeviceLost);
            if !m.active() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!m.active());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_mice_are_replayed_and_stamped() {
        let dir = std::env::temp_dir().join(format!("mousetouch-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.sock");
        let m = Monitor::start_at(&path);
        let a = m.for_device(1);
        let b = m.for_device(2);
        a.set_device(Some(Event::Device { name: "Black  raton".into(), w_mm: 90.0, h_mm: 52.0, source: "hidraw" }));
        b.set_device(Some(Event::Device { name: "Mm v2".into(), w_mm: 90.0, h_mm: 52.0, source: "hidraw" }));

        let client = UnixStream::connect(&path).unwrap();
        for _ in 0..200 {
            if m.active() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        b.send(&Event::Scroll { ms: 5, dx: 0, dy: 120 });
        a.set_device(None);

        let mut lines = BufReader::new(client).lines();
        let mut next = || -> serde_json::Value { serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap() };
        assert_eq!(next()["t"], "hello");
        let first = next();
        assert_eq!((first["t"].as_str(), first["dev"].as_u64(), first["name"].as_str()), (Some("device"), Some(1), Some("Black  raton")));
        let second = next();
        assert_eq!((second["t"].as_str(), second["dev"].as_u64()), (Some("device"), Some(2)));
        let scroll = next();
        assert_eq!((scroll["t"].as_str(), scroll["dev"].as_u64(), scroll["dy"].as_i64()), (Some("scroll"), Some(2), Some(120)));
        let lost = next();
        assert_eq!((lost["t"].as_str(), lost["dev"].as_u64()), (Some("device_lost"), Some(1)));
        // Only the second mouse is replayed from now on.
        assert_eq!(m.inner.devices.lock().unwrap().iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![2]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
