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

//! Sun math: today's sunrise, sunset, and solar noon, and whether a given
//! instant falls between the first two. Only the day/night boundary matters
//! now that the wallpaper is a static image — it is what flips the theme.

use anyhow::{anyhow, Result};
use chrono::{DateTime, NaiveDate, Utc};
use sunrise::{Coordinates, SolarDay, SolarEvent};

use crate::location::Coords;

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
