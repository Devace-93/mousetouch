//! Configuration model. Stored as TOML in
//! `$XDG_CONFIG_HOME/mousetouch/config.toml`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub workspace_swipe: WorkspaceSwipe,
    pub pinch: Pinch,
    pub scroll: Scroll,
    pub taps: Taps,
    #[serde(rename = "gesture")]
    pub gestures: Vec<GestureRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// Extra name match: a device whose evdev name contains this is used
    /// even when its vendor/product ids are not a Magic Mouse's. Real Magic
    /// Mice are recognised by id, whatever they were renamed to on macOS.
    pub device_match: String,
    /// Scroll-wheel events from the mouse driver are dropped while at least
    /// this many fingers touch the surface (prevents scrolling mid-gesture).
    pub suppress_scroll_from_fingers: u8,
}

impl Default for General {
    fn default() -> Self {
        Self {
            device_match: "Magic Mouse".into(),
            suppress_scroll_from_fingers: 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Content follows the fingers (macOS style): swipe left -> next workspace.
    Natural,
    /// Swipe left -> previous workspace.
    Inverted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    Horizontal,
    Vertical,
    /// Lock to whichever direction the fingers take first; the compositor
    /// decides what each means (GNOME: horizontal = workspaces, vertical =
    /// overview).
    Both,
}

/// Continuous, compositor-animated workspace switching. Implemented by
/// replaying the finger motion on a virtual touchpad with `synthetic_fingers`
/// contacts, so the desktop's native swipe gesture handles animation,
/// velocity, pausing and dragging back.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceSwipe {
    pub enabled: bool,
    /// Real fingers on the mouse that trigger the swipe.
    pub fingers: u8,
    pub direction: Direction,
    /// Finger travel multiplier. Higher = less travel needed per workspace.
    pub sensitivity: f64,
    /// Contacts to synthesise on the virtual touchpad (4 = COSMIC, 3 = GNOME).
    pub synthetic_fingers: u8,
    /// Millimetres the fingers must travel before a swipe starts.
    pub activation_mm: f64,
    /// Axis the compositor uses for workspace swipes.
    pub axis: Axis,
    /// Whether the compositor's touchpad "natural scrolling" is on. `None`
    /// means auto-detect (COSMIC / GNOME) and fall back to `false`.
    pub compositor_natural_scroll: Option<bool>,
}

impl Default for WorkspaceSwipe {
    fn default() -> Self {
        Self {
            enabled: true,
            fingers: 2,
            direction: Direction::Natural,
            sensitivity: 0.4,
            synthetic_fingers: 4,
            activation_mm: 2.0,
            axis: Axis::Horizontal,
            compositor_natural_scroll: None,
        }
    }
}

/// Two fingers moving apart or together are replayed as a two-contact pinch
/// on the virtual touchpad, so applications receive the compositor's native
/// pinch-zoom gesture.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Pinch {
    pub enabled: bool,
    pub fingers: u8,
    /// Exponent applied to the finger-distance ratio (1.0 = same zoom as a
    /// touchpad; 2.0 = twice as aggressive).
    pub sensitivity: f64,
    /// Change in finger distance (mm) before a pinch starts.
    pub activation_mm: f64,
}

