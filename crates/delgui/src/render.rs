//! Turning parsed delta output into something egui can draw.

use std::ops::Range;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use delgui_core::ansi::{self, Color, Line, Style};
use egui::text::{LayoutJob, TextFormat};
use egui::{
    Color32, Context, CursorIcon, Event, EventFilter, FontId, Galley, Id, Key, Modifiers, OpenUrl,
    Rect, Response, Sense, Stroke, Ui, UiBuilder, Vec2, Widget, WidgetInfo, WidgetType,
};
use unicode_width::UnicodeWidthStr;

use crate::theme::Tokens;

/// Resolve delta's colours against a palette.
///
/// Indices 0-15 are the terminal's own theme colours, which delta uses for
/// structural elements (file headers, hunk rules, line numbers). A terminal
/// takes these from the user's colour scheme; we have to supply them, so they
/// come from the app's theme -- which is also why index 4 is the chrome accent:
/// it is the only one delta reaches for by default.
pub struct Palette {
    pub ansi16: [Color32; 16],
    pub foreground: Color32,
    pub background: Color32,
}

impl Palette {
    /// The two are built together on purpose. `background` is not a colour of
    /// its own: it is *whatever the diff is drawn on*, and it is used to
    /// resolve reverse-video spans. Typing it independently is how it drifted
    /// one grey step away from the surface it was supposed to match.
    pub fn from_tokens(t: &Tokens) -> Self {
        Self {
            ansi16: t.ansi16,
            foreground: t.text_primary,
            background: t.surface_sunken,
        }
    }

    fn resolve(&self, c: Color) -> Color32 {
        match c {
            Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
            Color::Indexed(i) => xterm256(i, &self.ansi16),
        }
    }
}

/// The standard xterm 256-colour cube, with the first 16 deferred to the theme.
fn xterm256(i: u8, ansi16: &[Color32; 16]) -> Color32 {
    match i {
        0..=15 => ansi16[i as usize],
        16..=231 => {
            let n = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Color32::from_rgb(level(n / 36), level((n / 6) % 6), level(n % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            Color32::from_rgb(v, v, v)
        }
    }
}

/// True when delta printed nothing to draw: the two inputs are the same.
pub fn is_empty(lines: &[Line]) -> bool {
    ansi::body(lines).is_empty()
}

/// One OSC 8 link in a prepared diff, addressed in character offsets so it can
/// be hit-tested against an egui [`Galley`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hyperlink {
    pub char_range: Range<usize>,
    pub target: String,
}

/// The visual tail requested by ANSI erase-to-end-of-line (`ESC[K`).
///
/// It is metadata rather than spaces in the laid-out text: terminal erase cells
/// are paint, not file content, and must not turn into trailing whitespace when
/// the user copies a selection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EraseFill {
    pub row: usize,
    pub from_column: usize,
    pub background: Color32,
}

/// Large enough to keep widget overhead low, small enough that scrolling only
/// submits a few hundred rows to egui's painter and accessibility tree.
///
/// The chunk is also the unit of the accessibility tree: one `Role::Label` node
/// per chunk, *always* -- registration is what makes a row reachable, and a row
/// that is merely scrolled out of view must not become unreachable -- with the
/// per-row `Role::TextRun` children built only while the chunk is drawn. Which
/// is why [`RENDERED_DIFF_LABEL`] names the enclosing region and never a chunk:
/// a widget-info label on a `Role::Label` node is written as the node's *value*
/// (`response.rs:962-968`), i.e. as a claim about what the label says, so a
/// chunk's own text is the only honest thing to put there.
const PREPARED_CHUNK_ROWS: usize = 192;
const VIEWPORT_OVERSCAN_CHUNKS: usize = 1;
const RENDERED_DIFF_LABEL: &str = "Rendered diff";
/// Deliberately not [`RENDERED_DIFF_LABEL`]: the region and the keyboard target
/// are two nodes, and a test that asserts the diff is named exactly once cannot
/// tell them apart if they share a string.
const DIFF_SCROLL_LABEL: &str = "Diff scroll region";

/// Distinguishes one retained layout from the next without hashing or copying
/// its text. AccessKit updates are incremental, so a new layout must seed every
/// off-screen node once even though later frames can leave unchanged nodes out.
static NEXT_ACCESSIBILITY_REVISION: AtomicU64 = AtomicU64::new(1);

/// Whether a chunk is handed to egui's selection machinery this frame.
///
/// `Anchored` is not "selected": it is "egui must re-encounter this galley or
/// it will drop the selection or truncate the copy". See [`SelectionEnds`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mount {
    Culled,
    Anchored,
}

#[derive(Clone, Copy)]
struct ChunkFrame {
    mount: Mount,
    publish_accessibility: bool,
}

/// Which chunks hold the ends of a label selection.
///
/// egui knows exactly this (`WidgetTextCursor::widget_id`) but keeps it private
/// -- `states`, `CurrentSelection` and `WidgetTextCursor` are all private
/// (`label_text_selection.rs:21,65,85`) -- and exposes only `has_selection()`,
/// which is true for a selection *anywhere in any viewport*, including the
/// collapsed one a single click on any ordinary label leaves behind (`on_label`
/// stores a `CCursorRange::one` on a plain press, `:590-628`). Gating culling on
/// that flag therefore turns culling off for the whole diff whenever the user
/// clicks the "ignoring" note or a flash pill.
///
/// So we mirror the one signal that actually assigns an end to a widget: a
/// pointer press or drag over it (`cursor_for`, `:369-376`). Only the two
/// endpoints have to be re-encountered each frame -- `on_end_pass` takes the
/// selection away unless both `has_reached_primary` and `has_reached_secondary`
/// were set (`:222-227`), and the `is_in_middle` test at `:502` is
/// `has_reached_primary != has_reached_secondary`, which works with gaps.
///
/// `None` is meaningful: that end lives in a widget outside the diff, which is
/// drawn unconditionally and so is always "reached" without help from us.
///
/// Two paths could in principle disagree with egui, and both were read and do
/// not apply: keyboard selection stays inside the galley that already holds
/// primary (`:571-574`, `:688`), and the upward/downward drag-extension
/// branches (`:377-403`) only fire for widgets that are on screen and therefore
/// drawn anyway. Re-check both on an egui bump; the symptom of a disagreement
/// is a silently dropped selection, never a wrong render.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct SelectionEnds {
    anchor: Option<Id>,
    focus: Option<Id>,
}

impl SelectionEnds {
    fn holds(&self, id: Id) -> bool {
        self.anchor == Some(id) || self.focus == Some(id)
    }
}

/// What one frame decided to register, draw and hand to the selection.
struct MountPlan {
    visible: Range<usize>,
    ends: SelectionEnds,
    copying: bool,
}

/// What the previous AccessKit pass mounted for one retained layout.
///
/// `drawn` is kept so a chunk that has just left the viewport is submitted one
/// last time without its per-row text-run children. After that its complete
/// label node remains in AccessKit unchanged until it comes back into view.
#[derive(Clone, Default)]
struct AccessibilityMount {
    revision: u64,
    frame: u64,
    pass: u64,
    drawn: Vec<usize>,
}

impl MountPlan {
    fn mount(&self, id: Id) -> Mount {
        if self.copying || self.ends.holds(id) {
            Mount::Anchored
        } else {
            Mount::Culled
        }
    }

    /// Drawn means the galley is painted and the chunk is handed to egui's
    /// selection. Registration is unconditional and happens either way.
    fn drawn(&self, index: usize, id: Id) -> bool {
        self.visible.contains(&index) || self.mount(id) == Mount::Anchored
    }
}

/// Deterministic per-chunk ids, rather than the auto-ids `push_id`/`place` used
/// to hand out. They let the mount decision be made *before* the chunk is
/// placed, keep an AccessKit node's identity across a scroll instead of churning
/// the subtree, and let [`SelectionEnds`] be keyed by `Id`: a stale id from a
/// previous, longer layout simply matches nothing.
fn chunk_id(ui: &Ui, index: usize) -> Id {
    ui.id().with(("prepared-diff-chunk", index))
}

fn selection_key(ui: &Ui) -> Id {
    ui.id().with("prepared-diff-selection")
}

fn accessibility_key(ui: &Ui) -> Id {
    ui.unique_id().with("prepared-diff-accessibility")
}

