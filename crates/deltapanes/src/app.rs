use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use deltapanes_core::ansi::{self, Line};
use deltapanes_core::config::{self, DeltaConfig};
use deltapanes_core::delta::{Delta, Input, Options};
use deltapanes_core::language;
use deltapanes_core::watch::FileWatcher;
use egui::Key;

use crate::hotkey::Hotkey;
use crate::keys::{self, Action};
use crate::render::{Palette, to_layout_job};

/// Beyond this, delta itself becomes the bottleneck: ~0.8 s at 2 MB and ~6.6 s
/// at 19 MB, producing roughly seven times the input in ANSI. We refuse rather
/// than hang, since a diff you wait ten seconds for is a diff you did by eye.
const MAX_PANEL_BYTES: usize = 4 * 1024 * 1024;

/// Delta re-runs on every width change, so resizing has to settle first.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(120);

/// A save is rarely one filesystem event, and a file mid-write reads as
/// truncated, so watch events are coalesced before re-reading.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(180);

/// delta is a two-way tool. More panels means more pairs against one reference,
/// and past a handful the columns are too narrow to read anyway.
const MAX_PANELS: usize = 6;

pub struct Panel {
    pub text: String,
    pub path: Option<PathBuf>,
    /// Syntax sniffed from the panel's own content, recomputed only on edit.
    /// `None` means "this looks like prose", which is a perfectly good answer.
    pub detected: Option<&'static str>,
    /// Re-read and re-diff when the bound file changes on disk.
    pub watch: bool,
}

impl Panel {
    fn empty() -> Self {
        Self { text: String::new(), path: None, detected: None, watch: false }
    }

    fn to_input(&self) -> Input {
        match &self.path {
            Some(p) => Input::Path(p.clone()),
            None => Input::Buffer(self.text.as_bytes().to_vec()),
        }
    }

    fn resniff(&mut self) {
        self.detected = language::detect(&self.text);
    }

    /// What this panel would tell delta about its own syntax: a real path
    /// speaks for itself, otherwise fall back to what the content looks like.
    fn language_hint(&self) -> Option<String> {
        if let Some(ext) = self.path.as_ref().and_then(|p| p.extension()) {
            return Some(ext.to_string_lossy().into_owned());
        }
        self.detected.map(String::from)
    }

    fn source_label(&self) -> String {
        match &self.path {
            Some(p) => p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            None => format!("paste · {} bytes", self.text.len()),
        }
    }

    fn bind(&mut self, path: PathBuf) {
        self.text = std::fs::read_to_string(&path).unwrap_or_default();
        self.path = Some(path);
        self.resniff();
    }

    fn reload(&mut self) {
        if let Some(p) = &self.path {
            if let Ok(text) = std::fs::read_to_string(p) {
                self.text = text;
                self.resniff();
            }
        }
    }

    fn clear(&mut self) {
        *self = Self::empty();
    }
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
#[derive(Default, PartialEq, Clone)]
struct RenderKey {
    revision: u64,
    reference: usize,
    shown: usize,
    args: Vec<String>,
}

struct Cached {
    key: RenderKey,
    lines: Vec<Line>,
}

enum Job {
    Done(usize, RenderKey, Vec<Line>),
    Failed(String),
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
    /// Empty means "use whatever the panels imply". Typed text wins over that.
    language_override: String,
    themes: Vec<(String, bool)>,
    gitconfig: DeltaConfig,
    features: BTreeSet<String>,
    show_config: bool,
    show_help: bool,
    palette: Palette,

