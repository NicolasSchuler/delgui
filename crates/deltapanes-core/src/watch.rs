//! Watching panel-bound files for changes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

type TargetKey = (PathBuf, OsString);

/// Only one repaint signal needs to be outstanding. The changed targets live in
/// [`Pending`], so filling this channel coalesces more events instead of growing
/// an unbounded queue during a save storm.
const SIGNAL_CAPACITY: usize = 1;
const ERROR_CAPACITY: usize = 8;

#[derive(Default)]
struct Pending {
    changed: Vec<TargetKey>,
    errors: VecDeque<notify::Error>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Identify a file by its canonical directory plus its name, so that a path
/// reached through a symlink or a relative prefix still matches the events the
/// OS reports.
fn key_for(path: &Path) -> Option<TargetKey> {
    // `Path::parent` represents the parent of a bare relative name as an empty
    // path. notify rejects that on macOS; it means the current directory, so
    // make that meaning explicit before registration.
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let dir = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    Some((dir, path.file_name()?.to_os_string()))
}

fn signal(tx: &SyncSender<()>, wake: &impl Fn()) {
    if tx.try_send(()).is_ok() {
        wake();
    }
}

fn queue_paths(
    paths: impl IntoIterator<Item = PathBuf>,
    targets: &Mutex<HashSet<TargetKey>>,
    pending: &Mutex<Pending>,
    tx: &SyncSender<()>,
    wake: &impl Fn(),
) {
    let targets = lock(targets);
    let relevant: Vec<TargetKey> = paths
        .into_iter()
        .filter_map(|path| key_for(&path))
        .filter(|key| targets.contains(key))
        .collect();
    drop(targets);
    if relevant.is_empty() {
        return;
    }

    let mut pending = lock(pending);
    let before = pending.changed.len();
    for key in relevant {
        if !pending.changed.contains(&key) {
            pending.changed.push(key);
        }
    }
    let added = pending.changed.len() != before;
    drop(pending);
    if added {
        signal(tx, wake);
    }
}

fn push_error(pending: &Mutex<Pending>, error: notify::Error) {
    let mut pending = lock(pending);
    if pending.errors.len() == ERROR_CAPACITY {
        pending.errors.pop_front();
    }
    pending.errors.push_back(error);
}

/// Watches a small set of individual files and reports which ones changed.
///
/// It watches each file's **parent directory**, not the file. Editors and
/// formatters overwhelmingly save by writing a temporary file and renaming it
/// over the target, which swaps the inode out from under a file-level watch --
/// so the first save would be seen and every one after it silently missed.
pub struct FileWatcher {
    inner: RecommendedWatcher,
    rx: Receiver<()>,
    pending: Arc<Mutex<Pending>>,
    target_keys: Arc<Mutex<HashSet<TargetKey>>>,
    /// Canonical parent directory -> how many watched files live in it.
    dirs: HashMap<PathBuf, usize>,
    /// (canonical parent, file name) -> the path as the caller gave it.
    targets: HashMap<TargetKey, PathBuf>,
}

impl FileWatcher {
    /// `wake` is called from the watcher thread whenever an event arrives, so
    /// an event-driven UI can schedule a repaint instead of polling.
    pub fn new(wake: impl Fn() + Send + 'static) -> notify::Result<Self> {
        let (tx, rx) = sync_channel(SIGNAL_CAPACITY);
        let pending = Arc::new(Mutex::new(Pending::default()));
        let target_keys = Arc::new(Mutex::new(HashSet::new()));
        let callback_pending = Arc::clone(&pending);
        let callback_targets = Arc::clone(&target_keys);
        let callback_tx = tx.clone();
        let inner =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
                Ok(event)
                    if matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    ) =>
                {
                    queue_paths(
                        event.paths,
                        &callback_targets,
                        &callback_pending,
                        &callback_tx,
                        &wake,
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    push_error(&callback_pending, error);
                    signal(&callback_tx, &wake);
                }
            })?;
        Ok(Self {
            inner,
            rx,
            pending,
            target_keys,
            dirs: HashMap::new(),
            targets: HashMap::new(),
        })
    }

    pub fn watch(&mut self, path: &Path) -> notify::Result<()> {
        let Some(key) = key_for(path) else {
            return Ok(());
        };
        if self.targets.contains_key(&key) {
            return Ok(()); // already watched
        }
        // Register before recording. The other order books the file as watched
        // even when the directory could not be registered -- and since callers
        // skip anything `is_watching` already claims, that state never retries:
        // the checkbox stays ticked over a watch that does not exist.
        let first_in_dir = !self.dirs.contains_key(&key.0);
        if first_in_dir {
            self.inner.watch(&key.0, RecursiveMode::NonRecursive)?;
        }
        *self.dirs.entry(key.0.clone()).or_insert(0) += 1;
        self.targets.insert(key.clone(), path.to_path_buf());
        lock(&self.target_keys).insert(key);
        Ok(())
    }

    pub fn unwatch(&mut self, path: &Path) {
        let Some(key) = key_for(path) else {
            return;
        };
        if !self.targets.contains_key(&key) {
            return;
        }
        lock(&self.target_keys).remove(&key);
        self.targets.remove(&key);
        if let Some(count) = self.dirs.get_mut(&key.0) {
            *count -= 1;
            if *count == 0 {
                self.dirs.remove(&key.0);
                if let Err(error) = self.inner.unwatch(&key.0) {
                    push_error(&self.pending, error);
                }
            }
        }
    }

    /// Drain pending events, returning the watched paths that changed.
    ///
    /// Directory-level watching means we hear about siblings too, so events are
    /// filtered back down to the files actually asked for on the callback thread.
    pub fn poll(&mut self) -> Vec<PathBuf> {
        while self.rx.try_recv().is_ok() {}
        let changed = std::mem::take(&mut lock(&self.pending).changed);
        changed
            .into_iter()
            .filter_map(|key| self.targets.get(&key).cloned())
            .collect()
    }

    /// Drain asynchronous backend failures reported after watcher creation.
    ///
    /// Registration failures are still returned directly from [`Self::watch`].
    /// The GUI should call this alongside [`Self::poll`] and surface the newest
    /// error as a notice; errors stay queued until read and are capped so an
    /// unhealthy backend cannot grow memory without bound.
    pub fn poll_errors(&mut self) -> Vec<notify::Error> {
        lock(&self.pending).errors.drain(..).collect()
    }

    pub fn is_watching(&self, path: &Path) -> bool {
        key_for(path).is_some_and(|k| self.targets.contains_key(&k))
    }

    /// Everything currently registered, as the caller originally named it.
    ///
    /// Reconciling against this rather than against whatever paths the caller
    /// still holds is the only way to drop a watch on a file a panel has since
    /// forgotten -- otherwise a directory keeps waking the UI for a file nothing
    /// is looking at any more.
    pub fn watched(&self) -> Vec<PathBuf> {
        self.targets.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{ERROR_CAPACITY, FileWatcher, Pending, key_for, push_error, queue_paths};
    use std::collections::HashSet;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{TryRecvError, sync_channel};
    use std::time::{Duration, Instant};

    // macOS routes all RecommendedWatcher instances through the same FSEvents
    // service. Exercising several live watchers concurrently can delay each
    // one's first batch past the test deadline even though each contract is
    // sound in isolation, so serialize only the integration-style tests.
    static LIVE_WATCH: Mutex<()> = Mutex::new(());

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
        let _serial = LIVE_WATCH.lock().unwrap_or_else(|e| e.into_inner());
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
        let _serial = LIVE_WATCH.lock().unwrap_or_else(|e| e.into_inner());
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
        let _serial = LIVE_WATCH.lock().unwrap_or_else(|e| e.into_inner());
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
        assert!(
            changed.is_empty(),
            "reported an unwatched sibling: {changed:?}"
        );
    }

    #[test]
    fn unwatch_stops_reporting() {
        let _serial = LIVE_WATCH.lock().unwrap_or_else(|e| e.into_inner());
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
        assert!(
            changed.is_empty(),
            "still reporting after unwatch: {changed:?}"
        );
    }

    #[test]
    fn a_bare_relative_file_watches_the_current_directory() {
        let _serial = LIVE_WATCH.lock().unwrap_or_else(|e| e.into_inner());
        let path = std::path::Path::new("Cargo.toml");
        let mut w = FileWatcher::new(|| {}).unwrap();

        w.watch(path).unwrap();

        assert!(w.is_watching(path));
        assert!(w.watched().contains(&path.to_path_buf()));
        w.unwatch(path);
    }

    #[test]
    fn callback_filters_siblings_and_coalesces_targets_behind_one_signal() {
        let dir = std::env::temp_dir().join(format!("dpw-queue-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (first, second, sibling) = (
            dir.join("a.txt"),
            dir.join("b.txt"),
            dir.join("unwatched.txt"),
        );
        let first_key = key_for(&first).unwrap();
        let second_key = key_for(&second).unwrap();
        let targets = Mutex::new(HashSet::from([first_key.clone(), second_key.clone()]));
        let pending = Mutex::new(Pending::default());
        let (tx, rx) = sync_channel(1);
        let wakes = AtomicUsize::new(0);
        let wake = || {
            wakes.fetch_add(1, Ordering::Relaxed);
        };

        queue_paths(
            [sibling, first.clone(), first],
            &targets,
            &pending,
            &tx,
            &wake,
        );
        // A second target is retained even though the single wake slot is full.
        queue_paths([second], &targets, &pending, &tx, &wake);

        assert_eq!(pending.lock().unwrap().changed, vec![first_key, second_key]);
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert_eq!(rx.try_recv(), Ok(()));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backend_error_queue_discards_the_oldest_entry_at_its_bound() {
        let pending = Mutex::new(Pending::default());
        for index in 0..ERROR_CAPACITY + 3 {
            push_error(&pending, notify::Error::generic(&format!("error {index}")));
        }

        let pending = pending.lock().unwrap();
        assert_eq!(pending.errors.len(), ERROR_CAPACITY);
        assert_eq!(pending.errors.front().unwrap().to_string(), "error 3");
    }
}