/// The diff's single keyboard target, and the name of the region the chunks
/// land in.
///
/// Call this once per frame from *inside* the scroll viewport closure and above
/// the merge/plain branch. Three things depend on that placement:
///
/// - The AccessKit node for the enclosing `Ui` must exist before the first chunk
///   registers. `Context::accesskit_node_builder` parents a new node by walking
///   up to the first ancestor that *already has a node* (`context.rs:599-627`),
///   so a chunk that registers first attaches to the root window and this group
///   comes out empty.
/// - `Ui::scroll_with_delta` writes the per-pass `PassState::scroll_delta`
///   (`ui.rs:1490-1500`), which whichever `ScrollArea` ends first `mem::take`s
///   (`scroll_area.rs:1094-1102`). Outside the closure the delta would go to the
///   wrong scroll area, and `ui.clip_rect()` would be the card, not the
///   viewport, so the page size would be wrong too.
/// - Above the branch is what makes merge mode one tab stop as well.
///
/// It allocates no space: `Ui::interact` builds a `WidgetRect` and calls
/// `create_widget` without advancing the cursor (`ui.rs:906-933`), so
/// `show_viewport`'s `set_min_size` and merge mode's stacking are untouched.
pub fn diff_region(ui: &mut Ui) -> Response {
    ui.ctx().accesskit_node_builder(ui.unique_id(), |node| {
        node.set_role(egui::accesskit::Role::Group);
        node.set_label(RENDERED_DIFF_LABEL);
    });

    // `focusable_noninteractive` is `Sense::FOCUSABLE` (`sense.rs:52`), so
    // `interactive()` is false and this steals no click, drag or hover from the
    // chunks underneath it -- but `is_focusable()` is true, and that is the
    // crux: `create_widget` calls `check_for_id_clash` when the sense is
    // interactive *or* focusable (`context.rs:1276-1279`), and that is the only
    // writer of `PassState::used_ids` (`context.rs:1163`). `Memory::end_pass`
    // drops focus for any focused id missing from `used_ids`
    // (`memory/mod.rs:632-637`), so registering here unconditionally -- rather
    // than on a chunk, which culling unmounts -- is what makes keyboard focus
    // survive scrolling past the row it started on.
    let response = ui.interact(
        ui.clip_rect(),
        diff_region_id(),
        Sense::focusable_noninteractive(),
    );
    response
        .widget_info(|| WidgetInfo::labeled(WidgetType::Other, ui.is_enabled(), DIFF_SCROLL_LABEL));
    // `WidgetType::Other` is the right egui event metadata for this custom
    // focus target, but egui maps it to AccessKit's `Role::Unknown`. Keep the
    // metadata and explicitly publish the semantic role screen readers need.
    // `show_viewport` makes this the parent of the virtualized chunk nodes after
    // they have registered, while retaining the stable enclosing region above.
    ui.ctx().accesskit_node_builder(response.id, |node| {
        node.set_role(crate::ui::scroll_region_role());
        node.set_label(DIFF_SCROLL_LABEL);
        node.set_bounds(egui::accesskit::Rect {
            x0: f64::from(response.rect.left()),
            y0: f64::from(response.rect.top()),
            x1: f64::from(response.rect.right()),
            y1: f64::from(response.rect.bottom()),
        });
    });

    if response.has_focus() {
        // Without the lock a bare arrow key hands focus to a neighbouring
        // widget instead of scrolling. Left/Right stay unfiltered because
        // nothing here handles horizontal scrolling yet.
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                response.id,
                EventFilter {
                    vertical_arrows: true,
                    ..Default::default()
                },
            );
        });
        let page = ui.clip_rect().height().max(48.0) * 0.9;
        let delta = ui.input_mut(|input| {
            if input.consume_key(Modifiers::NONE, Key::PageDown) {
                -page
            } else if input.consume_key(Modifiers::NONE, Key::PageUp) {
                page
            } else if input.consume_key(Modifiers::NONE, Key::ArrowDown) {
                -32.0
            } else if input.consume_key(Modifiers::NONE, Key::ArrowUp) {
                32.0
            } else if input.consume_key(Modifiers::NONE, Key::End) {
                -1.0e9
            } else if input.consume_key(Modifiers::NONE, Key::Home) {
                1.0e9
            } else {
                0.0
            }
        });
        // No `is_rect_visible` guard: the keyboard target is the viewport, and
        // a viewport is by definition visible.
        if delta != 0.0 {
            ui.scroll_with_delta(Vec2::new(0.0, delta));
        }
    }
    response
}

/// A constant, deliberately not derived from `ui.id()`: the enclosing
/// `ScrollArea` is salted per shown panel, so a derived id would drop keyboard
/// focus every time the candidate changes.
pub fn diff_region_id() -> Id {
    Id::new("delgui-diff-keyboard")
}

/// One ring around the whole viewport, painted by the caller *after* the scroll
/// area's closure so it is not buried under the erase fills the chunks draw.
pub fn focus_ring(ui: &Ui, rect: Rect) {
    ui.painter().rect_stroke(
        rect,
        0.0,
        ui.visuals().widgets.active.bg_stroke,
        egui::StrokeKind::Inside,
    );
}

/// How wide the "there is more this way" fade at a scroll edge is.
const EDGE_FADE: f32 = 18.0;
/// How many bands it is drawn in. epaint has no gradient shape, and eight steps
/// is where the banding stops being visible at this width.
const EDGE_BANDS: u32 = 8;

/// Mark a horizontal scroll edge the diff runs past.
///
/// The scrollbars here float, and deliberately: a solid one takes width away
/// from the diff, which changes the column count delta laid out against the
/// moment content overflows and re-runs it for the new number. But floating
/// means invisible until the pointer goes looking, and what that leaves on
/// screen is a line of code sliced through the middle of a glyph at the card's
/// inner edge -- which reads as corruption, not as more text. A fade says
/// "continues" without costing a column.
pub fn scroll_edges(ui: &Ui, t: &Tokens, rect: Rect, offset: f32, content: f32) {
    let ground = t.surface_sunken;
    let painter = ui.painter().with_clip_rect(rect);
    let band = |x: f32, sign: f32| {
        for i in 0..EDGE_BANDS {
            let step = EDGE_FADE / EDGE_BANDS as f32;
            let near = x + sign * step * i as f32;
            // Opaque at the edge itself, clear by `EDGE_FADE` inwards.
            let alpha = 1.0 - (i as f32 / EDGE_BANDS as f32);
            painter.rect_filled(
                Rect::from_x_y_ranges(
                    if sign < 0.0 {
                        near - step..=near
                    } else {
                        near..=near + step
                    },
                    rect.y_range(),
                ),
                0.0,
                ground.gamma_multiply(alpha),
            );
        }
    };
    // A half-pixel of slack: `content_size` and the viewport agree exactly at
    // the ends, and a fade drawn over nothing is a smudge on a diff.
    if offset > 0.5 {
        band(rect.left(), 1.0);
    }
    if offset + rect.width() + 0.5 < content {
        band(rect.right(), -1.0);
    }
}

struct PreparedChunk {
    galley: Arc<Galley>,
    /// Where the chunk is placed, in pixels from the top of the layout.
    top: f32,
    /// Where the chunk starts, in diff rows. `top` positions it for the
    /// painter; this positions it for the find bar's line numbers.
    first_row: usize,
    hyperlinks: Vec<Hyperlink>,
    erase_fills: Vec<EraseFill>,
}

impl PreparedChunk {
    fn bottom(&self) -> f32 {
        self.top + self.galley.size().y
    }

