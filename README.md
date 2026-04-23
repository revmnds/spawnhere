# spawnhere

Draw a gesture on your Hyprland desktop → spawn a floating window exactly where you drew.

> A launcher that skips the "where does the window go" problem: the rectangle you sketch *is* the window's final position and size.

## What it looks like

1. Press your bind (e.g. `Super+\``).
2. The screen dims; you draw a rough area with the mouse.
   - **Freehand**: just click and drag — the bounding box is the spawn area.
   - **Rectangle**: hold `Shift` while dragging for a clean rounded rect.
   - **Bare click**: no drag — the app opens at its own natural size at that point.
3. Release — a fuzzy-matching app picker appears inside your drawing.
4. Type / arrow-key / click to pick. The app spawns as a floating window at your drawn bbox.

Or skip the picker with `--term`, `--spawn <cmd>`, or `--default` (pinned app).

## Requirements

- **Compositor: Hyprland** (0.34+ recommended).
- **Session**: Wayland.
- **Runtime**: systemd-logind (for `$XDG_RUNTIME_DIR`).

spawnhere is intentionally Hyprland-only. It relies on `hyprctl dispatch exec` for the atomic "spawn floating at (x, y, w, h)" action that Sway / river / Wayfire don't expose in one shot. GNOME / KDE additionally don't advertise `zwlr_layer_shell_v1`, so the overlay can't even draw there.

## Install

### From source

```bash
git clone https://github.com/yourname/spawnhere
cd spawnhere
cargo build --release
install -Dm755 target/release/spawnhere ~/.local/bin/spawnhere
```

### AUR (Arch Linux)

```bash
yay -S spawnhere
# or, for the git tip
yay -S spawnhere-git
```

(PKGBUILD lives in `packaging/aur/`.)

## Hyprland binds

The recommended pair uses one bind for "launch my pinned default app" and a Shift variant for "open the full picker":

```conf
# Super + `        → spawn the pinned default app directly
# Super + Shift + `→ open the picker (also how you change the pin)
bind = SUPER, grave, exec, spawnhere --default
bind = SUPER SHIFT, grave, exec, spawnhere
```

Other patterns (see `examples/hyprland-binds.conf`):

```conf
# Gesture + always picker, no default concept
bind = SUPER, grave, exec, spawnhere

# Gesture + terminal, skip picker
bind = SUPER, t, exec, spawnhere --term

# Gesture + fixed app
bind = SUPER ALT, b, exec, spawnhere --spawn "firefox"
```

Then `hyprctl reload`.

## CLI

```
spawnhere [OPTIONS]

  -s, --spawn <CMD>        Spawn CMD directly at the drawn bbox (skips picker)
  -t, --term               Shortcut for `--spawn $TERMINAL`
  -d, --default            Spawn the pinned default app (Ctrl+P in the picker
                           pins / unpins). Falls back to the picker if nothing
                           is pinned. While the overlay is up, hold CTRL to
                           escape to the picker for this spawn.
      --padding <PX>       Extra pixels added to the bbox per side (default: 0)
  -V, --version
  -h, --help
```

## Picker keybindings

Inside the picker:

| Key | Action |
|---|---|
| `↵` Enter | Launch the selected app |
| `↑` / `↓` | Navigate |
| `PgUp` / `PgDn` | Page through the list |
| Letter keys | Fuzzy-filter by name (case-insensitive) |
| `Backspace` | Delete a character from the search |
| `Del` | Forget the selected recent app (drops it from history) |
| `Ctrl + P` | Pin (or unpin) the selected app as the `--default` target |
| `Esc` or right-click | Cancel |

Mouse: click a row to launch, click the `×` on a recent row to forget, click outside the card to cancel.

Pinned apps show a ★ next to their name; the `Ctrl+P` hint in the footer flips to **★ Unpin** (gold) when the current selection is the pinned one.

## Pin workflow

1. First time: `Super+\`` → overlay appears. Nothing is pinned, so it falls through to the picker.
2. Pick your favourite app, press `Ctrl + P` → a one-shot panel explains the binds and a ★ appears next to the name.
3. Press `Enter` (or `Esc`) to dismiss.
4. Next time: `Super+\`` → draw → the pinned app spawns directly, no picker.
5. To change: `Super+Shift+\`` → picker → navigate → `Ctrl + P` again on the new app.
6. To escape the pin for one spawn: with `Super+\``, hold `Ctrl` while drawing — the picker opens on release.

## Config

Optional `~/.config/spawnhere/config.toml`:

```toml
default_term = "kitty"         # fallback for --term if $TERMINAL unset

[rules.kitty]
min_width = 480
cell_px = [10, 22]             # snap to terminal cell grid

[rules.firefox]
min_width = 1200
min_height = 800
```

See `examples/config.toml` for the full, commented reference.

| Key | Type | Meaning |
|---|---|---|
| `default_term` | string | `--term` fallback when `$TERMINAL` is unset |
| `rules.<class>.min_width` | u32 | Floor for bbox width (applies only when the user drew a bbox — bare clicks keep the app's natural size) |
| `rules.<class>.min_height` | u32 | Floor for bbox height (same caveat) |
| `rules.<class>.cell_px` | `[u32, u32]` | Round bbox w/h down to nearest multiple of `[cell_w, cell_h]` — useful for terminals |

Rule matching uses the first path component of the chosen exec: `/usr/bin/kitty -1 fish` → `[rules.kitty]`.

## MRU

Every spawn appends the exec to `~/.local/share/spawnhere/history`. When the query is empty, the picker sorts recent picks to the top under a "Recent" section header, then everything else below under "Other apps". Delete the file to reset, or press `Del` on a row to forget it.

## Pinned default

The `--default` target is stored in `~/.local/share/spawnhere/default` (single line holding the exec string). Pin/unpin via `Ctrl + P` in the picker, or delete the file to reset.

## Multi-monitor

The overlay appears on whichever monitor Hyprland decides is "active" when the bind fires (typically the one with focus). Drawing happens on that monitor; the spawned window lands on the same one. To use spawnhere on another monitor, focus it first (hover or keyboard-switch) and then press the bind.

### Bar / panel safe-area

If you run a top bar (Quickshell, Waybar, eww, etc.), spawnhere queries `hyprctl layers -j` at spawn time to find any layer-shells anchored to an edge and clamps the final bbox to the usable area — so windows never land behind your bar, even when the bar uses `exclusiveZone = 0` (Ignore mode) and doesn't show up in the compositor's `reserved` array.

## HiDPI

The overlay honors the compositor's advertised scale factor via `scale_factor_changed` + `set_buffer_scale`. Vector shapes and glyph rasterization run at physical pixel density; icons are rasterized at `24 × scale` pixels.

## Typography

The picker ships with **Inter Variable** embedded in the binary (Open Font License 1.1, `assets/fonts/`). The UI looks identical on any machine, independent of the system's `sans-serif` resolution.

## License

MIT. See also `assets/fonts/OFL.txt` for Inter's license.
