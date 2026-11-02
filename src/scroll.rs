//! Wheel shaping for the Magic Mouse's one-finger scrolling.
//!
//! The kernel driver turns finger motion into wheel events, and the daemon
//! re-emits them through the virtual mouse. Four things happen on the way:
//!
//! * **Direction debounce.** The driver starts scrolling after only ~1.3 mm
//!   of travel, and a fingertip landing on the curved shell rolls about that
//!   far before the hand moves, often the other way. The first wheel events
//!   of every touch are held and summed; forwarding starts once the net
//!   movement on an axis is clearly one way, and the opposite part is gone.
//!
//! * **Consistent resolution.** The driver emits low-resolution notches
//!   (`REL_WHEEL`, one per ~1.4 mm, starting after 2.9 mm) and
//!   high-resolution steps (`REL_WHEEL_HI_RES`, 1/10 notch per ~0.13 mm,
//!   starting after 1.3 mm) from two separate counters, so they disagree and
//!   a fast flick can produce a frame with notches but no hi-res step. When
//!   that is the first wheel frame libinput ever sees from a device it
//!   switches that device to "kernel bug" emulation for good, and from then
//!   on scrolling feels different (whole notches instead of smooth steps).
//!   Which mode you got depended on the first scroll after the mouse
//!   connected. The shaper therefore ignores the driver's notches, keeps one
//!   hi-res counter per axis, and emits both resolutions from it, hi-res
//!   first in every frame.
//!
//! * **Acceleration.** The finger speed is measured from the wheel stream
//!   (the driver reports about 92 hi-res units per mm). A slow drag is
//!   forwarded as is; faster movement gets a gain that ramps smoothly up to
//!   `1 + acceleration` at a hard flick, the way macOS scrolls further the
//!   faster the finger goes.
//!
//! * **Momentum.** When the finger lifts while still moving, scrolling goes
//!   on at the lift speed and decays by `momentum_decay` every millisecond
//!   (Apple's documented rate is 0.998), emitted in ~8 ms frames from
//!   `tick`. A new touch stops it at once. macOS does this in the toolkit;
//!   here it has to be the daemon, since wheel events have no "phase".
//!
//! `mode = "notches"` releases movement in whole notches (what terminals and
//! most apps treat as "scroll three lines"); `mode = "smooth"` releases every
//! hi-res step as it comes. `speed` scales the movement before either.

use crate::config::{Scroll, ScrollMode};
use evdev::{EventType, InputEvent, RelativeAxisCode};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// One wheel notch in high-resolution units (Linux convention).
pub const HI_RES_NOTCH: i32 = 120;

/// Hi-res units the kernel driver reports per millimetre of finger travel
/// (one notch per ~1.3 mm).
const UNITS_PER_MM: f64 = HI_RES_NOTCH as f64 / 1.3;
/// Finger speed (mm/s) where the acceleration gain starts to rise, and
/// where it reaches `1 + acceleration`.
const ACCEL_FROM_MM_S: f64 = 150.0;
const ACCEL_TO_MM_S: f64 = 1000.0;
/// Wheel samples older than this no longer count towards the speed.
const SPEED_WINDOW: Duration = Duration::from_millis(100);
/// A finger that produced no wheel event for this long before lifting had
/// stopped: no momentum.
const LIFT_GAP: Duration = Duration::from_millis(60);
/// Lift speed (hi-res units per ms, after gain) needed to start coasting;
/// about 80 mm/s of finger travel.
const COAST_START: f64 = 80.0 * UNITS_PER_MM / 1000.0;
/// Coasting ends below this speed (about 10 mm/s).
const COAST_STOP: f64 = 10.0 * UNITS_PER_MM / 1000.0;
/// The coast never starts faster than this (hi-res units per ms): a hard
/// flick then travels about 80 notches, which is what a page-flinging swipe
/// does on macOS. Without the cap a flick at 1 m/s would scroll for ten
/// screens.
const COAST_MAX: f64 = 20.0;
/// How often momentum frames are emitted.
pub const COAST_FRAME: Duration = Duration::from_millis(8);

