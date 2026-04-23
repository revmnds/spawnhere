use anyhow::{Context, Result};
use clap::Parser;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;

mod apps;
mod config;
mod history;
mod hyprland;
mod overlay;
mod picker;
mod stroke;

use history::History;

#[derive(Parser)]
#[command(name = "spawnhere", version, about = "Draw a gesture to spawn a floating window")]
struct Cli {
    /// Command to spawn directly at the drawn bbox (skips the picker).
    /// If omitted, an app picker appears after the gesture.
    #[arg(long, short = 's', conflicts_with = "term")]
    spawn: Option<String>,

    /// Shortcut for `--spawn $TERMINAL` (falls back to `config.default_term`,
    /// then `kitty`). Draws the gesture, skips the picker, spawns a terminal.
    #[arg(long, short = 't')]
    term: bool,

    /// Minimum width in pixels if stroke bbox is smaller.
    #[arg(long, default_value_t = 400)]
    min_width: u32,

    /// Minimum height in pixels if stroke bbox is smaller.
    #[arg(long, default_value_t = 300)]
    min_height: u32,

    /// Extra pixels added to the bbox on each side (0 = exact stroke fidelity).
    #[arg(long, default_value_t = 0)]
    padding: u32,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    hyprland::ensure_running().context("Hyprland not detected")?;

    // Single-instance: another spawnhere already running? Exit quietly so the
    // same keybind can be spammed without stacking overlays.
    let _lock = match acquire_single_instance_lock()? {
        Some(f) => f,
        None => return Ok(()),
    };

    let cfg = config::Config::load();

    let preset_exec = if cli.term {
        Some(config::resolve_terminal(&cfg))
    } else {
        cli.spawn.clone()
    };

    let outcome = overlay::run(overlay::RunConfig {
        preset_exec,
        padding: cli.padding,
        min_width: cli.min_width,
        min_height: cli.min_height,
        history: History::load(),
    })
    .context("overlay failed")?;

    let (bbox, exec, screen) = match outcome {
        overlay::Outcome::Spawn { bbox, exec, screen } => (bbox, exec, screen),
        overlay::Outcome::Cancelled => return Ok(()),
    };

    // Re-clamp after per-app rule expansions: `rules.X.min_width` can grow
    // the box back over an edge. `screen` is the overlay's rect.
    let bbox = config::apply_rule(bbox, cfg.rule_for(&exec)).clamp_to_rect(screen);
    hyprland::spawn_floating(&exec, bbox)?;
    History::record(&exec);
    Ok(())
}

fn lock_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .expect("XDG_RUNTIME_DIR is set under systemd-logind (required by Hyprland)");
    dir.join("spawnhere.lock")
}

/// Returns `Some(File)` if we acquired the single-instance lock (caller must
/// hold the file alive for the lock to persist), `None` if another instance
/// already has it.
fn acquire_single_instance_lock() -> Result<Option<File>> {
    let path = lock_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening lock file {}", path.display()))?;

    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e).context("acquiring lock"),
    }
}