    fn label(&self, columns: usize, column_width: f32, mount: Mount) -> PreparedLabel<'_> {
        PreparedLabel {
            chunk: self,
            columns,
            column_width: column_width.max(1.0),
            mount,
        }
    }

    /// Everything a chunk does once it has a `Response`, whether that came from
    /// laying it out ([`PreparedLabel`]) or from placing it by hand
    /// ([`PreparedLayout::show_viewport`]).
    ///
    /// The order of the five steps below is load-bearing; each says why.
    fn draw(
        &self,
        ui: &Ui,
        mut response: Response,
        rect: Rect,
        columns: usize,
        column_width: f32,
        frame: ChunkFrame,
    ) -> Response {
        let galley = self.galley.clone();
        let galley_pos = rect.left_top();
        let visible = ui.is_rect_visible(rect);

        if frame.publish_accessibility {
            // `WidgetInfo::labeled` owns its value, so this copies the chunk's
            // text. That is bounded to viewport-scale work after the first pass
            // by `show_viewport`; unchanged off-screen nodes remain in
            // AccessKit's incremental tree and are merely kept as children of
            // the diff region.
            response.widget_info(|| {
                WidgetInfo::labeled(WidgetType::Label, ui.is_enabled(), galley.text())
            });

            // egui *implements* `ScrollIntoView` for every registered widget
            // (`context.rs:1305-1323`) but only ever *advertises* `Focus` and
            // `Click` (`response.rs:907-923`), and that path runs only for
            // focusable widgets. Without this, the macOS adapter never offers
            // AXScrollToVisible and an off-screen row cannot be navigated to.
            ui.ctx().accesskit_node_builder(response.id, |node| {
                node.add_action(egui::accesskit::Action::ScrollIntoView);
            });
        }

        // The chunks are not tab stops any more, so clicking one is what puts
        // the keyboard on the diff. It has to target `diff_region_id()`, not
        // this response: `create_widget` calls `surrender_focus` for every
        // non-focusable widget (`context.rs:1271-1273`), so a chunk could not
        // hold focus even for a frame. `diff_region` registered that id earlier
        // in this same pass.
        if response.clicked() || response.drag_started() {
            ui.memory_mut(|memory| memory.request_focus(diff_region_id()));
        }

        if visible {
            for fill in &self.erase_fills {
                let Some(row) = galley.rows.get(fill.row) else {
                    continue;
                };
                let row_rect = row.rect().translate(galley_pos.to_vec2());
                let start =
                    (galley_pos.x + fill.from_column as f32 * column_width).max(row_rect.right());
                let end = galley_pos.x + columns as f32 * column_width;
                if start < end {
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(start, row_rect.top()),
                            egui::pos2(end, row_rect.bottom()),
                        ),
                        0.0,
                        fill.background,
                    );
                }
            }
        }

        // This is what paints the galley (`label_text_selection.rs:187`), so it
        // has to come after the erase fills or the background would be painted
        // over the text.
        if visible || frame.mount == Mount::Anchored {
            egui::text_selection::LabelSelectionState::label_text_selection(
                ui,
                &response,
                galley_pos,
                galley,
                ui.visuals().text_color(),
                Stroke::NONE,
            );
        }

        // Must stay last. `on_label` asks for `CursorIcon::Text` whenever the
        // response is hovered (`label_text_selection.rs:559-561`) and
        // `Context::set_cursor_icon` is a plain last-write-wins store
        // (`context.rs:1643-1645`) -- a child `Ui` does not buffer platform
        // output, so there is nothing to scope and ordering is the whole fix.
        // Mid-drag the hand still cannot win, because `on_end_pass` forces the
        // I-beam unconditionally after all widget code (`:218-220`); that is
        // correct.
        if visible {
            let hovered = ui
                .input(|i| i.pointer.hover_pos())
                .filter(|p| rect.contains(*p))
                .map(|p| self.galley.cursor_from_pos(p - galley_pos).index.0)
                .and_then(|index| {
                    self.hyperlinks
                        .iter()
                        .find(|link| link.char_range.contains(&index))
                });
            if let Some(link) = hovered {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                if response.clicked_with_open_in_background() {
                    ui.open_url(OpenUrl {
                        url: link.target.clone(),
                        new_tab: true,
                    });
                } else if response.clicked() {
                    ui.open_url(OpenUrl {
                        url: link.target.clone(),
                        new_tab: false,
                    });
                }
                if ui.style().url_in_tooltip {
                    response = response.on_hover_text(link.target.clone());
                }
            }
        }
        response
    }
}

/// A diff block laid out once and cheap to reuse on every frame.
///
/// Cache this alongside the render/style key and draw it with [`Self::show`] or
/// [`Self::show_viewport`]. The chunks keep egui's cross-label selectable-text
/// behavior, make OSC 8 spans clickable, and paint `ESC[K` without adding
/// synthetic spaces to the copied text.
///
/// It deliberately keeps no `String` of its own. The characters are already
/// retained once, inside each chunk's `Galley` (`Galley::text()` is
/// `&self.job.text`, epaint `text_layout_types.rs:1010-1013`), so a second copy
/// would double the memory of the largest diffs for nothing. The questions the
/// app used to answer by scanning that string are answered here instead.
pub struct PreparedLayout {
    chunks: Vec<PreparedChunk>,
    columns: usize,
    height: f32,
    galley_width: f32,
    accessibility_revision: u64,
}

impl PreparedLayout {
    /// Draw every chunk. Merge mode uses this because control rows interrupt the
    /// diff and make its vertical positions depend on the surrounding widgets.
    pub fn show(&self, ui: &mut Ui, column_width: f32) -> Response {
        // The `Id`-keyed [`SelectionEnds`] tracker cannot be used here: these
        // chunks take auto-ids, which are not knowable before they are placed.
        // The cliff is milder -- every chunk is placed regardless, so the
        // context-global flag only costs an off-screen `TextShape` each, and
        // merge mode diffs at zero context, so a hunk's layout is a handful of
        // rows. It is the same shape of problem as the one `show_viewport`
        // fixes, left live rather than papered over.
        let mount = if label_selection_active(ui.ctx()) {
            Mount::Anchored
        } else {
            Mount::Culled
        };
        ui.scope_builder(
            UiBuilder::new().accessibility_parent(diff_region_id()),
            |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let mut response: Option<Response> = None;
                for chunk in &self.chunks {
                    let chunk_response = ui.add(chunk.label(self.columns, column_width, mount));
                    response = Some(match response {
                        Some(current) => current.union(chunk_response),
                        None => chunk_response,
                    });
                }
                response.unwrap_or_else(|| ui.allocate_response(Vec2::ZERO, Sense::hover()))
            },
        )
        .inner
    }

    /// Keep every chunk reachable; draw and update the ones the viewport needs.
    ///
    /// Culling decides what is *painted*, never what is *reachable*. Every chunk
    /// gets a `Response` on every frame -- an off-screen one with
    /// `Sense::hover()`, which is `Sense::empty()`, so `create_widget` skips
    /// `check_for_id_clash`, skips `interested_in_focus` and keeps the rect out
    /// of hit-testing while still consuming AccessKit action requests, which is
    /// unconditional (`context.rs:1300-1338`).
    /// `Ui::interact` clips only `interact_rect`, leaving `rect` whole
    /// (`ui.rs:926`), and `rect` is what `Action::ScrollIntoView` scrolls to --
    /// so an off-screen chunk carries its true bounds and scrolls itself
    /// correctly into view.
    ///
    /// AccessKit's `TreeUpdate` is incremental: unchanged nodes should be
    /// omitted, but their IDs must remain in their parent's child list. The
    /// first pass for a layout therefore seeds all chunk values once. Later
    /// passes publish only drawn chunks and chunks drawn on the previous pass;
    /// the latter clears text-run children that would otherwise linger after a
    /// row scrolls away. The region keeps every chunk ID in document order.
    pub fn show_viewport(&self, ui: &mut Ui, viewport: Rect, column_width: f32) {
        let origin = ui.cursor().left_top();
        let width = self
            .galley_width
            .max(self.columns as f32 * column_width.max(1.0));
        ui.set_min_size(Vec2::new(width, self.height));

        let key = selection_key(ui);
        let selecting = label_selection_active(ui.ctx());
        let mut ends = if selecting {
            ui.data(|data| data.get_temp::<SelectionEnds>(key))
                .unwrap_or_default()
        } else {
            // Nothing to keep alive, and a stale end would pin a chunk forever.
            SelectionEnds::default()
        };
        // Replicates the private `got_copy_event`
        // (`label_text_selection.rs:679-686`), gated on a selection existing so
        // an unrelated Cmd-C does not force a full-mount frame on a 100k-line
        // diff. `copy_text` accumulates only from galleys that actually ran
        // `on_label` (`:288`, `:575-577`), so a culled middle would truncate the
        // clipboard silently: the copy frame has to mount everything.
        let copying = selecting
            && ui.input(|i| {
                i.events
                    .iter()
                    .any(|e| matches!(e, Event::Copy | Event::Cut))
            });
        let plan = self.mount_plan(viewport, ends, copying);

        let accessibility_key = accessibility_key(ui);
        let frame = ui.ctx().cumulative_frame_nr();
        let pass = ui.ctx().cumulative_pass_nr();
        let previous_accessibility = ui
            .data(|data| data.get_temp::<AccessibilityMount>(accessibility_key))
            .unwrap_or_default();
        // The closure runs only while AccessKit is active. A gap means its
        // adapter may have discarded this subtree, so seed again even when the
        // retained layout itself did not change.
        let accesskit_active = ui
            .ctx()
            .accesskit_node_builder(ui.unique_id(), |_| {})
            .is_some();
        let seed_accessibility = accesskit_active
            && (previous_accessibility.revision != self.accessibility_revision
                || previous_accessibility.pass.checked_add(1) != Some(pass)
                // `run_ui` discards every AccessKit update except its final
                // multipass output. If this generation was first seeded in an
                // earlier pass of the same frame, seed the final pass too.
                || previous_accessibility.frame == frame);

        // Touch screens have no hover, so a drag there has to stay available
        // for scrolling. `FOCUSABLE` comes off for the same reason egui's own
        // `Label` strips it (`widgets/label.rs:166`): one chunk per 192 rows
        // must not be one tab stop each. The diff's single tab stop is
        // [`diff_region`].
        let selection_sense = if ui.input(|i| i.has_touch_screen()) {
            Sense::click()
        } else {
            Sense::click_and_drag()
        } - Sense::FOCUSABLE;

        let mut hit: Option<Id> = None;
        let mut drawn_chunks = Vec::new();
        for index in 0..self.chunks.len() {
            let chunk = &self.chunks[index];
            let rect = Rect::from_min_size(
                origin + Vec2::new(0.0, chunk.top),
                Vec2::new(width, chunk.galley.size().y),
            );
            let id = chunk_id(ui, index);
            let drawn = plan.drawn(index, id);
            if drawn {
                drawn_chunks.push(index);
            }
            let sense = if drawn {
                selection_sense
            } else {
                Sense::hover()
            };
            let response = ui.interact(rect, id, sense);
            if drawn && response.contains_pointer() {
                hit = Some(id);
            }
            let was_drawn = previous_accessibility.drawn.binary_search(&index).is_ok();
            chunk.draw(
                ui,
                response,
                rect,
                self.columns,
                column_width,
                ChunkFrame {
                    mount: plan.mount(id),
                    publish_accessibility: seed_accessibility || drawn || was_drawn,
                },
            );
        }

        if accesskit_active {
            // Chunk builders created above initially attach to the enclosing Ui.
            // Re-parent them under the semantic scroll view, then leave that
            // scroll view as the region's only child. Explicit child lists also
            // retain unchanged off-screen nodes that were deliberately absent
            // from this incremental update.
            let chunks = (0..self.chunks.len())
                .map(|index| chunk_id(ui, index).accesskit_id())
                .collect::<Vec<_>>();
            ui.ctx()
                .accesskit_node_builder(diff_region_id(), |node| node.set_children(chunks));
            ui.ctx().accesskit_node_builder(ui.unique_id(), |node| {
                node.set_children(vec![diff_region_id().accesskit_id()]);
            });
            ui.data_mut(|data| {
                data.insert_temp(
                    accessibility_key,
                    AccessibilityMount {
                        revision: self.accessibility_revision,
                        frame,
                        pass,
                        drawn: drawn_chunks,
                    },
                );
            });
        }

        let (pressed, down, shift) = ui.input(|i| {
            (
                i.pointer.any_pressed(),
                i.pointer.any_down(),
                i.modifiers.shift,
            )
        });
        if pressed {
            match hit {
                // A plain press collapses the selection onto this chunk; a
                // shift-press moves only the focus end.
                Some(id) if shift => ends.focus = Some(id),
                Some(id) => {
                    ends = SelectionEnds {
                        anchor: Some(id),
                        focus: Some(id),
                    }
                }
                // Pressing anywhere but the diff gives its ends up. Consistent
                // with egui, which clears the selection outright on
                // `any_pressed && !any_hovered` (`:262-268`) -- and this is what
                // stops a click on some other label pinning the whole diff.
                None if !shift => ends = SelectionEnds::default(),
                None => {}
            }
        } else if down && let Some(id) = hit {
            // A drag that started elsewhere and entered the diff, mirroring
            // `cursor_for`'s own `contains_pointer` branch (`:373-376`).
            ends.focus = Some(id);
        }
        ui.data_mut(|data| data.insert_temp(key, ends));
    }

    /// True when there is nothing to copy or search.
    ///
    /// Exact rather than a row count: only a layout whose every chunk laid out
    /// to the empty string has no text, since a non-final chunk always holds
    /// [`PREPARED_CHUNK_ROWS`] - 1 row breaks.
    pub fn is_empty(&self) -> bool {
        self.chunks
            .iter()
            .all(|chunk| chunk.galley.text().is_empty())
    }

    /// The whole diff as one string. Built on demand -- a *Copy diff* click --
    /// never per frame.
    pub fn to_text(&self) -> String {
        let mut out =
            String::with_capacity(self.chunks.iter().map(|c| c.galley.text().len() + 1).sum());
        for (index, chunk) in self.chunks.iter().enumerate() {
            // Keyed on the index, not on `!out.is_empty()`. The accumulator this
            // replaces was only accidentally right: a first chunk that laid out
            // to the empty string would have swallowed the seam break after it.
            if index > 0 {
                out.push('\n');
            }
            out.push_str(chunk.galley.text());
        }
        out
    }

    /// The rendered line of every match, in document order.
    ///
    /// Scanned chunk by chunk rather than over [`Self::to_text`], because the
    /// point of dropping the retained string is not to rebuild it. That is exact
    /// for any query without a newline in it: `prepare_chunk` appends `"\n"`
    /// between rows and at neither end, so the seam between two chunks is one
    /// row break exactly like every break inside one, and
    /// `first_row(k+1) == first_row(k) + rows(k)` accounts for it. The find bar
    /// has exactly one writer, a `TextEdit::singleline`, and egui 0.36.1 cannot
    /// put a newline into one: paste is `replace(['\r','\n'], " ")`
    /// (`text_edit/builder.rs:1121`), a typed bare `"\n"`/`"\r"` is skipped
    /// (`:1129`), and Enter goes to focus handling.
    pub fn matching_rows(&self, query: &str) -> Vec<usize> {
        let mut rows = Vec::new();
        for chunk in &self.chunks {
            rows.extend(
                line_offsets(chunk.galley.text(), query)
                    .into_iter()
                    .map(|row| row + chunk.first_row),
            );
        }
        rows
    }

    fn mount_plan(&self, viewport: Rect, ends: SelectionEnds, copying: bool) -> MountPlan {
        MountPlan {
            visible: self.visible_chunk_range(viewport),
            ends,
            copying,
        }
    }

    fn visible_chunk_range(&self, viewport: Rect) -> Range<usize> {
        let first_intersecting = self
            .chunks
            .partition_point(|chunk| chunk.bottom() < viewport.top());
        let first = first_intersecting.saturating_sub(VIEWPORT_OVERSCAN_CHUNKS);
        let after_last = self
            .chunks
            .partition_point(|chunk| chunk.top <= viewport.bottom());
        let end = after_last
            .saturating_add(VIEWPORT_OVERSCAN_CHUNKS)
            .min(self.chunks.len());
        first..end.max(first)
    }

    #[cfg(test)]
    fn chunk_count(&self) -> usize {
        self.chunks.len()
    }
}

