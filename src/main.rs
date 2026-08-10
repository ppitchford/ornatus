// ornatus — Wayland wallpaper and theme daemon
// Copyright (C) 2026 Philipp Pitchford
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! ornatus — a Wayland-native wallpaper and theme daemon.
//!
//! Draws a static image to a `wlr-layer-shell` background surface on every
//! output, and flips a light/dark theme at the day/night boundary.
//!
//! Module layout:
//!   - [`config`]    — filesystem paths, `config.toml` loading, defaults
//!   - [`location`]  — geographic coordinates (fixed, or cached IP geolocation)
//!   - [`sun`]       — sunrise/sunset math for the day/night boundary
//!   - [`wallpaper`] — JPEG decode, scale-to-cover, write into an SHM buffer
//!   - [`theme`]     — the `current` marker, per-app symlinks, reload signals
//!   - [`wayland`]   — layer-surface state and the periodic refresh loop
//!
//! `main` wires these together: resolve location, compute the sun, apply the
//! initial theme, connect to the compositor, then run a calloop event loop that
//! services Wayland events, a periodic refresh timer, and Unix signals.

use anyhow::{anyhow, Context, Result};
use calloop::{
    signals::{Signal, Signals},
    timer::{TimeoutAction, Timer},
    EventLoop,
};
use calloop_wayland_source::WaylandSource;
use chrono::Utc;
use clap::Parser;
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tracing::{info, warn};
use wayland_client::{
    globals::{registry_queue_init, GlobalList},
    Connection, EventQueue,
};

mod config;
mod location;
mod sun;
mod theme;
mod wallpaper;
mod wayland;

use config::{Config, Paths};
use location::LocationResolver;
use sun::SunDay;
use theme::{Theme, ThemeManager};
use wallpaper::Wallpaper;
use wayland::WaylandApp;

/// Wayland-native wallpaper and theme daemon.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Force an immediate refresh of a running instance (sends SIGUSR1) and exit.
    #[arg(long)]
    refresh: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ornatus=info".into()),
        )
        .init();

    let cli   = Cli::parse();
    let paths = Paths::resolve()?;

    if cli.refresh {
        return send_refresh(&paths.pid_file);
    }

    info!("ornatus {} starting", env!("CARGO_PKG_VERSION"));

    // PID file is held for the lifetime of `main`; its Drop impl removes the
    // file on any exit path (clean shutdown, ?-propagated error, or panic).
    let _pid_file = PidFile::write(paths.pid_file.clone())?;

    let config = Config::load_or_create(&paths.config_file)?;
    info!(?config, "configuration loaded");

    let resolver = LocationResolver::new(config.location.clone(), paths.location_cache.clone());
    let coords   = resolver.resolve()?;
    info!(lat = coords.lat, lon = coords.lon, "location resolved");

    let now = Utc::now();

    // Shift `now` by the longitude offset (4 min/°) to get local mean solar
    // time; its date is the solar day we're currently in.
    let lon_offset = chrono::Duration::seconds((coords.lon * 240.0) as i64);
    let local_date = (now + lon_offset).date_naive();
    let sunday     = SunDay::compute(coords, local_date)?;
    info!(
        sunrise    = %sunday.sunrise.format("%H:%M:%SZ"),
        sunset     = %sunday.sunset.format("%H:%M:%SZ"),
        solar_noon = %sunday.solar_noon.format("%H:%M:%SZ"),
        is_daytime = sunday.is_daytime(now),
        "sun events computed"
    );

    let is_daytime = sunday.is_daytime(now);
    let theme_mgr  = ThemeManager::new(config.theme_dir.clone(), paths.config_dir.clone());
    theme_mgr.apply(Theme::from_is_daytime(is_daytime))?;

    let wallpaper = Wallpaper::new(config.wallpaper.clone());
    info!(path = %wallpaper.path().display(), "wallpaper source");

    // ── Wayland setup ───────────────────────────────────────────────────────
    // Launched from the compositor's own config, ornatus races the compositor's
    // startup: the socket may not be listening yet, and the required globals may
    // not be advertised at the first registry snapshot. Both are retried with a
    // bounded backoff so a slow boot no longer aborts the daemon before the event
    // loop starts.
    let conn = connect_with_retry(WAYLAND_STARTUP_TIMEOUT)?;
    let (globals, mut event_queue) =
        registry_queue_init_with_retry(&conn, WAYLAND_STARTUP_TIMEOUT)?;
    let qh = event_queue.handle();

    let mut app = WaylandApp::new(
        &globals,
        &qh,
        wallpaper,
        coords,
        theme_mgr,
        is_daytime,
        resolver,
        config.refresh_interval_secs,
    )?;

    await_outputs(&mut event_queue, &mut app, &qh, OUTPUT_STARTUP_TIMEOUT)?;

    // ── Event loop ──────────────────────────────────────────────────────────
    let mut event_loop: EventLoop<WaylandApp> = EventLoop::try_new()
        .context("creating calloop event loop")?;
    let handle      = event_loop.handle();
    let loop_signal = event_loop.get_signal();

    // Periodic refresh: recompute the sun and switch the theme on day/night
    // crossings. The wallpaper is static and is not redrawn here.
    let refresh_interval = Duration::from_secs(config.refresh_interval_secs);
    handle
        .insert_source(
            Timer::from_duration(refresh_interval),
            move |_, _, app: &mut WaylandApp| {
                if let Err(err) = app.refresh() {
                    warn!(error = %err, "refresh failed");
                }
                TimeoutAction::ToDuration(refresh_interval)
            },
        )
        .map_err(|e| anyhow!("inserting timer source: {e}"))?;

    // Signal handling:
    //   SIGUSR1          → manual refresh (identical to the timer path)
    //   SIGTERM / SIGINT → request a clean shutdown via LoopSignal
    let shutdown_signal = loop_signal.clone();
    handle
        .insert_source(
            Signals::new(&[Signal::SIGUSR1, Signal::SIGTERM, Signal::SIGINT])
                .context("creating signal source")?,
            move |event, _, app: &mut WaylandApp| {
                let sig = event.signal();
                match sig {
                    Signal::SIGUSR1 => {
                        info!("SIGUSR1 received, refreshing");
                        if let Err(err) = app.refresh() {
                            warn!(error = %err, "refresh on SIGUSR1 failed");
                        }
                    }
                    Signal::SIGTERM | Signal::SIGINT => {
                        info!(signal = ?sig, "termination signal received, shutting down");
                        shutdown_signal.stop();
                    }
                    _ => {}
                }
            },
        )
        .map_err(|e| anyhow!("inserting signal source: {e}"))?;

    // Wayland events as another source on the same loop.
    WaylandSource::new(conn, event_queue)
        .insert(handle)
        .map_err(|e| anyhow!("inserting Wayland source: {e}"))?;

    info!(
        refresh_interval_secs = config.refresh_interval_secs,
        "entering event loop"
    );

    event_loop
        .run(None, &mut app, |_| {})
        .context("event loop failed")?;

    info!("shutdown complete");
    Ok(())
}

