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

//! Wayland client state.
//!
//! Owns per-output `LayerSurface`s with lazy-loaded `FrameSet`s, plus the
//! periodic `refresh` entry point invoked from the calloop timer.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use std::path::PathBuf;
use tracing::{debug, info, warn};
use wayland_client::{
    globals::GlobalList,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, QueueHandle,
};

use crate::frame::FrameSet;
use crate::location::{Coords, LocationResolver};
use crate::sun::{self, SunDay};
use crate::theme::{Theme, ThemeManager};

// Initial SHM pool size — 64 MB allows two 4K BGRA buffers comfortably.
const POOL_INITIAL_BYTES: usize = 64 * 1024 * 1024;

/// Application-wide Wayland state.
pub struct WaylandApp {
    registry_state:   RegistryState,
    output_state:     OutputState,
    compositor:       CompositorState,
    layer_shell:      LayerShell,
    shm:              Shm,
    surfaces:         Vec<OutputSurface>,
    frame_dir:        PathBuf,
    frame_count:      u32,
    frame_position:   f64,
    coords:           Coords,
    theme_mgr:        ThemeManager,
    last_is_daytime:  bool,
    resolver:         LocationResolver,
    refresh_interval: ChronoDuration,
    last_refresh_at:  DateTime<Utc>,
}

/// One per attached output.
struct OutputSurface {
    output:     wl_output::WlOutput,
    layer:      LayerSurface,
    pool:       SlotPool,
    /// Pixel dimensions (logical × scale).
    width:      u32,
    height:     u32,
    scale:      i32,
    configured: bool,
    frames:     Option<FrameSet>,
}

impl WaylandApp {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        globals:               &GlobalList,
        qh:                    &QueueHandle<Self>,
        frame_dir:             PathBuf,
        frame_count:           u32,
        coords:                Coords,
        theme_mgr:             ThemeManager,
        frame_position:        f64,
        is_daytime:            bool,
        resolver:              LocationResolver,
        refresh_interval_secs: u64,
    ) -> Result<Self> {
        let compositor  = CompositorState::bind(globals, qh)
            .context("wl_compositor not advertised")?;
        let layer_shell = LayerShell::bind(globals, qh)
            .context("zwlr_layer_shell_v1 not advertised — non-wlroots compositor?")?;
        let shm         = Shm::bind(globals, qh)
            .context("wl_shm not advertised")?;

        Ok(Self {
            registry_state: RegistryState::new(globals),
            output_state:   OutputState::new(globals, qh),
            compositor,
            layer_shell,
            shm,
            surfaces:        Vec::new(),
            frame_dir,
            frame_count,
            frame_position,
            coords,
            theme_mgr,
            last_is_daytime:  is_daytime,
            resolver,
            refresh_interval: ChronoDuration::seconds(refresh_interval_secs as i64),
            last_refresh_at:  Utc::now(),
        })
    }

    /// Create a layer surface for every currently-known output. Called once
    /// at startup after the initial roundtrip has populated the output state.
    pub fn attach_to_outputs(&mut self, qh: &QueueHandle<Self>) {
        let outputs: Vec<_> = self.output_state.outputs().collect();
        for output in outputs {
            self.create_surface_for(output, qh);
        }
    }

    /// Periodic refresh: detect wake from suspend, recompute the sun's
    /// current position, redraw all configured surfaces, and switch the
    /// theme on a day/night crossing.
    pub fn refresh(&mut self) -> Result<()> {
        let now = Utc::now();

        // Suspend/wake detection via wall-clock jump. A timer that should
        // have fired ~refresh_interval seconds ago but is firing now after
        // a much larger gap is overwhelmingly likely to have been suspended.
        // The user may have moved — force a fresh geolocation fetch.
        let elapsed = now - self.last_refresh_at;
        if elapsed > self.refresh_interval * 2 {
            info!(
                elapsed_secs = elapsed.num_seconds(),
                "wake from suspend detected",
            );
            self.refresh_location();
        }

        // Recompute today's sun events for the local solar date.
        let lon_offset = ChronoDuration::seconds((self.coords.lon * 240.0) as i64);
        let local_date = (now + lon_offset).date_naive();
        let sunday     = SunDay::compute(self.coords, local_date)?;

        // Theme switch on day/night crossings only.
        let is_daytime = sunday.is_daytime(now);
        if is_daytime != self.last_is_daytime {
            info!(is_daytime, "daytime state changed");
            self.theme_mgr.apply(Theme::from_is_daytime(is_daytime))?;
            self.last_is_daytime = is_daytime;
        }

        // Update frame position and redraw all surfaces.
        self.frame_position = sun::frame_at(self.coords, now);
        debug!(frame_position = self.frame_position, "refreshed");

        let count = self.surfaces.len();
        for i in 0..count {
            self.draw_at(i);
        }

        self.last_refresh_at = now;
        Ok(())
    }

    /// Force a location refetch. On failure, keep the existing `self.coords`
    /// — they're either the stale cache value or a previously-good fetch,
    /// both of which are more useful than aborting the refresh entirely.
    fn refresh_location(&mut self) {
        match self.resolver.resolve_force() {
            Ok(coords) => {
                info!(lat = coords.lat, lon = coords.lon, "location refreshed");
                self.coords = coords;
            }
            Err(err) => warn!(
                error = %err,
                "force-refresh location failed; keeping existing coords",
            ),
        }
    }

    /// Idempotent: skips if a surface already exists for this output.
    fn create_surface_for(&mut self, output: wl_output::WlOutput, qh: &QueueHandle<Self>) {
        if self.surfaces.iter().any(|s| s.output == output) {
            return;
        }

        let info = match self.output_state.info(&output) {
            Some(i) => i,
            None => {
                warn!("no info for output yet; skipping surface creation");
                return;
            }
        };

        let wl_surface = self.compositor.create_surface(qh);
        let layer      = self.layer_shell.create_layer_surface(
            qh,
            wl_surface,
            Layer::Background,
            Some("ornatus"),
            Some(&output),
        );

        // Full-screen, ignore other exclusive zones (we render under bars).
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);  // compositor decides
        layer.commit();        // initial commit elicits a configure event

        let pool = match SlotPool::new(POOL_INITIAL_BYTES, &self.shm) {
            Ok(p) => p,
            Err(err) => {
                warn!(error = %err, "failed to create SlotPool; output skipped");
                return;
            }
        };

        let display_name = info.name.clone().unwrap_or_else(|| format!("output_{}", info.id));
        info!(output = %display_name, scale = info.scale_factor, "created layer surface");

        self.surfaces.push(OutputSurface {
            output,
            layer,
            pool,
            width:      0,
            height:     0,
            scale:      info.scale_factor,
            configured: false,
            frames:     None,
        });
    }

    /// Draw the surface at `index`: ensure the FrameSet matches the current
    /// dimensions, then blend the bracketing pair into a fresh buffer.
    fn draw_at(&mut self, index: usize) {
        let surface = &mut self.surfaces[index];

        if !surface.configured || surface.width == 0 || surface.height == 0 {
            return;
        }

        // FrameSet construction is cheap — no I/O. Decode happens lazily inside
        // blend_into when the bracketing pair isn't yet cached.
        let need_new = match &surface.frames {
            None      => true,
            Some(fs)  => fs.width() != surface.width || fs.height() != surface.height,
        };
        if need_new {
            surface.frames = Some(FrameSet::new(
                self.frame_dir.clone(),
                self.frame_count,
                surface.width,
                surface.height,
            ));
        }

        let width  = surface.width  as i32;
        let height = surface.height as i32;
        let stride = width * 4;

        let (buffer, canvas) = match surface.pool.create_buffer(
            width, height, stride, wl_shm::Format::Argb8888,
        ) {
            Ok(pair) => pair,
            Err(err) => {
                warn!(error = %err, "failed to create buffer");
                return;
            }
        };

        if let Err(err) = surface.frames.as_mut().unwrap().blend_into(self.frame_position, canvas) {
            warn!(error = %err, "frame blend failed; skipping surface paint");
            return;
        }

        let wl_surface = surface.layer.wl_surface();
        wl_surface.set_buffer_scale(surface.scale);
        if let Err(err) = buffer.attach_to(wl_surface) {
            warn!(error = %err, "failed to attach buffer");
            return;
        }
        wl_surface.damage_buffer(0, 0, width, height);
        surface.layer.commit();

        debug!(
            width,
            height,
            position = self.frame_position,
            "painted surface"
        );
    }
}