/// The line each match starts on, counting from the start of `text`.
///
/// Lives here rather than beside the find bar because the invariant it depends
/// on -- one `'\n'` per rendered row, at neither end -- belongs beside the
/// chunking that creates it.
fn line_offsets(text: &str, query: &str) -> Vec<usize> {
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

/// Build and shape a reusable diff block.
///
/// This must run on the UI thread because font shaping belongs to egui's
/// [`Context`]. Rebuild when the rendered rows, column count, font, palette,
/// line height or pixels-per-point change; otherwise retain the returned value.
pub fn prepare_layout(
    ctx: &Context,
    rows: &[Line],
    columns: usize,
    font: FontId,
    palette: &Palette,
    line_height: f32,
) -> PreparedLayout {
    let mut chunks = Vec::with_capacity(rows.len().div_ceil(PREPARED_CHUNK_ROWS));
    let mut height = 0.0;
    let mut first_row = 0usize;
    let mut galley_width: f32 = 0.0;

    for chunk_rows in rows.chunks(PREPARED_CHUNK_ROWS) {
        let mut chunk = prepare_chunk(ctx, chunk_rows, columns, font.clone(), palette, line_height);
        chunk.top = height;
        // Counted from the rows actually laid out rather than
        // `index * PREPARED_CHUNK_ROWS`, so it stays true at any chunk size and
        // for the short final chunk.
        chunk.first_row = first_row;
        first_row += chunk_rows.len();
        height += chunk.galley.size().y;
        galley_width = galley_width.max(chunk.galley.size().x);
        chunks.push(chunk);
    }

    PreparedLayout {
        chunks,
        columns,
        height,
        galley_width,
        accessibility_revision: NEXT_ACCESSIBILITY_REVISION.fetch_add(1, Ordering::Relaxed),
    }
}

fn prepare_chunk(
    ctx: &Context,
    rows: &[Line],
    columns: usize,
    font: FontId,
    palette: &Palette,
    line_height: f32,
) -> PreparedChunk {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    job.keep_trailing_whitespace = true;
    job.first_row_min_height = line_height;
    let plain = TextFormat {
        font_id: font.clone(),
        line_height: Some(line_height),
        ..Default::default()
    };
    let mut hyperlinks: Vec<Hyperlink> = Vec::new();
    let mut erase_fills = Vec::new();
    let mut char_offset = 0usize;

    for (row, line) in rows.iter().enumerate() {
        if row > 0 {
            job.append("\n", 0.0, plain.clone());
            char_offset += 1;
        }
        let mut used = 0usize;
        for span in &line.spans {
            let start = char_offset;
            used += UnicodeWidthStr::width(span.text.as_str());
            char_offset += span.text.chars().count();
            job.append(
                &span.text,
                0.0,
                format_for(&span.style, font.clone(), palette, line_height),
            );
            if let Some(target) = &span.link
                && start < char_offset
            {
                if let Some(last) = hyperlinks.last_mut()
                    && last.target == *target
                    && last.char_range.end == start
                {
                    last.char_range.end = char_offset;
                } else {
                    hyperlinks.push(Hyperlink {
                        char_range: start..char_offset,
                        target: target.clone(),
                    });
                }
            }
        }
        if let Some(bg) = line.fill_to_eol
            && columns > used
        {
            erase_fills.push(EraseFill {
                row,
                from_column: used,
                background: palette.resolve(bg),
            });
        }
    }

    PreparedChunk {
        galley: ctx.fonts_mut(|fonts| fonts.layout_job(job)),
        top: 0.0,
        first_row: 0,
        hyperlinks,
        erase_fills,
    }
}

/// The selectable widget backed by a cached [`PreparedLayout`].
pub struct PreparedLabel<'a> {
    chunk: &'a PreparedChunk,
    columns: usize,
    column_width: f32,
    mount: Mount,
}

