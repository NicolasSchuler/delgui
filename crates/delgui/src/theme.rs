//! The app's own colours, spacing and type scale.
//!
//! Two things make this more than decoration.
//!
//! The first is that **delta's output is a fixed quantity**. Its default styling
//! reaches for exactly one terminal palette entry -- index 4, the blue that
//! draws every hunk rule and the side-by-side divider -- and everything else it
//! emits is truecolor. So the chrome accent is not a free choice: pick anything
//! but blue and the app permanently disagrees with the rules drawn inside its
//! own diff. Graphite therefore builds outwards from delta rather than inwards
//! from taste.
//!
//! The second is that **a light theme is not optional**. delta swaps its own
//! backgrounds between `#3f0001`/`#002800` and `#ffe0e0`/`#d0ffd0`, and those
//! are truecolor values we must reproduce byte-exactly. Chrome and diff have to
//! agree about which mode they are in, or one of them is wrong.
//!
//! Content is the *extreme* in both themes -- darkest in dark, pure white in
//! light -- and chrome steps towards mid-grey from there. delta's light
//! backgrounds are tinted whites and only read correctly against real white.

use egui::style::{Selection, WidgetVisuals, Widgets};
use egui::{Color32, CornerRadius, Margin, Stroke, Style, Theme, Visuals};

/// Named colour roles. Everything the app draws resolves through one of these;
/// no view is allowed its own hex value, which is how the old code ended up with
/// an error red and a drop-target blue that belonged to no palette.
#[derive(Clone, Copy)]
pub struct Tokens {
    pub surface_sunken: Color32,
    pub surface: Color32,
    pub surface_raised: Color32,
    pub surface_overlay: Color32,
    pub hover_fill: Color32,
    pub active_fill: Color32,
    pub border_subtle: Color32,
    pub border_strong: Color32,
    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub text_muted: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_solid: Color32,
    pub accent_quiet: Color32,
    pub on_accent: Color32,
    pub success: Color32,
    pub danger: Color32,
    pub danger_quiet: Color32,
    pub warning: Color32,
    /// The terminal's own sixteen colours, which delta uses for structural
    /// elements and a terminal takes from the user's scheme. We are not a
    /// terminal, so we supply them -- see [`crate::render::Palette`].
    pub ansi16: [Color32; 16],
    pub dark: bool,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

impl Tokens {
    pub const fn dark() -> Self {
        Self {
            surface_sunken: rgb(0x0e, 0x0f, 0x11),
            surface: rgb(0x13, 0x14, 0x17),
            surface_raised: rgb(0x19, 0x1b, 0x1f),
            surface_overlay: rgb(0x1e, 0x21, 0x26),
            hover_fill: rgb(0x26, 0x2a, 0x30),
            active_fill: rgb(0x2e, 0x33, 0x3a),
            border_subtle: rgb(0x24, 0x27, 0x2c),
            border_strong: rgb(0x33, 0x37, 0x3e),
            text_primary: rgb(0xe6, 0xe8, 0xeb),
            text_secondary: rgb(0x9a, 0xa1, 0xab),
            // Small metadata also appears in interactive labels. This is the
            // dimmest neutral that still clears 4.5:1 when their fill is active.
            text_muted: rgb(0x92, 0x9c, 0xa8),
            accent: rgb(0x4c, 0x8d, 0xf6),
            accent_hover: rgb(0x6f, 0xa8, 0xff),
            accent_solid: rgb(0x36, 0x70, 0xce),
            accent_quiet: rgb(0x1b, 0x27, 0x40),
            on_accent: rgb(0xff, 0xff, 0xff),
            success: rgb(0x6f, 0xbf, 0x77),
            danger: rgb(0xe0, 0x65, 0x5f),
            danger_quiet: rgb(0x2a, 0x14, 0x14),
            warning: rgb(0xd9, 0xa4, 0x41),
            // Index 4 is the chrome accent on purpose: it is the only palette
            // entry delta reaches for by default, so its hunk rules and column
            // divider come out the same blue as the app's own accent.
            ansi16: [
                rgb(0x12, 0x14, 0x1a), // black
                rgb(0xd2, 0x54, 0x4f), // red
                rgb(0x6f, 0xbf, 0x77), // green
                rgb(0xd9, 0xa4, 0x41), // yellow
                rgb(0x6f, 0xa8, 0xff), // blue -- delta's structure
                rgb(0xb9, 0x8a, 0xe0), // magenta
                rgb(0x54, 0xb5, 0xb5), // cyan
                rgb(0xc7, 0xcb, 0xd1), // white
                rgb(0x4b, 0x53, 0x5e),
                rgb(0xe0, 0x65, 0x5f),
                rgb(0x88, 0xd4, 0x8f),
                rgb(0xe8, 0xc0, 0x6a),
                rgb(0x8f, 0xbe, 0xff),
                rgb(0xcf, 0xa6, 0xee),
                rgb(0x74, 0xcb, 0xcb),
                rgb(0xf2, 0xf4, 0xf7),
            ],
            dark: true,
        }
    }

