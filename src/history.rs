//! MRU history: each spawn appends the chosen exec to
//! `~/.local/share/spawnhere/history`. On load, counts and recency boost
//! nucleo-matcher scores in the picker so yesterday's kitty sits on top.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

/// One boost per prior pick of this exec, plus a larger bump for the most
/// recent pick. Chosen so a single fresh pick outranks a stale hot app.
const RECENCY_BONUS: u32 = 64;
const COUNT_BONUS: u32 = 4;

#[derive(Debug, Default)]
pub struct History {
    counts: HashMap<String, u32>,
    last: Option<String>,
}

impl History {
    pub fn load() -> Self {
        let Some(path) = history_path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        let mut counts: HashMap<String, u32> = HashMap::new();
        let mut last = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            *counts.entry(line.to_string()).or_default() += 1;
            last = Some(line.to_string());
        }
        Self { counts, last }
    }

    pub fn score_bonus(&self, exec: &str) -> u32 {
        let count = self.counts.get(exec).copied().unwrap_or(0);
        let mut bonus = count.saturating_mul(COUNT_BONUS);
        if self.last.as_deref() == Some(exec) {
            bonus = bonus.saturating_add(RECENCY_BONUS);
        }
        bonus
    }

    /// Has this exec been picked at least once before?
    pub fn has_picked(&self, exec: &str) -> bool {
        self.counts.contains_key(exec)
    }

    /// Append one pick to the history file. Silent on failure — history is a
    /// quality-of-life feature, not something we should crash the launcher over.
    pub fn record(exec: &str) {
        let Some(path) = history_path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        else {
            return;
        };
        let _ = writeln!(f, "{exec}");
    }

    /// Forget an exec: drop in-memory counts + last, and rewrite the history
    /// file without any occurrence of that exec. Silent on I/O failure.
    pub fn forget(&mut self, exec: &str) {
        self.counts.remove(exec);
        if self.last.as_deref() == Some(exec) {
            self.last = None;
        }
        let Some(path) = history_path() else { return };
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        let kept: Vec<&str> = text.lines().filter(|l| l.trim() != exec).collect();
        let _ = std::fs::write(&path, kept.join("\n") + if kept.is_empty() { "" } else { "\n" });
    }
}

fn history_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("spawnhere").join("history"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_history_gives_zero_bonus() {
        let h = History::default();
        assert_eq!(h.score_bonus("kitty"), 0);
    }

    #[test]
    fn last_pick_gets_recency_bump() {
        let mut h = History::default();
        h.counts.insert("kitty".to_string(), 3);
        h.last = Some("kitty".to_string());
        assert_eq!(h.score_bonus("kitty"), 3 * COUNT_BONUS + RECENCY_BONUS);
    }

    #[test]
    fn non_last_pick_gets_count_only() {
        let mut h = History::default();
        h.counts.insert("kitty".to_string(), 5);
        h.counts.insert("firefox".to_string(), 1);
        h.last = Some("firefox".to_string());
        assert_eq!(h.score_bonus("kitty"), 5 * COUNT_BONUS);
    }

    #[test]
    fn fresh_pick_outranks_stale_hot_app() {
        // Firefox picked many times long ago; kitty picked once just now.
        let mut h = History::default();
        h.counts.insert("firefox".to_string(), 10);
        h.counts.insert("kitty".to_string(), 1);
        h.last = Some("kitty".to_string());
        assert!(h.score_bonus("kitty") > h.score_bonus("firefox"));
    }

    #[test]
    fn forget_clears_memory_state() {
        let mut h = History::default();
        h.counts.insert("kitty".to_string(), 3);
        h.counts.insert("firefox".to_string(), 1);
        h.last = Some("kitty".to_string());
        h.forget("kitty");
        assert!(!h.has_picked("kitty"));
        assert!(h.has_picked("firefox"));
        assert_eq!(h.last, None);
        assert_eq!(h.score_bonus("kitty"), 0);
    }
}