    cache: HashMap<usize, Cached>,
    error: Option<String>,
    rendering: bool,
    columns: usize,
    pending_resize: Option<Instant>,
    pending_reload: Option<Instant>,
    watcher: Option<FileWatcher>,
    hotkey: Option<Hotkey>,
    /// Rects of the panels, recorded during layout so a dropped file can be
    /// routed to whichever one it landed on.
    panel_rects: Vec<egui::Rect>,
    autorun: bool,
    /// Bumped whenever any panel's contents change. See [`RenderKey`].
    revision: u64,
    tx: Sender<Job>,
    rx: Receiver<Job>,
}

impl App {
    pub fn new(
        delta: Delta,
        preload: Option<String>,
        files: &[PathBuf],
        watch: bool,
        hotkey: Option<Hotkey>,
    ) -> Self {
        let (tx, rx) = channel();
        let mut panels = vec![Panel::empty(), Panel::empty()];
        if let Some(text) = preload {
            panels[0].text = text;
        }
        // A preloaded clipboard occupies panel A, so files fill from whichever
        // panels are still empty, adding panels when more than two arrive.
        for f in files {
            let slot = match panels.iter().position(|p| p.text.is_empty() && p.path.is_none()) {
                Some(i) => i,
                None if panels.len() < MAX_PANELS => {
                    panels.push(Panel::empty());
                    panels.len() - 1
                }
                None => break,
            };
            panels[slot].bind(f.clone());
            panels[slot].watch = watch;
        }
        for p in panels.iter_mut() {
            p.resniff();
        }
        let autorun = panels.iter().take(2).all(|p| !p.text.is_empty());
        let themes = delta.syntax_themes();
        let gitconfig = config::discover(None);
        let features = gitconfig.active.iter().cloned().collect();

        Self {
            delta,
            panels,
            reference: 0,
            shown: 1,
            focused: 0,
            opts: Options { side_by_side: true, ..Options::default() },
            language_override: String::new(),
            themes,
            gitconfig,
            features,
            show_config: false,
            show_help: false,
            palette: Palette::dark(),
            cache: HashMap::new(),
            error: None,
            rendering: false,
            columns: 120,
            pending_resize: None,
            pending_reload: None,
            watcher: None,
            hotkey,
            panel_rects: Vec::new(),
            autorun,
            revision: 0,
            tx,
            rx,
        }
    }

    // ---- comparison ------------------------------------------------------

    /// Resolve the syntax to hand delta, in order of how much we trust it.
    ///
    /// delta infers from the right-hand path only, so the shown panel is
    /// consulted first: whatever it says is what delta would conclude alone.
    fn resolve_language(&self) -> Option<String> {
        let explicit = self.language_override.trim();
        if !explicit.is_empty() {
            return Some(explicit.to_string());
        }
        self.panels
            .get(self.shown)
            .and_then(Panel::language_hint)
            .or_else(|| self.panels[self.reference].language_hint())
    }

    fn effective_options(&self) -> Options {
        Options {
            width: self.columns as u16,
            default_language: self.resolve_language(),
            features: self.features.iter().cloned().collect(),
            ..self.opts.clone()
        }
    }

    fn current_key(&self) -> RenderKey {
        RenderKey {
            revision: self.revision,
            reference: self.reference,
            shown: self.shown,
            args: self.effective_options().to_args(),
        }
    }

    /// Record that panel contents changed, invalidating cached diffs.
    fn touch(&mut self) {
        self.revision += 1;
    }

    fn compare(&mut self, ctx: &egui::Context) {
        self.normalize();
        if let Some(i) = self.panels.iter().position(|p| p.text.len() > MAX_PANEL_BYTES) {
            self.error = Some(format!(
                "Panel {} holds {:.1} MB, over the {} MB limit.\n\
                 delta takes several seconds at this size and produces ~7x its input in styled output.",
                title(i),
                self.panels[i].text.len() as f64 / 1e6,
                MAX_PANEL_BYTES / 1024 / 1024
            ));
            return;
        }
        let key = self.current_key();
        if self.cache.get(&self.shown).is_some_and(|c| c.key == key) {
            return; // already rendered for these inputs, flags and width
        }
        self.rendering = true;
        self.error = None;

        let (delta, opts) = (self.delta.clone(), self.effective_options());
        let (left, right) = (self.panels[self.reference].to_input(), self.panels[self.shown].to_input());
        let (tx, ctx, shown) = (self.tx.clone(), ctx.clone(), self.shown);
        std::thread::spawn(move || {
            let job = match delta.render(&left, &right, &opts) {
                Ok(bytes) => Job::Done(shown, key, ansi::parse(&bytes)),
                Err(e) => Job::Failed(e.to_string()),
            };
            let _ = tx.send(job);
            ctx.request_repaint();
        });
    }

