use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant, SystemTime};

use delgui_core::ansi::{self, Line};
use delgui_core::config::{self, DeltaConfig};
use delgui_core::delta::{Appearance, Delta, DeltaError, Input, Options, Whitespace};
use delgui_core::language;
use delgui_core::merge::{self, Hunk};
use delgui_core::watch::FileWatcher;
use egui::{Align, Color32, Frame, Key, Layout, Margin, RichText, Stroke, TextStyle, Vec2};

use crate::fonts::{self, Face, Family, Probe};
use crate::hotkey::Hotkey;
use crate::keys::{self, Action};
use crate::render::{self, Palette};
use crate::settings::{Context, MONO_PT, ResultPlacement, Settings, ThemeChoice, UI_PT};
use crate::theme::{Tokens, radius};
use crate::ui;

/// Beyond this, delta itself becomes the bottleneck: ~0.8 s at 2 MB and ~6.6 s
/// at 19 MB, producing roughly seven times the input in ANSI. We refuse rather
/// than hang, since a diff you wait ten seconds for is a diff you did by eye.
///
/// Decimal megabytes, because that is the unit the message reports in.
pub(crate) const MAX_PANEL_BYTES: usize = 4_000_000;

/// Above this, editing stops re-rendering by itself and waits to be asked.
/// delta costs about a second per megabyte, and a diff that recomputes while
/// you type is worse than one you trigger.
pub(crate) const AUTO_RENDER_BYTES: usize = 1_000_000;

/// Delta re-runs on every width change, so resizing has to settle first.
pub(crate) const RESIZE_DEBOUNCE: Duration = Duration::from_millis(120);

/// Typing is a stream of changes, and each one would otherwise be a subprocess.
pub(crate) const EDIT_DEBOUNCE: Duration = Duration::from_millis(300);

/// A save is rarely one filesystem event, and a file mid-write reads as
/// truncated, so watch events are coalesced before re-reading.
pub(crate) const WATCH_DEBOUNCE: Duration = Duration::from_millis(180);

/// delta is a two-way tool. More panels means more pairs against one reference,
/// and past a handful the columns are too narrow to read anyway.
pub(crate) const MAX_PANELS: usize = 6;

/// Takes to remember. These are snapshots of a buffer that may be megabytes, so
/// they are bounded twice; the Undo control goes quiet at the end of the history
/// rather than silently doing nothing.
/// How long a "Copied" or "Saved to …" confirmation stays up.
const FLASH: Duration = Duration::from_secs(4);

/// In a short Combine window, the source and result panels are both useful but
/// neither may consume the entire operation the screen exists for: inspecting
/// and taking a difference. These are outer panel sizes, including margins.
const MIN_DIFF_OUTER_HEIGHT: f32 = 128.0;
// Frame margins, one maximum-scale control row, its gap, and one complete
// monospace row. Below this the result's Save/Copy actions are reduced to a
// clipped sliver at the supported 720×480 / 20 pt combination.
const MIN_RESULT_OUTER_HEIGHT: f32 = 128.0;

pub(crate) const UNDO_DEPTH: usize = 100;
pub(crate) const UNDO_BYTES: usize = 64_000_000;

/// Where `git mergetool` expects the merge to be written, and whether it was.
///
/// Git decides from the exit status whether to stage the file it handed over
/// (`mergetool.<tool>.trustExitCode`), so the answer has to outlive the window
/// -- hence the shared flag rather than a return value.
pub struct MergeTool {
    pub merged: PathBuf,
    pub resolved: Arc<AtomicBool>,
}

/// What the command line asked for, as distinct from what is remembered.
///
/// One value rather than six parameters because it keeps growing: every launch
/// path the app gains -- a paste, a hotkey, git handing over a conflict -- adds
/// to it, and they are all the same question.
#[derive(Default)]
pub struct Launch {
    pub preload: Option<String>,
    pub files: Vec<PathBuf>,
    pub watch: bool,
    pub combine: bool,
    pub mergetool: Option<MergeTool>,
    pub hotkey: Option<Hotkey>,
}

pub struct Panel {
    pub text: String,
    pub path: Option<PathBuf>,
    /// The buffer has drifted from the file it came from.
    ///
    /// This is the difference between handing delta a path and handing it the
    /// bytes on screen. Without it, typing into a panel opened from a file is
    /// silently discarded: delta re-reads the file and renders what is on disk,
    /// while the panel shows something else entirely.
    pub edited: bool,
    /// Syntax sniffed from the panel's own content, recomputed only on edit.
    /// `None` means "this looks like prose", which is a perfectly good answer.
    pub detected: Option<&'static str>,
    /// What the user said the syntax is, overriding both of the above.
    pub language: Option<String>,
    /// Re-read and re-diff when the bound file changes on disk.
    pub watch: bool,
    /// A requested or watched reload could not be read. The previous buffer is
    /// preserved, but it must not continue supporting a "fresh" or
    /// "identical" claim until a reload succeeds or the user takes local
    /// control by editing it.
    disk_stale: bool,
    /// This panel is being assembled out of the others rather than compared with
    /// them. While it is, it is the baseline, so every diff on screen reads "my
    /// result against a candidate" and taking a difference writes here.
    pub result: bool,
    /// Where this result was last written.
    ///
    /// Deliberately not `path`: binding it there would make the result a
    /// file-backed panel, which switches on *Reload from disk*, *Discard my
    /// edits* and *Follow changes on disk* in its menu -- three one-click ways to
    /// throw the merge away, in the menu you open to copy it out.
    pub saved_to: Option<PathBuf>,
    /// The exact text written by the last successful save in this process.
    ///
    /// A path alone cannot tell whether another editor replaced the file after
    /// that save. Keeping the snapshot lets the next plain Save refuse to
    /// overwrite newer work; Save As remains the explicit path for choosing and
    /// confirming a replacement through the native file dialog.
    saved_snapshot: Option<String>,
    /// Cheap identity of the file created by the last canonical save. This is
    /// checked on repaint so an external replacement withdraws the saved/Git
    /// resolution state without re-reading a multi-megabyte result every frame.
    saved_stamp: Option<SavedFileStamp>,
    /// The result holds work that is not on disk. Distinct from `edited`, which
    /// means "diverged from the file this panel was opened from" and is false for
    /// a result that has never been saved anywhere.
    pub dirty: bool,
    /// Content generation used by render keys. Changing panel C must not make a
    /// cached comparison of A and B stale.
    revision: u64,
    /// Cached with language detection whenever the text changes. Panel headers
    /// are painted every frame, so they must not rescan a multi-megabyte buffer.
    line_count: usize,
}

impl Panel {
    fn empty() -> Self {
        Self {
            text: String::new(),
            path: None,
            edited: false,
            detected: None,
            language: None,
            watch: false,
            disk_stale: false,
            result: false,
            saved_to: None,
            saved_snapshot: None,
            saved_stamp: None,
            dirty: false,
            revision: 0,
            line_count: 0,
        }
    }

    /// What to hand delta for this side.
    ///
    /// Core callers may hand an untouched file over as a path. The GUI forces a
    /// buffer snapshot so a later disk write cannot disagree with its cache key,
    /// and carries the extension separately as `--default-language`.
    fn to_input(&self, force_buffer: bool) -> Input {
        match &self.path {
            Some(p) if !self.edited && !force_buffer => Input::Path(p.clone()),
            _ => Input::Buffer(self.text.as_bytes().to_vec()),
        }
    }

    fn resniff(&mut self) {
        self.detected = language::detect(&self.text);
        self.line_count = if self.text.is_empty() {
            0
        } else {
            self.text.lines().count().max(1)
        };
    }

    /// What this panel would tell delta about its own syntax, in order of how
    /// much we trust it. A path still speaks even once the buffer has been
    /// edited -- the bytes then travel as a pipe, so `--default-language` is
    /// what carries the extension across.
    fn language_hint(&self) -> Option<String> {
        if let Some(explicit) = self
            .language
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            return Some(explicit.to_string());
        }
        for path in [self.path.as_ref(), self.saved_to.as_ref()]
            .into_iter()
            .flatten()
        {
            if let Some(ext) = path.extension() {
                return Some(ext.to_string_lossy().into_owned());
            }
        }
        self.detected.map(String::from)
    }

    /// Free to be filled by a paste or a dropped file.
    ///
    /// A result panel never is, however empty it looks: `--paste`, the global
    /// hotkey and drag-and-drop all fill the first empty slot, and the hotkey
    /// fires from other applications -- so an empty result would be claimed out
    /// of sight, which is the one thing that must not happen to it.
    fn is_empty(&self) -> bool {
        self.text.is_empty() && self.path.is_none() && !self.result
    }

    fn name(&self) -> String {
        if self.result {
            return self
                .saved_to
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Result".into());
        }
        match &self.path {
            Some(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            None if self.text.is_empty() => "Empty".into(),
            None => "Pasted text".into(),
        }
    }

    /// The one-line subtitle under the name. Lines, not bytes: a diff is counted
    /// in lines, and `2097152 bytes` was never a sentence anyone read.
    fn detail(&self) -> Option<String> {
        if self.text.is_empty() {
            return None;
        }
        let lines = self.line_count.max(1);
        Some(format!("{lines} line{}", if lines == 1 { "" } else { "s" }))
    }

    fn bind(&mut self, path: PathBuf) -> Result<(), String> {
        // Reported rather than swallowed: `unwrap_or_default` here produced a
        // panel that looked empty but still handed delta the real file, so the
        // panel and the diff disagreed about what was being compared.
        let text = read_panel_text(&path)?;
        self.text = text;
        self.path = Some(path);
        self.edited = false;
        self.disk_stale = false;
        self.language = None;
        self.resniff();
        Ok(())
    }

    fn reload(&mut self) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let text = read_panel_text(&path)?;
        self.text = text;
        self.edited = false;
        self.disk_stale = false;
        self.resniff();
        Ok(())
    }

    fn clear(&mut self) {
        // Including `watch`: there is no longer a file to follow. Not the result
        // role, though -- an emptied result is the "start from nothing" case,
        // not a way out of building one.
        let result = self.result;
        *self = Self::empty();
        self.result = result;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SavedFileStamp {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

fn saved_file_stamp(path: &Path) -> Option<SavedFileStamp> {
    let metadata = path.metadata().ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Some(SavedFileStamp {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        Some(SavedFileStamp {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

/// Read at most one byte beyond the supported panel size. Checking only after
/// `read_to_string` lets a multi-gigabyte file freeze or exhaust the GUI before
/// the friendly size error can ever be shown.
fn read_panel_text(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| panel_read_error(path, &e))?;
    if !meta.file_type().is_file() {
        return Err(format!(
            "Could not read {} as text: it is not a regular file.",
            path.display()
        ));
    }
    if meta.len() > MAX_PANEL_BYTES as u64 {
        return Err(panel_too_large(path, meta.len() as usize));
    }
    let mut file = File::open(path).map_err(|e| panel_read_error(path, &e))?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((MAX_PANEL_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| panel_read_error(path, &e))?;
    if bytes.len() > MAX_PANEL_BYTES {
        return Err(panel_too_large(path, bytes.len()));
    }
    String::from_utf8(bytes).map_err(|e| {
        format!(
            "Could not read {} as text: {e}. delgui compares text.",
            path.display()
        )
    })
}

fn panel_read_error(path: &Path, error: &std::io::Error) -> String {
    format!(
        "Could not read {} as text: {error}. delgui compares text.",
        path.display()
    )
}

fn panel_too_large(path: &Path, bytes: usize) -> String {
    format!(
        "Could not read {}: it is {:.1} MB. delgui stops at {} MB so the GUI stays responsive.",
        path.display(),
        bytes as f64 / 1e6,
        MAX_PANEL_BYTES / 1_000_000
    )
}

fn paths_refer_to_same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    if let (Ok(a), Ok(b)) = (a.canonicalize(), b.canonicalize())
        && a == b
    {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if let (Ok(a), Ok(b)) = (a.metadata(), b.metadata()) {
            return a.dev() == b.dev() && a.ino() == b.ino();
        }
    }
    false
}

/// A notify backend error can name the watched file, its sibling, or the
/// directory that is actually registered. An error with no paths is global.
fn watch_error_affects(path: &Path, affected: &[PathBuf]) -> bool {
    if affected.is_empty() {
        return true;
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    affected.iter().any(|affected| {
        paths_refer_to_same_file(path, affected)
            || paths_refer_to_same_file(parent, affected)
            || affected
                .parent()
                .filter(|affected_parent| !affected_parent.as_os_str().is_empty())
                .is_some_and(|affected_parent| {
                    paths_refer_to_same_file(parent, affected_parent)
                })
    })
}

static SAVE_NONCE: AtomicU64 = AtomicU64::new(0);

/// Replace `path` only after all bytes have reached a sibling file. A failed
/// write therefore leaves the previous saved result intact.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("result");
    let mut last_collision = None;
    for _ in 0..32 {
        let nonce = SAVE_NONCE.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".{name}.delgui-{}-{nonce}", std::process::id()));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_collision = Some(e);
                continue;
            }
            Err(e) => return Err(e),
        };
        let result = (|| {
            if let Ok(meta) = path.metadata() {
                file.set_permissions(meta.permissions())?;
            }
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temp, path)?;
            // Best effort: not every platform lets a directory be opened, but
            // where it does this also makes the rename durable across power loss.
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        return result;
    }
    Err(last_collision.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not reserve a save file",
        )
    }))
}

/// Panels are labelled by position, so the labels stay stable as a set rather
/// than travelling with a panel when one is removed.
fn title(i: usize) -> char {
    (b'A' + i as u8) as char
}

/// Identifies a rendered diff cheaply enough to rebuild every frame.
///
/// It deliberately holds a revision counter rather than the panel text: this is
/// constructed on each repaint to decide whether anything needs re-rendering,
/// and copying multi-megabyte buffers sixty times a second to answer "did
/// anything change?" would cost more than the rendering it guards.
#[derive(Default, PartialEq, Clone, Debug)]
struct RenderKey {
    /// Rendering/configuration generation shared by every pair.
    revision: u64,
    reference: usize,
    shown: usize,
    reference_revision: u64,
    shown_revision: u64,
    args: Vec<String>,
}

struct Cached {
    key: RenderKey,
    lines: Vec<Line>,
    /// Each difference and the rows delta drew for it, for a render made in
    /// merge mode where the two lined up. Empty otherwise, and empty is what
    /// makes the take controls absent rather than wrong.
    hunks: Vec<(Hunk, Range<usize>)>,
    /// Why a diff that should have carried take controls does not.
    problem: Option<String>,
    /// The column count delta laid this out against. The window's current width
    /// is not the same thing while a re-render is in flight, and padding the
    /// `ESC[K` fill to the wrong one is visible as a ragged right edge.
    columns: usize,
}

#[derive(Clone, PartialEq)]
struct LayoutStyleKey {
    font: egui::FontId,
    ansi16: [Color32; 16],
    foreground: Color32,
    background: Color32,
    line_height: f32,
    pixels_per_point: f32,
}

struct PreparedDiff {
    render_key: RenderKey,
    style: LayoutStyleKey,
    whole: render::PreparedLayout,
    hunks: Vec<render::PreparedLayout>,
}

/// What a panel's ⋯ menu asked for, applied once its borrows are over. A menu
/// closes on the first click, so at most one of these can be pending.
#[derive(Clone, Copy)]
enum PanelRequest {
    Open,
    Remove,
}

/// What the result band's header asked for, applied once its borrows are over.
#[derive(Default)]
struct BandActions {
    /// `Some(true)` asks for a dialog; `Some(false)` overwrites where it was.
    save: Option<bool>,
    copy: bool,
    placement: Option<ResultPlacement>,
    seed: Option<Option<usize>>,
    stop: bool,
    remove: bool,
}

struct ResultIdentityView {
    name: Option<String>,
    state: String,
    state_is_success: bool,
    destination: Option<String>,
    detail: Option<String>,
    flash: Option<String>,
}

#[derive(Clone)]
struct ResultUndo {
    generation: u64,
    before: String,
    after: String,
}

enum DestructiveAction {
    LoadFiles(Vec<(usize, PathBuf)>),
    Reload(usize),
    Clear(usize),
    Remove(usize),
    Reseed(Option<usize>),
}

enum Job {
    Done(usize, Cached, Duration),
    Failed(RenderKey, String, Duration),
}

/// One render, and -- in merge mode -- the hunks that go with it.
///
/// Merge mode runs the diff itself rather than letting delta run it internally:
/// the bytes `Delta::diff` returns are both what gets parsed for structure and
/// what gets piped back in to be drawn, so the take controls cannot end up
/// describing a comparison other than the one on screen. Reading `@@` back out
/// of the rendering was the first design and it cannot be made safe -- delta
/// rejects `--hunk-header-style` twice, and a line of file content is
/// indistinguishable from a header once a gitconfig has emptied the line-number
/// columns.
fn render_job(
    delta: &Delta,
    left: &Input,
    right: &Input,
    opts: &Options,
    key: RenderKey,
    columns: usize,
    merging: bool,
) -> Result<Cached, DeltaError> {
    // One path for every render. delta's two-file mode shells out to
    // `git diff --no-index` itself, so running that step here costs a process
    // spawn rather than a second diff, and `patch_path_matches_two_file_mode`
    // pins the two as byte-identical. What it buys is that the app knows the
    // diff it is showing: the context width, the hunk ranges, and anything else
    // decided before delta sees a patch never reaches delta's argv, so it cannot
    // be recovered from the rendering afterwards.
    let patch = delta.diff(left, right, opts)?;
    let rendered = ansi::parse(&delta.render_patch(&patch, opts)?);
    let hunks = merge::parse(&patch);
    // Every render is marked, so every render knows where its differences are --
    // which is all Previous/Next change needs. The marker rows come back out
    // here; `keep` decides whether the one delta drew stays as the header.
    let keep = !merging && opts.hunk_headers;
    let (lines, located) = merge::prepare_rows(ansi::body(&rendered), hunks.len(), keep);
    if !merging {
        return Ok(Cached {
            key,
            lines,
            columns,
            // Without spans there is nothing to navigate, which is a quieter
            // failure than merge mode's: no controls appear and the diff is
            // unaffected, so it is not worth a banner.
            hunks: located
                .map(|spans| hunks.into_iter().zip(spans).collect())
                .unwrap_or_default(),
            problem: None,
        });
    }
    // Both sides are buffers in merge mode, so these are exactly the strings a
    // take will splice -- not a file that may have moved on since git read it.
    let texts = left
        .as_bytes()
        .zip(right.as_bytes())
        .and_then(|(a, b)| Some((std::str::from_utf8(a).ok()?, std::str::from_utf8(b).ok()?)));
    let (hunks, problem) = match (located, texts) {
        (Some(spans), Some((base, cand))) if merge::verify(base, cand, &hunks) => {
            (hunks.into_iter().zip(spans).collect(), None)
        }
        (None, _) => (
            Vec::new(),
            Some(
                "delgui cannot tell which rows each difference covers here, so it is not \
                 offering to take them. The diff itself is unaffected."
                    .to_string(),
            ),
        ),
        _ => (
            Vec::new(),
            Some(
                "The diff does not account for everything these panels differ by, so \
                 delgui is not offering to take from it. The diff itself is unaffected."
                    .to_string(),
            ),
        ),
    };
    Ok(Cached {
        key,
        lines,
        columns,
        hunks,
        problem,
    })
}

pub struct App {
    delta: Delta,
    panels: Vec<Panel>,
    /// Panel every other panel is compared against.
    reference: usize,
    /// Panel whose diff against the reference is on screen. Never the reference.
    shown: usize,
    focused: usize,

    opts: Options,
    settings: Settings,
    /// What is currently installed in egui, so fonts are rebuilt on a change and
    /// not on every frame -- `set_fonts` compares the whole file byte by byte
    /// and then throws away the glyph atlas.
    installed: (Option<Face>, Option<Face>, Option<Face>, f32, f32),
    catalog: Vec<Family>,
    catalog_rx: Option<Receiver<Vec<Family>>>,
    probe: Option<Probe>,

    themes: Vec<(String, bool)>,
    gitconfig: DeltaConfig,
    features: BTreeSet<String>,
    show_settings: bool,
    show_help: bool,

    cache: HashMap<usize, Cached>,
    /// One slot, not a map keyed by panel: only `shown` is ever drawn, so every
    /// other entry was an unreachable galley kept alive -- and a stale entry
    /// also kept the find bar and *Copy diff* serving a comparison the cache no
    /// longer held, which is how *Copy diff* could put the previous pair's diff
    /// on the clipboard after a panel was removed.
    prepared: Option<PreparedDiff>,
    /// The render currently running, if any. A single bool could not say *what*
    /// was running, so results arrived out of order and a failure could not be
    /// attributed -- which turned one failed render into an unbounded respawn
    /// loop, thousands of subprocesses a second.
    in_flight: Option<RenderKey>,
    /// The last render that failed, so we do not immediately try it again.
    failed: Option<RenderKey>,
    error: Option<String>,
    notice: Option<String>,

    columns: usize,
    pending_resize: Option<Instant>,
    pending_edit: Option<Instant>,
    pending_reload: Option<Instant>,
    watcher: Option<FileWatcher>,
    hotkey: Option<Hotkey>,
    /// Rects of the panels, recorded during layout so a dropped file can be
    /// routed to whichever one it landed on.
    panel_rects: Vec<egui::Rect>,
    /// True once the user has asked for a diff at least once. Until then the
    /// app shows guidance instead of running delta over two empty buffers.
    compared: bool,
    /// Bumped when rendering configuration or panel topology changes. Individual
    /// content changes use `Panel::revision`, so an unrelated panel does not
    /// invalidate the pair on screen.
    revision: u64,
    /// A render was asked for outright -- by ⌘Enter, or by taking a difference --
    /// rather than following from a keystroke. `tick` will not schedule one for a
    /// pair over `AUTO_RENDER_BYTES` and `schedule` drops it while another is in
    /// flight, so without this a take on a large pair would leave the diff stale
    /// and every control on it dead until the user pressed ⌘Enter.
    requested: bool,
    /// Result identity survives leaving take mode. This flag only controls
    /// whether the result is currently the writable baseline with take controls.
    building_result: bool,
    result_generation: u64,
    undo: VecDeque<ResultUndo>,
    redo: Vec<ResultUndo>,
    undo_bytes: usize,
    /// Where each hunk was drawn, as (top, height) relative to the start of the
    /// diff's content, so a take can put the viewport back where it was.
    /// Set when git launched the app to resolve a conflict, which changes two
    /// things: where a save goes, and what the process exit means.
    mergetool: Option<MergeTool>,
    hunk_boxes: Vec<(f32, f32)>,
    hunk_cursor: usize,
    /// A jump asked for by the keyboard, waiting for the frame that knows how
    /// many differences there are and where they were drawn.
    pending_hunk_move: isize,
    pending_find_move: isize,
    show_find: bool,
    find_query: String,
    find_cursor: usize,
    focus_find: bool,
    find_jump: bool,
    /// The diff's scroll offset as of last frame; where to put it back once the
    /// render a take asked for has landed; and where to put it back now.
    diff_offset: f32,
    pending_offset: Option<f32>,
    restore_offset: Option<f32>,
    /// A close request held back while the result holds unsaved work, and the
    /// answer once it has been given.
    quit_guard: bool,
    /// Whether the platform menu's Quit has been pointed at the window rather
    /// than at the process. Retried each frame until it takes, because the menu
    /// winit builds may not exist on the first one. See [`crate::menu`].
    quit_menu_guarded: bool,
    closing: bool,
    destructive: Option<DestructiveAction>,
    focus_panel: Option<usize>,
    /// One-shot requests that keep a keyboard-selected source card and
    /// candidate tab visible inside their independent horizontal scroll areas.
    reveal_panel: Option<usize>,
    reveal_pair: Option<usize>,
    /// Stable editor to restore after a destructive confirmation closes.
    /// Popup items disappear before the modal is drawn, so the currently
    /// focused id is not necessarily a valid restoration target.
    destructive_restore_focus: Option<egui::Id>,
    /// The active modal, the control that had focus before it opened, and the
    /// modal's safe default control. egui traps focus to a modal layer, but it
    /// does not choose or restore a meaningful control for us.
    modal_focus: Option<(egui::Id, Option<egui::Id>, egui::Id)>,
    /// Focus below a modal is still disabled for the pass in which that modal
    /// disappears. Restore on the following pass, after the old modal layer has
    /// been retired, rather than requesting an id egui cannot yet register.
    pending_modal_focus_restore: Option<(egui::Id, u64)>,
    /// A short confirmation next to the result's own controls -- "Copied",
    /// "Saved to …". Not a banner: these answer a button that was just pressed,
    /// and a dismissible strip across the diff is too much furniture for that.
    flash: Option<(String, Instant)>,
    tx: Sender<Job>,
    rx: Receiver<Job>,
}

/// Every label the settings drawer draws through [`ui::field`], as a set --
/// "Theme" labels both the app theme and the syntax theme. [`ui::field_column`]
/// sizes the label column from these and [`ui::field`] asserts that the label it
/// was handed is one of them, so a new field that is not added here fails
/// `settings_drawer_stays_inside_its_supported_narrow_width`.
const SETTINGS_FIELDS: &[&str] = &[
    "Theme",
    "Interface",
    "Interface size",
    "Diff",
    "Diff size",
    "Show",
    "Highlight",
    "Whitespace",
    "Ignore lines matching",
];

/// The width at which the drawer's label column and a usable control fit side
/// by side -- which is the whole reason the column is measured per drawer.
///
/// Measured, not guessed, and pinned by
/// `the_settings_drawer_aligns_its_fields_at_its_default_width`: at 13 pt in the
/// system UI font "Ignore lines matching" shapes to 165, plus 8 of gap and the
/// 180 the size sliders need is 353 of content; the drawer's own scroll
/// allowance is 6 and the frame's symmetric margin is 32. An earlier 360 was
/// arithmetic on a 124 pt guess at that label, and left the drawer stacked at
/// its own default width. A larger UI font stacks the whole drawer, as one,
/// which is the point of deciding it per drawer rather than per row.
const SETTINGS_DRAWER_WIDTH: f32 = 400.0;

impl App {
    pub fn new(delta: Delta, mut settings: Settings, launch: Launch) -> Self {
        let Launch {
            preload,
            files,
            watch,
            combine,
            mergetool,
            hotkey,
        } = launch;
        let (tx, rx) = channel();
        let mut panels = vec![Panel::empty(), Panel::empty()];
        let mut error = None;
        if let Some(text) = preload {
            panels[0].text = text;
        }
        // A preloaded clipboard occupies panel A, so files fill from whichever
        // panels are still empty, adding panels when more than two arrive.
        //
        // Git's three files go in by position instead. An ancestor that is an
        // empty file is an ordinary conflict -- both sides added the file -- and
        // it would otherwise read as an empty panel for the next file to claim,
        // putting "ours" where the ancestor belongs.
        for (n, f) in files.iter().enumerate() {
            let slot = if mergetool.is_some() {
                while panels.len() <= n {
                    panels.push(Panel::empty());
                }
                n
            } else {
                match panels.iter().position(Panel::is_empty) {
                    Some(i) => i,
                    None if panels.len() < MAX_PANELS => {
                        panels.push(Panel::empty());
                        panels.len() - 1
                    }
                    None => break,
                }
            };
            if let Err(e) = panels[slot].bind(f.clone()) {
                error.get_or_insert(e);
            }
            panels[slot].watch = watch;
        }
        for p in panels.iter_mut() {
            p.resniff();
        }
        // An explicitly opened empty file is still an input. Only untouched
        // paste slots wait for the first comparison request.
        let compared = panels.iter().take(2).all(|p| !p.is_empty());
        let themes = delta.syntax_themes();
        let syntax_theme_reset = settings.sanitise_syntax_theme(&themes);
        let gitconfig = config::discover(None);
        let available_features: BTreeSet<_> = gitconfig.selectable_features().into_iter().collect();
        let (features, features_reset) = if settings.inherit_gitconfig {
            settings.features.as_ref().map_or_else(
                || (gitconfig.active.iter().cloned().collect(), false),
                |saved| {
                    let selected: BTreeSet<_> = saved
                        .iter()
                        .filter(|name| available_features.contains(*name))
                        .cloned()
                        .collect();
                    (
                        selected,
                        saved.iter().any(|name| !available_features.contains(name)),
                    )
                },
            )
        } else {
            (BTreeSet::new(), false)
        };
        let installed = (
            settings.ui_font.clone(),
            settings.ui_font_strong.clone(),
            settings.mono_font.clone(),
            settings.ui_pt,
            settings.mono_pt,
        );
        let mut startup_notices = Vec::new();
        if syntax_theme_reset {
            startup_notices.push(if settings.inherit_gitconfig {
                "The saved delta syntax theme is no longer available, so delgui returned to your gitconfig."
            } else {
                "The saved delta syntax theme is no longer available, so delgui returned to delta's default."
            });
        }
        if features_reset {
            startup_notices.push(
                "A saved delta feature is no longer defined by your gitconfig, so it was disabled.",
            );
        }
        let notice = (!startup_notices.is_empty()).then(|| startup_notices.join(" "));

        // Enumerating installed fonts takes a few hundred milliseconds warm and
        // considerably longer cold. It fills a picker, so it has no business
        // holding up the first frame.
        let (font_tx, catalog_rx) = channel();
        std::thread::spawn(move || {
            let _ = font_tx.send(fonts::scan());
        });
        let show_settings = settings.settings_open;
        // The empty launch says the panels are ready to paste into, so make
        // that true without requiring a preparatory click. A launch carrying
        // files or text keeps focus neutral, and an open Settings drawer keeps
        // focus with the controls the user left visible.
        let focus_panel =
            (!show_settings && panels.iter().all(Panel::is_empty)).then_some(0);

        let mut app = Self {
            delta,
            panels,
            reference: 0,
            shown: 1,
            focused: 0,
            opts: Options {
                side_by_side: settings.side_by_side,
                line_numbers: settings.line_numbers,
                wrap: settings.wrap,
                hunk_headers: settings.hunk_headers,
                whitespace: settings.whitespace,
                granularity: settings.granularity,
                ignore_blank_lines: settings.ignore_blank_lines,
                ignore_cr_at_eol: settings.ignore_cr_at_eol,
                syntax_theme: settings.syntax_theme.clone(),
                inherit_gitconfig: settings.inherit_gitconfig,
                ..Options::default()
            },
            installed,
            catalog: Vec::new(),
            catalog_rx: Some(catalog_rx),
            probe: None,
            settings,
            themes,
            gitconfig,
            features,
            show_settings,
            show_help: false,
            cache: HashMap::new(),
            prepared: None,
            in_flight: None,
            failed: None,
            error,
            notice,
            columns: 120,
            pending_resize: None,
            pending_edit: None,
            pending_reload: None,
            watcher: None,
            hotkey,
            panel_rects: Vec::new(),
            compared,
            revision: 0,
            requested: false,
            building_result: false,
            result_generation: 0,
            undo: VecDeque::new(),
            redo: Vec::new(),
            undo_bytes: 0,
            hunk_boxes: Vec::new(),
            hunk_cursor: 0,
            pending_hunk_move: 0,
            pending_find_move: 0,
            show_find: false,
            find_query: String::new(),
            find_cursor: 0,
            focus_find: false,
            find_jump: false,
            diff_offset: 0.0,
            pending_offset: None,
            restore_offset: None,
            quit_guard: false,
            quit_menu_guarded: false,
            closing: false,
            destructive: None,
            focus_panel,
            reveal_panel: None,
            reveal_pair: None,
            destructive_restore_focus: None,
            modal_focus: None,
            pending_modal_focus_restore: None,
            flash: None,
            mergetool,
            tx,
            rx,
        };
        if combine || app.mergetool.is_some() {
            // Seeded from the first panel, which is the one `--combine a.rs b.rs`
            // names first and the likeliest starting point -- and, under
            // `--mergetool`, git's common ancestor. Starting from the ancestor
            // is what makes each side's changes a difference to take; starting
            // from git's own half-merged file would mean diffing against its
            // conflict markers.
            app.start_result(Some(0));
        }
        if let Some(tool) = &app.mergetool {
            app.notice = Some(format!(
                "Resolving {} for git. {} writes the merge back and closes the question; \
                 quitting without it tells git the conflict is unresolved.",
                tool.merged.display(),
                keys::save_label(),
            ));
        }
        app
    }

    // ---- comparison ------------------------------------------------------

    /// Resolve the syntax to hand delta, in order of how much we trust it.
    ///
    /// delta infers from the right-hand path only, so the shown panel is
    /// consulted first: whatever it says is what delta would conclude alone.
    fn resolve_language(&self) -> Option<String> {
        self.panels
            .get(self.shown)
            .and_then(Panel::language_hint)
            .or_else(|| self.panels[self.reference].language_hint())
    }

    /// Whether the diff on screen is "my result against a candidate".
    ///
    /// Merge mode is not a flag of its own: it *is* the baseline being a result
    /// panel. Clicking another panel's letter chip leaves it and clicking the
    /// result's letter returns, with no second piece of state to get out of step.
    fn merging(&self) -> bool {
        self.building_result && self.panels.get(self.reference).is_some_and(|p| p.result)
    }

    fn result_panel(&self) -> Option<usize> {
        self.panels.iter().position(|p| p.result)
    }

    fn panel_label(&self, i: usize) -> String {
        let panel = &self.panels[i];
        let name = panel.name();
        let duplicate = panel.path.is_some()
            && self
                .panels
                .iter()
                .enumerate()
                .any(|(j, other)| j != i && other.path.is_some() && other.name() == name);
        if duplicate {
            let parent = panel
                .path
                .as_deref()
                .and_then(Path::parent)
                .map_or_else(|| ".".into(), |p| p.display().to_string());
            format!("{name} — {parent}")
        } else {
            name
        }
    }

    fn effective_options(&self) -> Options {
        let merging = self.merging();
        Options {
            width: self.columns as u16,
            default_language: self.resolve_language(),
            features: self.features.iter().cloned().collect(),
            // Forced here rather than on `self.opts`, which `save` persists
            // verbatim: building a result once would otherwise rewrite the
            // user's own toolbar defaults for good.
            // Always: the app locates every difference it draws, merging or
            // not, and `--hunk-label` is what makes that possible.
            marked_hunks: true,
            // Merge mode draws its own control row where the header was, and
            // delta's header decoration would land inside the hunk.
            hunk_headers: !merging && self.opts.hunk_headers,
            // Only merge mode: a take is a pair of line ranges, so it owns the
            // structure it splices from. A plain render leaves git's own
            // heuristics alone, because delta's two-file mode would have.
            pin_hunk_structure: merging,
            // Zero while merging, because at git's default of three a
            // thirteen-line file with four independent changes is one hunk -- a
            // single take for the whole file, which is no way to pick anything.
            context: if merging {
                0
            } else {
                self.settings.context.lines()
            },
            // Ignoring differences is a way of reading a diff, and merge mode is
            // not reading: a take splices a hunk's lines wholesale, and
            // `merge::verify` requires everything between the hunks to be
            // identical on both sides -- which is exactly what an ignore makes
            // false. Forced here rather than on `self.opts`, so that building a
            // result does not rewrite the setting.
            whitespace: if merging {
                Whitespace::Exact
            } else {
                self.opts.whitespace
            },
            ignore_blank_lines: !merging && self.opts.ignore_blank_lines,
            ignore_cr_at_eol: !merging && self.opts.ignore_cr_at_eol,
            ignore_matching: (!merging)
                .then(|| self.settings.ignore_matching.trim())
                .filter(|pattern| !pattern.is_empty())
                .map(str::to_owned),
            ..self.opts.clone()
        }
    }

