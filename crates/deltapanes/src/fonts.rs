//! Choosing, loading and checking the fonts the app draws with.
//!
//! Three things forced this to be more than `FontDefinitions::default()`.
//!
//! **egui's bundled fonts cannot draw the app's own labels.** The proportional
//! family is `Ubuntu-Light` with `NotoEmoji` and an icon font behind it; between
//! them they have no `⏎`, no `⌫`, no `✕` and no `→`. Every one of those was on
//! screen as a tofu box, including in the primary button.
//!
//! **There is no bold.** Only `Ubuntu-Light` and `Hack-Regular` ship, and
//! `RichText::strong()` is a *colour* in egui, not a weight -- so a type scale
//! with any emphasis in it needs a font file from somewhere.
//!
//! **A diff must stay column-aligned.** `render::to_layout_job` pads lines to a
//! column count that delta also laid out against, so a proportional or
//! partially-covered font silently ruins the thing the app exists to show. Hence
//! [`probe`], which measures rather than trusts.
//!
//! egui 0.36 rasterises through `skrifa`/`harfrust`, not `ab_glyph`, so `.ttc`
//! collections and variable fonts both load -- which is what makes the system
//! font usable at all on macOS, where nearly everything ships as a collection.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::epaint::text::VariationCoords;
use egui::{FontData, FontDefinitions, FontFamily, FontId, FontTweak};
use serde::{Deserialize, Serialize};

/// Registered name of the heavier proportional face. It always exists as a
/// family, even when it resolves to the same bytes as the regular one: egui
/// panics if a `FontFamily::Name` has no entry.
const STRONG_FAMILY: &str = "strong";
const UI_REGULAR: &str = "ui-regular";
const UI_STRONG: &str = "ui-strong";
const MONO: &str = "mono";

/// The weight asked of a variable face for emphasis. Enough to read as heavier
/// at 13pt without turning into a headline.
const STRONG_WEIGHT: f32 = 560.0;

pub fn strong_family() -> FontFamily {
    FontFamily::Name(STRONG_FAMILY.into())
}

/// One loadable face: a file plus which face inside it. The index matters --
/// macOS ships Menlo as four faces in one `.ttc`, and Iosevka as several dozen.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Face {
    pub family: String,
    pub path: PathBuf,
    pub index: u32,
}

/// A family as offered in the picker: the face to draw with, and a heavier
/// sibling if the family ships one as a separate file.
#[derive(Clone, Debug)]
pub struct Family {
    pub regular: Face,
    pub strong: Option<Face>,
}

impl Family {
    pub fn name(&self) -> &str {
        &self.regular.family
    }
}

/// macOS's system font. A 7.9 MB variable file carrying `wght` from 1 to 1000,
/// the Apple keyboard glyphs (`⌘⇧⌫⏎`), and the shapes every other native app on
/// the machine is drawn with -- which is most of why the default look changes so
/// much. Loaded from the OS at runtime; nothing is redistributed.
#[cfg(target_os = "macos")]
const SYSTEM_UI_FONT: &str = "/System/Library/Fonts/SFNS.ttf";

/// The face used when the user has expressed no preference. `None` means
/// egui's bundled fonts, which is the honest answer everywhere but macOS: we
/// cannot know what a given Linux or Windows box has installed until the
/// catalogue scan finishes, and a font that changes one second after launch is
/// worse than one that never does.
pub fn default_ui_face() -> Option<Face> {
    #[cfg(target_os = "macos")]
    if Path::new(SYSTEM_UI_FONT).is_file() {
        return Some(Face {
            family: "System Font".into(),
            path: SYSTEM_UI_FONT.into(),
            index: 0,
        });
    }
    None
}

