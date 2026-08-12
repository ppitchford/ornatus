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

//! Theme management: write the `current` marker file, swap per-app symlinks
//! pointing at the dark or light theme bundle, and signal each app to reload.
//!
//! Idempotent: if the marker and all symlinks already reflect the requested
//! theme, the operation is a no-op and no reload signals are sent.

use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use tracing::{debug, info, warn};

/// Light or dark theme variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Light,
    Dark,
}

impl Theme {
    pub fn name(self) -> &'static str {
        match self {
            Theme::Light => "light",
            Theme::Dark  => "dark",
        }
    }

    /// Map a daytime/nighttime signal to a theme variant.
    pub fn from_is_daytime(is_daytime: bool) -> Self {
        if is_daytime { Theme::Light } else { Theme::Dark }
    }
}

/// One symlink mapping: a filename inside the theme bundle, and its
/// destination relative to the user's config directory.
struct Mapping {
    source_name:   &'static str,
    dest_relative: &'static str,
}

/// Theme bundle file → per-app symlink destination. Hardcoded because these
/// reflect application config conventions, not user preferences.
const MAPPINGS: &[Mapping] = &[
    Mapping { source_name: "kitty.conf", dest_relative: "kitty/current-theme.conf" },
    Mapping { source_name: "fuzzel.ini", dest_relative: "fuzzel/fuzzel.ini" },
    Mapping { source_name: "mako.conf",  dest_relative: "mako/config" },
];

pub struct ThemeManager {
    theme_dir:  PathBuf,  // ~/.config/theme/
    config_dir: PathBuf,  // ~/.config/
}

impl ThemeManager {
    pub fn new(theme_dir: PathBuf, config_dir: PathBuf) -> Self {
        Self { theme_dir, config_dir }
    }

    /// Apply the theme, returning whether anything actually changed.
    pub fn apply(&self, theme: Theme) -> Result<bool> {
        let marker_changed = self.write_marker(theme)?;
        let links_changed  = self.update_symlinks(theme)?;
        let changed        = marker_changed || links_changed;

        if changed {
            self.signal_reloads(theme);
            info!(theme = theme.name(), "theme applied");
        } else {
            debug!(theme = theme.name(), "theme already current, no changes");
        }
        Ok(changed)
    }

    fn write_marker(&self, theme: Theme) -> Result<bool> {
        let marker      = self.theme_dir.join("current");
        let new_content = format!("{}\n", theme.name());

        if fs::read_to_string(&marker).ok().as_deref() == Some(new_content.as_str()) {
            return Ok(false);
        }

        if let Some(parent) = marker.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating theme dir {}", parent.display()))?;
        }
        fs::write(&marker, &new_content)
            .with_context(|| format!("writing theme marker {}", marker.display()))?;
        debug!(path = %marker.display(), theme = theme.name(), "wrote theme marker");
        Ok(true)
    }

    fn update_symlinks(&self, theme: Theme) -> Result<bool> {
        let bundle      = self.theme_dir.join(theme.name());
        let mut changed = false;

        for m in MAPPINGS {
            let src = bundle.join(m.source_name);
            let dst = self.config_dir.join(m.dest_relative);

            if !src.exists() {
                warn!(source = %src.display(), "skipping symlink — theme source missing");
                continue;
            }

            // Skip if the symlink already points at the right source
            if let Ok(current_target) = fs::read_link(&dst)
                && current_target == src
            {
                continue;
            }

            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating dir {}", parent.display()))?;
            }

            // Remove any existing file or symlink at the destination; ignore
            // "not found" since that's the common case on first apply.
            let _ = fs::remove_file(&dst);

            unix_fs::symlink(&src, &dst).with_context(|| {
                format!("creating symlink {} -> {}", dst.display(), src.display())
            })?;

            debug!(source = %src.display(), dest = %dst.display(), "updated symlink");
            changed = true;
        }
        Ok(changed)
    }

    fn signal_reloads(&self, theme: Theme) {
        // Best-effort: apps may not be running, errors are logged at debug.
        run_quiet("pkill",   &["-SIGUSR1", "kitty"]);
        run_quiet("makoctl", &["reload"]);
        // Chromium and Electron apps (Helium, Obsidian) follow the portal's
        // color-scheme rather than any config file. xdg-desktop-portal-gtk
        // reads it from here and broadcasts SettingChanged; they repaint live.
        run_quiet("gsettings", &[
            "set",
            "org.gnome.desktop.interface",
            "color-scheme",
            match theme {
                Theme::Dark  => "prefer-dark",
                Theme::Light => "prefer-light",
            },
        ]);
    }
}

fn run_quiet(program: &str, args: &[&str]) {
    match Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => {
            debug!(program, ?args, "reload signal sent");
        }
        Ok(status) => {
            debug!(program, ?args, code = ?status.code(), "reload returned non-zero");
        }
        Err(err) => {
            debug!(program, ?args, error = %err, "failed to spawn reload command");
        }
    }
}