/// How long to wait for the compositor to become ready when ornatus is exec'd
/// from the compositor's own config and wins the startup race.
const WAYLAND_STARTUP_TIMEOUT: Duration = Duration::from_secs(2);

/// Backoff between startup readiness polls.
const WAYLAND_STARTUP_BACKOFF: Duration = Duration::from_millis(100);

/// How long to wait for outputs to finish advertising themselves before
/// entering the event loop.
const OUTPUT_STARTUP_TIMEOUT: Duration = Duration::from_millis(500);

/// Backoff between output readiness roundtrips.
const OUTPUT_STARTUP_BACKOFF: Duration = Duration::from_millis(20);

/// The globals `WaylandApp::new` binds unconditionally. If any is missing the
/// daemon can't function, so we wait for all three before constructing state.
const REQUIRED_GLOBALS: [&str; 3] = ["wl_compositor", "zwlr_layer_shell_v1", "wl_shm"];

/// Connect to the compositor, retrying until `timeout` elapses. Even with
/// `WAYLAND_DISPLAY` set, the socket may not be listening yet on the earliest
/// boots; on timeout the final error propagates unchanged.
fn connect_with_retry(timeout: Duration) -> Result<Connection> {
    let deadline = Instant::now() + timeout;
    loop {
        match Connection::connect_to_env() {
            Ok(conn) => return Ok(conn),
            Err(err) if Instant::now() < deadline => {
                warn!(error = %err, "Wayland connect failed; retrying");
                std::thread::sleep(WAYLAND_STARTUP_BACKOFF);
            }
            Err(err) => {
                return Err(err)
                    .context("connecting to Wayland compositor (is WAYLAND_DISPLAY set?)");
            }
        }
    }
}

/// Initialise the Wayland registry, retrying until every entry in
/// `REQUIRED_GLOBALS` is advertised or `timeout` elapses. Each attempt takes a
/// fresh registry snapshot, so globals the compositor advertises late are
/// eventually seen. On timeout the most recent snapshot is returned regardless,
/// letting `WaylandApp::new` surface the precise `<interface> not advertised`
/// error it would have produced without the retry.
fn registry_queue_init_with_retry(
    conn: &Connection,
    timeout: Duration,
) -> Result<(GlobalList, EventQueue<WaylandApp>)> {
    let deadline = Instant::now() + timeout;
    loop {
        let (globals, event_queue) =
            registry_queue_init(conn).context("initialising Wayland registry")?;
        if required_globals_present(&globals) || Instant::now() >= deadline {
            return Ok((globals, event_queue));
        }
        warn!("required Wayland globals not yet advertised; retrying");
        std::thread::sleep(WAYLAND_STARTUP_BACKOFF);
    }
}

