//! User settings, persisted by eframe (`persistence` feature, app id
//! "RingScan": `%APPDATA%\RingScan\data\app.ron`).
//!
//! Everything is `#[serde(default)]`, so settings written by an older or
//! newer build load without errors: unknown fields are ignored and missing
//! ones get their defaults. Window geometry and panel widths are not here:
//! eframe persists the window and egui's memory (which holds panel sizes)
//! on its own.
//!
//! A default that changes for users who already have a settings file goes
//! through [`Settings::migrate`]: the file's `version` (0 when missing, so
//! files from before versioning) says which one-time changes are still due.
//! History:
//!
//! * 1: free space is hidden by default (the chart, the "Share" column and
//!   the tooltip percentages then use the used space; the center disc
//!   still says how much is free). Older files stored `show_free: true`,
//!   the old default, so it is switched off once; the toggle stays.

use ringscan_core::{Lang, SizeMode, layout::Params};
use serde::{Deserialize, Serialize};

/// Storage key for [`Settings`].
pub const KEY: &str = "ringscan.settings";

/// Current settings version (see the module docs).
pub const VERSION: u32 = 1;

/// `version` of a file written before the field existed.
fn version_zero() -> u32 {
    0
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LangPref {
    /// Follow the Windows display language.
    #[default]
    Auto,
    Uk,
    En,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemePref {
    /// Follow the Windows app theme.
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColorMode {
    /// Hue by the angular position of the top-level folder.
    #[default]
    Folders,
    /// Stable color per file type group.
    Types,
    /// Gradient by last modification time.
    Age,
}

/// Mirror of [`SizeMode`] (the core enum is not serde).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SizePref {
    #[default]
    Allocated,
    Logical,
}

impl From<SizePref> for SizeMode {
    fn from(p: SizePref) -> SizeMode {
        match p {
            SizePref::Allocated => SizeMode::Allocated,
            SizePref::Logical => SizeMode::Logical,
        }
    }
}

/// Sort column of the "Contents" list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortColumn {
    Name,
    #[default]
    Size,
    Files,
    Modified,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Settings version: missing (0) in old files, [`VERSION`] in new ones
    /// (struct-level `serde(default)` would give the current one).
    #[serde(default = "version_zero")]
    pub version: u32,
    pub language: LangPref,
    pub theme: ThemePref,
    pub size_mode: SizePref,
    pub color_mode: ColorMode,
    pub show_free: bool,
    /// Number of rings drawn around the focus.
    pub max_depth: u8,
    /// Smallest arc of its own, in degrees; smaller items are merged into a
    /// gray "N smaller items" arc.
    pub min_arc_deg: f32,
    pub left_panel: bool,
    pub right_panel: bool,
    /// 0 = contents, 1 = largest files, 2 = file types.
    pub right_tab: usize,
    pub sort: SortColumn,
    pub sort_ascending: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            version: VERSION,
            language: LangPref::Auto,
            theme: ThemePref::System,
            size_mode: SizePref::Allocated,
            color_mode: ColorMode::Folders,
            show_free: false,
            max_depth: 12,
            min_arc_deg: 0.3,
            left_panel: true,
            right_panel: true,
            right_tab: 0,
            sort: SortColumn::Size,
            sort_ascending: false,
        }
    }
}

pub const MAX_DEPTH_RANGE: std::ops::RangeInclusive<u8> = 3..=24;
pub const MIN_ARC_RANGE: std::ops::RangeInclusive<f32> = 0.01..=3.0;

impl Settings {
    /// One-time changes for settings loaded from an older file; call once
    /// after loading (see the module docs).
    pub fn migrate(&mut self) {
        if self.version < 1 {
            self.show_free = false;
        }
        // A newer build's file keeps its version (its migrations ran).
        self.version = self.version.max(VERSION);
    }

    /// Clamps values that may come from an edited or old settings file.
    pub fn sanitize(&mut self) {
        self.max_depth = self
            .max_depth
            .clamp(*MAX_DEPTH_RANGE.start(), *MAX_DEPTH_RANGE.end());
        if !self.min_arc_deg.is_finite() {
            self.min_arc_deg = 0.3;
        }
        self.min_arc_deg = self
            .min_arc_deg
            .clamp(*MIN_ARC_RANGE.start(), *MIN_ARC_RANGE.end());
        self.right_tab = self.right_tab.min(2);
    }

    pub fn layout_params(&self) -> Params {
        Params {
            mode: self.size_mode.into(),
            max_depth: self.max_depth,
            min_angle: self.min_arc_deg.to_radians(),
            show_free: self.show_free,
        }
    }

    /// The UI language: the explicit choice, or the OS language for Auto.
    pub fn lang(&self, os_ukrainian: bool) -> Lang {
        match self.language {
            LangPref::Uk => Lang::Uk,
            LangPref::En => Lang::En,
            LangPref::Auto if os_ukrainian => Lang::Uk,
            LangPref::Auto => Lang::En,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(json: &str) -> Settings {
        let mut s: Settings = serde_json::from_str(json).expect("settings");
        s.migrate();
        s
    }

    #[test]
    fn free_space_hidden_by_default() {
        let s = Settings::default();
        assert!(!s.show_free);
        assert_eq!(s.version, VERSION);
        assert!(!s.layout_params().show_free);
    }

    #[test]
    fn old_files_hide_free_space_once() {
        // Written before versioning, with the old default.
        let mut s = load(r#"{"show_free": true, "max_depth": 9}"#);
        assert!(!s.show_free);
        assert_eq!((s.version, s.max_depth), (VERSION, 9));
        // Switched back on and saved: stays on.
        s.show_free = true;
        let saved = serde_json::to_string(&s).unwrap();
        assert!(load(&saved).show_free);
        // An empty (or unreadable-field) file gets all defaults.
        let s = load("{}");
        assert!(!s.show_free);
        assert_eq!(s.version, VERSION);
    }

    #[test]
    fn current_files_keep_their_choice() {
        let s = load(&format!(r#"{{"version": {VERSION}, "show_free": true}}"#));
        assert!(s.show_free);
        let s = load(&format!(r#"{{"version": {VERSION}, "show_free": false}}"#));
        assert!(!s.show_free);
        // A newer build's file loads as is.
        let s = load(r#"{"version": 7, "show_free": true}"#);
        assert!(s.show_free);
        assert_eq!(s.version, 7);
    }
}
