//! What survives a restart.
//!
//! Panel *contents* deliberately do not. The app exists partly so that text you
//! would not paste into a web diff can still be diffed, and writing that text to
//! `~/Library/Application Support` on a 30-second autosave timer would quietly
//! undo the whole point. Only preferences are stored.

use deltapanes_core::delta::Whitespace;
use serde::{Deserialize, Serialize};

use crate::fonts::Face;

/// serde cannot derive impls for a type from another crate, and `deltapanes-core`
/// has no business depending on serde: how the GUI persists a preference is not
/// a fact about invoking delta. This is serde's own answer -- a local definition
/// it generates the impls from -- and it keeps one enum in the API.
#[derive(Serialize, Deserialize)]
#[serde(remote = "Whitespace")]
enum WhitespaceDef {
    Exact,
    Amount,
    All,
}

/// How much unchanged text each difference is shown with.
///
/// `Whole` is what "show me the file, with the changes marked" means; it reaches
/// git as a context width large enough to cover any file the app will open, and
/// `MAX_PANEL_BYTES` is four megabytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Context {
    /// Just the changed lines. What merge mode uses, for the same reason.
    Tight,
    #[default]
    Normal,
    Whole,
}

impl Context {
    pub const ALL: [Self; 3] = [Self::Tight, Self::Normal, Self::Whole];

    pub fn label(self) -> &'static str {
        match self {
            Self::Tight => "Changes only",
            Self::Normal => "Some context",
            Self::Whole => "Whole file",
        }
    }

    pub fn lines(self) -> u32 {
        match self {
            Self::Tight => 0,
            Self::Normal => 3,
            Self::Whole => 1_000_000,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum ThemeChoice {
    /// Follow the OS. egui learns this from winit at startup and updates live
    /// when the system appearance changes.
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub fn preference(self) -> egui::ThemePreference {
        match self {
            Self::System => egui::ThemePreference::System,
            Self::Light => egui::ThemePreference::Light,
            Self::Dark => egui::ThemePreference::Dark,
        }
    }

    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }
}

/// Where the result being built sits relative to the diff.
///
/// Bottom is what kdiff3 and VS Code's merge editor do, and it costs the diff no
/// width -- which matters, since delta lays out against a column count and a
/// side-by-side diff is the widest thing in the window. On a wide screen that
/// trade is the wrong way round: horizontal space is what there is most of, and
/// a full-height column shows far more of the result at once. Hence a choice
/// rather than a default.
///
/// `Left` puts it under the same half of the diff its own text appears in -- the
/// result is the left column of every comparison while it is being built -- so a
/// take lands twice within one eye movement.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum ResultPlacement {
    #[default]
    Bottom,
    Left,
    Right,
}

impl ResultPlacement {
    pub const ALL: [Self; 3] = [Self::Bottom, Self::Left, Self::Right];

    pub fn label(self) -> &'static str {
        match self {
            Self::Bottom => "Bottom",
            Self::Left => "Left",
            Self::Right => "Right",
        }
    }
}

/// Sizes are clamped on the way in as well as in the UI: a hand-edited
/// `app.ron` should not be able to produce a window with 2pt text and no way
/// back to the control that fixes it.
pub const UI_PT: std::ops::RangeInclusive<f32> = 10.0..=20.0;
pub const MONO_PT: std::ops::RangeInclusive<f32> = 9.0..=24.0;
const DEFAULT_UI_PT: f32 = 13.0;
const DEFAULT_MONO_PT: f32 = 12.5;

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    pub ui_font: Option<Face>,
    /// A separately-designed heavier cut, when the family ships one. Absent for
    /// variable fonts, where the weight comes from the `wght` axis instead.
    pub ui_font_strong: Option<Face>,
    pub mono_font: Option<Face>,
    pub ui_pt: f32,
    pub mono_pt: f32,

    // delta rendering options worth remembering. Re-picking a syntax theme on
    // every launch is the opposite of "it respects your configuration".
    pub side_by_side: bool,
    pub line_numbers: bool,
    pub wrap: bool,
    pub hunk_headers: bool,
    pub context: Context,
    #[serde(with = "WhitespaceDef")]
    pub whitespace: Whitespace,
    pub ignore_blank_lines: bool,
    pub ignore_cr_at_eol: bool,
    /// Empty rather than absent when unset: a text field the user cleared and a
    /// field they never touched are the same state, and `Option<String>` invites
    /// storing `Some("")`, which git reads as "every line matches".
    pub ignore_matching: String,
    pub syntax_theme: Option<String>,
    pub inherit_gitconfig: bool,
    /// Explicit feature selection. `None` is a pre-feature-settings migration
    /// state that seeds from the active gitconfig once; `Some([])` remembers
    /// that the user deliberately disabled every feature.
    pub features: Option<Vec<String>>,
    /// Whether the settings drawer was open. An inspector you left open should
    /// still be open next time.
    pub settings_open: bool,
    pub result_placement: ResultPlacement,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::default(),
            ui_font: crate::fonts::default_ui_face(),
            ui_font_strong: None,
            mono_font: None,
            ui_pt: DEFAULT_UI_PT,
            mono_pt: DEFAULT_MONO_PT,
            side_by_side: true,
            line_numbers: true,
            wrap: true,
            // Off by default for the same reason `--file-style=omit` is passed:
            // there is exactly one "file" in a two-buffer comparison and the app
            // draws its own headers, so delta's boxed `1:` above the diff is a
            // decoration for a fact already on screen. Available in Settings for
            // long multi-hunk diffs, where it earns its place.
            hunk_headers: false,
            context: Context::default(),
            whitespace: Whitespace::Exact,
            ignore_blank_lines: false,
            ignore_cr_at_eol: false,
            ignore_matching: String::new(),
            syntax_theme: None,
            inherit_gitconfig: true,
            features: None,
            settings_open: false,
            result_placement: ResultPlacement::default(),
        }
    }
}

