# magicwand

Draw a gesture on your Hyprland desktop → spawn a floating window exactly where you drew.

> A launcher that skips the "where does the window go" problem: the rectangle you sketch *is* the window's final position and size.

## What it looks like

1. Press your bind (e.g. `Super+\``).
2. The screen dims; you draw a rough rectangle with the mouse.
3. Release — a fuzzy-matching app picker appears inside the stroke.
4. Type / arrow-key / click to pick. The app spawns as a floating window at your drawn bbox.

Or skip step 3 with `--term`: gesture → terminal at that bbox, no picker.

## Requirements

- **Compositor: Hyprland** (0.34+ recommended).
- **Session**: Wayland.
- **Runtime**: systemd-logind (for `$XDG_RUNTIME_DIR`).

magicwand is intentionally Hyprland-only. It relies on `hyprctl dispatch exec` for the atomic "spawn floating at (x, y, w, h)" action that Sway / river / Wayfire don't expose in one shot. GNOME / KDE additionally don't advertise `zwlr_layer_shell_v1`, so the overlay can't even draw there.

## Install

### From source

```bash
git clone https://github.com/yourname/magicwand
cd magicwand
cargo build --release
install -Dm755 target/release/magicwand ~/.local/bin/magicwand
```

### AUR (Arch Linux)

```bash
yay -S magicwand
# or, for the git tip
yay -S magicwand-git
```

(PKGBUILD lives in `packaging/aur/`.)

## Hyprland binds

Add to `~/.config/hypr/hyprland.conf` (or `~/.config/hypr/custom/keybinds.conf`):

```conf
# Draw → pick app → spawn floating
bind = SUPER, grave, exec, magicwand

# Draw → spawn terminal (skips the picker)
bind = SUPER SHIFT, grave, exec, magicwand --term

# Draw → spawn a specific command
bind = SUPER ALT, grave, exec, magicwand --spawn "firefox"
```

Then `hyprctl reload`.

## CLI

```
magicwand [OPTIONS]

  -s, --spawn <CMD>        Command to spawn directly at the drawn bbox (skips picker)
  -t, --term               Shortcut for `--spawn $TERMINAL`
      --min-width <PX>     Minimum bbox width (default: 400)
      --min-height <PX>    Minimum bbox height (default: 300)
      --padding <PX>       Extra pixels added to the bbox per side (default: 0)
  -V, --version
  -h, --help
```

## Config

Optional `~/.config/magicwand/config.toml`:

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
| `rules.<class>.min_width` | u32 | Floor for bbox width (overrides CLI `--min-width` if larger) |
| `rules.<class>.min_height` | u32 | Floor for bbox height |
| `rules.<class>.cell_px` | `[u32, u32]` | Round bbox w/h down to nearest multiple of `[cell_w, cell_h]` — useful for terminals |

Rule matching uses the first path component of the chosen exec: `/usr/bin/kitty -1 fish` → `[rules.kitty]`.

## MRU

Every spawn appends the exec to `~/.local/share/magicwand/history`. When the query is empty, the picker sorts recent picks to the top under a "Recent" section header, then everything else below under "Other apps". Delete the file to reset.

## Multi-monitor

The overlay appears on whichever monitor Hyprland decides is "active" when the bind fires (typically the one with focus). Drawing happens on that monitor; the spawned window lands on the same one. To use magicwand on another monitor, focus it first (hover or keyboard-switch) and then press the bind.

## HiDPI

The overlay honors the compositor's advertised scale factor via `scale_factor_changed` + `set_buffer_scale`. Vector shapes and glyph rasterization run at physical pixel density; icons are rasterized at `24 × scale` pixels.

## License

MIT.
