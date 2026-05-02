# Snap-to-Windows for the Drawing Overlay

## Goal

Help the user spawn windows that align cleanly with existing windows on the focused monitor. The crosshair and the dragged rectangle corner should magnetically lock onto the corners and edges of nearby windows (and the safe-area edges), with Figma-style alignment guides confirming the lock visually.

This applies in two phases of the overlay:

1. **Idle (before drawing)** — the rendered crosshair and the would-be drag origin snap to candidate points so a click starts at a snapped position.
2. **Drawing (during drag)** — in `Rectangle` and `Square` modes the moving corner snaps to candidate points; `Freehand` is unaffected.

## Non-Goals

- No snap inside `Freehand` mode. Freehand exists to bypass alignment.
- No live-refresh of the candidate set while the overlay is open. The overlay is transient; one snapshot at startup is enough.
- No bypass modifier in v1. The three standard modifiers (Shift/Ctrl/Alt) are already used; `Super` is captured by Hyprland. Bypass is via config (`snap = false`).

## User-Facing Behavior

### Idle

When the cursor sits within the snap radius of a candidate point or edge, the rendered crosshair jumps to the snapped point. A subtle cyan ring (radius ~7px, 1.5px stroke) is drawn at the snapped point. If snap is active on both axes (corner) two dashed alignment guides extend across the screen — vertical magenta on X-axis lock, horizontal cyan on Y-axis lock. If only one axis is snapped (edge) only that one guide is drawn.

The actual hardware cursor stays free; only the visual crosshair and the seeded `drag_origin` move. This avoids fighting the compositor's pointer position and keeps motion buttery.

### Drawing (Rectangle / Square)

The cursor end of the rectangle snaps using the same algorithm. In `Square` mode the snap is applied to the cursor *first*, then the 1:1 diagonal is computed from `rect_start` to the snapped cursor — applying the constraint after snap would break either the lock or the 1:1 ratio. The `rect_start` corner is already snapped from the idle phase if applicable.

Guides render the same way (vertical magenta / horizontal cyan, dashed) but extend from the snapped corner across the screen so the user can sight-check long-distance alignment.

### Freehand

No snap, no guides, no ring. Drawn strokes pass through untouched.

### Config

```toml
[gesture]
snap           = true   # default true; false disables the whole feature
snap_radius_px = 12     # default 12 logical pixels
```

## Architecture

### New module: `src/snap.rs`

Contains:

- `WindowRect { x, y, w, h }` — a candidate rect in monitor-local logical coords.
- `SnapTargets` — derived from a `Vec<WindowRect>` plus the safe-area rect. Holds:
  - `points: Vec<(f32, f32)>` — corners and edge midpoints of every rect.
  - `x_lines: Vec<f32>` — every left/right edge X-coord (deduped, sorted).
  - `y_lines: Vec<f32>` — every top/bottom edge Y-coord (deduped, sorted).
- `SnapResult { point: (f32, f32), x_guide: Option<f32>, y_guide: Option<f32> }`.
- `Snapper { targets, radius_px }` with `snap(p: (f32, f32)) -> SnapResult`.

The `snap` algorithm:

1. Find the nearest target *point* within Euclidean radius. If found, lock both axes to it and emit `x_guide = Some(p.x)`, `y_guide = Some(p.y)`.
2. Otherwise find the nearest `x_line` within radius (1-D distance) and the nearest `y_line` within radius — independently. Each axis snaps if and only if its line is in range. The unaffected axis stays at the cursor value.
3. If neither matches, return `SnapResult { point: input, x_guide: None, y_guide: None }`.

Edge midpoints in the points list let users align centers easily, not just corners.

The safe-area rect contributes its own 4 corners and 4 edges to the targets — useful for slamming a new window flush against the screen edge or one of the bars.

### `hyprland::focused_monitor_clients()`