/// Every family installed on the machine, regular face first.
///
/// Costs a few hundred milliseconds on a warm cache and considerably more on a
/// cold one, so this belongs on a thread -- it exists to fill a picker, not to
/// hold up the first frame.
pub fn scan() -> Vec<Family> {
    use std::collections::BTreeMap;
    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    // Keyed by family name so several weights of the same family collapse into
    // one entry rather than flooding the list.
    let mut families: BTreeMap<String, Family> = BTreeMap::new();
    for face in db.faces() {
        let fontdb::Source::File(path) = &face.source else {
            continue; // in-memory faces have no path to remember in settings
        };
        let Some((name, _)) = face.families.first() else {
            continue;
        };
        // Names beginning with a dot are Apple's private system faces; the
        // usable ones are exposed under ordinary names as well.
        if name.starts_with('.') {
            continue;
        }
        // Never seed a family with an italic face. If it happens to be
        // enumerated before the normal cut, an equal-weight normal face would
        // otherwise tie in `better_regular` and never replace it.
        if face.style != fontdb::Style::Normal {
            continue;
        }
        let this = Face {
            family: name.clone(),
            path: path.clone(),
            index: face.index,
        };
        let weight = face.weight.0;
        let entry = families.entry(name.clone()).or_insert_with(|| Family {
            regular: this.clone(),
            strong: None,
        });
        // "Regular" is whatever sits closest to 400 from below; SF Mono ships at
        // 295, so an equality test would drop it.
        let current = &mut entry.regular;
        if weight <= 450 && better_regular(weight, face_weight(&db, current)) {
            *current = this.clone();
        }
        if (560..=760).contains(&weight) {
            entry.strong.get_or_insert(this);
        }
    }
    let mut out: Vec<Family> = families.into_values().collect();
    out.sort_by_key(|f| f.name().to_lowercase());
    out
}

fn face_weight(db: &fontdb::Database, face: &Face) -> u16 {
    db.faces()
        .find(|f| {
            matches!(&f.source, fontdb::Source::File(p) if p == &face.path) && f.index == face.index
        })
        .map_or(400, |f| f.weight.0)
}

fn better_regular(candidate: u16, current: u16) -> bool {
    (400i32 - candidate as i32).abs() < (400i32 - current as i32).abs()
}

/// Read a face and confirm the bytes parse.
///
/// The check is not paranoia: `FontsImpl::new` *panics* on data it cannot
/// parse, one pass after `set_fonts` was called, from inside eframe's event
/// loop. Without this, picking a bitmap-only or `.dfont` file from the list
/// would take the app down with a backtrace pointing at epaint.
pub fn load(face: &Face) -> Option<Vec<u8>> {
    let bytes = std::fs::read(&face.path).ok()?;
    read_fonts::FontRef::from_index(&bytes, face.index).ok()?;
    Some(bytes)
}

fn font(bytes: Vec<u8>, index: u32, weight: Option<f32>) -> Arc<FontData> {
    Arc::new(FontData {
        font: bytes.into(),
        index,
        tweak: FontTweak {
            coords: match weight {
                // Ignored by a face with no `wght` axis, which is the correct
                // degradation: emphasis then falls back to the regular cut.
                Some(w) => VariationCoords::new([(b"wght", w)]),
                None => VariationCoords::default(),
            },
            ..FontTweak::default()
        },
    })
}

/// Build the font set: the user's choices in front, egui's bundled fonts behind
/// them as a fallback chain.
///
/// `Hack` is deliberately added to the *proportional* chain too. egui does not
/// put it there, which is why arrows and box-drawing characters in ordinary
/// labels came out as tofu.
pub fn definitions(
    ui: Option<&Face>,
    ui_strong: Option<&Face>,
    mono: Option<&Face>,
) -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    let bundled_proportional = defs.families[&FontFamily::Proportional].clone();
    let mut proportional = Vec::new();
    let mut strong = Vec::new();

    if let Some(face) = ui
        && let Some(bytes) = load(face)
    {
        defs.font_data
            .insert(UI_REGULAR.into(), font(bytes.clone(), face.index, None));
        proportional.push(UI_REGULAR.to_owned());
        // A separate bold file wins over a variation axis, because a real
        // designed cut beats an interpolated one.
        match ui_strong.and_then(|s| load(s).map(|b| (b, s.index))) {
            Some((heavy, index)) => {
                defs.font_data
                    .insert(UI_STRONG.into(), font(heavy, index, None));
            }
            None => {
                defs.font_data.insert(
                    UI_STRONG.into(),
                    font(bytes, face.index, Some(STRONG_WEIGHT)),
                );
            }
        }
        strong.push(UI_STRONG.to_owned());
    }
    if let Some(face) = mono
        && let Some(bytes) = load(face)
    {
        defs.font_data
            .insert(MONO.into(), font(bytes, face.index, None));
        defs.families.insert(
            FontFamily::Monospace,
            std::iter::once(MONO.to_owned())
                .chain(defs.families[&FontFamily::Monospace].clone())
                .collect(),
        );
    }

    proportional.push("Hack".to_owned());
    proportional.extend(bundled_proportional);
    strong.extend(proportional.clone());
    defs.families.insert(FontFamily::Proportional, proportional);
    defs.families
        .insert(FontFamily::Name(STRONG_FAMILY.into()), strong);
    defs
}

