//! Gesture recognition for one Magic Mouse. Consumes raw evdev events from
//! the grabbed device, forwards pointer/button/wheel events to the virtual
//! mouse, and turns multi-finger touches into gestures.

use crate::config::{Action, Axis, Config, Direction, GestureKind, SwipeDir};
use crate::desktop_settings::{self, DesktopSettings};
use crate::focus::Focus;
use crate::hidraw::RawTouch;
use crate::monitor::{Event as Mon, Finger, Monitor};
use crate::scroll::ScrollGate;
use crate::virt::{VirtualKeyboard, VirtualMouse, VirtualTouchpad};
use anyhow::Result;
use evdev::{AbsoluteAxisCode, EventType, InputEvent, RelativeAxisCode};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

const MAX_SLOTS: usize = 16;

/// Calibration of one multitouch axis of the real device.
#[derive(Debug, Clone, Copy)]
pub struct AxisInfo {
    pub min: i32,
    pub max: i32,
    /// Units per millimetre.
    pub res: f64,
}

impl AxisInfo {
    fn to_mm(&self, v: i32) -> f64 {
        (v - self.min) as f64 / self.res
    }

    /// Size of the axis in millimetres, as the driver calibrates it.
    fn span_mm(&self) -> f64 {
        (self.max - self.min) as f64 / self.res
    }
}

/// hid-magicmouse sizes the Magic Mouse's touch area as 90.56 mm along X and
/// 51.52 mm along Y, but X runs across the mouse (it is ~57 mm wide) and Y
/// along it: the two sizes are swapped. Millimetres derived from it make a
/// vertical stroke read ~3x shorter than the same horizontal one, so a
/// vertical workspace swipe needed an abrupt flick. Swipes scale Y by this
/// factor, `(span_x / span_y)^2` with that calibration (1 for any other),
/// and leave X alone so sensitivities tuned on horizontal swipes keep their
/// feel.
pub fn vertical_swipe_gain(ax: &AxisInfo, ay: &AxisInfo) -> f64 {
    let (x, y) = (ax.span_mm(), ay.span_mm());
    if (x - 90.56).abs() < 2.0 && (y - 51.52).abs() < 2.0 { (x / y).powi(2) } else { 1.0 }
}

#[derive(Debug, Clone, Copy)]
struct Touch {
    /// Contact id (hidraw touch id, or evdev tracking id).
    id: i32,
    x: i32,
    y: i32,
    start_x: i32,
    start_y: i32,
    /// True until the first SYN_REPORT after the contact appeared; position
    /// events in that first frame still define the landing point.
    fresh: bool,
}

/// Devices shared by every mouse: one touchpad and one keyboard.
pub struct Shared {
    pub touchpad: VirtualTouchpad,
    pub keyboard: VirtualKeyboard,
    /// Position of `Action::Toggle` / `Action::Cycle` rules, keyed by rule
    /// and focused app. Lives here so it survives a Bluetooth reconnect.
    pub toggles: HashMap<String, usize>,
}

#[derive(Debug)]
enum SwipeState {
    /// Fingers are down but have not travelled far enough yet.
    Armed,
    /// Replaying motion on the virtual touchpad; `sign` flips direction,
    /// `axis` picks the component forwarded.
    Workspace { sign: f64, axis: Axis },
    /// Replaying a pinch: two synthetic contacts whose distance follows the
    /// real finger distance (`sep0` = real distance when the pinch started).
    Pinch { sep0: f64, origin: (f64, f64) },
    /// A discrete swipe rule already fired (or nothing matched); wait for the
    /// finger count to change.
    Done,
}

struct Session {
    start: Instant,
    max_fingers: usize,
    count: usize,
    /// Centroid (mm) when the current finger count was established.
    anchor: (f64, f64),
    /// Finger spread (mm) when the current finger count was established.
    sep_anchor: f64,
    last_centroid: (f64, f64),
    swipe: SwipeState,
    /// True once any continuous or discrete swipe ran in this session.
    gestured: bool,
    /// A physical button was pressed while fingers were down: not a tap.
    clicked: bool,
    /// Travel forwarded to the virtual touchpad so far (mm), for the live view.
    pad: (f64, f64),
    /// Pinch ratio replayed so far, for the live view.
    scale: f64,
}

struct PendingTap {
    fingers: usize,
    deadline: Instant,
}

