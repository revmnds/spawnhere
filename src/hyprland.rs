//! Hyprland IPC adapter — spawns a floating window at a given bbox via
//! `hyprctl dispatch exec` with a dispatch prefix.

use crate::stroke::Bbox;
use anyhow::{bail, Context, Result};
use std::process::Command;

pub fn ensure_running() -> Result<()> {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        bail!(
            "Hyprland not detected.\n\
             \n\
             magicwand needs a running Hyprland session — HYPRLAND_INSTANCE_SIGNATURE is unset.\n\
             Check your session:  echo $XDG_CURRENT_DESKTOP   (should contain \"Hyprland\")\n\
             \n\
             GNOME, KDE, Sway, and other compositors are not supported."
        );
    }
    Ok(())
}

/// Spawn a floating window via `hyprctl dispatch exec` with inline window
/// rules. `bbox` is in monitor-local coords (matches Hyprland's `move X Y`
/// window rule semantics); Hyprland places the new window on the active
/// monitor's current workspace, which is the same monitor our overlay was on.
pub fn spawn_floating(cmd: &str, bbox: Bbox) -> Result<()> {
    let payload = format!(
        "[float;move {} {};size {} {}] {}",
        bbox.x.max(0),
        bbox.y.max(0),
        bbox.w,
        bbox.h,
        cmd
    );

    let status = Command::new("hyprctl")
        .args(["dispatch", "exec", &payload])
        .status()
        .context("executing hyprctl — is Hyprland running?")?;

    if !status.success() {
        bail!("hyprctl dispatch exec exited with {status}");
    }
    Ok(())
}