    pub const fn light() -> Self {
        Self {
            surface_sunken: rgb(0xff, 0xff, 0xff),
            surface: rgb(0xf7, 0xf8, 0xfa),
            surface_raised: rgb(0xff, 0xff, 0xff),
            surface_overlay: rgb(0xff, 0xff, 0xff),
            hover_fill: rgb(0xed, 0xf0, 0xf3),
            active_fill: rgb(0xe2, 0xe7, 0xed),
            border_subtle: rgb(0xe4, 0xe7, 0xeb),
            border_strong: rgb(0xc9, 0xcf, 0xd7),
            text_primary: rgb(0x16, 0x18, 0x1c),
            text_secondary: rgb(0x55, 0x60, 0x6d),
            // Small metadata also appears in interactive labels. Keep 4.5:1
            // even against the darker active fill, not just the white canvas.
            text_muted: rgb(0x5d, 0x67, 0x73),
            accent: rgb(0x25, 0x63, 0xc7),
            accent_hover: rgb(0x1e, 0x54, 0xac),
            accent_solid: rgb(0x25, 0x63, 0xc7),
            accent_quiet: rgb(0xe8, 0xef, 0xfc),
            on_accent: rgb(0xff, 0xff, 0xff),
            success: rgb(0x1f, 0x7a, 0x34),
            danger: rgb(0xc0, 0x39, 0x2b),
            danger_quiet: rgb(0xfd, 0xec, 0xea),
            warning: rgb(0x8a, 0x5a, 0x00),
            // A light terminal scheme inverts the roles: 0-7 are the darker
            // variants and 8-15 the lighter ones, so 7 and 15 are deliberately
            // not near-white -- delta uses them as foregrounds.
            ansi16: [
                rgb(0x1f, 0x23, 0x28),
                rgb(0xc0, 0x39, 0x2b),
                rgb(0x1f, 0x7a, 0x34),
                rgb(0x8a, 0x5a, 0x00),
                rgb(0x1f, 0x5f, 0xbf), // blue -- delta's structure
                rgb(0x82, 0x50, 0xa8),
                rgb(0x0f, 0x72, 0x85),
                rgb(0x5a, 0x62, 0x6c),
                rgb(0x8a, 0x92, 0x9d),
                rgb(0xe0, 0x55, 0x48),
                rgb(0x2e, 0x9c, 0x4a),
                rgb(0xa9, 0x74, 0x00),
                rgb(0x3a, 0x7f, 0xd8),
                rgb(0x9c, 0x68, 0xc0),
                rgb(0x15, 0x9a, 0xb0),
                rgb(0x2b, 0x31, 0x38),
            ],
            dark: false,
        }
    }