pub struct Engine {
    cfg: Arc<RwLock<Config>>,
    shared: Arc<Mutex<Shared>>,
    mouse: VirtualMouse,
    ax: AxisInfo,
    ay: AxisInfo,
    /// See `vertical_swipe_gain`.
    swipe_gain_y: f64,
    slots: [Option<Touch>; MAX_SLOTS],
    /// Last position reported in each slot. The kernel suppresses ABS events
    /// whose value equals the slot's cached value, so a new contact may land
    /// without an X or Y event; the previous value is then still correct.
    slot_last: [(i32, i32); MAX_SLOTS],
    current_slot: usize,
    touch_dirty: bool,
    finger_count: usize,
    session: Option<Session>,
    pending_tap: Option<PendingTap>,
    last_tap: Option<(usize, Instant)>,
    desktop: desktop_settings::Shared,
    /// Focused window app id (for per-application rules).
    focus: Focus,
    /// Max finger travel seen in the frame before all fingers lifted.
    last_travel: f64,
    /// Centroid speed (mm/s) over the last ~20 ms before all fingers lifted.
    last_speed: f64,
    /// Recent (time, centroid) samples, newest last, about 100 ms deep.
    history: std::collections::VecDeque<(Instant, (f64, f64))>,
    /// When true, touch data comes from `handle_touch_frame` (hidraw) and
    /// evdev ABS events are ignored.
    hidraw_mode: bool,
    /// Scroll-start filter (see `scroll.rs`).
    scroll: ScrollGate,
    /// Scratch buffer for events the gate releases.
    scroll_out: Vec<InputEvent>,
    /// Live event stream for the settings app (see monitor.rs).
    monitor: Monitor,
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Arc<RwLock<Config>>,
        shared: Arc<Mutex<Shared>>,
        mouse: VirtualMouse,
        ax: AxisInfo,
        ay: AxisInfo,
        desktop: desktop_settings::Shared,
        focus: Focus,
        monitor: Monitor,
    ) -> Self {
        Self {
            cfg,
            shared,
            mouse,
            swipe_gain_y: vertical_swipe_gain(&ax, &ay),
            ax,
            ay,
            slots: [None; MAX_SLOTS],
            slot_last: [(0, 0); MAX_SLOTS],
            current_slot: 0,
            touch_dirty: false,
            finger_count: 0,
            session: None,
            pending_tap: None,
            last_tap: None,
            desktop,
            focus,
            last_travel: 0.0,
            last_speed: 0.0,
            history: std::collections::VecDeque::with_capacity(16),
            hidraw_mode: false,
            scroll: ScrollGate::new(),
            scroll_out: Vec::new(),
            monitor,
        }
    }

    /// Switch the touch source to raw HID reports.
    pub fn set_hidraw_mode(&mut self, on: bool) {
        self.hidraw_mode = on;
        self.slots = [None; MAX_SLOTS];
    }

    /// One decoded HID report: the complete set of contacts right now.
    pub fn handle_touch_frame(&mut self, touches: &[RawTouch]) -> Result<()> {
        let mut next: [Option<Touch>; MAX_SLOTS] = [None; MAX_SLOTS];
        let mut used = 0usize;
        for t in touches.iter().filter(|t| t.down) {
            if used >= MAX_SLOTS {
                break;
            }
            let existing = self.slots.iter().flatten().find(|s| s.id == t.id as i32);
            next[used] = Some(match existing {
                Some(prev) => Touch { id: prev.id, x: t.x, y: t.y, start_x: prev.start_x, start_y: prev.start_y, fresh: false },
                None => Touch { id: t.id as i32, x: t.x, y: t.y, start_x: t.x, start_y: t.y, fresh: true },
            });
            used += 1;
        }
        self.slots = next;
        self.process_frame()
    }

    /// Earliest instant at which `tick` needs to run, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        let tap = self.pending_tap.as_ref().map(|p| p.deadline);
        let scroll = self.scroll.next_deadline();
        match (tap, scroll) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Time-based work: fire single taps whose double-tap window expired.
    pub fn tick(&mut self, now: Instant) -> Result<()> {
        if let Some(p) = &self.pending_tap {
            if now >= p.deadline {
                let fingers = p.fingers;
                self.pending_tap = None;
                self.fire_rule(GestureKind::Tap, fingers, None)?;
            }
        }
        if self.scroll.next_deadline().is_some_and(|d| now >= d) {
            let scroll_cfg = self.cfg.read().unwrap().scroll.clone();
            self.scroll_out.clear();
            self.scroll.tick(now, &scroll_cfg, &mut self.scroll_out);
            if !self.scroll_out.is_empty() {
                self.forward_scroll_out();
                // Momentum frames are not tied to a device frame.
                self.mouse.flush()?;
            }
        }
        Ok(())
    }

    /// Report and queue the wheel events the gate released.
    fn forward_scroll_out(&mut self) {
        if self.monitor.active() {
            let (mut dx, mut dy) = (0, 0);
            for e in &self.scroll_out {
                match RelativeAxisCode(e.code()) {
                    RelativeAxisCode::REL_WHEEL_HI_RES => dy += e.value(),
                    RelativeAxisCode::REL_HWHEEL_HI_RES => dx += e.value(),
                    _ => {}
                }
            }
            if dx != 0 || dy != 0 {
                self.monitor.send(&Mon::Scroll { ms: self.monitor.now_ms(), dx, dy });
            }
        }
        for e in self.scroll_out.drain(..) {
            self.mouse.push(e);
        }
    }

    pub fn handle(&mut self, ev: InputEvent) -> Result<()> {
        match ev.event_type() {
            EventType::ABSOLUTE => {
                if !self.hidraw_mode {
                    self.handle_abs(ev)
                }
            }
            EventType::RELATIVE => {
                let code = RelativeAxisCode(ev.code());
                let is_wheel = matches!(
                    code,
                    RelativeAxisCode::REL_WHEEL
                        | RelativeAxisCode::REL_HWHEEL
                        | RelativeAxisCode::REL_WHEEL_HI_RES
                        | RelativeAxisCode::REL_HWHEEL_HI_RES
                );
                if is_wheel {
                    let cfg = self.cfg.read().unwrap();
                    let limit = cfg.general.suppress_scroll_from_fingers as usize;
                    if limit > 0 && self.finger_count >= limit {
                        return Ok(());
                    }
                    let scroll_cfg = cfg.scroll.clone();
                    drop(cfg);
                    self.scroll_out.clear();
                    self.scroll.feed(ev, Instant::now(), &scroll_cfg, &mut self.scroll_out);
                    self.forward_scroll_out();
                    return Ok(());
                }
                self.mouse.push(ev);
            }
            EventType::KEY => {
                // BTN_* press while touching: the touch is a click, not a tap.
                if ev.value() == 1 && (0x100..0x160).contains(&ev.code()) {
                    if let Some(s) = &mut self.session {
                        s.clicked = true;
                    }
                }
                if self.monitor.active() && ev.value() != 2 && (0x100..0x160).contains(&ev.code()) {
                    let button = match ev.code() {
                        0x110 => "left",
                        0x111 => "right",
                        0x112 => "middle",
                        0x113 => "side",
                        0x114 => "extra",
                        _ => "other",
                    };
                    self.monitor.send(&Mon::Button { ms: self.monitor.now_ms(), button, down: ev.value() == 1 });
                }
                self.mouse.push(ev)
            }
            EventType::SYNCHRONIZATION => {
                if ev.code() == 0 {
                    // SYN_REPORT: end of frame.
                    self.mouse.flush()?;
                    if self.touch_dirty {
                        self.touch_dirty = false;
                        self.process_frame()?;
                    }
                } else if ev.code() == 3 {
                    // SYN_DROPPED: resync by forgetting touches.
                    self.slots = [None; MAX_SLOTS];
                    self.touch_dirty = true;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_abs(&mut self, ev: InputEvent) {
        let code = AbsoluteAxisCode(ev.code());
        let v = ev.value();
        log::trace!("abs {code:?} = {v}");
        match code {
            AbsoluteAxisCode::ABS_MT_SLOT => {
                self.current_slot = (v.max(0) as usize).min(MAX_SLOTS - 1);
            }
            AbsoluteAxisCode::ABS_MT_TRACKING_ID => {
                let slot = &mut self.slots[self.current_slot];
                if v < 0 {
                    *slot = None;
                } else if slot.is_none() {
                    let (x, y) = self.slot_last[self.current_slot];
                    *slot = Some(Touch { id: v, x, y, start_x: x, start_y: y, fresh: true });
                }
                self.touch_dirty = true;
            }
            AbsoluteAxisCode::ABS_MT_POSITION_X => {
                self.slot_last[self.current_slot].0 = v;
                if let Some(t) = &mut self.slots[self.current_slot] {
                    t.x = v;
                    if t.fresh {
                        t.start_x = v;
                    }
                    self.touch_dirty = true;
                }
            }
            AbsoluteAxisCode::ABS_MT_POSITION_Y => {
                self.slot_last[self.current_slot].1 = v;
                if let Some(t) = &mut self.slots[self.current_slot] {
                    t.y = v;
                    if t.fresh {
                        t.start_y = v;
                    }
                    self.touch_dirty = true;
                }
            }
            _ => {}
        }
    }

    fn centroid_mm(&self) -> (usize, (f64, f64)) {
        let mut n = 0usize;
        let (mut sx, mut sy) = (0.0, 0.0);
        for t in self.slots.iter().flatten() {
            n += 1;
            sx += self.ax.to_mm(t.x);
            sy += self.ay.to_mm(t.y);
        }
        if n == 0 {
            (0, (0.0, 0.0))
        } else {
            (n, (sx / n as f64, sy / n as f64))
        }
    }

    /// Finger spread (mm): mean distance of the fingers to their centroid,
    /// doubled so that for two fingers it equals the distance between them.
    fn spread_mm(&self, centroid: (f64, f64)) -> f64 {
        let (mut n, mut sum) = (0usize, 0.0);
        for t in self.slots.iter().flatten() {
            let dx = self.ax.to_mm(t.x) - centroid.0;
            let dy = self.ay.to_mm(t.y) - centroid.1;
            sum += (dx * dx + dy * dy).sqrt();
            n += 1;
        }
        if n < 2 { 0.0 } else { 2.0 * sum / n as f64 }
    }

    /// Largest distance any finger moved from where it landed (mm).
    fn max_travel_mm(&self) -> f64 {
        self.slots
            .iter()
            .flatten()
            .map(|t| {
                let dx = (t.x - t.start_x) as f64 / self.ax.res;
                let dy = (t.y - t.start_y) as f64 / self.ay.res;
                (dx * dx + dy * dy).sqrt()
            })
            .fold(0.0, f64::max)
    }

    fn process_frame(&mut self) -> Result<()> {
        let now = Instant::now();
        let (n, c) = self.centroid_mm();
        let travel = self.max_travel_mm();
        let prev_n = self.finger_count;
        self.finger_count = n;
        for t in self.slots.iter_mut().flatten() {
            t.fresh = false;
        }
        log::trace!("frame: {n} finger(s) centroid=({:.1},{:.1})mm travel={travel:.1}mm", c.0, c.1);

        if n == 0 {
            if let Some(s) = self.session.take() {
                if matches!(s.swipe, SwipeState::Workspace { .. } | SwipeState::Pinch { .. }) {
                    self.shared.lock().unwrap().touchpad.end()?;
                    self.monitor_gesture_end(&s.swipe);
                }
                // Lift speed measured up to the lift itself: a finger that
                // rested a while before leaving (no new samples, since the
                // kernel drops unchanged positions) counts as slow.
                if let Some(&(_, c_last)) = self.history.back() {
                    if let Some(&(t0, c0)) = self.history.iter().rev().find(|(t, _)| now.duration_since(*t) >= Duration::from_millis(20)) {
                        let dt = now.duration_since(t0).as_secs_f64().max(0.005);
                        self.last_speed = ((c_last.0 - c0.0).powi(2) + (c_last.1 - c0.1).powi(2)).sqrt() / dt;
                    }
                }
                let (taps, scroll_cfg) = {
                    let cfg = self.cfg.read().unwrap();
                    (cfg.taps.clone(), cfg.scroll.clone())
                };
                // A finger that lifts while scrolling leaves the page coasting.
                self.scroll.lift(now, &scroll_cfg);
                let dur = now.duration_since(s.start);
                let reason = if s.gestured {
                    Some("a gesture ran")
                } else if s.clicked {
                    Some("button pressed")
                } else if dur > Duration::from_millis(taps.tap_max_ms) {
                    Some("too long")
                } else if self.last_travel > taps.tap_max_mm {
                    Some("moved too far")
                } else if self.last_speed > taps.tap_max_lift_speed {
                    Some("lifted while moving")
                } else {
                    None
                };
                let is_tap = reason.is_none();
                log::debug!(
                    "touch ended: {} finger(s), {} ms, travel {:.1} mm, lift speed {:.0} mm/s -> {}",
                    s.max_fingers, dur.as_millis(), self.last_travel, self.last_speed,
                    reason.map_or("tap".to_string(), |r| format!("not a tap ({r})"))
                );
                if self.monitor.active() {
                    let ms = self.monitor.now_ms();
                    self.monitor.send(&Mon::TouchEnd {
                        ms,
                        fingers: s.max_fingers,
                        duration_ms: dur.as_millis() as u64,
                        travel_mm: self.last_travel,
                        lift_speed: self.last_speed,
                        tap: is_tap,
                        reason,
                    });
                    self.monitor.send(&Mon::Frame {
                        ms,
                        fingers: Vec::new(),
                        state: "idle",
                        max_fingers: s.max_fingers,
                        travel_mm: 0.0,
                        pad_dx: None,
                        pad_dy: None,
                        scale: None,
                    });
                }
                if is_tap {
                    self.register_tap(s.max_fingers, now, Duration::from_millis(taps.double_tap_ms))?;
                }
            }
            self.last_travel = 0.0;
            self.last_speed = 0.0;
            self.history.clear();
            return Ok(());
        }
        self.last_travel = travel;
        // Lift speed: displacement between this frame and the most recent
        // sample at least 20 ms older (two reports), so a single noisy frame
        // cannot decide. A tapping finger has settled by now; a flick has not.
        // When the finger count changes the centroid jumps to wherever the
        // remaining fingers are; that jump is not motion, so the history is
        // restarted and the speed measured before the change is kept until
        // enough new samples exist.
        if n != prev_n {
            self.history.clear();
        }
        self.history.push_back((now, c));
        while self.history.len() > 1 && now.duration_since(self.history[0].0) > Duration::from_millis(100) {
            self.history.pop_front();
        }
        if let Some(&(t0, c0)) = self.history.iter().rev().find(|(t, _)| now.duration_since(*t) >= Duration::from_millis(20)) {
            let dt = now.duration_since(t0).as_secs_f64().max(0.005);
            self.last_speed = ((c.0 - c0.0).powi(2) + (c.1 - c0.1).powi(2)).sqrt() / dt;
        }

        let spread = self.spread_mm(c);
        if self.session.is_none() {
            // New touch: decide the scroll direction afresh.
            self.scroll.reset();
        }
        let mut session = match self.session.take() {
            None => Session {
                start: now,
                max_fingers: n,
                count: n,
                anchor: c,
                sep_anchor: spread,
                last_centroid: c,
                swipe: SwipeState::Armed,
                gestured: false,
                clicked: false,
                pad: (0.0, 0.0),
                scale: 1.0,
            },
            Some(s) => s,
        };

        if session.count != n {
            // Finger count changed: close any running swipe and re-arm.
            if matches!(session.swipe, SwipeState::Workspace { .. } | SwipeState::Pinch { .. }) {
                self.shared.lock().unwrap().touchpad.end()?;
                self.monitor_gesture_end(&session.swipe);
            }
            session.count = n;
            session.max_fingers = session.max_fingers.max(n);
            session.anchor = c;
            session.sep_anchor = spread;
            session.last_centroid = c;
            session.swipe = SwipeState::Armed;
            session.pad = (0.0, 0.0);
            session.scale = 1.0;
        }

        match session.swipe {
            SwipeState::Workspace { sign, axis } => {
                let dx = c.0 - session.last_centroid.0;
                let dy = (c.1 - session.last_centroid.1) * self.swipe_gain_y;
                let sens = self.cfg.read().unwrap().workspace_swipe.sensitivity * self.desktop.read().unwrap().swipe_gain as f64;
                let (vx, vy) = match axis {
                    Axis::Horizontal => (dx * sens * sign, 0.0),
                    Axis::Vertical => (0.0, dy * sens * sign),
                    // `Auto` is resolved when the swipe starts; never stored.
                    Axis::Both | Axis::Auto => (dx * sens * sign, dy * sens * sign),
                };
                if vx != 0.0 || vy != 0.0 {
                    self.shared.lock().unwrap().touchpad.move_by(vx, vy)?;
                    session.pad.0 += vx;
                    session.pad.1 += vy;
                }
            }
            SwipeState::Pinch { sep0, origin } => {
                let sens = self.cfg.read().unwrap().pinch.sensitivity;
                let positions = pinch_positions(origin, c, session.anchor, sep0, spread, sens);
                self.shared.lock().unwrap().touchpad.move_to(&positions)?;
                session.scale = (spread.max(0.5) / sep0.max(0.5)).powf(sens);
            }
            SwipeState::Armed => {
                let dx = c.0 - session.anchor.0;
                // Vertical travel corrected like the swipe itself, so the
                // direction (and when the swipe starts) is judged on equal
                // footing for both axes.
                let dy = (c.1 - session.anchor.1) * self.swipe_gain_y;
                let dist = (dx * dx + dy * dy).sqrt();
                let (ws, pinch) = {
                    let cfg = self.cfg.read().unwrap();
                    (cfg.workspace_swipe.clone(), cfg.pinch.clone())
                };
                let dsep = spread - session.sep_anchor;
                if pinch.enabled && pinch.fingers as usize == n && dsep.abs() >= pinch.activation_mm && dsep.abs() > dist {
                    // Fingers moving apart/together faster than they travel: pinch.
                    let origin = VirtualTouchpad::center_mm();
                    let sep0 = session.sep_anchor.max(1.0);
                    let positions = pinch_positions(origin, c, session.anchor, sep0, spread, pinch.sensitivity);
                    let mut sh = self.shared.lock().unwrap();
                    sh.touchpad.begin_at(&pinch_positions(origin, session.anchor, session.anchor, sep0, sep0, pinch.sensitivity))?;
                    sh.touchpad.move_to(&positions)?;
                    drop(sh);
                    session.gestured = true;
                    session.swipe = SwipeState::Pinch { sep0, origin };
                    session.scale = (spread.max(0.5) / sep0.max(0.5)).powf(pinch.sensitivity);
                    log::debug!("pinch begin: {n} fingers, spread {sep0:.1} -> {spread:.1} mm");
                    self.monitor_gesture_begin("pinch", n, None);
                } else if dist >= ws.activation_mm {
                    let dir = if dx.abs() >= dy.abs() {
                        if dx > 0.0 { SwipeDir::Right } else { SwipeDir::Left }
                    } else if dy > 0.0 {
                        SwipeDir::Down
                    } else {
                        SwipeDir::Up
                    };
                    let desktop = self.desktop.read().unwrap().clone();
                    let ws_axis = ws.axis.resolve(desktop.workspace_axis);
                    if self.fire_rule(GestureKind::Swipe, n, Some(dir))? {
                        session.gestured = true;
                        session.swipe = SwipeState::Done;
                    } else if ws.enabled && ws.fingers as usize == n && axis_matches(ws_axis, dir) {
                        session.gestured = true;
                        let natural = ws.direction == Direction::Natural;
                        let compositor_natural = ws.compositor_natural_scroll.unwrap_or(desktop.natural_scroll);
                        // The compositor already follows the fingers when its
                        // own natural scrolling is on; otherwise flip.
                        let sign = if natural != compositor_natural { -1.0 } else { 1.0 };
                        // With `both`, lock to the direction the fingers took first.
                        let axis = match ws_axis {
                            Axis::Both if matches!(dir, SwipeDir::Up | SwipeDir::Down) => Axis::Vertical,
                            Axis::Both => Axis::Horizontal,
                            a => a,
                        };
                        let mut sh = self.shared.lock().unwrap();
                        sh.touchpad.begin(ws.synthetic_fingers.unwrap_or(desktop.synthetic_fingers) as usize)?;
                        let sens = ws.sensitivity * desktop.swipe_gain as f64;
                        let (vx, vy) = match axis {
                            Axis::Horizontal => (dx * sens * sign, 0.0),
                            _ => (0.0, dy * sens * sign),
                        };
                        sh.touchpad.move_by(vx, vy)?;
                        drop(sh);
                        session.swipe = SwipeState::Workspace { sign, axis };
                        session.pad = (vx, vy);
                        log::debug!("workspace swipe begin: {n} fingers, dir {dir:?}");
                        self.monitor_gesture_begin("workspace_swipe", n, Some(dir));
                    } else {
                        // Nothing to do with this motion; a short touch may
                        // still end as a tap (fingertips drift when tapping).
                        session.swipe = SwipeState::Done;
                    }
                }
            }
            SwipeState::Done => {}
        }

        if self.monitor.active() {
            self.send_frame(&session, travel);
        }
        session.last_centroid = c;
        self.session = Some(session);
        Ok(())
    }

    // -- live event stream -------------------------------------------------

    fn send_frame(&self, session: &Session, travel: f64) {
        let fingers = self
            .slots
            .iter()
            .flatten()
            .map(|t| Finger {
                id: t.id,
                x: self.ax.to_mm(t.x),
                y: self.ay.to_mm(t.y),
                x0: self.ax.to_mm(t.start_x),
                y0: self.ay.to_mm(t.start_y),
            })
            .collect();
        let (pad, scale) = match session.swipe {
            SwipeState::Workspace { .. } => (Some(session.pad), None),
            SwipeState::Pinch { .. } => (None, Some(session.scale)),
            _ => (None, None),
        };
        self.monitor.send(&Mon::Frame {
            ms: self.monitor.now_ms(),
            fingers,
            state: state_name(&session.swipe),
            max_fingers: session.max_fingers,
            travel_mm: travel,
            pad_dx: pad.map(|p| p.0),
            pad_dy: pad.map(|p| p.1),
            scale,
        });
    }

    fn monitor_gesture_begin(&self, gesture: &'static str, fingers: usize, dir: Option<SwipeDir>) {
        if self.monitor.active() {
            self.monitor.send(&Mon::GestureBegin { ms: self.monitor.now_ms(), gesture, fingers, dir: dir.map(dir_name) });
        }
    }

    fn monitor_gesture_end(&self, swipe: &SwipeState) {
        if self.monitor.active() {
            let gesture = match swipe {
                SwipeState::Pinch { .. } => "pinch",
                _ => "workspace_swipe",
            };
            self.monitor.send(&Mon::GestureEnd { ms: self.monitor.now_ms(), gesture });
        }
    }

    fn register_tap(&mut self, fingers: usize, now: Instant, window: Duration) -> Result<()> {
        let cfg = self.cfg.read().unwrap();
        let has_double = cfg.gestures.iter().any(|g| g.enabled && g.kind == GestureKind::DoubleTap && g.fingers as usize == fingers);
        let has_single = cfg.gestures.iter().any(|g| g.enabled && g.kind == GestureKind::Tap && g.fingers as usize == fingers);
        drop(cfg);

        if let Some((f, t)) = self.last_tap {
            if f == fingers && now.duration_since(t) <= window && has_double {
                self.last_tap = None;
                self.pending_tap = None;
                log::debug!("double tap: {fingers} fingers");
                self.fire_rule(GestureKind::DoubleTap, fingers, None)?;
                return Ok(());
            }
        }
        self.last_tap = Some((fingers, now));
        if has_double {
            // Hold the single tap until the double-tap window closes.
            self.pending_tap = if has_single { Some(PendingTap { fingers, deadline: now + window }) } else { None };
        } else if has_single {
            self.fire_rule(GestureKind::Tap, fingers, None)?;
        }
        Ok(())
    }

    /// Run the first enabled rule matching (kind, fingers, direction).
    /// Returns true if a rule fired.
    fn fire_rule(&mut self, kind: GestureKind, fingers: usize, dir: Option<SwipeDir>) -> Result<bool> {
        let focused = self.focus.lock().unwrap().clone().unwrap_or_default().to_ascii_lowercase();
        let action = {
            let cfg = self.cfg.read().unwrap();
            let mut candidates = cfg
                .gestures
                .iter()
                .filter(|g| g.enabled && g.kind == kind && g.fingers as usize == fingers && (kind != GestureKind::Swipe || g.direction == dir));
            // A rule scoped to the focused app wins over a generic one.
            let scoped = candidates
                .clone()
                .find(|g| g.app.as_deref().map_or(false, |a| !focused.is_empty() && focused.contains(&a.to_ascii_lowercase())));
            scoped.or_else(|| candidates.find(|g| g.app.is_none())).map(|g| (g.action.clone(), g.app.clone()))
        };
        if self.monitor.active() {
            self.monitor.send(&Mon::Rule {
                ms: self.monitor.now_ms(),
                kind: kind_name(kind),
                fingers,
                dir: dir.map(dir_name),
                matched: action.is_some(),
                action: action.as_ref().map(|(a, _)| describe_action(a, &self.desktop.read().unwrap())),
                app: action.as_ref().and_then(|(_, app)| app.clone()),
            });
        }
        let Some((action, app)) = action else { return Ok(false) };
        log::info!("gesture {kind:?} {fingers}f {dir:?} app={app:?} -> {action:?}");
        let toggle_key = format!("{kind:?}/{fingers}/{dir:?}/{app:?}@{focused}");
        self.run_action(&action, &toggle_key)?;
        Ok(true)
    }

    fn run_action(&mut self, action: &Action, toggle_key: &str) -> Result<()> {
        match action {
            Action::Workspaces | Action::Apps => {
                let d = self.desktop.read().unwrap();
                let combo = if matches!(action, Action::Workspaces) { &d.workspaces_combo } else { &d.apps_combo };
                if let Err(e) = self.shared.lock().unwrap().keyboard.tap_combo(combo) {
                    log::error!("{action:?} action '{combo}': {e}");
                }
            }
            Action::Key { combo } => {
                if let Err(e) = self.shared.lock().unwrap().keyboard.tap_combo(combo) {
                    log::error!("key action '{combo}': {e}");
                }
            }
            Action::Toggle { on, off } => {
                let mut sh = self.shared.lock().unwrap();
                let pos = sh.toggles.entry(toggle_key.to_string()).or_insert(0);
                let combo = if *pos % 2 == 0 { on } else { off };
                *pos += 1;
                if let Err(e) = sh.keyboard.tap_combo(combo) {
                    log::error!("toggle action '{combo}': {e}");
                }
            }
            Action::Cycle { steps } => {
                if steps.is_empty() {
                    return Ok(());
                }
                let mut sh = self.shared.lock().unwrap();
                let pos = sh.toggles.entry(toggle_key.to_string()).or_insert(0);
                let combo = &steps[*pos % steps.len()];
                *pos += 1;
                log::debug!("cycle step {}/{}: {combo}", *pos, steps.len());
                if let Err(e) = sh.keyboard.tap_combo(combo) {
                    log::error!("cycle action '{combo}': {e}");
                }
            }
            Action::Button { button } => {
                if let Err(e) = self.mouse.click(button) {
                    log::error!("button action '{button}': {e}");
                }
            }
            Action::Command { command } => {
                if let Err(e) = std::process::Command::new("sh").arg("-c").arg(command).spawn() {
                    log::error!("command '{command}': {e}");
                }
            }
        }
        Ok(())
    }
}

fn state_name(s: &SwipeState) -> &'static str {
    match s {
        SwipeState::Armed => "armed",
        SwipeState::Workspace { .. } => "workspace",
        SwipeState::Pinch { .. } => "pinch",
        SwipeState::Done => "done",
    }
}

fn dir_name(d: SwipeDir) -> &'static str {
    match d {
        SwipeDir::Left => "left",
        SwipeDir::Right => "right",
        SwipeDir::Up => "up",
        SwipeDir::Down => "down",
    }
}

fn kind_name(k: GestureKind) -> &'static str {
    match k {
        GestureKind::Swipe => "swipe",
        GestureKind::Tap => "tap",
        GestureKind::DoubleTap => "double_tap",
    }
}