// ── Handlers ────────────────────────────────────────────────────────────────

impl OutputHandler for WaylandApp {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output_state }

    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.create_surface_for(output, qh);
    }

    fn update_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        // Recovery path: if `new_output` fired before this output's info was
        // populated, `create_surface_for` bailed out and nothing retried.
        // It's idempotent, so this is a no-op when the surface already exists.
        self.create_surface_for(output, qh);
    }

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let before = self.surfaces.len();
        self.surfaces.retain(|s| s.output != output);
        if self.surfaces.len() < before {
            info!("output destroyed; surface torn down");
        }
    }
}

impl CompositorHandler for WaylandApp {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl LayerShellHandler for WaylandApp {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        self.surfaces.retain(|s| s.layer.wl_surface() != layer.wl_surface());
        info!("layer surface closed by compositor");
    }

    fn configure(
        &mut self,
        _: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let index = self.surfaces.iter()
            .position(|s| s.layer.wl_surface() == layer.wl_surface());

        if let Some(i) = index {
            let (logical_w, logical_h) = configure.new_size;
            let s = &mut self.surfaces[i];
            s.width      = logical_w.max(1) * s.scale as u32;
            s.height     = logical_h.max(1) * s.scale as u32;
            s.configured = true;
            debug!(
                logical = ?(logical_w, logical_h),
                pixel   = ?(s.width, s.height),
                scale   = s.scale,
                "layer surface configured"
            );
            self.draw_at(i);
        }
    }
}

impl ShmHandler for WaylandApp {
    fn shm_state(&mut self) -> &mut Shm { &mut self.shm }
}

impl ProvidesRegistryState for WaylandApp {
    fn registry(&mut self) -> &mut RegistryState { &mut self.registry_state }
    registry_handlers![OutputState];
}

delegate_compositor!(WaylandApp);
delegate_output!(WaylandApp);
delegate_layer!(WaylandApp);
delegate_shm!(WaylandApp);
delegate_registry!(WaylandApp);
