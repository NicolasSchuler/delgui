use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use deltapanes_core::ansi::{self, Line};
use deltapanes_core::language;
use deltapanes_core::watch::FileWatcher;
use deltapanes_core::delta::{Delta, Input, Options};
use egui::{FontId, Key};

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

pub struct Panel {
    pub title: String,
    pub text: String,
    pub path: Option<PathBuf>,
    /// Syntax sniffed from the panel's own content, recomputed only on edit.
    /// `None` means "this looks like prose", which is a perfectly good answer.
    pub detected: Option<&'static str>,
    /// Re-read and re-diff when the bound file changes on disk.
    pub watch: bool,
}

impl Panel {
    fn new(title: &str) -> Self {
        Self { title: title.into(), text: String::new(), path: None, detected: None, watch: false }
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

    /// Bind the panel to a file, adopting its contents.
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
}

#[derive(Default, PartialEq, Clone)]
struct RenderKey {
    left: String,
    right: String,
    args: Vec<String>,
}

enum Job {
    Done(Vec<Line>),
    Failed(String),
}

pub struct App {
    delta: Delta,
    panels: [Panel; 2],
    opts: Options,
    /// Empty means "use whatever the panels imply". Typed text wins over that.
    language_override: String,
    palette: Palette,

    diff: Vec<Line>,
    error: Option<String>,
    rendering: bool,
    last_key: RenderKey,
    columns: usize,
    pending_resize: Option<Instant>,
    watcher: Option<FileWatcher>,
    pending_reload: Option<Instant>,
    /// Rects of the two panels, recorded during layout so a dropped file can be
    /// routed to whichever one it landed on.
    panel_rects: [egui::Rect; 2],
    /// Compare once on the first frame when both panels arrived pre-filled.
    autorun: bool,
    tx: Sender<Job>,
    rx: Receiver<Job>,
}

impl App {
    pub fn new(delta: Delta, preload: Option<String>, files: &[PathBuf], watch: bool) -> Self {
        let (tx, rx) = channel();
        let mut panels = [Panel::new("A"), Panel::new("B")];
        if let Some(text) = preload {
            panels[0].text = text;
        }
        // A preloaded clipboard occupies panel A, so files fill from whichever
        // panels are still empty.
        let mut slots = panels.iter().position(|p| p.text.is_empty()).unwrap_or(0);
        for f in files {
            if slots > 1 {
                break;
            }
            panels[slots].text = std::fs::read_to_string(f).unwrap_or_default();
            panels[slots].path = Some(f.clone());
            panels[slots].watch = watch;
            slots += 1;
        }
        for p in panels.iter_mut() {
            p.resniff();
        }
        let autorun = panels.iter().all(|p| !p.text.is_empty());
        Self {
            delta,
            panels,
            opts: Options { side_by_side: true, ..Options::default() },
            language_override: String::new(),
            palette: Palette::dark(),
            diff: Vec::new(),
            error: None,
            rendering: false,
            last_key: RenderKey::default(),
            columns: 120,
            pending_resize: None,
            watcher: None,
            pending_reload: None,
            panel_rects: [egui::Rect::NOTHING; 2],
            autorun,
            tx,
            rx,
        }
    }

