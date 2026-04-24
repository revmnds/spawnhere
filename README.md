<div align="center">

# spawnhere

**Draw where you want a window. Get a window.**

A launcher for Hyprland: sketch a rectangle on screen, pick an app, and it spawns at that size and position.

[![AUR](https://img.shields.io/aur/version/spawnhere?color=1793d1&label=AUR&logo=arch-linux&logoColor=white)](https://aur.archlinux.org/packages/spawnhere)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

<video src="https://github.com/user-attachments/assets/c5826039-14b3-48ac-bdbe-5c7b68ab2be2" autoplay loop muted playsinline width="720"></video>

</div>

## Install

```bash
yay -S spawnhere
```

Or from source:

```bash
cargo install --git https://github.com/revmnds/spawnhere
```

## Usage

Add to `~/.config/hypr/hyprland.conf`:

```conf
bind = SUPER,       grave, exec, spawnhere --default   # pinned app
bind = SUPER SHIFT, grave, exec, spawnhere             # picker
```

Reload with `hyprctl reload`, press the bind, drag a rectangle, pick an app. Hold `Shift` for a 1:1 square, `Alt` for freehand, or click without dragging to spawn at default size.

See `spawnhere --help` for all flags.

## Config

Optional: `~/.config/spawnhere/config.toml`

```toml
default_term = "kitty"

[rules.firefox]
min_width  = 1200
min_height = 800

[gesture]
default            = "rectangle"   # or "freehand"
click_spawn_width  = 720
click_spawn_height = 480
```

Full reference: [`examples/config.toml`](examples/config.toml).

## Requirements

Hyprland 0.34+ · Wayland

## License

[MIT](LICENSE)