    pub fn of(theme: Theme) -> Self {
        match theme {
            Theme::Dark => Self::dark(),
            Theme::Light => Self::light(),
        }
    }
}

/// Radii, in one place because egui shares `WidgetVisuals::corner_radius` across
/// buttons, checkboxes, combo boxes and text fields -- anything that wants a
/// different one has to say so at the call site.
pub mod radius {
    use egui::CornerRadius;
    pub const CHIP: CornerRadius = CornerRadius::same(4);
    pub const CONTROL: CornerRadius = CornerRadius::same(6);
    pub const CARD: CornerRadius = CornerRadius::same(10);
}

/// Text roles beyond egui's five. Named styles because the alternative --
/// `RichText::size()` at every call site -- is how a type scale rots.
pub const STRONG: &str = "Strong";
pub const MICRO: &str = "Micro";
pub const CHORD: &str = "Chord";

fn widget(bg: Color32, stroke: Color32, fg: Color32) -> WidgetVisuals {
    WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: bg,
        bg_stroke: Stroke::new(1.0, stroke),
        // egui's default fattens this from 1.0 to 2.0 across the hover states,
        // so labels and checkmarks visibly thicken under the cursor. Pinned.
        fg_stroke: Stroke::new(1.0, fg),
        corner_radius: radius::CONTROL,
        // Expansion paints a widget outside its own rect, which nudges anything
        // column-aligned next to it. Not next to a diff.
        expansion: 0.0,
    }
}

pub fn visuals(t: &Tokens) -> Visuals {
    let shadow = |blur: u8, alpha: u8| egui::epaint::Shadow {
        offset: [0, (blur / 2) as i8],
        blur: blur * 2,
        spread: 0,
        color: Color32::from_black_alpha(alpha),
    };
    Visuals {
        dark_mode: t.dark,
        panel_fill: t.surface,
        window_fill: t.surface_overlay,
        extreme_bg_color: t.surface_sunken,
        text_edit_bg_color: Some(t.surface_sunken),
        faint_bg_color: t.surface_raised,
        code_bg_color: t.surface_raised,
        window_stroke: Stroke::new(1.0, t.border_strong),
        window_corner_radius: CornerRadius::same(12),
        menu_corner_radius: CornerRadius::same(8),
        hyperlink_color: t.accent,
        warn_fg_color: t.warning,
        error_fg_color: t.danger,
        // egui's "weak" text is the primary colour at 60% alpha, which on a dark
        // ground is a smudge. A real muted colour reads as deliberate.
        weak_text_alpha: 1.0,
        weak_text_color: Some(t.text_muted),
        selection: Selection {
            bg_fill: t.accent_quiet,
            stroke: Stroke::new(1.0, t.accent),
        },
        widgets: Widgets {
            noninteractive: widget(t.surface, t.border_subtle, t.text_secondary),
            inactive: widget(t.surface_overlay, t.border_subtle, t.text_primary),
            hovered: widget(t.hover_fill, t.border_strong, t.text_primary),
            active: widget(t.active_fill, t.accent, t.text_primary),
            open: widget(t.hover_fill, t.border_strong, t.text_primary),
        },
        window_shadow: shadow(8, if t.dark { 140 } else { 28 }),
        popup_shadow: shadow(4, if t.dark { 120 } else { 22 }),
        button_frame: true,
        collapsing_header_frame: false,
        indent_has_left_vline: false,
        striped: false,
        interact_cursor: Some(egui::CursorIcon::PointingHand),
        ..if t.dark {
            Visuals::dark()
        } else {
            Visuals::light()
        }
    }
}

pub fn spacing(s: &mut Style) {
    let sp = &mut s.spacing;
    sp.item_spacing = egui::vec2(8.0, 8.0);
    // egui defaults to one pixel of vertical padding, which is most of why
    // stock egui reads as a debug tool. `button_style` subtracts the border
    // width again, so this lands on a visual 10x6 and a ~28px control.
    sp.button_padding = egui::vec2(11.0, 7.0);
    sp.interact_size = egui::vec2(48.0, 28.0);
    sp.window_margin = Margin::same(20);
    sp.menu_margin = Margin::same(6);
    sp.menu_spacing = 4.0;
    sp.indent = 16.0;
    sp.icon_width = 16.0;
    sp.icon_width_inner = 10.0;
    sp.icon_spacing = 8.0;
    sp.combo_width = 200.0;
    sp.combo_height = 320.0;
    sp.text_edit_width = 200.0;
    sp.tooltip_width = 340.0;
    sp.indent_ends_with_horizontal_line = false;
    // A solid scrollbar takes width away from the diff, which changes the
    // column count the moment content overflows and re-runs delta for it.
    sp.scroll = egui::style::ScrollStyle {
        bar_width: 8.0,
        ..egui::style::ScrollStyle::thin()
    };
}