    fn poll(&mut self) {
        while let Ok(job) = self.rx.try_recv() {
            self.rendering = false;
            match job {
                Job::Done(panel, key, lines) => {
                    self.cache.insert(panel, Cached { key, lines });
                }
                Job::Failed(e) => self.error = Some(e),
            }
        }
    }

    fn shown_diff(&self) -> Option<&[Line]> {
        self.cache.get(&self.shown).map(|c| c.lines.as_slice())
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

    fn add_panel(&mut self) {
        if self.panels.len() < MAX_PANELS {
            self.panels.push(Panel::empty());
            self.focused = self.panels.len() - 1;
        }
    }

    fn remove_panel(&mut self, i: usize) {
        if self.panels.len() <= 2 {
            return;
        }
        if let Some(path) = self.panels[i].path.clone() {
            if let Some(w) = self.watcher.as_mut() {
                w.unwatch(&path);
            }
        }
        self.panels.remove(i);
        // Cached diffs are keyed by index, which the removal has shifted.
        self.cache.clear();
        self.touch();
        if self.reference > i {
            self.reference -= 1;
        }
        self.normalize();
    }

    fn paste_into_new_panel(&mut self, ctx: &egui::Context) {
        let Some(text) = crate::clipboard_text() else { return };
        let slot = match self.panels.iter().position(|p| p.text.is_empty() && p.path.is_none()) {
            Some(i) => i,
            None if self.panels.len() < MAX_PANELS => {
                self.panels.push(Panel::empty());
                self.panels.len() - 1
            }
            None => self.focused,
        };
        self.panels[slot].clear();
        self.panels[slot].text = text;
        self.panels[slot].resniff();
        self.touch();
        self.shown = slot;
        self.focused = slot;
        self.compare(ctx);
    }

    // ---- files -----------------------------------------------------------

    /// Bring the watcher in line with the panels' `watch` flags.
    fn sync_watches(&mut self, ctx: &egui::Context) {
        let wanted: Vec<Option<PathBuf>> = self
            .panels
            .iter()
            .map(|p| p.watch.then(|| p.path.clone()).flatten())
            .collect();
        if wanted.iter().all(Option::is_none) && self.watcher.is_none() {
            return;
        }
        if self.watcher.is_none() {
            let ctx = ctx.clone();
            match FileWatcher::new(move || ctx.request_repaint()) {
                Ok(w) => self.watcher = Some(w),
                Err(e) => {
                    self.error = Some(format!("could not start file watching: {e}"));
                    for p in self.panels.iter_mut() {
                        p.watch = false;
                    }
                    return;
                }
            }
        }
        let watcher = self.watcher.as_mut().expect("just constructed");
        let mut failure = None;
        for (panel, want) in self.panels.iter().zip(&wanted) {
            match want {
                Some(path) if !watcher.is_watching(path) => {
                    if let Err(e) = watcher.watch(path) {
                        failure = Some(format!("cannot watch {}: {e}", path.display()));
                    }
                }
                None => {
                    if let Some(path) = &panel.path {
                        if watcher.is_watching(path) {
                            watcher.unwatch(path);
                        }
                    }
                }
                _ => {}
            }
        }
        self.error = self.error.take().or(failure);
    }

    fn poll_watches(&mut self) {
        let Some(watcher) = self.watcher.as_mut() else { return };
        let changed = watcher.poll();
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

    /// Route dropped files to the panel they landed on, spilling into the
    /// following panels when several arrive at once.
    fn accept_drops(&mut self, ctx: &egui::Context) {
        let (files, pos) = ctx.input(|i| {
            (
                i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect::<Vec<_>>(),
                i.pointer.latest_pos(),
            )
        });
        if files.is_empty() {
            return;
        }
        let mut slot = self.drop_target(pos);
        for file in files {
            if slot >= self.panels.len() {
                if self.panels.len() >= MAX_PANELS {
                    break;
                }
                self.panels.push(Panel::empty());
            }
            self.panels[slot].bind(file);
            slot += 1;
        }
        self.touch();
        self.cache.clear();
        self.compare(ctx);
    }

    /// Whichever panel is under the cursor; failing that, the first empty one.
    fn drop_target(&self, pos: Option<egui::Pos2>) -> usize {
        if let Some(pos) = pos {
            if let Some(i) = self.panel_rects.iter().position(|r| r.contains(pos)) {
                return i;
            }
        }
        self.panels
            .iter()
            .position(|p| p.text.is_empty() && p.path.is_none())
            .unwrap_or(0)
    }

    // ---- input -----------------------------------------------------------

    fn handle_keys(&mut self, ctx: &egui::Context) {
        for action in ctx.input(keys::pressed) {
            match action {
                Action::Compare => self.compare(ctx),
                Action::AddPanel => self.add_panel(),
                Action::RemovePanel => {
                    let victim = self.shown;
                    self.remove_panel(victim);
                    self.compare(ctx);
                }
                Action::ClearPanel => {
                    let f = self.focused;
                    self.panels[f].clear();
                    self.cache.remove(&f);
                    self.touch();
                }
                Action::PasteIntoNewPanel => self.paste_into_new_panel(ctx),
                Action::ShowDiff(i) => {
                    if i < self.panels.len() && i != self.reference {
                        self.shown = i;
                        self.compare(ctx);
                    }
                }
                Action::MakeReference => {
                    self.reference = self.shown;
                    self.cache.clear();
                    self.normalize();
                    self.compare(ctx);
                }
                Action::ToggleSideBySide => self.opts.side_by_side = !self.opts.side_by_side,
                Action::ToggleLineNumbers => self.opts.line_numbers = !self.opts.line_numbers,
                Action::ToggleWrap => self.opts.wrap = !self.opts.wrap,
                Action::ToggleHelp => self.show_help = !self.show_help,
            }
        }
        if self.show_help && ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.show_help = false;
        }
    }

    fn poll_hotkey(&mut self, ctx: &egui::Context) {
        if self.hotkey.as_ref().is_some_and(Hotkey::fired) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            self.paste_into_new_panel(ctx);
        }
    }

