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

//! Wayland client state.
//!
//! Owns one background `LayerSurface` per output, plus the periodic `refresh`
//! entry point invoked from the calloop timer.
//!
//! Surfaces are created reactively, from `new_output`. `sweep_unattached` is a
//! net for outputs the reactive path misses — it warns whenever it has to act,
//! so its log lines are the evidence for whether it is still needed.
//!
//! Each surface renders the wallpaper exactly once per size: the image is
//! decoded straight into the compositor's SHM buffer and nothing is kept
//! afterwards. The refresh tick no longer touches pixels at all — it exists to
//! flip the theme at the day/night boundary.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputInfo, OutputState},
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
use tracing::{debug, info, warn};
use wayland_client::{
    globals::GlobalList,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, QueueHandle,
};

use crate::location::{Coords, LocationResolver};
use crate::sun::SunDay;
use crate::theme::{Theme, ThemeManager};
use crate::wallpaper::Wallpaper;

/// Application-wide Wayland state.
pub struct WaylandApp {
    registry_state:   RegistryState,
    output_state:     OutputState,
    compositor:       CompositorState,
    layer_shell:      LayerShell,
    shm:              Shm,
    surfaces:         Vec<OutputSurface>,
    wallpaper:        Wallpaper,
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
    /// Created at first draw, sized to exactly one buffer. `SlotPool` grows
    /// itself by doubling if it ever needs more, so there is no reason to
    /// reserve ahead.
    pool:       Option<SlotPool>,
    /// Size the compositor asked for, before scaling.
    logical_w:  u32,
    logical_h:  u32,
    scale:      i32,
    configured: bool,
    /// Pixel dimensions of the buffer currently attached, if any. Guards
    /// against re-decoding the image on a configure that changed nothing.
    painted:    Option<(u32, u32)>,
}

impl OutputSurface {
    /// Buffer dimensions: the compositor's logical size times the output scale.
    fn pixel_size(&self) -> (u32, u32) {
        (
            self.logical_w * self.scale.max(1) as u32,
            self.logical_h * self.scale.max(1) as u32,
        )
    }
}

