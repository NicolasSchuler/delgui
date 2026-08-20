#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
/// Compiled only for tests: it exists to generate `docs/reference.md` and to
/// fail when the code it documents moves out from under it.
#[cfg(test)]
mod docs;
mod fonts;
mod hotkey;
mod keys;
mod render;
mod settings;
mod theme;
mod ui;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use delgui_core::delta::{self, Delta, DeltaError};

const USAGE: &str = "\
delgui — a paste-first GUI frontend for delta

  delgui [OPTIONS] [FILE]...

  --paste       load the clipboard into the first panel
  --combine     start a result panel, seeded from the first file
  --watch       re-diff when a file given here changes on disk
  --hotkey      register a system-wide hotkey that pastes into a fresh panel
  --mergetool   resolve a conflict for git: BASE LOCAL REMOTE MERGED
  --help        this message

As git's diff and merge tool:

  git config --global diff.tool delgui
  git config --global difftool.delgui.cmd 'delgui \"$LOCAL\" \"$REMOTE\"'
  git config --global merge.tool delgui
  git config --global mergetool.delgui.cmd \
      'delgui --mergetool \"$BASE\" \"$LOCAL\" \"$REMOTE\" \"$MERGED\"'
  git config --global mergetool.delgui.trustExitCode true

Rendering is done by the real delta binary, so your [delta] gitconfig applies.
";

const FLAGS: [&str; 7] = [
    "--paste",
    "--combine",
    "--watch",
    "--hotkey",
    "--mergetool",
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
        eprintln!("delgui: unknown option {bad}\n");
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
    // than quietly rendering a worse diff ourselves. Git is one too, and for the
    // same reason -- every render runs `git diff --no-index` before delta sees
    // anything -- so it is asked for here rather than discovered by a diff that
    // fails on every keystroke.
    let delta = Delta::discover().unwrap_or_else(cannot_start);
    delta::require_git().unwrap_or_else(cannot_start);

    // Opt-in: claiming a system-wide shortcut without being asked is rude, and
    // it fails loudly here rather than silently doing nothing.
    let hotkey = flag("--hotkey")
        .then(|| match hotkey::Hotkey::register() {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("delgui: could not register the global hotkey: {e}");
                None
            }
        })
        .flatten();

    let watch = flag("--watch");
    let combine = flag("--combine");
    // Git's four, in git's order. Refused rather than guessed at: a merge tool
    // that took the wrong file for the ancestor would produce a plausible merge
    // of the wrong two things.
    let mergetool = flag("--mergetool").then(|| {
        let [base, local, remote, merged] = files.as_slice() else {
            eprintln!(
                "delgui: --mergetool needs exactly four files — BASE LOCAL REMOTE MERGED, \
                 which is what git passes.\n"
            );
            eprint!("{USAGE}");
            std::process::exit(2);
        };
        (
            [base.clone(), local.clone(), remote.clone()],
            merged.clone(),
        )
    });
    let resolved = Arc::new(AtomicBool::new(false));
    // Only the three inputs become panels; the fourth is where the answer goes.
    let files = match &mergetool {
        Some((inputs, _)) => inputs.to_vec(),
        None => files,
    };
    let mergetool = mergetool.map(|(_, merged)| app::MergeTool {
        merged,
        resolved: Arc::clone(&resolved),
    });
    let in_mergetool = mergetool.is_some();
    let outcome = eframe::run_native(
        "delgui",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1240.0, 860.0])
                .with_min_inner_size([720.0, 480.0])
                .with_app_id("delgui")
                .with_title("delgui"),
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
                delta,
                settings,
                app::Launch {
                    preload,
                    files,
                    watch,
                    combine,
                    mergetool,
                    hotkey,
                },
            )))
        }),
    );
    // Git stages the file it handed over only on a zero exit, and only with
    // `trustExitCode`. Closing the window without writing the merge is a real
    // answer -- "not resolved" -- and has to be reported as one.
    if in_mergetool && !resolved.load(Ordering::Relaxed) {
        outcome?;
        std::process::exit(1);
    }
    outcome
}

/// Say why, in both places someone might be looking: the terminal this may have
/// been launched from, and the screen of someone who double-clicked it.
fn cannot_start<T>(error: DeltaError) -> T {
    eprintln!("delgui: {error}");
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("delgui can’t start")
        .set_description(error.to_string())
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
    std::process::exit(1);
}

pub fn clipboard_text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}