    // ---- drawing ---------------------------------------------------------

    fn toolbar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            if ui.button("Compare  ⌘⏎").clicked() {
                self.compare(ctx);
            }
            ui.separator();
            ui.checkbox(&mut self.opts.side_by_side, "side-by-side");
            ui.checkbox(&mut self.opts.line_numbers, "line numbers");
            ui.checkbox(&mut self.opts.wrap, "wrap");
            ui.separator();
            ui.label("lang:");
            let auto = self
                .panels
                .get(self.shown)
                .and_then(Panel::language_hint)
                .or_else(|| self.panels[self.reference].language_hint());
            ui.add(
                egui::TextEdit::singleline(&mut self.language_override)
                    .desired_width(64.0)
                    .hint_text(auto.clone().unwrap_or_else(|| "prose".into())),
            )
            .on_hover_text(match &auto {
                Some(l) => format!("detected {l} — type to override"),
                None => "looks like prose; left unhighlighted — type to override".into(),
            });
            ui.separator();
            ui.toggle_value(&mut self.show_config, "delta config");
            if ui.button("+ panel").on_hover_text("⌘N").clicked() {
                self.add_panel();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut status = format!("{} · {} cols", self.delta.version_string, self.columns);
                if let Some(h) = &self.hotkey {
                    status = format!("{status} · {} to paste", h.label);
                }
                ui.label(egui::RichText::new(status).weak().small())
                    .on_hover_text("⌘/ for keys");
                if self.rendering {
                    ui.spinner();
                }
            });
        });
    }

    /// Milestone 3: show what the user's gitconfig already tells delta, and let
    /// the named presets in it be switched on and off.
    fn config_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut dirty = false;
        ui.horizontal_wrapped(|ui| {
            ui.label("syntax theme:");
            let current = self.opts.syntax_theme.clone().unwrap_or_else(|| "(delta default)".into());
            egui::ComboBox::from_id_salt("theme")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    dirty |= ui
                        .selectable_value(&mut self.opts.syntax_theme, None, "(delta default)")
                        .changed();
                    for (name, dark) in &self.themes {
                        let label = format!("{name}  {}", if *dark { "· dark" } else { "· light" });
                        dirty |= ui
                            .selectable_value(&mut self.opts.syntax_theme, Some(name.clone()), label)
                            .changed();
                    }
                });
            ui.separator();
            dirty |= ui
                .checkbox(&mut self.opts.inherit_gitconfig, "inherit gitconfig")
                .on_hover_text(
                    "off passes --no-gitconfig, which is the only way to get output \
                     independent of your gitconfig and working directory",
                )
                .changed();
        });

        ui.add_space(4.0);
        if self.gitconfig.is_empty() {
            ui.weak(
                "No [delta] section found in your gitconfig. deltapanes is using its own \
                 defaults; anything you set here applies to this session only.",
            );
        } else {
            let names = self.gitconfig.selectable_features();
            if !names.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.label("features:");
                    for name in names {
                        let mut on = self.features.contains(&name);
                        if ui.checkbox(&mut on, &name).changed() {
                            if on {
                                self.features.insert(name.clone());
                            } else {
                                self.features.remove(&name);
                            }
                            dirty = true;
                        }
                    }
                });
            }
            if !self.gitconfig.settings.is_empty() {
                egui::CollapsingHeader::new(format!(
                    "{} setting(s) from your gitconfig",
                    self.gitconfig.settings.len()
                ))
                .show(ui, |ui| {
                    for (k, v) in &self.gitconfig.settings {
                        ui.label(egui::RichText::new(format!("{k} = {v}")).monospace().small());
                    }
                    for src in &self.gitconfig.sources {
                        ui.label(egui::RichText::new(src.display().to_string()).weak().small());
                    }
                });
            }
        }
        if dirty {
            self.compare(ctx);
        }
    }

    fn panel_row(&mut self, ui: &mut egui::Ui) {
        let n = self.panels.len();
        self.panel_rects.resize(n, egui::Rect::NOTHING);
        let mut remove = None;
        let mut new_reference = None;

        ui.columns(n, |cols| {
            for (i, ui) in cols.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    let is_ref = i == self.reference;
                    if ui
                        .selectable_label(is_ref, egui::RichText::new(title(i).to_string()).strong())
                        .on_hover_text("make this the reference everything is compared against")
                        .clicked()
                    {
                        new_reference = Some(i);
                    }
                    if is_ref {
                        ui.label(egui::RichText::new("ref").weak().small());
                    }
                    ui.label(egui::RichText::new(self.panels[i].source_label()).weak().small());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if n > 2 && ui.small_button("✕").on_hover_text("remove panel").clicked() {
                            remove = Some(i);
                        }
                        if self.panels[i].path.is_some() {
                            if ui.small_button("unbind").clicked() {
                                self.panels[i].path = None;
                                self.panels[i].watch = false;
                                self.revision += 1;
                            }
                            ui.checkbox(&mut self.panels[i].watch, "watch")
                                .on_hover_text("re-read and re-diff when the file changes on disk");
                        }
                        if ui.small_button("open…").clicked() {
                            if let Some(p) = rfd::FileDialog::new().pick_file() {
                                self.panels[i].bind(p);
                                self.revision += 1;
                            }
                        }
                    });
                });
                self.panel_rects[i] = ui.max_rect();
                egui::ScrollArea::both().id_salt(i).show(ui, |ui| {
                    let response = ui.add(
                        egui::TextEdit::multiline(&mut self.panels[i].text)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY)
                            .desired_rows(10)
                            .hint_text("paste here"),
                    );
                    if response.has_focus() {
                        self.focused = i;
                    }
                    // Sniffing is bounded but not free, so it happens on edit
                    // rather than every frame.
                    if response.changed() {
                        self.panels[i].resniff();
                        self.revision += 1;
                    }
                });
            }
        });

        if let Some(i) = new_reference {
            self.reference = i;
            self.cache.clear();
            self.normalize();
        }
        if let Some(i) = remove {
            self.remove_panel(i);
        }
    }

    /// One tab per non-reference panel. delta is a two-way tool, so N panels
    /// means N-1 pairs against the reference rather than any N-way diff.
    fn tabs(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.panels.len() <= 2 {
            return;
        }
        ui.horizontal(|ui| {
            for i in 0..self.panels.len() {
                if i == self.reference {
                    continue;
                }
                let label = format!("{} → {}", title(self.reference), title(i));
                if ui.selectable_label(self.shown == i, label).clicked() {
                    self.shown = i;
                    self.compare(ctx);
                }
            }
        });
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_help;
        egui::Window::new("keys")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                egui::Grid::new("keys-grid").num_columns(2).spacing([18.0, 4.0]).show(ui, |ui| {
                    for (label, describe) in keys::help_rows() {
                        ui.label(egui::RichText::new(label).monospace().strong());
                        ui.label(describe);
                        ui.end_row();
                    }
                });
                if let Some(h) = &self.hotkey {
                    ui.add_space(6.0);
                    ui.weak(format!("{} works system-wide while deltapanes runs", h.label));
                }
            });
        self.show_help = open;
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();
        self.poll();
        self.poll_watches();
        self.poll_hotkey(ctx);
        if self.autorun {
            self.autorun = false;
            self.compare(ctx);
        }

        let font = egui::FontId::monospace(13.0);
        // delta lays out against a column count, so the GUI's pixel width has to
        // be translated back into columns and the diff re-rendered on resize.
        let glyph = ctx.fonts_mut(|f| f.glyph_width(&font, ' ')).max(1.0);

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui, ctx));
        if self.show_config {
            egui::Panel::top("config").show(ui, |ui| self.config_ui(ui, ctx));
        }
        egui::Panel::top("panels")
            .resizable(true)
            .default_size(240.0)
            .show(ui, |ui| self.panel_row(ui));
        egui::Panel::top("tabs").show_separator_line(false).show(ui, |ui| self.tabs(ui, ctx));

        egui::CentralPanel::default().show(ui, |ui| {
            let cols = ((ui.available_width() / glyph).floor() as usize).clamp(20, 400);
            if cols != self.columns {
                self.columns = cols;
                self.pending_resize = Some(Instant::now());
            }
            if let Some(e) = &self.error {
                ui.colored_label(egui::Color32::from_rgb(0xef, 0x7b, 0x74), e);
                return;
            }
            match self.shown_diff() {
                None => {
                    ui.weak("Paste into two panels, then press ⌘⏎.   ⌘/ for keys.");
                }
                Some(lines) => {
                    let job = to_layout_job(lines, self.columns, font.clone(), &self.palette);
                    egui::ScrollArea::both().show(ui, |ui| {
                        ui.add(egui::Label::new(job).selectable(true));
                    });
                }
            }
        });

        self.handle_keys(ctx);
        self.help_window(ctx);
        self.accept_drops(ctx);
        self.sync_watches(ctx);
        if !ctx.input(|i| i.raw.hovered_files.is_empty()) {
            let target = self.drop_target(ctx.input(|i| i.pointer.latest_pos()));
            if let Some(rect) = self.panel_rects.get(target) {
                ui.painter().rect_stroke(
                    *rect,
                    4.0,
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(0x7d, 0xb0, 0xf0)),
                    egui::StrokeKind::Inside,
                );
            }
        }
        // Re-read only once the writes stop, so a half-written file is never
        // what gets diffed.
        if let Some(at) = self.pending_reload {
            if at.elapsed() >= WATCH_DEBOUNCE {
                self.pending_reload = None;
                for p in self.panels.iter_mut() {
                    if p.watch {
                        p.reload();
                    }
                }
                self.touch();
                self.cache.clear();
                self.compare(ctx);
            } else {
                ctx.request_repaint_after(WATCH_DEBOUNCE);
            }
        }
        // Re-run once the drag settles rather than on every resize frame.
        if let Some(at) = self.pending_resize {
            if at.elapsed() >= RESIZE_DEBOUNCE {
                self.pending_resize = None;
                if !self.cache.is_empty() {
                    self.compare(ctx);
                }
            } else {
                ctx.request_repaint_after(RESIZE_DEBOUNCE);
            }
        }
        // A toolbar toggle should take effect without a second click. compare()
        // is a no-op when the cache already matches these inputs.
        if !self.rendering && !self.cache.is_empty() {
            self.compare(ctx);
        }
    }
}