/// What a chosen font is actually capable of, measured rather than assumed.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Probe {
    pub space: f32,
    /// Every glyph the diff leans on advances by the same width.
    pub fixed_pitch: bool,
    /// Width of `中` relative to a space. delta pads CJK to two columns, so
    /// anything but exactly 2.0 drifts. `None` means the character is missing
    /// altogether, which drifts too -- egui substitutes a one-column box.
    pub cjk_ratio: Option<f32>,
}

impl Probe {
    pub fn cjk_aligns(&self) -> bool {
        self.cjk_ratio.is_some_and(|r| (r - 2.0).abs() < 0.01)
    }
}

/// Measure the monospace family as installed.
///
/// Deliberately does not use `Fonts::has_glyph`: it compares against the
/// replacement face and so reports *every* glyph as missing whenever the chosen
/// font also supplies `◻`, which the common ones do. A width of zero is the
/// reliable signal.
pub fn probe(ctx: &egui::Context, font: &FontId) -> Probe {
    ctx.fonts_mut(|f| {
        let space = f.glyph_width(font, ' ');
        let fixed_pitch = space > 0.0
            && "iMW0#l|_@│"
                .chars()
                .all(|c| (f.glyph_width(font, c) - space).abs() < 0.01);
        let cjk = f.glyph_width(font, '中');
        Probe {
            space,
            fixed_pitch,
            cjk_ratio: (cjk > 0.0 && space > 0.0).then(|| cjk / space),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// egui panics -- inside eframe's event loop, one pass after the call -- if a
    /// family names a font that is not in `font_data`, or if a `FontFamily::Name`
    /// has no entry at all. Both are easy to reach by editing `definitions`.
    #[test]
    fn every_family_names_only_fonts_that_exist() {
        let sets = [
            definitions(None, None, None),
            definitions(default_ui_face().as_ref(), None, None),
        ];
        for defs in sets {
            let strong = FontFamily::Name(STRONG_FAMILY.into());
            for family in [
                FontFamily::Proportional,
                FontFamily::Monospace,
                strong.clone(),
            ] {
                let names = defs.families.get(&family).unwrap_or_else(|| {
                    panic!("family {family:?} has no entry, which egui treats as fatal")
                });
                assert!(!names.is_empty(), "family {family:?} is empty");
                for name in names {
                    assert!(
                        defs.font_data.contains_key(name),
                        "family {family:?} names {name:?}, which is not registered"
                    );
                }
            }
        }
    }

    /// The default face is loaded before the first frame, so a path that does not
    /// parse would take the app down at launch rather than at a picker click.
    #[test]
    fn the_default_interface_face_loads_if_there_is_one() {
        if let Some(face) = default_ui_face() {
            assert!(
                load(&face).is_some(),
                "{} did not parse",
                face.path.display()
            );
        }
    }

    /// Nothing here may panic on a machine's real font collection, and a face we
    /// offer in the picker has to be one we can actually install.
    #[test]
    fn scanned_families_are_loadable_and_use_normal_faces() {
        let found = scan();
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        for family in &found {
            let face = db
                .faces()
                .find(|face| {
                    matches!(&face.source, fontdb::Source::File(path) if path == &family.regular.path)
                        && face.index == family.regular.index
                })
                .unwrap_or_else(|| panic!("could not find the selected face for {}", family.name()));
            assert_eq!(
                face.style,
                fontdb::Style::Normal,
                "offered an italic or oblique face as the regular face for {}",
                family.name()
            );
        }
        for family in found.iter().take(40) {
            assert!(
                load(&family.regular).is_some(),
                "offered {} but could not load {}",
                family.name(),
                family.regular.path.display()
            );
        }
    }
}
