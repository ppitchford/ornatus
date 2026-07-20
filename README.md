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
- Build-time system libraries `libwayland-dev` and `libxkbcommon-dev`
  (Debian/Ubuntu names; located via `pkg-config` while compiling)
- `XDG_RUNTIME_DIR` set at runtime (used for the PID file)

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

## Running at startup

`ornatus` is a foreground daemon; start it once per session from your
compositor's config. It draws to a `wlr-layer-shell` background surface, so it
must run *after* the compositor is up — but it tolerates being launched early:
if the Wayland socket or the compositor's required globals aren't ready yet, it
retries for up to two seconds before giving up, so racing the compositor's own
startup is safe.

**Sway** — `~/.config/sway/config`:

```
exec ornatus
```

**Hyprland** — `~/.config/hypr/hyprland.conf`:

```
exec-once = ornatus
```

**River** — `~/.config/river/init`:

```
riverctl spawn ornatus
```

### Alternative: systemd user service

If you drive your session with systemd and reach `graphical-session.target`, a
unit is provided in [`contrib/ornatus.service`](contrib/ornatus.service):

```sh
install -Dm644 contrib/ornatus.service ~/.config/systemd/user/ornatus.service
systemctl --user enable --now ornatus.service
```

It is ordered `After=graphical-session.target` and reloads (sends `SIGUSR1`) on
`systemctl --user reload ornatus`. Your compositor must export `WAYLAND_DISPLAY`
into the systemd user environment for this to work — many do so automatically,
otherwise run `systemctl --user import-environment WAYLAND_DISPLAY` once the
compositor is running.

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

With `location.source = "auto"`, ornatus resolves coordinates from
`https://ipapi.co/json/` and caches them under
`$XDG_CACHE_HOME/ornatus/location.json`. The cache has a 24-hour TTL: within a
day the cached value is reused with no network access, and it is also re-fetched
on resume from suspend (in case you have moved). If a fetch fails, ornatus falls
back to the last cached value, so it keeps working offline once primed. Sun math
is always computed locally. Set a fixed location to avoid all network access.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