impl Settings {
    /// `#[serde(default)]` fills in fields a newer build added, but it cannot
    /// repair a value that is merely absurd.
    pub fn sanitised(mut self) -> Self {
        // `f32::clamp` leaves NaN as NaN. Egui then receives a non-finite font
        // size and can poison layout coordinates, so repair non-finite values
        // to the documented defaults before applying the user-facing range.
        if !self.ui_pt.is_finite() {
            self.ui_pt = DEFAULT_UI_PT;
        }
        if !self.mono_pt.is_finite() {
            self.mono_pt = DEFAULT_MONO_PT;
        }
        self.ui_pt = self.ui_pt.clamp(*UI_PT.start(), *UI_PT.end());
        self.mono_pt = self.mono_pt.clamp(*MONO_PT.start(), *MONO_PT.end());
        // A font can be uninstalled, or the settings file carried to another
        // machine. Dropping the choice is better than drawing nothing.
        if self
            .ui_font
            .as_ref()
            .is_some_and(|f| crate::fonts::load(f).is_none())
        {
            self.ui_font = crate::fonts::default_ui_face();
            self.ui_font_strong = None;
        }
        if self
            .mono_font
            .as_ref()
            .is_some_and(|f| crate::fonts::load(f).is_none())
        {
            self.mono_font = None;
        }
        self
    }

    /// Drop a persisted syntax theme that the current delta no longer offers.
    ///
    /// The catalog comes from `Delta::syntax_themes`, which is only available
    /// after settings have been loaded. Call this once at app construction,
    /// before copying `syntax_theme` into delta's render options. The return
    /// value says whether the persisted selection changed.
    pub fn sanitise_syntax_theme(&mut self, available: &[(String, bool)]) -> bool {
        let valid = self
            .syntax_theme
            .as_ref()
            .is_none_or(|selected| available.iter().any(|(name, _)| name == selected));
        if valid {
            return false;
        }
        self.syntax_theme = None;
        true
    }

    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        storage
            .and_then(|s| eframe::get_value::<Self>(s, eframe::APP_KEY))
            .unwrap_or_default()
            .sanitised()
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_MONO_PT, DEFAULT_UI_PT, MONO_PT, Settings, UI_PT};

    #[test]
    fn non_finite_sizes_return_to_defaults_before_clamping() {
        let settings = Settings {
            ui_pt: f32::NAN,
            mono_pt: f32::INFINITY,
            ..Settings::default()
        }
        .sanitised();

        assert_eq!(settings.ui_pt, DEFAULT_UI_PT);
        assert_eq!(settings.mono_pt, DEFAULT_MONO_PT);

        let settings = Settings {
            ui_pt: f32::NEG_INFINITY,
            mono_pt: f32::NAN,
            ..Settings::default()
        }
        .sanitised();
        assert_eq!(settings.ui_pt, DEFAULT_UI_PT);
        assert_eq!(settings.mono_pt, DEFAULT_MONO_PT);
    }

    #[test]
    fn finite_sizes_are_still_clamped_to_the_supported_range() {
        let settings = Settings {
            ui_pt: *UI_PT.start() - 1.0,
            mono_pt: *MONO_PT.end() + 1.0,
            ..Settings::default()
        }
        .sanitised();

        assert_eq!(settings.ui_pt, *UI_PT.start());
        assert_eq!(settings.mono_pt, *MONO_PT.end());
    }

    #[test]
    fn stale_syntax_themes_are_cleared_after_catalog_discovery() {
        let available = vec![("GitHub".to_string(), false), ("Dracula".to_string(), true)];
        let mut settings = Settings {
            syntax_theme: Some("Removed theme".to_string()),
            ..Settings::default()
        };

        assert!(settings.sanitise_syntax_theme(&available));
        assert_eq!(settings.syntax_theme, None);
        assert!(!settings.sanitise_syntax_theme(&available));
    }

    #[test]
    fn installed_syntax_themes_and_default_selection_are_preserved() {
        let available = vec![("GitHub".to_string(), false)];
        let mut selected = Settings {
            syntax_theme: Some("GitHub".to_string()),
            ..Settings::default()
        };
        let mut default = Settings::default();

        assert!(!selected.sanitise_syntax_theme(&available));
        assert_eq!(selected.syntax_theme.as_deref(), Some("GitHub"));
        assert!(!default.sanitise_syntax_theme(&available));
        assert_eq!(default.syntax_theme, None);
    }
}