    fn current_key(&self) -> RenderKey {
        RenderKey {
            revision: self.revision,
            reference: self.reference,
            shown: self.shown,
            reference_revision: self.panels[self.reference].revision,
            shown_revision: self.panels[self.shown].revision,
            args: self.effective_options().fingerprint(),
        }
    }

    /// Record that panel contents changed, invalidating cached diffs.
    fn touch(&mut self) {
        self.revision += 1;
        self.failed = None;
    }

    fn touch_panel(&mut self, i: usize) {
        if let Some(panel) = self.panels.get_mut(i) {
            panel.revision = panel.revision.wrapping_add(1);
        }
        self.failed = None;
    }

    /// Same, for a change that arrived one keystroke at a time.
    fn touch_edit(&mut self, i: usize) {
        self.touch_panel(i);
        self.pending_edit = Some(Instant::now());
    }

    fn pair(&self) -> (usize, usize) {
        (self.reference, self.shown)
    }

    /// Start a render if one is warranted and none is running.
    ///
    /// Single-flight on purpose: the previous shape spawned a thread per call
    /// site with a single `bool` to guard it, so holding ⌘Enter started twenty-five
    /// delta processes a second and whichever finished last won -- the visible
    /// diff could go backwards in time.
    fn schedule(&mut self, ctx: &egui::Context) {
        self.normalize();
        if !self.compared || self.in_flight.is_some() {
            return;
        }
        let key = self.current_key();
        if self.cache.get(&self.shown).is_some_and(|c| c.key == key) {
            self.requested = false;
            return; // already rendered for these inputs, flags and width
        }
        if self.failed.as_ref() == Some(&key) {
            self.requested = false;
            return; // it failed for exactly these inputs; wait to be asked again
        }
        let (reference, shown) = self.pair();
        for i in [reference, shown] {
            if self.panels[i].text.len() > MAX_PANEL_BYTES {
                self.error = Some(format!(
                    "Panel {} holds {:.1} MB. delgui stops at {} MB because delta takes \
                     about a second per megabyte and produces seven times its input in \
                     styled output.",
                    title(i),
                    self.panels[i].text.len() as f64 / 1e6,
                    MAX_PANEL_BYTES / 1_000_000
                ));
                self.failed = Some(key);
                return;
            }
        }
        self.error = None;

        let (delta, opts) = (self.delta.clone(), self.effective_options());
        let merging = self.merging();
        // A render is a snapshot of what the editors show. Passing live paths
        // lets the file change after the key is built but before delta opens it,
        // producing output that belongs to neither the panel nor the cache key.
        let (left, right) = (
            self.panels[reference].to_input(true),
            self.panels[shown].to_input(true),
        );
        let (tx, ctx, columns) = (self.tx.clone(), ctx.clone(), self.columns);
        let job_key = key.clone();
        eprintln!(
            "delgui: rendering panel {} against {} ({} bytes, {columns} columns)",
            title(shown),
            title(reference),
            self.pair_bytes(),
        );
        self.in_flight = Some(key);
        self.requested = false;
        std::thread::spawn(move || {
            let started = Instant::now();
            let job = match render_job(
                &delta,
                &left,
                &right,
                &opts,
                job_key.clone(),
                columns,
                merging,
            ) {
                Ok(cached) => Job::Done(shown, cached, started.elapsed()),
                Err(e) => Job::Failed(job_key, e.to_string(), started.elapsed()),
            };
            let _ = tx.send(job);
            ctx.request_repaint();
        });
    }

    /// Render now, whatever the cache thinks.
    ///
    /// Also re-reads the active pair: a file can change on disk without anything
    /// in the app noticing, but an unrelated broken panel must not block A vs B.
    fn compare_now(&mut self, ctx: &egui::Context) {
        let (reference, shown) = self.pair();
        for i in [reference, shown] {
            if !self.panels[i].edited && self.panels[i].path.is_some() {
                if let Err(e) = self.panels[i].reload() {
                    self.panels[i].disk_stale = true;
                    self.error = Some(e);
                    return;
                }
                self.touch_panel(i);
            }
        }
        // Automatic scheduling suppresses a key that has already failed so a
        // transient delta error cannot spin. Compare is the explicit retry
        // path, so it must be allowed to try that same key exactly once.
        self.failed = None;
        self.compared = true;
        self.pending_edit = None;
        self.requested = true;
        self.schedule(ctx);
    }

    fn poll(&mut self) {
        while let Ok(job) = self.rx.try_recv() {
            match job {
                Job::Done(panel, cached, elapsed) => {
                    if self.in_flight.as_ref() == Some(&cached.key) {
                        self.in_flight = None;
                    }
                    if panel == self.shown {
                        self.restore_offset = self.pending_offset.take();
                    }
                    // Raised here and not while drawing: a notice created in the
                    // draw path is recreated on the next frame, so dismissing it
                    // does nothing.
                    if let Some(problem) = &cached.problem {
                        self.notice = Some(problem.clone());
                    }
                    eprintln!(
                        "delgui: render completed in {} ms ({} rows, {} differences)",
                        elapsed.as_millis(),
                        cached.lines.len(),
                        cached.hunks.len(),
                    );
                    self.cache.insert(panel, cached);
                }
                Job::Failed(key, e, elapsed) => {
                    if self.in_flight.as_ref() == Some(&key) {
                        self.in_flight = None;
                    }
                    self.failed = Some(key);
                    eprintln!(
                        "delgui: render failed after {} ms; the previous diff was kept",
                        elapsed.as_millis(),
                    );
                    self.error = Some(e);
                }
            }
        }
    }

    fn shown_diff(&self) -> Option<&Cached> {
        self.cache.get(&self.shown)
    }

    /// Whether what is on screen was rendered from what is in the panels now.
    fn is_fresh(&self) -> bool {
        let (reference, shown) = self.pair();
        if self.panels[reference].disk_stale || self.panels[shown].disk_stale {
            return false;
        }
        let key = self.current_key();
        self.cache.get(&self.shown).is_some_and(|c| c.key == key)
    }

    fn auto_renders(&self) -> bool {
        self.pair_bytes() <= AUTO_RENDER_BYTES
    }

    fn pair_bytes(&self) -> usize {
        let (a, b) = self.pair();
        self.panels[a].text.len() + self.panels[b].text.len()
    }

    fn reports_no_differences(&self) -> bool {
        self.is_fresh()
            && self
                .shown_diff()
                .is_some_and(|cached| render::is_empty(&cached.lines))
    }

    /// Keep `reference`, `shown` and `focused` pointing at panels that exist,
    /// and keep the shown panel distinct from the reference.
    fn normalize(&mut self) {
        let n = self.panels.len();
        self.reference = self.reference.min(n - 1);
        self.focused = self.focused.min(n - 1);
        if self.shown >= n || self.shown == self.reference {
            self.shown = (0..n).find(|i| *i != self.reference).unwrap_or(0);
        }
    }

    // ---- panel management ------------------------------------------------

    fn panel_has_unsaved_content(&self, i: usize) -> bool {
        self.panels.get(i).is_some_and(|panel| {
            if panel.result {
                panel.dirty
                    || (panel.saved_to.is_some() && !self.result_last_save_is_current(i))
            } else if panel.path.is_some() {
                panel.edited
            } else {
                !panel.text.is_empty()
            }
        })
    }

    fn unsaved_panels(&self) -> Vec<usize> {
        (0..self.panels.len())
            .filter(|&i| self.panel_has_unsaved_content(i))
            .collect()
    }

    fn affected_panels(&self, action: &DestructiveAction) -> Vec<usize> {
        let mut affected = match action {
            DestructiveAction::LoadFiles(loads) => loads
                .iter()
                .filter_map(|(i, _)| (*i < self.panels.len()).then_some(*i))
                .collect(),
            DestructiveAction::Reload(i)
            | DestructiveAction::Clear(i)
            | DestructiveAction::Remove(i) => vec![*i],
            DestructiveAction::Reseed(_) => self.result_panel().into_iter().collect(),
        };
        affected.sort_unstable();
        affected.dedup();
        affected
    }

    fn request_destructive(&mut self, action: DestructiveAction) {
        let affected = self.affected_panels(&action);
        let loses_work = affected
            .iter()
            .any(|&i| self.panel_has_unsaved_content(i));
        if loses_work {
            // A confirmation often originates in a popup. Its focused menu
            // item vanishes when the popup closes, so restoring that id after
            // Cancel would only let egui's dead-man switch clear focus on the
            // next pass. Editors remain mounted and are a predictable fallback.
            self.destructive_restore_focus = match action {
                DestructiveAction::Remove(_) => Some(panel_edit_id(0)),
                _ => affected.first().map(|&i| {
                    if self.panels[i].result {
                        result_edit_id(i)
                    } else {
                        panel_edit_id(i)
                    }
                }),
            };
            self.destructive = Some(action);
        } else {
            self.destructive_restore_focus = None;
            self.apply_destructive(action);
        }
    }

    fn apply_destructive(&mut self, action: DestructiveAction) {
        match action {
            DestructiveAction::LoadFiles(loads) => {
                for (slot, file) in loads {
                    while slot >= self.panels.len() && self.panels.len() < MAX_PANELS {
                        self.panels.push(Panel::empty());
                    }
                    if slot < self.panels.len() {
                        self.open_into(slot, file);
                    }
                }
            }
            DestructiveAction::Reload(i) => self.reload_panel(i),
            DestructiveAction::Clear(i) => self.clear_panel(i),
            DestructiveAction::Remove(i) => self.remove_panel(i),
            DestructiveAction::Reseed(seed) => self.start_result(seed),
        }
    }

    fn modal_active(&self) -> bool {
        self.show_help || self.quit_guard || self.destructive.is_some()
    }

    fn enter_modal_focus(
        &mut self,
        ctx: &egui::Context,
        modal: egui::Id,
        safe_control: egui::Id,
        restore_focus: Option<egui::Id>,
    ) {
        if self.modal_focus.as_ref().is_some_and(|state| state.0 == modal) {
            return;
        }
        let pending_previous = self
            .pending_modal_focus_restore
            .take()
            .map(|(target, _)| target);
        let previous = self
            .modal_focus
            .take()
            .and_then(|(_, previous, _)| previous)
            .or(restore_focus)
            .or(pending_previous)
            .or_else(|| ctx.memory(|memory| memory.focused()));
        ctx.memory_mut(|memory| memory.request_focus(safe_control));
        self.modal_focus = Some((modal, previous, safe_control));
    }

    fn leave_modal_focus(&mut self, ctx: &egui::Context, modal: egui::Id) {
        let Some((active, previous, safe_control)) = self.modal_focus else {
            return;
        };
        if active != modal {
            return;
        }
        self.modal_focus = None;
        ctx.memory_mut(|memory| memory.surrender_focus(safe_control));
        self.pending_modal_focus_restore =
            previous.map(|previous| (previous, ctx.cumulative_pass_nr()));
    }

    fn restore_pending_modal_focus(&mut self, ctx: &egui::Context) {
        if self.modal_active() {
            return;
        }
        if let Some((target, closed_at)) = self.pending_modal_focus_restore
            && ctx.cumulative_pass_nr() > closed_at
        {
            self.pending_modal_focus_restore = None;
            ctx.memory_mut(|memory| memory.request_focus(target));
        }
    }

    fn mark_modal(
        ctx: &egui::Context,
        response: &egui::Response,
        role: egui::accesskit::Role,
        label: &'static str,
    ) {
        ctx.accesskit_node_builder(response.id, |node| {
            node.set_role(role);
            node.set_label(label);
            node.set_modal();
            node.set_bounds(egui::accesskit::Rect {
                x0: f64::from(response.rect.left()),
                y0: f64::from(response.rect.top()),
                x1: f64::from(response.rect.right()),
                y1: f64::from(response.rect.bottom()),
            });
        });
    }

    fn modal_dimensions(ctx: &egui::Context) -> (f32, f32) {
        let content = ctx.content_rect().size();
        (
            (content.x - 64.0).clamp(280.0, 420.0),
            // Leave room for the frame outside the inner Ui. Keeping this
            // budget independent of the variable body is what keeps the safe
            // footer on-screen at the supported 720×480 / 20 pt combination.
            (content.y - 112.0).clamp(180.0, 620.0),
        )
    }

    fn mark_scroll_view(
        ctx: &egui::Context,
        id: egui::Id,
        label: &'static str,
        bounds: Option<egui::Rect>,
    ) {
        ctx.accesskit_node_builder(id, |node| {
            node.set_role(egui::accesskit::Role::ScrollView);
            node.set_label(label);
            if let Some(bounds) = bounds {
                node.set_bounds(egui::accesskit::Rect {
                    x0: f64::from(bounds.left()),
                    y0: f64::from(bounds.top()),
                    x1: f64::from(bounds.right()),
                    y1: f64::from(bounds.bottom()),
                });
            }
        });
    }

    fn add_panel(&mut self) {
        if self.panels.len() < MAX_PANELS {
            self.panels.push(Panel::empty());
            self.focused = self.panels.len() - 1;
            self.focus_panel = Some(self.focused);
            self.reveal_panel = Some(self.focused);
        }
    }

    fn clear_panel(&mut self, i: usize) {
        let Some(panel) = self.panels.get_mut(i) else {
            return;
        };
        let result = panel.result;
        panel.clear();
        if result {
            panel.dirty = true;
            self.clear_result_history();
        }
        self.cache.remove(&i);
        self.touch_panel(i);
        self.focus_panel = Some(i);
    }

    fn remove_panel(&mut self, i: usize) {
        if self.panels.len() <= 2 {
            return;
        }
        let removed_result = self.panels.get(i).is_some_and(|p| p.result);
        self.panels.remove(i);
        if removed_result {
            self.building_result = false;
            self.begin_result_generation();
        }
        // Cached diffs are keyed by index, which the removal has shifted.
        self.cache.clear();
        self.touch();
        // `>=`, not `>`: removing the baseline itself used to leave `reference`
        // pointing at whatever slid into that slot, so an unrelated panel
        // silently became the baseline. Same for the other two.
        for pointer in [&mut self.reference, &mut self.shown, &mut self.focused] {
            if *pointer >= i && *pointer > 0 {
                *pointer -= 1;
            }
        }
        self.normalize();
        self.focus_panel = Some(self.focused);
    }

    fn set_reference(&mut self, i: usize) {
        self.reference = i;
        self.cache.clear();
        self.touch();
        self.normalize();
    }

    /// Compare the same two panels the other way round.
    ///
    /// Which side a panel is on is the whole meaning of red and green: the
    /// baseline is "before", so `-` is what the baseline has and the shown panel
    /// does not. Reading the same pair as an undo of itself is an ordinary
    /// thing to want, and this is the one action that says so.
    ///
    /// It exchanges the two rather than adding a "reversed" flag beside them,
    /// because a second notion of direction is a second thing to keep in step:
    /// `RenderKey`, the language delta infers from the right-hand path, the pair
    /// strip's own labels and merge mode's line ranges all read `reference` and
    /// `shown` already, and all four would have had to learn about the flag.
    ///
    /// `MakeReference` is not this. It promotes the shown panel and lets
    /// `normalize` pick whatever is left, which with more than two panels is the
    /// first other one -- not the panel that was the baseline a moment ago.
    fn swap_sides(&mut self) {
        // The baseline is the result being built, and its candidates are what
        // it is built from; there is no other way round for that to be.
        if self.merging() {
            return;
        }
        let (reference, shown) = self.pair();
        if reference == shown {
            return;
        }
        // Assigned before `set_reference`, whose `normalize` would otherwise see
        // `shown == reference` and pick a third panel instead of this one.
        self.shown = reference;
        self.set_reference(shown);
    }

    fn paste_into_new_panel(&mut self) {
        let Some(text) = crate::clipboard_text() else {
            self.notice = Some(
                "The clipboard holds no text. delgui compares text; images and files \
                 have to be dropped onto a panel instead."
                    .into(),
            );
            return;
        };
        let slot = match self.panels.iter().position(Panel::is_empty) {
            Some(i) => i,
            None if self.panels.len() < MAX_PANELS => {
                self.panels.push(Panel::empty());
                self.panels.len() - 1
            }
            // Never destroy: the hotkey fires from other applications, where
            // overwriting a panel would happen out of sight.
            None => {
                self.notice = Some(format!(
                    "All {MAX_PANELS} panels are full. Clear one from its ⋯ menu, or remove a panel."
                ));
                return;
            }
        };
        self.panels[slot].clear();
        self.panels[slot].text = text;
        self.panels[slot].resniff();
        self.touch_panel(slot);
        self.shown = slot;
        self.focused = slot;
        self.focus_panel = Some(slot);
        self.compared = true;
    }

    // ---- building a result -----------------------------------------------

    /// Start, or re-seed, the result panel.
    ///
    /// It becomes the baseline, which is what puts the take controls on screen:
    /// every diff then reads "my result against a candidate". The shown tab moves
    /// to one that actually differs -- decided by comparing the text, since
    /// nothing has been rendered against it yet.
    fn start_result(&mut self, seed: Option<usize>) {
        let (slot, created) = match self.result_panel() {
            Some(existing) => (existing, false),
            None if self.panels.len() < MAX_PANELS => {
                self.panels.push(Panel::empty());
                (self.panels.len() - 1, true)
            }
            None => {
                self.notice = Some(format!(
                    "All {MAX_PANELS} panels are in use. Remove one from its ⋯ menu first."
                ));
                return;
            }
        };
        let (text, language) = match seed {
            Some(i) => (self.panels[i].text.clone(), self.panels[i].language.clone()),
            None => (String::new(), None),
        };
        // A newly created or explicitly re-seeded result is a new lifecycle.
        // Old take snapshots must never be able to write into it.
        self.begin_result_generation();
        let panel = &mut self.panels[slot];
        panel.clear();
        panel.result = true;
        panel.dirty = !text.is_empty();
        panel.text = text;
        panel.language = language;
        panel.resniff();
        self.touch_panel(slot);
        self.building_result = true;
        self.compared = true;
        self.set_reference(slot);
        if let Some(i) = (0..self.panels.len())
            .find(|&i| i != slot && self.panels[i].text != self.panels[slot].text)
        {
            self.shown = i;
        }
        self.focused = slot;
        if created {
            self.focus_panel = Some(slot);
        }
        self.requested = true;
    }

    /// Leave merge mode, keeping the text.
    ///
    /// Not "remove the panel": `remove_panel` refuses below two panels, so with a
    /// result and one candidate the advertised way out would be greyed out
    /// exactly when it is wanted.
    fn stop_building(&mut self) {
        if self.result_panel().is_some() {
            self.building_result = false;
            self.cache.clear();
            self.touch();
        }
    }

    fn resume_result(&mut self, i: usize) {
        if self.panels.get(i).is_some_and(|p| p.result) {
            self.building_result = true;
            self.set_reference(i);
        }
    }

    /// Write a candidate's version of one difference into the result.
    fn take_hunk(&mut self, index: usize, ctx: &egui::Context) {
        let (reference, shown) = self.pair();
        let Some(hunk) = self
            .cache
            .get(&shown)
            .and_then(|c| c.hunks.get(index))
            .map(|(h, _)| h.clone())
        else {
            return;
        };
        let Some(text) = merge::take(
            &self.panels[reference].text,
            &self.panels[shown].text,
            &hunk,
        ) else {
            self.notice = Some(
                "That difference no longer describes what these panels hold. Comparing again \
                 brings the list up to date."
                    .into(),
            );
            return;
        };
        // Everything above the taken rows survives the re-render unchanged, so
        // the viewport only has to move when the take was above it. Without this
        // the content slides up under a fixed pixel offset and the next
        // difference moves out from under the pointer, on every single take.
        if let Some(&(top, height)) = self.hunk_boxes.get(index)
            && self.diff_offset > top
        {
            // Held until the render lands rather than applied now: until then the
            // rows on screen are still the old ones, and moving the viewport
            // against them would be a visible jump in the wrong direction.
            self.pending_offset = Some((self.diff_offset - height).max(top));
        }
        self.push_undo(reference, text.clone());
        self.write_result(reference, text, ctx);
        self.requested = true;
        self.schedule(ctx);
    }

    /// Replace the result's text from outside its own text field.
    fn write_result(&mut self, i: usize, text: String, ctx: &egui::Context) {
        self.panels[i].text = text;
        self.panels[i].resniff();
        self.panels[i].dirty = true;
        // `edited` as well: `compare_now` re-reads every panel that is not
        // edited, so on a saved result the next ⌘Enter would read the take straight
        // back off disk.
        self.panels[i].edited = self.panels[i].path.is_some();
        forget_text_undo(ctx, result_edit_id(i));
        self.touch_panel(i);
        self.pending_edit = None;
    }

    fn result_edited(&mut self, i: usize) {
        self.panels[i].resniff();
        self.panels[i].edited = self.panels[i].path.is_some();
        self.panels[i].dirty = true;
        self.clear_result_history();
        self.touch_edit(i);
    }

    /// Remember the result's text, so a take or a re-seed can be undone.
    fn push_undo(&mut self, i: usize, after: String) {
        let before = self.panels[i].text.clone();
        self.undo_bytes += before.len() + after.len();
        self.undo.push_back(ResultUndo {
            generation: self.result_generation,
            before,
            after,
        });
        self.redo.clear();
        while self.undo.len() > UNDO_DEPTH || self.undo_bytes > UNDO_BYTES {
            match self.undo.pop_front() {
                Some(dropped) => self.undo_bytes -= dropped.before.len() + dropped.after.len(),
                None => break,
            }
        }
    }

    fn clear_result_history(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.undo_bytes = 0;
    }

    fn begin_result_generation(&mut self) {
        self.result_generation = self.result_generation.wrapping_add(1);
        self.clear_result_history();
    }

    fn undo_take(&mut self, ctx: &egui::Context) {
        let Some(i) = self.result_panel() else { return };
        let Some(entry) = self.undo.pop_back() else {
            return;
        };
        self.undo_bytes -= entry.before.len() + entry.after.len();
        if entry.generation != self.result_generation || self.panels[i].text != entry.after {
            self.clear_result_history();
            self.notice = Some(
                "Undo was cleared because the result changed after that take. Your newer edits were kept."
                    .into(),
            );
            return;
        }
        let before = entry.before.clone();
        self.write_result(i, before, ctx);
        self.redo.push(entry);
        self.requested = true;
        self.schedule(ctx);
    }

    fn redo_take(&mut self, ctx: &egui::Context) {
        let Some(i) = self.result_panel() else { return };
        let Some(entry) = self.redo.pop() else { return };
        if entry.generation != self.result_generation || self.panels[i].text != entry.before {
            self.clear_result_history();
            self.notice = Some(
                "Redo was cleared because the result changed. Your newer edits were kept.".into(),
            );
            return;
        }
        self.write_result(i, entry.after.clone(), ctx);
        self.undo_bytes += entry.before.len() + entry.after.len();
        self.undo.push_back(entry);
        self.requested = true;
        self.schedule(ctx);
    }

    fn copy_result(&mut self, ctx: &egui::Context) {
        let Some(i) = self.result_panel() else { return };
        ctx.copy_text(self.panels[i].text.clone());
        self.flash = Some(("Copied".into(), Instant::now()));
    }

    /// The destination a plain Save should use without opening a dialog.
    fn result_save_target(&self, i: usize) -> Option<PathBuf> {
        self.mergetool
            .as_ref()
            .map(|tool| tool.merged.clone())
            .or_else(|| self.panels[i].saved_to.clone())
    }

    /// Write the result to a file. The only thing in delgui that writes one.
    fn save_result(&mut self, ask: bool) -> bool {
        let Some(i) = self.result_panel() else {
            self.notice = Some(
                "There is no result to save yet. Combine… under the diff starts one from any \
                 panel."
                    .into(),
            );
            return false;
        };
        // Git named the destination when it launched us, so the first ⌘S has
        // somewhere to go without asking. Save as… still asks, which is the way
        // out if the answer belongs somewhere else after all.
        let known = self.result_save_target(i);
        let path = match known {
            Some(p) if !ask => p,
            _ => match rfd::FileDialog::new()
                .set_file_name(self.suggested_file_name())
                .save_file()
            {
                Some(p) => p,
                None => return false,
            },
        };
        // Saving over an input would leave that panel showing text the file no
        // longer holds, and take the difference it was there to show with it.
        if let Some(j) = self.panels.iter().position(|p| {
            p.path
                .as_ref()
                .is_some_and(|input| paths_refer_to_same_file(input, &path))
        }) {
            self.error = Some(format!(
                "{} is open in panel {}. Saving over it would replace what you are comparing \
                 against — choose another name.",
                path.display(),
                title(j)
            ));
            return false;
        }
        self.save_result_to(i, path, ask)
    }

    /// Write to a destination selected by `save_result`.
    ///
    /// `explicit_destination` means the native Save As dialog chose the path.
    /// In mergetool mode a different explicit destination is an export: Git's
    /// `MERGED` file remains the canonical Save target and the result stays
    /// dirty relative to it.
    fn save_result_to(
        &mut self,
        i: usize,
        path: PathBuf,
        explicit_destination: bool,
    ) -> bool {
        let export_only = self.mergetool.as_ref().is_some_and(|tool| {
            explicit_destination && !paths_refer_to_same_file(&path, &tool.merged)
        });
        let conflict_recovery = if self.mergetool.is_some() {
            "Use Export copy… to preserve both versions, or choose Git's exact target there to confirm replacing it."
        } else {
            "Use Save as… to choose or confirm a destination."
        };

        // A native Save As dialog owns overwrite confirmation. A later plain
        // Save does not, so compare the destination with the exact bytes this
        // process last put there before replacing anything.
        if !explicit_destination
            && self.panels[i]
                .saved_to
                .as_deref()
                .is_some_and(|saved| paths_refer_to_same_file(saved, &path))
            && let Some(snapshot) = self.panels[i].saved_snapshot.as_deref()
        {
            match std::fs::read(&path) {
                Ok(current) if current == snapshot.as_bytes() => {}
                Ok(_) => {
                    self.error = Some(format!(
                        "{} changed outside delgui after the last save. Nothing was overwritten. {conflict_recovery}",
                        path.display(),
                    ));
                    return false;
                }
                Err(e) => {
                    self.error = Some(format!(
                        "Could not verify {} before saving: {e}. Nothing was overwritten. {conflict_recovery}",
                        path.display(),
                    ));
                    return false;
                }
            }
        }

        match atomic_write(&path, self.panels[i].text.as_bytes()) {
            Ok(()) => {
                if export_only {
                    eprintln!(
                        "delgui: exported a result copy ({} bytes); the canonical target is unchanged",
                        self.panels[i].text.len(),
                    );
                    self.flash = Some((
                        format!("Exported a copy to {}", path.display()),
                        Instant::now(),
                    ));
                } else {
                    eprintln!(
                        "delgui: saved the result ({} bytes)",
                        self.panels[i].text.len(),
                    );
                    self.flash = Some((format!("Saved to {}", path.display()), Instant::now()));
                    self.panels[i].saved_stamp = saved_file_stamp(&path);
                    self.panels[i].saved_to = Some(path);
                    self.panels[i].saved_snapshot = Some(self.panels[i].text.clone());
                    self.panels[i].dirty = false;
                }
                // Saving and closing can happen in one frame. Publish here so
                // Git sees the write even when there is no next frame.
                self.publish_resolution();
                true
            }
            // Reported for the same reason a failed read is: a save that
            // silently did not happen is worse than no save button at all.
            Err(e) => {
                self.error = Some(format!("Could not write {}: {e}", path.display()));
                false
            }
        }
    }

