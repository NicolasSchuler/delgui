//! Watching panel-bound files for changes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

type TargetKey = (PathBuf, OsString);
type EventTargets = HashMap<TargetKey, Vec<TargetKey>>;

struct WatchedTarget {
    path: PathBuf,
    event_keys: Vec<TargetKey>,
}

/// Only one repaint signal needs to be outstanding. The changed targets live in
/// [`Pending`], so filling this channel coalesces more events instead of growing
/// an unbounded queue during a save storm.
const SIGNAL_CAPACITY: usize = 1;
const ERROR_CAPACITY: usize = 8;

#[derive(Default)]
struct Pending {
    changed: Vec<TargetKey>,
    event_keys: Vec<TargetKey>,
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

/// A file symlink can point outside its own directory. Keep the link's key as
/// well, so replacing the link itself still requests a reload.
fn event_keys_for(path: &Path, source_key: &TargetKey) -> Vec<TargetKey> {
    let mut keys = vec![source_key.clone()];
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        && let Some((target, intermediate_links)) = symlink_chain(path)
    {
        for watched_path in intermediate_links.iter().chain(std::iter::once(&target)) {
            if watched_path.parent().is_some_and(Path::is_dir)
                && let Some(target_key) = key_for(watched_path)
                && !keys.contains(&target_key)
            {
                keys.push(target_key);
            }
            // A directory watch is tied to that directory's inode. Watch its
            // entry in the nearest existing parent so removal and recreation
            // can rebind the watch even when no file event survives deletion.
            let mut directory = watched_path.parent();
            while let Some(dir) = directory {
                if dir.parent().is_some_and(Path::is_dir) {
                    if let Some(recovery_key) = key_for(dir)
                        && !keys.contains(&recovery_key)
                    {
                        keys.push(recovery_key);
                    }
                    break;
                }
                directory = dir.parent();
            }
        }
    }
    keys
}

/// Keep each file symlink in the chain so retargeting an intermediate link
/// refreshes the final target. Also works when the final file is absent.
fn symlink_chain(path: &Path) -> Option<(PathBuf, Vec<PathBuf>)> {
    let mut target = path.to_path_buf();
    let mut intermediate_links = Vec::new();
    for _ in 0..40 {
        let destination = std::fs::read_link(&target).ok()?;
        let parent = target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        target = if destination.is_absolute() {
            destination
        } else {
            parent.join(destination)
        };
        match std::fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                intermediate_links.push(target.clone());
            }
            Ok(_) => {
                return Some((target.canonicalize().unwrap_or(target), intermediate_links));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Some((target, intermediate_links));
            }
            Err(_) => return None,
        }
    }
    None
}

fn directories_for(keys: &[TargetKey]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for (dir, _) in keys {
        if !dirs.contains(dir) {
            dirs.push(dir.clone());
        }
    }
    dirs
}

fn signal(tx: &SyncSender<()>, wake: &impl Fn()) {
    if tx.try_send(()).is_ok() {
        wake();
    }
}

