//! End-to-end check of the watch loop: a file changes on disk, the watcher
//! reports it, and re-rendering through delta reflects the new contents.
//!
//! This covers everything the GUI does on a watch event except drawing.

use std::time::{Duration, Instant};

use deltapanes_core::ansi;
use deltapanes_core::delta::{Delta, Input, Options};
use deltapanes_core::watch::FileWatcher;

fn visible(bytes: &[u8]) -> String {
    ansi::parse(bytes)
        .iter()
        .map(|l| l.plain_text())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_saved_file_changes_what_delta_renders() {
    let d = Delta::discover().expect("these tests require `delta` on PATH");
    let dir = std::env::temp_dir().join(format!("dp-rerender-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let left = dir.join("a.rs");
    let right = dir.join("b.rs");
    std::fs::write(&left, "fn main() {\n    let x = 1;\n}\n").unwrap();
    std::fs::write(&right, "fn main() {\n    let x = 2;\n}\n").unwrap();

    let opts = Options {
        inherit_gitconfig: false,
        default_language: Some("rs".into()),
        ..Options::default()
    };
    let render = |d: &Delta| {
        d.render(&Input::Path(left.clone()), &Input::Path(right.clone()), &opts)
            .expect("render")
    };

    let before = visible(&render(&d));
    assert!(before.contains("let x = 2;"));

    let mut watcher = FileWatcher::new(|| {}).unwrap();
    watcher.watch(&right).unwrap();
    // Discard anything the platform reports from just before the watch began.
    std::thread::sleep(Duration::from_millis(400));
    watcher.poll();

    // Save the way an editor does: write a sibling, rename it over the target.
    let tmp = dir.join("b.rs.tmp");
    std::fs::write(&tmp, "fn main() {\n    let x = 99;\n    println!(\"{x}\");\n}\n").unwrap();
    std::fs::rename(&tmp, &right).unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut noticed = false;
    while Instant::now() < deadline && !noticed {
        noticed = watcher.poll().iter().any(|p| p == &right);
        if !noticed {
            std::thread::sleep(Duration::from_millis(40));
        }
    }
    assert!(noticed, "the watcher never reported the save");

    let after = visible(&render(&d));
    std::fs::remove_dir_all(&dir).ok();

    assert_ne!(before, after, "re-render produced identical output after a save");
    assert!(after.contains("let x = 99;"), "re-render missed the new contents:\n{after}");
    assert!(!after.contains("let x = 2;"), "re-render still showed the old contents");
}
