# Notes

## Memory

Measured across the same two outputs, before and after the move from 16
retained solar-gradient frames to a single static image:

    VmRSS      226120 kB -> 64948 kB    -71.3%
    RssAnon     98564 kB -> 18644 kB    -81.1%   retained frames gone
    RssShmem   122852 kB -> 40952 kB    -66.7%   pools sized to one buffer
    RssFile      4704 kB ->  5352 kB    +13.8%

RssShmem lands at 40952 kB against a predicted 40950 kB — 2880*1920*4 plus
3440*1440*4 — so each pool holds exactly one buffer and nothing is reserved
ahead.

Two independent causes, both needed. Retained frames were 98 MB of the
baseline; oversized SlotPools were 123 MB. Dropping the frames alone would
have landed near 146 MB.

Peak memory during decode is transient and higher than the retained figure:
the full-resolution decode and the scaled copy are both live briefly.

## Startup race

OutputState::outputs() yields every *bound* output, but info() returns Some
only after that output's wl_output::done has been dispatched. A single startup
roundtrip does not guarantee that, so create_surface_for hits its
"no info for output yet" early return.

Recovery then depends on new_output, which SCTK gates behind pending_xdg
(output.rs:525) — with zxdg_output_manager_v1 bound it fires only on the
*second* wl_output::done. registry_queue_init_with_retry waits for compositor,
layer-shell and shm, but not for wl_output or the xdg manager, so which path a
given boot takes varies.

calloop-wayland-source was ruled out as a cause: its before_sleep correctly
forces a dispatch when events are already buffered.

The fix — a bounded readiness loop in main, plus sweep_unattached as a
backstop — was derived from code reading. No failing boot was ever captured.
The warn! in sweep_unattached is the instrument that will show whether the
reactive path is sufficient in practice.

## Colour scheme is asserted on change, never reconciled

Applying a theme is idempotent: if the marker and the symlinks already agree,
nothing is written and no signals are sent. The `org.gnome.desktop.interface
color-scheme` GSetting is therefore asserted only when the theme *changes*, so a
key that is wrong for any other reason stays wrong until the next sunrise or
sunset. Setting it by hand puts the desktop and the browsers in disagreement and
ornatus will not correct it.

A reconcile-on-apply in `theme.rs` closes this, and it is a small change: assert
the key whenever apply runs rather than only on the change branch.

The same shape shows up twice more, so it is worth naming as a class — the
daemon sets state but never reconciles it. `Config::load_or_create` is called
once at `main.rs:98`, and SIGUSR1 runs only the sun-and-theme path, so
`--refresh` does not reload `config.toml`. The wallpaper is likewise redrawn
only when a surface is configured or resized, never on the refresh tick, which
is why a wallpaper change needs the daemon restarted and why the failure is
quiet: the refresh succeeds and logs normally while rendering nothing new.