impl Default for Pinch {
    fn default() -> Self {
        Self { enabled: true, fingers: 2, sensitivity: 1.5, activation_mm: 3.0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScrollMode {
    /// Release movement in whole wheel notches (120 units). Apps treat a
    /// notch as "scroll a few lines"; this is the classic mouse-wheel feel.
    Notches,
    /// Release every high-resolution step as it arrives (1/10 notch). The
    /// default: smooth in browsers and GTK/Qt apps; terminals and some
    /// toolkits only understand notches and scroll faster.
    Smooth,
}

/// One-finger scrolling (the wheel events the kernel driver synthesises).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Scroll {
    /// Hold the first wheel events of every touch until the net movement
    /// clearly points one way, dropping the opposite-direction part. Removes
    /// the short backwards jump a scroll often starts with (the fingertip
    /// rolls on the shell as it lands).
    pub direction_debounce: bool,
    /// Net movement, in high-resolution wheel units (120 = one notch),
    /// needed before the direction is decided and scrolling starts.
    pub debounce_units: i32,
    /// How movement is released once the direction is decided.
    pub mode: ScrollMode,
    /// Multiplier on the scroll distance (1.0 = as the kernel driver
    /// reports it, about one notch per 1.3 mm of finger travel).
    pub speed: f64,
    /// Extra distance for fast finger movement, on top of `speed`: the gain
    /// ramps smoothly from 1 at a slow drag (150 mm/s) to `1 + acceleration`
    /// at a hard flick (1000 mm/s and above). 0 turns it off.
    pub acceleration: f64,
    /// Keep scrolling after the finger lifts while moving, slowing down the
    /// way macOS does; touching the mouse again stops it.
    pub momentum: bool,
    /// Fraction of the speed kept every millisecond while coasting. 0.998 is
    /// Apple's documented deceleration rate (about two seconds from a hard
    /// flick to a stop); 0.99 is their "fast" rate, stopping in half a second.
    pub momentum_decay: f64,
}

impl Default for Scroll {
    fn default() -> Self {
        Self {
            direction_debounce: true,
            debounce_units: 40,
            mode: ScrollMode::Smooth,
            speed: 1.0,
            acceleration: 0.5,
            momentum: true,
            momentum_decay: 0.998,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Taps {
    /// Max duration of a touch to count as a tap.
    pub tap_max_ms: u64,
    /// Max finger travel during a tap. A fingertip rolling onto the Magic
    /// Mouse's curved surface reports 7-13 mm of drift, so this is generous;
    /// `tap_max_lift_speed` is what separates taps from scroll flicks.
    pub tap_max_mm: f64,
    /// Max finger speed (mm/s) just before lifting. A tapping finger comes to
    /// rest before it leaves; a scroll flick is still moving.
    pub tap_max_lift_speed: f64,
    /// Max gap between two taps to count as a double tap.
    pub double_tap_ms: u64,
}

impl Default for Taps {
    fn default() -> Self {
        Self {
            tap_max_ms: 400,
            tap_max_mm: 15.0,
            tap_max_lift_speed: 80.0,
            double_tap_ms: 500,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GestureKind {
    Swipe,
    Tap,
    DoubleTap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SwipeDir {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// Key combination, e.g. "super+w", "ctrl+alt+right". Several combos
    /// separated by commas are pressed in sequence ("ctrl+equal,ctrl+equal").
    Key { combo: String },
    /// Alternate between two key sequences, remembered per focused app:
    /// first trigger sends `on`, the next one `off`.
    Toggle { on: String, off: String },
    /// Walk through several key sequences, one per trigger, wrapping around;
    /// position remembered per focused app. Used for smart zoom levels.
    Cycle { steps: Vec<String> },
    /// Mouse button click: "left", "right" or "middle".
    Button { button: String },
    /// Shell command, run with `sh -c`.
    Command { command: String },
}

/// A discrete gesture: fires its action once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GestureRule {
    pub kind: GestureKind,
    pub fingers: u8,
    /// Only for `swipe`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<SwipeDir>,
    pub action: Action,
    /// Only fire when the focused window's app id contains this text
    /// (case-insensitive), e.g. "firefox". Rules with `app` win over generic
    /// ones. Needs a compositor exposing toplevel info (COSMIC, KDE, Sway,
    /// Hyprland); see focus.rs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: General::default(),
            workspace_swipe: WorkspaceSwipe::default(),
            pinch: Pinch::default(),
            scroll: Scroll::default(),
            taps: Taps::default(),
            gestures: vec![
                GestureRule {
                    kind: GestureKind::DoubleTap,
                    fingers: 2,
                    direction: None,
                    action: Action::Key { combo: default_overview_combo() },
                    app: None,
                    enabled: true,
                },
                GestureRule {
                    kind: GestureKind::Tap,
                    fingers: 3,
                    direction: None,
                    action: Action::Key { combo: default_app_grid_combo() },
                    app: None,
                    enabled: true,
                },
            ],
        }
    }
}

/// Keyboard shortcut that shows the workspaces on the current desktop.
fn default_overview_combo() -> String {
    match std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        d if d.contains("cosmic") => "super+w".into(),
        d if d.contains("gnome") => "super".into(),
        d if d.contains("kde") => "super+w".into(),
        _ => "super+w".into(),
    }
}

/// Keyboard shortcut that shows all installed applications (COSMIC App
/// Library, GNOME app grid, KDE launcher).
fn default_app_grid_combo() -> String {
    match std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_lowercase().as_str() {
        d if d.contains("kde") => "super".into(),
        _ => "super+a".into(),
    }
}

pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into());
            home.join(".config")
        });
    base.join("mousetouch").join("config.toml")
}

pub fn load(path: &Path) -> Result<Config> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(cfg)
}

/// Load the config, writing a commented default file if none exists.
pub fn load_or_create(path: &Path) -> Result<Config> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let cfg = Config::default();
        fs::write(path, to_toml(&cfg)?)?;
        log::info!("wrote default config to {}", path.display());
        return Ok(cfg);
    }
    load(path)
}

pub fn to_toml(cfg: &Config) -> Result<String> {
    let body = toml::to_string_pretty(cfg)?;
    Ok(format!(
        "# mousetouch configuration\n\
         # Edit by hand or with `mousetouch-settings`. The daemon reloads on save.\n\n{body}"
    ))
}

/// Resolve `compositor_natural_scroll` when it is set to auto.
pub fn detect_compositor_natural_scroll() -> bool {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_lowercase();
    if desktop.contains("cosmic") {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let candidates = [
            home.join(".config/cosmic/com.system76.CosmicComp/v1/input_touchpad"),
            PathBuf::from("/usr/share/cosmic/com.system76.CosmicComp/v1/input_touchpad"),
        ];
        for p in candidates {
            if let Ok(text) = fs::read_to_string(&p) {
                // RON: `natural_scroll: Some(true)` / `natural_scroll: None`
                let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
                if compact.contains("natural_scroll:Some(true)") {
                    return true;
                }
                if compact.contains("natural_scroll:Some(false)") || compact.contains("natural_scroll:None") {
                    return false;
                }
            }
        }
        return false;
    }
    if desktop.contains("gnome") {
        if let Ok(out) = std::process::Command::new("gsettings")
            .args(["get", "org.gnome.desktop.peripherals.touchpad", "natural-scroll"])
            .output()
        {
            return String::from_utf8_lossy(&out.stdout).trim() == "true";
        }
    }
    false
}
