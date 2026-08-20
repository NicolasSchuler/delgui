use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use deltapanes_core::ansi::{self, Line};
use deltapanes_core::config::{self, DeltaConfig};
use deltapanes_core::delta::{Appearance, Delta, DeltaError, Input, Options, Whitespace};
use deltapanes_core::language;
use deltapanes_core::merge::{self, Hunk};
use deltapanes_core::watch::FileWatcher;
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
const MAX_PANEL_BYTES: usize = 4_000_000;

/// Above this, editing stops re-rendering by itself and waits to be asked.
/// delta costs about a second per megabyte, and a diff that recomputes while
/// you type is worse than one you trigger.
const AUTO_RENDER_BYTES: usize = 1_000_000;

/// Delta re-runs on every width change, so resizing has to settle first.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(120);

/// Typing is a stream of changes, and each one would otherwise be a subprocess.
const EDIT_DEBOUNCE: Duration = Duration::from_millis(300);

/// A save is rarely one filesystem event, and a file mid-write reads as
/// truncated, so watch events are coalesced before re-reading.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(180);

/// delta is a two-way tool. More panels means more pairs against one reference,
/// and past a handful the columns are too narrow to read anyway.
const MAX_PANELS: usize = 6;

/// Takes to remember. These are snapshots of a buffer that may be megabytes, so
/// they are bounded twice; the Undo control goes quiet at the end of the history
/// rather than silently doing nothing.
/// How long a "Copied" or "Saved to …" confirmation stays up.
const FLASH: Duration = Duration::from_secs(4);

const UNDO_DEPTH: usize = 100;
const UNDO_BYTES: usize = 64_000_000;

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
            result: false,
            saved_to: None,
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
            "Could not read {} as text: {e}. deltapanes compares text.",
            path.display()
        )
    })
}

fn panel_read_error(path: &Path, error: &std::io::Error) -> String {
    format!(
        "Could not read {} as text: {error}. deltapanes compares text.",
        path.display()
    )
}

