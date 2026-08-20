# CLAUDE.md

Working agreement for Claude when contributing to `ornatus`.

## Project

Wayland-native wallpaper and theme daemon for wlroots compositors. Draws a
static image to a `wlr-layer-shell` background surface on every output, and at
sunrise and sunset flips a light/dark marker and re-points per-application
config symlinks so the terminal, launcher and notification daemon follow the
sky. Single user, single binary, Rust. See `README.md` for usage and
configuration, `NOTES.md` for engineering findings.

Running in production on the author's machine — it owns the desktop's
appearance, so a regression is visible immediately and at every login.

## Working agreement — learning vehicle

**This repo is a learning vehicle. I write the Rust; you teach.**

This is a deliberate change of terms as of 2026-08-20. Much of the existing
code was written by Claude, in this repo and in `frame`, under an agreement
that permitted it. That agreement is withdrawn.

- Explain the concept, name the approach, point at the relevant crate API or
  idiom.
- Review what I write and say plainly what is wrong with it.
- **Do not hand me a finished implementation to paste, and do not write it "as
  an example" for me to adapt.** This is the rule the previous agreement
  lacked, and its absence is why the code is not mine.
- When I am stuck, narrow the gap rather than closing it: the next hint, not
  the answer.
- **Fade deliberately.** Start explain-then-write. Once a pattern is familiar,
  drop to review-after — I attempt it solo, you critique — then to hint-only
  for the routine. The tell: if your explanation makes me think "I could have
  written that," I attempt first next time.
- **Exception — diagnosis.** Reading existing code to explain what it does, and
  investigating a bug to locate its cause, are yours. Writing the fix is mine.

Reading this codebase is part of the exercise. It is roughly 1,600 lines across
eight files, small enough to read completely, and I have not read all of it.

## Stack

- `smithay-client-toolkit` + `wayland-client` — `wlr-layer-shell` surfaces, SHM
  buffers, output management.
- `calloop` + `calloop-wayland-source` — event loop, signal handling.
- `image` with only the `jpeg` feature — no format zoo.
- `chrono` + `sunrise` — local sun math, no network.
- `ureq`, blocking — IP geolocation only. Deliberately no async runtime.

## Layout

| File | Responsibility |
| --- | --- |
| `main.rs` | entrypoint, config load, event loop, signal wiring |
| `config.rs` | `config.toml` parsing and defaults |
| `location.rs` | IP geolocation, cached |
| `sun.rs` | sunrise/sunset computation |
| `theme.rs` | marker file, symlink re-pointing, colour-scheme GSetting |
| `wallpaper.rs` | image decode and scaling |
| `wayland/state.rs` | layer-shell surfaces, output handling, SHM pools |

## Known shape of the code

`NOTES.md` records three findings worth reading before changing anything:
memory behaviour after the move to a single static image, an output-readiness
race at startup, and the reconcile gap — the daemon asserts state on change but
never reconciles it, which shows up in the colour-scheme key, in `--refresh`
not reloading `config.toml`, and in the wallpaper redrawing only on surface
configure or resize.

## Anti-patterns

- No async runtime. `ureq` is blocking on purpose and the event loop is
  `calloop`.
- No configurability for users who do not exist. Audience of one.
- No abstractions added "in case we need them later."