/// One-line description of an action for the live view.
fn describe_action(a: &Action, d: &DesktopSettings) -> String {
    match a {
        Action::Workspaces => format!("workspaces ({})", d.workspaces_combo),
        Action::Apps => format!("apps ({})", d.apps_combo),
        Action::Key { combo } => combo.clone(),
        Action::Toggle { on, off } => format!("{on} ⇄ {off}"),
        Action::Cycle { steps } => steps.join(" → "),
        Action::Button { button } => format!("click {button}"),
        Action::Command { command } => format!("$ {command}"),
    }
}

fn axis_matches(axis: Axis, dir: SwipeDir) -> bool {
    match axis {
        Axis::Horizontal => matches!(dir, SwipeDir::Left | SwipeDir::Right),
        Axis::Vertical => matches!(dir, SwipeDir::Up | SwipeDir::Down),
        Axis::Both => true,
        // Resolved before this is called.
        Axis::Auto => false,
    }
}

/// Where the two synthetic pinch contacts sit on the pad (mm): centred on
/// `origin` plus the real centroid's displacement, separated by a base
/// distance scaled with the real spread ratio raised to `sensitivity`.
fn pinch_positions(origin: (f64, f64), centroid: (f64, f64), anchor: (f64, f64), sep0: f64, spread: f64, sensitivity: f64) -> [(f64, f64); 2] {
    const BASE_SEP_MM: f64 = 40.0;
    let ratio = (spread.max(0.5) / sep0.max(0.5)).powf(sensitivity);
    let sep = (BASE_SEP_MM * ratio).clamp(4.0, 180.0);
    let cx = origin.0 + (centroid.0 - anchor.0);
    let cy = origin.1 + (centroid.1 - anchor.1);
    [(cx - sep / 2.0, cy), (cx + sep / 2.0, cy)]
}