#[derive(Debug, Clone, Copy)]
struct Coast {
    /// When the finger lifted.
    start: Instant,
    /// Speed at lift, hi-res units per ms, signed.
    v0: f64,
    /// Distance already handed to the accumulator.
    emitted: f64,
}

#[derive(Debug, Default)]
struct AxisGate {
    /// Low-resolution notches seen from the driver while undecided (only
    /// used when no hi-res step arrived at all, e.g. a hard flick).
    lo_seen: i32,
    /// Pending movement in hi-res units, already scaled by `speed`.
    acc: f64,
    /// Direction decided for this touch (+1 / -1); `None` while holding.
    committed: Option<i32>,
    /// Events held so far (for the debug log only).
    held: u32,
    /// Recent wheel samples `(when, raw units, scaled units)`, newest last.
    samples: VecDeque<(Instant, f64, f64)>,
    coast: Option<Coast>,
}

impl AxisGate {
    fn trim(&mut self, now: Instant) {
        while self.samples.front().is_some_and(|(t, _, _)| now.duration_since(*t) > SPEED_WINDOW) {
            self.samples.pop_front();
        }
    }

    /// Speed over the sample window, hi-res units per ms: `(raw, scaled)`.
    fn speed(&self, now: Instant) -> (f64, f64) {
        let Some(&(t0, _, _)) = self.samples.front() else { return (0.0, 0.0) };
        // The window spans from the oldest sample to now, never less than a
        // report interval, so one isolated step does not read as a flick.
        let ms = now.duration_since(t0).as_secs_f64().max(0.012) * 1000.0;
        let (raw, scaled) = self.samples.iter().fold((0.0, 0.0), |(r, s), (_, a, b)| (r + a, s + b));
        (raw / ms, scaled / ms)
    }
}

#[derive(Debug, Default)]
pub struct ScrollGate {
    /// Index 0 = vertical wheel, 1 = horizontal wheel.
    axes: [AxisGate; 2],
    /// When the next momentum frame is due, while any axis coasts.
    next_frame: Option<Instant>,
}

/// Gain for a finger speed given in hi-res units per ms.
fn accel_gain(raw_units_per_ms: f64, acceleration: f64) -> f64 {
    if acceleration <= 0.0 {
        return 1.0;
    }
    let mm_s = raw_units_per_ms.abs() * 1000.0 / UNITS_PER_MM;
    let x = ((mm_s - ACCEL_FROM_MM_S) / (ACCEL_TO_MM_S - ACCEL_FROM_MM_S)).clamp(0.0, 1.0);
    let smooth = x * x * (3.0 - 2.0 * x);
    1.0 + acceleration * smooth
}

/// Distance covered after `t_ms` of coasting from speed `v0` with a decay
/// of `decay` per ms: the integral of `v0 * decay^t`.
fn coast_distance(v0: f64, decay: f64, t_ms: f64) -> f64 {
    let k = -decay.ln();
    v0 * (1.0 - (-k * t_ms).exp()) / k
}

fn codes(axis: usize) -> (RelativeAxisCode, RelativeAxisCode) {
    if axis == 0 {
        (RelativeAxisCode::REL_WHEEL, RelativeAxisCode::REL_WHEEL_HI_RES)
    } else {
        (RelativeAxisCode::REL_HWHEEL, RelativeAxisCode::REL_HWHEEL_HI_RES)
    }
}