fn panel_too_large(path: &Path, bytes: usize) -> String {
    format!(
        "Could not read {}: it is {:.1} MB. deltapanes stops at {} MB so the GUI stays responsive.",
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
        let temp = parent.join(format!(".{name}.deltapanes-{}-{nonce}", std::process::id()));
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
    Done(usize, Cached),
    Failed(RenderKey, String),
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
                "deltapanes cannot tell which rows each difference covers here, so it is not \
                 offering to take them. The diff itself is unaffected."
                    .to_string(),
            ),
        ),
        _ => (
            Vec::new(),
            Some(
                "The diff does not account for everything these panels differ by, so \
                 deltapanes is not offering to take from it. The diff itself is unaffected."
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
    prepared: HashMap<usize, PreparedDiff>,
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
    /// A render was asked for outright -- by ⌘⏎, or by taking a difference --
    /// rather than following from a keystroke. `tick` will not schedule one for a
    /// pair over `AUTO_RENDER_BYTES` and `schedule` drops it while another is in
    /// flight, so without this a take on a large pair would leave the diff stale
    /// and every control on it dead until the user pressed ⌘⏎.
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
    closing: bool,
    destructive: Option<DestructiveAction>,
    focus_panel: Option<usize>,
    /// A short confirmation next to the result's own controls -- "Copied",
    /// "Saved to …". Not a banner: these answer a button that was just pressed,
    /// and a dismissible strip across the diff is too much furniture for that.
    flash: Option<(String, Instant)>,
    tx: Sender<Job>,
    rx: Receiver<Job>,
}

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
        let compared = panels.iter().take(2).all(|p| !p.text.is_empty());
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
                "The saved delta syntax theme is no longer available, so deltapanes returned to your gitconfig."
            } else {
                "The saved delta syntax theme is no longer available, so deltapanes returned to delta's default."
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
            prepared: HashMap::new(),
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
            closing: false,
            destructive: None,
            focus_panel: None,
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
    /// site with a single `bool` to guard it, so holding ⌘⏎ started twenty-five
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
                    "Panel {} holds {:.1} MB. deltapanes stops at {} MB because delta takes \
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
        self.in_flight = Some(key);
        self.requested = false;
        std::thread::spawn(move || {
            let job = match render_job(
                &delta,
                &left,
                &right,
                &opts,
                job_key.clone(),
                columns,
                merging,
            ) {
                Ok(cached) => Job::Done(shown, cached),
                Err(e) => Job::Failed(job_key, e.to_string()),
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
                    self.error = Some(e);
                    return;
                }
                self.touch_panel(i);
            }
        }
        self.compared = true;
        self.pending_edit = None;
        self.requested = true;
        self.schedule(ctx);
    }

    fn poll(&mut self) {
        while let Ok(job) = self.rx.try_recv() {
            match job {
                Job::Done(panel, cached) => {
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
                    self.cache.insert(panel, cached);
                }
                Job::Failed(key, e) => {
                    if self.in_flight.as_ref() == Some(&key) {
                        self.in_flight = None;
                    }
                    self.failed = Some(key);
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
        let loses_work = self
            .affected_panels(&action)
            .into_iter()
            .any(|i| self.panel_has_unsaved_content(i));
        if loses_work {
            self.destructive = Some(action);
        } else {
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

    fn add_panel(&mut self) {
        if self.panels.len() < MAX_PANELS {
            self.panels.push(Panel::empty());
            self.focused = self.panels.len() - 1;
            self.focus_panel = Some(self.focused);
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

    fn paste_into_new_panel(&mut self) {
        let Some(text) = crate::clipboard_text() else {
            self.notice = Some(
                "The clipboard holds no text. deltapanes compares text; images and files \
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
        // edited, so on a saved result the next ⌘⏎ would read the take straight
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
        self.panels[i]
            .saved_to
            .clone()
            .or_else(|| self.mergetool.as_ref().map(|tool| tool.merged.clone()))
    }

    /// Write the result to a file. The only thing in deltapanes that writes one.
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
        match atomic_write(&path, self.panels[i].text.as_bytes()) {
            Ok(()) => {
                self.flash = Some((format!("Saved to {}", path.display()), Instant::now()));
                self.panels[i].saved_to = Some(path);
                self.panels[i].dirty = false;
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

    fn open_into(&mut self, i: usize, path: PathBuf) {
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
            Err(e) => self.error = Some(e),
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
        let Some(watcher) = self.watcher.as_mut() else {
            return;
        };
        let changed = watcher.poll();
        if let Some(error) = watcher.poll_errors().into_iter().last() {
            self.error = Some(format!(
                "File watching reported an error and may be out of date: {error}"
            ));
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

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if self.modal_active() {
            return;
        }
        // egui's own undo fires only for the field that has focus, so ⌘Z over
        // the diff does nothing at all -- which is exactly where it gets pressed
        // after a take. Claiming it there and nowhere else is what keeps the two
        // undo histories from fighting over one buffer.
        let typing = ctx.text_edit_focused();
        for action in ctx.input(keys::pressed) {
            match action {
                Action::OpenFile => self.choose_file_for_panel(self.shown),
                Action::CloseWindow => {
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
                    }
                }
                Action::MakeReference => {
                    let shown = self.shown;
                    self.set_reference(shown);
                }
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
                        self.pending_find_move =
                            if action == Action::NextMatch { 1 } else { -1 };
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
        let resolved = self.result_panel().is_some_and(|i| {
            !self.panels[i].dirty
                && self.panels[i]
                    .saved_to
                    .as_deref()
                    .is_some_and(|at| paths_refer_to_same_file(at, &tool.merged))
        });
        tool.resolved.store(resolved, Ordering::Relaxed);
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
        ui.horizontal_wrapped(|ui| {
            let ready = {
                let (a, b) = self.pair();
                !self.panels[a].text.is_empty() || !self.panels[b].text.is_empty()
            };
            let busy = self.in_flight.is_some();
            let label = if busy { "Rendering…" } else { "Compare" };
            if ui::primary(ui, &t, label, keys::compare_label(), ready && !busy) {
                self.compare_now(ctx);
            }

            ui.add_space(8.0);
            let view_changed = ui::segmented(
                ui,
                &t,
                &mut [
                    ("Side by side", &mut self.opts.side_by_side),
                    ("Numbers", &mut self.opts.line_numbers),
                    ("Wrap", &mut self.opts.wrap),
                ],
            );
            if view_changed {
                // An inherited option can turn a mode back on after its button
                // was switched off. The toolbar is an explicit user command.
                self.take_view_control();
            }

            ui.horizontal_wrapped(|ui| {
                ui.label(ui::micro(format!("{} cols", self.columns)).color(t.text_muted))
                    .on_hover_text(
                        "delta lays out against a column count, so the window's width is \
                         translated back into columns and the diff re-rendered on resize.",
                    );
                if self.panels.len() < MAX_PANELS && ui::ghost(ui, "+ Panel").clicked() {
                    self.add_panel();
                }
                let gear = ui.add(
                    egui::Button::selectable(self.show_settings, "Settings")
                        .corner_radius(radius::CONTROL)
                        .min_size(Vec2::new(0.0, 26.0)),
                );
                if gear.on_hover_text(keys::settings_hint()).clicked() {
                    self.show_settings = !self.show_settings;
                }
                if ui::icon(ui, "?", "Help", None)
                    .on_hover_text(keys::help_hint())
                    .clicked()
                {
                    self.show_help = !self.show_help;
                }
            });
        });
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
        let card_width = panel_card_width(ui.available_width(), inputs.len());
        let row_height = ui.available_height();
        egui::ScrollArea::horizontal()
            .id_salt("input-panels")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for i in inputs {
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
                            ui.horizontal_wrapped(|ui| {
                                let chip = ui.add(
                                    egui::Button::selectable(is_ref, ui::strong(title(i).to_string()))
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
                                ui.label(RichText::new(&panel_label).color(t.text_primary))
                                    .on_hover_text(
                                        panel
                                            .path
                                            .as_ref()
                                            .map_or_else(|| panel_label.clone(), |p| p.display().to_string()),
                                    );
                                if is_ref {
                                    ui.label(ui::micro("baseline").color(t.accent));
                                }
                                if panel.edited {
                                    ui.label(ui::micro("edited").color(t.warning)).on_hover_text(
                                        "The text here no longer matches the file. \
                                         What you see is what gets compared.",
                                    );
                                }
                                if panel.watch && panel.edited {
                                    ui.label(ui::micro("follow paused").color(t.warning))
                                        .on_hover_text(
                                            "Following is paused while this panel has edits, so a disk change cannot overwrite them.",
                                        );
                                }
                                if let Some(detail) = panel.detail() {
                                    ui.label(ui::small(detail).color(t.text_muted));
                                }
                                ui.horizontal_wrapped(|ui| {
                                    self.language_chip(ui, i, &t);
                                    if ui::ghost(ui, "Open…")
                                        .on_hover_text(format!("Load a file into panel {}", title(i)))
                                        .clicked()
                                    {
                                        open = Some(i);
                                    }
                                    if self.panel_menu(ui, i, &t) {
                                        remove = Some(i);
                                    }
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
                                        .hint_text(format!(
                                            "Panel {} editor — paste here, or drop a file",
                                            title(i)
                                        )),
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
                                // Sniffing is bounded but not free, so it
                                // happens on edit rather than every frame.
                                if response.changed() {
                                    self.panels[i].resniff();
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
                            },
                        );
                    }
                });
            });

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
    fn panel_menu(&mut self, ui: &mut egui::Ui, i: usize, t: &Tokens) -> bool {
        let mut remove = false;
        let menu_name = format!("Panel {} options", title(i));
        let button = egui::Button::new(RichText::new("⋯").color(t.text_secondary))
            .frame_when_inactive(false)
            .corner_radius(radius::CHIP)
            .min_size(Vec2::splat(26.0));
        let (response, _) = egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
            let has_file = self.panels[i].path.is_some();
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
                remove = true;
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
        remove
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
        let mut seed: Option<Option<usize>> = None;
        let mut stop = false;
        let mut undo = false;
        let mut go = None;
        let mut resume = None;
        ui.horizontal_wrapped(|ui| {
            if merging {
                ui.label(ui::micro("building").color(t.accent));
                ui.label(ui::strong(self.panel_label(reference)).color(t.text_primary));
                ui.label(ui::micro("from").color(t.text_muted));
            } else {
                let lead = if self.panels.len() <= 2 {
                    "comparing"
                } else {
                    "against"
                };
                ui.label(ui::micro(lead).color(t.text_muted));
                let reference_label = if self.panels.len() <= 2 {
                    format!("{} · {}", title(reference), self.panel_label(reference))
                } else {
                    self.panel_label(reference)
                };
                ui.label(RichText::new(reference_label).color(t.text_secondary));
                if self.panels.len() <= 2 {
                    ui.label(RichText::new("→").color(t.text_muted));
                    ui.label(
                        ui::strong(format!("{} · {}", title(shown), self.panel_label(shown)))
                            .color(t.text_primary),
                    );
                }
            }
            if merging || self.panels.len() > 2 {
                ui.add_space(6.0);
                for i in 0..self.panels.len() {
                    if i == reference {
                        continue;
                    }
                    let label = format!("{} · {}", title(i), self.panel_label(i));
                    let tab = egui::Button::selectable(shown == i, label)
                        .corner_radius(radius::CONTROL)
                        .min_size(Vec2::new(0.0, 26.0));
                    if ui.add(tab).clicked() {
                        go = Some(i);
                    }
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if merging {
                    if ui::ghost(ui, "Stop building")
                        .on_hover_text("Keep the text, put the take controls away.")
                        .clicked()
                    {
                        stop = true;
                    }
                    let can_undo = !self.undo.is_empty();
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
                    // Per candidate, and said so: taking from one panel can add
                    // differences against another, so there is no single number
                    // that counts down to zero.
                    let left = self.cache.get(&shown).map_or(0, |c| c.hunks.len());
                    ui.label(
                        ui::small(format!(
                            "{left} difference{} from {}",
                            if left == 1 { "" } else { "s" },
                            title(shown)
                        ))
                        .color(t.text_muted),
                    );
                } else {
                    let button = egui::Button::new("Combine…")
                        .corner_radius(radius::CONTROL)
                        .min_size(Vec2::new(0.0, 26.0));
                    egui::containers::menu::MenuButton::from_button(button)
                        .ui(ui, |ui| {
                            ui.label(ui::micro("start a result from").color(t.text_muted));
                            for i in 0..self.panels.len() {
                                if self.panels[i].text.is_empty() {
                                    continue;
                                }
                                let label = format!("{} · {}", title(i), self.panel_label(i));
                                if ui.button(label).clicked() {
                                    seed = Some(Some(i));
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
                    if self.result_panel().is_some()
                        && ui::ghost(ui, "Back to the result")
                            .on_hover_text("Make the result the baseline again")
                            .clicked()
                    {
                        resume = self.result_panel();
                    }
                }
            });
        });
        if let Some(i) = go {
            self.shown = i;
        }
        if let Some(from) = seed {
            self.request_destructive(DestructiveAction::Reseed(from));
        }
        if let Some(i) = resume {
            self.resume_result(i);
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
        if self
            .prepared
            .get(&self.shown)
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
        self.prepared.insert(
            self.shown,
            PreparedDiff {
                render_key: cached.key.clone(),
                style,
                whole,
                hunks,
            },
        );
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
        // Claimed before anything borrows `self`, and defaulted to the keyboard's
        // pending move so a chord and a button click go through one path.
        let mut move_hunk = std::mem::take(&mut self.pending_hunk_move);
        let mut move_find = std::mem::take(&mut self.pending_find_move);
        let mut take = None;
        let mut boxes = Vec::new();
        let mut offset = self.diff_offset;

        if let Some(prepared) = self.prepared.get(&self.shown)
            && !prepared.whole.text().is_empty()
        {
            let copy_text = prepared.whole.text().to_owned();
            // From the cache, not from `prepared`: the per-hunk layouts exist
            // only while merging, and the count is what every render knows.
            let hunk_count = self.cache.get(&self.shown).map_or(0, |c| c.hunks.len());
            let find_lines = find_line_offsets(&copy_text, &self.find_query);
            let mut close_find = false;
            ui.horizontal_wrapped(|ui| {
                if hunk_count > 0 {
                    if ui::ghost(ui, "Previous change").clicked() {
                        move_hunk = -1;
                    }
                    ui.label(
                        ui::small(format!(
                            "{} of {}",
                            self.hunk_cursor.min(hunk_count - 1) + 1,
                            hunk_count
                        ))
                        .color(t.text_muted),
                    );
                    if ui::ghost(ui, "Next change").clicked() {
                        move_hunk = 1;
                    }
                }
                if let Some(what) = self.effective_options().ignoring() {
                    ui.label(ui::small(what).color(t.text_muted)).on_hover_text(
                        "Some differences are being left out of this diff, from Settings.",
                    );
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui::ghost(ui, "Copy diff").clicked() {
                        ctx.copy_text(copy_text.clone());
                        self.flash = Some(("Diff copied".into(), Instant::now()));
                    }
                    if let Some((text, at)) = &self.flash
                        && text == "Diff copied"
                        && at.elapsed() < FLASH
                    {
                        ui::status(ui, text, t.success);
                    }
                });
            });
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
                    if ui::ghost(ui, "Previous").clicked() {
                        move_find = -1;
                    }
                    if ui::ghost(ui, "Next").clicked()
                        || (response.has_focus() && ui.input(|input| input.key_pressed(Key::Enter)))
                    {
                        move_find = 1;
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
            if self.find_jump && !find_lines.is_empty() {
                self.find_cursor = 0;
                self.restore_offset = Some(find_lines[0] as f32 * line_height);
                self.find_jump = false;
            } else if move_find != 0 && !find_lines.is_empty() {
                self.find_cursor = moved_cursor(self.find_cursor, find_lines.len(), move_find);
                self.restore_offset = Some(find_lines[self.find_cursor] as f32 * line_height);
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
                        let Some(prepared) = self.prepared.get(&self.shown) else {
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
                        let out = area.show(ui, |ui| {
                            // delta's colours are reproduced exactly; the only
                            // thing that may be tinted is the selection over
                            // them, whose default washes out on a diff ground.
                            ui.visuals_mut().selection.bg_fill = if t.dark {
                                Color32::from_rgb(0x3a, 0x4a, 0x63)
                            } else {
                                Color32::from_rgb(0xcd, 0xdd, 0xf5)
                            };
                            if !merging || c.hunks.is_empty() {
                                ui.add(prepared.whole.label(glyph));
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
                            // Zero spacing so the hunks still read as one block
                            // with control rows cut into it, rather than as a
                            // stack of separate cards. Selection stitches across
                            // adjacent labels, so the diff stays copyable whole.
                            ui.spacing_mut().item_spacing.y = 0.0;
                            let origin = ui.cursor().top();
                            for (n, (hunk, _span)) in c.hunks.iter().enumerate() {
                                let (control, clicked) = self.hunk_control(ui, &t, hunk, !stale);
                                if clicked {
                                    take = Some(n);
                                }
                                let body = ui.add(prepared.hunks[n].label(glyph));
                                boxes.push((
                                    control.top() - origin,
                                    body.rect.bottom() - control.top(),
                                ));
                            }
                        });
                        offset = out.state.offset.y;
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
                            "Replace these lines of {} with {}'s",
                            self.panel_label(self.reference),
                            self.panel_label(self.shown)
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

    /// What the result is called, how far it is from disk, and how big it is.
    fn result_identity(&mut self, ui: &mut egui::Ui, t: &Tokens, i: usize) {
        ui.label(ui::micro("result").color(t.accent));
        ui.label(ui::strong(self.panel_label(i)).color(t.text_primary));
        let state = match (&self.panels[i].saved_to, self.panels[i].dirty) {
            (None, _) => "not saved yet",
            (Some(_), true) => "unsaved changes",
            (Some(_), false) => "",
        };
        if !state.is_empty() {
            ui.label(ui::small(state).color(t.warning));
        }
        if let Some(detail) = self.panels[i].detail() {
            ui.label(ui::small(detail).color(t.text_muted));
        }
        if let Some((text, at)) = self.flash.clone() {
            if at.elapsed() < FLASH {
                ui::status(ui, &text, t.success);
                ui.ctx().request_repaint_after(FLASH);
            } else {
                self.flash = None;
            }
        }
    }

    /// The ways out of the feature: the clipboard, a file, or somewhere else.
    fn result_controls(&mut self, ui: &mut egui::Ui, t: &Tokens, i: usize, act: &mut BandActions) {
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
            if !first_save && ui.button("Save as…").clicked() {
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
        let mut act = BandActions::default();
        // Placed at the side there is no room for the name and every control on
        // one line, and a header that truncates is worse than one that stacks.
        if ui.available_width() < 560.0 {
            ui.horizontal_wrapped(|ui| self.result_identity(ui, &t, i));
            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| self.result_controls(ui, &t, i, &mut act));
        } else {
            ui.horizontal(|ui| {
                self.result_identity(ui, &t, i);
                ui.add_space(8.0);
                self.result_controls(ui, &t, i, &mut act);
            });
        }
        ui.add_space(6.0);
        egui::ScrollArea::both()
            .id_salt("result-band")
            .auto_shrink([false, false])
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
        let anchor = ui.min_rect().right_top() + egui::vec2(-8.0, 8.0);
        egui::Area::new(ui.id().with("stale"))
            .fixed_pos(anchor - egui::vec2(160.0, 0.0))
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                Frame::new()
                    .fill(t.surface_overlay)
                    .stroke(Stroke::new(1.0, t.border_strong))
                    .corner_radius(radius::CHIP)
                    .inner_margin(Margin::symmetric(10, 5))
                    .show(ui, |ui| {
                        ui.label(ui::small(text).color(t.text_secondary));
                    });
            });
    }

    fn no_difference(&self, ui: &mut egui::Ui, t: &Tokens) {
        let (a, b) = self.pair();
        let lines = self.panels[b].text.lines().count();
        if self.panels[a].text.is_empty() && self.panels[b].text.is_empty() {
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
        ui::empty_state(
            ui,
            t,
            "Nothing to compare yet",
            "Put text in two panels and deltapanes will diff them with the real delta binary.",
            &[
                ("paste", "into a panel, or drop a file onto it"),
                (keys::compare_label(), "compare"),
                (keys::help_label(), "keyboard shortcuts"),
            ],
        );
    }

    // ---- settings --------------------------------------------------------

    fn settings_drawer(&mut self, ui: &mut egui::Ui) {
        let t = ui::tokens(ui);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
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
                ui::field(ui, &t, "Theme", |ui| {
                    ui::choice(ui, &t, &mut theme, &options);
                });
                self.settings.theme = theme;

                let catalog_ready = self.catalog_rx.is_none();
                ui::field(ui, &t, "Interface", |ui| {
                    self.font_combo(ui, false, catalog_ready);
                });
                ui::field(ui, &t, "Size", |ui| {
                    ui.spacing_mut().slider_width = 120.0;
                    ui.add(
                        egui::Slider::new(&mut self.settings.ui_pt, UI_PT)
                            .suffix(" pt")
                            .text("Interface font size"),
                    );
                });
                ui.add_space(6.0);
                ui::field(ui, &t, "Diff", |ui| {
                    self.font_combo(ui, true, catalog_ready);
                });
                ui::field(ui, &t, "Size", |ui| {
                    ui.spacing_mut().slider_width = 120.0;
                    ui.add(
                        egui::Slider::new(&mut self.settings.mono_pt, MONO_PT)
                            .suffix(" pt")
                            .text("Diff font size"),
                    );
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
                ui::field(ui, &t, "Theme", |ui| {
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
                        .width(180.0)
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
                ui::field(ui, &t, "Show", |ui| {
                    let options: Vec<(Context, &str)> =
                        Context::ALL.iter().map(|c| (*c, c.label())).collect();
                    dirty |= ui::choice(ui, &t, &mut self.settings.context, &options);
                });
                ui::field(ui, &t, "Whitespace", |ui| {
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
                ui::field(ui, &t, "Ignore lines matching", |ui| {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.settings.ignore_matching)
                            .desired_width(180.0)
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
                            "No [delta] section in your gitconfig, so deltapanes is using \
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
        let current = if mono {
            self.settings.mono_font.clone()
        } else {
            self.settings.ui_font.clone()
        };
        let label = current
            .as_ref()
            .map(|f| f.family.clone())
            .unwrap_or_else(|| {
                if mono {
                    "Hack (bundled)".into()
                } else {
                    "Default".into()
                }
            });
        let salt = if mono { "font-mono" } else { "font-ui" };
        egui::ComboBox::from_id_salt(salt)
            .selected_text(label)
            .width(180.0)
            .show_ui(ui, |ui| {
                let mut pick: Option<Option<(Face, Option<Face>)>> = None;
                if ui
                    .selectable_label(
                        current.is_none(),
                        if mono { "Hack (bundled)" } else { "Default" },
                    )
                    .clicked()
                {
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
                "Opening the selected file will replace text that exists only in deltapanes.",
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
        let response = egui::Modal::new(egui::Id::new("destructive-confirmation"))
            .frame(
                Frame::new()
                    .fill(t.surface_overlay)
                    .stroke(Stroke::new(1.0, t.border_strong))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(Margin::same(24)),
            )
            .backdrop_color(Color32::from_black_alpha(if t.dark { 160 } else { 60 }))
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(RichText::new(heading).text_style(TextStyle::Heading));
                ui.add_space(8.0);
                ui.label(RichText::new(explanation).color(t.text_secondary));
                if !names.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new(names).color(t.warning));
                }
                ui.add_space(16.0);
                ui.horizontal_wrapped(|ui| {
                    if result_affected && ui::ghost(ui, "Save result, then continue").clicked() {
                        save_first = true;
                    }
                    if ui::ghost(ui, verb).clicked() {
                        confirm = true;
                    }
                    if ui::primary(ui, &t, "Cancel", "Esc", true) {
                        cancel = true;
                    }
                });
            });
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
    }

    /// Hold a close request while any panel contains work that exists only in
    /// this process. Result contents can be saved; other panels still require an
    /// explicit discard decision.
    fn quit_guard(&mut self, ctx: &egui::Context) {
        let unsaved = self.unsaved_panels();
        if ctx.input(|i| i.viewport().close_requested()) && !unsaved.is_empty() && !self.closing {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.show_help = false;
            self.destructive = None;
            self.quit_guard = true;
        }
        if !self.quit_guard {
            return;
        }
        if unsaved.is_empty() {
            self.quit_guard = false;
            return;
        }
        let result = self
            .result_panel()
            .filter(|i| self.panels[*i].dirty && unsaved.contains(i));
        let other_unsaved = unsaved.iter().any(|i| Some(*i) != result);
        let t = Tokens::of(ctx.theme());
        let mut discard = false;
        let mut keep = false;
        let mut save_and_quit = false;
        let mut save_result = false;
        let response = egui::Modal::new(egui::Id::new("quit-guard"))
            .frame(
                Frame::new()
                    .fill(t.surface_overlay)
                    .stroke(Stroke::new(1.0, t.border_strong))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(Margin::same(24)),
            )
            .backdrop_color(Color32::from_black_alpha(if t.dark { 160 } else { 60 }))
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(RichText::new("Unsaved panel contents").text_style(TextStyle::Heading));
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!(
                        "{} panel{} contain{} text or edits that exist only in deltapanes.",
                        unsaved.len(),
                        if unsaved.len() == 1 { "" } else { "s" },
                        if unsaved.len() == 1 { "s" } else { "" },
                    ))
                    .color(t.text_secondary),
                );
                ui.add_space(16.0);
                ui.horizontal_wrapped(|ui| {
                    if result.is_some() && !other_unsaved {
                        if ui::primary(ui, &t, "Save and quit", keys::save_label(), true) {
                            save_and_quit = true;
                        }
                    } else if result.is_some() && ui::ghost(ui, "Save result").clicked() {
                        save_result = true;
                    }
                    if ui::ghost(ui, "Discard and quit").clicked() {
                        discard = true;
                    }
                    if ui::ghost(ui, "Keep working").clicked() {
                        keep = true;
                    }
                });
            });
        if save_result {
            let _ = self.save_result(false);
        }
        if save_and_quit && self.save_result(false) {
            self.closing = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if discard {
            self.closing = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if keep || response.should_close() {
            self.quit_guard = false;
        }
    }

    fn help_modal(&mut self, ctx: &egui::Context) {
        if !self.show_help {
            return;
        }
        let t = Tokens::of(ctx.theme());
        let response = egui::Modal::new(egui::Id::new("help"))
            .frame(
                Frame::new()
                    .fill(t.surface_overlay)
                    .stroke(Stroke::new(1.0, t.border_strong))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(Margin::same(24)),
            )
            .backdrop_color(Color32::from_black_alpha(if t.dark { 160 } else { 60 }))
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(RichText::new("Keyboard & gestures").text_style(TextStyle::Heading));
                ui.add_space(12.0);
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
                            "{} pastes into deltapanes from anywhere while it runs",
                            h.label
                        ))
                        .color(t.text_muted),
                    );
                }
            });
        if response.should_close() {
            self.show_help = false;
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
                for (i, p) in self.panels.iter_mut().enumerate() {
                    // Following a file must not silently destroy typing. An
                    // edited panel keeps its buffer and stops tracking.
                    if p.watch && !p.edited {
                        match p.reload() {
                            Ok(()) => reloaded.push(i),
                            Err(e) => self.error = Some(e),
                        }
                    }
                }
                for i in reloaded {
                    self.touch_panel(i);
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

fn find_line_offsets(text: &str, query: &str) -> Vec<usize> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut matches = Vec::new();
    let mut last_byte = 0;
    let mut line = 0;
    for (byte, _) in text.match_indices(query) {
        line += text.as_bytes()[last_byte..byte]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count();
        matches.push(line);
        last_byte = byte;
    }
    matches
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

fn panel_card_width(available: f32, count: usize) -> f32 {
    let count = count.max(1) as f32;
    ((available - 8.0 * (count - 1.0)) / count).max(280.0)
}

fn source_panel_sizes(window_height: f32) -> (f32, f32) {
    if window_height < 640.0 {
        (130.0, 96.0)
    } else {
        (260.0, 160.0)
    }
}

fn effective_result_placement(width: f32, preferred: ResultPlacement) -> ResultPlacement {
    if width < 760.0 && preferred != ResultPlacement::Bottom {
        ResultPlacement::Bottom
    } else {
        preferred
    }
}

fn result_panel_sizes(window_height: f32, placement: ResultPlacement) -> (f32, f32) {
    match placement {
        ResultPlacement::Bottom if window_height < 640.0 => (128.0, 88.0),
        ResultPlacement::Bottom => (240.0, 150.0),
        ResultPlacement::Left | ResultPlacement::Right => (440.0, 260.0),
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
        self.publish_resolution();

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
            let mut open = true;
            egui::Window::new("Settings")
                .id(egui::Id::new("settings-overlay"))
                .open(&mut open)
                .resizable(true)
                .default_width(340.0)
                .min_width(240.0)
                .max_width((ui.available_width() - 32.0).max(240.0))
                .frame(
                    Frame::window(&ctx.style_of(ctx.theme()))
                        .inner_margin(Margin::symmetric(16, 14)),
                )
                .show(ctx, |ui| self.settings_drawer(ui));
            self.show_settings &= open;
        } else if self.show_settings {
            let max_width = (ui.available_width() - 360.0).clamp(240.0, 420.0);
            egui::Panel::right("settings")
                .resizable(true)
                .default_size(340.0_f32.min(max_width))
                .min_size(240.0)
                .max_size(max_width)
                .frame(
                    Frame::new()
                        .fill(t.surface_raised)
                        .inner_margin(Margin::symmetric(16, 14)),
                )
                .show(ui, |ui| self.settings_drawer(ui));
        }

        let (source_default, source_min) = source_panel_sizes(ui.available_height());
        egui::Panel::top("panels")
            .resizable(true)
            .default_size(source_default)
            .min_size(source_min)
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
            let (result_default, result_min) = result_panel_sizes(ui.available_height(), placement);
            let panel = match placement {
                // Costs the diff no width, which matters: delta lays out against
                // a column count, and side by side is the widest thing here.
                ResultPlacement::Bottom => {
                    egui::Panel::bottom("result-bottom").default_size(result_default)
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

        self.handle_keys(ctx);
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
                path: PathBuf::from("deltapanes-test-delta-does-not-exist"),
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

    fn test_mergetool_app(files: &[PathBuf], merged: PathBuf) -> (App, Arc<AtomicBool>) {
        let resolved = Arc::new(AtomicBool::new(false));
        let app = App::new(
            Delta {
                path: PathBuf::from("deltapanes-test-delta-does-not-exist"),
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
        assert!(app.panels[result].text.is_empty(), "seeded from an empty base");
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

        let (mut app, resolved) =
            test_mergetool_app(&[base, local, remote], merged.clone());
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

        let (mut app, resolved) =
            test_mergetool_app(&[base, local.clone(), local], merged.clone());
        let result = app.result_panel().expect("a result to merge into");

        app.publish_resolution();
        assert!(!resolved.load(Ordering::Relaxed), "nothing written yet");

        app.panels[result].saved_to = Some(merged.clone());
        app.panels[result].dirty = false;
        app.publish_resolution();
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
            features: Some(vec!["deltapanes-test-feature-that-does-not-exist".into()]),
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
    fn find_reports_the_rendered_line_of_each_match() {
        assert_eq!(
            find_line_offsets("zero\nneedle\ntwo needle\n", "needle"),
            vec![1, 2]
        );
        assert!(find_line_offsets("needle", "").is_empty());
    }

    #[test]
    fn navigation_clamps_a_cursor_left_over_from_a_longer_render() {
        assert_eq!(moved_cursor(99, 2, -1), 0);
        assert_eq!(moved_cursor(99, 2, 1), 0);
    }

    fn temp_path(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        std::env::temp_dir().join(format!("deltapanes-{label}-{}-{nonce}", std::process::id()))
    }

    fn file_panel(text: &str) -> Panel {
        let mut p = Panel::empty();
        p.text = text.to_string();
        p.path = Some(PathBuf::from("/tmp/thing.rs"));
        p.resniff();
        p
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

    /// A take writes the candidate's lines into the result, and says so: without
    /// `edited`, `compare_now` re-reads a saved result from disk and the take is
    /// silently undone by the next ⌘⏎.
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
                .starts_with(&format!(".{stem}.deltapanes-"))
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

    #[test]
    fn panel_width_policy_scrolls_instead_of_crushing_dense_rows() {
        assert_eq!(panel_card_width(600.0, 2), 296.0);
        assert_eq!(panel_card_width(600.0, 6), 280.0);
        assert!(panel_card_width(1_440.0, 2) > 600.0);
    }

    #[test]
    fn compact_windows_keep_source_and_result_editors_visible() {
        assert_eq!(source_panel_sizes(600.0), (130.0, 96.0));
        assert_eq!(source_panel_sizes(900.0), (260.0, 160.0));
        assert_eq!(
            effective_result_placement(700.0, ResultPlacement::Right),
            ResultPlacement::Bottom
        );
        assert_eq!(
            effective_result_placement(1_200.0, ResultPlacement::Right),
            ResultPlacement::Right
        );
        assert_eq!(
            result_panel_sizes(600.0, ResultPlacement::Bottom),
            (128.0, 88.0)
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