impl Widget for PreparedLabel<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let chunk = self.chunk;
        let size = Vec2::new(
            chunk
                .galley
                .size()
                .x
                .max(self.columns as f32 * self.column_width),
            chunk.galley.size().y,
        );
        let mut sense = Sense::hover();
        let selection = if ui.input(|i| i.has_touch_screen()) {
            Sense::click()
        } else {
            Sense::click_and_drag()
        };
        sense |= selection;
        // See `show_viewport`: the diff is one tab stop, not one per chunk.
        sense -= Sense::FOCUSABLE;
        let (rect, response) = ui.allocate_exact_size(size, sense);
        chunk.draw(
            ui,
            response,
            rect,
            self.columns,
            self.column_width,
            ChunkFrame {
                mount: self.mount,
                publish_accessibility: true,
            },
        )
    }
}

/// True when *any* label selection exists, in any viewport -- including the
/// collapsed one a single click on any ordinary label leaves behind
/// (`label_text_selection.rs:284-286`, `:590-628`).
///
/// That makes it useless as a culling decision on its own, which is what
/// [`SelectionEnds`] exists to replace. It is still the right gate for the two
/// blanket cases: whether a copy event can concern the diff at all, and merge
/// mode, whose auto-id chunks cannot be tracked individually.
fn label_selection_active(ctx: &Context) -> bool {
    ctx.plugin::<egui::text_selection::LabelSelectionState>()
        .lock()
        .has_selection()
}

/// Build the former one-block layout used by regression tests.
///
/// Production rendering now uses bounded chunks: egui stitches selection across
/// adjacent labels, while the scroll view can omit chunks outside its viewport.
///
/// The rows are used exactly as given: trimming belongs to `ansi::body`, which
/// has to be the only place that decides where the body starts, or the row
/// indices the hunks were located against silently shift.
///
/// `columns` must be the width delta itself laid out against, not the window's
/// current width: the padding below has to land where delta thought the right
/// edge was, and during a re-render those two disagree.
#[cfg(test)]
pub fn to_layout_job(
    rows: &[Line],
    columns: usize,
    font: FontId,
    palette: &Palette,
    line_height: f32,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let plain = TextFormat {
        font_id: font.clone(),
        // A blank line takes its height from the format of whatever *opened*
        // the paragraph -- which is this newline. Leave it unset and every empty
        // row in the diff is shorter than the rows around it.
        line_height: Some(line_height),
        ..Default::default()
    };

    for (idx, line) in rows.iter().enumerate() {
        if idx > 0 {
            job.append("\n", 0.0, plain.clone());
        }
        let mut used = 0usize;
        for span in &line.spans {
            used += UnicodeWidthStr::width(span.text.as_str());
            job.append(
                &span.text,
                0.0,
                format_for(&span.style, font.clone(), palette, line_height),
            );
        }
        // `ESC[K` asks the terminal to paint the current background out to the
        // right edge. delta already knows the column count -- we passed it via
        // --width -- so padding with spaces in that background reproduces it
        // exactly, and keeps the whole diff inside one selectable text block.
        if let Some(bg) = line.fill_to_eol
            && columns > used
        {
            let pad = " ".repeat(columns - used);
            let style = Style {
                bg: Some(bg),
                ..Default::default()
            };
            job.append(
                &pad,
                0.0,
                format_for(&style, font.clone(), palette, line_height),
            );
        }
    }
    job
}

