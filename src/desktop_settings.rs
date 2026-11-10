//! What the desktop expects from a gesture, read from its own settings so the
//! defaults follow the user's: contacts for a workspace swipe, touchpad
//! natural scrolling, and the shortcuts that open the workspaces overview and
//! the application library. GNOME keeps them in dconf, COSMIC in RON files,
//! KDE in INI files; each is watched, so a shortcut changed in the desktop's
//! settings applies to the next gesture without a restart. Pantheon
//! (elementary OS) keeps them in dconf like GNOME, under Gala's own keys.
//! The axis workspaces are laid out on is detected the same way (COSMIC
//! lets the user switch between a horizontal and a vertical layout).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::config::Axis;
use crate::virt::parse_combo;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Desktop {
    Cosmic,
    Gnome,
    Kde,
    Pantheon,
    Other,
}

/// Resolved values; `mousetouch desktop` prints them as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DesktopSettings {
    pub desktop: Desktop,
    /// Contacts the compositor's workspace swipe wants (fixed per desktop).
    pub synthetic_fingers: u8,
    /// The compositor's touchpad "natural scrolling" switch.
    pub natural_scroll: bool,
    /// Shortcut that shows the workspaces.
    pub workspaces_combo: String,
    /// Shortcut that shows every installed application.
    pub apps_combo: String,
    /// Direction(s) the desktop's own touchpad swipe understands: COSMIC
    /// follows its workspace layout setting; horizontal elsewhere. GNOME and
    /// KDE also open the overview with a vertical swipe, but `both` is left
    /// to an explicit choice until that is tested on real installs.
    pub workspace_axis: Axis,
    /// Multiplier on the workspace swipe's travel on the virtual touchpad.
    /// 1 where the compositor reads libinput's gesture deltas itself. On
    /// Pantheon's X11 session Touchégg recognises the swipe and measures it
    /// against the size of the touchpad, and Gala undoes a switch that ends
    /// under 20 % of that: on our large virtual pad a normal swipe on the
    /// mouse never got there, so the workspace changed and came back.
    pub swipe_gain: u8,
}

/// `swipe_gain` when Touchégg drives the gesture: with the default
/// sensitivity (0.4) a 25 mm swipe on the mouse becomes 40 mm on the pad,
/// the shortest travel Gala kept in testing (elementary OS 7.1, Touchégg 2.0.14).
const TOUCHEGG_SWIPE_GAIN: u8 = 4;

pub type Shared = Arc<RwLock<DesktopSettings>>;

impl DesktopSettings {
    pub fn fallback(desktop: Desktop) -> Self {
        let (synthetic_fingers, workspaces_combo, apps_combo, workspace_axis) = match desktop {
            Desktop::Gnome => (3, "super", "super+a", Axis::Horizontal),
            Desktop::Kde => (4, "super+w", "super", Axis::Horizontal),
            // cosmic-comp's default layout when no setting is stored.
            Desktop::Cosmic => (4, "super+w", "super+a", Axis::Vertical),
            // Gala: multitasking view and the applications menu.
            Desktop::Pantheon => (4, "super+down", "super+space", Axis::Horizontal),
            Desktop::Other => (4, "super+w", "super+a", Axis::Horizontal),
        };
        Self {
            desktop,
            synthetic_fingers,
            natural_scroll: false,
            workspaces_combo: workspaces_combo.into(),
            apps_combo: apps_combo.into(),
            workspace_axis,
            swipe_gain: 1,
        }
    }
}

pub fn current_desktop() -> Desktop {
    let name = std::env::var("XDG_CURRENT_DESKTOP")
        .or_else(|_| std::env::var("DESKTOP_SESSION"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    match classify(&name) {
        Desktop::Other => desktop_from_processes(),
        d => d,
    }
}

fn classify(name: &str) -> Desktop {
    if name.contains("cosmic") {
        Desktop::Cosmic
    } else if name.contains("pantheon") {
        // Before GNOME: some sessions announce `Pantheon:GNOME`.
        Desktop::Pantheon
    } else if name.contains("gnome") || name.contains("ubuntu") {
        Desktop::Gnome
    } else if name.contains("kde") || name.contains("plasma") {
        Desktop::Kde
    } else {
        Desktop::Other
    }
}

/// A service started without the session's variables: look at what runs.
fn desktop_from_processes() -> Desktop {
    let Ok(procs) = std::fs::read_dir("/proc") else { return Desktop::Other };
    for entry in procs.flatten() {
        let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else { continue };
        match comm.trim() {
            "cosmic-comp" => return Desktop::Cosmic,
            "gnome-shell" => return Desktop::Gnome,
            "kwin_wayland" | "kwin_x11" => return Desktop::Kde,
            "gala" => return Desktop::Pantheon,
            _ => {}
        }
    }
    Desktop::Other
}

/// Is a process with this `comm` running?
fn process_running(name: &str) -> bool {
    let Ok(procs) = std::fs::read_dir("/proc") else { return false };
    procs
        .flatten()
        .any(|entry| std::fs::read_to_string(entry.path().join("comm")).map_or(false, |comm| comm.trim() == name))
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".config"))
}

