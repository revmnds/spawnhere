//! User's pinned default app. Single-line file at
//! `$XDG_DATA_HOME/spawnhere/default` holding the exec string to launch when
//! invoked with `--default` (paired with a dedicated Hyprland bind). Set and
//! cleared from the picker with `P`.

use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Default, Clone)]
pub struct Pinned {
    exec: Option<String>,
}

impl Pinned {
    pub fn load() -> Self {
        let Some(path) = pinned_path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            Self::default()
        } else {
            Self { exec: Some(trimmed.to_string()) }
        }
    }

    pub fn exec(&self) -> Option<&str> {
        self.exec.as_deref()
    }

    pub fn is(&self, exec: &str) -> bool {
        self.exec.as_deref() == Some(exec)
    }

    /// Persist `exec` as the default. Silent on failure — pinning is a
    /// nice-to-have, not worth crashing the launcher over.
    pub fn set(&mut self, exec: &str) {
        self.exec = Some(exec.to_string());
        let Some(path) = pinned_path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::File::create(&path) {
            let _ = writeln!(f, "{exec}");
        }
    }

    pub fn clear(&mut self) {
        self.exec = None;
        let Some(path) = pinned_path() else { return };
        let _ = std::fs::remove_file(path);
    }
}

fn pinned_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("spawnhere").join("default"))
}
