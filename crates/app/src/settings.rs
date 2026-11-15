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
//!
//! The automatic ring count ([`RingMode::Auto`], the default after
//! v0.1.0) is not a versioned migration but a rule for a missing
//! `auto_rings` field: builds before it (v0.1.0) stored only `max_depth`,
//! where 12 was the default and anything else the user's choice, so a file
//! without the field means Auto for 12 and that fixed count otherwise.
//! The same rule covers a file that a newer build wrote and v0.1.0 then
//! saved again (it drops the field it does not know): a fixed count other
//! than 12 survives the round trip, a fixed 12 comes back as Auto.

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

/// How many rings the chart draws around the focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingMode {
    /// As many as fit the chart with room for a line of label text
    /// (`chart::auto_rings`).
    Auto,
    /// Always this many (within [`MAX_DEPTH_RANGE`]).
    Fixed(u8),
}

/// The fixed ring count every settings file had before version 2.
const OLD_DEFAULT_RINGS: u8 = 12;

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
    /// The ring count follows the chart's size ([`RingMode::Auto`]); None
    /// when the file did not have the field (see the module docs). Stored
    /// next to `max_depth` rather than as one enum so builds that predate
    /// it still read `max_depth`. Use [`Settings::ring_mode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auto_rings: Option<bool>,
    /// Number of rings drawn around the focus when `auto_rings` is off;
    /// kept while it is on, for when it is switched off again.
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
            auto_rings: Some(true),
            max_depth: OLD_DEFAULT_RINGS,
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
        // A missing `auto_rings` field (any version) is settled once.
        self.auto_rings = Some(self.ring_mode() == RingMode::Auto);
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

    pub fn ring_mode(&self) -> RingMode {
        // A missing field: 12 was the old default, anything else a choice.
        if self
            .auto_rings
            .unwrap_or(self.max_depth == OLD_DEFAULT_RINGS)
        {
            RingMode::Auto
        } else {
            RingMode::Fixed(self.max_depth)
        }
    }

    pub fn set_ring_mode(&mut self, mode: RingMode) {
        match mode {
            RingMode::Auto => self.auto_rings = Some(true),
            RingMode::Fixed(n) => {
                self.auto_rings = Some(false);
                self.max_depth = n;
            }
        }
        self.sanitize();
    }

    /// Layout parameters; `auto_rings` is the ring count [`RingMode::Auto`]
    /// stands for at the moment (it depends on the chart's size).
    pub fn layout_params(&self, auto_rings: u8) -> Params {
        Params {
            mode: self.size_mode.into(),
            max_depth: match self.ring_mode() {
                RingMode::Auto => auto_rings,
                RingMode::Fixed(n) => n,
            },
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
        assert!(!s.layout_params(12).show_free);
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
    fn rings_follow_the_chart_by_default() {
        let s = Settings::default();
        assert_eq!(s.ring_mode(), RingMode::Auto);
        assert_eq!(s.layout_params(8).max_depth, 8);
        assert_eq!(s.layout_params(15).max_depth, 15);
        let mut s = s;
        s.set_ring_mode(RingMode::Fixed(10));
        assert_eq!(s.layout_params(8).max_depth, 10);
        // Out of range: clamped.
        s.set_ring_mode(RingMode::Fixed(99));
        assert_eq!(s.ring_mode(), RingMode::Fixed(*MAX_DEPTH_RANGE.end()));
        // Back to Auto keeps the fixed count for later.
        s.set_ring_mode(RingMode::Auto);
        assert_eq!((s.ring_mode(), s.max_depth), (RingMode::Auto, 24));
        let saved = serde_json::to_string(&s).unwrap();
        assert_eq!(load(&saved).ring_mode(), RingMode::Auto);
    }

    #[test]
    fn files_without_the_ring_mode_keep_a_chosen_count() {
        // v0.1.0 and older files with the old default: Auto.
        for json in [
            r#"{"version": 1, "max_depth": 12}"#,
            r#"{"max_depth": 12}"#,
            r#"{"version": 1}"#,
            "{}",
        ] {
            let s = load(json);
            assert_eq!(s.ring_mode(), RingMode::Auto, "{json}");
            assert_eq!(s.auto_rings, Some(true), "{json}");
            assert_eq!(s.version, VERSION);
        }
        // Any other count was chosen: it stays fixed.
        for n in [3u8, 9, 11, 13, 24] {
            let s = load(&format!(r#"{{"version": 1, "max_depth": {n}}}"#));
            assert_eq!(s.ring_mode(), RingMode::Fixed(n));
        }
        // A fixed 12 chosen in this build is kept.
        let mut s = load("{}");
        s.set_ring_mode(RingMode::Fixed(12));
        let saved = serde_json::to_string(&s).unwrap();
        assert_eq!(load(&saved).ring_mode(), RingMode::Fixed(12));
        // Fixed(14) saved here, then saved again by v0.1.0 (which drops the
        // field it does not know and keeps the version): still Fixed(14).
        let mut s = Settings::default();
        s.set_ring_mode(RingMode::Fixed(14));
        let mut v: serde_json::Value = serde_json::to_value(&s).unwrap();
        v.as_object_mut().unwrap().remove("auto_rings").unwrap();
        assert_eq!(load(&v.to_string()).ring_mode(), RingMode::Fixed(14));
        // A newer file's explicit choice wins over the rule.
        let s = load(r#"{"version": 9, "auto_rings": false, "max_depth": 12}"#);
        assert_eq!((s.ring_mode(), s.version), (RingMode::Fixed(12), 9));
        let s = load(r#"{"version": 9, "auto_rings": true, "max_depth": 7}"#);
        assert_eq!((s.ring_mode(), s.max_depth), (RingMode::Auto, 7));
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