impl Engine {
    /// Fingers currently on the surface (for tests and diagnostics).
    #[allow(dead_code)]
    pub fn finger_count(&self) -> usize {
        self.finger_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evdev::{BusType, InputId};

    fn abs(code: AbsoluteAxisCode, v: i32) -> InputEvent {
        InputEvent::new(EventType::ABSOLUTE.0, code.0, v)
    }
    fn syn() -> InputEvent {
        InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0)
    }

    fn engine() -> Engine {
        let cfg = Arc::new(RwLock::new(Config::default()));
        let shared = Arc::new(Mutex::new(Shared {
            touchpad: VirtualTouchpad::new().unwrap(),
            keyboard: VirtualKeyboard::new().unwrap(),
            toggles: HashMap::new(),
        }));
        let mouse = VirtualMouse::new("Test Magic Mouse", InputId::new(BusType::BUS_BLUETOOTH, 0x4c, 0x269, 1)).unwrap();
        let ax = AxisInfo { min: -1100, max: 1258, res: 26.0 };
        let ay = AxisInfo { min: -1589, max: 2047, res: 70.0 };
        Engine::new(cfg, shared, mouse, ax, ay, Arc::new(RwLock::new(desktop_settings::DesktopSettings::fallback(desktop_settings::Desktop::Other))), Arc::new(Mutex::new(None)), Monitor::disabled())
    }