Calls `hyprctl clients -j`, filters to the focused monitor (compare `monitor` numeric index to the focused monitor's `id`), drops `mapped == false` and `hidden == true`, and returns each visible window's bbox in **monitor-local** coords (subtract the monitor's `(x, y)` origin from each `at`).

Workspace filter: keep only windows whose `workspace.id` matches the focused workspace of the monitor — otherwise we'd snap to windows on hidden workspaces.

Returns `Result<Vec<Bbox>>`. Failure path: log to stderr and return an empty vec; snap silently does nothing rather than crashing the overlay.

### `overlay.rs` integration

`RunConfig` gains `windows: Vec<Bbox>` and `snap: SnapConfig { enabled, radius_px }`. `AppState` gains:

- `snapper: Option<Snapper>` — `None` if disabled or no candidates.
- `last_snap: Option<SnapResult>` — set on every motion/press; consumed by `draw`.

In the pointer handlers:

- On `Enter` and `Motion` and `Press`: compute `SnapResult` from the raw `(x, y)`. Store `cursor` as the snapped point (so the crosshair tracks it) and use the snapped point everywhere we currently use the raw position — including `drag_origin` and `commit_or_extend_drag`. Store `last_snap` for the renderer.
- During modifier-driven re-resolve (`update_modifiers`), re-snap the cursor before reshaping the stroke — keeps lock through Shift/Ctrl toggles.

In `draw`:

- Add a render pass after the stroke and before the crosshair:
  - If `last_snap.x_guide` is some, draw a vertical dashed magenta line at that X across the full overlay height.
  - If `last_snap.y_guide` is some, draw a horizontal dashed cyan line at that Y across the full overlay width.
  - If `last_snap.point != cursor_raw` (i.e. snap was active), draw the cyan ring at `last_snap.point`.
- The crosshair already draws at `self.cursor`, which is now the snapped point. No change needed there.

### `main.rs` integration

After `focused_monitor_safe_area`, also call `focused_monitor_clients`. Pass both into `RunConfig` (the safe-area rect is already needed downstream, but a separate `safe_area` field on `RunConfig` lets `Snapper` consume it without re-querying).

If `cfg.gesture.snap == false` or the clients list is empty, skip building `Snapper` (state stays `None`, all snap calls are no-ops).

### `config.rs` changes

`GestureConfig` gains:

```rust
pub snap: bool,                  // default true
pub snap_radius_px: f32,         // default 12.0
```

Defaults documented in `examples/config.toml`.

## Visual Spec

| Element | Color (RGBA) | Stroke / Size |
|---|---|---|
| Snap ring | `(80, 230, 255, 220)` | radius 7px, stroke 1.5px |
| X-axis guide | `(255, 60, 200, 180)` | 1px stroke, dasharray `4 4` |
| Y-axis guide | `(80, 230, 255, 180)` | 1px stroke, dasharray `4 4` |

Guides span the full overlay width/height (don't bother trimming).

## Testing Strategy

### Unit tests (in `snap.rs`)

- `snap_point_locks_to_corner_within_radius`
- `snap_returns_input_when_no_targets_in_radius`
- `snap_returns_axis_only_when_only_edge_matches`
- `snap_picks_nearest_corner_when_multiple_in_radius`
- `snap_includes_edge_midpoints`
- `snap_includes_safe_area_edges`
- `disabled_snapper_returns_input_unchanged`

### Manual checks

1. Open spawnhere over 2 adjacent windows; cursor near their shared edge → both windows' edges should produce the same X line, snap should activate cleanly.
2. Drag a rectangle from a snapped origin in one window's corner to a snapped target on another → both ends locked, two pairs of guides.
3. Toggle `snap = false` in config → no rings, no guides, behavior identical to current main.
4. Spawn picker with no other windows on the workspace → only safe-area snap active.

## File-Level Changes

- **New** `src/snap.rs` (~180 lines).
- **Modify** `src/hyprland.rs` — add `focused_monitor_clients()` (~40 lines).
- **Modify** `src/overlay.rs` — `RunConfig` fields, `AppState` fields, snap calls in pointer handlers, render pass for ring + guides (~80 lines added).
- **Modify** `src/main.rs` — fetch clients, populate `RunConfig` (~10 lines).
- **Modify** `src/config.rs` — add `snap` and `snap_radius_px` to `GestureConfig` (~6 lines).
- **Modify** `src/main.rs` mod declaration — `mod snap;`.
- **Modify** `examples/config.toml` — document new keys (~6 lines).

## Open Questions

None. Ready for plan.
