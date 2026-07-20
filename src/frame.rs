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

//! Gradient frame loading and blending.
//!
//! Frames are decoded on demand: only the two bracketing the current
//! position are kept resident; the rest are evicted at every blend.
//! Resident memory stays bounded to ~2 × (width × height × 4) bytes
//! regardless of how long the daemon runs, at the cost of one ~700ms
//! decode each time the position crosses a frame boundary — invisible
//! against a 60-second refresh interval.

use anyhow::{Context, Result};
use image::imageops::FilterType;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;
use tracing::{debug, info};

/// A set of `count` gradient frames sourced from `dir/NN.jpg`, decoded and
/// resized to a specific output's dimensions on demand.
pub struct FrameSet {
    dir:    PathBuf,
    count:  u32,
    width:  u32,
    height: u32,
    /// Decoded BGRA8 frames keyed by index. After every `blend_into`, holds
    /// only the two frames bracketing the current position.
    cache:  HashMap<u32, Vec<u8>>,
}

impl FrameSet {
    /// Create an empty frame set. No I/O happens here — frames are decoded
    /// the first time `blend_into` needs them.
    pub fn new(dir: PathBuf, count: u32, width: u32, height: u32) -> Self {
        Self {
            dir,
            count,
            width,
            height,
            cache: HashMap::new(),
        }
    }

    pub fn width(&self)  -> u32 { self.width }
    pub fn height(&self) -> u32 { self.height }

    /// Decode and cache the frame at `index` if it isn't already cached.
    fn ensure_loaded(&mut self, index: u32) -> Result<()> {
        if self.cache.contains_key(&index) {
            return Ok(());
        }

        let start = Instant::now();
        let path  = self.dir.join(format!("{:02}.jpg", index));
        let img   = image::open(&path)
            .with_context(|| format!("opening frame {}", path.display()))?;

        // Triangle filter is several times faster than Lanczos3 and visually
        // indistinguishable for smooth gradients.
        let resized = img.resize_exact(self.width, self.height, FilterType::Triangle);
        let rgba    = resized.to_rgba8();

        // wl_shm Argb8888 is native-endian; on little-endian machines the byte
        // order in memory is B, G, R, A.
        let mut bgra = rgba.into_raw();
        for chunk in bgra.chunks_exact_mut(4) {
            chunk.swap(0, 2);
        }

        self.cache.insert(index, bgra);
        info!(
            index,
            elapsed_ms = start.elapsed().as_millis(),
            cache_size = self.cache.len(),
            "frame decoded"
        );
        Ok(())
    }

    /// Blend the two frames adjacent to `position` into `output`.
    ///
    /// `position` is the continuous value from `sun::frame_at` — in
    /// `[0, count)`. The lower frame is `floor(position) mod count`, the
    /// upper is the next, and the interpolation factor is `position.fract()`.
    ///
    /// Before blending, evicts any cached frame outside the bracketing pair
    /// and decodes the pair if not already cached.
    ///
    /// `output` must be exactly `width * height * 4` bytes (BGRA8).
    pub fn blend_into(&mut self, position: f64, output: &mut [u8]) -> Result<()> {
        let count     = self.count;
        let pos       = position.rem_euclid(count as f64);
        let lower     = (pos.floor() as u32) % count;
        let upper     = (lower + 1) % count;
        let t         = pos - pos.floor();
        let t_u16     = (t * 256.0) as u16;
        let inv_t_u16 = 256 - t_u16;

        // Evict first, then load — keeps peak memory at 2 × frame size even
        // mid-transition.
        self.cache.retain(|&k, _| k == lower || k == upper);
        self.ensure_loaded(lower)?;
        self.ensure_loaded(upper)?;

        let frame_a = &self.cache[&lower];
        let frame_b = &self.cache[&upper];

        debug_assert_eq!(output.len(), frame_a.len());
        debug_assert_eq!(output.len(), frame_b.len());

        // Scalar fixed-point blend. SIMD would be faster but we re-blend at
        // most once per refresh tick, so 10ms is invisible.
        for ((out, &a), &b) in output.iter_mut().zip(frame_a.iter()).zip(frame_b.iter()) {
            *out = ((a as u16 * inv_t_u16 + b as u16 * t_u16) >> 8) as u8;
        }

        debug!(lower, upper, t, "blended adjacent frames");
        Ok(())
    }
}