    #[test]
    fn magic_mouse_vertical_travel_is_corrected() {
        // hid-magicmouse's calibration (90.7 x 51.9 mm, swapped).
        let ax = AxisInfo { min: -1100, max: 1258, res: 26.0 };
        let ay = AxisInfo { min: -1589, max: 2047, res: 70.0 };
        let g = vertical_swipe_gain(&ax, &ay);
        assert!((g - 3.05).abs() < 0.05, "{g}");
        // Any other device is left alone.
        let pad = AxisInfo { min: 0, max: 3000, res: 30.0 };
        assert_eq!(vertical_swipe_gain(&pad, &pad), 1.0);
        assert!((engine().swipe_gain_y - g).abs() < 1e-9);
    }

    #[test]
    fn click_during_touch_is_not_a_tap() {
        let mut e = engine();
        for ev in [
            abs(AbsoluteAxisCode::ABS_MT_SLOT, 0),
            abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 1),
            abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 0),
            abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, 0),
            syn(),
            InputEvent::new(EventType::KEY.0, 0x110, 1), // BTN_LEFT down
            syn(),
            InputEvent::new(EventType::KEY.0, 0x110, 0),
            syn(),
            abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1),
            syn(),
        ] {
            e.handle(ev).unwrap();
        }
        assert!(e.last_tap.is_none(), "a click must not register as a tap");
    }

    #[test]
    fn staggered_lift_is_still_a_tap() {
        // Two resting fingers; one leaves 10 ms before the other. The centroid
        // jumps half the finger distance at that moment, which must not be
        // read as lift speed.
        let mut e = engine();
        e.set_hidraw_mode(true);
        let both = [RawTouch { id: 1, x: -300, y: 0, down: true }, RawTouch { id: 2, x: 300, y: 0, down: true }];
        let one = [RawTouch { id: 1, x: -300, y: 0, down: true }];
        e.handle_touch_frame(&both).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        e.handle_touch_frame(&both).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        e.handle_touch_frame(&one).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        e.handle_touch_frame(&[]).unwrap();
        assert!(e.last_speed <= 80.0, "lift speed {} mm/s came from the centroid jump", e.last_speed);
        assert_eq!(e.last_tap.map(|(f, _)| f), Some(2), "should register a 2-finger tap");
    }

    #[test]
    fn two_finger_spread_is_a_pinch() {
        let mut e = engine();
        for ev in [
            abs(AbsoluteAxisCode::ABS_MT_SLOT, 0),
            abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 1),
            abs(AbsoluteAxisCode::ABS_MT_POSITION_X, -100),
            abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, 0),
            abs(AbsoluteAxisCode::ABS_MT_SLOT, 1),
            abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 2),
            abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 100),
            abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, 0),
            syn(),
        ] {
            e.handle(ev).unwrap();
        }
        // Spread both fingers outwards by 6 mm each (156 units), centroid fixed.
        for step in 1..=4 {
            for ev in [
                abs(AbsoluteAxisCode::ABS_MT_SLOT, 0),
                abs(AbsoluteAxisCode::ABS_MT_POSITION_X, -100 - 39 * step),
                abs(AbsoluteAxisCode::ABS_MT_SLOT, 1),
                abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 100 + 39 * step),
                syn(),
            ] {
                e.handle(ev).unwrap();
            }
        }
        assert!(matches!(e.session.as_ref().unwrap().swipe, SwipeState::Pinch { .. }), "pinch should be running");
    }

    #[test]
    fn two_finger_swipe_is_detected() {
        let mut e = engine();
        // Two fingers land.
        for ev in [
            abs(AbsoluteAxisCode::ABS_MT_SLOT, 0),
            abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 1),
            abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 0),
            abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, 0),
            abs(AbsoluteAxisCode::ABS_MT_SLOT, 1),
            abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 2),
            abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 300),
            abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, 0),
            syn(),
        ] {
            e.handle(ev).unwrap();
        }
        assert_eq!(e.finger_count(), 2);
        assert!(matches!(e.session.as_ref().unwrap().swipe, SwipeState::Armed));
        // Move both fingers 10 mm to the left (260 units).
        for step in 1..=5 {
            for ev in [
                abs(AbsoluteAxisCode::ABS_MT_SLOT, 0),
                abs(AbsoluteAxisCode::ABS_MT_POSITION_X, -52 * step),
                abs(AbsoluteAxisCode::ABS_MT_SLOT, 1),
                abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 300 - 52 * step),
                syn(),
            ] {
                e.handle(ev).unwrap();
            }
        }
        assert!(matches!(e.session.as_ref().unwrap().swipe, SwipeState::Workspace { .. }), "swipe should be running");
        // Lift.
        for ev in [
            abs(AbsoluteAxisCode::ABS_MT_SLOT, 0),
            abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1),
            abs(AbsoluteAxisCode::ABS_MT_SLOT, 1),
            abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1),
            syn(),
        ] {
            e.handle(ev).unwrap();
        }
        assert_eq!(e.finger_count(), 0);
        assert!(e.session.is_none());
    }

    #[test]
    fn two_finger_double_tap_is_detected() {
        let mut e = engine();
        let tap = |e: &mut Engine| {
            for ev in [
                abs(AbsoluteAxisCode::ABS_MT_SLOT, 0),
                abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 1),
                abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 0),
                abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, 0),
                abs(AbsoluteAxisCode::ABS_MT_SLOT, 1),
                abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 2),
                abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 300),
                abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, 0),
                syn(),
                abs(AbsoluteAxisCode::ABS_MT_SLOT, 0),
                abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1),
                abs(AbsoluteAxisCode::ABS_MT_SLOT, 1),
                abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1),
                syn(),
            ] {
                e.handle(ev).unwrap();
            }
        };
        tap(&mut e);
        assert!(e.last_tap.is_some(), "first tap registered");
        tap(&mut e);
        assert!(e.last_tap.is_none(), "double tap fired and cleared");
    }
}
