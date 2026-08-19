//! Watching panel-bound files for changes.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Watches a small set of individual files and reports which ones changed.
///
/// It watches each file's **parent directory**, not the file. Editors and
/// formatters overwhelmingly save by writing a temporary file and renaming it
/// over the target, which swaps the inode out from under a file-level watch --
/// so the first save would be seen and every one after it silently missed.
pub struct FileWatcher {
    inner: RecommendedWatcher,
    rx: Receiver<PathBuf>,
    /// Canonical parent directory -> how many watched files live in it.
    dirs: HashMap<PathBuf, usize>,
    /// (canonical parent, file name) -> the path as the caller gave it.
    targets: HashMap<(PathBuf, OsString), PathBuf>,
}

impl FileWatcher {
    /// `wake` is called from the watcher thread whenever an event arrives, so
    /// an event-driven UI can schedule a repaint instead of polling.
    pub fn new(wake: impl Fn() + Send + 'static) -> notify::Result<Self> {
        let (tx, rx) = channel();
        let inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if !matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                return;
            }
            let mut sent = false;
            for p in event.paths {
                sent |= tx.send(p).is_ok();
            }
            if sent {
                wake();
            }
        })?;
        Ok(Self { inner, rx, dirs: HashMap::new(), targets: HashMap::new() })
    }

    pub fn watch(&mut self, path: &Path) -> notify::Result<()> {
        let Some(key) = self.key_for(path) else {
            return Ok(());
        };
        if self.targets.insert(key.clone(), path.to_path_buf()).is_some() {
            return Ok(()); // already watched
        }
        let count = self.dirs.entry(key.0.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            self.inner.watch(&key.0, RecursiveMode::NonRecursive)?;
        }
        Ok(())
    }

    pub fn unwatch(&mut self, path: &Path) {
        let Some(key) = self.key_for(path) else { return };
        if self.targets.remove(&key).is_none() {
            return;
        }
        if let Some(count) = self.dirs.get_mut(&key.0) {
            *count -= 1;
            if *count == 0 {
                self.dirs.remove(&key.0);
                let _ = self.inner.unwatch(&key.0);
            }
        }
    }

    /// Drain pending events, returning the watched paths that changed.
    ///
    /// Directory-level watching means we hear about siblings too, so events are
    /// filtered back down to the files actually asked for.
    pub fn poll(&mut self) -> Vec<PathBuf> {
        let mut changed: Vec<PathBuf> = Vec::new();
        while let Ok(p) = self.rx.try_recv() {
            let Some(key) = self.key_for(&p) else { continue };
            if let Some(target) = self.targets.get(&key) {
                if !changed.contains(target) {
                    changed.push(target.clone());
                }
            }
        }
        changed
    }

    pub fn is_watching(&self, path: &Path) -> bool {
        self.key_for(path).is_some_and(|k| self.targets.contains_key(&k))
    }

    /// Identify a file by its canonical directory plus its name, so that a path
    /// reached through a symlink or a relative prefix still matches the events
    /// the OS reports.
    fn key_for(&self, path: &Path) -> Option<(PathBuf, OsString)> {
        let parent = path.parent()?;
        let dir = parent.canonicalize().unwrap_or_else(|_| parent.to_path_buf());
        Some((dir, path.file_name()?.to_os_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::FileWatcher;
    use std::time::{Duration, Instant};

    /// Let the watch take effect and discard anything the platform reports
    /// from just before it did. macOS FSEvents delivers with latency and will
    /// happily hand over an event for a write that happened moments earlier.
    fn settle(w: &mut FileWatcher) {
        std::thread::sleep(Duration::from_millis(400));
        w.poll();
    }

    fn wait_for(w: &mut FileWatcher, want: &std::path::Path, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if w.poll().iter().any(|p| p == want) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        false
    }

    #[test]
    fn detects_a_plain_overwrite() {
        let dir = std::env::temp_dir().join(format!("dpw-plain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one").unwrap();

        let mut w = FileWatcher::new(|| {}).unwrap();
        w.watch(&file).unwrap();
        settle(&mut w);
        std::fs::write(&file, "two").unwrap();

        let seen = wait_for(&mut w, &file, 5);
        std::fs::remove_dir_all(&dir).ok();
        assert!(seen, "a direct overwrite went unnoticed");
    }

    /// The reason this watches directories: an editor saving by atomic rename
    /// replaces the inode, which a file-level watch stops following.
    #[test]
    fn survives_an_atomic_rename_save() {
        let dir = std::env::temp_dir().join(format!("dpw-rename-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one").unwrap();

        let mut w = FileWatcher::new(|| {}).unwrap();
        w.watch(&file).unwrap();
        settle(&mut w);

        for round in 0..2 {
            let tmp = dir.join(format!("a.txt.tmp{round}"));
            std::fs::write(&tmp, format!("round {round}")).unwrap();
            std::fs::rename(&tmp, &file).unwrap();
            assert!(
                wait_for(&mut w, &file, 5),
                "missed save {round} -- the watch did not follow the new inode"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ignores_siblings_in_the_same_directory() {
        let dir = std::env::temp_dir().join(format!("dpw-sib-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (watched, other) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&watched, "one").unwrap();

        let mut w = FileWatcher::new(|| {}).unwrap();
        w.watch(&watched).unwrap();
        settle(&mut w);
        std::fs::write(&other, "unrelated").unwrap();
        std::thread::sleep(Duration::from_millis(600));

        let changed = w.poll();
        std::fs::remove_dir_all(&dir).ok();
        assert!(changed.is_empty(), "reported an unwatched sibling: {changed:?}");
    }

    #[test]
    fn unwatch_stops_reporting() {
        let dir = std::env::temp_dir().join(format!("dpw-un-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one").unwrap();

        let mut w = FileWatcher::new(|| {}).unwrap();
        w.watch(&file).unwrap();
        assert!(w.is_watching(&file));
        settle(&mut w);
        w.unwatch(&file);
        assert!(!w.is_watching(&file));

        std::fs::write(&file, "two").unwrap();
        std::thread::sleep(Duration::from_millis(600));

        let changed = w.poll();
        std::fs::remove_dir_all(&dir).ok();
        assert!(changed.is_empty(), "still reporting after unwatch: {changed:?}");
    }
}