    /// `config_after.merged.rs` -- never one of the inputs' own names.
    fn suggested_file_name(&self) -> String {
        let source = self
            .panels
            .get(self.shown)
            .and_then(|p| p.path.as_ref())
            .or_else(|| self.panels.iter().find_map(|p| p.path.as_ref()));
        let Some(path) = source else {
            return "result.txt".into();
        };
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "result".into());
        match path.extension() {
            Some(ext) => format!("{stem}.merged.{}", ext.to_string_lossy()),
            None => format!("{stem}.merged"),
        }
    }

    // ---- files -----------------------------------------------------------

    fn can_open_in_panel(&mut self, i: usize) -> bool {
        if self.panels[i].result {
            self.notice = Some(
                "Open files in an input panel. Select an input's comparison tab, or add a panel, then open the file."
                    .into(),
            );
            return false;
        }
        true
    }

    fn open_into(&mut self, i: usize, path: PathBuf) {
        if !self.can_open_in_panel(i) {
            return;
        }
        match self.panels[i].bind(path) {
            Ok(()) => {
                self.cache.remove(&i);
                self.touch_panel(i);
                self.compared = true;
                self.focused = i;
                self.focus_panel = Some(i);
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn choose_file_for_panel(&mut self, i: usize) {
        if !self.can_open_in_panel(i) {
            return;
        }
        if let Some(path) = rfd::FileDialog::new().pick_file() {
            self.request_destructive(DestructiveAction::LoadFiles(vec![(i, path)]));
        }
    }

    /// Reload a panel through the user-facing action contract: a failed read
    /// leaves the buffer and render revision alone, and explains why.
    fn reload_panel(&mut self, i: usize) {
        match self.panels[i].reload() {
            Ok(()) => {
                self.touch_panel(i);
                self.focus_panel = Some(i);
            }
            Err(e) => {
                self.panels[i].disk_stale = true;
                self.error = Some(format!("{e} The previous snapshot is still shown."));
            }
        }
    }

    /// Bring the watcher in line with the panels' `watch` flags.
    fn sync_watches(&mut self, ctx: &egui::Context) {
        let wanted: Vec<PathBuf> = self
            .panels
            .iter()
            .filter(|p| p.watch)
            .filter_map(|p| p.path.clone())
            .collect();
        if wanted.is_empty() && self.watcher.is_none() {
            return;
        }
        if self.watcher.is_none() {
            let ctx = ctx.clone();
            match FileWatcher::new(move || ctx.request_repaint()) {
                Ok(w) => self.watcher = Some(w),
                Err(e) => {
                    self.notice = Some(format!("Could not start file watching: {e}"));
                    for p in self.panels.iter_mut() {
                        p.watch = false;
                    }
                    return;
                }
            }
        }
        let failed = {
            let watcher = self.watcher.as_mut().expect("just constructed");
            // Reconciled against the watcher's own list, not the panels': a panel
            // that has since dropped its path cannot tell us what to stop watching,
            // and a stale watch on a busy directory wakes the UI forever.
            for path in watcher.watched() {
                if !wanted.contains(&path) {
                    watcher.unwatch(&path);
                }
            }

            let mut failed = Vec::new();
            for path in &wanted {
                if !watcher.is_watching(path)
                    && let Err(e) = watcher.watch(path)
                {
                    failed.push((path.clone(), e.to_string()));
                }
            }
            failed
        };
        for (path, error) in failed {
            // Clearing the request is deliberate. Otherwise every repaint
            // retries the same failed registration and recreates the notice
            // immediately after it is dismissed. Re-checking the menu item is
            // the explicit retry path.
            for panel in self.panels.iter_mut() {
                if panel.path.as_ref() == Some(&path) {
                    panel.watch = false;
                }
            }
            self.notice = Some(format!("Cannot watch {}: {error}", path.display()));
        }
    }

    fn poll_watches(&mut self) {
        let (changed, backend_error) = {
            let Some(watcher) = self.watcher.as_mut() else {
                return;
            };
            let changed = watcher.poll();
            let errors = watcher.poll_errors();
            let backend_error = errors.last().map(|latest| {
                let global = errors.iter().any(|error| error.paths.is_empty());
                let affected = if global {
                    Vec::new()
                } else {
                    errors
                        .iter()
                        .flat_map(|error| error.paths.iter().cloned())
                        .collect()
                };
                (latest.to_string(), affected)
            });
            (changed, backend_error)
        };
        if let Some((error, affected)) = backend_error {
            self.handle_watch_backend_error(&error, &affected);
        }
        if changed.is_empty() {
            return;
        }
        let relevant = self
            .panels
            .iter()
            .any(|p| p.watch && p.path.as_ref().is_some_and(|q| changed.contains(q)));
        if relevant {
            self.pending_reload = Some(Instant::now());
        }
    }

    fn handle_watch_backend_error(&mut self, error: &str, affected: &[PathBuf]) {
        let mut marked = 0;
        for panel in &mut self.panels {
            let Some(path) = panel.path.as_deref() else {
                continue;
            };
            if panel.watch
                && !panel.edited
                && watch_error_affects(path, affected)
                && !panel.disk_stale
            {
                panel.disk_stale = true;
                marked += 1;
            }
        }
        eprintln!(
            "delgui: file watcher backend error; {marked} followed panel snapshot(s) marked stale"
        );
        self.error = Some(if marked == 0 {
            format!("File watching reported an error and may be out of date: {error}")
        } else {
            format!(
                "File watching reported an error. {marked} followed panel snapshot(s) may be out of date: {error} Reload from disk retries them."
            )
        });
    }

    /// Panels a drop would land in, in order. Empty means the pointer is not
    /// over a panel, and the drop is refused rather than guessed at.
    fn drop_targets(&self, ctx: &egui::Context, count: usize) -> Vec<usize> {
        let pos = ctx.input(|i| i.pointer.latest_pos());
        let Some(first) = pos.and_then(|p| self.panel_rects.iter().position(|r| r.contains(p)))
        else {
            return Vec::new();
        };
        // The result is skipped rather than counted: its rect is `NOTHING` so a
        // drop cannot start on it, but a run of files dropped on an earlier
        // panel would otherwise spill into it and overwrite the merge.
        (first..)
            .take_while(|i| *i < MAX_PANELS.max(self.panels.len()))
            .filter(|i| self.panels.get(*i).is_none_or(|p| !p.result))
            .take(count)
            .collect()
    }

    /// Route dropped files to the panels they landed on, spilling into the
    /// following panels when several arrive at once.
    fn accept_drops(&mut self, ctx: &egui::Context) {
        if self.modal_active() {
            return;
        }
        let files: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if files.is_empty() {
            return;
        }
        let targets = self.drop_targets(ctx, files.len());
        if targets.is_empty() {
            self.notice = Some("Drop files onto a panel to load them.".into());
            return;
        }
        let loads = targets.into_iter().zip(files).collect();
        self.request_destructive(DestructiveAction::LoadFiles(loads));
    }

    // ---- input -----------------------------------------------------------

    fn take_view_control(&mut self) {
        if self.opts.inherit_gitconfig {
            self.opts.inherit_gitconfig = false;
            self.features.clear();
            self.notice = Some(
                "The view controls now override your gitconfig; config inheritance was turned off."
                    .into(),
            );
        }
    }

    fn capture_keys(&self, ctx: &egui::Context) -> Vec<Action> {
        if self.modal_active() {
            return Vec::new();
        }
        // egui's own undo fires only for the field that has focus, so ⌘Z over
        // the diff does nothing at all -- which is exactly where it gets pressed
        // after a take. Claiming it there and nowhere else is what keeps the two
        // undo histories from fighting over one buffer.
        let typing = ctx.text_edit_focused();
        let actions: Vec<_> = ctx.input(keys::pressed).into_iter().filter(|action| {
            !(typing && matches!(action, Action::UndoTake | Action::RedoTake))
        }).collect();
        for &action in &actions {
            ctx.input_mut(|input| keys::consume(input, action));
        }
        // Held Take is intentionally absent from actions, but its repeated
        // Enter must not activate the focused widget either.
        ctx.input_mut(|input| keys::consume(input, Action::TakeCurrentDifference));
        actions
    }

    fn handle_keys(&mut self, ctx: &egui::Context, actions: Vec<Action>) {
        if self.modal_active() {
            return;
        }
        let typing = ctx.text_edit_focused();
        for action in actions {
            match action {
                Action::OpenFile => self.choose_file_for_panel(self.shown),
                // One window, so quitting and closing it are the same request.
                // Both go through `close_requested`, which is the only thing
                // `quit_guard` can hook -- see the note on the ⌘Q binding.
                Action::CloseWindow | Action::Quit => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Action::Compare => self.compare_now(ctx),
                Action::AddPanel => self.add_panel(),
                Action::RemovePanel => {
                    let victim = self.shown;
                    self.request_destructive(DestructiveAction::Remove(victim));
                }
                Action::PasteIntoNewPanel => self.paste_into_new_panel(),
                Action::ShowDiff(i) => {
                    if i < self.panels.len() && i != self.reference {
                        self.shown = i;
                        self.reveal_panel = (!self.panels[i].result).then_some(i);
                        self.reveal_pair = Some(i);
                    }
                }
                Action::MakeReference => {
                    let shown = self.shown;
                    self.set_reference(shown);
                }
                Action::SwapSides => self.swap_sides(),
                Action::ToggleSideBySide => {
                    self.opts.side_by_side = !self.opts.side_by_side;
                    self.take_view_control();
                }
                Action::ToggleLineNumbers => {
                    self.opts.line_numbers = !self.opts.line_numbers;
                    self.take_view_control();
                }
                Action::ToggleWrap => {
                    self.opts.wrap = !self.opts.wrap;
                    self.take_view_control();
                }
                Action::ToggleSettings => self.show_settings = !self.show_settings,
                Action::ToggleHelp => self.show_help = !self.show_help,
                Action::Find => self.open_find(),
                // Walking the matches only makes sense once there is something
                // to walk; before that ⌘G is the same request as ⌘F.
                Action::NextMatch | Action::PreviousMatch => {
                    if self.find_query.is_empty() {
                        self.open_find();
                    } else {
                        self.show_find = true;
                        self.pending_find_move = if action == Action::NextMatch { 1 } else { -1 };
                    }
                }
                Action::NextChange => self.pending_hunk_move = 1,
                Action::PreviousChange => self.pending_hunk_move = -1,
                Action::SaveResult => {
                    let _ = self.save_result(false);
                }
                Action::SaveResultAs => {
                    let _ = self.save_result(true);
                }
                Action::TakeCurrentDifference => {
                    let hunk_count = self.shown_diff().map_or(0, |cached| cached.hunks.len());
                    if self.merging() && self.is_fresh() && hunk_count > 0 {
                        self.take_hunk(self.hunk_cursor.min(hunk_count - 1), ctx);
                    }
                }
                Action::UndoTake => {
                    if !typing {
                        self.undo_take(ctx);
                    }
                }
                Action::RedoTake => {
                    if !typing {
                        self.redo_take(ctx);
                    }
                }
            }
        }
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.show_help = false;
            self.show_find = false;
        }
    }

    /// Keep git's answer current every frame and immediately after a save.
    ///
    /// Not "did a save succeed once": the question git asks on exit is whether
    /// the file it handed over now holds the merge, and a save followed by more
    /// typing does not. Cheap enough to redo rather than track from six places
    /// -- the same reasoning `RenderKey` is rebuilt on.
    fn publish_resolution(&self) {
        let Some(tool) = &self.mergetool else {
            return;
        };
        let resolved = self
            .result_panel()
            .is_some_and(|i| self.result_is_saved_to(i, &tool.merged));
        tool.resolved.store(resolved, Ordering::Relaxed);
    }

    fn result_is_saved_to(&self, i: usize, target: &Path) -> bool {
        self.panels.get(i).is_some_and(|panel| {
            !panel.dirty
                && panel.saved_snapshot.is_some()
                && panel
                    .saved_stamp
                    .is_some_and(|stamp| saved_file_stamp(target) == Some(stamp))
                && panel
                    .saved_to
                    .as_deref()
                    .is_some_and(|at| paths_refer_to_same_file(at, target))
        })
    }

    fn result_last_save_is_current(&self, i: usize) -> bool {
        self.panels
            .get(i)
            .and_then(|panel| panel.saved_to.as_deref())
            .is_some_and(|target| self.result_is_saved_to(i, target))
    }

    fn result_bytes_are_saved_to(&self, i: usize, target: &Path) -> bool {
        self.panels.get(i).is_some_and(|panel| {
            !panel.dirty
                && panel
                    .saved_to
                    .as_deref()
                    .is_some_and(|at| paths_refer_to_same_file(at, target))
                && panel.saved_snapshot.as_deref().is_some_and(|snapshot| {
                    std::fs::read(target)
                        .ok()
                        .is_some_and(|bytes| bytes == snapshot.as_bytes())
                })
        })
    }

    /// Reconcile the cheap saved-file stamp with the exact bytes at the point
    /// where a false saved claim could lose the only remaining copy: close.
    /// Repainting only stats the file; closing may read the result once.
    fn refresh_saved_result_from_disk(&mut self) {
        let Some(i) = self.result_panel() else {
            return;
        };
        let Some(target) = self.panels[i].saved_to.clone() else {
            return;
        };
        if self.panels[i].dirty || self.panels[i].saved_snapshot.is_none() {
            return;
        }
        self.panels[i].saved_stamp = if self.result_bytes_are_saved_to(i, &target) {
            saved_file_stamp(&target)
        } else {
            None
        };
    }

    fn merge_unresolved(&self) -> bool {
        self.mergetool.as_ref().is_some_and(|tool| {
            !self
                .result_panel()
                .is_some_and(|i| self.result_is_saved_to(i, &tool.merged))
        })
    }

    #[cfg(test)]
    fn merge_unresolved_on_disk(&self) -> bool {
        self.mergetool.as_ref().is_some_and(|tool| {
            !self
                .result_panel()
                .is_some_and(|i| self.result_bytes_are_saved_to(i, &tool.merged))
        })
    }

    /// The find bar, and the reason it is not there if it cannot be.
    fn open_find(&mut self) {
        if self
            .cache
            .get(&self.shown)
            .is_some_and(|cached| !render::is_empty(&cached.lines))
        {
            self.show_find = true;
            self.focus_find = true;
        } else {
            self.notice = Some("There is no rendered diff to search yet.".into());
        }
    }

    fn poll_hotkey(&mut self, ctx: &egui::Context) {
        let fired = self.hotkey.as_ref().is_some_and(Hotkey::fired);
        if fired && !self.modal_active() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            self.paste_into_new_panel();
        }
    }

    // ---- appearance ------------------------------------------------------

    /// Bring egui and delta in line with the settings, and only when they have
    /// actually changed: rebuilding fonts throws away the whole glyph atlas.
    fn sync_appearance(&mut self, ctx: &egui::Context) {
        let want = (
            self.settings.ui_font.clone(),
            self.settings.ui_font_strong.clone(),
            self.settings.mono_font.clone(),
            self.settings.ui_pt,
            self.settings.mono_pt,
        );
        if self.installed != want {
            let faces_changed = (&self.installed.0, &self.installed.1, &self.installed.2)
                != (&want.0, &want.1, &want.2);
            if faces_changed {
                ctx.set_fonts(fonts::definitions(
                    want.0.as_ref(),
                    want.1.as_ref(),
                    want.2.as_ref(),
                ));
                ui::font_definitions_replaced(ctx);
                self.probe = None; // measure again once the new faces are live
            }
            ctx.all_styles_mut(|s| crate::theme::type_scale(s, want.3, want.4));
            self.installed = want;
        }
        ctx.set_theme(self.settings.theme.preference());
        // delta decides its own plus/minus backgrounds from a terminal query a
        // GUI cannot answer. Saying which mode we are in is what keeps a light
        // window from wrapping a dark diff.
        let appearance = if ctx.theme() == egui::Theme::Dark {
            Appearance::Dark
        } else {
            Appearance::Light
        };
        self.opts.appearance = Some(appearance);
    }

    fn poll_catalog(&mut self) {
        if let Some(rx) = &self.catalog_rx
            && let Ok(found) = rx.try_recv()
        {
            self.catalog = found;
            self.catalog_rx = None;
        }
    }

    fn mono_font(&self) -> egui::FontId {
        egui::FontId::new(self.settings.mono_pt, egui::FontFamily::Monospace)
    }

    // ---- drawing ---------------------------------------------------------

    fn toolbar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let t = ui::tokens(ui);
        let ready = {
            let (a, b) = self.pair();
            !self.panels[a].is_empty() || !self.panels[b].is_empty()
        };
        let busy = self.in_flight.is_some();
        let label = if busy { "Rendering…" } else { "Compare" };
        let can_add_panel = self.panels.len() < MAX_PANELS;
        let mut compare = false;
        let mut view_changed = false;
        let mut add_panel = false;
        let mut toggle_settings = false;
        let mut toggle_help = false;
        let strip_height = ui
            .spacing()
            .interact_size
            .y
            .max(ui.text_style_height(&TextStyle::Body) + 2.0 * ui.spacing().button_padding.y)
            // The mode rail adds two points of margin and one of border on
            // each side. Reserve it before laying out any toolbar control.
            + 6.0;

        // The fixed utilities are laid out first and reserve their width. The
        // modes own the remaining width and scroll only if a large interface
        // font makes their indivisible segmented control wider than that lane.
        // A wrapped row with a nested right-to-left group let both runs paint in
        // the same pixels at the supported 720 px / 20 pt combination.
        let opts = &mut self.opts;
        egui::containers::Sides::new()
            .shrink_left()
            .height(strip_height)
            .show(
                ui,
                |ui| {
                    // Ordinary horizontal starts at interact_size.y and cannot
                    // recenter earlier widgets when a later frame is taller.
                    ui.horizontal_centered(|ui| {
                        compare = ui::primary(
                            ui,
                            &t,
                            label,
                            keys::compare_label(),
                            ready && !busy,
                        );
                        ui.add_space(8.0);
                        egui::ScrollArea::horizontal()
                            .id_salt("toolbar-modes")
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.ctx().accesskit_node_builder(ui.unique_id(), |node| {
                                    node.set_role(egui::accesskit::Role::ScrollView);
                                    node.set_label("View modes");
                                });
                                ui.spacing_mut().interact_size.y = strip_height - 6.0;
                                view_changed = ui::segmented(
                                    ui,
                                    &t,
                                    &mut [
                                        ("Side by side", &mut opts.side_by_side),
                                        ("Numbers", &mut opts.line_numbers),
                                        ("Wrap", &mut opts.wrap),
                                    ],
                                );
                            });
                    });
                },
                |ui| {
                    // This lane lays out from the right edge. Emit its visual
                    // last item first so the established left-to-right order
                    // remains + Panel, Settings, Help.
                    if ui::icon(ui, "?", "Help", None)
                        .on_hover_text(keys::help_hint())
                        .clicked()
                    {
                        toggle_help = true;
                    }
                    let gear = ui.add(
                        egui::Button::selectable(self.show_settings, "Settings")
                            .corner_radius(radius::CONTROL)
                            .min_size(Vec2::new(0.0, 26.0)),
                    );
                    if gear.on_hover_text(keys::settings_hint()).clicked() {
                        toggle_settings = true;
                    }
                    if can_add_panel && ui::ghost(ui, "+ Panel").clicked() {
                        add_panel = true;
                    }
                },
            );

        if compare {
            self.compare_now(ctx);
        }
        if view_changed {
            // An inherited option can turn a mode back on after its button was
            // switched off. The toolbar is an explicit user command.
            self.take_view_control();
        }
        if add_panel {
            self.add_panel();
        }
        if toggle_settings {
            self.show_settings = !self.show_settings;
        }
        if toggle_help {
            self.show_help = !self.show_help;
        }
    }

    /// One card per panel: what it holds, where it came from, and everything you
    /// can do to it behind a single menu that does not move.
    fn panel_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let t = ui::tokens(ui);
        let n = self.panels.len();
        // Cleared as well as resized: a panel that has since become the result
        // must not keep the rect it had, or a dropped file would still land on
        // it. It keeps a slot so the indices line up, holding `Rect::NOTHING`,
        // which no pointer is ever inside.
        self.panel_rects.clear();
        self.panel_rects.resize(n, egui::Rect::NOTHING);
        let mut remove = None;
        let mut new_reference = None;
        let mut open = None;

        // The result has its own band under the diff: it is the buffer being
        // authored, and a sixth of a 260 px strip is not an editor.
        let inputs: Vec<usize> = (0..n).filter(|i| !self.panels[*i].result).collect();
        let reveal_target = self.reveal_panel.filter(|target| inputs.contains(target));
        let mut panel_revealed = self.reveal_panel.is_some() && reveal_target.is_none();
        let card_width = panel_card_width(ui.available_width(), inputs.len());
        egui::ScrollArea::horizontal()
            .id_salt("input-panels")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // The horizontal scrollbar reserves height, even while
                // floating. Measuring outside the scroll area made each card
                // six points too tall and grew the remembered source split
                // by another six points on every frame with many panels.
                let row_height = ui.available_height();
                ui.ctx().accesskit_node_builder(ui.unique_id(), |node| {
                    node.set_role(egui::accesskit::Role::ScrollView);
                    node.set_label("Input panels");
                });
                ui.horizontal(|ui| {
                    for i in inputs {
                        let mut editor_gained_focus = false;
                        ui.allocate_ui_with_layout(
                            Vec2::new(card_width, row_height),
                            Layout::top_down(Align::Min),
                            |ui| {
                                ui.set_min_width(card_width);
                let is_ref = i == self.reference;
                let is_shown = i == self.shown;
                let panel_label = self.panel_label(i);
                let frame = ui::card(&t, t.surface_raised, is_ref || is_shown)
                    .stroke(Stroke::new(
                        1.0,
                        if is_ref {
                            t.accent
                        } else if is_shown {
                            t.border_strong
                        } else {
                            t.border_subtle
                        },
                    ))
                    .inner_margin(Margin::ZERO);
                let card = frame.show(ui, |ui| {
                    ui.set_min_height(ui.available_height());
                    let header = Frame::new()
                        .inner_margin(Margin::symmetric(10, 7))
                        .show(ui, |ui| {
                            // One row, never wrapped, with fixed metadata and
                            // actions laid out before the flexible name.
                            //
                            // This used to be a `horizontal_wrapped` holding a
                            // second `horizontal_wrapped` for the actions. A
                            // nested wrapped layout wraps to *its own* left
                            // edge, which is halfway across the row, so as soon
                            // as a card got narrow the actions came off in a
                            // staircase: "Open…" on its own line at x≈340, "⋯"
                            // below that. It hit the reference panel first and
                            // hardest, because `baseline` is the tag that makes
                            // its header the widest. Reserve the right-hand
                            // widgets first, then give the name exactly the
                            // remaining width so it is the only flexible item.
                            ui.horizontal(|ui| {
                                let chip = ui.add(
                                    egui::Button::selectable(
                                        is_ref,
                                        ui::strong(title(i).to_string()),
                                    )
                                    .corner_radius(radius::CHIP)
                                    .min_size(Vec2::new(24.0, 22.0)),
                                );
                                if chip
                                    .on_hover_text(if is_ref {
                                        "This panel is the baseline. Everything else is compared against it."
                                    } else {
                                        "Make this panel the baseline everything is compared against"
                                    })
                                    .clicked()
                                {
                                    new_reference = Some(i);
                                }

                                let panel = &self.panels[i];
                                let edited = panel.edited;
                                let disk_stale = panel.disk_stale;
                                let follow_paused = panel.watch && panel.edited;
                                let detail = panel.detail();
                                let tooltip = panel.path.as_ref().map_or_else(
                                    || panel_label.clone(),
                                    |path| path.display().to_string(),
                                );

                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    match self.panel_menu(ui, i, &t) {
                                        Some(PanelRequest::Open) => open = Some(i),
                                        Some(PanelRequest::Remove) => remove = Some(i),
                                        None => {}
                                    }
                                    self.language_chip(ui, i, &t);
                                    let mut metadata = Vec::new();
                                    if is_ref {
                                        metadata.push((ui::micro("baseline").color(t.accent), ""));
                                    }
                                    if edited {
                                        metadata.push((ui::micro("edited").color(t.warning),
                                            "The text here no longer matches the file. What you see is what gets compared."));
                                    }
                                    if follow_paused {
                                        metadata.push((ui::micro("follow paused").color(t.warning),
                                            "Following is paused while this panel has edits, so a disk change cannot overwrite them."));
                                    }
                                    if disk_stale {
                                        metadata.push((ui::micro("reload failed").color(t.danger),
                                            "The file could not be reloaded. The previous snapshot is still here; Reload from disk retries it."));
                                    }
                                    if let Some(detail) = detail {
                                        metadata.push((ui::small(detail).color(t.text_muted), ""));
                                    }
                                    if !metadata.is_empty() {
                                        let metadata: Vec<_> = metadata.into_iter().map(|(text, hint)| {
                                            (egui::WidgetText::from(text).into_galley(
                                                ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, TextStyle::Body,
                                            ), hint)
                                        }).collect();
                                        let natural_width = metadata.iter().map(|(text, _)| text.size().x).sum::<f32>()
                                            + ui.spacing().item_spacing.x * (metadata.len() - 1) as f32;
                                        let width = natural_width.min(ui.available_width()).max(1.0);
                                        ui.allocate_ui_with_layout(Vec2::new(width, 22.0), Layout::left_to_right(Align::Center), |ui| {
                                            // Keep the panel letter and menu fixed even when
                                            // failure/edited/follow states need more width.
                                            egui::ScrollArea::horizontal()
                                                .id_salt(("panel-metadata", i))
                                                .max_width(width)
                                                .auto_shrink([true, true])
                                                .show(ui, |ui| {
                                                    ui.horizontal(|ui| {
                                                        for (text, hint) in metadata {
                                                            let response = ui.add(egui::Label::new(text).extend());
                                                            if !hint.is_empty() {
                                                                response.on_hover_text(hint);
                                                            }
                                                        }
                                                    });
                                                });
                                        });
                                    }

                                    // The nested left-to-right box keeps the
                                    // truncated text at the card's left edge;
                                    // its width is whatever the metadata left.
                                    let name_width = ui.available_width().max(0.0);
                                    ui.allocate_ui_with_layout(
                                        Vec2::new(name_width, 22.0),
                                        Layout::left_to_right(Align::Center),
                                        |ui| {
                                            ui.set_min_size(Vec2::new(name_width, 22.0));
                                            ui.set_max_width(name_width);
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(&panel_label)
                                                        .color(t.text_primary),
                                                )
                                                .truncate(),
                                            )
                                            .on_hover_text(tooltip);
                                        },
                                    );
                                });
                            });
                        })
                        .response;
                    ui.painter().hline(
                        header.rect.x_range(),
                        header.rect.bottom(),
                        Stroke::new(1.0, t.border_subtle),
                    );
                    Frame::new().inner_margin(Margin::symmetric(4, 4)).show(ui, |ui| {
                        egui::ScrollArea::both()
                            .id_salt(i)
                            .auto_shrink([false, false])
                            .max_height(whole_rows(ui, EDITOR_PAD))
                            .show(ui, |ui| {
                                let editor_name = format!(
                                    "Panel {} editor, {}",
                                    title(i),
                                    self.panel_label(i)
                                );
                                let response = ui.add(
                                    egui::TextEdit::multiline(&mut self.panels[i].text)
                                        .id(panel_edit_id(i))
                                        .font(TextStyle::Monospace)
                                        .frame(Frame::new().inner_margin(Margin::symmetric(8, 6)))
                                        .background_color(t.surface_sunken)
                                        .desired_width(f32::INFINITY)
                                        .desired_rows(8)
                                        // Not "Panel A editor — …": the card is
                                        // already headed by an A chip, and the
                                        // screen reader gets `editor_name` below.
                                        .hint_text("Paste here, or drop a file"),
                                );
                                ui.ctx().accesskit_node_builder(response.id, |node| {
                                    node.set_label(editor_name);
                                });
                                if self.focus_panel == Some(i) {
                                    response.request_focus();
                                    self.focus_panel = None;
                                }
                                if response.has_focus() {
                                    self.focused = i;
                                    ui.painter().rect_stroke(
                                        response.rect,
                                        radius::CONTROL,
                                        Stroke::new(2.0, t.accent),
                                        egui::StrokeKind::Inside,
                                    );
                                }
                                if response.gained_focus() {
                                    editor_gained_focus = true;
                                }
                                // Sniffing is bounded but not free, so it
                                // happens on edit rather than every frame.
                                if response.changed() {
                                    self.panels[i].resniff();
                                    // Editing takes deliberate local control of
                                    // the snapshot. It may differ from disk,
                                    // which `edited` and `follow paused` say;
                                    // it is no longer pretending a failed disk
                                    // reload succeeded.
                                    self.panels[i].disk_stale = false;
                                    // Editing a file-backed panel detaches the
                                    // buffer from the file. Anything else means
                                    // delta renders the file while the panel
                                    // shows something else.
                                    self.panels[i].edited = self.panels[i].path.is_some();
                                    self.touch_edit(i);
                                }
                            });
                    });
                });
                self.panel_rects[i] = card.response.rect;
                if reveal_target == Some(i) || editor_gained_focus {
                    card.response.scroll_to_me(Some(Align::Center));
                    panel_revealed = reveal_target == Some(i);
                }
                            },
                        );
                    }
                });
            });

        if panel_revealed {
            self.reveal_panel = None;
        }

        if let Some(i) = new_reference {
            self.set_reference(i);
        }
        if let Some(i) = open {
            self.choose_file_for_panel(i);
        }
        if let Some(i) = remove {
            self.request_destructive(DestructiveAction::Remove(i));
        }
        let _ = ctx;
    }

    /// Everything a panel can do, in one button that stays where it was. The
    /// old header grew and shrank as a panel changed state, so the control you
    /// were reaching for moved out from under the cursor.
    ///
    /// The two items that need the whole `App` back before they can run say so
    /// by returning; the rest borrow it here and are done.
    fn panel_menu(&mut self, ui: &mut egui::Ui, i: usize, t: &Tokens) -> Option<PanelRequest> {
        let mut request = None;
        let menu_name = format!("Panel {} options", title(i));
        let button = egui::Button::new(RichText::new("⋯").color(t.text_secondary))
            .frame_when_inactive(false)
            .corner_radius(radius::CHIP)
            .min_size(Vec2::splat(26.0));
        let (response, _) = egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
            let has_file = self.panels[i].path.is_some();
            // Moved in from the header, where it was the item that made the row
            // too wide to fit and came off in a staircase. Everything else a
            // panel can do was already here; leaving one action outside was
            // what kept the header growing.
            if ui.button("Open…").clicked() {
                request = Some(PanelRequest::Open);
                ui.close();
            }
            if i != self.reference && ui.button("Make baseline").clicked() {
                let target = i;
                self.set_reference(target);
                ui.close();
            }
            if has_file {
                if ui.button("Reload from disk").clicked() {
                    self.request_destructive(DestructiveAction::Reload(i));
                    ui.close();
                }
                if self.panels[i].edited && ui.button("Discard my edits").clicked() {
                    self.request_destructive(DestructiveAction::Reload(i));
                    ui.close();
                }
                let mut follow = self.panels[i].watch;
                if ui
                    .checkbox(&mut follow, "Follow changes on disk")
                    .on_hover_text(
                        "Re-read the file and re-compare whenever it is saved. \
                         Only file-backed panels have anything to follow.",
                    )
                    .changed()
                {
                    self.panels[i].watch = follow;
                }
            }
            ui.separator();
            if ui.button("Clear panel").clicked() {
                self.request_destructive(DestructiveAction::Clear(i));
                ui.close();
            }
            if self.panels.len() > 2 && ui.button("Remove panel").clicked() {
                request = Some(PanelRequest::Remove);
                ui.close();
            }
        });
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                response.enabled(),
                menu_name.clone(),
            )
        });
        request
    }

    /// The syntax delta will use for this panel, shown rather than hidden in a
    /// tooltip -- "why is my paste not coloured?" is the question it answers.
    fn language_chip(&mut self, ui: &mut egui::Ui, i: usize, t: &Tokens) {
        let panel = &self.panels[i];
        if panel.text.is_empty() && panel.path.is_none() {
            return; // "prose" is not a useful thing to say about nothing
        }
        let from_path = panel.language.is_none() && panel.path.is_some() && !panel.edited;
        let (label, colour) = match panel.language_hint() {
            Some(l) => (l, t.text_secondary),
            None => ("prose".to_string(), t.text_muted),
        };
        let accessible_name = format!("Panel {} language: {label}", title(i));
        let button = egui::Button::new(ui::small(label).color(colour))
            .frame_when_inactive(false)
            .corner_radius(radius::CHIP)
            .min_size(Vec2::new(0.0, 22.0));
        let response = egui::containers::menu::MenuButton::from_button(button)
            .ui(ui, |ui| {
                ui.label(ui::micro("syntax").color(t.text_muted));
                if from_path {
                    ui.label(
                        RichText::new(
                            "The file extension supplies this language hint. Rendering uses a \
                             buffer snapshot, so typing a language below overrides it.",
                        )
                        .color(t.text_muted),
                    );
                }
                let mut text = self.panels[i].language.clone().unwrap_or_default();
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut text)
                            .desired_width(140.0)
                            .hint_text("rs, py, json…"),
                    )
                    .changed()
                {
                    self.panels[i].language = (!text.trim().is_empty()).then_some(text);
                    self.touch_panel(i);
                }
                if ui.button("Detect automatically").clicked() {
                    self.panels[i].language = None;
                    self.touch_panel(i);
                    ui.close();
                }
            })
            .0;
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                response.enabled(),
                accessible_name.clone(),
            )
        });
        response.on_hover_text(if from_path {
            "Derived from the file's extension — click to override"
        } else {
            "Sniffed from the content — click to set it yourself"
        });
    }

    fn result_seed_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.panels
            .iter()
            .enumerate()
            .filter_map(|(index, panel)| (!panel.text.is_empty()).then_some(index))
    }

    /// Which pair is on screen -- and, while a result is being built, what is
    /// being taken from and how much of it is left.
    ///
    /// Kept visible at two panels as well, where it is a caption rather than a
    /// tab strip: with it hidden, nothing at all said which side was the
    /// baseline. It is also where a result is started, because this is the line
    /// that names the pair the user is looking at.
    fn pair_strip(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let t = ui::tokens(ui);
        let (reference, shown) = self.pair();
        let merging = self.merging();
        let panel_labels = (0..self.panels.len())
            .map(|i| self.panel_label(i))
            .collect::<Vec<_>>();
        let seeds = self
            .result_seed_indices()
            .map(|i| (i, format!("{} · {}", title(i), panel_labels[i])))
            .collect::<Vec<_>>();
        let result_panel = self.result_panel();
        let result_exists = result_panel.is_some();
        let can_undo = !self.undo.is_empty();
        let reveal_pair = self.reveal_pair;
        let fresh = self.is_fresh();
        let merge_status = merging.then(|| {
            if !fresh {
                if self.in_flight.is_some() {
                    "Comparing…".to_owned()
                } else {
                    "Count out of date".to_owned()
                }
            } else if self
                .cache
                .get(&shown)
                .is_some_and(|cached| cached.problem.is_some())
            {
                "Take unavailable".to_owned()
            } else {
                let left = self.cache.get(&shown).map_or(0, |c| c.hunks.len());
                format!(
                    "{left} difference{} from {}",
                    if left == 1 { "" } else { "s" },
                    title(shown)
                )
            }
        });
        let mut seed: Option<Option<usize>> = None;
        let mut stop = false;
        let mut undo = false;
        let mut go = None;
        let mut resume = None;
        let mut swap = false;
        let has_candidate_tabs = merging || panel_labels.len() > 2;
        // A two-panel comparison has no candidate tab strip to scroll. Treat a
        // reveal request as satisfied now, or it can survive until a later
        // panel is added and scroll an unrelated candidate into view.
        let mut pair_revealed = reveal_pair.is_some() && !has_candidate_tabs;
        // Swapping two empty panels is a no-op. Combine stays live because its
        // menu can always start empty and can seed from a non-active panel.
        let pair_has_text =
            !self.panels[reference].text.is_empty() || !self.panels[shown].text.is_empty();
        let strip_height = ui
            .spacing()
            .interact_size
            .y
            .max(ui.text_style_height(&TextStyle::Body) + 12.0);

        egui::containers::Sides::new()
            .shrink_left()
            .height(strip_height)
            .show(
                ui,
                |ui| {
                    let selector_width = ui.available_width();
                    let selector = egui::ScrollArea::horizontal()
                        .id_salt("pair-selector")
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            let mut tab_ids = Vec::new();
                            let mut focused_tab = None;
                            ui.horizontal(|ui| {
                                if merging {
                                    ui.label(ui::micro("building").color(t.accent));
                                    let full = panel_labels[reference].clone();
                                    ui.add_sized(
                                        [selector_width.min(220.0), strip_height],
                                        egui::Label::new(
                                            ui::strong(&full).color(t.text_primary),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(&full);
                                    ui.label(ui::micro("from").color(t.text_muted));
                                } else {
                                    let lead = if panel_labels.len() <= 2 {
                                        "comparing"
                                    } else {
                                        "against"
                                    };
                                    ui.label(ui::micro(lead).color(t.text_muted));
                                    let reference_label = if panel_labels.len() <= 2 {
                                        format!(
                                            "{} · {}",
                                            title(reference), panel_labels[reference]
                                        )
                                    } else {
                                        panel_labels[reference].clone()
                                    };
                                    let name_width = if panel_labels.len() <= 2 {
                                        ((selector_width - 80.0) / 2.0).clamp(80.0, 240.0)
                                    } else {
                                        selector_width.min(220.0)
                                    };
                                    ui.add_sized(
                                        [name_width, strip_height],
                                        egui::Label::new(
                                            RichText::new(&reference_label)
                                                .color(t.text_secondary),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(&reference_label);
                                    if panel_labels.len() <= 2 {
                                        ui.label(RichText::new("→").color(t.text_muted));
                                        let shown_label = format!(
                                            "{} · {}",
                                            title(shown), panel_labels[shown]
                                        );
                                        ui.add_sized(
                                            [name_width, strip_height],
                                            egui::Label::new(
                                                ui::strong(&shown_label).color(t.text_primary),
                                            )
                                            .truncate(),
                                        )
                                        .on_hover_text(&shown_label);
                                    }
                                }
                                if has_candidate_tabs {
                                    ui.add_space(6.0);
                                    ui.scope(|ui| {
                                        let tab_list_id = ui.unique_id();
                                        ui.ctx().accesskit_node_builder(tab_list_id, |node| {
                                            node.set_role(egui::accesskit::Role::TabList);
                                            node.set_label("Comparison candidates");
                                        });
                                        ui.horizontal(|ui| {
                                            for (i, panel_label) in panel_labels.iter().enumerate() {
                                                if i == reference {
                                                    continue;
                                                }
                                                let label =
                                                    format!("{} · {panel_label}", title(i));
                                                let font = TextStyle::Body.resolve(ui.style());
                                                let text_width = ui.fonts_mut(|fonts| {
                                                    fonts
                                                        .layout_no_wrap(
                                                            label.clone(),
                                                            font,
                                                            t.text_primary,
                                                        )
                                                        .size()
                                                        .x
                                                });
                                                let width = (text_width
                                                    + 2.0 * ui.spacing().button_padding.x)
                                                    .clamp(48.0, 240.0);
                                                let tab =
                                                    egui::Button::selectable(shown == i, &label)
                                                        .truncate()
                                                        .corner_radius(radius::CONTROL)
                                                        .min_size(Vec2::new(0.0, strip_height));
                                                let response = ui
                                                    .add_sized([width, strip_height], tab)
                                                    .on_hover_text(&label);
                                                ui.ctx().accesskit_node_builder(
                                                    response.id,
                                                    |node| {
                                                        node.set_role(egui::accesskit::Role::Tab);
                                                        node.set_label(label.as_str());
                                                        node.clear_toggled();
                                                        node.set_selected(shown == i);
                                                    },
                                                );
                                                if response.clicked() {
                                                    go = Some(i);
                                                }
                                                tab_ids.push((i, response.id));
                                                if response.has_focus() {
                                                    focused_tab = Some(tab_ids.len() - 1);
                                                    ui.memory_mut(|memory| {
                                                        memory.set_focus_lock_filter(
                                                            response.id,
                                                            egui::EventFilter {
                                                                horizontal_arrows: true,
                                                                ..Default::default()
                                                            },
                                                        );
                                                    });
                                                }
                                                if response.gained_focus()
                                                    || reveal_pair == Some(i)
                                                {
                                                    response.scroll_to_me(Some(Align::Center));
                                                    pair_revealed = reveal_pair == Some(i);
                                                }
                                            }
                                        });
                                        let bounds = ui.clip_rect().intersect(ui.min_rect());
                                        ui.ctx().accesskit_node_builder(tab_list_id, |node| {
                                            node.set_bounds(egui::accesskit::Rect {
                                                x0: f64::from(bounds.left()),
                                                y0: f64::from(bounds.top()),
                                                x1: f64::from(bounds.right()),
                                                y1: f64::from(bounds.bottom()),
                                            });
                                        });
                                    });
                                }
                            });
                            if let Some(current) = focused_tab {
                                let direction = ui.input_mut(|input| {
                                    if input.consume_key(egui::Modifiers::NONE, Key::ArrowRight) {
                                        1
                                    } else if input
                                        .consume_key(egui::Modifiers::NONE, Key::ArrowLeft)
                                    {
                                        -1
                                    } else {
                                        0
                                    }
                                });
                                if direction != 0 && !tab_ids.is_empty() {
                                    let next = moved_cursor(current, tab_ids.len(), direction);
                                    let (panel, id) = tab_ids[next];
                                    go = Some(panel);
                                    ui.memory_mut(|memory| {
                                        // A tab that only just gained focus still had the
                                        // default event filter when egui processed this
                                        // frame's input. Cancel that pending cardinal move
                                        // before applying the tab list's explicit wrap.
                                        memory.move_focus(egui::FocusDirection::None);
                                        memory.request_focus(id);
                                    });
                                }
                            }
                        });
                    let _ = selector;
                },
                |ui| {
                    if merging {
                    if ui::ghost(ui, "Stop building")
                        .on_hover_text("Keep the text, put the take controls away.")
                        .clicked()
                    {
                        stop = true;
                    }
                    if ui
                        .add_enabled(
                            can_undo,
                            egui::Button::new("Undo")
                                .frame_when_inactive(false)
                                .corner_radius(radius::CONTROL)
                                .min_size(Vec2::new(0.0, 26.0)),
                        )
                        .on_hover_text("Put the result back as it was before the last take")
                        .on_disabled_hover_text("Nothing taken yet")
                        .clicked()
                    {
                        undo = true;
                    }
                    ui.label(
                            ui::small(merge_status.as_deref().unwrap_or("Comparing…"))
                                .color(t.text_muted),
                    );
                } else {
                    let button = egui::Button::new("Combine…")
                        .corner_radius(radius::CONTROL)
                        .min_size(Vec2::new(0.0, 26.0));
                    egui::containers::menu::MenuButton::from_button(button)
                        .ui(ui, |ui| {
                            ui.label(ui::micro("start a result from").color(t.text_muted));
                            for (i, label) in &seeds {
                                if ui.button(label).clicked() {
                                    seed = Some(Some(*i));
                                    ui.close();
                                }
                            }
                            if ui.button("Start empty").clicked() {
                                seed = Some(None);
                                ui.close();
                            }
                        })
                        .0
                        .on_hover_text(
                            "Build a new panel out of these: take the differences you want \
                             from either side, edit it by hand, then save or copy it.",
                        );
                    if result_exists
                        && ui::ghost(ui, "Back to the result")
                            .on_hover_text("Make the result the baseline again")
                            .clicked()
                    {
                        resume = result_panel;
                    }
                    // An action, so it sits with the actions rather than beside
                    // the description on the left. Spelled out rather than
                    // drawn as `⇄`: `ui::icon` may only be handed glyphs the
                    // font chain is known to have.
                    if ui::ghost_enabled(ui, "Swap", pair_has_text)
                        .on_hover_text(format!(
                            "Compare these two the other way round, so what is removed \
                             here is added there.  {}",
                            keys::swap_label(),
                        ))
                        .on_disabled_hover_text("Both panels are empty")
                        .clicked()
                    {
                        swap = true;
                    }
                    }
                },
            );
        if let Some(i) = go {
            self.shown = i;
            self.reveal_panel = (!self.panels[i].result).then_some(i);
            self.reveal_pair = Some(i);
        } else if pair_revealed {
            self.reveal_pair = None;
        }
        if let Some(from) = seed {
            self.request_destructive(DestructiveAction::Reseed(from));
        }
        if let Some(i) = resume {
            self.resume_result(i);
        }
        if swap {
            self.swap_sides();
        }
        if stop {
            self.stop_building();
        }
        if undo {
            self.undo_take(ctx);
        }
    }

    fn prepare_shown_diff(
        &mut self,
        ctx: &egui::Context,
        font: &egui::FontId,
        palette: &Palette,
        line_height: f32,
    ) {
        // Only merge mode draws the diff hunk by hunk. A plain render is one
        // block, and laying every hunk out a second time to reach it would
        // double the layout cost of the longest diffs.
        let merging = self.merging();
        let Some(cached) = self.cache.get(&self.shown) else {
            // `diff_area` calls this first and is called unconditionally from
            // the central panel, so dropping the slot here is what stops a
            // layout outliving the comparison it was built for by even a frame.
            self.prepared = None;
            return;
        };
        let style = LayoutStyleKey {
            font: font.clone(),
            ansi16: palette.ansi16,
            foreground: palette.foreground,
            background: palette.background,
            line_height,
            pixels_per_point: ctx.pixels_per_point(),
        };
        // `RenderKey` carries `shown`, so a matching key already implies a
        // matching panel; the slot needs no index of its own.
        if self
            .prepared
            .as_ref()
            .is_some_and(|prepared| prepared.render_key == cached.key && prepared.style == style)
        {
            return;
        }
        let rows = ansi::body(&cached.lines);
        let whole = render::prepare_layout(
            ctx,
            rows,
            cached.columns,
            font.clone(),
            palette,
            line_height,
        );
        let hunks = if merging {
            cached
                .hunks
                .iter()
                .map(|(_, span)| {
                    render::prepare_layout(
                        ctx,
                        &rows[span.clone()],
                        cached.columns,
                        font.clone(),
                        palette,
                        line_height,
                    )
                })
                .collect()
        } else {
            Vec::new()
        };
        self.prepared = Some(PreparedDiff {
            render_key: cached.key.clone(),
            style,
            whole,
            hunks,
        });
    }

    fn diff_area(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, glyph: f32) {
        let t = ui::tokens(ui);
        // The scrollbar floats, so the only width the diff loses is this pad --
        // which has to be counted, since the column count is what delta laid out
        // against and a mismatch shows up as a ragged right edge.
        const PAD: f32 = 12.0;
        let cols = (((ui.available_width() - 2.0 * PAD) / glyph).floor() as usize).clamp(20, 1000);
        if cols != self.columns {
            self.columns = cols;
            self.pending_resize = Some(Instant::now());
        }

        if let Some(text) = self.notice.clone() {
            if ui::banner(ui, &t, ui::Tone::Warning, &text) {
                self.notice = None;
            }
            ui.add_space(8.0);
        }
        if let Some(text) = self.error.clone() {
            if ui::banner(ui, &t, ui::Tone::Error, &text) {
                self.error = None;
            }
            ui.add_space(8.0);
        }

        let font = self.mono_font();
        let palette = Palette::from_tokens(&t);
        let line_height = self.settings.mono_pt * 1.36;
        self.prepare_shown_diff(ctx, &font, &palette, line_height);
        let restore = self.restore_offset.take();
        let merging = self.merging();
        let fresh = self.is_fresh();
        // Claimed before anything borrows `self`, and defaulted to the keyboard's
        // pending move so a chord and a button click go through one path.
        let mut move_hunk = std::mem::take(&mut self.pending_hunk_move);
        let mut move_find = std::mem::take(&mut self.pending_find_move);
        let mut find_row = None;
        let mut find_offset = None;
        let mut take = None;
        let mut boxes = Vec::new();
        // Whether the diff owns the keyboard, learned inside the scroll area --
        // which is the only place that knows -- and painted after it.
        let mut focused = false;
        let mut offset = self.diff_offset;

        if let Some(prepared) = self.prepared.as_ref()
            && !prepared.whole.is_empty()
        {
            // From the cache, not from `prepared`: the per-hunk layouts exist
            // only while merging, and the count is what every render knows.
            let hunk_count = self.cache.get(&self.shown).map_or(0, |c| c.hunks.len());
            let mut close_find = false;
            let mut copy = false;
            let hunk_cursor = self.hunk_cursor;
            let ignoring = self.effective_options().ignoring();
            let copied_status = self.flash.as_ref().is_some_and(|(text, at)| {
                text == "Diff copied" && at.elapsed() < FLASH
            });
            let control_height = ui
                .spacing()
                .interact_size
                .y
                .max(ui.text_style_height(&TextStyle::Body) + 12.0);
            egui::containers::Sides::new()
                .shrink_left()
                .height(control_height)
                .show(
                    ui,
                    |ui| {
                        egui::ScrollArea::horizontal()
                            .id_salt("diff-navigation")
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.ctx().accesskit_node_builder(ui.unique_id(), |node| {
                                    node.set_role(egui::accesskit::Role::ScrollView);
                                    node.set_label("Difference navigation");
                                });
                                ui.horizontal(|ui| {
                                    if hunk_count > 0 {
                                        // Live only when there is somewhere to go. With one
                                        // difference these wrapped to the same one, so they were
                                        // full-strength controls that visibly did nothing.
                                        let walkable = hunk_count > 1;
                                        let previous = ui::ghost_enabled(
                                            ui,
                                            "Previous change",
                                            walkable,
                                        )
                                        .on_disabled_hover_text("Only one difference");
                                        if previous.gained_focus() {
                                            previous.scroll_to_me(Some(Align::Center));
                                        }
                                        if previous.clicked() {
                                            move_hunk = -1;
                                        }
                                        ui.label(
                                            ui::small(format!(
                                                "{} of {}",
                                                hunk_cursor.min(hunk_count - 1) + 1,
                                                hunk_count
                                            ))
                                            .color(t.text_muted),
                                        );
                                        let next = ui::ghost_enabled(ui, "Next change", walkable)
                                            .on_disabled_hover_text("Only one difference");
                                        if next.gained_focus() {
                                            next.scroll_to_me(Some(Align::Center));
                                        }
                                        if next.clicked() {
                                            move_hunk = 1;
                                        }
                                    }
                                    if let Some(what) = &ignoring {
                                        ui.label(ui::small(what).color(t.text_muted)).on_hover_text(
                                            "Some differences are being left out of this diff, from Settings.",
                                        );
                                    }
                                });
                            });
                    },
                    |ui| {
                    // Beside the diff, which is the only thing it describes --
                    // it used to sit in the toolbar between the view modes and
                    // the actions, where it was shown even with both panels
                    // empty and nothing laid out to any width at all.
                    ui.label(ui::micro(format!("{} cols", self.columns)).color(t.text_muted))
                        .on_hover_text(
                            "delta lays out against a column count, so the window's width is \
                             translated back into columns and the diff re-rendered on resize.",
                        );
                    if ui::ghost_enabled(ui, "Copy diff", fresh)
                        .on_disabled_hover_text(
                            "Re-render the current panels before copying this diff.",
                        )
                        .clicked()
                    {
                        // Deferred: the closure holds `&mut self`, and building
                        // the string needs the layout back. It used to be built
                        // every frame instead -- ~14 MB of copying, sixty times
                        // a second, at a 2 MB pair.
                        copy = true;
                    }
                    if copied_status || copy {
                        ui::status(ui, "Diff copied", t.success);
                    }
                    },
                );
            if copy && let Some(prepared) = self.prepared.as_ref() {
                ctx.copy_text(prepared.whole.to_text());
                self.flash = Some(("Diff copied".into(), Instant::now()));
            }
            if self.show_find {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Find in diff");
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.find_query)
                            .id(egui::Id::new("find-in-diff"))
                            .desired_width(180.0)
                            .hint_text("Search rendered text"),
                    );
                    ui.ctx().accesskit_node_builder(response.id, |node| {
                        node.set_label("Find in rendered diff");
                    });
                    if self.focus_find {
                        response.request_focus();
                        self.focus_find = false;
                    }
                    if response.changed() {
                        self.find_cursor = 0;
                        self.find_jump = true;
                    }
                    // TextEdit has now applied this frame's input. Searching
                    // earlier jumped to the previous query's first match and
                    // consumed the request before the new matches existed.
                    let find_lines = prepared.whole.matching_rows(&self.find_query);
                    if ui::ghost(ui, "Previous").clicked() {
                        move_find = -1;
                    }
                    if ui::ghost(ui, "Next").clicked()
                        || (response.has_focus() && ui.input(|input| input.key_pressed(Key::Enter)))
                    {
                        move_find = 1;
                    }
                    if self.find_jump && !find_lines.is_empty() {
                        self.find_cursor = 0;
                        find_row = Some(find_lines[0]);
                        self.find_jump = false;
                    } else if move_find != 0 && !find_lines.is_empty() {
                        self.find_cursor = moved_cursor(self.find_cursor, find_lines.len(), move_find);
                        find_row = Some(find_lines[self.find_cursor]);
                    }
                    if find_lines.is_empty() {
                        ui.label(
                            ui::small(if self.find_query.is_empty() {
                                "Type to search"
                            } else {
                                "No matches"
                            })
                            .color(t.text_muted),
                        );
                    } else {
                        ui.label(
                            ui::small(format!(
                                "{} of {}",
                                self.find_cursor.min(find_lines.len() - 1) + 1,
                                find_lines.len()
                            ))
                            .color(t.text_muted),
                        );
                    }
                    if ui::icon(ui, "×", "Close find", None)
                        .on_hover_text("Close find")
                        .clicked()
                    {
                        close_find = true;
                    }
                });
            }
            if move_hunk != 0 && hunk_count > 0 {
                self.hunk_cursor = moved_cursor(self.hunk_cursor, hunk_count, move_hunk);
                if let Some((top, _)) = self.hunk_boxes.get(self.hunk_cursor) {
                    self.restore_offset = Some(*top);
                }
            }
            if close_find {
                self.show_find = false;
            }
            ui.add_space(4.0);
        }

        ui::card(&t, t.surface_sunken, false)
            .inner_margin(Margin::symmetric(PAD as i8, 10))
            .show(ui, |ui| {
                ui.set_min_height(ui.available_height());
                match self.cache.get(&self.shown) {
                    Some(c) if !render::is_empty(&c.lines) => {
                        let stale = !self.is_fresh();
                        let Some(prepared) = self.prepared.as_ref() else {
                            return;
                        };
                        let mut area = egui::ScrollArea::both()
                            // Salted per panel: one shared offset means switching
                            // candidate keeps a pixel position into a different
                            // document.
                            .id_salt(self.shown)
                            .auto_shrink([false, false]);
                        if let Some(y) = restore {
                            area = area.vertical_scroll_offset(y);
                        }
                        let out = area.show_viewport(ui, |ui, viewport| {
                            // delta's colours are reproduced exactly; the only
                            // thing that may be tinted is the selection over
                            // them, whose default washes out on a diff ground.
                            ui.visuals_mut().selection.bg_fill = if t.dark {
                                Color32::from_rgb(0x3a, 0x4a, 0x63)
                            } else {
                                Color32::from_rgb(0xcd, 0xdd, 0xf5)
                            };
                            // Above the merge/plain branch, and inside the
                            // viewport closure: that is what makes merge mode
                            // one tab stop too, puts the region's accessibility
                            // node in place before the first chunk claims a
                            // parent, and sends a page-key scroll to *this*
                            // scroll area.
                            focused = render::diff_region(ui).has_focus();
                            if !merging || c.hunks.is_empty() {
                                prepared.whole.show_viewport(ui, viewport, glyph);
                                find_offset = find_row.map(|row| row as f32 * line_height);
                                // One rendered line is one laid-out row --
                                // delta does the wrapping, and `Extend` stops
                                // egui redoing it -- so where a hunk was drawn
                                // is arithmetic, and the block does not have to
                                // be cut up to find out.
                                boxes.extend(c.hunks.iter().map(|(_, span)| {
                                    (
                                        span.start as f32 * line_height,
                                        span.len() as f32 * line_height,
                                    )
                                }));
                                return;
                            }
                            ui.scope_builder(
                                egui::UiBuilder::new()
                                    .accessibility_parent(render::diff_region_id()),
                                |ui| {
                                    // Zero spacing so the hunks still read as one block
                                    // with control rows cut into it, rather than as a
                                    // stack of separate cards. Selection stitches across
                                    // adjacent labels, so the diff stays copyable whole.
                                    ui.spacing_mut().item_spacing.y = 0.0;
                                    let origin = ui.cursor().top();
                                    for (n, (hunk, span)) in c.hunks.iter().enumerate() {
                                        let (control, clicked) =
                                            self.hunk_control(ui, &t, hunk, !stale);
                                        if clicked {
                                            take = Some(n);
                                        }
                                        let body = prepared.hunks[n].show(ui, glyph);
                                        if let Some(row) = find_row.filter(|row| span.contains(row)) {
                                            // Merge controls add height between rendered
                                            // rows. Use this frame's body position so
                                            // finds also land correctly after resizing
                                            // or changing fonts.
                                            find_offset = Some(
                                                body.rect.top() - origin
                                                    + (row - span.start) as f32 * line_height,
                                            );
                                        }
                                        boxes.push((
                                            control.top() - origin,
                                            body.rect.bottom() - control.top(),
                                        ));
                                    }
                                },
                            );
                        });
                        offset = out.state.offset.y;
                        render::scroll_edges(
                            ui,
                            &t,
                            out.inner_rect,
                            out.state.offset.x,
                            out.content_size.x,
                        );
                        // After the closure, so the ring is not buried under the
                        // erase fills the chunks paint.
                        if focused {
                            render::focus_ring(ui, out.inner_rect);
                        }
                        if stale {
                            self.stale_pill(ui, &t);
                        }
                    }
                    Some(_) if self.reports_no_differences() => self.no_difference(ui, &t),
                    Some(_) => {
                        self.stale_empty_diff(ui, &t);
                        self.stale_pill(ui, &t);
                    }
                    None => self.nothing_yet(ui, &t),
                }
            });

        self.hunk_boxes = boxes;
        self.diff_offset = offset;
        if find_offset.is_some() {
            self.restore_offset = find_offset;
        }
        if let Some(n) = take {
            self.take_hunk(n, ctx);
        }
    }

    /// The row that replaces delta's hunk header: where this difference is, and
    /// the one control that takes it.
    ///
    /// Left-aligned and immediately above the rows it acts on, so which lines a
    /// click affects is a matter of looking rather than of remembering.
    fn hunk_control(
        &self,
        ui: &mut egui::Ui,
        t: &Tokens,
        hunk: &Hunk,
        live: bool,
    ) -> (egui::Rect, bool) {
        let mut clicked = false;
        let rect = Frame::new()
            .inner_margin(Margin::symmetric(0, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let button = egui::Button::new(ui::small(format!(
                        "Use {}'s version",
                        title(self.shown)
                    )))
                    .corner_radius(radius::CHIP)
                    .min_size(Vec2::new(0.0, 22.0));
                    clicked = ui
                        .add_enabled(live, button)
                        .on_hover_text(format!(
                            "Replace these lines of {} with {}'s.  {}",
                            self.panel_label(self.reference),
                            self.panel_label(self.shown),
                            keys::take_label(),
                        ))
                        .on_disabled_hover_text(
                            "Re-reading the panels — this list is a moment out of date.",
                        )
                        .clicked();
                    ui.add(
                        egui::Label::new(ui::small(location(hunk)).color(t.text_muted))
                            .selectable(true),
                    )
                    .on_hover_text(header_text(hunk));
                });
            })
            .response
            .rect;
        ui.painter().hline(
            rect.x_range(),
            rect.top(),
            Stroke::new(1.0, t.border_subtle),
        );
        (rect, clicked)
    }

    fn result_identity_view(&mut self, i: usize) -> ResultIdentityView {
        let name = self.panels[i]
            .saved_to
            .is_some()
            .then(|| self.panel_label(i));
        let (state, state_is_success) = if let Some(tool) = &self.mergetool {
            if self.result_is_saved_to(i, &tool.merged) {
                ("Git target saved".to_owned(), true)
            } else {
                ("Git target not saved".to_owned(), false)
            }
        } else {
            match (
                &self.panels[i].saved_to,
                self.panels[i].dirty,
                self.result_last_save_is_current(i),
            ) {
                (None, _, _) => ("not saved yet".to_owned(), false),
                (Some(_), true, _) => ("unsaved changes".to_owned(), false),
                (Some(_), false, true) => ("saved".to_owned(), true),
                (Some(_), false, false) => ("saved file missing or changed".to_owned(), false),
            }
        };
        let destination = self
            .mergetool
            .as_ref()
            .map(|tool| format!("Git target: {}", tool.merged.display()))
            .or_else(|| {
                self.panels[i]
                    .saved_to
                    .as_ref()
                    .map(|path| format!("Saved to: {}", path.display()))
            });
        let flash = match self.flash.clone() {
            Some((text, at)) if at.elapsed() < FLASH => {
                Some(text)
            }
            Some(_) => {
                self.flash = None;
                None
            }
            None => None,
        };
        ResultIdentityView {
            name,
            state,
            state_is_success,
            destination,
            detail: self.panels[i].detail(),
            flash,
        }
    }

    /// What the result is called, how far it is from disk, and how big it is.
    fn result_identity(ui: &mut egui::Ui, t: &Tokens, view: &ResultIdentityView) {
        ui.label(ui::micro("result").color(t.accent));
        // The name only once it *is* one. Until the result is saved,
        // `Panel::name` has nothing to go on and answers "Result", so the band
        // read "RESULT Result not saved yet" -- the same fact three times, with
        // the tag and the status each saying it better than the middle one did.
        if let Some(name) = &view.name {
            ui.label(ui::strong(name).color(t.text_primary));
        }
        ui.label(ui::small(&view.state).color(if view.state_is_success {
            t.success
        } else {
            t.warning
        }));
        if let Some(full) = &view.destination {
            let response = ui
                .add_sized(
                    [ui.available_width().clamp(96.0, 320.0), 20.0],
                    egui::Label::new(ui::small(full).color(t.text_muted)).truncate(),
                )
                .on_hover_text(full);
            ui.ctx().accesskit_node_builder(response.id, |node| {
                node.set_description(full.as_str());
            });
        }
        if let Some(detail) = &view.detail {
            ui.label(ui::small(detail).color(t.text_muted));
        }
        if let Some(text) = &view.flash {
            ui::status(ui, text, t.success);
            ui.ctx().request_repaint_after(FLASH);
        }
    }

    /// Save As is an export in mergetool mode, so name the effect rather than
    /// suggesting that it retargets Git's canonical MERGED file.
    fn alternate_result_save_label(&self) -> &'static str {
        if self.mergetool.is_some() {
            "Export copy…"
        } else {
            "Save as…"
        }
    }

    /// The ways out of the feature: the clipboard, a file, or somewhere else.
    fn result_controls(&self, ui: &mut egui::Ui, t: &Tokens, i: usize, act: &mut BandActions) {
        let has_text = !self.panels[i].text.is_empty();
        let first_save = self.result_save_target(i).is_none();
        if ui::primary(
            ui,
            t,
            if first_save { "Save…" } else { "Save" },
            keys::save_label(),
            true,
        ) {
            act.save = Some(first_save);
        }
        if ui
            .add_enabled(
                has_text,
                egui::Button::new("Copy")
                    .frame_when_inactive(false)
                    .corner_radius(radius::CONTROL)
                    .min_size(Vec2::new(0.0, 26.0)),
            )
            .on_hover_text("Copy the whole result to the clipboard")
            .clicked()
        {
            act.copy = true;
        }
        let menu = egui::Button::new(RichText::new("⋯").color(t.text_secondary))
            .frame_when_inactive(false)
            .corner_radius(radius::CHIP)
            .min_size(Vec2::splat(26.0));
        let (response, _) = egui::containers::menu::MenuButton::from_button(menu).ui(ui, |ui| {
            if !first_save && ui.button(self.alternate_result_save_label()).clicked() {
                act.save = Some(true);
                ui.close();
            }
            ui.label(ui::micro("place").color(t.text_muted));
            for placement in ResultPlacement::ALL {
                let on = self.settings.result_placement == placement;
                if ui.radio(on, placement.label()).clicked() {
                    act.placement = Some(placement);
                    ui.close();
                }
            }
            ui.separator();
            // Re-seeding lives here because `Combine…` is gone from the strip
            // once a result exists, and starting over from a different panel is a
            // normal thing to want half way through.
            ui.label(ui::micro("start over from").color(t.text_muted));
            for j in 0..self.panels.len() {
                if self.panels[j].result || self.panels[j].text.is_empty() {
                    continue;
                }
                let label = format!("{} · {}", title(j), self.panel_label(j));
                if ui.button(label).clicked() {
                    act.seed = Some(Some(j));
                    ui.close();
                }
            }
            if ui.button("nothing").clicked() {
                act.seed = Some(None);
                ui.close();
            }
            ui.separator();
            if ui.button("Stop building").clicked() {
                act.stop = true;
                ui.close();
            }
            if self.panels.len() > 2 && ui.button("Remove the result panel").clicked() {
                act.remove = true;
                ui.close();
            }
        });
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                response.enabled(),
                "Result options",
            )
        });
    }

    /// The result, in a band of its own.
    ///
    /// This is the buffer being authored, so it gets a whole edge of the window:
    /// as a column in the panel row it would be one sixth of a 260 px strip. The
    /// band being there at all is also what says the app is building something,
    /// which is a stronger signal than any label.
    fn result_band(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let t = ui::tokens(ui);
        let Some(i) = self.result_panel() else {
            return;
        };
        let identity = self.result_identity_view(i);
        let mut act = BandActions::default();
        let header_height = ui
            .spacing()
            .interact_size
            .y
            .max(ui.text_style_height(&TextStyle::Body) + 12.0);
        // Controls keep their hit bounds; the potentially long destination and
        // status own the flexible lane and scroll without adding another row to
        // the compact result editor.
        egui::containers::Sides::new()
            .shrink_left()
            .height(header_height)
            .show(
                ui,
                |ui| {
                    let identity_scroll = egui::ScrollArea::horizontal()
                        .id_salt("result-identity")
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            let scroll_id = ui.unique_id();
                            Self::mark_scroll_view(
                                ctx,
                                scroll_id,
                                "Result status and destination",
                                None,
                            );
                            ui.horizontal(|ui| Self::result_identity(ui, &t, &identity));
                            scroll_id
                        });
                    Self::mark_scroll_view(
                        ctx,
                        identity_scroll.inner,
                        "Result status and destination",
                        Some(identity_scroll.inner_rect),
                    );
                },
                |ui| self.result_controls(ui, &t, i, &mut act),
            );
        ui.add_space(6.0);
        egui::ScrollArea::both()
            .id_salt("result-band")
            .auto_shrink([false, false])
            .max_height(whole_rows(ui, EDITOR_PAD))
            .show(ui, |ui| {
                let editor_name = format!("Result editor, {}", self.panel_label(i));
                let response = ui.add(
                    egui::TextEdit::multiline(&mut self.panels[i].text)
                        // Addressable, so a take can clear the field's own undo
                        // history rather than leave it describing a text that no
                        // longer exists.
                        .id(result_edit_id(i))
                        .font(TextStyle::Monospace)
                        .frame(Frame::new().inner_margin(Margin::symmetric(8, 6)))
                        .background_color(t.surface_sunken)
                        .desired_width(f32::INFINITY)
                        .desired_rows(6)
                        .hint_text("Result editor — take differences from the diff, or type here"),
                );
                ui.ctx().accesskit_node_builder(response.id, |node| {
                    node.set_label(editor_name);
                });
                if self.focus_panel == Some(i) {
                    response.request_focus();
                    self.focus_panel = None;
                }
                if response.has_focus() {
                    self.focused = i;
                    ui.painter().rect_stroke(
                        response.rect,
                        radius::CONTROL,
                        Stroke::new(2.0, t.accent),
                        egui::StrokeKind::Inside,
                    );
                }
                if response.changed() {
                    self.result_edited(i);
                }
            });
        if let Some(ask) = act.save {
            self.save_result(ask);
        }
        if act.copy {
            self.copy_result(ctx);
        }
        if let Some(placement) = act.placement {
            self.settings.result_placement = placement;
        }
        if let Some(from) = act.seed {
            self.request_destructive(DestructiveAction::Reseed(from));
        }
        if act.stop {
            self.stop_building();
        }
        if act.remove {
            self.request_destructive(DestructiveAction::Remove(i));
        }
    }

    fn stale_pill(&self, ui: &mut egui::Ui, t: &Tokens) {
        let text = if self.in_flight.is_some() {
            "Re-rendering…".to_string()
        } else if self.auto_renders() {
            "Out of date".to_string()
        } else {
            format!("Out of date — {} to re-render", keys::compare_label())
        };
        let bounds = ui.min_rect().intersect(ui.clip_rect()).shrink(8.0);
        if bounds.height() < ui.text_style_height(&TextStyle::Small) + 12.0
            || bounds.width() < 40.0
        {
            return;
        }
        // This overlays the diff without advancing its layout. A foreground
        // Area escaped the diff's clip when a failure banner left little room,
        // drawing the stale state over the result editor below it.
        let mut overlay = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("stale")
                .max_rect(bounds)
                .layout(Layout::right_to_left(Align::Min)),
        );
        overlay.set_clip_rect(bounds);
        Frame::new()
            .fill(t.surface_overlay)
            .stroke(Stroke::new(1.0, t.border_strong))
            .corner_radius(radius::CHIP)
            .inner_margin(Margin::symmetric(10, 5))
            .show(&mut overlay, |ui| {
                ui.add(egui::Label::new(ui::small(text).color(t.text_secondary)).truncate());
            });
    }

    fn no_difference(&self, ui: &mut egui::Ui, t: &Tokens) {
        let (a, b) = self.pair();
        let lines = self.panels[b].text.lines().count();
        if self.panels[a].is_empty() && self.panels[b].is_empty() {
            ui::empty_state(
                ui,
                t,
                "Nothing to compare yet",
                "Both panels are empty.",
                &[],
            );
            return;
        }
        if self.merging() {
            ui::identical(
                ui,
                t,
                &format!(
                    "Your result already matches {} — {lines} line{}. Take from another \
                     panel, or save it.",
                    self.panel_label(b),
                    if lines == 1 { "" } else { "s" }
                ),
            );
            return;
        }
        // An ignore that turns a difference into "no differences" has to say so
        // here above all: this is the screen someone reads as "these are the
        // same file".
        let qualifier = match self.effective_options().ignoring() {
            Some(what) => format!(" — {what}"),
            None => String::new(),
        };
        ui::identical(
            ui,
            t,
            &format!(
                "{} and {} are identical — {lines} line{}{qualifier}.",
                self.panel_label(a),
                self.panel_label(b),
                if lines == 1 { "" } else { "s" }
            ),
        );
    }

    fn stale_empty_diff(&self, ui: &mut egui::Ui, t: &Tokens) {
        ui::empty_state(
            ui,
            t,
            "Comparison out of date",
            "The panels changed after the last comparison.",
            &[],
        );
    }

    fn nothing_yet(&self, ui: &mut egui::Ui, t: &Tokens) {
        let (a, b) = self.pair();
        if !self.panels[a].is_empty() || !self.panels[b].is_empty() {
            if self.in_flight.is_some() {
                ui::empty_state(ui, t, "Comparing…", "Preparing the differences between these panels.", &[]);
            } else {
                let body = if self.auto_renders() {
                    "Choose Compare to see the differences between these panels."
                } else {
                    "Automatic comparison is paused for large inputs. Choose Compare to see the differences."
                };
                ui::empty_state(ui, t, "Ready to compare", body, &[(keys::compare_label(), "compare")]);
            }
            return;
        }
        ui::empty_state(
            ui,
            t,
            "Nothing to compare yet",
            // Where "drop a file onto a panel" now lives. It was a row in the
            // list below, keyed on the word `paste` -- set in the chord style,
            // so a verb was drawn as a key you could press.
            "Put text in two panels, or drop a file onto one, and delgui will diff \
             them with the real delta binary.",
            &[
                (keys::paste_panel_label(), "paste into a new panel"),
                (keys::compare_label(), "compare"),
                (keys::help_label(), "keyboard shortcuts"),
            ],
        );
    }

    // ---- settings --------------------------------------------------------

    fn settings_drawer(&mut self, ui: &mut egui::Ui) {
        let t = ui::tokens(ui);
        let drawer_width = ui.available_width().min(ui.clip_rect().width());
        // What the scroll area reserves for its own bar, asked for rather than
        // guessed: the 8 that used to sit here was only safe because the theme
        // asks for a floating bar, whose allowance is 6.
        let content_width = (drawer_width - ui.spacing().scroll.allocated_width()).max(1.0);
        egui::ScrollArea::both()
            .id_salt("settings-drawer-scroll")
            .max_width(drawer_width)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Keep ordinary rows on the drawer width. At the maximum UI
                // font, indivisible native controls may still be wider; the
                // horizontal axis is enabled as a last-resort path to them.
                ui.set_width(content_width);
                // Measured here rather than at each row: every `ui::field`
                // below is a direct child of this `Ui`, so `available_width` is
                // the same at this point as it is at each of them -- which is
                // what makes one measurement legitimate for all nine.
                let fields = ui::field_column(ui, SETTINGS_FIELDS);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Settings").text_style(TextStyle::Heading));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui::icon(ui, "×", "Close settings", None)
                            .on_hover_text("Close settings")
                            .clicked()
                        {
                            self.show_settings = false;
                        }
                    });
                });
                ui.add_space(12.0);

                ui::section(ui, &t, "Appearance");
                let mut theme = self.settings.theme;
                let options: Vec<(ThemeChoice, &str)> =
                    ThemeChoice::ALL.iter().map(|c| (*c, c.label())).collect();
                ui::field(ui, &t, fields, "Theme", |ui| {
                    ui::choice(ui, &t, &mut theme, &options);
                });
                self.settings.theme = theme;

                let catalog_ready = self.catalog_rx.is_none();
                ui::field(ui, &t, fields, "Interface", |ui| {
                    self.font_combo(ui, false, catalog_ready);
                });
                ui::field(ui, &t, fields, "Interface size", |ui| {
                    ui.spacing_mut().slider_width = 112.0;
                    ui::slider_visuals(ui, &t);
                    let response = ui.add(
                        egui::Slider::new(&mut self.settings.ui_pt, UI_PT).suffix(" pt"),
                    );
                    response.ctx.accesskit_node_builder(response.id, |node| {
                        node.set_label("Interface font size");
                    });
                });
                ui.add_space(6.0);
                ui::field(ui, &t, fields, "Diff", |ui| {
                    self.font_combo(ui, true, catalog_ready);
                });
                ui::field(ui, &t, fields, "Diff size", |ui| {
                    ui.spacing_mut().slider_width = 112.0;
                    ui::slider_visuals(ui, &t);
                    let response = ui.add(
                        egui::Slider::new(&mut self.settings.mono_pt, MONO_PT).suffix(" pt"),
                    );
                    response.ctx.accesskit_node_builder(response.id, |node| {
                        node.set_label("Diff font size");
                    });
                });
                if let Some(p) = self.probe {
                    if !p.fixed_pitch {
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(
                                "That font is not monospaced. delta lays its output out in \
                             columns, so the diff will not line up.",
                            )
                            .color(t.warning),
                        );
                    } else if !p.cjk_aligns() {
                        ui.add_space(6.0);
                        ui.label(
                            ui::small(
                                "No double-width CJK in this font, so lines containing CJK will \
                             drift. delta pads them to two columns.",
                            )
                            .color(t.text_muted),
                        );
                    }
                }

                ui.add_space(16.0);
                let mut dirty = false;
                ui::section(ui, &t, "syntax");
                ui::field(ui, &t, fields, "Theme", |ui| {
                    let automatic = if self.opts.inherit_gitconfig {
                        "from your gitconfig"
                    } else {
                        "delta default"
                    };
                    let current = self
                        .opts
                        .syntax_theme
                        .clone()
                        .unwrap_or_else(|| automatic.into());
                    egui::ComboBox::from_id_salt("syntax-theme")
                        .selected_text(current)
                        .width(ui::control_width(ui))
                        .show_ui(ui, |ui| {
                            dirty |= ui
                                .selectable_value(
                                    &mut self.opts.syntax_theme,
                                    None,
                                    automatic,
                                )
                                .changed();
                            for (name, dark) in &self.themes {
                                let label =
                                    format!("{name}  ·  {}", if *dark { "dark" } else { "light" });
                                dirty |= ui
                                    .selectable_value(
                                        &mut self.opts.syntax_theme,
                                        Some(name.clone()),
                                        label,
                                    )
                                    .changed();
                            }
                        });
                });
                // A syntax theme built for the other appearance is legible only by
                // accident: it sets the code colours, while --light/--dark sets the
                // ground they land on.
                if let Some(name) = &self.opts.syntax_theme {
                    let dark_theme = self.themes.iter().find(|(n, _)| n == name).map(|(_, d)| *d);
                    if let Some(dark) = dark_theme
                        && dark != t.dark
                    {
                        ui.add_space(6.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "{name} is a {} theme.",
                                    if dark { "dark" } else { "light" }
                                ))
                                .color(t.warning),
                            );
                            if ui::ghost(
                                ui,
                                if dark {
                                    "Switch to dark"
                                } else {
                                    "Switch to light"
                                },
                            )
                            .clicked()
                            {
                                self.settings.theme = if dark {
                                    ThemeChoice::Dark
                                } else {
                                    ThemeChoice::Light
                                };
                            }
                        });
                    }
                }

                ui.add_space(16.0);
                ui::section(ui, &t, "differences");
                let merging = self.merging();
                ui::field(ui, &t, fields, "Show", |ui| {
                    let options: Vec<(Context, &str)> =
                        Context::ALL.iter().map(|c| (*c, c.label())).collect();
                    dirty |= ui::choice(ui, &t, &mut self.settings.context, &options);
                });
                ui::field(ui, &t, fields, "Whitespace", |ui| {
                    dirty |= ui::choice(
                        ui,
                        &t,
                        &mut self.opts.whitespace,
                        &[
                            (Whitespace::Exact, "Exact"),
                            (Whitespace::Amount, "Ignore amount"),
                            (Whitespace::All, "Ignore all"),
                        ],
                    );
                });
                dirty |= ui
                    .checkbox(&mut self.opts.ignore_blank_lines, "Ignore blank lines")
                    .changed();
                dirty |= ui
                    .checkbox(
                        &mut self.opts.ignore_cr_at_eol,
                        "Ignore Windows line endings",
                    )
                    .on_hover_text(
                        "A file saved with CRLF differs from the same file saved with LF on                          every single line.",
                    )
                    .changed();
                ui::field(ui, &t, fields, "Ignore lines matching", |ui| {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.settings.ignore_matching)
                            .desired_width(ui::control_width(ui))
                            .hint_text("regular expression"),
                    );
                    // On losing focus, not on every keystroke: a half-typed
                    // pattern is usually not a valid one, and git refuses the
                    // whole diff over it.
                    dirty |= response.lost_focus();
                });
                if merging {
                    ui.add_space(6.0);
                    ui.label(
                        ui::small(
                            "While a result is being built these are off: a take copies a                              difference's lines exactly, so nothing may be left out of one.",
                        )
                        .color(t.text_muted),
                    );
                }

                ui.add_space(16.0);
                ui::section(ui, &t, "delta");
                // Here rather than under "differences": this is a delta flag and
                // it colours part of a changed line, where everything in that
                // section decides which lines are changed at all. Which is also
                // why it needs no merge-mode exception -- it cannot make
                // `merge::verify` false.
                ui::field(ui, &t, fields, "Highlight", |ui| {
                    dirty |= ui::choice(
                        ui,
                        &t,
                        &mut self.opts.granularity,
                        &crate::settings::GRANULARITIES,
                    );
                });
                dirty |= ui
                    .checkbox(&mut self.opts.hunk_headers, "Hunk headers")
                    .on_hover_text(
                        "delta's boxed line number above each hunk. Worth having on a long \
                     diff, redundant on two pasted buffers.",
                    )
                    .changed();
                if self.merging() {
                    ui.label(
                        ui::small(
                            "While a result is being built, each hunk header is replaced by \
                             the control that takes it.",
                        )
                        .color(t.text_muted),
                    );
                }
                let inherit_changed = ui
                    .checkbox(&mut self.opts.inherit_gitconfig, "Use my [delta] gitconfig")
                    .on_hover_text(
                        "Off passes --no-gitconfig, which is the only way to get output that \
                     does not depend on your gitconfig or working directory.",
                    )
                    .changed();
                if inherit_changed {
                    if self.opts.inherit_gitconfig {
                        self.features = self.gitconfig.active.iter().cloned().collect();
                    } else {
                        self.features.clear();
                    }
                    dirty = true;
                }

                if !self.opts.inherit_gitconfig {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "Gitconfig inheritance is off. Config-defined feature presets are disabled.",
                        )
                        .color(t.text_muted),
                    );
                } else if self.gitconfig.is_empty() {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "No [delta] section in your gitconfig, so delgui is using \
                         delta's own defaults.",
                        )
                        .color(t.text_muted),
                    );
                } else {
                    let names = self.gitconfig.selectable_features();
                    if !names.is_empty() {
                        ui.add_space(8.0);
                        ui.label(ui::micro("features from your gitconfig").color(t.text_muted));
                        for name in names {
                            let mut on = self.features.contains(&name);
                            let hover = self
                                .gitconfig
                                .features
                                .get(&name)
                                .map(|s| {
                                    s.iter()
                                        .map(|(k, v)| format!("{k} = {v}"))
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                })
                                .unwrap_or_else(|| "a preset built into delta".into());
                            if ui.checkbox(&mut on, &name).on_hover_text(hover).changed() {
                                if on {
                                    self.features.insert(name.clone());
                                } else {
                                    self.features.remove(&name);
                                }
                                dirty = true;
                            }
                        }
                    }
                    if !self.gitconfig.sources.is_empty() {
                        ui.add_space(6.0);
                        for src in &self.gitconfig.sources {
                            ui.label(ui::small(src.display().to_string()).color(t.text_muted));
                        }
                    }
                }

                ui.add_space(16.0);
                ui::section(ui, &t, "Render pipeline");
                let argv = self.command_line();
                Frame::new()
                    .fill(t.surface_sunken)
                    .stroke(Stroke::new(1.0, t.border_subtle))
                    .corner_radius(radius::CONTROL)
                    .inner_margin(Margin::same(8))
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(ui::chord(argv.clone()).color(t.text_secondary))
                                .selectable(true)
                                .wrap(),
                        );
                    });
                ui.add_space(6.0);
                if ui::ghost(ui, "Copy pipeline shape").clicked() {
                    ui.ctx().copy_text(argv);
                }
                ui.label(
                    ui::small(
                        "/dev/fd/N marks private in-memory snapshots; this diagnostic shape is not a directly runnable command.",
                    )
                    .color(t.text_muted),
                );

                ui.add_space(16.0);
                ui.label(ui::small(self.delta.version_string.clone()).color(t.text_muted));
                ui.label(
                    ui::small(
                        "Comparisons use private snapshots. Only an explicit Save writes a result to disk.",
                    )
                    .color(t.text_muted),
                );

                if dirty {
                    self.touch();
                }
            });
    }

    /// The shape of the owned Git-to-delta pipeline. Snapshot descriptors are
    /// deliberately placeholders: exposing a process-local descriptor as a
    /// supposedly runnable command would be less truthful than naming it.
    fn command_line(&self) -> String {
        let options = self.effective_options();
        let git = options
            .git_diff_args()
            .into_iter()
            .chain(["/dev/fd/N".into(), "/dev/fd/N".into()])
            .map(|word| shell_quote(&word))
            .collect::<Vec<_>>()
            .join(" ");
        let delta = options
            .to_args()
            .into_iter()
            .map(|word| shell_quote(&word))
            .collect::<Vec<_>>()
            .join(" ");
        format!("git {git} | delta {delta}")
    }

    fn font_combo(&mut self, ui: &mut egui::Ui, mono: bool, ready: bool) {
        /// What `None` actually selects, said the same way in both pickers.
        ///
        /// It used to read "Default" in the interface picker, which was the one
        /// label it could not be: `Settings::default` sets `ui_font` to
        /// [`fonts::default_ui_face`] -- the system font -- while `None` means
        /// egui's bundled families with `Hack` pushed to the *front* of the
        /// proportional chain (`fonts::definitions`). Picking "Default" therefore
        /// put every label, heading and button in the app into a monospace face,
        /// which is the opposite of the default and reads as an unfinished app.
        /// The real default is reachable by name: `fonts::scan` lists the same
        /// face as "System Font", and it compares equal, so it shows as selected.
        const BUNDLED: &str = "Hack (bundled)";

        let current = if mono {
            self.settings.mono_font.clone()
        } else {
            self.settings.ui_font.clone()
        };
        let label = current
            .as_ref()
            .map(|f| f.family.clone())
            .unwrap_or_else(|| BUNDLED.into());
        let salt = if mono { "font-mono" } else { "font-ui" };
        egui::ComboBox::from_id_salt(salt)
            .selected_text(label)
            .width(ui::control_width(ui))
            .show_ui(ui, |ui| {
                let mut pick: Option<Option<(Face, Option<Face>)>> = None;
                if ui.selectable_label(current.is_none(), BUNDLED).clicked() {
                    pick = Some(None);
                }
                if !ready {
                    ui.label(ui::small("Looking for installed fonts…"));
                }
                for family in &self.catalog {
                    let selected = current.as_ref() == Some(&family.regular);
                    if ui.selectable_label(selected, family.name()).clicked() {
                        pick = Some(Some((family.regular.clone(), family.strong.clone())));
                    }
                }
                if let Some(choice) = pick {
                    match (mono, choice) {
                        (true, c) => self.settings.mono_font = c.map(|(r, _)| r),
                        (false, Some((regular, strong))) => {
                            self.settings.ui_font = Some(regular);
                            self.settings.ui_font_strong = strong;
                        }
                        (false, None) => {
                            self.settings.ui_font = None;
                            self.settings.ui_font_strong = None;
                        }
                    }
                }
            });
    }

    fn destructive_modal(&mut self, ctx: &egui::Context) {
        let Some(action) = self.destructive.as_ref() else {
            self.restore_pending_modal_focus(ctx);
            return;
        };
        let affected = self.affected_panels(action);
        let names = affected
            .iter()
            .filter(|&&i| self.panel_has_unsaved_content(i))
            .map(|&i| format!("{} · {}", title(i), self.panel_label(i)))
            .collect::<Vec<_>>()
            .join(", ");
        let (heading, verb, explanation) = match action {
            DestructiveAction::LoadFiles(loads) => (
                "Replace unsaved panel contents?",
                if loads.len() == 1 {
                    "Replace panel"
                } else {
                    "Replace panels"
                },
                "Opening the selected file will replace text that exists only in delgui.",
            ),
            DestructiveAction::Reload(_) => (
                "Discard edits and reload?",
                "Discard and reload",
                "Reloading from disk will replace the edits currently in this panel.",
            ),
            DestructiveAction::Clear(_) => (
                "Clear unsaved panel contents?",
                "Clear panel",
                "Clearing removes text that has not been saved anywhere.",
            ),
            DestructiveAction::Remove(_) => (
                "Remove a panel with unsaved contents?",
                "Remove panel",
                "Removing this panel also removes text that has not been saved anywhere.",
            ),
            DestructiveAction::Reseed(_) => (
                "Start the result over?",
                "Start over",
                "Starting over replaces the current result and clears its take history.",
            ),
        };
        let result_affected = self
            .result_panel()
            .is_some_and(|i| self.panels[i].dirty && affected.contains(&i));
        let t = Tokens::of(ctx.theme());
        let mut confirm = false;
        let mut cancel = false;
        let mut save_first = false;
        let modal_id = egui::Id::new("destructive-confirmation");
        let mut safe_control = None;
        let (modal_width, modal_height) = Self::modal_dimensions(ctx);
        let response = egui::Modal::new(modal_id)
            .frame(
                Frame::new()
                    .fill(t.surface_overlay)
                    .stroke(Stroke::new(1.0, t.border_strong))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(Margin::same(24)),
            )
            .backdrop_color(Color32::from_black_alpha(if t.dark { 160 } else { 60 }))
            .show(ctx, |ui| {
                ui.set_width(modal_width);
                ui.set_max_height(modal_height);
                ui.label(RichText::new(heading).text_style(TextStyle::Heading));
                ui.add_space(8.0);
                let details = egui::ScrollArea::vertical()
                    .id_salt("destructive-details")
                    .auto_shrink([false, true])
                    // The remaining budget covers a wrapped two-row footer at
                    // the maximum interface font plus its surrounding gaps.
                    .max_height((modal_height - 168.0).max(72.0))
                    .show(ui, |ui| {
                        let scroll_id = ui.unique_id();
                        Self::mark_scroll_view(
                            ctx,
                            scroll_id,
                            "Destructive action details",
                            None,
                        );
                        ui.label(RichText::new(explanation).color(t.text_secondary));
                        if !names.is_empty() {
                            ui.add_space(6.0);
                            ui.label(RichText::new(names).color(t.warning));
                        }
                        scroll_id
                    });
                Self::mark_scroll_view(
                    ctx,
                    details.inner,
                    "Destructive action details",
                    Some(details.inner_rect),
                );
                ui.add_space(16.0);
                ui.horizontal_wrapped(|ui| {
                    if result_affected && ui::ghost(ui, "Save result, then continue").clicked() {
                        save_first = true;
                    }
                    if ui::ghost(ui, verb).clicked() {
                        confirm = true;
                    }
                    safe_control = Some(ui.next_auto_id());
                    if ui::primary(ui, &t, "Cancel", "Esc", true) {
                        cancel = true;
                    }
                });
            });
        Self::mark_modal(
            ctx,
            &response.response,
            egui::accesskit::Role::AlertDialog,
            heading,
        );
        if let Some(safe_control) = safe_control {
            let restore_focus = self.destructive_restore_focus.take();
            self.enter_modal_focus(ctx, modal_id, safe_control, restore_focus);
        }
        if save_first && self.save_result(false) {
            confirm = true;
        }
        if confirm {
            if let Some(action) = self.destructive.take() {
                self.apply_destructive(action);
            }
        } else if cancel || response.should_close() {
            self.destructive = None;
        }
        if self.destructive.is_none() {
            self.leave_modal_focus(ctx, modal_id);
        }
    }

    /// Hold a close request while any panel contains work that exists only in
    /// this process. Result contents can be saved; other panels still require an
    /// explicit discard decision.
    fn quit_guard(&mut self, ctx: &egui::Context) {
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if close_requested {
            self.refresh_saved_result_from_disk();
        }
        let unsaved = self.unsaved_panels();
        let merge_unresolved = self.merge_unresolved();
        if close_requested {
            if let Some(tool) = &self.mergetool {
                tool.resolved.store(!merge_unresolved, Ordering::Relaxed);
            }
        }
        if close_requested
            && (!unsaved.is_empty() || merge_unresolved)
            && !self.closing
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.show_help = false;
            self.destructive = None;
            self.quit_guard = true;
        }
        if !self.quit_guard {
            self.restore_pending_modal_focus(ctx);
            return;
        }
        if unsaved.is_empty() && !merge_unresolved {
            self.quit_guard = false;
            self.leave_modal_focus(ctx, egui::Id::new("quit-guard"));
            return;
        }
        let result = self
            .result_panel()
            .filter(|i| unsaved.contains(i));
        let has_result = self.result_panel().is_some();
        let other_unsaved = unsaved.iter().any(|i| Some(*i) != result);
        let t = Tokens::of(ctx.theme());
        let mut discard = false;
        let mut keep = false;
        let mut save_and_quit = false;
        let mut save_result = false;
        let modal_id = egui::Id::new("quit-guard");
        let mut safe_control = None;
        let save_shortcut = ctx.input(|input| keys::pressed(input).contains(&Action::SaveResult));
        let (modal_width, modal_height) = Self::modal_dimensions(ctx);
        let response = egui::Modal::new(modal_id)
            .frame(
                Frame::new()
                    .fill(t.surface_overlay)
                    .stroke(Stroke::new(1.0, t.border_strong))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(Margin::same(24)),
            )
            .backdrop_color(Color32::from_black_alpha(if t.dark { 160 } else { 60 }))
            .show(ctx, |ui| {
                ui.set_width(modal_width);
                ui.set_max_height(modal_height);
                ui.label(
                    RichText::new(if merge_unresolved {
                        "Git merge is not saved"
                    } else {
                        "Unsaved panel contents"
                    })
                    .text_style(TextStyle::Heading),
                );
                ui.add_space(8.0);
                let details = egui::ScrollArea::vertical()
                    .id_salt("quit-details")
                    .auto_shrink([false, true])
                    .max_height((modal_height - 168.0).max(72.0))
                    .show(ui, |ui| {
                        let scroll_id = ui.unique_id();
                        Self::mark_scroll_view(ctx, scroll_id, "Unsaved work details", None);
                        if merge_unresolved {
                            let target = self
                                .mergetool
                                .as_ref()
                                .map(|tool| tool.merged.display().to_string())
                                .unwrap_or_default();
                            ui.label(
                                RichText::new(format!(
                                    "Git's target has not been saved: {target}. Quitting unresolved makes the mergetool report failure."
                                ))
                                .color(t.text_secondary),
                            );
                            if other_unsaved {
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new(format!(
                                        "{} other panel{} also contain unsaved text or edits.",
                                        unsaved.len().saturating_sub(usize::from(result.is_some())),
                                        if unsaved.len().saturating_sub(usize::from(result.is_some())) == 1 {
                                            ""
                                        } else {
                                            "s"
                                        },
                                    ))
                                    .color(t.warning),
                                );
                            }
                        } else {
                            ui.label(
                                RichText::new(format!(
                                    "{} panel{} contain{} text or edits that exist only in delgui.",
                                    unsaved.len(),
                                    if unsaved.len() == 1 { "" } else { "s" },
                                    if unsaved.len() == 1 { "s" } else { "" },
                                ))
                                .color(t.text_secondary),
                            );
                        }
                        scroll_id
                    });
                Self::mark_scroll_view(
                    ctx,
                    details.inner,
                    "Unsaved work details",
                    Some(details.inner_rect),
                );
                ui.add_space(16.0);
                ui.horizontal_wrapped(|ui| {
                    if merge_unresolved && !other_unsaved && has_result {
                        if ui::primary(
                            ui,
                            &t,
                            "Save to Git target and quit",
                            keys::save_label(),
                            true,
                        ) {
                            save_and_quit = true;
                        }
                    } else if merge_unresolved && has_result {
                        if ui::primary(
                            ui,
                            &t,
                            "Save to Git target",
                            keys::save_label(),
                            true,
                        ) {
                            save_result = true;
                        }
                    } else if result.is_some() && !other_unsaved {
                        if ui::primary(ui, &t, "Save and quit", keys::save_label(), true) {
                            save_and_quit = true;
                        }
                    } else if result.is_some() && ui::ghost(ui, "Save result").clicked() {
                        save_result = true;
                    }
                    if ui::ghost(
                        ui,
                        if merge_unresolved {
                            if other_unsaved {
                                "Discard and quit unresolved"
                            } else {
                                "Quit unresolved"
                            }
                        } else {
                            "Discard and quit"
                        },
                    )
                    .clicked()
                    {
                        discard = true;
                    }
                    safe_control = Some(ui.next_auto_id());
                    if ui::ghost(ui, "Keep working").clicked() {
                        keep = true;
                    }
                });
            });
        Self::mark_modal(
            ctx,
            &response.response,
            egui::accesskit::Role::AlertDialog,
            if merge_unresolved {
                "Unresolved Git merge"
            } else {
                "Unsaved panel contents"
            },
        );
        if let Some(safe_control) = safe_control {
            self.enter_modal_focus(ctx, modal_id, safe_control, None);
        }
        if save_shortcut {
            if has_result
                && ((merge_unresolved && !other_unsaved)
                    || (result.is_some() && !other_unsaved))
            {
                save_and_quit = true;
            } else if has_result && (merge_unresolved || result.is_some()) {
                save_result = true;
            }
        }
        if save_result {
            let _ = self.save_result(false);
        }
        if save_and_quit && self.save_result(false) {
            self.closing = true;
            self.quit_guard = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if discard {
            self.closing = true;
            self.quit_guard = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if keep || response.should_close() {
            self.quit_guard = false;
        }
        if !self.quit_guard {
            self.leave_modal_focus(ctx, modal_id);
        }
    }

    fn help_modal(&mut self, ctx: &egui::Context) {
        let modal_id = egui::Id::new("help");
        if !self.show_help {
            self.leave_modal_focus(ctx, modal_id);
            self.restore_pending_modal_focus(ctx);
            return;
        }
        let t = Tokens::of(ctx.theme());
        let content = ctx.content_rect().size();
        let modal_width = (content.x - 64.0).clamp(280.0, 420.0);
        // Account for the frame's 48 points of vertical margin plus the
        // heading/footer. A max on the inner Ui alone otherwise still lets the
        // visible Close action cross the bottom edge at 720×480 and 20 pt.
        let modal_height = (content.y - 112.0).clamp(140.0, 620.0);
        let mut close = false;
        let mut safe_control = None;
        let response = egui::Modal::new(modal_id)
            .frame(
                Frame::new()
                    .fill(t.surface_overlay)
                    .stroke(Stroke::new(1.0, t.border_strong))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(Margin::same(24)),
            )
            .backdrop_color(Color32::from_black_alpha(if t.dark { 160 } else { 60 }))
            .show(ctx, |ui| {
                ui.set_width(modal_width);
                ui.set_max_height(modal_height);
                ui.label(RichText::new("Keyboard & gestures").text_style(TextStyle::Heading));
                ui.add_space(12.0);
                egui::ScrollArea::both()
                    .id_salt("help-contents")
                    .auto_shrink([false, false])
                    .max_height((modal_height - 92.0).max(100.0))
                    .show(ui, |ui| {
                        ui.ctx().accesskit_node_builder(ui.unique_id(), |node| {
                            node.set_role(egui::accesskit::Role::ScrollView);
                            node.set_label("Keyboard shortcuts");
                        });
                        let mut group = "";
                        egui::Grid::new("keys-grid")
                            .num_columns(2)
                            .spacing([24.0, 8.0])
                            .min_row_height(22.0)
                            .show(ui, |ui| {
                                for row in keys::help_rows() {
                                    if row.group != group {
                                        group = row.group;
                                        ui.label("");
                                        ui.label(ui::micro(row.group).color(t.text_muted));
                                        ui.end_row();
                                    }
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        ui.label(
                                            ui::chord(row.label)
                                                .color(t.text_primary)
                                                .background_color(t.surface),
                                        );
                                    });
                                    ui.label(RichText::new(row.describe).color(t.text_secondary));
                                    ui.end_row();
                                }
                            });
                        if let Some(h) = &self.hotkey {
                            ui.add_space(12.0);
                            ui.label(
                                ui::small(format!(
                                    "{} pastes into delgui from anywhere while it runs",
                                    h.label
                                ))
                                .color(t.text_muted),
                            );
                        }
                    });
                ui.add_space(12.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    safe_control = Some(ui.next_auto_id());
                    if ui::primary(ui, &t, "Close", "Esc", true) {
                        close = true;
                    }
                });
            });
        Self::mark_modal(
            ctx,
            &response.response,
            egui::accesskit::Role::Dialog,
            "Keyboard and gestures",
        );
        if let Some(safe_control) = safe_control {
            self.enter_modal_focus(ctx, modal_id, safe_control, None);
        }
        if close || response.should_close() {
            self.show_help = false;
            self.leave_modal_focus(ctx, modal_id);
        }
    }

    /// Outline every panel a drop would land in -- all of them, not just the
    /// one under the cursor, since dropping three files writes three panels.
    fn draw_drop_hint(&self, ctx: &egui::Context, ui: &egui::Ui) {
        if self.modal_active() {
            return;
        }
        let hovering = ctx.input(|i| i.raw.hovered_files.len());
        if hovering == 0 {
            return;
        }
        let t = Tokens::of(ctx.theme());
        for i in self.drop_targets(ctx, hovering) {
            if let Some(rect) = self.panel_rects.get(i) {
                ui.painter().rect_stroke(
                    *rect,
                    radius::CARD,
                    Stroke::new(2.0, t.accent_hover),
                    egui::StrokeKind::Inside,
                );
            }
        }
    }

    /// Debounced work, run once per frame after everything has been drawn.
    fn tick(&mut self, ctx: &egui::Context) {
        // Re-read only once the writes stop, so a half-written file is never
        // what gets diffed.
        if let Some(at) = self.pending_reload {
            if at.elapsed() >= WATCH_DEBOUNCE {
                self.pending_reload = None;
                let mut reloaded = Vec::new();
                let mut reload_failures = Vec::new();
                for (i, p) in self.panels.iter_mut().enumerate() {
                    // Following a file must not silently destroy typing. An
                    // edited panel keeps its buffer and stops tracking.
                    if p.watch && !p.edited {
                        match p.reload() {
                            Ok(()) => reloaded.push(i),
                            Err(e) => {
                                p.disk_stale = true;
                                reload_failures.push((i, e));
                            }
                        }
                    }
                }
                for i in reloaded {
                    self.touch_panel(i);
                }
                if let Some((i, error)) = reload_failures.pop() {
                    eprintln!(
                        "delgui: watched panel {} could not reload; the previous snapshot was kept",
                        title(i),
                    );
                    self.error = Some(format!(
                        "Panel {} could not reload: {error} The previous snapshot is still shown; retry Reload from disk when the file is available.",
                        title(i),
                    ));
                }
            } else {
                ctx.request_repaint_after(WATCH_DEBOUNCE);
            }
        }
        let settled = |at: &mut Option<Instant>, wait: Duration| -> bool {
            match *at {
                Some(t) if t.elapsed() >= wait => {
                    *at = None;
                    true
                }
                Some(_) => false,
                None => true,
            }
        };
        let resized = settled(&mut self.pending_resize, RESIZE_DEBOUNCE);
        let typed = settled(&mut self.pending_edit, EDIT_DEBOUNCE);
        if !resized {
            ctx.request_repaint_after(RESIZE_DEBOUNCE);
        }
        if !typed {
            ctx.request_repaint_after(EDIT_DEBOUNCE);
        }
        // Every other change -- a toggle, a tab, a theme -- takes effect on its
        // own. Only the two streams above wait, and only a big pair waits for
        // the user to ask, since delta costs about a second per megabyte.
        if self.requested || should_auto_render(resized, typed, self.pair_bytes()) {
            self.schedule(ctx);
        }
    }
}