/// The type scale. `ui_pt` and `mono_pt` come from the user's settings; the rest
/// are derived so a size change moves the whole scale together.
pub fn type_scale(s: &mut Style, ui_pt: f32, mono_pt: f32) {
    use egui::FontFamily::{Monospace, Proportional};
    use egui::{FontId, TextStyle};
    s.text_styles = [
        (TextStyle::Heading, FontId::new(ui_pt + 2.0, Proportional)),
        (TextStyle::Body, FontId::new(ui_pt, Proportional)),
        // `TextStyle::Button` never actually reaches a `Button` in egui 0.36 --
        // `AtomLayout::fallback_font` overwrites it with `Body` -- but it does
        // reach ComboBox and CollapsingHeader. Keeping them equal makes the
        // outcome the same either way.
        (TextStyle::Button, FontId::new(ui_pt, Proportional)),
        // egui's default is 9pt, which reads as leftover debug output.
        (TextStyle::Small, FontId::new(ui_pt - 2.0, Proportional)),
        (TextStyle::Monospace, FontId::new(mono_pt, Monospace)),
        (
            TextStyle::Name(STRONG.into()),
            FontId::new(ui_pt, crate::fonts::strong_family()),
        ),
        (
            TextStyle::Name(MICRO.into()),
            FontId::new(ui_pt - 3.0, crate::fonts::strong_family()),
        ),
        (
            TextStyle::Name(CHORD.into()),
            FontId::new(ui_pt - 2.0, Monospace),
        ),
    ]
    .into();
    s.override_text_valign = Some(egui::Align::Center);
}

/// Install both themes at once, so a later switch to the other one does not
/// reveal an unstyled half.
pub fn install(ctx: &egui::Context, ui_pt: f32, mono_pt: f32) {
    ctx.set_visuals_of(Theme::Dark, visuals(&Tokens::dark()));
    ctx.set_visuals_of(Theme::Light, visuals(&Tokens::light()));
    ctx.all_styles_mut(|s| {
        spacing(s);
        type_scale(s, ui_pt, mono_pt);
        // This utility has no animation-driven meaning, so keep every modal,
        // popup and state transition immediate. Egui does not expose the
        // platform Reduce Motion preference on all supported backends; a zero
        // duration is the predictable accessible behavior everywhere.
        s.animation_time = 0.0;
    });
}

#[cfg(test)]
mod tests {
    use egui::Color32;

    use super::Tokens;

    fn luminance(color: Color32) -> f64 {
        let linear = |channel: u8| {
            let channel = f64::from(channel) / 255.0;
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
    }

    fn contrast(a: Color32, b: Color32) -> f64 {
        let (light, dark) = if luminance(a) >= luminance(b) {
            (luminance(a), luminance(b))
        } else {
            (luminance(b), luminance(a))
        };
        (light + 0.05) / (dark + 0.05)
    }

    #[test]
    fn muted_text_clears_normal_text_contrast_on_every_background() {
        for (name, tokens) in [("dark", Tokens::dark()), ("light", Tokens::light())] {
            for background in [
                tokens.surface_sunken,
                tokens.surface,
                tokens.surface_raised,
                tokens.surface_overlay,
                tokens.hover_fill,
                tokens.active_fill,
                tokens.accent_quiet,
                tokens.danger_quiet,
            ] {
                let ratio = contrast(tokens.text_muted, background);
                assert!(ratio >= 4.5, "{name} muted text contrast was {ratio:.3}:1");
            }
        }
    }
}
