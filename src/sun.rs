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

//! Sun math.
//!
//! Today's sunrise, sunset, and solar noon (for theme switching), plus a
//! continuous frame-position function that maps the current sun's altitude
//! and azimuth to a position in Apple's 16-frame Solar Gradients sequence.

use anyhow::{anyhow, Result};
use chrono::{DateTime, NaiveDate, Utc};
use sunrise::{Coordinates, SolarDay, SolarEvent};

use crate::location::Coords;

/// Solar Gradients per-frame metadata: `(altitude_deg, azimuth_deg)`.
///
/// Extracted from Apple's `Solar Gradients.heic` (the `apple_desktop:solar`
/// XMP block). Azimuth uses macOS convention: degrees from north, clockwise.
const SOLAR_GRADIENTS: [(f64, f64); 16] = [
    (-87.6, 260.6),  //  0: solar midnight
    (-17.1,  90.2),  //  1: astronomical dawn (E)
    (-11.1,  90.2),  //  2: nautical dawn (E)
    ( -5.4,  90.2),  //  3: civil dawn (E)
    (  0.2,  90.2),  //  4: sunrise
    (  6.0,  90.2),  //  5: early morning (E)
    ( 12.0,  90.2),  //  6: morning (E)
    ( 18.5,  90.2),  //  7: mid-morning (E)
    ( 88.4,  92.5),  //  8: solar noon
    ( 18.4, 270.0),  //  9: mid-afternoon (W)
    ( 12.1, 270.0),  // 10: afternoon (W)
    (  6.4, 270.0),  // 11: late afternoon (W)
    (  0.8, 270.0),  // 12: sunset
    ( -5.4, 270.0),  // 13: civil dusk (W)
    (-11.2, 270.0),  // 14: nautical dusk (W)
    (-17.7, 270.0),  // 15: astronomical dusk (W)
];

/// Sun events for a given date and location.
#[derive(Debug, Clone, Copy)]
pub struct SunDay {
    pub sunrise:    DateTime<Utc>,
    pub sunset:     DateTime<Utc>,
    pub solar_noon: DateTime<Utc>,
}

impl SunDay {
    pub fn compute(coords: Coords, date: NaiveDate) -> Result<Self> {
        let coord = Coordinates::new(coords.lat, coords.lon).ok_or_else(|| {
            anyhow!("invalid coordinates: lat={}, lon={}", coords.lat, coords.lon)
        })?;
        let day = SolarDay::new(coord, date);

        let sunrise = day.event_time(SolarEvent::Sunrise)
            .ok_or_else(|| anyhow!("no sunrise on {date} (polar day or night)"))?;
        let sunset = day.event_time(SolarEvent::Sunset)
            .ok_or_else(|| anyhow!("no sunset on {date} (polar day or night)"))?;
        let solar_noon = sunrise + (sunset - sunrise) / 2;

        Ok(Self { sunrise, sunset, solar_noon })
    }

    pub fn is_daytime(&self, now: DateTime<Utc>) -> bool {
        now >= self.sunrise && now < self.sunset
    }
}

/// Continuous frame position in `[0, 16)` for the current moment.
///
/// Algorithm:
///   1. Compute the sun's altitude and azimuth for the given coords and time.
///   2. Determine east/west hemisphere from the azimuth.
///   3. Walk the appropriate altitude sequence (morning: frames 0→8 ascending;
///      afternoon: frames 8→15→0 descending) and locate the two frames whose
///      stored altitudes bracket the current sun altitude.
///   4. Linearly interpolate by altitude fraction.
///
/// Frames cluster near the horizon (dawn and dusk, where the sky changes
/// rapidly) and frame 8 alone covers altitudes from ~18° to zenith. This
/// matches what Apple encoded into the HEIC and is what produces the
/// non-uniform pacing you observed.
pub fn frame_at(coords: Coords, now: DateTime<Utc>) -> f64 {
    let pos = sun::pos(now.timestamp_millis(), coords.lat, coords.lon);
    let alt = pos.altitude.to_degrees();

    // The `sun` crate adds π internally (see lib.rs line 74), so its azimuth
    // is already from north, clockwise, in radians [0, 2π) — macOS convention.
    let az = pos.azimuth.to_degrees().rem_euclid(360.0);

    if az < 180.0 {
        morning_position(alt)
    } else {
        afternoon_position(alt)
    }
}

/// Position within the morning sequence (frames 0 → 8, altitudes ascending).
fn morning_position(alt: f64) -> f64 {
    for i in 0..8 {
        let hi = SOLAR_GRADIENTS[i + 1].0;
        if alt < hi {
            let lo = SOLAR_GRADIENTS[i].0;
            let t = ((alt - lo) / (hi - lo)).clamp(0.0, 1.0);
            return i as f64 + t;
        }
    }
    8.0
}

/// Position within the afternoon sequence (frames 8 → 15 → 0, altitudes descending).
fn afternoon_position(alt: f64) -> f64 {
    // Sequence wraps from frame 15 back to frame 0 at the bottom.
    let alts: [f64; 9] = [
        SOLAR_GRADIENTS[8].0,
        SOLAR_GRADIENTS[9].0,
        SOLAR_GRADIENTS[10].0,
        SOLAR_GRADIENTS[11].0,
        SOLAR_GRADIENTS[12].0,
        SOLAR_GRADIENTS[13].0,
        SOLAR_GRADIENTS[14].0,
        SOLAR_GRADIENTS[15].0,
        SOLAR_GRADIENTS[0].0,
    ];
    for i in 0..8 {
        let hi = alts[i];
        let lo = alts[i + 1];
        if alt > lo {
            let t = ((hi - alt) / (hi - lo)).clamp(0.0, 1.0);
            return (8 + i) as f64 + t;
        }
    }
    16.0  // wraps to 0.0 in FrameSet::blend_into via rem_euclid
}