/// Quote one POSIX shell word. Single quotes suppress every metacharacter; an
/// embedded quote is represented by ending the quote, inserting an escaped
/// quote, and starting it again.
fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

fn moved_cursor(current: usize, len: usize, direction: isize) -> usize {
    debug_assert!(len > 0);
    let current = current.min(len - 1);
    if direction < 0 {
        current.checked_sub(1).unwrap_or(len - 1)
    } else {
        (current + 1) % len
    }
}

fn should_auto_render(resize_settled: bool, edit_settled: bool, pair_bytes: usize) -> bool {
    resize_settled && edit_settled && pair_bytes <= AUTO_RENDER_BYTES
}

/// How far a panel editor's first text row sits below the top of its viewport:
/// the top half of the `TextEdit` frame's `Margin::symmetric(8, 6)`.
///
/// The *top* margin only, deliberately. Row `k` spans
/// `pad + k * row ..= pad + (k + 1) * row`, so a viewport of `pad + n * row`
/// ends exactly where row `n` does. Counting both margins instead leaves the
/// bottom six pixels showing the top six pixels of the row after it, which is
/// the same sliced-glyph artefact one row further down.
const EDITOR_PAD: f32 = 6.0;

/// The tallest a text viewport can be here without ending in half a line.
///
/// A scroll viewport whose height is not `pad + n * row_height` clips its last
/// row through the middle of the glyphs, and a horizontally sliced `}` at the
/// bottom edge of a card reads as a rendering fault rather than as more content
/// below -- which is a bad thing for a diff tool to look like. The leftover
/// becomes padding inside the card, where it is invisible.
fn whole_rows(ui: &egui::Ui, pad: f32) -> f32 {
    let row = ui.text_style_height(&TextStyle::Monospace);
    let rows = ((ui.available_height() - pad) / row).floor().max(1.0);
    pad + rows * row
}