fn queue_paths(
    paths: impl IntoIterator<Item = PathBuf>,
    targets: &Mutex<EventTargets>,
    pending: &Mutex<Pending>,
    tx: &SyncSender<()>,
    wake: &impl Fn(),
) {
    let targets = lock(targets);
    let mut relevant = Vec::new();
    for path in paths {
        if let Some(event_key) = key_for(&path)
            && let Some(watched) = targets.get(&event_key)
        {
            relevant.push((event_key, watched.clone()));
        }
    }
    drop(targets);
    if relevant.is_empty() {
        return;
    }

    let mut pending = lock(pending);
    let before = (pending.changed.len(), pending.event_keys.len());
    for (event_key, watched) in relevant {
        if !pending.event_keys.contains(&event_key) {
            pending.event_keys.push(event_key);
        }
        for key in watched {
            if !pending.changed.contains(&key) {
                pending.changed.push(key);
            }
        }
    }
    let added = (pending.changed.len(), pending.event_keys.len()) != before;
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
/// It watches each file's **parent directory**, plus the resolved target's
/// parent and that directory's entry when the file is a symlink. Editors and
/// formatters overwhelmingly save by writing a temporary file and renaming it
/// over the target, which swaps the inode out from under a file-level watch --
/// so the first save would be seen and every one after it silently missed.
pub struct FileWatcher {
    inner: RecommendedWatcher,
    rx: Receiver<()>,
    pending: Arc<Mutex<Pending>>,
    event_targets: Arc<Mutex<EventTargets>>,
    /// Canonical parent directory -> how many watched paths require it.
    dirs: HashMap<PathBuf, usize>,
    /// Registered logically, but the backend watch could not be rebound.
    failed_dirs: HashSet<PathBuf>,
    /// A changed symlink target could not register its new directory yet.
    failed_targets: HashSet<TargetKey>,
    /// (canonical parent, file name) -> the caller path and keys that report it.
    targets: HashMap<TargetKey, WatchedTarget>,
}

impl FileWatcher {
    /// `wake` is called from the watcher thread whenever an event arrives, so
    /// an event-driven UI can schedule a repaint instead of polling.
    pub fn new(wake: impl Fn() + Send + 'static) -> notify::Result<Self> {
        let (tx, rx) = sync_channel(SIGNAL_CAPACITY);
        let pending = Arc::new(Mutex::new(Pending::default()));
        let event_targets = Arc::new(Mutex::new(HashMap::new()));
        let callback_pending = Arc::clone(&pending);
        let callback_targets = Arc::clone(&event_targets);
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
            event_targets,
            dirs: HashMap::new(),
            failed_dirs: HashSet::new(),
            failed_targets: HashSet::new(),
            targets: HashMap::new(),
        })
    }

    pub fn watch(&mut self, path: &Path) -> notify::Result<()> {
        let Some(key) = key_for(path) else {
            return Ok(());
        };
        if self.failed_targets.contains(&key) {
            self.refresh_target(&key, &[])?;
            self.failed_targets.remove(&key);
        }
        if let Some(target) = self.targets.get(&key) {
            return self.rebind_failed_dirs(&directories_for(&target.event_keys));
        }
        // Register all directories before recording the target. A failed
        // later registration must not leave earlier ones active.
        let event_keys = event_keys_for(path, &key);
        self.register_dirs(&directories_for(&event_keys))?;
        let mut event_targets = lock(&self.event_targets);
        for event_key in &event_keys {
            event_targets
                .entry(event_key.clone())
                .or_default()
                .push(key.clone());
        }
        drop(event_targets);
        self.targets.insert(
            key,
            WatchedTarget {
                path: path.to_path_buf(),
                event_keys,
            },
        );
        Ok(())
    }

    fn register_dirs(&mut self, dirs: &[PathBuf]) -> notify::Result<()> {
        self.rebind_failed_dirs(dirs)?;
        let mut registered: Vec<PathBuf> = Vec::new();
        for dir in dirs {
            if !self.dirs.contains_key(dir) {
                if let Err(error) = self.inner.watch(dir, RecursiveMode::NonRecursive) {
                    for registered_dir in registered {
                        if let Err(rollback_error) = self.inner.unwatch(&registered_dir) {
                            push_error(&self.pending, rollback_error);
                        }
                    }
                    return Err(error);
                }
                registered.push(dir.clone());
            }
        }
        for dir in dirs {
            *self.dirs.entry(dir.clone()).or_insert(0) += 1;
        }
        Ok(())
    }

    fn rebind_failed_dirs(&mut self, dirs: &[PathBuf]) -> notify::Result<()> {
        for dir in dirs {
            if self.failed_dirs.contains(dir) {
                self.inner.watch(dir, RecursiveMode::NonRecursive)?;
                self.failed_dirs.remove(dir);
            }
        }
        Ok(())
    }

    fn release_dirs(&mut self, dirs: &[PathBuf]) {
        for dir in dirs {
            if let Some(count) = self.dirs.get_mut(dir) {
                *count -= 1;
                if *count == 0 {
                    self.dirs.remove(dir);
                    self.failed_dirs.remove(dir);
                    if let Err(error) = self.inner.unwatch(dir) {
                        push_error(&self.pending, error);
                    }
                }
            }
        }
    }

    fn refresh_target(
        &mut self,
        key: &TargetKey,
        received_events: &[TargetKey],
    ) -> notify::Result<()> {
        let Some(target) = self.targets.get(key) else {
            return Ok(());
        };
        let old_event_keys = target.event_keys.clone();
        let event_keys = event_keys_for(&target.path, key);
        if event_keys == old_event_keys {
            for dir in directories_for(&event_keys) {
                // A directory can be replaced between UI polls. Its keys then
                // look unchanged, but the OS watch still belongs to the old
                // inode. Only a matching directory-entry event needs a rebind.
                if let Some(entry_key) = key_for(&dir)
                    && event_keys.contains(&entry_key)
                    && received_events.contains(&entry_key)
                    && dir.is_dir()
                {
                    let _ = self.inner.unwatch(&dir);
                    if let Err(error) = self.inner.watch(&dir, RecursiveMode::NonRecursive) {
                        self.failed_dirs.insert(dir);
                        push_error(&self.pending, error);
                    } else {
                        self.failed_dirs.remove(&dir);
                    }
                }
            }
            return Ok(());
        }
        let old_dirs = directories_for(&old_event_keys);
        let new_dirs = directories_for(&event_keys);
        let to_add: Vec<_> = new_dirs
            .iter()
            .filter(|dir| !old_dirs.contains(dir))
            .cloned()
            .collect();
        self.register_dirs(&to_add)?;

        let mut event_targets = lock(&self.event_targets);
        for old_key in &old_event_keys {
            if !event_keys.contains(old_key)
                && let Some(watched) = event_targets.get_mut(old_key)
            {
                watched.retain(|watched_key| watched_key != key);
                if watched.is_empty() {
                    event_targets.remove(old_key);
                }
            }
        }
        for new_key in &event_keys {
            if !old_event_keys.contains(new_key) {
                event_targets
                    .entry(new_key.clone())
                    .or_default()
                    .push(key.clone());
            }
        }
        drop(event_targets);

        self.targets.get_mut(key).expect("watched above").event_keys = event_keys;
        let to_remove: Vec<_> = old_dirs
            .iter()
            .filter(|dir| !new_dirs.contains(dir))
            .cloned()
            .collect();
        self.release_dirs(&to_remove);
        Ok(())
    }

    pub fn unwatch(&mut self, path: &Path) {
        let Some(key) = key_for(path) else {
            return;
        };
        let Some(target) = self.targets.remove(&key) else {
            return;
        };
        self.failed_targets.remove(&key);
        let mut event_targets = lock(&self.event_targets);
        for event_key in &target.event_keys {
            if let Some(watched) = event_targets.get_mut(event_key) {
                watched.retain(|watched_key| watched_key != &key);
                if watched.is_empty() {
                    event_targets.remove(event_key);
                }
            }
        }
        drop(event_targets);
        self.release_dirs(&directories_for(&target.event_keys));
    }

    /// Drain pending events, returning the watched paths that changed.
    ///
    /// Directory-level watching means we hear about siblings too, so events are
    /// filtered back down to the files actually asked for on the callback thread.
    pub fn poll(&mut self) -> Vec<PathBuf> {
        while self.rx.try_recv().is_ok() {}
        let (changed, received_events) = {
            let mut pending = lock(&self.pending);
            (
                std::mem::take(&mut pending.changed),
                std::mem::take(&mut pending.event_keys),
            )
        };
        for key in &changed {
            if let Err(error) = self.refresh_target(key, &received_events) {
                self.failed_targets.insert(key.clone());
                push_error(&self.pending, error);
            } else {
                self.failed_targets.remove(key);
            }
        }
        changed
            .into_iter()
            .filter_map(|key| self.targets.get(&key).map(|target| target.path.clone()))
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
        key_for(path)
            .filter(|key| !self.failed_targets.contains(key))
            .and_then(|key| self.targets.get(&key))
            .is_some_and(|target| {
                directories_for(&target.event_keys)
                    .iter()
                    .all(|dir| !self.failed_dirs.contains(dir))
            })
    }

    /// Everything currently registered, as the caller originally named it.
    ///
    /// Reconciling against this rather than against whatever paths the caller
    /// still holds is the only way to drop a watch on a file a panel has since
    /// forgotten -- otherwise a directory keeps waking the UI for a file nothing
    /// is looking at any more.
    pub fn watched(&self) -> Vec<PathBuf> {
        self.targets
            .values()
            .map(|target| target.path.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ERROR_CAPACITY, FileWatcher, Pending, event_keys_for, key_for, push_error, queue_paths,
    };
    use std::collections::HashMap;
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

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn follows_a_symlink_target_in_another_directory() {
        use std::os::unix::fs::symlink;

        let _serial = LIVE_WATCH.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("dpw-link-{}", std::process::id()));
        let link_dir = root.join("links");
        let target_dir = root.join("targets");
        std::fs::create_dir_all(&link_dir).unwrap();
        std::fs::create_dir_all(&target_dir).unwrap();
        let target = target_dir.join("data.txt");
        let link = link_dir.join("data.txt");
        std::fs::write(&target, "one").unwrap();
        symlink(&target, &link).unwrap();

        let mut w = FileWatcher::new(|| {}).unwrap();
        w.watch(&link).unwrap();
        assert_eq!(w.dirs.get(&link_dir.canonicalize().unwrap()), Some(&1));
        assert_eq!(w.dirs.get(&target_dir.canonicalize().unwrap()), Some(&1));
        settle(&mut w);

        std::fs::write(&target, "two").unwrap();
        assert!(wait_for(&mut w, &link, 5), "missed a target overwrite");
        let replacement = target_dir.join("replacement.txt");
        std::fs::write(&replacement, "three").unwrap();
        std::fs::rename(&replacement, &target).unwrap();
        assert!(wait_for(&mut w, &link, 5), "missed a target rename save");
        std::fs::remove_file(&target).unwrap();
        assert!(wait_for(&mut w, &link, 5), "missed target removal");
        std::fs::write(&target, "recreated").unwrap();
        assert!(wait_for(&mut w, &link, 5), "missed target recreation");

        w.watch(&target).unwrap();
        assert_eq!(w.dirs.get(&target_dir.canonicalize().unwrap()), Some(&2));
        w.unwatch(&link);
        assert_eq!(w.dirs.get(&target_dir.canonicalize().unwrap()), Some(&1));
        assert!(!w.is_watching(&link));
        w.unwatch(&target);
        assert!(w.dirs.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn follows_a_symlink_after_its_target_directory_is_recreated() {
        use std::os::unix::fs::symlink;

        let _serial = LIVE_WATCH.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("dpw-dir-recreate-{}", std::process::id()));
        let link_dir = root.join("links");
        let target_dir = root.join("targets");
        std::fs::create_dir_all(&link_dir).unwrap();
        std::fs::create_dir_all(&target_dir).unwrap();
        let target = target_dir.join("data.txt");
        let link = link_dir.join("data.txt");
        std::fs::write(&target, "one").unwrap();
        symlink(&target, &link).unwrap();

        let mut watcher = FileWatcher::new(|| {}).unwrap();
        watcher.watch(&link).unwrap();
        settle(&mut watcher);

        std::fs::remove_dir_all(&target_dir).unwrap();
        assert!(
            wait_for(&mut watcher, &link, 5),
            "missed target directory removal"
        );
        assert!(
            watcher.poll_errors().is_empty(),
            "expected directory removal should not report a watcher failure"
        );
        std::fs::create_dir(&target_dir).unwrap();
        std::fs::write(&target, "two").unwrap();
        assert!(
            wait_for(&mut watcher, &link, 5),
            "missed target directory recreation"
        );

        settle(&mut watcher);
        std::fs::write(&target, "three").unwrap();
        assert!(
            wait_for(&mut watcher, &link, 5),
            "missed a later target overwrite"
        );

        // A remove/create batch can reach poll after the new directory already
        // exists. The event keys then look unchanged, but the old OS watch is
        // attached to the removed directory's inode.
        std::fs::remove_dir_all(&target_dir).unwrap();
        std::fs::create_dir(&target_dir).unwrap();
        std::fs::write(&target, "four").unwrap();
        assert!(
            wait_for(&mut watcher, &link, 5),
            "missed rapid directory replacement"
        );
        settle(&mut watcher);
        std::fs::write(&target, "five").unwrap();
        assert!(
            wait_for(&mut watcher, &link, 5),
            "lost the watch after replacement"
        );
        watcher.unwatch(&link);
        assert!(watcher.dirs.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_symlink_can_start_watching_before_its_target_directory_exists() {
        use std::os::unix::fs::symlink;

        let _serial = LIVE_WATCH.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("dpw-missing-dir-{}", std::process::id()));
        let link_dir = root.join("links");
        let target_dir = root.join("targets");
        std::fs::create_dir_all(&link_dir).unwrap();
        let target = target_dir.join("data.txt");
        let link = link_dir.join("data.txt");
        symlink(&target, &link).unwrap();

        let mut watcher = FileWatcher::new(|| {}).unwrap();
        watcher.watch(&link).unwrap();
        assert_eq!(watcher.dirs.get(&root.canonicalize().unwrap()), Some(&1));
        settle(&mut watcher);
        std::fs::create_dir(&target_dir).unwrap();
        std::fs::write(&target, "one").unwrap();
        assert!(
            wait_for(&mut watcher, &link, 5),
            "missed the new target directory"
        );
        settle(&mut watcher);
        std::fs::write(&target, "two").unwrap();
        assert!(
            wait_for(&mut watcher, &link, 5),
            "lost the newly bound watch"
        );
        watcher.unwatch(&link);
        assert!(watcher.dirs.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn replacing_a_symlink_follows_its_new_target() {
        use std::os::unix::fs::symlink;

        let _serial = LIVE_WATCH.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("dpw-retarget-{}", std::process::id()));
        let link_dir = root.join("links");
        let old_dir = root.join("old");
        let new_dir = root.join("new");
        for dir in [&link_dir, &old_dir, &new_dir] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let old_target = old_dir.join("data.txt");
        let new_target = new_dir.join("data.txt");
        let link = link_dir.join("data.txt");
        std::fs::write(&old_target, "old").unwrap();
        std::fs::write(&new_target, "new").unwrap();
        symlink(&old_target, &link).unwrap();

        let mut w = FileWatcher::new(|| {}).unwrap();
        w.watch(&link).unwrap();
        settle(&mut w);

        let new_link = link_dir.join("replacement-link");
        symlink(&new_target, &new_link).unwrap();
        std::fs::rename(&new_link, &link).unwrap();
        assert!(wait_for(&mut w, &link, 5), "missed a link replacement");
        assert!(!w.dirs.contains_key(&old_dir.canonicalize().unwrap()));
        assert_eq!(w.dirs.get(&new_dir.canonicalize().unwrap()), Some(&1));
        settle(&mut w);

        std::fs::write(&new_target, "newer").unwrap();
        assert!(wait_for(&mut w, &link, 5), "missed the new target");
        w.unwatch(&link);
        assert!(w.dirs.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn replacing_an_intermediate_symlink_follows_its_new_target() {
        use std::os::unix::fs::symlink;

        let _serial = LIVE_WATCH.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("dpw-middle-retarget-{}", std::process::id()));
        let link_dir = root.join("links");
        let middle_dir = root.join("middle");
        let old_dir = root.join("old");
        let new_dir = root.join("new");
        for dir in [&link_dir, &middle_dir, &old_dir, &new_dir] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let old_target = old_dir.join("data.txt");
        let new_target = new_dir.join("data.txt");
        let middle = middle_dir.join("data.txt");
        let link = link_dir.join("data.txt");
        std::fs::write(&old_target, "old").unwrap();
        std::fs::write(&new_target, "new").unwrap();
        symlink(&old_target, &middle).unwrap();
        symlink(&middle, &link).unwrap();

        let mut watcher = FileWatcher::new(|| {}).unwrap();
        watcher.watch(&link).unwrap();
        settle(&mut watcher);

        let replacement = middle_dir.join("replacement-link");
        symlink(&new_target, &replacement).unwrap();
        std::fs::rename(&replacement, &middle).unwrap();
        assert!(
            wait_for(&mut watcher, &link, 5),
            "missed intermediate link replacement"
        );
        assert!(!watcher.dirs.contains_key(&old_dir.canonicalize().unwrap()));
        assert_eq!(watcher.dirs.get(&new_dir.canonicalize().unwrap()), Some(&1));
        settle(&mut watcher);

        std::fs::write(&new_target, "newer").unwrap();
        assert!(wait_for(&mut watcher, &link, 5), "missed the new target");
        watcher.unwatch(&link);
        assert!(watcher.dirs.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn retries_a_target_whose_directory_watch_was_lost() {
        use notify::Watcher;
        use std::os::unix::fs::symlink;

        let _serial = LIVE_WATCH.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("dpw-retry-rebind-{}", std::process::id()));
        let link_dir = root.join("links");
        let target_dir = root.join("targets");
        std::fs::create_dir_all(&link_dir).unwrap();
        std::fs::create_dir_all(&target_dir).unwrap();
        let target = target_dir.join("data.txt");
        let link = link_dir.join("data.txt");
        std::fs::write(&target, "one").unwrap();
        symlink(&target, &link).unwrap();

        let mut watcher = FileWatcher::new(|| {}).unwrap();
        watcher.watch(&link).unwrap();
        let target_dir = target_dir.canonicalize().unwrap();
        watcher.inner.unwatch(&target_dir).unwrap();
        watcher.failed_dirs.insert(target_dir.clone());
        assert!(!watcher.is_watching(&link));

        let moved_dir = root.join("temporarily-moved");
        std::fs::rename(&target_dir, &moved_dir).unwrap();
        assert!(watcher.watch(&link).is_err());
        assert!(!watcher.is_watching(&link));
        std::fs::rename(&moved_dir, &target_dir).unwrap();
        watcher.watch(&link).unwrap();
        assert!(watcher.is_watching(&link));
        settle(&mut watcher);
        std::fs::write(&target, "two").unwrap();
        assert!(
            wait_for(&mut watcher, &link, 5),
            "failed to rebind target directory"
        );

        watcher.unwatch(&link);
        assert!(watcher.dirs.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn retries_a_failed_target_registration_against_the_current_symlink() {
        use std::os::unix::fs::symlink;

        let _serial = LIVE_WATCH.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("dpw-retry-target-{}", std::process::id()));
        let link_dir = root.join("links");
        let old_dir = root.join("old");
        let new_dir = root.join("new");
        for dir in [&link_dir, &old_dir, &new_dir] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let old_target = old_dir.join("data.txt");
        let new_target = new_dir.join("data.txt");
        let link = link_dir.join("data.txt");
        std::fs::write(&old_target, "old").unwrap();
        std::fs::write(&new_target, "new").unwrap();
        symlink(&old_target, &link).unwrap();

        let mut watcher = FileWatcher::new(|| {}).unwrap();
        watcher.watch(&link).unwrap();
        let replacement = link_dir.join("replacement-link");
        symlink(&new_target, &replacement).unwrap();
        std::fs::rename(&replacement, &link).unwrap();
        watcher.failed_targets.insert(key_for(&link).unwrap());
        assert!(!watcher.is_watching(&link));

        watcher.watch(&link).unwrap();
        assert!(watcher.is_watching(&link));
        assert!(!watcher.dirs.contains_key(&old_dir.canonicalize().unwrap()));
        assert_eq!(watcher.dirs.get(&new_dir.canonicalize().unwrap()), Some(&1));
        watcher.unwatch(&link);
        assert!(watcher.dirs.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_dangling_symlink_chain_keeps_the_final_target_directory() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("dpw-chain-{}", std::process::id()));
        let link_dir = root.join("links");
        let middle_dir = root.join("middle");
        let target_dir = root.join("targets");
        for dir in [&link_dir, &middle_dir, &target_dir] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let target = target_dir.join("data.txt");
        let middle = middle_dir.join("data.txt");
        let link = link_dir.join("data.txt");
        symlink(&target, &middle).unwrap();
        symlink(&middle, &link).unwrap();

        let source_key = key_for(&link).unwrap();
        let event_keys = event_keys_for(&link, &source_key);
        assert_eq!(
            event_keys,
            vec![
                source_key,
                key_for(&middle).unwrap(),
                key_for(&middle_dir).unwrap(),
                key_for(&target).unwrap(),
                key_for(&target_dir).unwrap(),
            ]
        );
        std::fs::remove_dir_all(&root).ok();
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
        let targets = Mutex::new(HashMap::from([
            (first_key.clone(), vec![first_key.clone()]),
            (second_key.clone(), vec![second_key.clone()]),
        ]));
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
