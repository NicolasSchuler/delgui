#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod hotkey;
mod keys;
mod render;

use deltapanes_core::delta::Delta;

const USAGE: &str = "\
deltapanes — a paste-first GUI frontend for delta

  deltapanes [OPTIONS] [FILE]...

  --paste     load the clipboard into the first panel
  --watch     re-diff when a file given here changes on disk
  --hotkey    register a system-wide hotkey that pastes into a fresh panel
  --help      this message

Rendering is done by the real delta binary, so your [delta] gitconfig applies.
";

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return Ok(());
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
            std::process::exit(1);
        }
    };

    // Opt-in: claiming a system-wide shortcut without being asked is rude, and
    // it fails loudly here rather than silently doing nothing.
    let hotkey = flag("--hotkey").then(|| match hotkey::Hotkey::register() {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("deltapanes: could not register the global hotkey: {e}");
            None
        }
    }).flatten();

    let watch = flag("--watch");
    eframe::run_native(
        "deltapanes",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1200.0, 820.0])
                .with_title("deltapanes"),
            ..Default::default()
        },
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::ThemePreference::Dark);
            Ok(Box::new(app::App::new(delta, preload, &files, watch, hotkey)))
        }),
    )
}

pub fn clipboard_text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}
