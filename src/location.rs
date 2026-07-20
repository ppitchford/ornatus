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

//! Geographic location resolution.
//!
//! For `LocationConfig::Fixed`, returns configured coords verbatim. For
//! `LocationConfig::Auto`, consults a JSON cache (refreshed daily) and falls
//! through to an IP geolocation request when stale. On fetch failure, falls
//! back to the stale cache if available.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::config::LocationConfig;

/// A geographic position in decimal degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coords {
    pub lat: f64,
    pub lon: f64,
}

/// On-disk cache entry, persisted as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedLocation {
    fetched_at: DateTime<Utc>,
    lat:        f64,
    lon:        f64,
}

/// Resolves coordinates and manages the on-disk cache.
pub struct LocationResolver {
    source:     LocationConfig,
    cache_path: PathBuf,
    ttl:        ChronoDuration,
}

impl LocationResolver {
    pub fn new(source: LocationConfig, cache_path: PathBuf) -> Self {
        Self {
            source,
            cache_path,
            ttl: ChronoDuration::hours(24),
        }
    }

    /// Resolve the current coordinates, preferring a fresh cache.
    pub fn resolve(&self) -> Result<Coords> {
        match self.source {
            LocationConfig::Fixed { lat, lon } => {
                debug!(lat, lon, "using fixed coordinates");
                Ok(Coords { lat, lon })
            }
            LocationConfig::Auto => self.resolve_auto(),
        }
    }

    /// Force a fresh resolution, bypassing the cache freshness check. Used
    /// after wake-from-suspend, when the user may have moved. Falls back to
    /// the stale cache on fetch failure — the user may still be offline.
    pub fn resolve_force(&self) -> Result<Coords> {
        match self.source {
            LocationConfig::Fixed { lat, lon } => {
                debug!(lat, lon, "using fixed coordinates");
                Ok(Coords { lat, lon })
            }
            LocationConfig::Auto => match self.fetch_and_cache() {
                Ok(coords) => Ok(coords),
                Err(err) => match self.read_cache() {
                    Some(c) => {
                        warn!(
                            error = %err,
                            "forced fetch failed, falling back to stale cache",
                        );
                        Ok(Coords { lat: c.lat, lon: c.lon })
                    }
                    None => Err(err),
                },
            },
        }
    }

    fn resolve_auto(&self) -> Result<Coords> {
        let cached = self.read_cache();

        // Use cached value if it's still fresh
        if let Some(ref c) = cached {
            let age = Utc::now() - c.fetched_at;
            if age < self.ttl {
                debug!(
                    lat = c.lat,
                    lon = c.lon,
                    age_secs = age.num_seconds(),
                    "using cached location"
                );
                return Ok(Coords { lat: c.lat, lon: c.lon });
            }
        }

        // Fall through to a fresh fetch, falling back to stale cache on failure
        match self.fetch_and_cache() {
            Ok(coords) => Ok(coords),
            Err(err) => match cached {
                Some(c) => {
                    warn!(error = %err, "fetch failed, falling back to stale cache");
                    Ok(Coords { lat: c.lat, lon: c.lon })
                }
                None => Err(err),
            },
        }
    }

    fn read_cache(&self) -> Option<CachedLocation> {
        let text = fs::read_to_string(&self.cache_path).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn fetch_and_cache(&self) -> Result<Coords> {
        let coords = fetch_geo_ip().context("IP geolocation request failed")?;

        let cached = CachedLocation {
            fetched_at: Utc::now(),
            lat:        coords.lat,
            lon:        coords.lon,
        };

        if let Some(parent) = self.cache_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("creating cache directory at {}", parent.display())
            })?;
        }

        let text = serde_json::to_string_pretty(&cached)
            .context("serialising cached location")?;
        fs::write(&self.cache_path, &text).with_context(|| {
            format!("writing location cache to {}", self.cache_path.display())
        })?;

        info!(lat = coords.lat, lon = coords.lon, "fetched and cached new location");
        Ok(coords)
    }
}

/// Subset of ipapi.co's JSON response that we care about.
#[derive(Debug, Deserialize)]
struct IpApiResponse {
    latitude:  f64,
    longitude: f64,
}

fn fetch_geo_ip() -> Result<Coords> {
    let response: IpApiResponse = ureq::get("https://ipapi.co/json/")
        .timeout(Duration::from_secs(5))
        .call()
        .context("HTTP request to ipapi.co failed")?
        .into_json()
        .context("ipapi.co returned malformed JSON")?;

    Ok(Coords {
        lat: response.latitude,
        lon: response.longitude,
    })
}
