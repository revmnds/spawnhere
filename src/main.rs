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
use stroke::Bbox;

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
        gesture: cfg.gesture.clone(),
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
    // Size resolution order:
    //   1. Drag → enforce `gesture.min_width/min_height` as a global floor
    //      (so a tiny drag can't produce a sliver window), then apply any
    //      matching `[rules.<class>]` on top (per-app floors / cell snap).
    //   2. Click (bbox.w == 0) with `gesture.click_spawn_*` set → centre a
    //      window of that size on the click point. Feels right for quick
    //      "drop a floating window here" usage without forcing the user to
    //      physically draw every time.
    //   3. Click with no click-spawn size configured → legacy behaviour:
    //      let the app open at its natural size at the click point.
    let safe = hyprland::focused_monitor_safe_area().unwrap_or(screen);
    let bbox = if bbox.w > 0 && bbox.h > 0 {
        bbox.enforce_min(cfg.gesture.min_width, cfg.gesture.min_height)
    } else if let (Some(cw), Some(ch)) =
        (cfg.gesture.click_spawn_width, cfg.gesture.click_spawn_height)
    {
        // bbox.x/y came from the single-point click; centre a cw×ch rect on it.
        Bbox {
            x: bbox.x - (cw as i32) / 2,
            y: bbox.y - (ch as i32) / 2,
            w: cw,
            h: ch,
        }
    } else {
        bbox
    };
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