/// True once every interface in `REQUIRED_GLOBALS` appears in the snapshot.
fn required_globals_present(globals: &GlobalList) -> bool {
    globals.contents().with_list(|list| {
        REQUIRED_GLOBALS
            .iter()
            .all(|needed| list.iter().any(|g| g.interface == *needed))
    })
}

/// Pump the queue until every output the compositor has advertised owns a
/// layer surface, or `timeout` elapses.
///
/// Surfaces are created reactively from `OutputHandler::new_output`, which SCTK
/// only fires once an output's `wl_output::done` — and its xdg-output info, if
/// the manager is bound — has arrived. A single roundtrip does not guarantee
/// that on a slow boot, which is what used to leave an output permanently
/// blank. This waits for the reactive path instead of racing it, then sweeps up
/// anything still unattached.
fn await_outputs(
    event_queue: &mut EventQueue<WaylandApp>,
    app:         &mut WaylandApp,
    qh:          &wayland_client::QueueHandle<WaylandApp>,
    timeout:     Duration,
) -> Result<()> {
    let start    = Instant::now();
    let deadline = start + timeout;

    loop {
        event_queue.roundtrip(app).context("Wayland roundtrip")?;

        if app.surface_count() > 0 && app.unattached_outputs() == 0 {
            info!(
                outputs    = app.surface_count(),
                elapsed_ms = start.elapsed().as_millis(),
                "all advertised outputs attached",
            );
            break;
        }

        if Instant::now() >= deadline {
            warn!(
                attached   = app.surface_count(),
                unattached = app.unattached_outputs(),
                elapsed_ms = start.elapsed().as_millis(),
                "timed out waiting for outputs; sweeping",
            );
            break;
        }

        std::thread::sleep(OUTPUT_STARTUP_BACKOFF);
    }

    // Anything the reactive path missed. Warns when it has to act; an output
    // that is still info-less is left for `new_output` to pick up once the
    // event loop is running.
    app.sweep_unattached(qh);
    Ok(())
}

/// Read the PID file, send SIGUSR1 to that process, exit.
/// Errors are routed through the same tracing/anyhow path as the daemon's,
/// so the user sees a clean message either way.
fn send_refresh(pid_file: &Path) -> Result<()> {
    let text = fs::read_to_string(pid_file).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => {
            anyhow!("no running ornatus instance (no PID file at {})", pid_file.display())
        }
        _ => anyhow::Error::new(e)
            .context(format!("reading PID file at {}", pid_file.display())),
    })?;

    let pid: i32 = text.trim().parse().with_context(|| {
        format!("parsing PID from {}", pid_file.display())
    })?;

    // SAFETY: libc::kill is safe to call with any pid/signal; we check the
    // return value and translate errno into an anyhow error.
    let ret = unsafe { libc::kill(pid, libc::SIGUSR1) };
    if ret != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Err(anyhow!(
                "stale PID file (no process {}); consider removing {}",
                pid,
                pid_file.display()
            ));
        }
        return Err(anyhow!("failed to send SIGUSR1 to {}: {}", pid, err));
    }

    info!("sent SIGUSR1 to ornatus pid {}", pid);
    Ok(())
}

/// PID file lifecycle bound to its own scope via Drop — guarantees cleanup
/// on clean shutdown, error propagation, or panic.
struct PidFile {
    path: PathBuf,
}

impl PidFile {
    fn write(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating runtime dir at {}", parent.display()))?;
        }
        let pid = std::process::id();
        fs::write(&path, pid.to_string())
            .with_context(|| format!("writing PID file at {}", path.display()))?;
        info!(pid, path = %path.display(), "wrote PID file");
        Ok(Self { path })
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        // Only unlink if the file still names *this* process. A dying instance
        // can otherwise race a newly-started one and delete its PID file.
        let me = std::process::id();
        match fs::read_to_string(&self.path) {
            Ok(contents) => match contents.trim().parse::<u32>() {
                Ok(pid) if pid == me => match fs::remove_file(&self.path) {
                    Ok(()) => info!(path = %self.path.display(), "removed PID file"),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => warn!(
                        error = %err,
                        path  = %self.path.display(),
                        "failed to remove PID file",
                    ),
                },
                Ok(pid) => info!(
                    path  = %self.path.display(),
                    owner = pid,
                    "PID file belongs to another instance, leaving it",
                ),
                Err(err) => warn!(
                    error = %err,
                    path  = %self.path.display(),
                    "PID file contents unparseable, leaving it",
                ),
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => warn!(
                error = %err,
                path  = %self.path.display(),
                "failed to read PID file",
            ),
        }
    }
}
