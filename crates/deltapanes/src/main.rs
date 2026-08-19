#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod render;

use deltapanes_core::delta::Delta;

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let preload = args.iter().any(|a| a == "--paste").then(clipboard_text).flatten();
    let watch = args.iter().any(|a| a == "--watch");
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
            Ok(Box::new(app::App::new(delta, preload, &files, watch)))
        }),
    )
}

fn clipboard_text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}