fn format_for(style: &Style, font: FontId, palette: &Palette, line_height: f32) -> TextFormat {
    let (mut fg, mut bg) = (
        style
            .fg
            .map(|c| palette.resolve(c))
            .unwrap_or(palette.foreground),
        style
            .bg
            .map(|c| palette.resolve(c))
            .unwrap_or(Color32::TRANSPARENT),
    );
    if style.reverse {
        let bg_solid = if bg == Color32::TRANSPARENT {
            palette.background
        } else {
            bg
        };
        (fg, bg) = (bg_solid, fg);
    }
    if style.dim {
        fg = fg.gamma_multiply(0.6);
    }
    TextFormat {
        font_id: font,
        color: fg,
        background: bg,
        italics: style.italic,
        underline: if style.underline {
            egui::Stroke::new(1.0, fg)
        } else {
            egui::Stroke::NONE
        },
        strikethrough: if style.strike {
            egui::Stroke::new(1.0, fg)
        } else {
            egui::Stroke::NONE
        },
        line_height: Some(line_height),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use delgui_core::ansi::Span;
    use egui::{Modifiers, PointerButton, RawInput, accesskit, pos2};

    fn palette() -> Palette {
        Palette {
            ansi16: [Color32::BLACK; 16],
            foreground: Color32::WHITE,
            background: Color32::BLACK,
        }
    }

    fn span(text: &str, link: Option<&str>) -> Span {
        Span {
            text: text.into(),
            style: Style::default(),
            link: link.map(str::to_owned),
        }
    }

    fn numbered_rows(count: usize) -> Vec<Line> {
        (0..count)
            .map(|row| Line {
                spans: vec![span(&format!("row {row}"), None)],
                fill_to_eol: None,
            })
            .collect()
    }

    fn layout(ctx: &Context, rows: &[Line]) -> PreparedLayout {
        prepare_layout(ctx, rows, 16, FontId::monospace(12.0), &palette(), 16.0)
    }

    /// A window small enough that only the first chunk of a many-chunk layout
    /// intersects the clip rect, so "registered" and "painted" are separable.
    fn small_screen() -> RawInput {
        RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(800.0, 400.0))),
            ..Default::default()
        }
    }

    fn key_press(key: Key) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    fn press(pos: egui::Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    fn with_events(events: Vec<Event>) -> RawInput {
        RawInput {
            events,
            ..small_screen()
        }
    }

    #[test]
    fn prepared_layout_keeps_erase_padding_out_of_selectable_text() {
        egui::__run_test_ui(|ui| {
            let rows = [Line {
                spans: vec![span("abc", None)],
                fill_to_eol: Some(Color::Indexed(1)),
            }];
            let prepared = prepare_layout(
                ui.ctx(),
                &rows,
                8,
                FontId::monospace(12.0),
                &palette(),
                16.0,
            );
            assert_eq!(prepared.to_text(), "abc");
            assert_eq!(
                prepared.chunks[0].erase_fills,
                &[EraseFill {
                    row: 0,
                    from_column: 3,
                    background: Color32::BLACK,
                }]
            );
            let response = prepared.show(ui, 7.0);
            assert!(response.rect.width() >= 56.0);
        });
    }

    #[test]
    fn prepared_layout_carries_osc8_links_as_character_ranges() {
        egui::__run_test_ui(|ui| {
            let rows = [Line {
                spans: vec![
                    span("α", None),
                    span("β", Some("https://example.test/a")),
                    span("γ", Some("https://example.test/a")),
                    span("!", None),
                ],
                fill_to_eol: None,
            }];
            let prepared = prepare_layout(
                ui.ctx(),
                &rows,
                8,
                FontId::monospace(12.0),
                &palette(),
                16.0,
            );
            assert_eq!(prepared.to_text(), "αβγ!");
            assert_eq!(
                prepared.chunks[0].hyperlinks,
                &[Hyperlink {
                    char_range: 1..3,
                    target: "https://example.test/a".into(),
                }]
            );
        });
    }

    #[test]
    fn prepared_layout_preserves_text_across_chunk_boundaries() {
        egui::__run_test_ui(|ui| {
            let rows = numbered_rows(PREPARED_CHUNK_ROWS + 1);
            let prepared = layout(ui.ctx(), &rows);
            let expected = (0..rows.len())
                .map(|row| format!("row {row}"))
                .collect::<Vec<_>>()
                .join("\n");

            assert_eq!(prepared.chunk_count(), 2);
            assert_eq!(prepared.to_text(), expected);
        });
    }

    #[test]
    fn viewport_work_is_bounded_to_nearby_chunks() {
        egui::__run_test_ui(|ui| {
            let rows = numbered_rows(PREPARED_CHUNK_ROWS * 8);
            let prepared = layout(ui.ctx(), &rows);
            let top = prepared.chunks[3].top + 8.0;
            let viewport = Rect::from_min_size(pos2(0.0, top), Vec2::new(800.0, 40.0));

            assert_eq!(prepared.chunk_count(), 8);
            assert_eq!(prepared.visible_chunk_range(viewport), 2..5);
        });
    }

    #[test]
    fn an_empty_layout_reports_itself_empty() {
        egui::__run_test_ui(|ui| {
            let nothing = layout(ui.ctx(), &[]);
            assert!(nothing.is_empty());
            assert_eq!(nothing.to_text(), "");

            let blank = layout(
                ui.ctx(),
                &[Line {
                    spans: Vec::new(),
                    fill_to_eol: None,
                }],
            );
            assert_eq!(blank.is_empty(), blank.to_text().is_empty());
            assert!(blank.is_empty());

            let many = layout(ui.ctx(), &numbered_rows(PREPARED_CHUNK_ROWS * 2 + 3));
            assert_eq!(many.is_empty(), many.to_text().is_empty());
            assert!(!many.is_empty());
        });
    }

    #[test]
    fn every_chunk_reports_the_diff_row_it_starts_at() {
        egui::__run_test_ui(|ui| {
            // Deliberately not a multiple, so the last chunk is short and the
            // arithmetic cannot be `index * PREPARED_CHUNK_ROWS` by luck.
            let rows = numbered_rows(PREPARED_CHUNK_ROWS * 2 + 7);
            let prepared = layout(ui.ctx(), &rows);
            let text = prepared.to_text();
            let lines = text.lines().collect::<Vec<_>>();

            assert_eq!(prepared.chunk_count(), 3);
            let mut counted = 0;
            for chunk in &prepared.chunks {
                let first = chunk.galley.text().lines().next().unwrap();
                assert_eq!(lines[chunk.first_row], first);
                counted += chunk.galley.text().lines().count();
            }
            assert_eq!(counted, rows.len());
        });
    }

    #[test]
    fn find_reports_the_rendered_line_of_each_match() {
        assert_eq!(
            line_offsets("zero\nneedle\ntwo needle\n", "needle"),
            vec![1, 2]
        );
        assert!(line_offsets("needle", "").is_empty());
    }

    #[test]
    fn find_reports_the_same_rows_across_a_chunk_boundary() {
        egui::__run_test_ui(|ui| {
            let n = PREPARED_CHUNK_ROWS;
            let wanted = [0, n - 1, n, n + 3];
            let rows = (0..n + 5)
                .map(|row| Line {
                    spans: vec![span(
                        if wanted.contains(&row) {
                            "needle"
                        } else {
                            "plain"
                        },
                        None,
                    )],
                    fill_to_eol: None,
                })
                .collect::<Vec<_>>();
            let prepared = layout(ui.ctx(), &rows);

            assert_eq!(prepared.chunk_count(), 2);
            assert_eq!(prepared.matching_rows("needle"), wanted.to_vec());
            // A dropped or doubled seam break would move every index past the
            // seam by one, and this is what would catch it.
            assert_eq!(
                prepared.matching_rows("needle"),
                line_offsets(&prepared.to_text(), "needle")
            );
        });
    }

    #[test]
    fn a_selection_outside_the_diff_does_not_stop_culling() {
        egui::__run_test_ui(|ui| {
            let prepared = layout(ui.ctx(), &numbered_rows(PREPARED_CHUNK_ROWS * 8));
            let ids = (0..8).map(|i| chunk_id(ui, i)).collect::<Vec<_>>();
            let top = prepared.chunks[3].top + 8.0;
            let viewport = Rect::from_min_size(pos2(0.0, top), Vec2::new(800.0, 40.0));
            let plan = prepared.mount_plan(viewport, SelectionEnds::default(), false);

            let drawn = (0..8)
                .filter(|&i| plan.drawn(i, ids[i]))
                .collect::<Vec<_>>();
            assert_eq!(drawn, vec![2, 3, 4]);
        });
    }

    #[test]
    fn an_offscreen_selection_end_stays_mounted() {
        egui::__run_test_ui(|ui| {
            let prepared = layout(ui.ctx(), &numbered_rows(PREPARED_CHUNK_ROWS * 8));
            let ids = (0..8).map(|i| chunk_id(ui, i)).collect::<Vec<_>>();
            let top = prepared.chunks[3].top + 8.0;
            let viewport = Rect::from_min_size(pos2(0.0, top), Vec2::new(800.0, 40.0));

            let straddling = SelectionEnds {
                anchor: Some(ids[0]),
                focus: Some(ids[7]),
            };
            let plan = prepared.mount_plan(viewport, straddling, false);
            assert_eq!(
                (0..8)
                    .filter(|&i| plan.drawn(i, ids[i]))
                    .collect::<Vec<_>>(),
                vec![0, 2, 3, 4, 7],
                "both ends, and nothing between them"
            );

            // Both ends above the viewport: the nearer one is not enough, egui
            // needs to re-encounter each of them.
            let above = SelectionEnds {
                anchor: Some(ids[0]),
                focus: Some(ids[1]),
            };
            let top = prepared.chunks[5].top + 8.0;
            let viewport = Rect::from_min_size(pos2(0.0, top), Vec2::new(800.0, 40.0));
            let plan = prepared.mount_plan(viewport, above, false);
            assert_eq!(
                (0..8)
                    .filter(|&i| plan.drawn(i, ids[i]))
                    .collect::<Vec<_>>(),
                vec![0, 1, 4, 5, 6]
            );
        });
    }

    #[test]
    fn copying_mounts_the_whole_selection() {
        egui::__run_test_ui(|ui| {
            let prepared = layout(ui.ctx(), &numbered_rows(PREPARED_CHUNK_ROWS * 8));
            let ids = (0..8).map(|i| chunk_id(ui, i)).collect::<Vec<_>>();
            let top = prepared.chunks[3].top + 8.0;
            let viewport = Rect::from_min_size(pos2(0.0, top), Vec2::new(800.0, 40.0));
            let plan = prepared.mount_plan(viewport, SelectionEnds::default(), true);

            assert!(
                (0..8).all(|i| plan.drawn(i, ids[i])),
                "a culled middle would truncate the clipboard silently"
            );
        });
    }

    #[test]
    fn the_diff_is_one_tab_stop_however_many_chunks_it_has() {
        let ctx = Context::default();
        ctx.enable_accesskit();
        let rows = numbered_rows(PREPARED_CHUNK_ROWS * 8);
        let mut sentinel = None;
        let pass = |input: RawInput, sentinel: &mut Option<Id>| {
            let mut output = ctx.run_ui(input, |ui| {
                let prepared = layout(ui.ctx(), &rows);
                diff_region(ui);
                let viewport = Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(800.0, 400.0));
                prepared.show_viewport(ui, viewport, 7.0);
                *sentinel = Some(ui.button("after").id);
            });
            output.textures_delta.clear();
            output.platform_output.accesskit_update
        };

        let initial = pass(small_screen(), &mut sentinel).expect("AccessKit tree update");
        let scroll_target = initial
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(DIFF_SCROLL_LABEL))
            .expect("named diff scroll target");
        assert_eq!(scroll_target.1.role(), crate::ui::scroll_region_role());

        let _ = pass(with_events(vec![key_press(Key::Tab)]), &mut sentinel);
        assert_eq!(ctx.memory(|m| m.focused()), Some(diff_region_id()));
        // The sentinel is what pins *exactly one* tab stop: were the chunks
        // still focusable, this Tab would land on chunk 0 instead.
        let _ = pass(with_events(vec![key_press(Key::Tab)]), &mut sentinel);
        assert_eq!(ctx.memory(|m| m.focused()), sentinel);
    }

    #[test]
    fn the_diff_is_one_tab_stop_in_merge_mode_too() {
        let ctx = Context::default();
        ctx.enable_accesskit();
        let hunks = (0..4)
            .map(|hunk| numbered_rows(6 + hunk))
            .collect::<Vec<_>>();
        let mut sentinel = None;
        let pass = |input: RawInput, sentinel: &mut Option<Id>| {
            let mut output = ctx.run_ui(input, |ui| {
                diff_region(ui);
                for rows in &hunks {
                    // Stands in for the hunk control row merge mode draws in
                    // place of delta's header.
                    ui.label("take");
                    layout(ui.ctx(), rows).show(ui, 7.0);
                }
                *sentinel = Some(ui.button("after").id);
            });
            output.textures_delta.clear();
            output.platform_output.accesskit_update
        };

        let initial = pass(small_screen(), &mut sentinel).expect("AccessKit tree update");
        let scroll = initial
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(DIFF_SCROLL_LABEL))
            .expect("the merge diff scroll view");
        let owned = scroll
            .1
            .children()
            .iter()
            .filter_map(|id| initial.nodes.iter().find(|(node_id, _)| node_id == id))
            .flat_map(|(_, node)| node.children())
            .copied()
            .collect::<Vec<_>>();
        let row_chunks = initial
            .nodes
            .iter()
            .filter(|(_, node)| {
                node.role() == accesskit::Role::Label
                    && node.value().is_some_and(|value| value.starts_with("row "))
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        assert_eq!(row_chunks.len(), hunks.len());
        assert!(
            row_chunks.iter().all(|id| owned.contains(id)),
            "merge-mode chunk labels are outside the diff scroll view",
        );

        let _ = pass(with_events(vec![key_press(Key::Tab)]), &mut sentinel);
        assert_eq!(ctx.memory(|m| m.focused()), Some(diff_region_id()));
        let _ = pass(with_events(vec![key_press(Key::Tab)]), &mut sentinel);
        assert_eq!(ctx.memory(|m| m.focused()), sentinel);
    }

    #[test]
    fn keyboard_focus_outlives_the_chunk_it_scrolled_past() {
        let ctx = Context::default();
        let rows = numbered_rows(PREPARED_CHUNK_ROWS * 8);
        ctx.memory_mut(|m| m.request_focus(diff_region_id()));

        // Two passes: `recently_gained_focus` shields the first from the
        // dead-man's-switch (memory/mod.rs:632-634), so only the second proves
        // the id was re-registered rather than merely not yet reaped.
        for _ in 0..2 {
            let mut output = ctx.run_ui(small_screen(), |ui| {
                let prepared = layout(ui.ctx(), &rows);
                diff_region(ui);
                // Parked at the bottom, so chunk 0 -- and every chunk but the
                // last -- is culled.
                let top = prepared.chunks[7].top;
                prepared.show_viewport(
                    ui,
                    Rect::from_min_size(pos2(0.0, top), Vec2::new(800.0, 40.0)),
                    7.0,
                );
            });
            output.textures_delta.clear();
            assert_eq!(ctx.memory(|m| m.focused()), Some(diff_region_id()));
        }
    }

    #[test]
    fn page_keys_scroll_the_diff_while_the_focused_row_is_off_screen() {
        let ctx = Context::default();
        ctx.all_styles_mut(|style| style.scroll_animation = egui::style::ScrollAnimation::none());
        let rows = numbered_rows(PREPARED_CHUNK_ROWS * 8);
        ctx.memory_mut(|m| m.request_focus(diff_region_id()));
        let mut offset = 0.0;

        // Three passes: one to lay the diff out, one to press the key, and one
        // for the scroll to land. `ScrollArea` is animated by default and a
        // zero-length `ScrollAnimation` still finishes on the *following*
        // frame, not the one that asked (`scroll_area.rs:1141-1161`), so the
        // offset a two-pass test reads is always the one from before the key.
        for pass in 0..3 {
            let input = if pass == 1 {
                with_events(vec![key_press(Key::PageDown)])
            } else {
                small_screen()
            };
            let mut output = ctx.run_ui(input, |ui| {
                let prepared = layout(ui.ctx(), &rows);
                let out = egui::ScrollArea::vertical().show_viewport(ui, |ui, viewport| {
                    diff_region(ui);
                    prepared.show_viewport(ui, viewport, 7.0);
                });
                offset = out.state.offset.y;
            });
            output.textures_delta.clear();
        }
        assert!(offset > 0.0, "PageDown scrolled the enclosing ScrollArea");
    }

    #[test]
    fn clicking_the_diff_focuses_the_scroll_region() {
        let ctx = Context::default();
        let rows = numbered_rows(PREPARED_CHUNK_ROWS * 2);
        let at = pos2(40.0, 40.0);
        let passes = [
            with_events(vec![Event::PointerMoved(at)]),
            with_events(vec![Event::PointerMoved(at), press(at, true)]),
            with_events(vec![press(at, false)]),
        ];
        for input in passes {
            let mut output = ctx.run_ui(input, |ui| {
                let prepared = layout(ui.ctx(), &rows);
                diff_region(ui);
                prepared.show_viewport(
                    ui,
                    Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(800.0, 400.0)),
                    7.0,
                );
            });
            output.textures_delta.clear();
        }
        assert_eq!(ctx.memory(|m| m.focused()), Some(diff_region_id()));
    }

    #[test]
    fn pressing_outside_the_diff_releases_its_selection_ends() {
        let ctx = Context::default();
        // Short on purpose. A chunk is up to 192 rows tall, so a full-size one
        // reaches past the bottom of any test window and there is no "outside"
        // left to press: the label below would be drawn over the diff rather
        // than beside it, and the press would still land on chunk 0.
        let rows = numbered_rows(4);
        let inside = pos2(40.0, 8.0);
        let outside = pos2(40.0, 380.0);
        let mut key = None;
        let mut chunk = None;
        let pass = |input: RawInput, key: &mut Option<Id>, chunk: &mut Option<Id>| {
            let mut output = ctx.run_ui(input, |ui| {
                let prepared = layout(ui.ctx(), &rows);
                diff_region(ui);
                prepared.show_viewport(
                    ui,
                    Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(800.0, 400.0)),
                    7.0,
                );
                *key = Some(selection_key(ui));
                *chunk = Some(chunk_id(ui, 0));
                // Something else selectable, at the bottom of the window.
                ui.scope_builder(
                    egui::UiBuilder::new().max_rect(Rect::from_min_size(
                        pos2(0.0, 370.0),
                        Vec2::new(200.0, 20.0),
                    )),
                    |ui| {
                        ui.label("elsewhere");
                    },
                );
            });
            output.textures_delta.clear();
        };

        pass(
            with_events(vec![Event::PointerMoved(inside)]),
            &mut key,
            &mut chunk,
        );
        pass(
            with_events(vec![Event::PointerMoved(inside), press(inside, true)]),
            &mut key,
            &mut chunk,
        );
        let ends = |key: Option<Id>| ctx.data(|d| d.get_temp::<SelectionEnds>(key.unwrap()));
        assert_eq!(
            ends(key),
            Some(SelectionEnds {
                anchor: chunk,
                focus: chunk,
            })
        );

        pass(
            with_events(vec![press(inside, false)]),
            &mut key,
            &mut chunk,
        );
        pass(
            with_events(vec![Event::PointerMoved(outside), press(outside, true)]),
            &mut key,
            &mut chunk,
        );
        assert_eq!(
            ends(key),
            Some(SelectionEnds::default()),
            "a press outside the diff hands its ends back"
        );
    }

    #[test]
    fn a_hovered_link_beats_the_selection_ibeam() {
        let ctx = Context::default();
        let rows = [Line {
            spans: vec![span("link text here", Some("https://example.test/a"))],
            fill_to_eol: None,
        }];
        let mut at = pos2(0.0, 0.0);
        let mut output = ctx.run_ui(small_screen(), |ui| {
            let prepared = layout(ui.ctx(), &rows);
            let response = prepared.show(ui, 7.0);
            at = response.rect.left_top() + Vec2::new(6.0, 6.0);
        });
        output.textures_delta.clear();

        // Two passes are mandatory: `on_label` asks for the I-beam only when
        // `response.hovered()`, which is computed at `begin_pass` from the
        // previous pass's widget rects -- so a single-pass test passes even
        // with the wrong ordering and pins nothing. No button is pressed: an
        // active drag makes `on_end_pass` force the I-beam regardless
        // (label_text_selection.rs:218-220), which is correct.
        let mut icon = CursorIcon::Default;
        for _ in 0..2 {
            let mut output = ctx.run_ui(with_events(vec![Event::PointerMoved(at)]), |ui| {
                let prepared = layout(ui.ctx(), &rows);
                prepared.show(ui, 7.0);
            });
            icon = output.platform_output.cursor_icon;
            output.textures_delta.clear();
        }
        assert_eq!(icon, CursorIcon::PointingHand);
    }

    fn initialized_layout(ctx: &Context, rows: &[Line]) -> PreparedLayout {
        let mut prepared = None;
        let mut output = ctx.run_ui(Default::default(), |ui| {
            prepared = Some(layout(ui.ctx(), rows));
        });
        output.textures_delta.clear();
        prepared.expect("the layout was prepared")
    }

    /// One pass of the production shape: the region, then every chunk, with a
    /// window small enough that only the first chunk is painted.
    fn accesskit_pass(
        ctx: &Context,
        prepared: &PreparedLayout,
        input: RawInput,
    ) -> accesskit::TreeUpdate {
        accesskit_pass_at(ctx, prepared, input, 0.0)
    }

    fn accesskit_pass_at(
        ctx: &Context,
        prepared: &PreparedLayout,
        input: RawInput,
        viewport_top: f32,
    ) -> accesskit::TreeUpdate {
        let mut output = ctx.run_ui(input, |ui| {
            diff_region(ui);
            prepared.show_viewport(
                ui,
                Rect::from_min_size(pos2(0.0, viewport_top), Vec2::new(800.0, 400.0)),
                7.0,
            );
        });
        output.textures_delta.clear();
        output
            .platform_output
            .accesskit_update
            .expect("AccessKit tree update")
    }

    #[test]
    fn the_whole_diff_reaches_accesskit_while_only_the_viewport_is_drawn() {
        let ctx = Context::default();
        ctx.enable_accesskit();
        let rows = numbered_rows(PREPARED_CHUNK_ROWS * 8);
        let prepared = initialized_layout(&ctx, &rows);
        let update = accesskit_pass(&ctx, &prepared, small_screen());

        let labels = update
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == accesskit::Role::Label)
            .count();
        assert_eq!(
            labels, 8,
            "every chunk is reachable, not just the drawn ones"
        );

        let region = update
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(RENDERED_DIFF_LABEL))
            .expect("the region node");
        let scroll = region
            .1
            .children()
            .iter()
            .find_map(|id| update.nodes.iter().find(|(node_id, _)| node_id == id))
            .filter(|(_, node)| node.role() == crate::ui::scroll_region_role())
            .expect("the region's scroll view");
        let text = scroll
            .1
            .children()
            .iter()
            .filter_map(|id| update.nodes.iter().find(|(node_id, _)| node_id == id))
            .filter(|(_, node)| node.role() == accesskit::Role::Label)
            .filter_map(|(_, node)| node.value())
            .collect::<Vec<_>>()
            .join("\n");
        let expected = (0..rows.len())
            .map(|row| format!("row {row}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            text, expected,
            "no row is unreachable because it is off screen"
        );
    }

    #[test]
    fn a_new_layout_seeds_accesskit_in_the_final_multipass_output() {
        let ctx = Context::default();
        ctx.enable_accesskit();
        let rows = numbered_rows(PREPARED_CHUNK_ROWS * 8);
        let prepared = initialized_layout(&ctx, &rows);
        let mut output = ctx.run_ui(small_screen(), |ui| {
            diff_region(ui);
            prepared.show_viewport(
                ui,
                Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(800.0, 400.0)),
                7.0,
            );
            if ui.ctx().current_pass_index() == 0 {
                ui.ctx()
                    .request_discard("exercise the final AccessKit pass");
            }
        });
        assert_eq!(output.platform_output.num_completed_passes, 2);
        let update = output
            .platform_output
            .accesskit_update
            .take()
            .expect("AccessKit tree update");
        output.textures_delta.clear();
        let labels = update
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == accesskit::Role::Label)
            .count();
        assert_eq!(
            labels, 8,
            "the discarded seed pass was not repeated in the delivered update"
        );
    }

    #[test]
    fn only_the_drawn_chunks_build_text_runs() {
        let ctx = Context::default();
        ctx.enable_accesskit();
        let rows = numbered_rows(PREPARED_CHUNK_ROWS * 8);
        let prepared = initialized_layout(&ctx, &rows);
        let update = accesskit_pass(&ctx, &prepared, small_screen());

        let runs = update
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == accesskit::Role::TextRun)
            .count();
        // Only chunk 0 intersects a 400px window, and one rendered line is one
        // galley row -- delta does the wrapping, not egui.
        assert_eq!(runs, PREPARED_CHUNK_ROWS);
        assert!(
            runs < rows.len(),
            "the whole diff is not laid into the tree"
        );
    }

    #[test]
    fn every_chunk_advertises_scroll_into_view() {
        let ctx = Context::default();
        ctx.enable_accesskit();
        let rows = numbered_rows(PREPARED_CHUNK_ROWS * 8);
        let prepared = initialized_layout(&ctx, &rows);
        let update = accesskit_pass(&ctx, &prepared, small_screen());

        let chunks = update
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == accesskit::Role::Label)
            .collect::<Vec<_>>();
        assert_eq!(chunks.len(), 8);
        assert!(
            chunks
                .iter()
                .all(|(_, node)| node.supports_action(accesskit::Action::ScrollIntoView)),
            "without this the macOS adapter never offers AXScrollToVisible"
        );
    }

    #[test]
    fn scrolling_a_chunk_into_view_moves_the_viewport() {
        let ctx = Context::default();
        ctx.enable_accesskit();
        ctx.all_styles_mut(|style| style.scroll_animation = egui::style::ScrollAnimation::none());
        let rows = numbered_rows(PREPARED_CHUNK_ROWS * 8);
        let mut last = None;
        let mut offset = 0.0;
        // Both outputs travel out through parameters: a closure that captured
        // either of them mutably could not be read between the two passes.
        let pass = |input: RawInput, last: &mut Option<Id>, offset: &mut f32| {
            let mut output = ctx.run_ui(input, |ui| {
                let prepared = layout(ui.ctx(), &rows);
                let out = egui::ScrollArea::vertical().show_viewport(ui, |ui, viewport| {
                    diff_region(ui);
                    *last = Some(chunk_id(ui, 7));
                    prepared.show_viewport(ui, viewport, 7.0);
                });
                *offset = out.state.offset.y;
            });
            output.textures_delta.clear();
        };

        pass(small_screen(), &mut last, &mut offset);
        assert_eq!(offset, 0.0);
        let target = last.unwrap().accesskit_id();
        pass(
            with_events(vec![Event::AccessKitActionRequest(
                accesskit::ActionRequest {
                    action: accesskit::Action::ScrollIntoView,
                    target_tree: accesskit::TreeId::ROOT,
                    target_node: target,
                    data: None,
                },
            )]),
            &mut last,
            &mut offset,
        );
        // One more, for the same reason as in
        // `page_keys_scroll_the_diff_while_the_focused_row_is_off_screen`: an
        // animated `ScrollArea` moves on the frame after the one that asked.
        pass(small_screen(), &mut last, &mut offset);
        assert!(
            offset > 0.0,
            "an off-screen chunk can scroll itself into view"
        );
    }

    #[test]
    fn the_diff_region_is_named_once_and_the_chunks_carry_their_text() {
        let ctx = Context::default();
        ctx.enable_accesskit();
        let secret = "the complete diff must not become the accessibility name";
        let rows = [Line {
            spans: vec![span(secret, None)],
            fill_to_eol: None,
        }];
        let prepared = initialized_layout(&ctx, &rows);
        let update = accesskit_pass(&ctx, &prepared, small_screen());

        let named = |what: &str| {
            update
                .nodes
                .iter()
                .filter(|(_, node)| node.label() == Some(what))
                .collect::<Vec<_>>()
        };
        let region = named(RENDERED_DIFF_LABEL);
        assert_eq!(region.len(), 1, "the region is named exactly once");
        let scroll = named(DIFF_SCROLL_LABEL);
        assert_eq!(scroll.len(), 1);
        assert_eq!(scroll[0].1.role(), crate::ui::scroll_region_role());
        let bounds = scroll[0].1.bounds().expect("scroll view bounds");
        assert_eq!((bounds.x0, bounds.y0), (0.0, 0.0));
        assert_eq!((bounds.x1, bounds.y1), (800.0, 400.0));
        assert!(
            update
                .nodes
                .iter()
                .all(|(_, node)| node.label() != Some(secret)),
            "the diff is the content of the chunks, never anyone's name"
        );

        let chunks = update
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == accesskit::Role::Label)
            .collect::<Vec<_>>();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].1.value(), Some(secret));
        assert_eq!(region[0].1.children(), &[scroll[0].0]);
        assert!(
            scroll[0].1.children().contains(&chunks[0].0),
            "the semantic scroll view does not own its rendered chunks"
        );
    }

    /// A scroll-only AccessKit pass must stay proportional to the viewport,
    /// not to a diff that can be tens of megabytes long.
    #[test]
    fn large_diff_accesskit_scroll_updates_only_viewport_scale_text() {
        let ctx = Context::default();
        ctx.enable_accesskit();
        let rows = numbered_rows(PREPARED_CHUNK_ROWS * 128);
        let prepared = initialized_layout(&ctx, &rows);

        let initial = accesskit_pass(&ctx, &prepared, small_screen());
        let initial_bytes = initial
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == accesskit::Role::Label)
            .filter_map(|(_, node)| node.value())
            .map(str::len)
            .sum::<usize>();
        assert!(
            initial_bytes > 100_000,
            "the workload fixture is too small: {initial_bytes} bytes"
        );

        // The retained layout is the important part: rebuilding it would make
        // this a new accessibility generation and correctly seed every node.
        let update = accesskit_pass_at(&ctx, &prepared, small_screen(), prepared.height / 2.0);
        let submitted_chunks = update
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == accesskit::Role::Label)
            .collect::<Vec<_>>();
        let submitted_bytes = submitted_chunks
            .iter()
            .filter_map(|(_, node)| node.value())
            .map(str::len)
            .sum::<usize>();

        // One visible chunk, one overscan chunk, and at most the same set from
        // the previous pass. This bound is independent of the 128-chunk input.
        assert!(
            submitted_chunks.len() <= 2 * (1 + 2 * VIEWPORT_OVERSCAN_CHUNKS),
            "submitted {} of {} chunks again",
            submitted_chunks.len(),
            prepared.chunk_count(),
        );
        assert!(
            submitted_bytes * 16 < initial_bytes,
            "scroll update copied {submitted_bytes} of {initial_bytes} bytes"
        );

        let region = update
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(RENDERED_DIFF_LABEL))
            .expect("the retained diff region");
        let scroll = update
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(DIFF_SCROLL_LABEL))
            .expect("the retained diff scroll view");
        assert_eq!(region.1.children(), &[scroll.0]);
        assert_eq!(
            scroll.1.children().len(),
            prepared.chunk_count(),
            "off-screen chunks stopped being children of the diff scroll view",
        );
    }

    #[test]
    fn legacy_layout_job_keeps_padding_until_the_owner_integrates_prepared_labels() {
        let rows = [Line {
            spans: vec![span("abc", None)],
            fill_to_eol: Some(Color::Indexed(1)),
        }];
        let job = to_layout_job(&rows, 8, FontId::monospace(12.0), &palette(), 16.0);
        assert_eq!(job.text, "abc     ");
    }
}