fn panel_card_width(available: f32, count: usize) -> f32 {
    let count = count.max(1) as f32;
    ((available - 8.0 * (count - 1.0)) / count).max(280.0)
}

/// How tall the input row starts out, given the height left under the toolbar.
///
/// A share rather than a constant. At a flat 260 a taller window gave every one
/// of those pixels to the diff: at 1130 the panels still showed seven lines of a
/// thirteen-line file while 230 of empty card sat under the diff. The split is
/// draggable and eframe remembers it, so this only decides where it starts --
/// but where it starts is what most people ever see.
fn source_panel_sizes(available: f32) -> (f32, f32, f32) {
    if available < 640.0 {
        // The maximum is intentional. A split remembered from a tall window
        // used to survive a resize to 720×480 and leave a bottom Result plus no
        // central diff at all.
        (130.0, 96.0, 130.0)
    } else {
        // Capped, because past a point the panels are just a text editor with a
        // diff underneath, and the diff is the thing being read.
        (
            (available * 0.3).clamp(260.0, 420.0),
            160.0,
            f32::INFINITY,
        )
    }
}

fn effective_result_placement(width: f32, preferred: ResultPlacement) -> ResultPlacement {
    if width < 760.0 && preferred != ResultPlacement::Bottom {
        ResultPlacement::Bottom
    } else {
        preferred
    }
}

/// How big the result band starts out, given what is left under the pair strip.
///
/// The bottom placement is a share for the same reason the input row is, and it
/// needs one more: this is the buffer being *authored*. At a flat 240 -- minus
/// margins and its own header -- that was four or five visible lines while the
/// diff above it kept four hundred pixels, which is not an editor either, just a
/// less cramped one than a column in the panel row would have been.
fn result_panel_sizes(available: f32, placement: ResultPlacement) -> (f32, f32, f32) {
    match placement {
        ResultPlacement::Bottom => {
            // 45% of what is left, and note what "left" means: this is called
            // after the toolbar, the input row and the pair strip have taken
            // theirs, so on a default 860 window it is handed 504, not 860.
            // A `< 640` compact branch here -- written as though the argument
            // were the window height -- therefore matched *always*, which is
            // why the band was stuck at the compact 128 and showed four lines
            // of the buffer being authored on any screen.
            let max = (available - MIN_DIFF_OUTER_HEIGHT)
                .max(MIN_RESULT_OUTER_HEIGHT)
                .min(available.max(0.0));
            let default = (available * 0.45).clamp(128.0, 520.0).min(max);
            // Never a floor above the default, or a short window is given a
            // band there is no room for.
            (default, default.min(150.0), max)
        }
        // A side already shows the whole result at once, and every pixel here
        // is one delta does not get to lay the diff out in.
        ResultPlacement::Left | ResultPlacement::Right => (440.0, 260.0, f32::INFINITY),
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let settings = Settings {
            side_by_side: self.opts.side_by_side,
            line_numbers: self.opts.line_numbers,
            wrap: self.opts.wrap,
            hunk_headers: self.opts.hunk_headers,
            whitespace: self.opts.whitespace,
            granularity: self.opts.granularity,
            ignore_blank_lines: self.opts.ignore_blank_lines,
            ignore_cr_at_eol: self.opts.ignore_cr_at_eol,
            syntax_theme: self.opts.syntax_theme.clone(),
            inherit_gitconfig: self.opts.inherit_gitconfig,
            features: Some(self.features.iter().cloned().collect()),
            settings_open: self.show_settings,
            ..self.settings.clone()
        };
        eframe::set_value(storage, eframe::APP_KEY, &settings);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();
        self.sync_appearance(ctx);
        self.poll();
        self.poll_catalog();
        self.poll_watches();
        self.poll_hotkey(ctx);
        // Consume shortcuts before widgets see Enter, then dispatch after the
        // editors have applied this frame's text. Save must include an edit
        // arriving in the same frame, and TextEdit keeps its own undo/redo.
        let actions = self.capture_keys(ctx);
        self.publish_resolution();
        if !self.quit_menu_guarded {
            self.quit_menu_guarded = crate::menu::guard_quit();
        }

        // delta lays out against a column count, so the GUI's pixel width has to
        // be translated back into columns and the diff re-rendered on resize.
        let font = self.mono_font();
        let glyph = ctx.fonts_mut(|f| f.glyph_width(&font, ' ')).max(1.0);
        if self.probe.is_none() {
            self.probe = Some(fonts::probe(ctx, &font));
        }

        let t = Tokens::of(ctx.theme());
        egui::Panel::top("toolbar")
            .frame(
                Frame::new()
                    .fill(t.surface_raised)
                    .inner_margin(Margin::symmetric(16, 10))
                    .stroke(Stroke::NONE),
            )
            .show(ui, |ui| self.toolbar(ui, ctx));

        if self.show_settings && ui.available_width() < 760.0 {
            // No title bar: `settings_drawer` draws its own heading and its own
            // close button, and egui's would stack a second "Settings" and a
            // second ✕ on top of them -- in default window chrome that matches
            // nothing else here, with a collapse triangle that folds the drawer
            // into a stub for no reason anyone asked for.
            egui::Window::new("Settings")
                .id(egui::Id::new("settings-overlay"))
                .title_bar(false)
                .resizable(true)
                .default_width(SETTINGS_DRAWER_WIDTH)
                .min_width(240.0)
                .max_width((ui.available_width() - 32.0).max(240.0))
                // Where the docked drawer would be, rather than over the panels
                // in the top-left corner. Only a default: the window remembers a
                // dragged position against its id.
                .default_pos(
                    ui.max_rect().right_top() + Vec2::new(-SETTINGS_DRAWER_WIDTH - 16.0, 12.0),
                )
                .frame(
                    Frame::window(&ctx.style_of(ctx.theme()))
                        .inner_margin(Margin::symmetric(16, 14)),
                )
                .show(ctx, |ui| self.settings_drawer(ui));
        } else if self.show_settings {
            // The cap leaves the diff at least 360: at the 760 the branch above
            // switches over at, that is exactly one default-width drawer.
            let max_width = (ui.available_width() - 360.0).clamp(240.0, 420.0);
            egui::Panel::right("settings")
                .resizable(true)
                .default_size(SETTINGS_DRAWER_WIDTH.min(max_width))
                .min_size(240.0)
                .max_size(max_width)
                .frame(
                    Frame::new()
                        .fill(t.surface_raised)
                        .inner_margin(Margin::symmetric(16, 14)),
                )
                .show(ui, |ui| self.settings_drawer(ui));
        }

        let compact_height = ui.available_height() < 640.0;
        let (source_default, source_min, source_max) =
            source_panel_sizes(ui.available_height());
        // Compact and regular splits have distinct ids. Otherwise clamping a
        // remembered tall split for a tiled window also overwrites the user's
        // preferred split when the window is widened again.
        let source_panel_id = if compact_height {
            "panels-compact"
        } else {
            "panels"
        };
        egui::Panel::top(source_panel_id)
            .resizable(true)
            .default_size(source_default)
            .min_size(source_min)
            .max_size(source_max)
            .show_separator_line(false)
            .frame(
                Frame::new()
                    .fill(t.surface)
                    .inner_margin(Margin::symmetric(16, 12)),
            )
            .show(ui, |ui| self.panel_row(ui, ctx));

        egui::Panel::top("pair")
            .show_separator_line(false)
            .frame(Frame::new().fill(t.surface).inner_margin(Margin {
                left: 16,
                right: 16,
                top: 0,
                bottom: 10,
            }))
            .show(ui, |ui| self.pair_strip(ui, ctx));

        // The inputs stay across the top; the result takes a whole edge of what is
        // left, so the buffer being authored is never a sixth of the panel row.
        // Added after the top panels and before the diff, so a side placement
        // reaches from the pair strip to the bottom of the window.
        if self.result_panel().is_some() {
            // An id per placement, not one shared: egui remembers a panel's size
            // against its id, and a height dragged at the bottom is not a width
            // at the side.
            let placement =
                effective_result_placement(ui.available_width(), self.settings.result_placement);
            let (result_default, result_min, result_max) =
                result_panel_sizes(ui.available_height(), placement);
            let panel = match placement {
                // Costs the diff no width, which matters: delta lays out against
                // a column count, and side by side is the widest thing here.
                ResultPlacement::Bottom => {
                    let id = if compact_height {
                        "result-bottom-compact"
                    } else {
                        "result-bottom"
                    };
                    egui::Panel::bottom(id).default_size(result_default)
                }
                ResultPlacement::Left => {
                    egui::Panel::left("result-left").default_size(result_default)
                }
                ResultPlacement::Right => {
                    egui::Panel::right("result-right").default_size(result_default)
                }
            };
            panel
                .resizable(true)
                .min_size(result_min)
                .max_size(result_max)
                .frame(
                    Frame::new()
                        .fill(t.surface_raised)
                        .inner_margin(Margin::symmetric(16, 12)),
                )
                .show(ui, |ui| self.result_band(ui, ctx));
        }

        egui::CentralPanel::no_frame()
            .frame(Frame::new().fill(t.surface).inner_margin(Margin {
                left: 16,
                right: 16,
                top: 0,
                bottom: 16,
            }))
            .show(ui, |ui| self.diff_area(ui, ctx, glyph));

        self.handle_keys(ctx, actions);
        self.quit_guard(ctx);
        if !self.quit_guard {
            self.destructive_modal(ctx);
        }
        if !self.quit_guard && self.destructive.is_none() {
            self.help_modal(ctx);
        }
        self.accept_drops(ctx);
        self.sync_watches(ctx);
        self.draw_drop_hint(ctx, ui);
        self.tick(ctx);
    }
}