fn data_dirs() -> Vec<PathBuf> {
    let raw = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    raw.split(':').filter(|s| !s.is_empty()).map(PathBuf::from).collect()
}

/// Files whose change means the settings may have changed.
fn watched_files(desktop: Desktop) -> Vec<PathBuf> {
    let cfg = config_home();
    match desktop {
        Desktop::Gnome | Desktop::Pantheon => vec![cfg.join("dconf/user")],
        Desktop::Cosmic => vec![
            cfg.join("cosmic/com.system76.CosmicSettings.Shortcuts/v1/custom"),
            cfg.join("cosmic/com.system76.CosmicComp/v1/input_touchpad"),
            cfg.join("cosmic/com.system76.CosmicComp/v1/workspaces"),
        ],
        Desktop::Kde => vec![cfg.join("kglobalshortcutsrc"), cfg.join("kcminputrc")],
        Desktop::Other => Vec::new(),
    }
}

pub fn detect() -> DesktopSettings {
    let desktop = current_desktop();
    let mut s = DesktopSettings::fallback(desktop);
    match desktop {
        Desktop::Gnome => detect_gnome(&mut s),
        Desktop::Cosmic => detect_cosmic(&mut s),
        Desktop::Kde => detect_kde(&mut s),
        Desktop::Pantheon => detect_pantheon(&mut s),
        Desktop::Other => {}
    }
    s
}

/// Detect now and keep the result current in the background. `on_change`
/// runs with the first result and after every change (the settings app
/// follows it through the event stream).
pub fn watch(on_change: impl Fn(&DesktopSettings) + Send + 'static) -> Shared {
    let settings = detect();
    log::info!(
        "desktop: {:?}, swipe contacts {}, natural scroll {}, workspaces '{}', apps '{}', axis {:?}, swipe gain {}",
        settings.desktop, settings.synthetic_fingers, settings.natural_scroll, settings.workspaces_combo, settings.apps_combo,
        settings.workspace_axis, settings.swipe_gain
    );
    on_change(&settings);
    let shared = Arc::new(RwLock::new(settings));
    let files = watched_files(shared.read().unwrap().desktop);
    if files.is_empty() {
        return shared;
    }
    let out = shared.clone();
    thread::Builder::new()
        .name("desktop-settings".into())
        .spawn(move || {
            let mut last = mtimes(&files);
            loop {
                thread::sleep(Duration::from_secs(1));
                let now = mtimes(&files);
                if now == last {
                    continue;
                }
                last = now;
                // Settings apps write several keys in a row.
                thread::sleep(Duration::from_millis(300));
                let fresh = detect();
                let mut cur = out.write().unwrap();
                if *cur != fresh {
                    log::info!(
                        "desktop settings changed: swipe contacts {}, natural scroll {}, workspaces '{}', apps '{}', axis {:?}",
                        fresh.synthetic_fingers, fresh.natural_scroll, fresh.workspaces_combo, fresh.apps_combo, fresh.workspace_axis
                    );
                    *cur = fresh.clone();
                    drop(cur);
                    on_change(&fresh);
                }
            }
        })
        .expect("spawning desktop settings watcher");
    shared
}

fn mtimes(files: &[PathBuf]) -> Vec<Option<SystemTime>> {
    files.iter().map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok()).collect()
}

/// Keep `combo` only if the virtual keyboard can type it.
fn accept(combo: Option<String>, slot: &mut String) {
    if let Some(c) = combo {
        match parse_combo(&c) {
            Ok(_) => *slot = c,
            Err(e) => log::warn!("ignoring desktop shortcut '{c}': {e}"),
        }
    }
}

