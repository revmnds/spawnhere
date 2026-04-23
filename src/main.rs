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
mod pinned;
mod stroke;

use history::History;
use pinned::Pinned;

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

    /// Spawn the pinned "default" app directly (skips the picker). Pin/unpin
    /// from the picker with P. Falls back to the picker if no app is pinned.
    #[arg(long, short = 'd', conflicts_with_all = ["spawn", "term"])]
    default: bool,

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
    } else if cli.default {
        // If nothing is pinned, fall through to the picker so the user can
        // choose and then press P to pin — that's the natural first-run flow.
        Pinned::load().exec().map(String::from)
    } else {
        cli.spawn.clone()
    };

    let outcome = overlay::run(overlay::RunConfig {
        preset_exec,
        padding: cli.padding,
        history: History::load(),
    })
    .context("overlay failed")?;

    let (bbox, exec, screen) = match outcome {
        overlay::Outcome::Spawn { bbox, exec, screen } => (bbox, exec, screen),
        overlay::Outcome::Cancelled => return Ok(()),
    };

    // Clamp to the safe area (monitor minus any layer-shells anchored to its
    // edges — quickshell/waybar/eww panels). Without this, expansions could
    // push the bbox into the bar's visual zone, since Hyprland's plain
    // `move X Y` is reserved-area-unaware. Falls back to the overlay's full
    // rect if hyprctl is unreachable.
    //
    // Per-app TOML rules (min_width / cell_px) only apply when the user
    // actually drew a rectangle. A bare click means "spawn here at the app's
    // natural size" — we don't override that with a rule, since that would
    // make every click expand into the rule's footprint.
    let safe = hyprland::focused_monitor_safe_area().unwrap_or(screen);
    let bbox = if bbox.w > 0 && bbox.h > 0 {
        config::apply_rule(bbox, cfg.rule_for(&exec)).clamp_to_rect(safe)
    } else {
        bbox.clamp_to_rect(safe)
    };
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
