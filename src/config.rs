//! Optional `~/.config/spawnhere/config.toml`. All fields are optional —
//! spawnhere runs fine with no file. The config drives:
//!
//! * `default_term` — which exec to spawn in `--term` mode if `$TERMINAL` is
//!   unset. Falls back to `"kitty"` if neither is set.
//! * `[rules.<class>]` — per-app bbox post-processing (min size, cell snap).
//!   `<class>` is matched against the first whitespace-separated token of the
//!   chosen exec. So `kitty -1` and `kitty --class foo` both match `[rules.kitty]`.
//! * `[gesture]` — which drag gesture is default and which modifiers select the
//!   other modes. Defaults match the Photoshop/Figma convention: plain drag is
//!   a rectangle, `Shift` constrains to 1:1, `Ctrl` switches to freehand.

use crate::stroke::Bbox;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub default_term: Option<String>,
    #[serde(default)]
    pub rules: HashMap<String, Rule>,
    #[serde(default)]
    pub gesture: GestureConfig,
}

#[derive(Debug, Deserialize, Default, Clone, Copy)]
pub struct Rule {
    /// Per-app floor — bumps bbox.w below this up to this.
    #[serde(default)]
    pub min_width: Option<u32>,
    #[serde(default)]
    pub min_height: Option<u32>,
    /// `[cell_w, cell_h]` in pixels. If present, bbox.w and bbox.h are rounded
    /// *down* to the nearest multiple so terminal windows land on cell
    /// boundaries — a drawn rectangle becomes a clean N×M cell grid.
    #[serde(default)]
    pub cell_px: Option<[u32; 2]>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct GestureConfig {
    /// Mode used when no matching modifier is held. Defaults to `Rectangle` —
    /// set `default = "freehand"` to restore the pre-rectangle-default era.
    pub default: GestureMode,
    /// Modifier that constrains to a 1:1 square. `None` disables the shortcut.
    pub square_modifier: Option<Modifier>,
    /// Modifier that switches to freehand. `None` disables the shortcut.
    pub freehand_modifier: Option<Modifier>,
    /// Pixels of cursor travel required before a press turns into a drag.
    /// Anything shorter is treated as a click (natural-size spawn at the press
    /// point). Prevents micro-jitter from producing sliver windows.
    pub drag_threshold_px: f32,
    /// Show the live `W×H` readout near the cursor while dragging a rectangle.
    pub show_dimensions: bool,
    /// Global floor for any drawn bbox (rectangle or freehand). A tiny drag
    /// that would produce an unusable sliver gets bumped up to this size,
    /// re-centered on the drawn region. Per-app `[rules.<class>]` can still
    /// raise this further. Set to 0 to disable.
    pub min_width: u32,
    pub min_height: u32,
    /// Default size used when the user spawns with a click (no drag). None =
    /// legacy behaviour (app's natural size). Given values, the click acts as
    /// "center of a window this big" — quick way to drop a floating window
    /// without measuring it. Per-app rules don't apply to clicks (spawnhere
    /// treats clicks as "open at natural size" historically); these keys keep
    /// that "click = shortcut" philosophy but give the shortcut a sensible
    /// footprint instead of whatever the app's default window size is.
    pub click_spawn_width: Option<u32>,
    pub click_spawn_height: Option<u32>,
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            default: GestureMode::Rectangle,
            square_modifier: Some(Modifier::Shift),
            // Alt (not Ctrl) because Ctrl is already overloaded for
            // "escape from --default into the picker" at release time. A
            // single Ctrl+drag would silently do both things — the user
            // would see the stroke change shape while also opening the
            // picker they may not have wanted.
            freehand_modifier: Some(Modifier::Alt),
            drag_threshold_px: 5.0,
            show_dimensions: true,
            min_width: 320,
            min_height: 200,
            click_spawn_width: Some(720),
            click_spawn_height: Some(480),
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum GestureMode {
    #[default]
    Rectangle,
    Freehand,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Modifier {
    Shift,
    Ctrl,
    Alt,
}

/// Effective gesture for a given modifier state, given the user's config.
/// `Square` is modelled separately from `Rectangle` so the renderer can draw
/// the 1:1 constraint and the HUD can annotate the readout with `(1:1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveMode {
    Rectangle,
    Square,
    Freehand,
}

impl GestureConfig {
    pub fn resolve(&self, shift: bool, ctrl: bool, alt: bool) -> EffectiveMode {
        let held = |m: Option<Modifier>| match m {
            Some(Modifier::Shift) => shift,
            Some(Modifier::Ctrl) => ctrl,
            Some(Modifier::Alt) => alt,
            None => false,
        };
        // Priority: freehand wins over square when both are held, so users can
        // opt into freehand even if Shift is latched from a previous gesture.
        if held(self.freehand_modifier) {
            return EffectiveMode::Freehand;
        }
        if held(self.square_modifier) {
            // Square only makes sense when the default is rectangle; if the
            // user flipped default to freehand, treat Shift as "constrain the
            // freehand bbox" — which we approximate as Rectangle since the
            // freehand stroke doesn't have an aspect constraint anyway.
            return match self.default {
                GestureMode::Rectangle => EffectiveMode::Square,
                GestureMode::Freehand => EffectiveMode::Rectangle,
            };
        }
        match self.default {
            GestureMode::Rectangle => EffectiveMode::Rectangle,
            GestureMode::Freehand => EffectiveMode::Freehand,
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                eprintln!("spawnhere: {} — ignoring: {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Look up a rule by the exec's leading token. Returns `None` if no rule
    /// matches — callers apply only the CLI-provided defaults in that case.
    pub fn rule_for(&self, exec: &str) -> Option<&Rule> {
        let class = exec.split_whitespace().next()?;
        // Strip any absolute path: `/usr/bin/kitty -1` → `kitty`.
        let class = std::path::Path::new(class)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(class);
        self.rules.get(class)
    }
}

/// Apply a rule to a bbox. `None` rule leaves the bbox unchanged.
pub fn apply_rule(mut bbox: Bbox, rule: Option<&Rule>) -> Bbox {
    let Some(rule) = rule else { return bbox };
    bbox = bbox.enforce_min(rule.min_width.unwrap_or(0), rule.min_height.unwrap_or(0));
    if let Some([cw, ch]) = rule.cell_px {
        if cw > 0 {
            bbox.w = (bbox.w / cw) * cw;
        }
        if ch > 0 {
            bbox.h = (bbox.h / ch) * ch;
        }
    }
    bbox
}

/// Terminal command for `--term` mode: config.default_term > $TERMINAL > "kitty".
pub fn resolve_terminal(cfg: &Config) -> String {
    cfg.default_term
        .clone()
        .or_else(|| std::env::var("TERMINAL").ok())
        .unwrap_or_else(|| "kitty".to_string())
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("spawnhere").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_example_config() {
        let raw = r#"
            default_term = "foot"

            [rules.kitty]
            min_width = 800
            cell_px = [10, 22]

            [rules.firefox]
            min_width = 1200
            min_height = 800
        "#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.default_term.as_deref(), Some("foot"));
        assert_eq!(cfg.rule_for("kitty").and_then(|r| r.min_width), Some(800));
        assert_eq!(cfg.rule_for("kitty -1 fish").and_then(|r| r.cell_px), Some([10, 22]));
        assert_eq!(cfg.rule_for("/usr/bin/kitty").and_then(|r| r.min_width), Some(800));
        assert_eq!(cfg.rule_for("firefox").and_then(|r| r.min_height), Some(800));
        assert!(cfg.rule_for("unknown").is_none());
    }

    #[test]
    fn cell_snap_rounds_down() {
        let rule = Rule {
            min_width: None,
            min_height: None,
            cell_px: Some([10, 22]),
        };
        let out = apply_rule(Bbox { x: 0, y: 0, w: 437, h: 231 }, Some(&rule));
        assert_eq!(out.w, 430); // 437 / 10 = 43 rows
        assert_eq!(out.h, 220); // 231 / 22 = 10 rows
    }

    #[test]
    fn absent_rule_is_identity() {
        let bbox = Bbox { x: 1, y: 2, w: 300, h: 400 };
        let out = apply_rule(bbox, None);
        assert_eq!(out.x, 1);
        assert_eq!(out.w, 300);
    }

    #[test]
    fn gesture_defaults_match_photoshop_convention() {
        let g = GestureConfig::default();
        assert_eq!(g.default, GestureMode::Rectangle);
        assert_eq!(g.square_modifier, Some(Modifier::Shift));
        // Alt, not Ctrl: Ctrl is reserved for the "escape from --default
        // into the picker" release-time behaviour.
        assert_eq!(g.freehand_modifier, Some(Modifier::Alt));
        assert_eq!(g.drag_threshold_px, 5.0);
        assert!(g.show_dimensions);
        assert_eq!(g.min_width, 320);
        assert_eq!(g.min_height, 200);
        assert_eq!(g.click_spawn_width, Some(720));
        assert_eq!(g.click_spawn_height, Some(480));
    }

    #[test]
    fn gesture_resolution_priority() {
        let g = GestureConfig::default();
        // args are (shift, ctrl, alt)
        assert_eq!(g.resolve(false, false, false), EffectiveMode::Rectangle);
        assert_eq!(g.resolve(true, false, false), EffectiveMode::Square);
        assert_eq!(g.resolve(false, false, true), EffectiveMode::Freehand);
        // Ctrl alone does not change the stroke shape — it only escapes
        // --default at release time, which is orthogonal.
        assert_eq!(g.resolve(false, true, false), EffectiveMode::Rectangle);
        // Freehand beats square when both modifiers are held.
        assert_eq!(g.resolve(true, false, true), EffectiveMode::Freehand);
    }

    #[test]
    fn gesture_default_flipped_to_freehand() {
        let g = GestureConfig {
            default: GestureMode::Freehand,
            ..GestureConfig::default()
        };
        // No modifiers: freehand (the flipped default).
        assert_eq!(g.resolve(false, false, false), EffectiveMode::Freehand);
        // Shift with freehand-default degrades to plain rectangle (the
        // constraint can't attach to a freehand stroke cleanly).
        assert_eq!(g.resolve(true, false, false), EffectiveMode::Rectangle);
        // Alt still triggers freehand explicitly.
        assert_eq!(g.resolve(false, false, true), EffectiveMode::Freehand);
    }

    #[test]
    fn parse_gesture_section() {
        let raw = r#"
            [gesture]
            default = "freehand"
            square_modifier = "alt"
            freehand_modifier = "shift"
            drag_threshold_px = 8.0
            show_dimensions = false
            min_width = 400
            min_height = 300
            click_spawn_width = 800
            click_spawn_height = 600
        "#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.gesture.default, GestureMode::Freehand);
        assert_eq!(cfg.gesture.square_modifier, Some(Modifier::Alt));
        assert_eq!(cfg.gesture.freehand_modifier, Some(Modifier::Shift));
        assert_eq!(cfg.gesture.drag_threshold_px, 8.0);
        assert!(!cfg.gesture.show_dimensions);
        assert_eq!(cfg.gesture.min_width, 400);
        assert_eq!(cfg.gesture.click_spawn_width, Some(800));
        assert_eq!(cfg.gesture.click_spawn_height, Some(600));
    }

    #[test]
    fn parse_partial_gesture_keeps_other_defaults() {
        let raw = r#"
            [gesture]
            default = "freehand"
        "#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.gesture.default, GestureMode::Freehand);
        // Other keys should still hold their defaults.
        assert_eq!(cfg.gesture.square_modifier, Some(Modifier::Shift));
        assert_eq!(cfg.gesture.drag_threshold_px, 5.0);
        assert_eq!(cfg.gesture.min_width, 320);
        assert_eq!(cfg.gesture.click_spawn_width, Some(720));
    }
}