    fn current_key(&self) -> RenderKey {
        RenderKey {
            left: self.panels[0].path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| self.panels[0].text.clone()),
            right: self.panels[1].path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| self.panels[1].text.clone()),
            args: self.effective_options().to_args(),
        }
    }

    /// Resolve the syntax to hand delta, in order of how much we trust it.
    ///
    /// delta infers from the right-hand path only, so panel B is consulted
    /// first: whatever it says is what delta would have concluded on its own.
    fn resolve_language(&self) -> Option<String> {
        let explicit = self.language_override.trim();
        if !explicit.is_empty() {
            return Some(explicit.to_string());
        }
        self.panels[1].language_hint().or_else(|| self.panels[0].language_hint())
    }

    fn effective_options(&self) -> Options {
        Options {
            width: self.columns as u16,
            default_language: self.resolve_language(),
            ..self.opts.clone()
        }
    }

    fn compare(&mut self, ctx: &egui::Context) {
        if let Some(p) = self.panels.iter().find(|p| p.text.len() > MAX_PANEL_BYTES) {
            self.error = Some(format!(
                "Panel {} holds {:.1} MB, over the {} MB limit.\n\
                 delta takes several seconds at this size and produces ~7x its input in styled output.",
                p.title,
                p.text.len() as f64 / 1e6,
                MAX_PANEL_BYTES / 1024 / 1024
            ));
            return;
        }
        let key = self.current_key();
        self.last_key = key;
        self.rendering = true;
        self.error = None;

        let (delta, opts) = (self.delta.clone(), self.effective_options());
        let (left, right) = (self.panels[0].to_input(), self.panels[1].to_input());
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let job = match delta.render(&left, &right, &opts) {
                Ok(bytes) => Job::Done(ansi::parse(&bytes)),
                Err(e) => Job::Failed(e.to_string()),
            };
            let _ = tx.send(job);
            ctx.request_repaint();
        });
    }

    /// Bring the watcher in line with the panels' `watch` flags.
    fn sync_watches(&mut self, ctx: &egui::Context) {
        let wanted: Vec<Option<PathBuf>> = self
            .panels
            .iter()
            .map(|p| p.watch.then(|| p.path.clone()).flatten())
            .collect();
        if wanted.iter().all(|w| w.is_none()) && self.watcher.is_none() {
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
        for (panel, want) in self.panels.iter().zip(&wanted) {
            match want {
                Some(path) if !watcher.is_watching(path) => {
                    if let Err(e) = watcher.watch(path) {
                        self.error = Some(format!("cannot watch {}: {e}", path.display()));
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
    /// remaining panel when several arrive at once.
    fn accept_drops(&mut self, ctx: &egui::Context) {
        let (files, pos) = ctx.input(|i| {
            (
                i.raw
                    .dropped_files
                    .iter()
                    .map(|f| f.path().to_path_buf())
                    .collect::<Vec<_>>(),
                i.pointer.latest_pos(),
            )
        });
        if files.is_empty() {
            return;
        }
        let mut slot = self.drop_target(pos);
        for file in files.into_iter().take(2) {
            self.panels[slot].bind(file);
            slot = 1 - slot;
        }
        self.compare(ctx);
    }

    /// Whichever panel is under the cursor; failing that, the first empty one.
    fn drop_target(&self, pos: Option<egui::Pos2>) -> usize {
        if let Some(pos) = pos {
            if let Some(i) = self.panel_rects.iter().position(|r| r.contains(pos)) {
                return i;
            }
        }
        self.panels.iter().position(|p| p.text.is_empty()).unwrap_or(0)
    }

    fn poll(&mut self) {
        while let Ok(job) = self.rx.try_recv() {
            self.rendering = false;
            match job {
                Job::Done(lines) => self.diff = lines,
                Job::Failed(e) => self.error = Some(e),
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();
        self.poll();
        self.poll_watches();
        if self.autorun {
            self.autorun = false;
            self.compare(ctx);
        }

        let font = FontId::monospace(13.0);
        // delta lays out against a column count, so the GUI's pixel width has to
        // be translated back into columns and the diff re-rendered on resize.
        let glyph = ctx.fonts_mut(|f| f.glyph_width(&font, ' ')).max(1.0);

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Compare  ⌘⏎").clicked() {
                    self.compare(ctx);
                }
                ui.separator();
                ui.checkbox(&mut self.opts.side_by_side, "side-by-side");
                ui.checkbox(&mut self.opts.line_numbers, "line numbers");
                ui.checkbox(&mut self.opts.wrap, "wrap");
                ui.checkbox(&mut self.opts.inherit_gitconfig, "inherit gitconfig");
                ui.separator();
                ui.label("lang:");
                let auto = self
                    .panels[1]
                    .language_hint()
                    .or_else(|| self.panels[0].language_hint());
                ui.add(
                    egui::TextEdit::singleline(&mut self.language_override)
                        .desired_width(64.0)
                        .hint_text(auto.clone().unwrap_or_else(|| "prose".into())),
                )
                .on_hover_text(match &auto {
                    Some(l) => format!("detected {l} — type to override"),
                    None => "looks like prose; left unhighlighted — type to override".into(),
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!("{}  ·  {} cols", self.delta.version_string, self.columns))
                            .weak()
                            .small(),
                    );
                    if self.rendering {
                        ui.spinner();
                    }
                });
            });
        });

        egui::Panel::top("panels")
            .resizable(true)
            .default_size(260.0)
            .show(ui, |ui| {
                ui.columns(2, |cols| {
                    for (i, ui) in cols.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.strong(&self.panels[i].title);
                            ui.label(egui::RichText::new(self.panels[i].source_label()).weak().small());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if self.panels[i].path.is_some() {
                                    if ui.small_button("unbind").clicked() {
                                        self.panels[i].path = None;
                                        self.panels[i].watch = false;
                                    }
                                    ui.checkbox(&mut self.panels[i].watch, "watch")
                                        .on_hover_text("re-read and re-diff when the file changes on disk");
                                }
                                if ui.small_button("open…").clicked() {
                                    if let Some(p) = rfd::FileDialog::new().pick_file() {
                                        self.panels[i].bind(p);
                                    }
                                }
                            });
                        });
                        self.panel_rects[i] = ui.max_rect();
                        egui::ScrollArea::both().id_salt(i).show(ui, |ui| {
                            let edited = ui
                                .add(
                                    egui::TextEdit::multiline(&mut self.panels[i].text)
                                        .font(egui::TextStyle::Monospace)
                                        .desired_width(f32::INFINITY)
                                        .desired_rows(10)
                                        .hint_text("paste here"),
                                )
                                .changed();
                            // Sniffing is bounded but not free, so it happens on
                            // edit rather than every frame.
                            if edited {
                                self.panels[i].resniff();
                            }
                        });
                    }
                });
            });

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
            if self.diff.is_empty() {
                ui.weak("Paste into both panels, then press ⌘⏎.");
                return;
            }
            egui::ScrollArea::both().show(ui, |ui| {
                let job = to_layout_job(&self.diff, self.columns, font.clone(), &self.palette);
                ui.add(egui::Label::new(job).selectable(true));
            });
        });

        if ctx.input(|i| i.key_pressed(Key::Enter) && i.modifiers.command) {
            self.compare(ctx);
        }
        self.accept_drops(ctx);
        self.sync_watches(ctx);
        if !ctx.input(|i| i.raw.hovered_files.is_empty()) {
            let target = self.drop_target(ctx.input(|i| i.pointer.latest_pos()));
            ui.painter().rect_stroke(
                self.panel_rects[target],
                4.0,
                egui::Stroke::new(2.0, egui::Color32::from_rgb(0x7d, 0xb0, 0xf0)),
                egui::StrokeKind::Inside,
            );
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
                self.compare(ctx);
            } else {
                ctx.request_repaint_after(WATCH_DEBOUNCE);
            }
        }
        // Re-run once the drag settles rather than on every resize frame.
        if let Some(at) = self.pending_resize {
            if at.elapsed() >= RESIZE_DEBOUNCE {
                self.pending_resize = None;
                if !self.diff.is_empty() {
                    self.compare(ctx);
                }
            } else {
                ctx.request_repaint_after(RESIZE_DEBOUNCE);
            }
        }
        // A toggle in the toolbar should take effect without a second click.
        if !self.rendering && !self.diff.is_empty() && self.current_key() != self.last_key {
            self.compare(ctx);
        }
    }
}
