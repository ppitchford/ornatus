// ornatus — solar-gradient wallpaper and theme daemon
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
    time::Duration,
};
use tracing::{info, warn};
use wayland_client::{globals::registry_queue_init, Connection};

mod config;
mod frame;
mod location;
mod sun;
mod theme;
mod wayland;

use config::{Config, Paths};
use location::LocationResolver;
use sun::SunDay;
use theme::{Theme, ThemeManager};
use wayland::WaylandApp;

/// Wayland-native solar-gradient wallpaper and theme daemon.
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

    let frame_pos = sun::frame_at(coords, now);
    info!(frame_position = %format!("{:.3}", frame_pos), "current frame position");

    // ── Wayland setup ───────────────────────────────────────────────────────
    let conn = Connection::connect_to_env()
        .context("connecting to Wayland compositor (is WAYLAND_DISPLAY set?)")?;
    let (globals, mut event_queue) = registry_queue_init(&conn)
        .context("initialising Wayland registry")?;
    let qh = event_queue.handle();

    let mut app = WaylandApp::new(
        &globals,
        &qh,
        config.wallpaper_dir.clone(),
        16,
        coords,
        theme_mgr,
        frame_pos,
        is_daytime,
        resolver,
        config.refresh_interval_secs,
        )?;

    // First roundtrip populates output info; then create layer surfaces.
    event_queue.roundtrip(&mut app).context("initial Wayland roundtrip")?;
    app.attach_to_outputs(&qh);

    // ── Event loop ──────────────────────────────────────────────────────────
    let mut event_loop: EventLoop<WaylandApp> = EventLoop::try_new()
        .context("creating calloop event loop")?;
    let handle      = event_loop.handle();
    let loop_signal = event_loop.get_signal();

    // Periodic refresh: recompute frame position, redraw, switch theme on
    // day/night crossings.
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
        match fs::remove_file(&self.path) {
            Ok(()) => info!(path = %self.path.display(), "removed PID file"),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => warn!(
                error = %err,
                path  = %self.path.display(),
                "failed to remove PID file",
            ),
        }
    }
}
