//! Hyprland IPC adapter — spawns a floating window at a given bbox via
//! `hyprctl dispatch exec` with a dispatch prefix.

use crate::stroke::Bbox;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::process::Command;

pub fn ensure_running() -> Result<()> {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        bail!(
            "Hyprland not detected.\n\
             \n\
             spawnhere needs a running Hyprland session — HYPRLAND_INSTANCE_SIGNATURE is unset.\n\
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
    // Only force a size when the user actually drew a rectangle. A bare click
    // produces a 0×0 bbox, and we let the app open at its own natural size at
    // the click point — no arbitrary floor imposed by spawnhere.
    let payload = if bbox.w > 0 && bbox.h > 0 {
        format!(
            "[float;move {} {};size {} {}] {}",
            bbox.x.max(0),
            bbox.y.max(0),
            bbox.w,
            bbox.h,
            cmd
        )
    } else {
        format!(
            "[float;move {} {}] {}",
            bbox.x.max(0),
            bbox.y.max(0),
            cmd
        )
    };

    let status = Command::new("hyprctl")
        .args(["dispatch", "exec", &payload])
        .status()
        .context("executing hyprctl — is Hyprland running?")?;

    if !status.success() {
        bail!("hyprctl dispatch exec exited with {status}");
    }
    Ok(())
}

fn hyprctl_json(args: &[&str]) -> Result<Value> {
    let out = Command::new("hyprctl")
        .args(args)
        .output()
        .context("executing hyprctl")?;
    if !out.status.success() {
        bail!("hyprctl {args:?} exited with {}", out.status);
    }
    serde_json::from_slice(&out.stdout).context("parsing hyprctl JSON output")
}

/// Returns the safe-area bbox (in monitor-local coords) of the focused
/// monitor — i.e. the monitor rect minus any layer-shells that occupy a
/// full edge (top/bottom bars, left/right docks). This works whether those
/// layers reserve space (`exclusiveZone > 0`) or not (`ExclusionMode.Ignore`,
/// common in Quickshell-based bars), because we read `hyprctl layers -j`
/// directly instead of relying on the compositor's `reserved` field.
///
/// Falls back to the full monitor rect if hyprctl can't be queried; that
/// way the caller never crashes on a misconfigured Hyprland session.
pub fn focused_monitor_safe_area() -> Result<Bbox> {
    let monitors = hyprctl_json(&["monitors", "-j"])?;
    let mon = monitors
        .as_array()
        .and_then(|a| a.iter().find(|m| m["focused"].as_bool().unwrap_or(false)))
        .context("no focused monitor reported by hyprctl")?;
    let mname = mon["name"].as_str().context("monitor name missing")?;
    let mw = mon["width"].as_u64().context("monitor width missing")? as u32;
    let mh = mon["height"].as_u64().context("monitor height missing")? as u32;
    let mx = mon["x"].as_i64().context("monitor x missing")? as i32;
    let my = mon["y"].as_i64().context("monitor y missing")? as i32;

    // The `reserved` array exposes each bar's declared exclusiveZone — which
    // can differ from the layer's rendered size (e.g. Quickshell bars add a
    // decorative `screenRounding` below the opaque bar area, so the layer is
    // taller than what actually shouldn't be spawned over). For each detected
    // edge layer, we'll prefer the largest reserved value that still fits
    // inside the layer; if none fits, we fall back to the layer's own size
    // (handles layers that declare exclusiveZone=0 / Ignore mode with no
    // exclusive zone). Reading `reserved` this way sidesteps the array's
    // ambiguous [top, right, bottom, left] vs [left, top, right, bottom] ordering.
    let reserved: Vec<u32> = mon["reserved"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_u64().map(|n| n as u32)).collect())
        .unwrap_or_default();

    let layers_root = hyprctl_json(&["layers", "-j"])?;
    let levels = match layers_root.get(mname).and_then(|m| m.get("levels")) {
        Some(l) => l,
        None => return Ok(Bbox { x: 0, y: 0, w: mw, h: mh }),
    };

    // Heuristic: an "edge layer" spans ~the entire perpendicular axis (≥90%)
    // and is anchored to one edge. Full-monitor layers (backgrounds) are
    // skipped via the area-ratio cutoff so wallpaper layers don't eat the
    // whole usable area.
    let (mut top, mut bottom, mut left, mut right) = (0u32, 0u32, 0u32, 0u32);
    if let Some(obj) = levels.as_object() {
        for layers in obj.values() {
            let Some(arr) = layers.as_array() else { continue };
            for l in arr {
                let lx = l["x"].as_i64().unwrap_or(0) as i32;
                let ly = l["y"].as_i64().unwrap_or(0) as i32;
                let lw = l["w"].as_u64().unwrap_or(0) as u32;
                let lh = l["h"].as_u64().unwrap_or(0) as u32;
                if lw == 0 || lh == 0 {
                    continue;
                }
                let area = lw as u64 * lh as u64;
                let mon_area = mw as u64 * mh as u64;
                if area * 5 >= mon_area * 4 {
                    // ≥80% of the monitor — almost certainly a background/wallpaper.
                    continue;
                }
                let spans_w = lw as u64 * 10 >= mw as u64 * 9;
                let spans_h = lh as u64 * 10 >= mh as u64 * 9;
                if spans_w {
                    let eff = effective_edge_size(&reserved, lh);
                    if ly == my {
                        top = top.max(eff);
                    } else if (ly + lh as i32) == (my + mh as i32) {
                        bottom = bottom.max(eff);
                    }
                } else if spans_h {
                    let eff = effective_edge_size(&reserved, lw);
                    if lx == mx {
                        left = left.max(eff);
                    } else if (lx + lw as i32) == (mx + mw as i32) {
                        right = right.max(eff);
                    }
                }
            }
        }
    }

    Ok(Bbox {
        x: left as i32,
        y: top as i32,
        w: mw.saturating_sub(left + right),
        h: mh.saturating_sub(top + bottom),
    })
}

/// Given a layer's size on its major axis and the monitor's `reserved` array,
/// pick the exclusive-zone value that best describes the bar's *reserved*
/// thickness (ignoring any decorative overflow like rounded corners).
///
/// The largest reserved entry that still fits inside the layer is chosen;
/// with one bar on an edge this is always the right one, regardless of the
/// array's edge order. Falls back to the full layer size when no reserved
/// entry fits — that covers bars with `exclusiveZone = 0` / Ignore mode.
fn effective_edge_size(reserved: &[u32], layer_size: u32) -> u32 {
    reserved
        .iter()
        .copied()
        .filter(|&r| r > 0 && r <= layer_size)
        .max()
        .unwrap_or(layer_size)
}