/// X keysym / libxkbcommon key name to the names `parse_combo` knows.
fn key_name(sym: &str) -> String {
    let lower = sym.to_ascii_lowercase();
    match lower.as_str() {
        "page_up" | "pgup" | "prior" => "pageup".into(),
        "page_down" | "pgdown" | "next" => "pagedown".into(),
        "return" | "enter" | "kp_enter" => "enter".into(),
        "escape" | "esc" => "esc".into(),
        "backspace" => "backspace".into(),
        "del" => "delete".into(),
        "ins" => "insert".into(),
        "kp_add" => "kpplus".into(),
        "kp_subtract" => "kpminus".into(),
        "print" | "sysreq" => "print".into(),
        "audioraisevolume" | "volume up" => "volumeup".into(),
        "audiolowervolume" | "volume down" => "volumedown".into(),
        "audiomute" | "volume mute" => "mute".into(),
        _ => lower,
    }
}

fn modifier_name(m: &str) -> Option<&'static str> {
    Some(match m.to_ascii_lowercase().as_str() {
        "super" | "meta" | "mod4" | "logo" | "win" => "super",
        "control" | "ctrl" | "primary" => "ctrl",
        "alt" | "mod1" => "alt",
        "shift" => "shift",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// GNOME: dconf through gsettings
// ---------------------------------------------------------------------------

fn gsettings(schema: &str, key: &str) -> Option<String> {
    let out = std::process::Command::new("gsettings").args(["get", schema, key]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// First element of a GVariant string array: `['<Super>s', '<Super>w']`.
fn first_quoted(v: &str) -> Option<String> {
    let start = v.find('\'')? + 1;
    let end = start + v[start..].find('\'')?;
    let s = &v[start..end];
    (!s.is_empty()).then(|| s.to_string())
}

/// GTK accelerator (`<Super><Shift>s`) to `super+shift+s`.
pub fn combo_from_gtk_accel(accel: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut rest = accel.trim();
    while let Some(stripped) = rest.strip_prefix('<') {
        let end = stripped.find('>')?;
        parts.push(modifier_name(&stripped[..end])?.to_string());
        rest = &stripped[end + 1..];
    }
    if !rest.is_empty() {
        parts.push(key_name(rest));
    }
    (!parts.is_empty()).then(|| parts.join("+"))
}

fn detect_gnome(s: &mut DesktopSettings) {
    if let Some(v) = gsettings("org.gnome.desktop.peripherals.touchpad", "natural-scroll") {
        s.natural_scroll = v == "true";
    }
    // The bare Super key opens the overview through mutter's overlay key;
    // when that is unset the shell's own binding is what works.
    let overlay = gsettings("org.gnome.mutter", "overlay-key").and_then(|v| first_quoted(&v));
    let workspaces = match overlay.as_deref() {
        Some(k) if k.to_ascii_lowercase().starts_with("super") => Some("super".to_string()),
        _ => gsettings("org.gnome.shell.keybindings", "toggle-overview")
            .and_then(|v| first_quoted(&v))
            .and_then(|a| combo_from_gtk_accel(&a)),
    };
    accept(workspaces, &mut s.workspaces_combo);
    let apps = gsettings("org.gnome.shell.keybindings", "toggle-application-view")
        .and_then(|v| first_quoted(&v))
        .and_then(|a| combo_from_gtk_accel(&a));
    accept(apps, &mut s.apps_combo);
}

// ---------------------------------------------------------------------------
// Pantheon (elementary OS): dconf through gsettings, Gala's keys
// ---------------------------------------------------------------------------

fn detect_pantheon(s: &mut DesktopSettings) {
    if let Some(v) = gsettings("org.gnome.desktop.peripherals.touchpad", "natural-scroll") {
        s.natural_scroll = v == "true";
    }
    // Gala reuses mutter's `show-desktop` for its multitasking view and
    // `panel-main-menu` for the applications menu.
    let binding = |key: &str| {
        gsettings("org.gnome.desktop.wm.keybindings", key).and_then(|v| first_quoted(&v)).and_then(|a| combo_from_gtk_accel(&a))
    };
    accept(binding("show-desktop"), &mut s.workspaces_combo);
    accept(binding("panel-main-menu"), &mut s.apps_combo);
    let gesture = |key: &str| gsettings("io.elementary.desktop.wm.gestures", key);
    if let Some(n) = pantheon_swipe_fingers(
        gesture("three-finger-swipe-horizontal").as_deref(),
        gesture("four-finger-swipe-horizontal").as_deref(),
    ) {
        s.synthetic_fingers = n;
    }
    // The X11 session: Touchégg recognises touchpad gestures for Gala.
    if process_running("touchegg") {
        s.swipe_gain = TOUCHEGG_SWIPE_GAIN;
    }
}

/// Contacts that switch workspaces, from the two `*-finger-swipe-horizontal`
/// values of `io.elementary.desktop.wm.gestures` (`'switch-to-workspace'` or
/// `'none'`). Four when both do: three fingers also maximise with a swipe up.
/// `None` when neither is set to it or the schema is missing.
pub fn pantheon_swipe_fingers(three: Option<&str>, four: Option<&str>) -> Option<u8> {
    let switches = |v: Option<&str>| v.map_or(false, |v| v.trim().trim_matches('\'') == "switch-to-workspace");
    if switches(four) {
        Some(4)
    } else if switches(three) {
        Some(3)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// COSMIC: RON files
// ---------------------------------------------------------------------------

fn detect_cosmic(s: &mut DesktopSettings) {
    s.natural_scroll = cosmic_natural_scroll();
    if let Some(axis) = cosmic_workspace_axis() {
        s.workspace_axis = axis;
    }
    let shortcuts = cosmic_shortcuts();
    accept(shortcuts.get("System(WorkspaceOverview)").cloned(), &mut s.workspaces_combo);
    accept(shortcuts.get("System(AppLibrary)").cloned(), &mut s.apps_combo);
}

fn cosmic_natural_scroll() -> bool {
    let mut candidates = vec![config_home().join("cosmic/com.system76.CosmicComp/v1/input_touchpad")];
    candidates.extend(data_dirs().into_iter().map(|d| d.join("cosmic/com.system76.CosmicComp/v1/input_touchpad")));
    for p in candidates {
        if let Ok(text) = std::fs::read_to_string(&p) {
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
    false
}

/// COSMIC's workspace layout: the user's `workspaces` entry, else the
/// shipped default. `None` when neither says (cosmic-comp then uses its
/// built-in default, vertical, which `fallback` already holds).
fn cosmic_workspace_axis() -> Option<Axis> {
    let rel = "cosmic/com.system76.CosmicComp/v1/workspaces";
    let mut candidates = vec![config_home().join(rel)];
    candidates.extend(data_dirs().into_iter().map(|d| d.join(rel)));
    candidates.iter().find_map(|p| parse_cosmic_workspace_layout(&std::fs::read_to_string(p).ok()?))
}

/// `workspace_layout: Horizontal` in a RON `WorkspaceConfig`.
pub fn parse_cosmic_workspace_layout(text: &str) -> Option<Axis> {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.contains("workspace_layout:Horizontal") {
        Some(Axis::Horizontal)
    } else if compact.contains("workspace_layout:Vertical") {
        Some(Axis::Vertical)
    } else {
        None
    }
}

/// Action -> combo, from the shipped defaults with the user's `custom` file
/// applied on top (a custom binding to `Disable` removes a default).
fn cosmic_shortcuts() -> HashMap<String, String> {
    let rel = "cosmic/com.system76.CosmicSettings.Shortcuts/v1";
    let mut bindings: Vec<(String, String)> = Vec::new();
    for dir in data_dirs() {
        if let Ok(text) = std::fs::read_to_string(dir.join(rel).join("defaults")) {
            bindings = parse_cosmic_bindings(&text);
            break;
        }
    }
    if let Ok(text) = std::fs::read_to_string(config_home().join(rel).join("custom")) {
        for (binding, action) in parse_cosmic_bindings(&text) {
            bindings.retain(|(b, _)| *b != binding);
            if action != "Disable" {
                bindings.insert(0, (binding, action));
            }
        }
    }
    let mut by_action = HashMap::new();
    for (binding, action) in bindings {
        by_action.entry(action).or_insert(binding);
    }
    by_action
}

/// `(modifiers: [Super, Shift], key: "w"): System(WorkspaceOverview),` pairs
/// from a RON map, as (`super+shift+w`, `System(WorkspaceOverview)`).
pub fn parse_cosmic_bindings(text: &str) -> Vec<(String, String)> {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let mut out = Vec::new();
    let mut rest = compact.as_str();
    while let Some(start) = rest.find("(modifiers:[") {
        let after = &rest[start..];
        let Some(close) = after.find(')') else { break };
        let binding = &after[1..close];
        let Some(tail) = after[close + 1..].strip_prefix(':') else {
            rest = &after[close + 1..];
            continue;
        };
        let action_len = action_end(tail);
        let action = &tail[..action_len];
        if let Some(combo) = combo_from_cosmic_binding(binding) {
            out.push((combo, action.to_string()));
        }
        rest = &tail[action_len..];
    }
    out
}

/// Length of a RON value up to the next top-level `,` or `}`.
fn action_end(s: &str) -> usize {
    let mut depth = 0i32;
    let mut in_str = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_str = !in_str,
            _ if in_str => {}
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' | '}' if depth <= 0 => return i,
            _ => {}
        }
    }
    s.len()
}

/// `modifiers:[Super,Shift],key:"w"` to `super+shift+w`.
fn combo_from_cosmic_binding(binding: &str) -> Option<String> {
    let mods_start = binding.find("modifiers:[")? + "modifiers:[".len();
    let mods_end = mods_start + binding[mods_start..].find(']')?;
    let mut parts: Vec<String> = binding[mods_start..mods_end]
        .split(',')
        .filter(|m| !m.is_empty())
        .map(|m| modifier_name(m).map(str::to_string))
        .collect::<Option<_>>()?;
    if let Some(k) = binding[mods_end..].find("key:\"") {
        let key_start = mods_end + k + "key:\"".len();
        let key_end = key_start + binding[key_start..].find('"')?;
        parts.push(key_name(&binding[key_start..key_end]));
    }
    (!parts.is_empty()).then(|| parts.join("+"))
}

// ---------------------------------------------------------------------------
// KDE: kglobalshortcutsrc / kcminputrc
// ---------------------------------------------------------------------------

fn detect_kde(s: &mut DesktopSettings) {
    let cfg = config_home();
    if let Ok(text) = std::fs::read_to_string(cfg.join("kcminputrc")) {
        // libinput touchpad settings live in a per-device [Libinput][...]
        // group; the key only appears for touchpads.
        s.natural_scroll = text.lines().any(|l| l.trim() == "NaturalScroll=true");
    }
    if let Ok(text) = std::fs::read_to_string(cfg.join("kglobalshortcutsrc")) {
        accept(kde_shortcut(&text, "kwin", "Overview").and_then(|k| combo_from_kde_key(&k)), &mut s.workspaces_combo);
        accept(
            kde_shortcut(&text, "plasmashell", "activate application launcher").and_then(|k| combo_from_kde_key(&k)),
            &mut s.apps_combo,
        );
    }
}

/// Active key of `name` in `[group]`: the value is `active,default,label`,
/// several active keys are separated by a tab (written `\t`). `None` when the
/// entry is missing or explicitly `none`.
pub fn kde_shortcut(text: &str, group: &str, name: &str) -> Option<String> {
    let mut in_group = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == format!("[{group}]");
            continue;
        }
        if !in_group {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        if k.trim() != name {
            continue;
        }
        let active = v.split(',').next().unwrap_or("").trim();
        let first = active.split("\\t").next().unwrap_or("").split('\t').next().unwrap_or("").trim();
        return (!first.is_empty() && first != "none").then(|| first.to_string());
    }
    None
}

/// `Meta+Shift+W` to `super+shift+w`.
pub fn combo_from_kde_key(key: &str) -> Option<String> {
    let parts: Vec<&str> = key.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
    let (last, mods) = parts.split_last()?;
    let mut out: Vec<String> = mods.iter().map(|m| modifier_name(m).map(str::to_string)).collect::<Option<_>>()?;
    out.push(modifier_name(last).map(str::to_string).unwrap_or_else(|| key_name(last)));
    Some(out.join("+"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gtk_accelerators() {
        assert_eq!(combo_from_gtk_accel("<Super>s").as_deref(), Some("super+s"));
        assert_eq!(combo_from_gtk_accel("<Control><Alt>Page_Up").as_deref(), Some("ctrl+alt+pageup"));
        assert_eq!(combo_from_gtk_accel("<Super><Shift>d").as_deref(), Some("super+shift+d"));
        assert_eq!(combo_from_gtk_accel(""), None);
        assert_eq!(first_quoted("['<Super>a']").as_deref(), Some("<Super>a"));
        assert_eq!(first_quoted("@as []"), None);
    }

    #[test]
    fn cosmic_bindings() {
        let ron = r#"{
            (modifiers: [Super], key: "a"): System(AppLibrary),
            (modifiers: [Super, Shift], key: "w"): System(WorkspaceOverview),
            (modifiers: [Super]): System(Launcher),
            (modifiers: [Super], key: "t"): Spawn("foo,bar"),
            (modifiers: [Super], key: "Left"): Focus(Left),
        }"#;
        let b = parse_cosmic_bindings(ron);
        assert_eq!(b[0], ("super+a".to_string(), "System(AppLibrary)".to_string()));
        assert_eq!(b[1], ("super+shift+w".to_string(), "System(WorkspaceOverview)".to_string()));
        assert_eq!(b[2], ("super".to_string(), "System(Launcher)".to_string()));
        assert_eq!(b[3].1, "Spawn(\"foo,bar\")");
        assert_eq!(b[4], ("super+left".to_string(), "Focus(Left)".to_string()));
    }

    #[test]
    fn kde_shortcuts() {
        let ini = "[kwin]\nOverview=Meta+W,Meta+W,Toggle Overview\nShow Desktop=none,Meta+D,Peek at Desktop\n\n[plasmashell]\nactivate application launcher=Alt+F1\\tMeta,Alt+F1\\tMeta,Activate Application Launcher\n";
        assert_eq!(kde_shortcut(ini, "kwin", "Overview").as_deref(), Some("Meta+W"));
        assert_eq!(kde_shortcut(ini, "kwin", "Show Desktop"), None);
        assert_eq!(kde_shortcut(ini, "plasmashell", "activate application launcher").as_deref(), Some("Alt+F1"));
        assert_eq!(combo_from_kde_key("Meta+W").as_deref(), Some("super+w"));
        assert_eq!(combo_from_kde_key("Meta").as_deref(), Some("super"));
        assert_eq!(combo_from_kde_key("Ctrl+Alt+PgUp").as_deref(), Some("ctrl+alt+pageup"));
    }

    #[test]
    fn cosmic_workspace_layout() {
        let ron = "(\n    workspace_mode: OutputBound,\n    workspace_layout: Horizontal,\n    workspace_wraparound: true,\n)";
        assert_eq!(parse_cosmic_workspace_layout(ron), Some(Axis::Horizontal));
        assert_eq!(parse_cosmic_workspace_layout("(workspace_layout: Vertical)"), Some(Axis::Vertical));
        // An entry without the key (older cosmic-comp) says nothing.
        assert_eq!(parse_cosmic_workspace_layout("(workspace_mode: Global)"), None);
        assert_eq!(DesktopSettings::fallback(Desktop::Cosmic).workspace_axis, Axis::Vertical);
        assert_eq!(DesktopSettings::fallback(Desktop::Gnome).workspace_axis, Axis::Horizontal);
        assert_eq!(DesktopSettings::fallback(Desktop::Other).workspace_axis, Axis::Horizontal);
    }

    #[test]
    fn pantheon() {
        assert_eq!(classify("pantheon"), Desktop::Pantheon);
        assert_eq!(classify("pantheon:gnome"), Desktop::Pantheon);
        assert_eq!(classify("ubuntu:gnome"), Desktop::Gnome);
        let s = DesktopSettings::fallback(Desktop::Pantheon);
        assert_eq!((s.synthetic_fingers, s.workspaces_combo.as_str(), s.apps_combo.as_str()), (4, "super+down", "super+space"));
        assert_eq!((s.workspace_axis, s.swipe_gain), (Axis::Horizontal, 1));
        assert!(parse_combo(&s.workspaces_combo).is_ok() && parse_combo(&s.apps_combo).is_ok());
        // Gala's defaults as gsettings prints them.
        assert_eq!(combo_from_gtk_accel(&first_quoted("['<Super>Down', '<Super>s']").unwrap()).as_deref(), Some("super+down"));
        assert_eq!(combo_from_gtk_accel(&first_quoted("['<Super>space', '<Alt>F2']").unwrap()).as_deref(), Some("super+space"));
        assert_eq!(pantheon_swipe_fingers(Some("'switch-to-workspace'"), Some("'switch-to-workspace'")), Some(4));
        assert_eq!(pantheon_swipe_fingers(Some("'switch-to-workspace'"), Some("'none'")), Some(3));
        assert_eq!(pantheon_swipe_fingers(Some("'none'"), Some("'none'")), None);
        assert_eq!(pantheon_swipe_fingers(None, None), None);
    }

    #[test]
    fn unknown_desktop_keeps_the_old_defaults() {
        let s = DesktopSettings::fallback(Desktop::Other);
        assert_eq!((s.synthetic_fingers, s.workspaces_combo.as_str(), s.apps_combo.as_str()), (4, "super+w", "super+a"));
        assert_eq!(DesktopSettings::fallback(Desktop::Gnome).synthetic_fingers, 3);
    }
}
