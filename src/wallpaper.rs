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

//! The wallpaper image: decode, scale to cover, and write into an SHM buffer.
//!
//! Nothing is retained between renders. The decoded image exists only for the
//! duration of `render_into`, which writes its result straight into the
//! compositor's shared-memory buffer — so the daemon's steady-state footprint
//! is the SHM buffer alone, not a copy of it. A render costs one full decode
//! (~300ms for a 4K JPEG) and happens only when a surface is first configured
//! or changes size, not on the refresh tick.

use anyhow::{bail, Context, Result};
use image::imageops::FilterType;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tracing::info;

/// A wallpaper source. Holds only the path — pixels are transient.
pub struct Wallpaper {
    path: PathBuf,
}

impl Wallpaper {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Decode the image, scale it to cover `width` × `height`, centre-crop the
    /// overflow, and write it into `output` as BGRA8.
    ///
    /// `output` must be exactly `width * height * 4` bytes.
    pub fn render_into(&self, width: u32, height: u32, output: &mut [u8]) -> Result<()> {
        let expected = width as usize * height as usize * 4;
        if output.len() != expected {
            bail!(
                "buffer is {} bytes, expected {} for {}x{}",
                output.len(),
                expected,
                width,
                height,
            );
        }
        if width == 0 || height == 0 {
            bail!("refusing to render at {}x{}", width, height);
        }

        let start = Instant::now();
        // `image` is built with jpeg support only; a PNG or WebP configured as
        // `wallpaper` fails here, and the error names the path.
        let img = image::open(&self.path).with_context(|| {
            format!("decoding wallpaper {} (JPEG only)", self.path.display())
        })?;

        let (crop_x, crop_y, crop_w, crop_h) =
            cover_crop(img.width(), img.height(), width, height);

        // Triangle is several times faster than Lanczos3 and the difference is
        // invisible when downscaling a photograph to screen resolution.
        let scaled = img
            .crop_imm(crop_x, crop_y, crop_w, crop_h)
            .resize_exact(width, height, FilterType::Triangle);
        let rgba = scaled.to_rgba8();

        // wl_shm Argb8888 is native-endian, so on little-endian machines the
        // byte order in memory is B, G, R, A.
        for (dst, src) in output.chunks_exact_mut(4).zip(rgba.as_raw().chunks_exact(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = src[3];
        }

        info!(
            path       = %self.path.display(),
            width,
            height,
            source     = ?(img.width(), img.height()),
            elapsed_ms = start.elapsed().as_millis(),
            "wallpaper rendered",
        );
        Ok(())
    }
}

/// The largest centred region of a `src_w` × `src_h` image matching the aspect
/// ratio of `dst_w` × `dst_h`. Scaling that region to the destination fills it
/// completely without distortion, at the cost of cropping the overflowing axis.
fn cover_crop(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> (u32, u32, u32, u32) {
    // Compare aspect ratios as cross-multiplied integers to avoid float error
    // deciding which axis overflows on an exact match.
    let src_wide = src_w as u64 * dst_h as u64 > dst_w as u64 * src_h as u64;

    let (crop_w, crop_h) = if src_wide {
        // Source is proportionally wider: full height, trim the sides.
        let w = (src_h as u64 * dst_w as u64 / dst_h as u64).min(src_w as u64) as u32;
        (w.max(1), src_h)
    } else {
        // Source is proportionally taller (or equal): full width, trim top and
        // bottom.
        let h = (src_w as u64 * dst_h as u64 / dst_w as u64).min(src_h as u64) as u32;
        (src_w, h.max(1))
    };

    ((src_w - crop_w) / 2, (src_h - crop_h) / 2, crop_w, crop_h)
}

#[cfg(test)]
mod tests {
    use super::cover_crop;

    #[test]
    fn identical_aspect_is_uncropped() {
        assert_eq!(cover_crop(3840, 2160, 1920, 1080), (0, 0, 3840, 2160));
    }

    #[test]
    fn wide_source_trims_the_sides() {
        // 2:1 source into a 1:1 target keeps full height, half the width.
        assert_eq!(cover_crop(2000, 1000, 500, 500), (500, 0, 1000, 1000));
    }

    #[test]
    fn tall_source_trims_top_and_bottom() {
        // 1:2 source into a 1:1 target keeps full width, half the height.
        assert_eq!(cover_crop(1000, 2000, 500, 500), (0, 500, 1000, 1000));
    }

    #[test]
    fn crop_never_exceeds_the_source() {
        for (sw, sh, dw, dh) in [
            (4000u32, 2000u32, 3440u32, 1440u32),
            (4000, 2000, 2880, 1920),
            (1, 1, 3440, 1440),
            (3, 7, 2880, 1920),
        ] {
            let (x, y, w, h) = cover_crop(sw, sh, dw, dh);
            assert!(w >= 1 && h >= 1, "empty crop for {sw}x{sh} -> {dw}x{dh}");
            assert!(x + w <= sw, "crop overflows width for {sw}x{sh} -> {dw}x{dh}");
            assert!(y + h <= sh, "crop overflows height for {sw}x{sh} -> {dw}x{dh}");
        }
    }
}
