# TODO

## Goal
Make layer-surface attachment survive a slow compositor boot, replace the
16-frame solar gradient with a single configurable static image, and cut
resident memory from the measured 226 MB.

## Tasks

### 1. Baseline
- [x] Record RSS of the running instance (`RssAnon` / `RssShmem` / `RssFile`
      from `/proc/<pid>/status`) into the Notes section below.

### 2. Startup race (`src/wayland/state.rs`, `src/main.rs`)
- [x] Make `create_surface_for` fully idempotent and failure-safe: destroy the
      layer surface if `SlotPool` creation fails, so a partial surface is never
      left committed and unowned. *Achieved by construction — moving the pool
      into `draw_at` (task 4) leaves no fallible step after the commit, so
      there is nothing to unwind.*
- [x] Make surface creation reactive: `new_output` is the primary path,
      `update_output` the recovery path (already committed in dda8875).
- [x] Replace the single `event_queue.roundtrip` + `attach_to_outputs` in
      `main.rs` with a bounded readiness loop: roundtrip until every bound
      output reports `info()`, or a 500 ms deadline elapses.
- [x] Replace `attach_to_outputs` with `sweep_unattached`, which creates
      surfaces only for outputs the reactive path missed and emits a `warn!`
      naming the output when it has to act.
- [x] Refresh `OutputSurface.scale` from the current `OutputInfo` in
      `update_output`, so a scale change no longer leaves a half-size buffer.

### 3. Static wallpaper (`src/frame.rs` → `src/wallpaper.rs`)
- [x] Add `Config.wallpaper: PathBuf` with `#[serde(default)]` defaulting to
      `~/Pictures/wallpapers/watchtower.jpg`; remove `wallpaper_dir`. An
      existing `config.toml` must still load.
- [x] Replace `FrameSet` with `Wallpaper`: decode the source, scale-to-cover
      with a centre crop for the target dimensions, swap RGBA→BGRA, write
      straight into the SHM canvas, retain nothing.
- [x] Delete `frame_at`, `morning_position`, `afternoon_position` and
      `SOLAR_GRADIENTS` from `src/sun.rs`; keep `SunDay` and `is_daytime`.
- [x] Drop `frame_dir`, `frame_count` and `frame_position` from `WaylandApp`
      and its constructor; drop the per-surface `frames` cache.
- [x] Remove the redraw loop from `refresh`. Location refetch, sun
      recomputation, and the day/night theme switch stay exactly as they are.
- [x] Update the crate description in `Cargo.toml` and the module docs in
      `main.rs`, `frame.rs`→`wallpaper.rs` and `state.rs` to match.

### 4. Memory (`src/wayland/state.rs`)
- [x] Make `OutputSurface.pool` lazy (`Option<SlotPool>`), created at first
      draw and sized to exactly one buffer (`width * height * 4`) instead of
      the flat 64 MB `POOL_INITIAL_BYTES`. `SlotPool::alloc` grows by doubling
      if it ever needs more.

### 5. Verify
- [x] `cargo build --release` clean, `cargo clippy` clean if available.
      Both clean, no warnings. `cargo test` passes 4 `cover_crop` tests.
- [x] Install with `install -Dm755 target/release/ornatus ~/.local/bin/ornatus`
      and restart the daemon (brief wallpaper flicker — Philipp may prefer to
      run this).
- [x] Confirm both outputs attach and paint watchtower.jpg undistorted.
      eDP-1 confirmed by screenshot; DP-3 was fully covered by windows, so it
      is confirmed by its `wallpaper rendered width=3440 height=1440` log line
      and by `RssShmem` matching the sum of both buffers exactly.
- [x] Record post-change RSS in Notes and state whether task 4 was needed
      separately from task 3.

### 6. Optional, vetoable
- [x] Rewrite `~/.config/ornatus/config.toml` to drop the now-dead
      `wallpaper_dir` key and name `wallpaper` explicitly. Verified by
      restarting the daemon: the file parses and both outputs render.
      Not tracked in the dotfiles bare repo, so there was nothing to commit.

## Notes

### Measured baseline (pid 1920, ornatus 0.1.0)
```
VmRSS    226120 kB
RssAnon   98564 kB   two retained BGRA frames per surface
RssShmem 122852 kB   two 64 MB SlotPools
RssFile    4704 kB
```
eDP-1 is 2880x1920 @ scale 2 (21.1 MiB/frame), DP-3 is 3440x1440 @ scale 1
(18.9 MiB/frame). Four retained frames = 80 MiB, matching `RssAnon` once
decode scratch is included.

### Measured result (pid 14128, same two outputs)
```
             before        after      delta
VmRSS      226120 kB    64948 kB    -71.3%
RssAnon     98564 kB    18644 kB    -81.1%   retained frames gone
RssShmem   122852 kB    40952 kB    -66.7%   pools sized to one buffer
RssFile      4704 kB     5352 kB     +13.8%
```
`RssShmem` is now 40952 kB against a predicted 40950 kB — 2880*1920*4 plus
3440*1440*4 — so the pools hold exactly one buffer each and nothing is
reserved ahead.

Task 3 and task 4 fixed different halves and both were needed. The retained
frames were 98 MB (43% of the baseline) and the oversized pools 123 MB (54%).
Task 3 alone would have landed at roughly 146 MB; the `POOL_INITIAL_BYTES`
change was separately necessary and was the larger of the two.

### Startup behaviour after the change
`all advertised outputs attached outputs=2 elapsed_ms=0` — both surfaces were
created by `new_output` inside the first roundtrip, and `sweep_unattached`
logged nothing. That is one clean boot, not proof; the sweep's `warn!` is what
to watch for over the next weeks of real boots.

### Race mechanism
`OutputState::outputs()` yields every *bound* output, but `info()` only returns
`Some` after that output's `wl_output::done` has been dispatched. The single
startup roundtrip does not guarantee that, so `create_surface_for` hits its
`no info for output yet` early return. Recovery then depends on `new_output`,
which SCTK gates behind `pending_xdg` (`output.rs:525`) — with
`zxdg_output_manager_v1` bound it fires only on the *second* `wl_output::done`.
`registry_queue_init_with_retry` waits for compositor/layer-shell/shm but not
for `wl_output` or the xdg manager, so which path a boot takes varies.
`calloop-wayland-source` was ruled out: its `before_sleep` correctly forces a
dispatch when events are already buffered.

### Risks
- No failing run was captured — `/tmp/ornatus.log` is a clean boot. The fix is
  derived from code reading, so the sweep's `warn!` is the instrument that will
  tell us whether the reactive path is actually sufficient.
- `image` stays jpeg-only. A PNG configured as `wallpaper` will fail; the error
  names the path.
- Scale-to-cover crops rather than distorts. On 3:2 and 43:18 panels the two
  outputs will show different crops of the same photo — this is intended, and
  differs from the old `resize_exact` behaviour.
- Peak memory during decode is transient and larger than the retained figure:
  the full-resolution decode plus the scaled copy are both live briefly.

## Discovered Tasks
- [x] Drop `sun = "0.3"` from `Cargo.toml`. `sun::pos` was called only from
      `frame_at`, so deleting that function left the dependency unused. Done as
      part of task 3's deletion rather than left as dead weight — say the word
      and it goes back.
