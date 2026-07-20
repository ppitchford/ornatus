# ornatus

Wayland-native solar-gradient wallpaper and theme daemon for wlroots compositors.

`ornatus` computes the sun's position for your location and blends between 16
gradient wallpaper frames as the day progresses, drawing directly to a
`wlr-layer-shell` background surface. At sunrise and sunset it also flips a
light/dark theme marker and re-points per-application config symlinks, so your
terminal, launcher and notification daemon follow the sky.

## Requirements

- A wlroots-based Wayland compositor (`wlr-layer-shell-unstable-v1`)
- Rust 2024 edition toolchain
- `XDG_RUNTIME_DIR` set (used for the PID file)

## Build

```sh
cargo build --release
install -Dm755 target/release/ornatus ~/.local/bin/ornatus
```

## Usage

```sh
ornatus              # run the daemon
ornatus --refresh    # force a running instance to refresh (sends SIGUSR1), then exit
ornatus --help
```

Logging uses `tracing` and defaults to `ornatus=info`. Override with
`RUST_LOG`, e.g. `RUST_LOG=ornatus=debug ornatus`.

Signals: `SIGUSR1` refreshes, `SIGTERM`/`SIGINT` shut down cleanly.

## Configuration

On first run a default config is written to
`$XDG_CONFIG_HOME/ornatus/config.toml` (falling back to `~/.config`):

```toml
wallpaper_dir         = "~/Pictures/wallpapers/solar-gradients"
theme_dir             = "~/.config/theme"
refresh_interval_secs = 60

[location]
source = "auto"
```

| Key | Meaning |
| --- | --- |
| `wallpaper_dir` | Directory holding the 16 gradient frames, named `00.jpg` … `15.jpg` |
| `theme_dir` | Holds the `light/` and `dark/` theme bundles plus the `current` marker file |
| `refresh_interval_secs` | How often the blend is recomputed and redrawn |
| `location.source` | `auto` for IP geolocation, or `fixed` with explicit coordinates |

For a fixed location (useful behind a VPN):

```toml
[location]
source = "fixed"
lat    = 47.3769
lon    = 8.5417
```

## Theming

Applying a theme writes the variant name to `<theme_dir>/current` — other tools
can watch that file — and symlinks each file from the active bundle into place:

| Bundle file | Symlinked to |
| --- | --- |
| `kitty.conf` | `~/.config/kitty/current-theme.conf` |
| `fuzzel.ini` | `~/.config/fuzzel/fuzzel.ini` |
| `mako.conf` | `~/.config/mako/config` |
| `mango.conf` | `~/.config/mango/theme.conf` |

Missing bundle files are skipped with a warning. After any change, reloads are
signalled best-effort via `pkill -SIGUSR1 kitty`, `makoctl reload` and
`mmsg -d reload_config`.

## Network use

With `location.source = "auto"`, ornatus makes a single HTTPS request to
`https://ipapi.co/json/` to resolve coordinates, cached under
`$XDG_CACHE_HOME/ornatus/location.json`. Sun math is computed locally. Set a
fixed location to avoid all network access.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
