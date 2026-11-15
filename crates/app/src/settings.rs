//! User settings, persisted by eframe (`persistence` feature, app id
//! "RingScan": `%APPDATA%\RingScan\data\app.ron`).
//!
//! Everything is `#[serde(default)]`, so settings written by an older or
//! newer build load without errors: unknown fields are ignored and missing
//! ones get their defaults. Window geometry and panel widths are not here:
//! eframe persists the window and egui's memory (which holds panel sizes)
//! on its own.

use ringscan_core::{Lang, SizeMode, layout::Params};
use serde::{Deserialize, Serialize};

/// Storage key for [`Settings`].
pub const KEY: &str = "ringscan.settings";

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
            language: LangPref::Auto,
            theme: ThemePref::System,
            size_mode: SizePref::Allocated,
            color_mode: ColorMode::Folders,
            show_free: true,
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
