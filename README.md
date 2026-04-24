<div align="center">

# spawnhere

**Draw where you want a window. Get a window.**

A launcher for Hyprland that skips the *"where does this go?"* step — the rectangle you sketch on screen **is** the window's final size and position.

[![AUR version](https://img.shields.io/aur/version/spawnhere?color=1793d1&label=AUR&logo=arch-linux&logoColor=white)](https://aur.archlinux.org/packages/spawnhere)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Compositor: Hyprland](https://img.shields.io/badge/compositor-Hyprland-0c96c5)](https://hyprland.org)
[![Vibecoded](https://img.shields.io/badge/100%25-vibecoded-ff6ec7?labelColor=1a1a2e)](#vibecoded)

<br>

<video src="https://github.com/user-attachments/assets/c5826039-14b3-48ac-bdbe-5c7b68ab2be2" autoplay loop muted playsinline width="720"></video>

</div>

---

## Why

spawnhere flips the usual launcher flow. Instead of picking an app and then arranging its window, you describe the space first — sketch a rectangle anywhere on screen — and choose what fills it. The gesture doubles as the geometry: one motion decides both the target window and where it will live.

## Quickstart

Install from the AUR:

```bash
yay -S spawnhere        # or paru, trizen, etc.
```

Add to `~/.config/hypr/hyprland.conf`:

```conf
bind = SUPER,       grave, exec, spawnhere --default   # quick-spawn pinned app
bind = SUPER SHIFT, grave, exec, spawnhere             # open the picker
```

Reload (`hyprctl reload`). Press `Super+` `` ` `` → draw → release → pick an app.

## How it works

Four gestures, one bind:

| Gesture | Result |
|---|---|
| **Drag** | Clean rounded rectangle. Dimensions float next to the cursor as you size it. Default mode — matches the Photoshop/Figma muscle memory. |
| **Shift + drag** | Same rectangle, constrained to a **1:1 square**. |
| **Alt + drag** | *Freehand* — draw whatever you like (a circle, a squiggle, the letter R) and the window spawns at the bounding box of your stroke. |
| **Click without dragging** | Opens a window centered on the click at the configured `click_spawn_*` size (default 720×480). A micro-jitter under 5 px still counts as a click. |

Release → a fuzzy picker appears inside your drawing. Type, `↵`, done.

> **Coming from an older spawnhere?** The default used to be freehand. If you prefer it that way, flip it in `config.toml` under `[gesture]` — see the [Config](#config) section.

<details>
<summary><b>Picker shortcuts</b></summary>

| Key | Action |
|---|---|
| `↵` Enter | Launch the selected app |
| `↑` / `↓` | Navigate |
| `PgUp` / `PgDn` | Page through the list |
| Letter keys | Fuzzy-filter by name |
| `Backspace` | Delete a search character |
| `Del` | Forget the selected recent app |
| `Ctrl + P` | Pin (or unpin) as the `--default` target — a ★ marks the pinned one |
| `Esc` / right-click | Cancel |

Mouse works too: click a row to launch, click the `×` on a recent row to forget, click outside the card to cancel.

</details>

## CLI

```
spawnhere [OPTIONS]

  -s, --spawn <CMD>     Spawn CMD directly at the drawn bbox (skip picker)
  -t, --term            Shortcut for --spawn $TERMINAL
  -d, --default         Spawn the pinned default app (Ctrl+P in picker to pin)
      --padding <PX>    Extra pixels added around the bbox (default: 0)
  -h, --help
  -V, --version
```

Hold `Ctrl` while drawing with `--default` to escape the pin and open the picker once.

More bind patterns in [`examples/hyprland-binds.conf`](examples/hyprland-binds.conf).

## Config

Optional: `~/.config/spawnhere/config.toml`

```toml
default_term = "kitty"         # --term fallback when $TERMINAL is unset

[rules.kitty]
min_width = 480
cell_px = [10, 22]             # snap terminal bboxes to the cell grid

[rules.firefox]
min_width = 1200
min_height = 800

[gesture]
default             = "rectangle"  # or "freehand" to restore the old default
square_modifier     = "shift"      # held modifier that forces 1:1
freehand_modifier   = "alt"        # held modifier that switches to freehand
drag_threshold_px   = 5.0          # below this, a drag is treated as a click
show_dimensions     = true         # W×H readout near the cursor while dragging
min_width           = 320          # global floor for any drawn bbox
min_height          = 200
click_spawn_width   = 720          # size used for click-to-spawn (no drag)
click_spawn_height  = 480          # set either to 0/omit for natural size
```

Rules match on the first path component of the chosen exec
(`/usr/bin/kitty -1 fish` → `[rules.kitty]`). The `min_*` floors apply **only when you drew a bbox** — bare clicks preserve the app's natural size.

Full commented reference: [`examples/config.toml`](examples/config.toml).

## Requirements

- **Hyprland** 0.34+
- A **Wayland** session
- `systemd-logind` (for `$XDG_RUNTIME_DIR`)

<details>
<summary><b>Why Hyprland-only?</b></summary>

spawnhere leans on `hyprctl dispatch exec` to perform the atomic
*"spawn floating at (x, y, w, h)"* action in a single shot. Sway, river and Wayfire don't expose that; GNOME and KDE don't advertise `zwlr_layer_shell_v1`, so the overlay cannot even draw. A per-compositor backend is possible but not on the roadmap.

</details>

## Advanced

<details>
<summary><b>Multi-monitor</b></summary>

The overlay appears on whichever monitor Hyprland considers "active" when the bind fires — usually the one with focus. The spawned window lands on the same monitor. To use spawnhere on another display, focus it first (hover or keyboard-switch) and then press the bind.

</details>

<details>
<summary><b>Bar / panel safe-area</b></summary>

If you run a top bar (Quickshell, Waybar, eww…), spawnhere queries `hyprctl layers -j` at spawn time, finds every edge-anchored layer-shell, and clamps the final bbox to the usable area. Windows never land behind your bar — even when it uses `exclusiveZone = 0` (Ignore mode) and doesn't show up in the compositor's `reserved` array.

</details>

<details>
<summary><b>HiDPI</b></summary>

Honors the compositor's advertised scale factor via `scale_factor_changed` + `set_buffer_scale`. Vector shapes and glyph rasterization run at physical pixel density; icons are rasterized at `24 × scale` pixels.

</details>

<details>
<summary><b>Recent apps & pinning</b></summary>

Every spawn appends the exec to `~/.local/share/spawnhere/history`. With an empty query the picker sorts recents to the top under *"Recent"*, then everything else under *"Other apps"*. Delete the file to reset, or press `Del` on a row to forget it.

The pinned `--default` target is stored in `~/.local/share/spawnhere/default` (single line, the exec string). Pin/unpin via `Ctrl + P` in the picker, or delete the file to reset.

</details>

<details>
<summary><b>Typography</b></summary>

Ships with [Inter Variable](https://rsms.me/inter/) embedded in the binary (Open Font License 1.1, `assets/fonts/`). The UI looks identical on any machine, independent of the system's `sans-serif` resolution.

</details>

## Build from source

```bash
git clone https://github.com/revmnds/spawnhere
cd spawnhere
cargo build --release
install -Dm755 target/release/spawnhere ~/.local/bin/spawnhere
```

Run the test suite:

```bash
cargo test
```

## Contributing

Bug reports and ideas welcome — [open an issue](https://github.com/revmnds/spawnhere/issues). For patches: small, focused PRs land fastest. The codebase is a single binary (~1.5k LOC) with unit tests for the geometry and picker logic.

## <a id="vibecoded"></a>Vibecoded

This project is **100% vibecoded** — every line of Rust, every pixel of the overlay, every test was written by [Claude](https://claude.ai) under my direction. I described what I wanted ("draw a gesture, spawn a window there, Hyprland only"), Claude translated intent into code, I steered the details and made final calls.

If you're new to this style of building: it's not *"AI generated the code so it's probably slop."* It's pair programming where you keep the wheel but your partner types at 10,000 WPM. The final output still has to work, the tests still have to pass, the UX still has to feel right — those calls are mine.

Read the code. It's small. It's commented. It's honest about what it is.

## License

[MIT](LICENSE) · Inter font under [OFL](assets/fonts/OFL.txt).