impl ScrollGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new touch started: forget the previous direction, hold again, and
    /// stop any coasting.
    pub fn reset(&mut self) {
        // Leftover sub-notch movement is dropped too, so a new touch never
        // releases a notch early because of the previous scroll.
        self.axes = Default::default();
        self.next_frame = None;
    }

    /// Feed one wheel event. Pushes the events to forward now into `out`.
    pub fn feed(&mut self, ev: InputEvent, now: Instant, cfg: &Scroll, out: &mut Vec<InputEvent>) {
        let code = RelativeAxisCode(ev.code());
        let (axis, hi_res) = match code {
            RelativeAxisCode::REL_WHEEL => (0, false),
            RelativeAxisCode::REL_WHEEL_HI_RES => (0, true),
            RelativeAxisCode::REL_HWHEEL => (1, false),
            RelativeAxisCode::REL_HWHEEL_HI_RES => (1, true),
            _ => {
                out.push(ev);
                return;
            }
        };
        // The finger is on the mouse again: whatever was coasting stops.
        if self.axes.iter().any(|g| g.coast.is_some()) {
            for g in &mut self.axes {
                g.coast = None;
                g.acc = 0.0;
            }
            self.next_frame = None;
        }
        let speed = if cfg.speed > 0.0 { cfg.speed } else { 1.0 };
        let g = &mut self.axes[axis];
        g.trim(now);
        if hi_res {
            let raw = ev.value() as f64;
            let gain = accel_gain(g.speed(now).0, cfg.acceleration);
            let scaled = raw * speed * gain;
            g.acc += scaled;
            g.samples.push_back((now, raw, scaled));
        } else if g.committed.is_none() {
            g.lo_seen += ev.value();
        }
        // After the decision the driver's own notches are ignored: they are
        // regenerated from the hi-res counter below.

        if g.committed.is_none() {
            g.held += 1;
            if !cfg.direction_debounce {
                g.committed = Some(0);
            } else {
                let threshold = cfg.debounce_units.max(1) as f64;
                let decided = if g.acc.abs() >= threshold {
                    Some(g.acc.signum() as i32)
                } else if g.acc == 0.0 && g.lo_seen != 0 {
                    // A hard flick: the driver's hi-res counter missed the
                    // first frame entirely. Use its notches instead.
                    let units = (g.lo_seen * HI_RES_NOTCH) as f64 * speed;
                    g.acc = units;
                    g.samples.push_back((now, (g.lo_seen * HI_RES_NOTCH) as f64, units));
                    Some(g.lo_seen.signum())
                } else {
                    None
                };
                let Some(sign) = decided else { return };
                g.committed = Some(sign);
                log::debug!(
                    "scroll gate: {} axis locked {} after {} event(s), net {:.0} hi-res units",
                    if axis == 0 { "vertical" } else { "horizontal" },
                    if sign > 0 { "+" } else { "-" },
                    g.held,
                    g.acc
                );
            }
        }
        g.lo_seen = 0;
        Self::release(g, axis, cfg, out);
    }

    /// All fingers left the mouse: start coasting on every axis that was
    /// still moving.
    pub fn lift(&mut self, now: Instant, cfg: &Scroll) {
        if !cfg.momentum || !(0.5..1.0).contains(&cfg.momentum_decay) {
            return;
        }
        let mut coasting = false;
        for (axis, g) in self.axes.iter_mut().enumerate() {
            if g.committed.is_none() {
                continue;
            }
            let Some(&(last, _, _)) = g.samples.back() else { continue };
            if now.duration_since(last) > LIFT_GAP {
                continue;
            }
            g.trim(now);
            let (_, v) = g.speed(now);
            if v.abs() < COAST_START {
                continue;
            }
            let v0 = v.clamp(-COAST_MAX, COAST_MAX);
            log::debug!(
                "scroll momentum: {} axis coasting from {:.1} units/ms (lift speed {:.1}), about {:.0} notches",
                if axis == 0 { "vertical" } else { "horizontal" },
                v0,
                v,
                coast_distance(v0, cfg.momentum_decay, 10_000.0).abs() / HI_RES_NOTCH as f64
            );
            g.coast = Some(Coast { start: now, v0, emitted: 0.0 });
            coasting = true;
        }
        if coasting {
            self.next_frame = Some(now);
        }
    }

    /// When `tick` next has something to emit.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.next_frame
    }

    /// Emit the momentum movement due at `now`, if any.
    pub fn tick(&mut self, now: Instant, cfg: &Scroll, out: &mut Vec<InputEvent>) {
        let Some(due) = self.next_frame else { return };
        if now < due {
            return;
        }
        let mut any = false;
        for (axis, g) in self.axes.iter_mut().enumerate() {
            let Some(mut c) = g.coast else { continue };
            let t_ms = now.duration_since(c.start).as_secs_f64() * 1000.0;
            let pos = coast_distance(c.v0, cfg.momentum_decay, t_ms);
            g.acc += pos - c.emitted;
            c.emitted = pos;
            let speed_now = c.v0 * cfg.momentum_decay.powf(t_ms);
            if speed_now.abs() < COAST_STOP {
                g.coast = None;
                // Sub-step leftovers are not worth a late event.
                g.acc = 0.0;
            } else {
                g.coast = Some(c);
                any = true;
            }
            Self::release(g, axis, cfg, out);
        }
        self.next_frame = any.then(|| now + COAST_FRAME);
    }

    /// Hand the accumulated movement to the output in the configured
    /// resolution.
    fn release(g: &mut AxisGate, axis: usize, cfg: &Scroll, out: &mut Vec<InputEvent>) {
        let (lo_code, hi_code) = codes(axis);
        match cfg.mode {
            ScrollMode::Notches => {
                let notches = (g.acc / HI_RES_NOTCH as f64).trunc() as i32;
                if notches != 0 {
                    g.acc -= (notches * HI_RES_NOTCH) as f64;
                    out.push(InputEvent::new(EventType::RELATIVE.0, hi_code.0, notches * HI_RES_NOTCH));
                    out.push(InputEvent::new(EventType::RELATIVE.0, lo_code.0, notches));
                }
            }
            ScrollMode::Smooth => {
                let units = g.acc.trunc() as i32;
                if units != 0 {
                    g.acc -= units as f64;
                    out.push(InputEvent::new(EventType::RELATIVE.0, hi_code.0, units));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wheel_hi(v: i32) -> InputEvent {
        InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_WHEEL_HI_RES.0, v)
    }
    fn wheel_lo(v: i32) -> InputEvent {
        InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_WHEEL.0, v)
    }
    fn values(out: &[InputEvent]) -> Vec<(u16, i32)> {
        out.iter().map(|e| (e.code(), e.value())).collect()
    }
    fn total(out: &[InputEvent], code: u16) -> i32 {
        out.iter().filter(|e| e.code() == code).map(|e| e.value()).sum()
    }
    const HI: u16 = RelativeAxisCode::REL_WHEEL_HI_RES.0;
    const LO: u16 = RelativeAxisCode::REL_WHEEL.0;

    /// The classic configuration most tests were written against: whole
    /// notches, one-notch debounce, no acceleration, no momentum.
    fn classic() -> Scroll {
        Scroll { debounce_units: 120, mode: ScrollMode::Notches, acceleration: 0.0, momentum: false, ..Scroll::default() }
    }

    /// Test time. The driver reports every ~11 ms; 92 hi-res units are one
    /// millimetre, so 12 units per report is a 12 mm/s crawl and 1000 units
    /// per report a 1 m/s flick.
    struct Clock(Instant);
    impl Clock {
        fn new() -> Self {
            Clock(Instant::now())
        }
        fn step(&mut self, ms: u64) -> Instant {
            self.0 += Duration::from_millis(ms);
            self.0
        }
    }

    #[test]
    fn opposite_blip_at_start_is_dropped() {
        let cfg = classic();
        let mut gate = ScrollGate::new();
        let mut out = Vec::new();
        let mut clk = Clock::new();
        // Landing roll: three steps the wrong way.
        for _ in 0..3 {
            gate.feed(wheel_hi(-12), clk.step(11), &cfg, &mut out);
        }
        assert!(out.is_empty(), "held while undecided");
        // Real movement: 13 steps up (156 units); net reaches +120 at step 13.
        for _ in 0..13 {
            gate.feed(wheel_hi(12), clk.step(11), &cfg, &mut out);
        }
        assert_eq!(values(&out), vec![(HI, 120), (LO, 1)], "one notch, hi-res first, nothing negative");
        // Afterwards a reversal is honoured once it adds up to a notch.
        out.clear();
        for _ in 0..10 {
            gate.feed(wheel_hi(-12), clk.step(11), &cfg, &mut out);
        }
        assert_eq!(values(&out), vec![(HI, -120), (LO, -1)]);
    }

    #[test]
    fn driver_notches_are_ignored_once_hi_res_flows() {
        let cfg = classic();
        let mut gate = ScrollGate::new();
        let mut out = Vec::new();
        let mut clk = Clock::new();
        for _ in 0..10 {
            gate.feed(wheel_hi(12), clk.step(11), &cfg, &mut out);
        }
        out.clear();
        gate.feed(wheel_lo(1), clk.step(11), &cfg, &mut out);
        assert!(out.is_empty(), "driver notch must not be forwarded twice");
    }

    #[test]
    fn hard_flick_without_hi_res_still_scrolls() {
        let cfg = classic();
        let mut gate = ScrollGate::new();
        let mut out = Vec::new();
        gate.feed(wheel_lo(-3), Instant::now(), &cfg, &mut out);
        assert_eq!(values(&out), vec![(HI, -360), (LO, -3)]);
    }

    #[test]
    fn smooth_mode_forwards_each_step() {
        let cfg = Scroll { debounce_units: 120, ..classic() };
        let cfg = Scroll { mode: ScrollMode::Smooth, ..cfg };
        let mut gate = ScrollGate::new();
        let mut out = Vec::new();
        let mut clk = Clock::new();
        for _ in 0..10 {
            gate.feed(wheel_hi(12), clk.step(11), &cfg, &mut out);
        }
        assert_eq!(values(&out), vec![(HI, 120)]);
        out.clear();
        gate.feed(wheel_hi(12), clk.step(11), &cfg, &mut out);
        assert_eq!(values(&out), vec![(HI, 12)]);
    }

    #[test]
    fn default_config_starts_after_a_third_of_a_millimetre() {
        let cfg = Scroll::default();
        assert_eq!(cfg.mode, ScrollMode::Smooth);
        let mut gate = ScrollGate::new();
        let mut out = Vec::new();
        let mut clk = Clock::new();
        for _ in 0..3 {
            gate.feed(wheel_hi(12), clk.step(11), &cfg, &mut out);
        }
        assert!(out.is_empty(), "36 units is under the 40-unit threshold");
        gate.feed(wheel_hi(12), clk.step(11), &cfg, &mut out);
        assert_eq!(values(&out), vec![(HI, 48)], "a slow drag is not accelerated");
    }

    #[test]
    fn speed_scales_movement() {
        let cfg = Scroll { speed: 0.5, ..classic() };
        let mut gate = ScrollGate::new();
        let mut out = Vec::new();
        let mut clk = Clock::new();
        for _ in 0..10 {
            gate.feed(wheel_hi(12), clk.step(11), &cfg, &mut out);
        }
        assert!(out.is_empty(), "60 scaled units is below the threshold");
        for _ in 0..10 {
            gate.feed(wheel_hi(12), clk.step(11), &cfg, &mut out);
        }
        assert_eq!(values(&out), vec![(HI, 120), (LO, 1)]);
    }

    #[test]
    fn reset_holds_again_for_the_next_touch() {
        let cfg = classic();
        let mut gate = ScrollGate::new();
        let mut out = Vec::new();
        let mut clk = Clock::new();
        for _ in 0..10 {
            gate.feed(wheel_hi(12), clk.step(11), &cfg, &mut out);
        }
        assert_eq!(out.len(), 2);
        gate.reset();
        out.clear();
        gate.feed(wheel_hi(12), clk.step(11), &cfg, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn debounce_off_still_shapes() {
        let cfg = Scroll { direction_debounce: false, ..classic() };
        let mut gate = ScrollGate::new();
        let mut out = Vec::new();
        let mut clk = Clock::new();
        gate.feed(wheel_hi(-12), clk.step(11), &cfg, &mut out);
        assert!(out.is_empty(), "notches mode waits for a whole notch");
        for _ in 0..9 {
            gate.feed(wheel_hi(-12), clk.step(11), &cfg, &mut out);
        }
        assert_eq!(values(&out), vec![(HI, -120), (LO, -1)]);
    }

    #[test]
    fn axes_are_independent() {
        let cfg = classic();
        let mut gate = ScrollGate::new();
        let mut out = Vec::new();
        let mut clk = Clock::new();
        for _ in 0..10 {
            gate.feed(wheel_hi(12), clk.step(11), &cfg, &mut out);
        }
        assert_eq!(out.len(), 2);
        out.clear();
        gate.feed(InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_HWHEEL_HI_RES.0, 12), clk.step(11), &cfg, &mut out);
        assert!(out.is_empty(), "horizontal still undecided");
    }

    #[test]
    fn acceleration_gain_ramps_with_finger_speed() {
        // 100 units per 11 ms is ~100 mm/s: below the ramp.
        assert_eq!(accel_gain(100.0 / 11.0, 1.0), 1.0);
        // 1 m/s and faster: full gain.
        assert!((accel_gain(UNITS_PER_MM, 1.0) - 2.0).abs() < 1e-9);
        assert!((accel_gain(3.0 * UNITS_PER_MM, 0.5) - 1.5).abs() < 1e-9);
        // Halfway up the ramp: halfway up the gain, smoothly.
        let mid = (ACCEL_FROM_MM_S + ACCEL_TO_MM_S) / 2.0 * UNITS_PER_MM / 1000.0;
        assert!((accel_gain(mid, 1.0) - 1.5).abs() < 1e-9);
        assert_eq!(accel_gain(mid, 0.0), 1.0, "off");
    }

    #[test]
    fn fast_flick_scrolls_further_than_the_raw_steps() {
        let cfg = Scroll { direction_debounce: false, acceleration: 1.0, momentum: false, ..Scroll::default() };
        let mut gate = ScrollGate::new();
        let mut out = Vec::new();
        let mut clk = Clock::new();
        // 1200 units every 11 ms is ~1.2 m/s.
        for _ in 0..10 {
            gate.feed(wheel_hi(1200), clk.step(11), &cfg, &mut out);
        }
        let got = total(&out, HI);
        assert!(got > 12000 && got < 24000, "gain between 1 and 2 over the ramp-up, got {got}");
        // Once the speed has been seen the gain is the full 2x.
        out.clear();
        gate.feed(wheel_hi(1200), clk.step(11), &cfg, &mut out);
        assert_eq!(total(&out, HI), 2400);
    }

    #[test]
    fn momentum_coasts_after_a_moving_lift_and_stops_on_touch() {
        let cfg = Scroll { direction_debounce: false, acceleration: 0.0, ..Scroll::default() };
        let mut gate = ScrollGate::new();
        let mut out = Vec::new();
        let mut clk = Clock::new();
        // 150 units per 11 ms: ~150 mm/s, a brisk scroll (13.6 units/ms,
        // under the coast cap).
        for _ in 0..10 {
            gate.feed(wheel_hi(150), clk.step(11), &cfg, &mut out);
        }
        assert_eq!(total(&out, HI), 1500);
        out.clear();
        let lift = clk.step(5);
        gate.lift(lift, &cfg);
        assert!(gate.next_deadline().is_some(), "coasting");
        let mut coasted = 0;
        let mut frames = 0;
        let mut t = lift;
        while let Some(due) = gate.next_deadline() {
            t = due;
            gate.tick(t, &cfg, &mut out);
            coasted += total(&out, HI);
            out.clear();
            frames += 1;
            assert!(frames < 2000, "must stop on its own");
        }
        let secs = t.duration_since(lift).as_secs_f64();
        // v0 ≈ 1500 / 104 ms ≈ 14.4 units/ms; distance ≈ v0 / 0.002 ≈ 7200
        // units (60 notches), minus the tail under the stop speed.
        assert!(coasted > 5500 && coasted < 7500, "coasted {coasted} units");
        assert!(secs > 0.9 && secs < 2.0, "took {secs:.2} s");
        assert!(gate.next_deadline().is_none());

        // Coast again, then a new touch cuts it short.
        let mut gate = ScrollGate::new();
        for _ in 0..10 {
            gate.feed(wheel_hi(150), clk.step(11), &cfg, &mut out);
        }
        out.clear();
        gate.lift(clk.step(5), &cfg);
        gate.tick(clk.step(8), &cfg, &mut out);
        assert!(total(&out, HI) > 0, "first coasting frame");
        gate.reset();
        assert!(gate.next_deadline().is_none(), "touch stops the coast");
        out.clear();
        gate.tick(clk.step(8), &cfg, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn no_momentum_for_a_finger_that_stopped_or_crawled() {
        let cfg = Scroll { direction_debounce: false, acceleration: 0.0, ..Scroll::default() };
        let mut gate = ScrollGate::new();
        let mut out = Vec::new();
        let mut clk = Clock::new();
        for _ in 0..10 {
            gate.feed(wheel_hi(150), clk.step(11), &cfg, &mut out);
        }
        // Rested 200 ms before lifting.
        gate.lift(clk.step(200), &cfg);
        assert!(gate.next_deadline().is_none(), "a finger that stopped does not coast");

        let mut gate = ScrollGate::new();
        // 20 units per 11 ms is ~20 mm/s: a crawl.
        for _ in 0..10 {
            gate.feed(wheel_hi(20), clk.step(11), &cfg, &mut out);
        }
        gate.lift(clk.step(5), &cfg);
        assert!(gate.next_deadline().is_none(), "a crawl does not coast");

        let off = Scroll { momentum: false, ..cfg };
        let mut gate = ScrollGate::new();
        for _ in 0..10 {
            gate.feed(wheel_hi(150), clk.step(11), &off, &mut out);
        }
        gate.lift(clk.step(5), &off);
        assert!(gate.next_deadline().is_none(), "disabled");
    }

    #[test]
    fn momentum_in_notches_mode_emits_whole_notches() {
        let cfg = Scroll { direction_debounce: false, acceleration: 0.0, mode: ScrollMode::Notches, ..Scroll::default() };
        let mut gate = ScrollGate::new();
        let mut out = Vec::new();
        let mut clk = Clock::new();
        // 200 units per 11 ms: ~200 mm/s.
        for _ in 0..10 {
            gate.feed(wheel_hi(200), clk.step(11), &cfg, &mut out);
        }
        out.clear();
        gate.lift(clk.step(5), &cfg);
        while let Some(due) = gate.next_deadline() {
            gate.tick(due, &cfg, &mut out);
        }
        assert!(out.iter().all(|e| e.code() != HI || e.value() % 120 == 0));
        assert_eq!(total(&out, HI), total(&out, LO) * 120);
        assert!(total(&out, LO) >= 10, "a brisk flick coasts at least ten notches, got {}", total(&out, LO));
    }

    #[test]
    fn coast_speed_is_capped() {
        let cfg = Scroll { direction_debounce: false, acceleration: 0.0, ..Scroll::default() };
        let mut gate = ScrollGate::new();
        let mut out = Vec::new();
        let mut clk = Clock::new();
        // 6000 units per 11 ms: an absurd 6 m/s.
        for _ in 0..10 {
            gate.feed(wheel_hi(6000), clk.step(11), &cfg, &mut out);
        }
        out.clear();
        gate.lift(clk.step(5), &cfg);
        while let Some(due) = gate.next_deadline() {
            gate.tick(due, &cfg, &mut out);
        }
        let coasted = total(&out, HI);
        let cap = (COAST_MAX / -cfg.momentum_decay.ln()) as i32;
        assert!(coasted <= cap && coasted > cap * 8 / 10, "coasted {coasted}, cap {cap}");
    }

    #[test]
    fn coast_distance_formula() {
        // Sum of 0.998^t for t = 0.. tends to 1/(1-0.998) = 500 per unit of speed.
        assert!((coast_distance(1.0, 0.998, 1e6) - 1.0 / -0.998f64.ln()).abs() < 1e-6);
        assert_eq!(coast_distance(1.0, 0.998, 0.0), 0.0);
        assert!(coast_distance(-2.0, 0.998, 100.0) < 0.0);
    }
}
