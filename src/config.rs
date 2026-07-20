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

//! Filesystem paths, configuration loading, and defaults.
//!
//! Paths follow the XDG Base Directory specification, falling back to the
//! conventional `~/.config`, `~/.cache`, etc. when the relevant env vars
//! are unset.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::{env, fs};
use tracing::info;

/// Resolved filesystem paths owned by the daemon itself.
///
/// Distinct from `Config` in that these are derived from the environment
/// (XDG dirs, HOME) rather than from user input.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir:     PathBuf,
    pub config_file:    PathBuf,
    pub location_cache: PathBuf,
    pub pid_file:       PathBuf,
}

impl Paths {
    pub fn resolve() -> Result<Self> {
        let config_dir  = xdg_dir("XDG_CONFIG_HOME", ".config")?;
        let cache_dir   = xdg_dir("XDG_CACHE_HOME",  ".cache")?;
        let runtime_dir = xdg_runtime_dir()?;
        Ok(Self {
            config_file:    config_dir.join("ornatus/config.toml"),
            location_cache: cache_dir.join("ornatus/location.json"),
            pid_file:       runtime_dir.join("ornatus.pid"),
            config_dir,
        })
    }
}

/// Resolve an XDG directory, falling back to `$HOME/<fallback>` if the
/// env var is unset or empty.
fn xdg_dir(var: &str, fallback: &str) -> Result<PathBuf> {
    if let Ok(value) = env::var(var)
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(fallback))
}

/// Resolve `$XDG_RUNTIME_DIR` strictly — no HOME fallback, since the runtime
/// dir is per-session and faking one risks leaving a stale PID file in `/tmp`.
fn xdg_runtime_dir() -> Result<PathBuf> {
    let value = env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .context("XDG_RUNTIME_DIR is unset or empty (required for the PID file)")?;
    Ok(PathBuf::from(value))
}

/// User-tunable configuration, loaded from `config.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Directory containing the 16 gradient frames (`00.jpg` … `15.jpg`).
    pub wallpaper_dir: PathBuf,

    /// Directory holding the `dark/` and `light/` theme bundles and the
    /// `current` marker file watched by other tools (Quickshell, Neovim).
    pub theme_dir: PathBuf,

    /// Source of geographic coordinates.
    pub location: LocationConfig,

    /// How often the daemon recomputes the current blend and redraws.
    pub refresh_interval_secs: u64,
}

/// Where the daemon gets its latitude and longitude.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum LocationConfig {
    /// Detect via IP geolocation, cached daily and refreshed on suspend/resume.
    Auto,
    /// Use these coordinates verbatim. Useful for VPN users or fixed setups.
    Fixed { lat: f64, lon: f64 },
}

impl Default for Config {
    fn default() -> Self {
        let home = env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/"));
        Self {
            wallpaper_dir:         home.join("Pictures/wallpapers/solar-gradients"),
            theme_dir:             home.join(".config/theme"),
            location:              LocationConfig::Auto,
            refresh_interval_secs: 60,
        }
    }
}

impl Config {
    /// Load the config file, creating it with defaults if it doesn't exist.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let text = fs::read_to_string(path)
                .with_context(|| format!("reading config file at {}", path.display()))?;
            toml::from_str(&text)
                .with_context(|| format!("parsing config file at {}", path.display()))
        } else {
            let config = Config::default();
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("creating config directory at {}", parent.display())
                })?;
            }
            let text = toml::to_string_pretty(&config)
                .context("serialising default config to TOML")?;
            fs::write(path, &text)
                .with_context(|| format!("writing default config to {}", path.display()))?;
            info!("wrote default config to {}", path.display());
            Ok(config)
        }
    }
}