/// Where a difference sits, in terms of the buffer being built rather than in
/// `@@` coordinates -- with git's own note of the enclosing function, which it
/// names better than any heuristic here would.
fn location(hunk: &Hunk) -> String {
    let line = hunk.old.start + 1;
    if hunk.context.is_empty() {
        format!("line {line}")
    } else {
        format!("line {line} · {}", hunk.context)
    }
}

/// The same as git wrote it, for anyone who would rather read that.
fn header_text(hunk: &Hunk) -> String {
    let side = |r: &Range<usize>| match r.len() {
        0 => format!("{},0", r.start),
        1 => format!("{}", r.start + 1),
        n => format!("{},{n}", r.start + 1),
    };
    format!(
        "@@ -{} +{} @@ {}",
        side(&hunk.old),
        side(&hunk.new),
        hunk.context
    )
}

/// The id of a result panel's editor, so its state can be reached from outside.
fn panel_edit_id(i: usize) -> egui::Id {
    egui::Id::new(("panel-edit", i))
}

fn result_edit_id(i: usize) -> egui::Id {
    egui::Id::new(("result-edit", i))
}

/// Forget egui's own undo history for a field we just rewrote behind its back.
///
/// `TextEditState`'s undoer is fed only while the field has focus, at one-second
/// granularity. Take five differences with focus in the diff, click into the
/// result, press ⌘Z, and it restores the text from before all five -- the whole
/// merge gone in one keystroke, with the app's own undo stack still describing
/// something else. Clearing it means the field's undo can never step back past a
/// take.
fn forget_text_undo(ctx: &egui::Context, id: egui::Id) {
    if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
        state.clear_undoer();
        state.store(ctx, id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app_with_settings(settings: Settings) -> App {
        App::new(
            Delta {
                path: PathBuf::from("delgui-test-delta-does-not-exist"),
                version: (0, 19, 0),
                version_string: "delta test".into(),
            },
            settings,
            Launch::default(),
        )
    }

    fn test_app() -> App {
        test_app_with_settings(Settings::default())
    }

    fn modal_test_context() -> (egui::Context, egui::Rect) {
        let ctx = egui::Context::default();
        ctx.set_fonts(crate::fonts::definitions(None, None, None));
        crate::theme::install(&ctx, 20.0, 24.0);
        ctx.enable_accesskit();
        let screen =
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::new(720.0, 480.0));
        (ctx, screen)
    }

    fn screen_input(screen: egui::Rect) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(screen),
            ..Default::default()
        }
    }

    fn assert_inside_screen(screen: egui::Rect, bounds: egui::accesskit::Rect, what: &str) {
        assert!(
            bounds.x0 >= f64::from(screen.left())
                && bounds.x1 <= f64::from(screen.right())
                && bounds.y0 >= f64::from(screen.top())
                && bounds.y1 <= f64::from(screen.bottom()),
            "{what} {bounds:?} escaped {screen:?}",
        );
    }

    #[test]
    fn settings_drawer_stays_inside_its_supported_narrow_width() {
        for ui_pt in [13.0, 20.0] {
            let mut app = test_app();
            let ctx = egui::Context::default();
            ctx.set_fonts(crate::fonts::definitions(None, None, None));
            crate::theme::install(&ctx, ui_pt, 12.5);
            ctx.enable_accesskit();
            let mut drawer_rect = egui::Rect::NOTHING;
            let mut left = 0.0;
            let mut output = ctx.run_ui(Default::default(), |ui| {
                ui.set_max_width(208.0);
                left = ui.cursor().left();
                ui.set_clip_rect(egui::Rect::from_min_size(
                    ui.cursor().left_top(),
                    egui::Vec2::new(208.0, ui.clip_rect().height()),
                ));
                app.settings_drawer(ui);
                drawer_rect = ui.min_rect();
            });
            // Unwrapped, not iterated: `as_ref().into_iter()` over a `None`
            // update is an empty scan, and an empty scan finds no offenders.
            // Every assertion below would then pass without having looked at
            // anything.
            let update = output
                .platform_output
                .accesskit_update
                .as_ref()
                .expect("AccessKit tree update");
            let past = |edge: &dyn Fn(&egui::accesskit::Rect) -> bool| {
                update
                    .nodes
                    .iter()
                    .filter_map(|(_, node)| {
                        let bounds = node.bounds()?;
                        edge(&bounds)
                            .then(|| node.label().or(node.value()).unwrap_or("").to_owned())
                    })
                    .collect::<Vec<_>>()
            };
            let right_edge = f64::from(left + 208.0);
            let left_edge = f64::from(left);
            let offenders = past(&|b| b.x1 > right_edge);
            let clipped_left = past(&|b| b.x0 < left_edge);
            output.textures_delta.clear();
            assert!(drawer_rect.left() >= left);
            assert!(
                clipped_left.is_empty(),
                "{ui_pt} pt settings content was clipped on the left: {clipped_left:?}",
            );
            // At the default type scale, nothing overflows -- and this is the
            // assertion with teeth. `drawer_rect` has none: the drawer's scroll
            // area does not auto-shrink, so its `min_rect` is the width it was
            // given whatever it holds, which is why the widgets themselves have
            // to be asked where they ended up.
            //
            // At the largest UI font the drawer does not promise this, and
            // asserting it would be asserting against the design: a native
            // combo box or a checkbox with a long label has no narrower form to
            // take, which is why `settings_drawer` enables the horizontal axis
            // and calls it a last resort. Note egui widens a `Ui` to fit an
            // over-wide child (`Region::expand_to_include_rect`), so the first
            // control that overflows takes the wrapped prose after it along.
            if ui_pt == 13.0 {
                assert!(
                    offenders.is_empty(),
                    "{ui_pt} pt settings content overflowed {}: {offenders:?}",
                    left + 208.0,
                );
            }
            assert!(
                drawer_rect.right() <= left + 208.0,
                "{ui_pt} pt settings rect {drawer_rect:?} exceeded {}",
                left + 208.0,
            );
        }
    }

    #[test]
    fn help_is_a_named_scrollable_modal_with_a_visible_close_action() {
        let mut app = test_app();
        app.show_help = true;
        let ctx = egui::Context::default();
        ctx.set_fonts(crate::fonts::definitions(None, None, None));
        crate::theme::install(&ctx, 20.0, 24.0);
        ctx.enable_accesskit();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::new(720.0, 480.0));
        let input = egui::RawInput {
            screen_rect: Some(screen),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |_ui| app.help_modal(&ctx));
        let update = output
            .platform_output
            .accesskit_update
            .take()
            .expect("AccessKit tree update");
        output.textures_delta.clear();

        let dialog = update
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Keyboard and gestures"))
            .expect("named help dialog");
        assert_eq!(dialog.1.role(), egui::accesskit::Role::Dialog);
        assert!(dialog.1.is_modal());

        let close = update
            .nodes
            .iter()
            .find(|(_, node)| {
                node.label()
                    .is_some_and(|label| label == "Close" || label.starts_with("Close "))
            })
            .and_then(|(_, node)| node.bounds())
            .unwrap_or_else(|| {
                let available = update
                    .nodes
                    .iter()
                    .filter_map(|(_, node)| {
                        node.label()
                            .or(node.value())
                            .map(|text| (node.role(), text.to_owned(), node.bounds()))
                    })
                    .collect::<Vec<_>>();
                panic!("bounded Close action; available: {available:?}")
            });
        assert!(
            close.x0 >= f64::from(screen.left())
                && close.x1 <= f64::from(screen.right())
                && close.y0 >= f64::from(screen.top())
                && close.y1 <= f64::from(screen.bottom()),
            "Close action {close:?} escaped the 720×480 viewport",
        );
        assert!(
            update
                .nodes
                .iter()
                .any(|(_, node)| node.role() == egui::accesskit::Role::ScrollView),
            "long help contents have no accessible scroll region",
        );
        let safe = app.modal_focus.expect("help established a focus contract").2;
        assert_eq!(ctx.memory(|memory| memory.focused()), Some(safe));
    }

    #[test]
    fn destructive_alert_keeps_actions_visible_with_long_names_at_max_font() {
        let mut app = test_app();
        while app.panels.len() < MAX_PANELS {
            app.add_panel();
        }
        for (i, panel) in app.panels.iter_mut().enumerate() {
            panel.path = Some(PathBuf::from(format!(
                "/root/{}/same.rs",
                format!("segment-{i}-").repeat(28),
            )));
            panel.text = "unsaved\n".into();
            panel.edited = true;
            panel.resniff();
        }
        app.request_destructive(DestructiveAction::LoadFiles(
            (0..MAX_PANELS)
                .map(|i| (i, PathBuf::from(format!("replacement-{i}"))))
                .collect(),
        ));

        let (ctx, screen) = modal_test_context();
        let mut output = ctx.run_ui(screen_input(screen), |_ui| app.destructive_modal(&ctx));
        let update = output
            .platform_output
            .accesskit_update
            .take()
            .expect("AccessKit tree update");
        output.textures_delta.clear();

        let dialog = update
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Replace unsaved panel contents?"))
            .expect("action-specific destructive alert");
        assert_eq!(dialog.1.role(), egui::accesskit::Role::AlertDialog);
        assert!(dialog.1.is_modal());
        assert_inside_screen(
            screen,
            dialog.1.bounds().expect("destructive alert bounds"),
            "destructive alert",
        );

        for label in ["Replace panels", "Cancel"] {
            let (id, node) = update
                .nodes
                .iter()
                .find(|(_, node)| {
                    node.label()
                        .is_some_and(|name| name == label || name.starts_with(&format!("{label} ")))
                })
                .unwrap_or_else(|| panic!("visible {label} action"));
            assert_inside_screen(
                screen,
                node.bounds().unwrap_or_else(|| panic!("{label} bounds")),
                label,
            );
            if label == "Cancel" {
                let safe = app.modal_focus.expect("destructive focus contract").2;
                assert_eq!(*id, safe.accesskit_id());
                assert_eq!(ctx.memory(|memory| memory.focused()), Some(safe));
            }
        }
        let details = update
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::ScrollView
                    && node.label() == Some("Destructive action details")
            })
            .expect("accessible destructive details");
        assert_inside_screen(
            screen,
            details.1.bounds().expect("destructive details bounds"),
            "destructive details",
        );
    }

    #[test]
    fn quit_alert_keeps_actions_visible_with_a_long_git_target_at_max_font() {
        let dir = temp_path("long-quit-modal");
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("base");
        let local = dir.join("local");
        let remote = dir.join("remote");
        for (path, text) in [(&base, "base\n"), (&local, "local\n"), (&remote, "remote\n")] {
            std::fs::write(path, text).unwrap();
        }
        let merged = PathBuf::from(format!(
            "/tmp/{}/MERGED",
            "very-long-segment/".repeat(70),
        ));
        let (mut app, _) = test_mergetool_app(&[base, local, remote], merged);
        let result = app.result_panel().unwrap();
        app.panels[result].text = "unwritten result\n".into();
        app.panels[result].dirty = true;
        app.quit_guard = true;

        let (ctx, screen) = modal_test_context();
        let mut output = ctx.run_ui(screen_input(screen), |_ui| app.quit_guard(&ctx));
        let update = output
            .platform_output
            .accesskit_update
            .take()
            .expect("AccessKit tree update");
        output.textures_delta.clear();

        let dialog = update
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Unresolved Git merge"))
            .expect("unresolved merge alert");
        assert_inside_screen(
            screen,
            dialog.1.bounds().expect("quit alert bounds"),
            "quit alert",
        );
        for label in [
            "Save to Git target and quit",
            "Quit unresolved",
            "Keep working",
        ] {
            let (id, node) = update
                .nodes
                .iter()
                .find(|(_, node)| {
                    node.label()
                        .is_some_and(|name| name == label || name.starts_with(&format!("{label} ")))
                })
                .unwrap_or_else(|| panic!("visible {label} action"));
            assert_inside_screen(
                screen,
                node.bounds().unwrap_or_else(|| panic!("{label} bounds")),
                label,
            );
            if label == "Keep working" {
                let safe = app.modal_focus.expect("quit focus contract").2;
                assert_eq!(*id, safe.accesskit_id());
                assert_eq!(ctx.memory(|memory| memory.focused()), Some(safe));
            }
        }
        let details = update
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::ScrollView
                    && node.label() == Some("Unsaved work details")
            })
            .expect("accessible unsaved-work details");
        assert_inside_screen(
            screen,
            details.1.bounds().expect("quit details bounds"),
            "quit details",
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn quit_alert_honors_its_displayed_save_shortcut() {
        let dir = temp_path("quit-save-shortcut");
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("base");
        let local = dir.join("local");
        let remote = dir.join("remote");
        for (path, text) in [(&base, "base\n"), (&local, "local\n"), (&remote, "remote\n")] {
            std::fs::write(path, text).unwrap();
        }
        let merged = dir.join("MERGED");
        let (mut app, resolved) =
            test_mergetool_app(&[base, local, remote], merged.clone());
        let result = app.result_panel().unwrap();
        app.panels[result].text = "saved by shortcut\n".into();
        app.panels[result].dirty = true;
        app.quit_guard = true;

        let (ctx, screen) = modal_test_context();
        let mut first = ctx.run_ui(screen_input(screen), |_ui| app.quit_guard(&ctx));
        first.textures_delta.clear();

        let mut input = screen_input(screen);
        input
            .events
            .push(egui::Event::ModifiersChanged(egui::Modifiers::COMMAND));
        input.events.push(egui::Event::Key {
            key: Key::S,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        let mut output = ctx.run_ui(input, |_ui| app.quit_guard(&ctx));
        assert_eq!(std::fs::read_to_string(&merged).unwrap(), "saved by shortcut\n");
        assert!(app.closing);
        assert!(!app.quit_guard);
        assert!(resolved.load(Ordering::Relaxed));
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::Close),
        );
        output.textures_delta.clear();

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn destructive_alert_restores_a_stable_editor_after_cancel() {
        let mut app = test_app();
        app.panels[0].text = "unsaved\n".into();
        app.panels[0].resniff();
        app.request_destructive(DestructiveAction::Clear(0));
        let stable = panel_edit_id(0);
        let transient = egui::Id::new("vanished-popup-item");

        let (ctx, screen) = modal_test_context();
        ctx.memory_mut(|memory| memory.request_focus(transient));
        let render = |ui: &mut egui::Ui, app: &mut App| {
            ui.interact(
                egui::Rect::from_min_size(egui::pos2(8.0, 8.0), egui::vec2(40.0, 24.0)),
                stable,
                egui::Sense::click(),
            );
            app.destructive_modal(&ctx);
        };

        let mut first = ctx.run_ui(screen_input(screen), |ui| render(ui, &mut app));
        let safe = app.modal_focus.expect("destructive focus contract").2;
        assert_eq!(ctx.memory(|memory| memory.focused()), Some(safe));
        first.textures_delta.clear();

        let mut escape = screen_input(screen);
        escape.events.push(egui::Event::Key {
            key: Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        let mut second = ctx.run_ui(escape, |ui| render(ui, &mut app));
        assert!(app.destructive.is_none());
        second.textures_delta.clear();

        let mut third = ctx.run_ui(screen_input(screen), |ui| render(ui, &mut app));
        assert_eq!(ctx.memory(|memory| memory.focused()), Some(stable));
        third.textures_delta.clear();

        let mut fourth = ctx.run_ui(screen_input(screen), |ui| render(ui, &mut app));
        assert_eq!(
            ctx.memory(|memory| memory.focused()),
            Some(stable),
            "restored focus survived egui's dead-man pass",
        );
        fourth.textures_delta.clear();
    }

    /// The drawer's own default width has to be one the fields fit in side by
    /// side, or the measurement that decides it is doing nothing: a drawer that
    /// stacks at its default width stacks always, until the user drags it.
    #[test]
    fn the_settings_drawer_aligns_its_fields_at_its_default_width() {
        let mut app = test_app();
        let ctx = egui::Context::default();
        ctx.set_fonts(crate::fonts::definitions(None, None, None));
        crate::theme::install(&ctx, crate::settings::DEFAULT_UI_PT, 12.5);
        ctx.enable_accesskit();
        let mut output = ctx.run_ui(Default::default(), |ui| {
            // What `settings_drawer` is handed inside the panel: the default
            // width less the frame's symmetric margin.
            ui.set_max_width(SETTINGS_DRAWER_WIDTH - 32.0);
            app.settings_drawer(ui);
        });
        // Cleared before anything that can fail: dropping an unapplied
        // `TexturesDelta` panics in a destructor, which aborts the process and
        // hides whatever the real failure was.
        output.textures_delta.clear();
        let update = output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("AccessKit tree update");
        // A plain label's text is its node's *value*; a button's is its name.
        let first = |text: &str| {
            update
                .nodes
                .iter()
                .filter(|(_, node)| node.value() == Some(text) || node.label() == Some(text))
                .filter_map(|(_, node)| node.bounds())
                .min_by(|a, b| a.y0.total_cmp(&b.y0))
                .unwrap_or_else(|| panic!("no node saying {text:?}"))
        };
        // The first "Theme" is the appearance one; its control is the theme
        // choice, whose first button is `ThemeChoice::System`.
        let label = first("Theme");
        let control = first(ThemeChoice::ALL[0].label());
        assert!(
            label.y0 < control.y1 && control.y0 < label.y1,
            "the label spans {}..{} and its control {}..{}: stacked, not aligned",
            label.y0,
            label.y1,
            control.y0,
            control.y1,
        );
    }

    fn test_mergetool_app(files: &[PathBuf], merged: PathBuf) -> (App, Arc<AtomicBool>) {
        let resolved = Arc::new(AtomicBool::new(false));
        let app = App::new(
            Delta {
                path: PathBuf::from("delgui-test-delta-does-not-exist"),
                version: (0, 19, 0),
                version_string: "delta test".into(),
            },
            Settings::default(),
            Launch {
                files: files.to_vec(),
                mergetool: Some(MergeTool {
                    merged,
                    resolved: Arc::clone(&resolved),
                }),
                ..Launch::default()
            },
        );
        (app, resolved)
    }

    /// Git's three inputs go in by position, and an *empty* ancestor is an
    /// ordinary conflict -- both sides added the file. Filled by "first empty
    /// panel", as every other launch path does, "ours" would land in the
    /// ancestor's panel and every difference would be measured from the wrong
    /// side.
    #[test]
    fn git_s_files_keep_their_positions_even_when_the_ancestor_is_empty() {
        let dir = temp_path("mergetool-layout");
        std::fs::create_dir_all(&dir).unwrap();
        let (base, local, remote) = (dir.join("b"), dir.join("l"), dir.join("r"));
        std::fs::write(&base, "").unwrap();
        std::fs::write(&local, "ours\n").unwrap();
        std::fs::write(&remote, "theirs\n").unwrap();
        let merged = dir.join("merged.txt");

        let (app, resolved) = test_mergetool_app(
            &[base.clone(), local.clone(), remote.clone()],
            merged.clone(),
        );

        assert_eq!(app.panels[0].path.as_ref(), Some(&base));
        assert_eq!(app.panels[1].path.as_ref(), Some(&local));
        assert_eq!(app.panels[2].path.as_ref(), Some(&remote));
        // The ancestor seeds the result, which is then the baseline: each side's
        // changes are differences to take, rather than a diff against git's own
        // half-merged file and its conflict markers.
        let result = app.result_panel().expect("a result to merge into");
        assert_eq!(app.reference, result);
        assert!(app.merging());
        assert!(
            app.panels[result].text.is_empty(),
            "seeded from an empty base"
        );
        // Nothing has been written, so git is told nothing was resolved.
        assert!(!resolved.load(Ordering::Relaxed));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Git's destination is already the primary Save target on the first frame,
    /// and a successful save must reach the shared exit flag before another
    /// frame gets a chance to publish it.
    #[test]
    fn mergetool_first_save_uses_git_s_target_and_resolves_immediately() {
        let dir = temp_path("mergetool-first-save");
        std::fs::create_dir_all(&dir).unwrap();
        let (base, local, remote) = (dir.join("b"), dir.join("l"), dir.join("r"));
        std::fs::write(&base, "before\n").unwrap();
        std::fs::write(&local, "ours\n").unwrap();
        std::fs::write(&remote, "theirs\n").unwrap();
        let merged = dir.join("conflicted.txt");

        let (mut app, resolved) = test_mergetool_app(&[base, local, remote], merged.clone());
        let result = app.result_panel().expect("a result to merge into");
        app.panels[result].text = "resolved\n".into();
        app.panels[result].dirty = true;

        assert_eq!(
            app.result_save_target(result).as_deref(),
            Some(merged.as_path()),
            "the primary Save control must not ask for another path",
        );
        assert!(app.save_result(false));
        assert_eq!(std::fs::read_to_string(&merged).unwrap(), "resolved\n");
        assert!(
            resolved.load(Ordering::Relaxed),
            "save-and-quit can close before the next frame",
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mergetool_export_does_not_retarget_plain_save_or_resolve_git() {
        let dir = temp_path("mergetool-export");
        std::fs::create_dir_all(&dir).unwrap();
        let (base, local, remote) = (dir.join("b"), dir.join("l"), dir.join("r"));
        std::fs::write(&base, "base\n").unwrap();
        std::fs::write(&local, "ours\n").unwrap();
        std::fs::write(&remote, "theirs\n").unwrap();
        let merged = dir.join("MERGED");
        let exported = dir.join("copy.txt");

        let (mut app, resolved) =
            test_mergetool_app(&[base, local, remote], merged.clone());
        let result = app.result_panel().unwrap();
        assert_eq!(app.alternate_result_save_label(), "Export copy…");
        app.panels[result].text = "resolved text\n".into();
        app.panels[result].dirty = true;

        assert!(app.save_result_to(result, exported.clone(), true));
        assert_eq!(
            std::fs::read_to_string(&exported).unwrap(),
            "resolved text\n"
        );
        assert!(!merged.exists());
        assert_eq!(
            app.result_save_target(result).as_deref(),
            Some(merged.as_path())
        );
        assert!(app.panels[result].dirty);
        assert!(app.panels[result].saved_snapshot.is_none());
        assert!(app.unsaved_panels().contains(&result));
        assert!(!resolved.load(Ordering::Relaxed));

        assert!(app.save_result(false));
        assert_eq!(
            std::fs::read_to_string(&merged).unwrap(),
            "resolved text\n"
        );
        assert!(!app.panels[result].dirty);
        assert!(resolved.load(Ordering::Relaxed));

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_unwritten_empty_mergetool_result_cancels_close() {
        let dir = temp_path("mergetool-unresolved-close");
        std::fs::create_dir_all(&dir).unwrap();
        let (base, local, remote) = (dir.join("b"), dir.join("l"), dir.join("r"));
        std::fs::write(&base, "").unwrap();
        std::fs::write(&local, "ours\n").unwrap();
        std::fs::write(&remote, "theirs\n").unwrap();
        let merged = dir.join("MERGED");

        let (mut app, resolved) = test_mergetool_app(&[base, local, remote], merged);
        let result = app.result_panel().unwrap();
        assert!(!app.panels[result].dirty);
        assert!(app.merge_unresolved());
        assert!(!resolved.load(Ordering::Relaxed));

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let mut output = ctx.run_ui(input, |ctx| app.quit_guard(ctx));

        assert!(app.quit_guard);
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::CancelClose)
        );
        let update = output
            .platform_output
            .accesskit_update
            .take()
            .expect("AccessKit tree update");
        let dialog = update
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Unresolved Git merge"))
            .expect("named unresolved-merge alert dialog");
        assert_eq!(dialog.1.role(), egui::accesskit::Role::AlertDialog);
        assert!(dialog.1.is_modal());
        output.textures_delta.clear();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_unresolved_mergetool_without_a_result_offers_no_impossible_save() {
        let dir = temp_path("mergetool-no-result-close");
        std::fs::create_dir_all(&dir).unwrap();
        let (base, local, remote) = (dir.join("b"), dir.join("l"), dir.join("r"));
        std::fs::write(&base, "base\n").unwrap();
        std::fs::write(&local, "ours\n").unwrap();
        std::fs::write(&remote, "theirs\n").unwrap();
        let merged = dir.join("MERGED");

        let (mut app, _) = test_mergetool_app(&[base, local, remote], merged);
        let result = app.result_panel().unwrap();
        app.remove_panel(result);
        assert!(app.result_panel().is_none());

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let mut output = ctx.run_ui(input, |ctx| app.quit_guard(ctx));
        let update = output
            .platform_output
            .accesskit_update
            .take()
            .expect("AccessKit tree update");

        assert!(app.quit_guard);
        assert!(
            !update.nodes.iter().any(|(_, node)| {
                node.label()
                    .is_some_and(|label| label.starts_with("Save to Git target"))
            }),
            "there is no result for the advertised Save action"
        );
        assert!(
            update
                .nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Quit unresolved"))
        );
        output.textures_delta.clear();

        let notice = app.notice.clone();
        let mut save_input = egui::RawInput::default();
        save_input
            .events
            .push(egui::Event::ModifiersChanged(egui::Modifiers::COMMAND));
        save_input.events.push(egui::Event::Key {
            key: Key::S,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        let mut save_output = ctx.run_ui(save_input, |ctx| app.quit_guard(ctx));
        assert!(app.quit_guard);
        assert!(!app.closing);
        assert_eq!(app.notice, notice, "the hidden Save action was not dispatched");
        assert!(
            !save_output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::Close)
        );
        save_output.textures_delta.clear();

        std::fs::remove_dir_all(dir).ok();
    }

    /// The question git asks on exit is whether the file it handed over holds
    /// the merge *now* -- not whether a save ever happened. Saving and then
    /// typing more leaves it unresolved again.
    #[test]
    fn git_is_told_the_conflict_is_resolved_only_while_it_actually_is() {
        let dir = temp_path("mergetool-exit");
        std::fs::create_dir_all(&dir).unwrap();
        let (base, local) = (dir.join("b"), dir.join("l"));
        std::fs::write(&base, "one\n").unwrap();
        std::fs::write(&local, "two\n").unwrap();
        let merged = dir.join("conflicted.txt");

        let (mut app, resolved) = test_mergetool_app(&[base, local.clone(), local], merged.clone());
        let result = app.result_panel().expect("a result to merge into");

        app.publish_resolution();
        assert!(!resolved.load(Ordering::Relaxed), "nothing written yet");

        app.panels[result].text = "merged\n".into();
        app.panels[result].dirty = true;
        assert!(app.save_result(false));
        assert!(resolved.load(Ordering::Relaxed), "written where git asked");

        app.panels[result].dirty = true;
        app.publish_resolution();
        assert!(!resolved.load(Ordering::Relaxed), "edited since");

        // Saved, but somewhere else: git's file is still the conflicted one.
        app.panels[result].dirty = false;
        app.panels[result].saved_to = Some(dir.join("elsewhere.txt"));
        app.publish_resolution();
        assert!(!resolved.load(Ordering::Relaxed), "not git's file");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_external_git_target_replacement_withdraws_resolution_and_guards_close() {
        let dir = temp_path("mergetool-external-replacement");
        std::fs::create_dir_all(&dir).unwrap();
        let (base, local, remote) = (dir.join("b"), dir.join("l"), dir.join("r"));
        std::fs::write(&base, "base\n").unwrap();
        std::fs::write(&local, "ours\n").unwrap();
        std::fs::write(&remote, "theirs\n").unwrap();
        let merged = dir.join("MERGED");
        let (mut app, resolved) =
            test_mergetool_app(&[base, local, remote], merged.clone());
        let result = app.result_panel().unwrap();
        app.panels[result].text = "resolved text\n".into();
        app.panels[result].dirty = true;
        assert!(app.save_result(false));
        assert!(resolved.load(Ordering::Relaxed));

        std::fs::write(&merged, "external replacement\n").unwrap();
        app.publish_resolution();
        assert!(app.merge_unresolved());
        assert!(app.merge_unresolved_on_disk());
        assert!(
            !resolved.load(Ordering::Relaxed),
            "an externally replaced MERGED file must not keep the success exit state",
        );

        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let mut output = ctx.run_ui(input, |ctx| app.quit_guard(ctx));
        assert!(app.quit_guard);
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::CancelClose)
        );
        output.textures_delta.clear();

        std::fs::remove_dir_all(dir).ok();
    }

    #[derive(Default)]
    struct TestStorage(std::collections::BTreeMap<String, String>);

    impl eframe::Storage for TestStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }

        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }

        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }

        fn flush(&mut self) {}
    }

    #[test]
    fn disabled_gitconfig_never_reactivates_saved_features() {
        let app = test_app_with_settings(Settings {
            inherit_gitconfig: false,
            features: Some(vec!["private-preset".into()]),
            ..Settings::default()
        });
        assert!(app.features.is_empty());
        assert!(!app.opts.inherit_gitconfig);
        assert!(
            !app.effective_options()
                .to_args()
                .iter()
                .any(|arg| arg.contains("private-preset"))
        );
    }

    #[test]
    fn an_explicit_empty_feature_selection_survives_startup() {
        let app = test_app_with_settings(Settings {
            features: Some(Vec::new()),
            ..Settings::default()
        });
        assert!(app.features.is_empty());
    }

    #[test]
    fn a_saved_feature_removed_from_gitconfig_is_disabled() {
        let app = test_app_with_settings(Settings {
            features: Some(vec!["delgui-test-feature-that-does-not-exist".into()]),
            ..Settings::default()
        });
        assert!(app.features.is_empty());
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("no longer defined"))
        );
    }

    #[test]
    fn selected_features_are_persisted() {
        let mut app = test_app();
        app.features = ["chosen".to_string()].into_iter().collect();
        let mut storage = TestStorage::default();

        eframe::App::save(&mut app, &mut storage);
        let loaded = Settings::load(Some(&storage));

        assert_eq!(loaded.features, Some(vec!["chosen".into()]));
    }

    #[test]
    fn app_starts_with_the_fonts_main_already_installed() {
        let settings = Settings {
            ui_pt: 15.0,
            mono_pt: 14.0,
            ..Settings::default()
        };
        let expected = (
            settings.ui_font.clone(),
            settings.ui_font_strong.clone(),
            settings.mono_font.clone(),
            15.0,
            14.0,
        );
        let app = test_app_with_settings(settings);
        assert_eq!(app.installed, expected);
    }

    #[test]
    fn panel_line_detail_is_updated_with_content_metadata() {
        let mut panel = Panel::empty();
        panel.text = "one\ntwo\nthree\n".into();
        panel.resniff();
        assert_eq!(panel.line_count, 3);
        assert_eq!(panel.detail().as_deref(), Some("3 lines"));
    }

    #[test]
    fn navigation_clamps_a_cursor_left_over_from_a_longer_render() {
        assert_eq!(moved_cursor(99, 2, -1), 0);
        assert_eq!(moved_cursor(99, 2, 1), 0);
    }

    fn find_test_pass(app: &mut App, ctx: &egui::Context, events: Vec<egui::Event>) {
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(720.0, 480.0));
        let mut input = screen_input(screen);
        input.events = events;
        let mut output = ctx.run_ui(input, |ui| app.diff_area(ui, ctx, 8.0));
        output.textures_delta.clear();
    }

    #[test]
    fn refining_find_targets_the_current_query_and_keeps_navigation_working() {
        let mut app = test_app();
        let text = (0..160)
            .map(|i| match i {
                0 => "a\n".to_string(),
                80 | 120 => "ab\n".to_string(),
                _ => format!("row {i}\n"),
            })
            .collect::<String>();
        app.cache.insert(app.shown, Cached {
            key: app.current_key(),
            lines: ansi::parse(text.as_bytes()),
            columns: app.columns,
            hunks: Vec::new(),
            problem: None,
        });
        app.show_find = true;
        app.find_query = "a".into();
        app.focus_find = true;
        let ctx = egui::Context::default();
        ctx.set_fonts(crate::fonts::definitions(None, None, None));
        crate::theme::install(&ctx, crate::settings::DEFAULT_UI_PT, app.settings.mono_pt);
        for _ in 0..3 {
            find_test_pass(&mut app, &ctx, Vec::new());
        }

        // Exercise TextEdit's mutation in the same frame as searching. Setting
        // find_query before the frame would miss the original ordering defect.
        find_test_pass(&mut app, &ctx, vec![egui::Event::Text("b".into())]);
        assert_eq!(app.find_query, "ab");
        let row_height = app.settings.mono_pt * 1.36;
        assert_eq!(app.restore_offset, Some(80.0 * row_height));
        for _ in 0..3 {
            find_test_pass(&mut app, &ctx, Vec::new());
        }
        assert_eq!(app.diff_offset, 80.0 * row_height);

        app.pending_find_move = -1;
        find_test_pass(&mut app, &ctx, Vec::new());
        assert_eq!(app.find_cursor, 1);
        assert_eq!(app.restore_offset, Some(120.0 * row_height));
        app.pending_find_move = 1;
        find_test_pass(&mut app, &ctx, Vec::new());
        assert_eq!(app.find_cursor, 0);
        assert_eq!(app.restore_offset, Some(80.0 * row_height));
    }

    #[test]
    fn find_in_a_result_uses_the_drawn_body_after_hunk_controls() {
        for (ui_pt, mono_pt) in [(13.0, 12.5), (20.0, 24.0)] {
            let hunks = (0..60)
                .map(|i| (hunk(i..i + 1, i..i + 1), 2 * i..2 * i + 2))
                .collect();
            let mut app = merging_app("base\n", "candidate\n", hunks);
            app.settings.mono_pt = mono_pt;
            let text = (0..60)
                .map(|i| {
                    if i == 40 {
                        "-TARGET\n+new\n".to_string()
                    } else {
                        format!("-old {i}\n+new {i}\n")
                    }
                })
                .collect::<String>();
            app.cache.get_mut(&1).unwrap().lines = ansi::parse(text.as_bytes());
            app.show_find = true;
            let ctx = egui::Context::default();
            ctx.set_fonts(crate::fonts::definitions(None, None, None));
            crate::theme::install(&ctx, ui_pt, mono_pt);
            for _ in 0..3 {
                find_test_pass(&mut app, &ctx, Vec::new());
            }

            app.find_query = "TARGET".into();
            app.find_jump = true;
            find_test_pass(&mut app, &ctx, Vec::new());
            let (top, total) = app.hunk_boxes[40];
            let body_height = 2.0 * mono_pt * 1.36;
            let expected = top + total - body_height;
            assert!((app.restore_offset.unwrap() - expected).abs() < 1.0);
            for _ in 0..3 {
                find_test_pass(&mut app, &ctx, Vec::new());
            }
            assert!((app.diff_offset - expected).abs() < 1.0);
        }
    }

    fn temp_path(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        std::env::temp_dir().join(format!("delgui-{label}-{}-{nonce}", std::process::id()))
    }

    fn file_panel(text: &str) -> Panel {
        let mut p = Panel::empty();
        p.text = text.to_string();
        p.path = Some(PathBuf::from("/tmp/thing.rs"));
        p.resniff();
        p
    }

    #[test]
    fn opening_a_selected_result_preserves_contents_and_save_identity() {
        let dir = temp_path("open-selected-result");
        std::fs::create_dir_all(&dir).unwrap();
        let opened = dir.join("new-input.txt");
        std::fs::write(&opened, "replacement input\n").unwrap();

        for mergetool in [false, true] {
            for dirty in [false, true] {
                let saved = dir.join(format!("result-{mergetool}-{dirty}.txt"));
                let resolved = Arc::new(AtomicBool::new(false));
                let mut app = test_app();
                app.panels[0].text = "seed\n".into();
                app.panels[1].text = "candidate\n".into();
                app.start_result(Some(0));
                let result = app.result_panel().unwrap();
                if mergetool {
                    app.mergetool = Some(MergeTool {
                        merged: saved.clone(),
                        resolved: resolved.clone(),
                    });
                }
                assert!(app.save_result_to(result, saved.clone(), true));
                if dirty {
                    app.panels[result].text.push_str("manual edit\n");
                    app.result_edited(result);
                }
                app.stop_building();
                app.set_reference(0);
                app.shown = result;
                app.publish_resolution();
                let before = (
                    app.panels[result].text.clone(),
                    app.panels[result].saved_snapshot.clone(),
                    app.panels[result].saved_stamp,
                    app.panels[result].revision,
                    app.result_identity_view(result).state,
                );

                // This is the native Open shortcut route. The result guard
                // must return before a file dialog is created.
                let ctx = egui::Context::default();
                let mut input = egui::RawInput::default();
                input.events.push(egui::Event::ModifiersChanged(egui::Modifiers::COMMAND));
                input.events.push(egui::Event::Key {
                    key: Key::O,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::COMMAND,
                });
                let mut output = ctx.run_ui(input, |_ui| {
                    let actions = app.capture_keys(&ctx);
                    app.handle_keys(&ctx, actions);
                });
                output.textures_delta.clear();
                assert!(app.notice.as_deref().is_some_and(|text| text.contains("input panel")));
                assert!(app.destructive.is_none());

                // A caller bypassing the dialog still cannot bind a result to
                // an input path or leave unrelated text claiming to be saved.
                app.open_into(result, opened.clone());
                app.publish_resolution();
                assert_eq!(app.panels[result].text, before.0);
                assert_eq!(app.panels[result].saved_snapshot, before.1);
                assert!(app.panels[result].saved_stamp == before.2);
                assert_eq!(app.panels[result].revision, before.3);
                assert_eq!(app.result_identity_view(result).state, before.4);
                assert_eq!(app.panels[result].saved_to.as_ref(), Some(&saved));
                assert_eq!(app.result_save_target(result).as_ref(), Some(&saved));
                assert!(app.panels[result].path.is_none());
                assert!(app.panels[result].result);
                assert_eq!(app.panels[result].dirty, dirty);
                assert_eq!(app.panel_has_unsaved_content(result), dirty);
                assert_eq!(resolved.load(Ordering::Relaxed), mergetool && !dirty);
                assert_eq!(std::fs::read_to_string(&saved).unwrap(), "seed\n");
            }
        }

        let mut app = test_app();
        app.open_into(1, opened.clone());
        assert_eq!(app.panels[1].text, "replacement input\n");
        assert_eq!(app.panels[1].path.as_ref(), Some(&opened));
        assert_eq!(app.focus_panel, Some(1));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn explicit_file_pairs_compare_even_when_one_or_both_files_are_empty() {
        let dir = temp_path("empty-file-comparison");
        std::fs::create_dir_all(&dir).unwrap();
        let left = dir.join("left.txt");
        let right = dir.join("right.txt");
        let delta = Delta::discover().expect("the test suite needs delta");
        for (a, b) in [("", "added\n"), ("deleted\n", ""), ("", "")] {
            std::fs::write(&left, a).unwrap();
            std::fs::write(&right, b).unwrap();
            let mut app = App::new(
                delta.clone(),
                Settings { inherit_gitconfig: false, ..Settings::default() },
                Launch { files: vec![left.clone(), right.clone()], ..Launch::default() },
            );
            assert!(app.compared, "explicit file pair {a:?} / {b:?} waits for Compare");
            assert_eq!(app.panels[0].path.as_ref(), Some(&left));
            assert_eq!(app.panels[1].path.as_ref(), Some(&right));
            let cached = render_job(
                &delta,
                &app.panels[0].to_input(true),
                &app.panels[1].to_input(true),
                &app.effective_options(),
                app.current_key(),
                app.columns,
                false,
            ).unwrap();
            assert_eq!(render::is_empty(&cached.lines), a == b);
            if a != b {
                let change = &cached.hunks[0].0;
                assert_eq!(change.old.len(), usize::from(!a.is_empty()));
                assert_eq!(change.new.len(), usize::from(!b.is_empty()));
            }

            let ctx = egui::Context::default();
            ctx.enable_accesskit();
            let mut output = ctx.run_ui(Default::default(), |ui| app.toolbar(ui, &ctx));
            output.textures_delta.clear();
            let update = output.platform_output.accesskit_update.unwrap();
            let compare = update.nodes.iter().find(|(_, node)| {
                node.label().is_some_and(|label| label.starts_with("Compare"))
            }).expect("Compare action");
            assert!(!compare.1.is_disabled(), "empty files must remain retryable");
        }

        let startup = |files, preload| App::new(
            delta.clone(), Settings::default(),
            Launch { files, preload, ..Launch::default() },
        );
        assert!(!startup(Vec::new(), None).compared);
        assert!(!startup(vec![left], None).compared);
        assert!(!startup(Vec::new(), Some("clipboard only".into())).compared);
        assert!(startup(vec![right], Some("clipboard content".into())).compared);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn toolbar_controls_share_a_center_at_normal_and_large_font_sizes() {
        for width in [1_240.0, 720.0] {
            for ui_pt in [13.0, 20.0] {
                for state in ["empty", "ready", "rendering"] {
                    let mut app = test_app();
                    if state != "empty" {
                        app.panels[0].text = "left".into();
                        app.panels[1].text = "right".into();
                    }
                    if state == "rendering" {
                        app.in_flight = Some(app.current_key());
                    }
                    let ctx = egui::Context::default();
                    ctx.set_fonts(crate::fonts::definitions(None, None, None));
                    crate::theme::install(&ctx, ui_pt, 12.5);
                    ctx.enable_accesskit();
                    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 480.0));
                    let mut output = ctx.run_ui(screen_input(screen), |ui| app.toolbar(ui, &ctx));
                    output.textures_delta.clear();
                    let update = output.platform_output.accesskit_update.unwrap();
                    let bounds = |name: &str| {
                        update.nodes.iter().find_map(|(_, node)| {
                            node.label().filter(|label| label.starts_with(name))
                                .and_then(|_| node.bounds())
                        }).unwrap_or_else(|| panic!("missing toolbar action {name}"))
                    };
                    let compare = bounds(if state == "rendering" { "Rendering…" } else { "Compare" });
                    let center = (compare.y0 + compare.y1) / 2.0;
                    for name in ["Side by side", "Numbers", "Wrap", "Settings", "+ Panel", "Help"] {
                        let other = bounds(name);
                        let difference = ((other.y0 + other.y1) / 2.0 - center).abs();
                        assert!(
                            difference <= 1.0,
                            "{width}pt / {ui_pt}pt / {state}: {name} is {difference}pt off Compare's center",
                        );
                    }
                    assert_inside_screen(screen, compare, "Compare");
                    for name in ["Settings", "+ Panel", "Help"] {
                        assert_inside_screen(screen, bounds(name), name);
                    }
                }
            }
        }
    }

    #[test]
    fn enter_shortcuts_do_not_activate_a_focused_toolbar_button() {
        for focused in ["Help", "Compare"] {
            for taking in [false, true] {
                let mut app = merging_app("a\n", "b\n", vec![(hunk(0..1, 0..1), 0..2)]);
                app.cache.get_mut(&1).unwrap().lines = ansi::parse(b"-a\n+b\n");
                let ctx = egui::Context::default();
                ctx.set_fonts(crate::fonts::definitions(None, None, None));
                crate::theme::install(&ctx, crate::settings::DEFAULT_UI_PT, app.settings.mono_pt);
                ctx.enable_accesskit();
                // Let real panel sizing establish columns before making this
                // fixture fresh. No test subprocess is needed during warmup.
                app.in_flight = Some(app.current_key());
                for _ in 0..3 {
                    app_ui_test_pass(&mut app, &ctx, Vec::new());
                }
                app.in_flight = None;
                let key = app.current_key();
                app.cache.get_mut(&app.shown).unwrap().key = key;
                let initial = app_ui_test_pass(&mut app, &ctx, Vec::new());
                let target = initial.nodes.iter().find(|(_, node)| {
                    node.label().is_some_and(|label| label.starts_with(focused))
                }).unwrap().0;
                app_ui_test_pass(&mut app, &ctx, vec![egui::Event::AccessKitActionRequest(egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::Focus,
                    target_tree: egui::accesskit::TreeId::ROOT,
                    target_node: target,
                    data: None,
                })]);
                let modifiers = if taking {
                    egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT)
                } else {
                    egui::Modifiers::COMMAND
                };
                app_ui_test_pass(&mut app, &ctx, vec![
                    egui::Event::ModifiersChanged(modifiers),
                    egui::Event::Key {
                        key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers,
                    },
                ]);
                assert_eq!(app.panels[app.reference].text, if taking { "b\n" } else { "a\n" });
                assert!(!app.show_help, "Enter shortcut also activated {focused}");
                assert!(!ctx.input(|input| input.key_pressed(Key::Enter)));
                if taking {
                    // No release: egui derives repeat=true for a held chord.
                    app_ui_test_pass(&mut app, &ctx, vec![
                        egui::Event::ModifiersChanged(modifiers),
                        egui::Event::Key {
                            key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers,
                        },
                    ]);
                    assert!(!app.show_help, "held Take activated {focused}");
                    assert!(!ctx.input(|input| input.key_pressed(Key::Enter)));
                }
            }
        }
    }

    fn app_ui_test_pass(
        app: &mut App,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
    ) -> egui::accesskit::TreeUpdate {
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1_240.0, 860.0));
        let mut input = screen_input(screen);
        input.events = events;
        // Separate TextEdit undo snapshots without sleeps.
        input.time = Some(ctx.cumulative_frame_nr() as f64 * 1.1);
        let mut frame = eframe::Frame::_new_kittest();
        app.quit_menu_guarded = true;
        let mut output = ctx.run_ui(input, |ui| eframe::App::ui(app, ui, &mut frame));
        output.textures_delta.clear();
        output.platform_output.accesskit_update.unwrap()
    }

    #[test]
    fn full_shortcut_chords_undo_and_redo_takes_from_the_diff() {
        let mut app = merging_app("a\n", "b\n", vec![(hunk(0..1, 0..1), 0..2)]);
        app.cache.get_mut(&1).unwrap().lines = ansi::parse(b"-a\n+b\n");
        let ctx = egui::Context::default();
        ctx.set_fonts(crate::fonts::definitions(None, None, None));
        crate::theme::install(&ctx, crate::settings::DEFAULT_UI_PT, app.settings.mono_pt);
        ctx.enable_accesskit();
        app.in_flight = Some(app.current_key());
        for _ in 0..3 {
            app_ui_test_pass(&mut app, &ctx, Vec::new());
        }
        app.take_hunk(0, &ctx);
        let result = app.result_panel().unwrap();
        assert_eq!(app.panels[result].text, "b\n");
        ctx.memory_mut(|memory| memory.request_focus(render::diff_region_id()));
        for (shift, expected) in [(false, "a\n"), (true, "b\n")] {
            let modifiers = egui::Modifiers { shift, ..egui::Modifiers::COMMAND };
            app_ui_test_pass(&mut app, &ctx, vec![
                egui::Event::ModifiersChanged(modifiers),
                egui::Event::Key { key: Key::Z, physical_key: None, pressed: true, repeat: false, modifiers },
                egui::Event::Key { key: Key::Z, physical_key: None, pressed: false, repeat: false, modifiers },
                egui::Event::ModifiersChanged(egui::Modifiers::NONE),
            ]);
            assert!(!ctx.text_edit_focused());
            assert_eq!(app.panels[result].text, expected, "take history shortcut was lost after modifier release");
            assert!(!ctx.input(|input| input.key_pressed(Key::Z)));
        }
    }

    #[test]
    fn loaded_large_inputs_explain_compare_without_starting_an_automatic_render() {
        let dir = temp_path("large-ready-state");
        std::fs::create_dir_all(&dir).unwrap();
        let left = dir.join("left.txt");
        let right = dir.join("right.txt");
        let line = "a line of text to compare\n";
        let content = line.repeat(AUTO_RENDER_BYTES / (2 * line.len()) + 1);
        std::fs::write(&left, &content).unwrap();
        std::fs::write(&right, &content).unwrap();
        let large = App::new(
            Delta {
                path: PathBuf::from("delgui-test-delta-does-not-exist"),
                version: (0, 19, 0), version_string: "delta test".into(),
            },
            Settings::default(),
            Launch { files: vec![left, right], ..Launch::default() },
        );
        assert!(large.compared);
        assert!(large.pair_bytes() > AUTO_RENDER_BYTES);
        let mut paste = test_app();
        paste.panels[0].text = "left\n".into();
        paste.panels[1].text = "right\n".into();
        let mut rendering = test_app();
        rendering.panels[0].text = "left\n".into();
        rendering.in_flight = Some(rendering.current_key());
        for (mut app, title, explanation, pending) in [
            (test_app(), "Nothing to compare yet", "Put text in two panels", false),
            (paste, "Ready to compare", "Choose Compare", false),
            (large, "Ready to compare", "Automatic comparison is paused for large inputs", false),
            (rendering, "Comparing…", "Preparing the differences", true),
        ] {
            let ctx = egui::Context::default();
            ctx.set_fonts(crate::fonts::definitions(None, None, None));
            crate::theme::install(&ctx, crate::settings::DEFAULT_UI_PT, app.settings.mono_pt);
            ctx.enable_accesskit();
            for _ in 0..2 {
                let tree = app_ui_test_pass(&mut app, &ctx, Vec::new());
                let contains = |expected: &str| tree.nodes.iter().any(|(_, node)| {
                    node.label().or(node.value()).is_some_and(|text| text.contains(expected))
                });
                assert!(contains(title), "missing state: {title}");
                assert!(contains(explanation), "missing next step: {explanation}");
                if title == "Ready to compare" {
                    assert!(contains(keys::compare_label()));
                    assert!(!contains("Nothing to compare yet"));
                }
                assert_eq!(app.in_flight.is_some(), pending, "empty-state copy changed rendering policy");
                assert!(!app.requested);
            }
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn editor_shortcuts_keep_newlines_undo_and_same_frame_save_correct() {
        let dir = temp_path("editor-shortcuts");
        std::fs::create_dir_all(&dir).unwrap();
        let saved = dir.join("result.txt");
        let mut app = merging_app("result\n", "candidate\n", vec![(hunk(0..1, 0..1), 0..2)]);
        app.cache.get_mut(&1).unwrap().lines = ansi::parse(b"-result\n+candidate\n");
        app.panels[0].text = "source\n".into();
        let result = app.result_panel().unwrap();
        assert!(app.save_result_to(result, saved.clone(), true));
        app.in_flight = Some(app.current_key());
        let ctx = egui::Context::default();
        ctx.set_fonts(crate::fonts::definitions(None, None, None));
        crate::theme::install(&ctx, crate::settings::DEFAULT_UI_PT, app.settings.mono_pt);
        ctx.enable_accesskit();
        for _ in 0..3 {
            app_ui_test_pass(&mut app, &ctx, Vec::new());
        }
        let focus = |app: &mut App, prefix: &str| {
            let tree = app_ui_test_pass(app, &ctx, Vec::new());
            let target = tree.nodes.iter().find(|(_, node)| {
                node.label().is_some_and(|label| label.starts_with(prefix))
            }).unwrap().0;
            app_ui_test_pass(app, &ctx, vec![egui::Event::AccessKitActionRequest(egui::accesskit::ActionRequest {
                action: egui::accesskit::Action::Focus,
                target_tree: egui::accesskit::TreeId::ROOT,
                target_node: target,
                data: None,
            })]);
        };
        let chord = |key, shift| {
            let modifiers = egui::Modifiers { shift, ..egui::Modifiers::COMMAND };
            vec![
                egui::Event::ModifiersChanged(modifiers),
                egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers },
                egui::Event::Key { key, physical_key: None, pressed: false, repeat: false, modifiers },
                egui::Event::ModifiersChanged(egui::Modifiers::NONE),
            ]
        };

        for (label, panel) in [("Panel A editor", 0), ("Result editor", result)] {
            focus(&mut app, label);
            let before = app.panels[panel].text.clone();
            app_ui_test_pass(&mut app, &ctx, chord(Key::Enter, false));
            assert_eq!(app.panels[panel].text, before, "Compare inserted a newline in {label}");
        }

        focus(&mut app, "Result editor");
        app_ui_test_pass(&mut app, &ctx, vec![egui::Event::Text("typed".into())]);
        let before_save = app.panels[result].text.clone();
        let mut edit_and_save = vec![egui::Event::Text(" and saved".into())];
        edit_and_save.extend(chord(Key::S, false));
        app_ui_test_pass(&mut app, &ctx, edit_and_save);
        let after_save = app.panels[result].text.clone();
        assert_ne!(after_save, before_save);
        assert_eq!(std::fs::read_to_string(&saved).unwrap(), after_save);
        assert!(!app.panels[result].dirty);

        app_ui_test_pass(&mut app, &ctx, chord(Key::Z, false));
        assert_eq!(app.panels[result].text, before_save, "TextEdit lost its undo chord");
        app_ui_test_pass(&mut app, &ctx, Vec::new());
        app_ui_test_pass(&mut app, &ctx, chord(Key::Z, true));
        assert_eq!(app.panels[result].text, after_save, "TextEdit lost its redo chord");

        // Take while the result editor retains focus; it must not also insert
        // Enter before deciding whether the cached hunk is still applicable.
        app.panels[result].text = "result\n".into();
        let key = app.current_key();
        app.cache.get_mut(&app.shown).unwrap().key = key;
        app_ui_test_pass(&mut app, &ctx, chord(Key::Enter, true));
        assert_eq!(app.panels[result].text, "candidate\n");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn source_panel_height_stays_bounded_across_frames_and_candidate_changes() {
        let mut app = test_app();
        while app.panels.len() < 6 {
            app.add_panel();
        }
        for (i, panel) in app.panels.iter_mut().enumerate() {
            panel.text = format!("panel {i}\n").repeat(20);
            panel.resniff();
        }
        app.in_flight = Some(app.current_key());
        let ctx = egui::Context::default();
        ctx.set_fonts(crate::fonts::definitions(None, None, None));
        crate::theme::install(&ctx, crate::settings::DEFAULT_UI_PT, app.settings.mono_pt);
        ctx.enable_accesskit();
        for frame in 0..12 {
            if frame == 5 {
                app.shown = 5;
                app.reveal_panel = Some(5);
            }
            app_ui_test_pass(&mut app, &ctx, Vec::new());
            let height = app.panel_rects.iter().map(|rect| rect.height()).fold(0.0, f32::max);
            assert!(height <= 270.0, "frame {frame}: source card grew to {height}pt");
        }
    }

    #[test]
    fn stale_status_stays_inside_even_a_short_diff_region() {
        for height in [20.0, 50.0, 120.0] {
            let app = test_app();
            let ctx = egui::Context::default();
            ctx.set_fonts(crate::fonts::definitions(None, None, None));
            crate::theme::install(&ctx, 20.0, 24.0);
            ctx.enable_accesskit();
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(320.0, height));
            for _ in 0..3 {
                let mut output = ctx.run_ui(screen_input(screen), |ui| {
                    ui.set_min_size(screen.size());
                    app.stale_pill(ui, &ui::tokens(ui));
                });
                output.textures_delta.clear();
                let update = output.platform_output.accesskit_update.unwrap();
                for (_, node) in update.nodes.iter().filter(|(_, node)| {
                    node.value().is_some_and(|text| text.starts_with("Out of date"))
                }) {
                    assert_inside_screen(screen, node.bounds().unwrap(), "stale diff status");
                }
                if height >= 50.0 {
                    assert!(update.nodes.iter().any(|(_, node)| {
                        node.value().is_some_and(|text| text.starts_with("Out of date"))
                    }), "stale state should remain visible when it fits");
                }
            }
        }
    }

    /// The panel is an editor, so what it shows has to be what gets compared.
    /// Handing delta the path instead means it re-reads the file and renders
    /// something the user cannot see -- which looked exactly like the app
    /// ignoring every keystroke.
    #[test]
    fn editing_a_file_panel_compares_the_buffer_and_not_the_file() {
        let mut p = file_panel("fn main() {}\n");
        assert!(
            matches!(p.to_input(false), Input::Path(_)),
            "an untouched file goes as a path"
        );
        p.edited = true;
        match p.to_input(false) {
            Input::Buffer(b) => assert_eq!(b, p.text.as_bytes()),
            Input::Path(_) => panic!("an edited panel handed delta the file on disk"),
        }
    }

    /// Merge mode splices `panel.text`, so the diff it splices from has to have
    /// been made of `panel.text` too. Handing git the path instead lets the file
    /// change between the diff and the take.
    #[test]
    fn merge_mode_sends_both_sides_as_buffers() {
        let p = file_panel("fn main() {}\n");
        assert!(matches!(p.to_input(true), Input::Buffer(_)));
    }

    /// Once the bytes travel as a pipe, delta has no filename to infer from, so
    /// the extension has to survive as `--default-language`.
    #[test]
    fn an_edited_file_panel_still_knows_its_language() {
        let mut p = file_panel("fn main() {}\n");
        p.edited = true;
        assert_eq!(p.language_hint().as_deref(), Some("rs"));
    }

    #[test]
    fn an_explicit_language_beats_both_the_extension_and_the_sniffer() {
        let mut p = file_panel("fn main() {}\n");
        p.language = Some("py".into());
        assert_eq!(p.language_hint().as_deref(), Some("py"));
        p.language = Some("   ".into());
        assert_eq!(
            p.language_hint().as_deref(),
            Some("rs"),
            "blank is not a choice"
        );
    }

    #[test]
    fn an_explicit_language_override_uses_a_buffer_snapshot() {
        let mut panel = file_panel("fn main() {}\n");
        panel.language = Some("py".into());
        match panel.to_input(true) {
            Input::Buffer(bytes) => assert_eq!(bytes, panel.text.as_bytes()),
            Input::Path(_) => panic!("a real extension would override the explicit language"),
        }
    }

    /// Prose is a normal thing to paste and delta renders it fine unhighlighted.
    #[test]
    fn a_pasted_prose_panel_claims_no_language() {
        let mut p = Panel::empty();
        p.text = "The quick brown fox jumps over the lazy dog.\nIt did so twice.\n".into();
        p.resniff();
        assert_eq!(p.language_hint(), None);
    }

    /// Clearing a panel has to drop the watch with the path, or the watcher
    /// keeps waking the UI for a file nothing is looking at.
    #[test]
    fn clearing_a_panel_stops_it_following_anything() {
        let mut p = file_panel("x\n");
        p.watch = true;
        p.clear();
        assert!(p.path.is_none() && !p.watch);
    }

    #[test]
    fn oversized_files_are_refused_before_binding() {
        let path = temp_path("oversized");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_PANEL_BYTES as u64 + 1).unwrap();
        let mut panel = Panel::empty();
        panel.text = "keep me\n".into();

        let error = panel.bind(path.clone()).unwrap_err();

        assert!(error.contains("stops at"));
        assert_eq!(panel.text, "keep me\n");
        assert!(panel.path.is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn non_regular_files_are_refused_without_opening_them() {
        let dir = temp_path("directory-input");
        std::fs::create_dir_all(&dir).unwrap();

        let error = read_panel_text(&dir).unwrap_err();

        assert!(error.contains("not a regular file"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Previous/Next change and the "n of m" counter are driven by
    /// `Cached::hunks`, which used to be filled only while merging -- so in a
    /// plain comparison, which is what the app mostly is, the controls never
    /// appeared and nothing said how many differences there were.
    #[test]
    fn a_plain_render_knows_where_its_differences_are() {
        let delta = Delta::discover().expect("these tests require `delta` on PATH");
        let mut lines: Vec<String> = (0..20).map(|i| format!("line {i}\n")).collect();
        let before: String = lines.concat();
        // Far enough apart that three lines of context cannot join them.
        lines[2] = "line two, changed\n".into();
        lines[17] = "line seventeen, changed\n".into();
        let after: String = lines.concat();

        for hunk_headers in [false, true] {
            let mut app = test_app();
            app.opts.hunk_headers = hunk_headers;
            app.panels[0].text = before.clone();
            app.panels[1].text = after.clone();
            assert!(!app.merging());

            let cached = render_job(
                &delta,
                &Input::Buffer(before.clone().into_bytes()),
                &Input::Buffer(after.clone().into_bytes()),
                &app.effective_options(),
                app.current_key(),
                app.columns,
                false,
            )
            .expect("render");

            assert_eq!(
                cached.hunks.len(),
                2,
                "two changes three lines apart are two differences (headers={hunk_headers})",
            );
            for (_, span) in &cached.hunks {
                assert!(
                    span.start < span.end && span.end <= cached.lines.len(),
                    "a difference points outside the rows that get drawn",
                );
            }
            assert!(
                !cached.lines.iter().any(|line| line
                    .spans
                    .iter()
                    .any(|span| span.text.contains(merge::HUNK_LABEL))),
                "the mark delta was asked for is still on screen (headers={hunk_headers})",
            );
        }
    }

    /// Which side a panel is on is the whole meaning of red and green, so
    /// swapping has to be exact: the same two panels, the other way round.
    #[test]
    fn swapping_sides_exchanges_the_two_panels_and_nothing_else() {
        let mut app = test_app();
        app.add_panel();
        app.reference = 0;
        app.shown = 2;
        app.swap_sides();
        assert_eq!((app.reference, app.shown), (2, 0));

        // The difference from `MakeReference`, which promotes the shown panel
        // and lets `normalize` pick whatever is left: the *first* other panel,
        // which past two panels is not the one that was the baseline.
        app.reference = 1;
        app.shown = 2;
        app.swap_sides();
        assert_eq!((app.reference, app.shown), (2, 1));
        app.reference = 1;
        app.shown = 2;
        let shown = app.shown;
        app.set_reference(shown);
        assert_eq!((app.reference, app.shown), (2, 0));

        // And back again, which is what makes it a toggle rather than a walk.
        app.reference = 0;
        app.shown = 1;
        app.swap_sides();
        app.swap_sides();
        assert_eq!((app.reference, app.shown), (0, 1));
    }

    /// The baseline is the result being built and its candidates are what it is
    /// built from; there is no other way round for that to be. The button is
    /// drawn only in the same branch, so this is the keyboard's guard.
    #[test]
    fn swapping_sides_is_refused_while_a_result_is_being_built() {
        let mut app = test_app();
        app.panels[0].result = true;
        app.reference = 0;
        app.shown = 1;
        app.building_result = true;
        assert!(app.merging());
        app.swap_sides();
        assert_eq!((app.reference, app.shown), (0, 1));
    }

    /// Ignoring differences is a way of *reading* a diff, and building a result
    /// is not reading: a take copies a difference's lines exactly, and
    /// `merge::verify` requires everything between the differences to be
    /// identical on both sides -- which is precisely what an ignore makes false.
    /// Left on, building a result would silently hide its own controls.
    #[test]
    fn building_a_result_ignores_nothing_and_keeps_the_settings() {
        let mut app = test_app();
        app.opts.whitespace = Whitespace::All;
        app.opts.ignore_blank_lines = true;
        app.opts.ignore_cr_at_eol = true;
        app.settings.ignore_matching = "  ^build   ".into();
        app.settings.context = Context::Whole;

        let reading = app.effective_options();
        assert_eq!(reading.whitespace, Whitespace::All);
        assert_eq!(reading.context, Context::Whole.lines());
        // Trimmed, because a pattern typed with a stray space is not a pattern
        // for a line that begins with one.
        assert_eq!(reading.ignore_matching.as_deref(), Some("^build"));
        assert!(reading.ignoring().is_some());

        app.panels[0].result = true;
        app.reference = 0;
        app.building_result = true;
        assert!(app.merging());
        let building = app.effective_options();
        assert_eq!(building.whitespace, Whitespace::Exact);
        assert!(!building.ignore_blank_lines);
        assert!(!building.ignore_cr_at_eol);
        assert_eq!(building.ignore_matching, None);
        assert_eq!(building.context, 0, "each change has to be takeable alone");
        assert_eq!(building.ignoring(), None);

        // Forced in `effective_options`, never on `opts`: `save` persists `opts`
        // verbatim, so building a result once would otherwise rewrite the
        // user's own settings for good.
        assert_eq!(app.opts.whitespace, Whitespace::All);
        assert!(app.opts.ignore_blank_lines);
        assert_eq!(app.settings.context, Context::Whole);
    }

    /// An empty pattern is not "match everything"; it is a field nobody filled
    /// in. Git would take it literally.
    #[test]
    fn an_empty_ignore_pattern_is_not_passed_to_git() {
        let mut app = test_app();
        app.settings.ignore_matching = "   ".into();
        assert_eq!(app.effective_options().ignore_matching, None);
        assert_eq!(app.effective_options().ignoring(), None);
    }

    /// An empty delta render means equality only for the exact revision that
    /// produced it. Editing either side must immediately withdraw that claim.
    #[test]
    fn identical_then_edit_does_not_report_no_differences_from_stale_cache() {
        let mut app = test_app();
        app.panels[0].text = "same\n".into();
        app.panels[1].text = "same\n".into();
        app.compared = true;
        app.cache.insert(
            app.shown,
            Cached {
                key: app.current_key(),
                lines: Vec::new(),
                columns: app.columns,
                hunks: Vec::new(),
                problem: None,
            },
        );
        assert!(app.reports_no_differences());

        app.panels[1].text.push_str("different\n");
        app.touch_edit(1);

        assert!(!app.reports_no_differences());
    }

    #[test]
    fn changing_an_unrelated_panel_keeps_the_active_pair_key_stable() {
        let mut app = test_app();
        app.panels.push(Panel::empty());
        let before = app.current_key();

        app.panels[2].text = "unrelated\n".into();
        app.touch_panel(2);

        assert_eq!(before, app.current_key());
    }

    #[test]
    fn compare_does_not_reload_an_unrelated_missing_panel() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.panels.push(Panel::empty());
        app.panels[0].text = "a\n".into();
        app.panels[1].text = "b\n".into();
        app.panels[2].path = Some(temp_path("missing-unrelated"));
        app.in_flight = Some(app.current_key()); // keep this test from spawning delta

        app.compare_now(&ctx);

        assert!(app.error.is_none());
    }

    #[test]
    fn explicit_compare_retries_an_exact_failed_render_once() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.panels[0].text = "left\n".into();
        app.panels[1].text = "right\n".into();
        app.compared = true;
        let key = app.current_key();
        app.failed = Some(key.clone());

        app.schedule(&ctx);
        assert!(app.in_flight.is_none(), "automatic retries stay suppressed");
        assert_eq!(app.failed.as_ref(), Some(&key));

        app.compare_now(&ctx);
        assert_eq!(app.in_flight.as_ref(), Some(&key));
        assert!(app.failed.is_none());
    }

    /// A panel with hunks in the cache, as if a merge-mode render had landed.
    fn merging_app(base: &str, cand: &str, hunks: Vec<(Hunk, Range<usize>)>) -> App {
        let mut app = test_app();
        app.panels.push(Panel::empty());
        app.panels[2].result = true;
        app.panels[2].text = base.into();
        app.panels[1].text = cand.into();
        app.reference = 2;
        app.shown = 1;
        app.building_result = true;
        app.result_generation = 1;
        app.compared = true;
        let key = app.current_key();
        app.cache.insert(
            1,
            Cached {
                key,
                lines: Vec::new(),
                columns: app.columns,
                hunks,
                problem: None,
            },
        );
        app
    }

    fn hunk(old: Range<usize>, new: Range<usize>) -> Hunk {
        Hunk {
            old,
            new,
            context: String::new(),
        }
    }

    #[test]
    fn merge_hunk_controls_belong_to_the_diff_scroll_view() {
        let mut app = merging_app("a\n", "b\n", vec![(hunk(0..1, 0..1), 0..2)]);
        app.cache.get_mut(&1).unwrap().lines = ansi::parse(b"-a\n+b\n");

        let ctx = egui::Context::default();
        ctx.set_fonts(crate::fonts::definitions(None, None, None));
        crate::theme::install(&ctx, crate::settings::DEFAULT_UI_PT, 12.5);
        ctx.enable_accesskit();
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(720.0, 480.0),
                )),
                ..Default::default()
            },
            |ui| app.diff_area(ui, &ctx, 8.0),
        );
        let update = output
            .platform_output
            .accesskit_update
            .take()
            .expect("AccessKit tree update");
        output.textures_delta.clear();

        let control = update
            .nodes
            .iter()
            .find(|(_, node)| {
                node.label()
                    .is_some_and(|label| label.starts_with("Use B's version"))
            })
            .map(|(id, _)| *id)
            .expect("merge hunk control");
        let (scroll_id, scroll) = update
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Diff scroll region"))
            .expect("diff ScrollView");
        assert_eq!(scroll.role(), egui::accesskit::Role::ScrollView);
        let mut pending = scroll.children().to_vec();
        let mut found = false;
        while let Some(id) = pending.pop() {
            if id == control {
                found = true;
                break;
            }
            if let Some((_, node)) = update.nodes.iter().find(|(candidate, _)| *candidate == id) {
                pending.extend_from_slice(node.children());
            }
        }
        assert!(
            found,
            "merge control is outside the diff ScrollView {scroll_id:?} accessibility subtree",
        );
    }

    /// A take writes the candidate's lines into the result, and says so: without
    /// `edited`, `compare_now` re-reads a saved result from disk and the take is
    /// silently undone by the next ⌘Enter.
    #[test]
    fn taking_a_difference_rewrites_the_result_and_marks_it_unsaved() {
        let ctx = egui::Context::default();
        let mut app = merging_app("a\nb\nc\n", "a\nB\nc\n", vec![(hunk(1..2, 1..2), 0..1)]);
        app.panels[2].saved_to = Some(PathBuf::from("/tmp/result.rs"));
        app.panels[2].path = Some(PathBuf::from("/tmp/result.rs"));
        app.take_hunk(0, &ctx);
        assert_eq!(app.panels[2].text, "a\nB\nc\n");
        assert!(app.panels[2].dirty, "a take leaves unsaved work");
        assert!(
            app.panels[2].edited,
            "…and detaches the buffer from the file"
        );
    }

    /// The ranges describe the panels as they were rendered. If they no longer
    /// fit, splicing something plausible into a buffer the user is about to save
    /// is the one outcome worth refusing.
    #[test]
    fn a_take_that_no_longer_fits_is_refused_rather_than_guessed() {
        let ctx = egui::Context::default();
        let mut app = merging_app("a\n", "b\n", vec![(hunk(4..6, 0..1), 0..1)]);
        app.take_hunk(0, &ctx);
        assert_eq!(app.panels[2].text, "a\n");
        assert!(app.notice.is_some(), "and says so");
    }

    #[test]
    fn undo_puts_the_result_back_as_it_was() {
        let ctx = egui::Context::default();
        let mut app = merging_app("a\nb\nc\n", "a\nB\nc\n", vec![(hunk(1..2, 1..2), 0..1)]);
        app.take_hunk(0, &ctx);
        app.undo_take(&ctx);
        assert_eq!(app.panels[2].text, "a\nb\nc\n");
        assert!(app.undo.is_empty(), "and the history is spent");
    }

    #[test]
    fn undo_never_replaces_manual_result_edits() {
        let ctx = egui::Context::default();
        let mut app = merging_app("a\nb\n", "a\nB\n", vec![(hunk(1..2, 1..2), 0..1)]);
        app.take_hunk(0, &ctx);
        app.panels[2].text.push_str("manual\n");
        app.result_edited(2);

        app.undo_take(&ctx);

        assert_eq!(app.panels[2].text, "a\nB\nmanual\n");
        assert!(app.undo.is_empty());
    }

    #[test]
    fn a_first_take_from_an_empty_result_can_be_undone_and_redone() {
        let ctx = egui::Context::default();
        let mut app = merging_app("", "candidate\n", vec![(hunk(0..0, 0..1), 0..1)]);

        app.take_hunk(0, &ctx);
        assert_eq!(app.panels[2].text, "candidate\n");
        app.undo_take(&ctx);
        assert_eq!(app.panels[2].text, "");
        app.redo_take(&ctx);
        assert_eq!(app.panels[2].text, "candidate\n");
    }

    #[test]
    fn a_diverged_undo_entry_preserves_newer_text() {
        let ctx = egui::Context::default();
        let mut app = merging_app("a\n", "b\n", vec![(hunk(0..1, 0..1), 0..1)]);
        app.take_hunk(0, &ctx);
        app.panels[2].text = "newer\n".into();

        app.undo_take(&ctx);

        assert_eq!(app.panels[2].text, "newer\n");
        assert!(app.undo.is_empty() && app.redo.is_empty());
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|n| n.contains("newer edits"))
        );
    }

    /// `--paste`, the global hotkey and dropped files all fill the first empty
    /// panel. The hotkey fires from other applications, so an empty result would
    /// be claimed out of sight -- which is exactly what must not happen to it.
    #[test]
    fn an_empty_result_is_not_an_empty_slot() {
        let mut p = Panel::empty();
        assert!(p.is_empty());
        p.result = true;
        assert!(!p.is_empty());
    }

    #[test]
    fn stopping_build_mode_keeps_the_result_identity_and_unsaved_state() {
        let mut app = test_app();
        app.panels[0].text = "left\n".into();
        app.panels[1].text = "right\n".into();
        app.start_result(Some(0));
        let result = app.result_panel().unwrap();
        app.panels[result].dirty = true;

        app.stop_building();

        assert_eq!(app.result_panel(), Some(result));
        assert!(!app.merging());
        assert!(app.panel_has_unsaved_content(result));
    }

    #[test]
    fn a_dirty_empty_result_is_still_unsaved() {
        let mut app = test_app();
        app.start_result(None);
        let result = app.result_panel().unwrap();
        app.panels[result].dirty = true;
        assert!(app.panels[result].text.is_empty());
        assert!(app.unsaved_panels().contains(&result));
    }

    #[test]
    fn destructive_requests_leave_unsaved_text_in_place_until_confirmed() {
        let mut app = test_app();
        app.panels.push(Panel::empty());
        app.panels[2].text = "pasted work\n".into();

        app.request_destructive(DestructiveAction::Clear(2));

        assert_eq!(app.panels[2].text, "pasted work\n");
        assert!(matches!(app.destructive, Some(DestructiveAction::Clear(2))));
        let action = app.destructive.take().unwrap();
        app.apply_destructive(action);
        assert!(app.panels[2].text.is_empty());
    }

    /// Clearing a result is the "start from nothing" case, not a way out of
    /// building one -- and `clear` is `*self = empty()`, which would drop the
    /// role along with everything else.
    #[test]
    fn clearing_a_result_leaves_it_a_result() {
        let mut p = Panel::empty();
        p.result = true;
        p.text = "x\n".into();
        p.clear();
        assert!(p.result && p.text.is_empty());
    }

    /// `reference > i` left the baseline pointing at whatever slid into that
    /// slot, so removing the baseline silently promoted an unrelated panel.
    #[test]
    fn removing_the_baseline_does_not_promote_a_stranger() {
        let mut app = test_app();
        app.panels.push(Panel::empty());
        for (i, p) in app.panels.iter_mut().enumerate() {
            p.text = format!("panel {i}\n");
        }
        app.reference = 1;
        app.shown = 2;
        app.remove_panel(1);
        assert_eq!(app.panels[app.reference].text, "panel 0\n");
        assert_ne!(app.shown, app.reference);
    }

    /// Saving over an input would leave that panel showing text the file no
    /// longer holds, and take the difference it was there to show with it.
    #[test]
    fn saving_over_an_input_panel_is_refused() {
        let path = temp_path("merge-input");
        std::fs::write(&path, "input\n").expect("write");
        let mut app = merging_app("result\n", "input\n", Vec::new());
        app.panels[1].path = Some(path.clone());
        app.panels[2].saved_to = Some(path.clone());
        app.save_result(false);
        assert!(app.error.is_some());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "input\n");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_plain_save_refuses_to_overwrite_an_external_change() {
        let dir = temp_path("result-external-change");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("result.txt");

        let mut app = test_app();
        assert_eq!(app.alternate_result_save_label(), "Save as…");
        app.start_result(None);
        let result = app.result_panel().unwrap();
        app.panels[result].saved_to = Some(path.clone());
        app.panels[result].text = "first save\n".into();
        app.panels[result].dirty = true;

        assert!(app.save_result(false));
        assert_eq!(
            app.panels[result].saved_snapshot.as_deref(),
            Some("first save\n")
        );

        std::fs::write(&path, "external change\n").unwrap();
        app.panels[result].text = "later delgui edit\n".into();
        app.panels[result].dirty = true;

        assert!(!app.save_result(false));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "external change\n"
        );
        assert!(app.panels[result].dirty);
        assert_eq!(
            app.panels[result].saved_snapshot.as_deref(),
            Some("first save\n")
        );
        let error = app.error.as_deref().expect("conflict explanation");
        assert!(error.contains(&path.display().to_string()));
        assert!(error.contains("changed outside delgui"));

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_externally_replaced_result_is_not_reported_saved_and_guards_close() {
        let dir = temp_path("result-external-replacement-close");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("result.txt");

        let mut app = test_app();
        app.start_result(None);
        let result = app.result_panel().unwrap();
        app.panels[result].saved_to = Some(path.clone());
        app.panels[result].text = "delgui result\n".into();
        app.panels[result].dirty = true;
        assert!(app.save_result(false));

        std::fs::write(&path, "external replacement\n").unwrap();

        assert!(app.panel_has_unsaved_content(result));
        assert_eq!(
            app.result_identity_view(result).state,
            "saved file missing or changed"
        );
        assert!(!app.save_result(false));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "external replacement\n"
        );
        assert_eq!(app.panels[result].text, "delgui result\n");

        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let mut output = ctx.run_ui(input, |ctx| app.quit_guard(ctx));
        assert!(app.quit_guard);
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::CancelClose)
        );
        output.textures_delta.clear();

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_deleted_result_destination_is_unsaved_and_guards_close() {
        let dir = temp_path("result-deleted-destination-close");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("result.txt");

        let mut app = test_app();
        app.start_result(None);
        let result = app.result_panel().unwrap();
        app.panels[result].saved_to = Some(path.clone());
        app.panels[result].text = "only in delgui now\n".into();
        app.panels[result].dirty = true;
        assert!(app.save_result(false));

        std::fs::remove_file(&path).unwrap();

        assert!(app.panel_has_unsaved_content(result));
        assert_eq!(
            app.result_identity_view(result).state,
            "saved file missing or changed"
        );

        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let mut output = ctx.run_ui(input, |ctx| app.quit_guard(ctx));
        assert!(app.quit_guard);
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::CancelClose)
        );
        assert_eq!(app.panels[result].text, "only in delgui now\n");
        output.textures_delta.clear();

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn saving_over_an_input_is_refused_through_a_path_alias() {
        let dir = temp_path("merge-input-alias");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("input.txt");
        std::fs::write(&path, "input\n").unwrap();
        let alias = dir.join(".").join("input.txt");
        let mut app = merging_app("result\n", "input\n", Vec::new());
        app.panels[1].path = Some(alias);
        app.panels[2].saved_to = Some(path.clone());

        assert!(!app.save_result(false));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "input\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn saving_over_a_hard_link_to_an_input_is_refused() {
        let dir = temp_path("merge-input-hardlink");
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("input.txt");
        let alias = dir.join("alias.txt");
        std::fs::write(&input, "input\n").unwrap();
        std::fs::hard_link(&input, &alias).unwrap();
        let mut app = merging_app("result\n", "input\n", Vec::new());
        app.panels[1].path = Some(input.clone());
        app.panels[2].saved_to = Some(alias);

        assert!(!app.save_result(false));
        assert_eq!(std::fs::read_to_string(&input).unwrap(), "input\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_empty_result_can_be_saved_atomically() {
        let path = temp_path("empty-result");
        std::fs::write(&path, "old contents\n").unwrap();
        let mut app = test_app();
        app.start_result(None);
        let result = app.result_panel().unwrap();
        app.panels[result].dirty = true;
        app.panels[result].saved_to = Some(path.clone());

        assert!(app.save_result(false));
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        assert!(!app.panels[result].dirty);
        let parent = path.parent().unwrap();
        let stem = path.file_name().unwrap().to_string_lossy();
        assert!(std::fs::read_dir(parent).unwrap().flatten().all(|entry| {
            !entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!(".{stem}.delgui-"))
        }));
        let _ = std::fs::remove_file(path);
    }

    /// Seeding from a panel makes the result the baseline, and lands on a tab
    /// with something to take -- which is decided by comparing the text, since
    /// nothing has been rendered against it yet.
    #[test]
    fn starting_a_result_lands_on_a_candidate_that_differs() {
        let mut app = test_app();
        app.panels[0].text = "one\n".into();
        app.panels[1].text = "two\n".into();
        app.start_result(Some(0));
        let result = app.result_panel().expect("a result panel");
        assert_eq!(app.reference, result);
        assert!(app.merging());
        assert_eq!(app.panels[result].text, "one\n");
        assert_eq!(app.shown, 1, "panel A is identical to the seed");
    }

    /// Merge mode is the baseline being a result panel, and nothing else: there
    /// is no second flag to get out of step with it.
    #[test]
    fn merge_mode_follows_the_baseline() {
        let mut app = test_app();
        app.panels[0].text = "one\n".into();
        app.panels[1].text = "two\n".into();
        app.start_result(Some(0));
        assert!(app.merging());
        app.set_reference(0);
        assert!(!app.merging(), "another baseline puts the controls away");
        let result = app.result_panel().expect("still there");
        app.set_reference(result);
        assert!(app.merging(), "and clicking back brings them out");
    }

    /// Merge mode has to reach delta and git as flags, not as a mode the cache
    /// cannot see: `to_args` alone never mentions the diff's context width.
    #[test]
    fn the_render_key_notices_merge_mode() {
        let mut app = test_app();
        app.panels[0].text = "one\n".into();
        app.panels[1].text = "two\n".into();
        let plain = app.current_key();
        app.start_result(Some(0));
        assert_ne!(plain.args, app.current_key().args);
        assert!(
            app.current_key().args.iter().any(|a| a == "--unified=0"),
            "the take unit has to be the fine one"
        );
    }

    /// End to end over the real binaries: render in merge mode, take the only
    /// difference, and check the result now says what the candidate says.
    ///
    /// Everything between the two -- our own `git diff`, delta over it, locating
    /// the marked rows, verifying the ranges against the buffers, splicing -- is
    /// exercised here in one go, which is the only place the whole chain is.
    #[test]
    fn a_take_over_a_real_render_lands_on_the_candidates_lines() {
        let delta = Delta::discover().expect("this test requires `delta` on PATH");
        let ctx = egui::Context::default();
        let mut app = App::new(delta, Settings::default(), Launch::default());
        app.opts.inherit_gitconfig = false;
        app.panels[0].text = "fn one() {}\nfn two() {}\nfn three() {}\n".into();
        app.panels[1].text = "fn one() {}\nfn TWO() {}\nfn three() {}\n".into();
        app.start_result(Some(0));

        // Rendered on this thread, by exactly the function the worker calls.
        let opts = app.effective_options();
        let (left, right) = (
            app.panels[app.reference].to_input(true),
            app.panels[app.shown].to_input(true),
        );
        let cached = render_job(
            &app.delta,
            &left,
            &right,
            &opts,
            app.current_key(),
            app.columns,
            true,
        )
        .expect("merge-mode render");
        assert_eq!(
            cached.problem, None,
            "the hunks and the rendering must line up"
        );
        assert_eq!(cached.hunks.len(), 1, "one changed line is one difference");
        let shown = app.shown;
        app.cache.insert(shown, cached);

        app.take_hunk(0, &ctx);
        assert_eq!(app.panels[app.reference].text, app.panels[shown].text);
    }

    #[test]
    fn auto_render_waits_for_both_debounces_and_respects_the_size_limit() {
        let small = AUTO_RENDER_BYTES - 1;
        let large = AUTO_RENDER_BYTES + 1;

        assert!(
            !should_auto_render(true, false, small),
            "typing had not settled"
        );
        assert!(
            !should_auto_render(false, true, small),
            "resizing had not settled"
        );
        assert!(
            should_auto_render(true, true, small),
            "a settled small edit should render"
        );
        assert!(
            !should_auto_render(true, true, large),
            "large inputs require Compare"
        );
    }

    /// A text viewport must end where a row ends.
    ///
    /// The arithmetic is one line and it has been wrong twice: `pad` is the
    /// `TextEdit` frame's *top* margin only, because that is what offsets row
    /// zero. Counting both margins leaves the bottom of the viewport showing
    /// the top few pixels of the row after the last one -- the same sliced
    /// glyphs, one row further down.
    #[test]
    fn an_editor_viewport_ends_on_a_row_boundary() {
        let ctx = egui::Context::default();
        ctx.set_fonts(crate::fonts::definitions(None, None, None));
        crate::theme::install(&ctx, crate::settings::DEFAULT_UI_PT, 12.5);
        let mut out = ctx.run_ui(Default::default(), |ui| {
            let row = ui.text_style_height(&TextStyle::Monospace);
            for slack in [0.0, 1.0, 7.0, row - 0.1] {
                ui.scope(|ui| {
                    ui.set_max_height(EDITOR_PAD + 11.0 * row + slack);
                    let height = whole_rows(ui, EDITOR_PAD);
                    let rows = (height - EDITOR_PAD) / row;
                    assert!(
                        (rows - rows.round()).abs() < 0.001,
                        "{height} is {rows} rows, not a whole number",
                    );
                    assert!(height <= ui.available_height() + 0.001);
                    assert!(height > ui.available_height() - row);
                });
            }
            // Never zero rows, however little is left: an empty viewport shows
            // nothing at all, which is worse than a cramped one.
            ui.scope(|ui| {
                ui.set_max_height(4.0);
                assert!(whole_rows(ui, EDITOR_PAD) > EDITOR_PAD);
            });
        });
        out.textures_delta.clear();
    }

    #[test]
    fn panel_width_policy_scrolls_instead_of_crushing_dense_rows() {
        assert_eq!(panel_card_width(600.0, 2), 296.0);
        assert_eq!(panel_card_width(600.0, 6), 280.0);
        assert!(panel_card_width(1_440.0, 2) > 600.0);
    }

    #[test]
    fn a_newly_added_panel_scrolls_into_the_minimum_width_viewport() {
        let mut app = test_app();
        while app.panels.len() < MAX_PANELS {
            app.add_panel();
        }
        let added = MAX_PANELS - 1;
        assert_eq!(app.focus_panel, Some(added));
        assert_eq!(app.reveal_panel, Some(added));

        let ctx = egui::Context::default();
        ctx.set_fonts(crate::fonts::definitions(None, None, None));
        crate::theme::install(&ctx, 20.0, 24.0);
        ctx.enable_accesskit();
        let screen =
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::new(720.0, 480.0));
        let mut last_update = None;
        // `scroll_to_me` is resolved when the ScrollArea ends and its offset is
        // visible on the following pass. A third pass proves the landed state is
        // stable rather than only protected by egui's newly-focused grace pass.
        for _ in 0..3 {
            let mut output = ctx.run_ui(screen_input(screen), |ui| app.panel_row(ui, &ctx));
            last_update = output.platform_output.accesskit_update.take();
            output.textures_delta.clear();
        }

        assert_eq!(ctx.memory(|memory| memory.focused()), Some(panel_edit_id(added)));
        assert!(app.reveal_panel.is_none());
        let card = app.panel_rects[added];
        assert!(
            card.left() >= screen.left() && card.right() <= screen.right(),
            "new Panel F card {card:?} stayed outside the 720-point viewport {screen:?}",
        );
        let update = last_update.expect("AccessKit tree update");
        let editor = update
            .nodes
            .iter()
            .find(|(_, node)| {
                node.label()
                    .is_some_and(|label| label.starts_with("Panel F editor"))
            })
            .and_then(|(_, node)| node.bounds())
            .expect("bounded Panel F editor");
        assert!(
            editor.x0 >= f64::from(screen.left()) && editor.x1 <= f64::from(screen.right()),
            "focused Panel F editor {editor:?} stayed outside the 720-point viewport",
        );
    }

    #[test]
    fn a_long_panel_name_leaves_every_header_item_inside_the_card() {
        let mut app = test_app();
        let long_name = format!("{}-implementation.rs", "very-long-component-name".repeat(8));
        let path = PathBuf::from("/tmp").join(&long_name);
        let panel = &mut app.panels[0];
        panel.path = Some(path);
        panel.text = "fn main() {}\n".into();
        panel.edited = true;
        panel.language = Some("rs".into());
        panel.resniff();

        let ctx = egui::Context::default();
        ctx.set_fonts(crate::fonts::definitions(None, None, None));
        crate::theme::install(&ctx, crate::settings::DEFAULT_UI_PT, 12.5);
        ctx.enable_accesskit();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(600.0, 360.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| app.panel_row(ui, &ctx));
        let update = output
            .platform_output
            .accesskit_update
            .take()
            .expect("AccessKit tree update");
        output.textures_delta.clear();
        let card = app.panel_rects[0];
        assert!(card.is_positive(), "the first panel card was not laid out");

        let bounds = |text: &str| {
            update
                .nodes
                .iter()
                .find_map(|(_, node)| {
                    (node.role() != egui::accesskit::Role::TextRun
                        && (node.label() == Some(text) || node.value() == Some(text)))
                    .then(|| node.bounds())
                    .flatten()
                })
                .unwrap_or_else(|| {
                    let available = update
                        .nodes
                        .iter()
                        .filter_map(|(_, node)| node.label().or(node.value()))
                        .collect::<Vec<_>>();
                    panic!("no bounded header node saying {text:?}; available: {available:?}")
                })
        };
        let metadata = [
            "BASELINE",
            "EDITED",
            "1 line",
            "Panel A language: rs",
            "Panel A options",
        ];
        for text in metadata {
            let item = bounds(text);
            assert!(
                item.x0 + 0.5 >= f64::from(card.left()) && item.x1 <= f64::from(card.right()) + 0.5,
                "{text:?} at {}..{} escaped card {}..{}",
                item.x0,
                item.x1,
                card.left(),
                card.right(),
            );
        }
        let name = bounds(&long_name);
        let baseline = bounds("BASELINE");
        assert!(
            name.x0 + 0.5 >= f64::from(card.left()) && name.x1 <= baseline.x0 + 0.5,
            "name {}..{} did not truncate before baseline {}..{}",
            name.x0,
            name.x1,
            baseline.x0,
            baseline.x1,
        );
    }

    #[test]
    fn combine_stays_reachable_for_an_empty_pair() {
        let mut app = test_app();
        app.add_panel();
        app.panels[2].text = "seed from panel C\n".into();
        app.panels[2].resniff();
        assert!(app.panels[app.reference].text.is_empty());
        assert!(app.panels[app.shown].text.is_empty());
        assert_eq!(app.result_seed_indices().collect::<Vec<_>>(), vec![2]);

        let ctx = egui::Context::default();
        ctx.set_fonts(crate::fonts::definitions(None, None, None));
        crate::theme::install(&ctx, crate::settings::DEFAULT_UI_PT, 12.5);
        ctx.enable_accesskit();
        let mut output = ctx.run_ui(Default::default(), |ui| app.pair_strip(ui, &ctx));
        let update = output
            .platform_output
            .accesskit_update
            .take()
            .expect("AccessKit tree update");
        output.textures_delta.clear();
        let combine = update
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Combine…"))
            .expect("the Combine menu button");
        assert!(
            !combine.1.is_disabled(),
            "Start empty and panel C were hidden behind a disabled menu"
        );
        assert!(
            update.nodes.iter().any(|(_, node)| {
                node.role() == egui::accesskit::Role::TabList
                    && node.label() == Some("Comparison candidates")
            }),
            "the candidate selector has no tab-list semantics",
        );
        assert_eq!(
            update
                .nodes
                .iter()
                .filter(|(_, node)| node.role() == egui::accesskit::Role::Tab)
                .count(),
            2,
            "every non-baseline candidate should be an accessible tab",
        );
        let tabs = update
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == egui::accesskit::Role::Tab)
            .map(|(_, node)| node)
            .collect::<Vec<_>>();
        assert!(
            tabs.iter().all(|node| node.is_selected().is_some()),
            "every accessible tab must publish selected or not-selected state",
        );
        assert_eq!(
            tabs.iter()
                .filter(|node| node.is_selected() == Some(true))
                .count(),
            1,
            "exactly one comparison candidate is selected",
        );
        assert!(
            tabs.iter().all(|node| node.toggled().is_none()),
            "Tab nodes must not retain Button::selectable's toggled state",
        );

        app.start_result(None);
        let result = app.result_panel().expect("Start empty created a result");
        assert!(app.panels[result].text.is_empty());
        app.start_result(Some(2));
        assert_eq!(app.panels[result].text, "seed from panel C\n");
    }

    #[test]
    fn candidate_arrows_move_exactly_once_and_wrap() {
        fn pass(
            app: &mut App,
            ctx: &egui::Context,
            input: egui::RawInput,
        ) -> egui::accesskit::TreeUpdate {
            let mut output = ctx.run_ui(input, |ui| app.pair_strip(ui, ctx));
            let update = output
                .platform_output
                .accesskit_update
                .take()
                .expect("AccessKit tree update");
            output.textures_delta.clear();
            update
        }

        let mut app = test_app();
        while app.panels.len() < 5 {
            app.add_panel();
        }
        for (i, panel) in app.panels.iter_mut().enumerate() {
            panel.text = format!("panel {i}\n");
            panel.resniff();
        }
        app.reference = 0;
        app.shown = 1;

        let ctx = egui::Context::default();
        ctx.set_fonts(crate::fonts::definitions(None, None, None));
        crate::theme::install(&ctx, 20.0, 24.0);
        ctx.enable_accesskit();
        let screen =
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::new(720.0, 480.0));
        let initial = pass(&mut app, &ctx, screen_input(screen));
        let tab = |prefix: &str| {
            initial
                .nodes
                .iter()
                .find(|(_, node)| {
                    node.role() == egui::accesskit::Role::Tab
                        && node
                            .label()
                            .is_some_and(|label| label.starts_with(prefix))
                })
                .map(|(id, _)| *id)
                .unwrap_or_else(|| panic!("candidate tab {prefix}"))
        };
        let first = tab("B ·");
        let second = tab("C ·");
        let last = tab("E ·");

        let focus = |target| {
            let mut input = screen_input(screen);
            input
                .events
                .push(egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Focus,
                        target_tree: egui::accesskit::TreeId::ROOT,
                        target_node: target,
                        data: None,
                    },
                ));
            input
        };
        assert_eq!(pass(&mut app, &ctx, focus(first)).focus, first);

        let arrow_right = || {
            let mut input = screen_input(screen);
            input.events.push(egui::Event::Key {
                key: Key::ArrowRight,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            });
            input
        };
        let moved = pass(&mut app, &ctx, arrow_right());
        assert_eq!(app.shown, 2, "one Right arrow selects the next candidate");
        assert_eq!(moved.focus, second, "focus must not skip a second tab");

        assert_eq!(pass(&mut app, &ctx, focus(last)).focus, last);
        let wrapped = pass(&mut app, &ctx, arrow_right());
        assert_eq!(app.shown, 1, "Right from the last candidate wraps first");
        assert_eq!(wrapped.focus, first);
    }

    #[test]
    fn a_two_panel_pair_consumes_an_unneeded_reveal_request() {
        let mut app = test_app();
        app.reveal_pair = Some(app.shown);
        let ctx = egui::Context::default();
        ctx.set_fonts(crate::fonts::definitions(None, None, None));
        crate::theme::install(&ctx, crate::settings::DEFAULT_UI_PT, 12.5);
        let mut output = ctx.run_ui(Default::default(), |ui| app.pair_strip(ui, &ctx));
        output.textures_delta.clear();
        assert!(app.reveal_pair.is_none());
    }

    #[test]
    fn compact_windows_keep_source_and_result_editors_visible() {
        assert_eq!(source_panel_sizes(600.0), (130.0, 96.0, 130.0));
        assert_eq!(
            source_panel_sizes(900.0),
            (270.0, 160.0, f32::INFINITY)
        );
        assert_eq!(
            effective_result_placement(700.0, ResultPlacement::Right),
            ResultPlacement::Bottom
        );
        assert_eq!(
            effective_result_placement(1_200.0, ResultPlacement::Right),
            ResultPlacement::Right
        );
        // Both of these are given the space *left* at their call site, not the
        // window height -- 504 under the pair strip on a default 860 window.
        assert_eq!(
            result_panel_sizes(280.0, ResultPlacement::Bottom),
            (128.0, 128.0, 152.0)
        );
        let (default, min, max) = result_panel_sizes(504.0, ResultPlacement::Bottom);
        assert!((default - 226.8).abs() < 0.01, "{default}");
        assert_eq!(min, 150.0);
        assert_eq!(max, 376.0);
    }

    #[test]
    fn compact_maximum_font_result_keeps_actions_and_one_editor_row_visible() {
        let mut app = test_app();
        app.panels[0].text = "result\n".into();
        app.panels[1].text = "candidate\n".into();
        app.start_result(Some(0));

        let ctx = egui::Context::default();
        ctx.set_fonts(crate::fonts::definitions(None, None, None));
        crate::theme::install(&ctx, 20.0, 24.0);
        ctx.enable_accesskit();
        // A 128-point outer bottom panel leaves 104 points after its symmetric
        // 12-point frame margins. This is the exact compact floor.
        let screen = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::Vec2::new(696.0, MIN_RESULT_OUTER_HEIGHT - 24.0),
        );
        let mut output = ctx.run_ui(screen_input(screen), |ui| app.result_band(ui, &ctx));
        let update = output
            .platform_output
            .accesskit_update
            .take()
            .expect("AccessKit tree update");
        output.textures_delta.clear();

        for name in ["Save", "Result status and destination"] {
            let node = update
                .nodes
                .iter()
                .find(|(_, node)| {
                    node.label()
                        .is_some_and(|label| label == name || label.starts_with(name))
                })
                .unwrap_or_else(|| panic!("visible {name}"));
            let bounds = node.1.bounds().unwrap_or_else(|| panic!("{name} bounds"));
            assert_inside_screen(screen, bounds, name);
            assert!(bounds.y1 > bounds.y0, "{name} has no visible height");
        }
        let editor = update
            .nodes
            .iter()
            .find(|(_, node)| {
                node.label()
                    .is_some_and(|label| label.starts_with("Result editor"))
            })
            .expect("visible Result editor")
            .1
            .bounds()
            .expect("Result editor bounds");
        // TextEdit advertises its full six-row content rectangle inside a
        // scroll viewport. Only the intersection must be on-screen, and it
        // must contain at least one complete maximum-scale row.
        let visible_height = editor.y1.min(f64::from(screen.bottom()))
            - editor.y0.max(f64::from(screen.top()));
        assert!(visible_height >= 32.0, "only {visible_height} pt of the result editor is visible");
    }

    /// The regression the shares replaced: both defaults used to be constants,
    /// so every pixel a taller window added went to the diff. At 1130 that left
    /// the panels showing seven lines of a thirteen-line file above 230 of empty
    /// card, and the result band -- the buffer actually being authored -- at
    /// four or five visible lines whatever the screen.
    #[test]
    fn a_taller_window_gives_the_editors_more_room() {
        let panels = |h: f32| source_panel_sizes(h).0;
        let result = |h: f32| result_panel_sizes(h, ResultPlacement::Bottom).0;
        assert!(panels(1_300.0) > panels(900.0));
        assert!(result(1_300.0) > result(900.0));
        // Bounded at both ends: never below what the old constants gave, and
        // never so far that the diff becomes the smaller half of the window.
        assert_eq!(panels(700.0), 260.0);
        assert_eq!(panels(5_000.0), 420.0);
        assert_eq!(result(700.0), 315.0);
        assert_eq!(result(5_000.0), 520.0);
        // The floor is the old compact size, and the minimum never exceeds the
        // default: a band that cannot be shrunk to fit is how a short window
        // loses the diff entirely.
        let (default, min, max) = result_panel_sizes(200.0, ResultPlacement::Bottom);
        assert_eq!((default, min, max), (128.0, 128.0, 128.0));
        // A side placement is not a share: it already shows the whole result,
        // and its width comes straight out of the column count delta lays the
        // diff out against.
        assert_eq!(
            result_panel_sizes(5_000.0, ResultPlacement::Right),
            (440.0, 260.0, f32::INFINITY)
        );
    }

    #[test]
    fn failed_discard_preserves_the_edit_and_surfaces_the_read_error() {
        let dir = temp_path("reload-error");
        let path = dir.join("panel.txt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "on disk\n").unwrap();

        let mut app = test_app();
        app.panels[0].bind(path.clone()).unwrap();
        app.panels[0].text = "my edit\n".into();
        app.panels[0].edited = true;
        let revision = app.revision;
        std::fs::write(&path, [0xff, 0xfe]).unwrap();

        app.reload_panel(0);

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(app.panels[0].text, "my edit\n");
        assert!(app.panels[0].edited);
        assert_eq!(
            app.revision, revision,
            "a failed reload must not invalidate the render"
        );
        assert!(
            app.error
                .as_deref()
                .is_some_and(|e| e.contains("Could not read"))
        );
    }

    #[test]
    fn a_failed_watched_reload_preserves_the_buffer_but_marks_the_diff_stale() {
        let dir = temp_path("watch-reload-stale");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("watched.txt");
        std::fs::write(&path, "before\n").unwrap();

        let mut app = test_app();
        app.panels[0].text = "reference\n".into();
        app.panels[1].bind(path.clone()).unwrap();
        app.panels[1].watch = true;
        app.compared = true;
        app.cache.insert(
            app.shown,
            Cached {
                key: app.current_key(),
                lines: Vec::new(),
                columns: app.columns,
                hunks: Vec::new(),
                problem: None,
            },
        );
        assert!(app.is_fresh());
        assert!(app.reports_no_differences());

        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        app.pending_reload = Some(Instant::now() - WATCH_DEBOUNCE);
        app.tick(&egui::Context::default());

        assert_eq!(app.panels[1].text, "before\n");
        assert!(app.panels[1].disk_stale);
        assert!(app.shown_diff().is_some(), "keep the last readable fallback");
        assert!(!app.is_fresh());
        assert!(!app.reports_no_differences());
        assert!(
            app.error
                .as_deref()
                .is_some_and(|error| error.contains("Could not read"))
        );

        std::fs::write(&path, "after\n").unwrap();
        app.reload_panel(1);
        assert!(!app.panels[1].disk_stale);
        assert_eq!(app.panels[1].text, "after\n");

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_watcher_backend_error_marks_only_affected_followed_snapshots_stale() {
        let first_dir = temp_path("watch-backend-error-first");
        let second_dir = temp_path("watch-backend-error-second");
        std::fs::create_dir_all(&first_dir).unwrap();
        std::fs::create_dir_all(&second_dir).unwrap();
        let first = first_dir.join("first.txt");
        let second = second_dir.join("second.txt");
        std::fs::write(&first, "first\n").unwrap();
        std::fs::write(&second, "second\n").unwrap();

        let mut app = test_app();
        app.panels[0].bind(first).unwrap();
        app.panels[0].watch = true;
        app.panels[1].bind(second).unwrap();
        app.panels[1].watch = true;

        app.handle_watch_backend_error("backend failed", std::slice::from_ref(&first_dir));

        assert!(app.panels[0].disk_stale);
        assert!(!app.panels[1].disk_stale, "an unrelated watch stayed usable");
        assert!(
            app.error
                .as_deref()
                .is_some_and(|error| error.contains("1 followed panel snapshot"))
        );

        app.handle_watch_backend_error("global backend failure", &[]);
        assert!(app.panels[1].disk_stale, "a pathless backend error is global");

        std::fs::remove_dir_all(first_dir).ok();
        std::fs::remove_dir_all(second_dir).ok();
    }

    #[test]
    fn a_failed_watch_is_not_retried_after_the_notice_is_dismissed() {
        let mut app = test_app();
        let path = temp_path("missing-watch-parent").join("panel.txt");
        app.panels[0].path = Some(path);
        app.panels[0].watch = true;
        let ctx = egui::Context::default();

        app.sync_watches(&ctx);

        assert!(!app.panels[0].watch, "the failed request remained armed");
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|n| n.contains("Cannot watch"))
        );
        app.notice = None;
        app.sync_watches(&ctx);
        assert!(
            app.notice.is_none(),
            "the failed watch was retried on the next repaint"
        );
    }

    #[test]
    fn copied_command_words_are_always_shell_quoted() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("a path/file"), "'a path/file'");
        assert_eq!(
            shell_quote("it's;$HOME$(touch nope)"),
            "'it'\\''s;$HOME$(touch nope)'"
        );
    }

    /// Positional labels, so removing a panel does not rename the others'
    /// identity out from under a tab the user was on.
    #[test]
    fn panels_are_labelled_by_position() {
        assert_eq!((title(0), title(1), title(5)), ('A', 'B', 'F'));
    }
}
