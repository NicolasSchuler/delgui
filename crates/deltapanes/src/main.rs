#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod fonts;
mod hotkey;
mod keys;
mod render;
mod settings;
mod theme;
mod ui;

use deltapanes_core::delta::Delta;

const USAGE: &str = "\
deltapanes — a paste-first GUI frontend for delta

  deltapanes [OPTIONS] [FILE]...

  --paste     load the clipboard into the first panel
  --combine   start a result panel, seeded from the first file
  --watch     re-diff when a file given here changes on disk
  --hotkey    register a system-wide hotkey that pastes into a fresh panel
  --help      this message

Rendering is done by the real delta binary, so your [delta] gitconfig applies.
";

const FLAGS: [&str; 6] = [
    "--paste",
    "--combine",
    "--watch",
    "--hotkey",
    "--help",
    "-h",
];

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return Ok(());
    }
    // Silently ignoring an unknown flag means `--wach` starts an app that does
    // not watch and never says why.
    if let Some(bad) = args
        .iter()
        .find(|a| a.starts_with('-') && !FLAGS.contains(&a.as_str()))
    {
        eprintln!("deltapanes: unknown option {bad}\n");
        eprint!("{USAGE}");
        std::process::exit(2);
    }
    let flag = |name: &str| args.iter().any(|a| a == name);
    let preload = flag("--paste").then(clipboard_text).flatten();
    // Positional paths mirror `delta A B`, so the app is a drop-in for the
    // shell invocation as well as a paste target.
    let files: Vec<std::path::PathBuf> = args
        .iter()
        .filter(|a| !a.starts_with('-'))
        .map(std::path::PathBuf::from)
        .collect();

    // delta is a hard dependency by design: if it is missing we say so rather
    // than quietly rendering a worse diff ourselves.
    let delta = match Delta::discover() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("deltapanes: {e}");
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("deltapanes can’t start")
                .set_description(e.to_string())
                .set_buttons(rfd::MessageButtons::Ok)
                .show();
            std::process::exit(1);
        }
    };

    // Opt-in: claiming a system-wide shortcut without being asked is rude, and
    // it fails loudly here rather than silently doing nothing.
    let hotkey = flag("--hotkey")
        .then(|| match hotkey::Hotkey::register() {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("deltapanes: could not register the global hotkey: {e}");
                None
            }
        })
        .flatten();

    let watch = flag("--watch");
    let combine = flag("--combine");
    eframe::run_native(
        "deltapanes",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1240.0, 860.0])
                .with_min_inner_size([720.0, 480.0])
                .with_app_id("deltapanes")
                .with_title("deltapanes"),
            ..Default::default()
        },
        Box::new(move |cc| {
            let settings = settings::Settings::load(cc.storage);
            // Fonts before the theme: the type scale names a font family that
            // only exists once the faces are registered.
            cc.egui_ctx.set_fonts(fonts::definitions(
                settings.ui_font.as_ref(),
                settings.ui_font_strong.as_ref(),
                settings.mono_font.as_ref(),
            ));
            theme::install(&cc.egui_ctx, settings.ui_pt, settings.mono_pt);
            cc.egui_ctx.set_theme(settings.theme.preference());
            Ok(Box::new(app::App::new(
                delta, settings, preload, &files, watch, combine, hotkey,
            )))
        }),
    )
}

pub fn clipboard_text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}