impl WaylandApp {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        globals:               &GlobalList,
        qh:                    &QueueHandle<Self>,
        wallpaper:             Wallpaper,
        coords:                Coords,
        theme_mgr:             ThemeManager,
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
            wallpaper,
            coords,
            theme_mgr,
            last_is_daytime:  is_daytime,
            resolver,
            refresh_interval: ChronoDuration::seconds(refresh_interval_secs as i64),
            last_refresh_at:  Utc::now(),
        })
    }

    /// Number of outputs that are known but have no surface yet. Zero, with at
    /// least one surface attached, is what `main` waits for before entering the
    /// event loop.
    pub fn unattached_outputs(&self) -> usize {
        self.output_state
            .outputs()
            .filter(|output| !self.surfaces.iter().any(|s| &s.output == output))
            .count()
    }

    pub fn surface_count(&self) -> usize {
        self.surfaces.len()
    }

    /// Attach anything the reactive path missed.
    ///
    /// `new_output` is the intended route and handles hotplug for the daemon's
    /// whole lifetime; this runs once, after the startup wait, and warns
    /// whenever it actually has to do something. If those warnings never appear
    /// in practice, this can go.
    pub fn sweep_unattached(&mut self, qh: &QueueHandle<Self>) {
        let outputs: Vec<_> = self.output_state.outputs().collect();
        for output in outputs {
            if self.surfaces.iter().any(|s| s.output == output) {
                continue;
            }
            match self.output_state.info(&output) {
                Some(info) => {
                    warn!(
                        output = %display_name(&info),
                        "output was not attached by new_output; attaching from startup sweep",
                    );
                    self.create_surface_for(output, qh);
                }
                None => warn!(
                    "output still has no info after the startup wait; \
                     leaving it for new_output",
                ),
            }
        }
    }

    /// Periodic refresh: detect wake from suspend, recompute the sun, and
    /// switch the theme on a day/night crossing. The wallpaper is static, so
    /// nothing is redrawn here.
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

        debug!(is_daytime, surfaces = self.surfaces.len(), "refreshed");
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
    ///
    /// Every step after the layer surface is committed is infallible, so this
    /// cannot leave a committed surface that nothing owns.
    fn create_surface_for(&mut self, output: wl_output::WlOutput, qh: &QueueHandle<Self>) {
        if self.surfaces.iter().any(|s| s.output == output) {
            return;
        }

        let info = match self.output_state.info(&output) {
            Some(i) => i,
            None => {
                warn!("no info for output yet; deferring surface creation");
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

        info!(
            output = %display_name(&info),
            scale  = info.scale_factor,
            "created layer surface",
        );

        self.surfaces.push(OutputSurface {
            output,
            layer,
            pool:       None,
            logical_w:  0,
            logical_h:  0,
            scale:      info.scale_factor,
            configured: false,
            painted:    None,
        });
    }

    /// Draw the surface at `index`, decoding the wallpaper into a fresh buffer
    /// if the surface isn't already showing this size.
    fn draw_at(&mut self, index: usize) {
        let surface = &mut self.surfaces[index];

        if !surface.configured {
            return;
        }
        let (width, height) = surface.pixel_size();
        if width == 0 || height == 0 {
            return;
        }

        // Already showing the right thing. Still commit: the toolkit acked the
        // configure for us and an ack only takes effect on the next commit.
        if surface.painted == Some((width, height)) {
            surface.layer.commit();
            return;
        }

        let stride = width as i32 * 4;
        let needed = width as usize * height as usize * 4;

        if surface.pool.is_none() {
            match SlotPool::new(needed, &self.shm) {
                Ok(pool) => surface.pool = Some(pool),
                Err(err) => {
                    warn!(error = %err, bytes = needed, "failed to create SlotPool");
                    return;
                }
            }
        }
        let Some(pool) = surface.pool.as_mut() else { return };

        let (buffer, canvas) = match pool.create_buffer(
            width as i32, height as i32, stride, wl_shm::Format::Argb8888,
        ) {
            Ok(pair) => pair,
            Err(err) => {
                warn!(error = %err, "failed to create buffer");
                return;
            }
        };

        if let Err(err) = self.wallpaper.render_into(width, height, canvas) {
            warn!(error = %err, "wallpaper render failed; leaving surface unpainted");
            return;
        }

        let wl_surface = surface.layer.wl_surface();
        wl_surface.set_buffer_scale(surface.scale);
        if let Err(err) = buffer.attach_to(wl_surface) {
            warn!(error = %err, "failed to attach buffer");
            return;
        }
        wl_surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.layer.commit();
        surface.painted = Some((width, height));

        debug!(width, height, scale = surface.scale, "painted surface");
    }
}

/// A human-readable name for an output: the connector name where the
/// compositor supplies one, else the global's numeric id.
fn display_name(info: &OutputInfo) -> String {
    info.name.clone().unwrap_or_else(|| format!("output_{}", info.id))
}

// ── Handlers ────────────────────────────────────────────────────────────────

impl OutputHandler for WaylandApp {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output_state }

    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.create_surface_for(output, qh);
    }

    fn update_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        // A scale change alters the pixel size without a new configure, so the
        // buffer has to be redrawn or the wallpaper ends up half-size.
        if let Some(info) = self.output_state.info(&output)
            && let Some(i) = self.surfaces.iter().position(|s| s.output == output)
        {
            if self.surfaces[i].scale != info.scale_factor {
                info!(
                    output = %display_name(&info),
                    from   = self.surfaces[i].scale,
                    to     = info.scale_factor,
                    "output scale changed",
                );
                self.surfaces[i].scale = info.scale_factor;
                self.draw_at(i);
            }
            return;
        }

        // Recovery path: `new_output` can fire before the output's info is
        // populated, in which case `create_surface_for` bailed and nothing
        // retried. It is idempotent, so this is a no-op once a surface exists.
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
            s.logical_w  = logical_w.max(1);
            s.logical_h  = logical_h.max(1);
            s.configured = true;
            debug!(
                logical = ?(s.logical_w, s.logical_h),
                pixel   = ?s.pixel_size(),
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
