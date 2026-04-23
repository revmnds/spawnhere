//! Optional `~/.config/magicwand/config.toml`. All fields are optional —
//! magicwand runs fine with no file. The config drives:
//!
//! * `default_term` — which exec to spawn in `--term` mode if `$TERMINAL` is
//!   unset. Falls back to `"kitty"` if neither is set.
//! * `[rules.<class>]` — per-app bbox post-processing (min size, cell snap).
//!   `<class>` is matched against the first whitespace-separated token of the
//!   chosen exec. So `kitty -1` and `kitty --class foo` both match `[rules.kitty]`.

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

impl Config {
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                eprintln!("magicwand: {} — ignoring: {e}", path.display());
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
    Some(base.join("magicwand").join("config.toml"))
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
}
