//! `FleetModel` — the overview grid of session previews, as a pure reducer.
//!
//! Sessions this window drives are fed by the shell and mirrored by a per-tile
//! [`TerminalModel`], so their previews are live — laid out at the session's
//! real size and scaled to the tile at draw time. The fleet never attaches
//! sessions itself; a tile with no live source renders as a placeholder. The
//! model reconciles a `SessionList` into tiles bucketed by attach-state section,
//! routes pumped `SessionData` to the matching tile by id, moves focus on
//! arrow/Tab navigation (previews are view-only, so keystrokes/text are never
//! forwarded), focuses on click via a shared grid layout that doubles as the
//! hit-test, and renders the whole grid — dimming unfocused tiles, bordering the
//! focused one, and badging bell/activity. Like [`TerminalModel`] it is pure, so
//! all of this is asserted headlessly by feeding events and inspecting the
//! returned `Vec<Cmd>`, the focused id, per-tile screen text, and the `Scene`.

use std::collections::HashSet;

use std::rc::Rc;

use ghost_render::{
    BadgeKind, CacheCounters, CellMetrics, Frame, Layer, RectPx, Rgba, Run, Scene, SceneId,
    SceneItem, Style, Transform, layout_frame,
};
use ghost_vt::protocol::{SessionEvent, SessionState};
use ghost_vt::query::ThemeColors;
use ghost_vt::session::SessionInfo;

use crate::event::SessionPush;
use crate::group::Group;
use crate::input::{Key, Mods, NamedKey};
use crate::text_input::TextInput;
use crate::{Cmd, PointPx, PointerPhase, SessionId, TerminalModel, UiEvent};

const GAP: f32 = 10.0;
const FOCUS_BORDER: f32 = 2.0;
const FOCUS_COLOR: Rgba = [0.30, 0.60, 0.95, 1.0];
/// Multi-select mark ring (Space / Ctrl-click): amber, distinct from focus.
const MARK_COLOR: Rgba = [0.95, 0.75, 0.25, 1.0];
const BADGE_PX: f32 = 10.0;
/// Colour of section header labels.
const SECTION_LABEL_COLOR: Rgba = [0.65, 0.70, 0.78, 1.0];
/// Preview area fill and the muted hint drawn on a tile with no live source.
const PLACEHOLDER_BG: Rgba = [0.10, 0.11, 0.14, 1.0];
const PLACEHOLDER_FG: Rgba = [0.42, 0.46, 0.54, 1.0];
/// Default grid for a fleet-created tile mirror until the shell hands it a
/// real-size model; previews are scaled to the tile regardless of this size.
/// Also the assumed aspect ratio used to shape tiles into little terminals.
const PREVIEW_COLS: u16 = 80;
const PREVIEW_ROWS: u16 = 24;
/// Most cards per row; the grid fits as many aspect-locked cards across as the
/// window width allows, up to this many.
const MAX_PER_ROW: usize = 8;
/// A card never shrinks below this many preview lines tall (the two chrome bands
/// are extra). With many sessions the grid scrolls rather than collapsing every
/// preview to an unreadable sliver; this floor guards small windows / odd metrics.
const MIN_PREVIEW_LINES: f32 = 8.0;
/// The COMPACT preview size (fraction of the session's native size) used when the
/// grid is crowded — a readable thumbnail. A few sessions grow ABOVE this, up to
/// native (1:1, beyond which a preview can't get sharper), to use the space; a
/// crowded grid shrinks to it and scrolls. Scale-aware (relative to native).
const PREVIEW_COMPACT_SCALE: f32 = 0.5;
/// A card grows no taller than this fraction of the viewport — a bit under half —
/// so even with few sessions a section header and other cards stay visible on a
/// short window (rather than one card filling the screen). Only binds when the
/// window is short; tall windows are capped by native size instead.
const MAX_CARD_VIEWPORT_FRAC: f32 = 0.45;
/// Lines of vertical scroll per mouse-wheel notch (sign only, like the terminal's
/// scrollback — magnitude is ignored so a touchpad and a notched wheel agree).
const SCROLL_LINES: f32 = 3.0;
/// Card chrome colours (metadata header, button footer).
const CARD_META_COLOR: Rgba = [0.62, 0.66, 0.74, 1.0];
const CARD_BG: Rgba = [0.07, 0.08, 0.10, 1.0];
const BUTTON_BG: Rgba = [0.17, 0.19, 0.24, 1.0];
const BUTTON_FG: Rgba = [0.80, 0.83, 0.89, 1.0];
/// Label of an insensitive action chip (e.g. detach on a session this window
/// does not drive) — visibly dimmed, and inert to clicks.
const BUTTON_DISABLED_FG: Rgba = [0.42, 0.46, 0.54, 0.7];
/// Confirm-overlay colours (a scrim, its prompt text, and the choice buttons).
const OVERLAY_BG: Rgba = [0.04, 0.04, 0.06, 0.82];
const OVERLAY_FG: Rgba = [0.92, 0.94, 0.97, 1.0];
/// Confirm-modal text is emphasized 50% over the terminal font size.
const MODAL_SCALE: f32 = 1.5;
/// Confirm-modal button chips: red for a destructive action (kill), green
/// for a simple confirmation (take over), neutral grey for the safe cancel;
/// the selected chip carries the focus ring.
const DESTRUCTIVE_BUTTON_BG: Rgba = [0.52, 0.15, 0.15, 1.0];
const AFFIRM_BUTTON_BG: Rgba = [0.13, 0.38, 0.20, 1.0];
const CANCEL_BUTTON_BG: Rgba = [0.24, 0.26, 0.31, 1.0];
/// How often (ms) the fleet asks the shell to re-enumerate sessions. A slow
/// backstop, not the state channel: per-session state arrives pushed
/// (`UiEvent::SessionPush`) and set changes arrive as `UiEvent::SessionsChanged`
/// hints; the floor covers hosts that predate subscriptions and missed hints.
const REFRESH_MS: u64 = 5_000;
// The floor is a backstop, not the state channel — it must stay slow enough
// that pushed state (not this tick) is what the user experiences.
const _: () = assert!(REFRESH_MS >= 5_000);

/// Bounds on a preview's width:height aspect. A tile follows its own grid's
/// shape, but one degenerate session (2 columns, or 400) must not produce a
/// sliver or a row-swallowing slab.
const MIN_TILE_ASPECT: f32 = 0.5;
const MAX_TILE_ASPECT: f32 = 4.0;

/// A laid-out tile: stable handle, session id, and pixel rect.
type Placement = (u64, SessionId, RectPx);

/// The confirm modal's geometry: the message line and the two choice buttons.
struct ConfirmLayout {
    message: RectPx,
    confirm: RectPx,
    cancel: RectPx,
}
/// A section header: its locality and the header band's rect.
/// A header band in the laid-out grid: an attach-state section, or a group
/// block (which also carries the whole block's rect for its accent outline).
#[derive(Clone, Copy)]
enum Band {
    Section(Locality),
    Group { idx: usize, block: RectPx },
}

type SectionHeader = (Band, RectPx);

/// Padding of a group block's outline around its header + member rows.
const BLOCK_PAD: f32 = 6.0;

/// [`SceneId::Section`] ranks 0–2 are the locality sections; group headers are
/// keyed from this base up.
const GROUP_SECTION_RANK_BASE: u8 = 3;

/// A per-card action button.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Button {
    Kill,
    Detach,
    Rename,
}

impl Button {
    fn label(self) -> &'static str {
        match self {
            Button::Kill => "kill",
            Button::Detach => "detach",
            Button::Rename => "rename",
        }
    }
}

/// A group-header action button. Which of these a header actually shows is
/// per-group ([`FleetModel::group_chipset`]): a chip renders only when it has
/// something to act on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GroupButton {
    /// Respawn every dead member in the background (hosts + seeded screens
    /// only; each child command starts when its session is first opened).
    Relaunch,
    /// Attach every member to this window and switch to the first.
    Open,
    /// Detach the members this window drives (they keep running).
    Detach,
    /// Dissolve the grouping; the sessions themselves are untouched.
    Ungroup,
    /// Kill every member and its process (confirmed first).
    Kill,
}

impl GroupButton {
    fn label(self) -> &'static str {
        match self {
            GroupButton::Relaunch => "relaunch",
            GroupButton::Open => "attach all",
            GroupButton::Detach => "detach",
            GroupButton::Ungroup => "ungroup",
            GroupButton::Kill => "kill",
        }
    }
}

/// Lay out a group header's action chips, right-aligned on the header band —
/// shared by the view and pointer hit-testing, like [`card_layout`] for cards.
fn group_buttons(
    header: RectPx,
    m: CellMetrics,
    set: &[GroupButton],
) -> Vec<(GroupButton, RectPx)> {
    let mut out = Vec::with_capacity(set.len());
    // Laid right-to-left from the band's right edge, half a cell apart.
    let mut x = header.x + header.w;
    for b in set.iter().rev() {
        let w = (b.label().chars().count() as f32 + 2.0) * m.advance;
        x -= w;
        out.push((
            *b,
            RectPx {
                x,
                y: header.y,
                w,
                h: header.h,
            },
        ));
        x -= m.advance * 0.5;
    }
    out.reverse();
    out
}

/// An action awaiting a yes/no confirmation (a modal overlay).
struct Pending {
    target: PendingTarget,
    action: PendingAction,
    /// Which button holds keyboard focus; starts on Cancel (the safe choice),
    /// so plain Enter never destroys anything.
    selected: Choice,
}

/// What a pending confirmation acts on: one session, or a whole group (by
/// index into the fleet's groups).
enum PendingTarget {
    Session(SessionId),
    Group(usize),
}

/// The confirm modal's two buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Choice {
    Confirm,
    Cancel,
}

impl Choice {
    fn other(self) -> Choice {
        match self {
            Choice::Confirm => Choice::Cancel,
            Choice::Cancel => Choice::Confirm,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingAction {
    /// Steal a session held by another window into this one.
    TakeOver,
    /// Kill the session and its process.
    Kill,
}

/// An in-progress inline rename of a tile.
struct Renaming {
    id: SessionId,
    buffer: TextInput,
}

/// Which window owns or sees a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Locality {
    ThisWindow,
    Elsewhere,
    Detached,
}

impl Locality {
    fn rank(self) -> u8 {
        match self {
            Locality::ThisWindow => 0,
            Locality::Elsewhere => 1,
            Locality::Detached => 2,
        }
    }
}

fn locality_for(mine: &HashSet<SessionId>, id: &str, attached: bool) -> Locality {
    if mine.contains(id) {
        Locality::ThisWindow
    } else if attached {
        Locality::Elsewhere
    } else {
        Locality::Detached
    }
}

/// Stable, deterministic sort key for a tile *within* its section — and, when the
/// grouping feature lands, within its group (the comparator is hierarchy-ready:
/// section/group first, then this key). Oldest session first by creation time so
/// a session keeps its slot for life and new ones land at the end; a tile with no
/// recorded creation time sorts last (newest) until reconcile fills it in.
///
/// The tie-break is the session name, not the tile's `handle`: `created_at` is
/// millisecond-resolution but sessions spawned in the same millisecond (rapid
/// scripted launches) still tie, and `handle` is assigned in `SessionList`
/// enumeration order — which varies (directory-read order) and resets every time a
/// fresh fleet is built (each F9 / dive-back). A handle tie-break therefore lets
/// tied tiles swap slots between rebuilds; the globally-unique, stable name never
/// does.
fn tile_order_key(t: &Tile) -> (i64, &str) {
    (t.created_at.unwrap_or(i64::MAX), t.id.as_str())
}

struct Tile {
    handle: u64,
    id: SessionId,
    model: TerminalModel,
    bell: bool,
    locality: Locality,
    /// The command the session runs (empty = the user's `$SHELL`), shown in the
    /// card header.
    command: Vec<String>,
    /// The session's process id, shown in the card header.
    pid: i32,
    /// Unix seconds at which the session was created, or `None` if the host
    /// hasn't recorded it (or the tile was adopted before its first reconcile).
    /// The primary, stable sort key (see [`tile_order_key`]).
    created_at: Option<i64>,
    /// Unseen-output count since this tile was last focused (drives the badge).
    activity: u32,
    /// Whether the shell has delivered output for this tile (i.e. this window
    /// drives the session). An unfed tile renders as a placeholder, not a live
    /// preview.
    fed: bool,
    /// Cached laid-out preview, rebuilt only when this tile's content or size
    /// changes (see [`FleetModel::refresh_dirty_frames`]); `view` clones the `Rc`
    /// rather than re-running `layout_frame` for every tile every frame. Sharing the
    /// SAME `Rc` across presents is what lets the renderer skip re-rastering an
    /// unchanged tile's Surface (an `Rc::ptr_eq` cache hit). `None` until the first
    /// refresh.
    frame: Option<Rc<Frame>>,
    /// Set when `frame` is stale (the tile got output or was resized) so the next
    /// refresh rebuilds it. Focus/bell/activity changes do not set this — they
    /// affect only the border/badge/selection, which `view` composes separately.
    frame_dirty: bool,
    /// The OSC 9;4 progress shown in the card header at the last feed, so a
    /// pure progress report (which dirties no screen rows) still repaints.
    progress: Option<ghost_term::Progress>,
    /// The session's working directory (display form, `~`-abbreviated), from
    /// the listing's descriptor read; `None` when unknown.
    cwd: Option<String>,
    /// A dead-but-remembered group member: its session is gone, but the tile
    /// stays in its group's block (previewing its recording's last screen)
    /// and activating it recreates the session. Never observed or attached;
    /// revived by a listing that names it again.
    dead: bool,
}

pub struct FleetModel {
    tiles: Vec<Tile>,
    focused: Option<SessionId>,
    /// Base (1x) cell metrics; physical metrics are these scaled by `scale`.
    metrics: CellMetrics,
    /// Device scale factor, propagated to every tile so previews track HiDPI.
    scale: f32,
    size_px: (u32, u32),
    mine: HashSet<SessionId>,
    next_handle: u64,
    /// Hit/miss tallies for the per-tile preview-frame cache: a dirty tile re-lays-out
    /// (miss + insert), an unchanged tile keeps its `Rc<Frame>` (hit). Lets a test — and
    /// the `RUST_LOG=ghost::cache` view — prove unchanged tiles are reused, not rebuilt.
    frames: CacheCounters,
    /// An action awaiting confirmation (kill, or stealing a session held
    /// elsewhere); drives a modal confirm overlay and swallows input until resolved.
    pending: Option<Pending>,
    /// An in-progress inline rename; swallows text/keys into its buffer.
    renaming: Option<Renaming>,
    /// The in-progress IME composition (empty when not composing). While non-empty,
    /// a rename swallows raw `Key::Char` presses so the eventual commit (`Text`) is
    /// the sole insertion — mirrors the terminal's preedit guard, avoiding a
    /// double-type of composed characters.
    preedit: String,
    /// The scheme's default fg/bg, stamped on every tile model so previews
    /// answer OSC 10/11 color queries like the single view does.
    theme: ThemeColors,
    /// Vertical scroll offset in physical pixels (0 = top). The grid lays out at a
    /// readable tile size regardless of session count and scrolls when it overflows
    /// the viewport, rather than shrinking previews to fit.
    scroll_y: f32,
    /// Sessions this fleet asked the shell to observe (`Cmd::Observe`) — every
    /// tile the window doesn't drive, so its preview is a live read-only
    /// mirror. Unobserved when the tile goes, the window takes the session
    /// over, or the fleet closes.
    observing: HashSet<SessionId>,
    /// Multi-selected tiles (Space / Ctrl-click), the input to group creation.
    /// Cleared by Escape — which marks claim ahead of the fleet toggle.
    marked: HashSet<SessionId>,
    /// User-defined groups, in creation order. Handed in and out by the root
    /// across fleet open/close; every mutation is persisted via
    /// `Cmd::SaveGroups`.
    groups: Vec<Group>,
    /// The group-name prompt (`g` with marks): `Some` while it is open.
    naming_group: Option<TextInput>,
    /// The in-flight pointer press, if any (see [`Grab`]): a click waiting for
    /// its release, or a tile drag in progress.
    grab: Option<Grab>,
}

/// Greedily pack cards of the given widths into rows: `(start, end, row_width)`
/// per row, half-open index ranges in order. A row takes cards while they fit
/// in `avail` (each after a `gap`) up to [`MAX_PER_ROW`]; a card wider than
/// `avail` still gets a row of its own.
fn pack_rows(widths: &[f32], avail: f32, gap: f32) -> Vec<(usize, usize, f32)> {
    let mut rows = Vec::new();
    let mut start = 0;
    let mut row_w = 0.0;
    for (i, &cw) in widths.iter().enumerate() {
        let grown = row_w + gap + cw;
        if i > start && (grown > avail || i - start >= MAX_PER_ROW) {
            rows.push((start, i, row_w));
            start = i;
            row_w = cw;
        } else {
            row_w = if i == start { cw } else { grown };
        }
    }
    if start < widths.len() {
        rows.push((start, widths.len(), row_w));
    }
    rows
}

/// Split a tile rect into its metadata header, preview area, and a row of three
/// equal action buttons, given the chrome `band` height (shared by `view` and
/// pointer hit-testing so the buttons land exactly where they are drawn).
fn card_layout(rect: RectPx, band: f32) -> (RectPx, RectPx, [(Button, RectPx); 3]) {
    let header_h = band.min(rect.h);
    let footer_h = band.min((rect.h - header_h).max(0.0));
    let footer_y = rect.y + rect.h - footer_h;
    let header = RectPx {
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: header_h,
    };
    let preview = RectPx {
        x: rect.x,
        y: rect.y + header_h,
        w: rect.w,
        h: (footer_y - (rect.y + header_h)).max(0.0),
    };
    let bw = rect.w / 3.0;
    let button = |i: f32, b: Button| {
        (
            b,
            RectPx {
                x: rect.x + i * bw,
                y: footer_y,
                w: bw,
                h: footer_h,
            },
        )
    };
    let buttons = [
        button(0.0, Button::Kill),
        button(1.0, Button::Detach),
        button(2.0, Button::Rename),
    ];
    (header, preview, buttons)
}

/// A navigation action: a 2-D arrow direction over the grid, or a linear Tab
/// step `(delta, wrap)` through tiles in layout order.
enum Nav {
    Dir(Dir),
    Step(i32, bool),
}

#[derive(Clone, Copy)]
enum Dir {
    Up,
    Down,
    Left,
    Right,
}

/// Map a key to a navigation action, or `None` if it isn't one. Arrows move in
/// 2-D (down really goes down a row, crossing section boundaries); Shift+Tab
/// steps backward, Tab forward, both wrapping.
fn nav(key: &Key, mods: Mods) -> Option<Nav> {
    match key {
        Key::Named(NamedKey::ArrowUp) => Some(Nav::Dir(Dir::Up)),
        Key::Named(NamedKey::ArrowDown) => Some(Nav::Dir(Dir::Down)),
        Key::Named(NamedKey::ArrowLeft) => Some(Nav::Dir(Dir::Left)),
        Key::Named(NamedKey::ArrowRight) => Some(Nav::Dir(Dir::Right)),
        Key::Named(NamedKey::Tab) if mods.shift => Some(Nav::Step(-1, true)),
        Key::Named(NamedKey::Tab) => Some(Nav::Step(1, true)),
        _ => None,
    }
}

/// How far the pointer may wobble (in physical pixels) between press and
/// release and still count as a click; past it a tile press becomes a drag.
const DRAG_SLOP: f32 = 6.0;

/// What a pointer press landed on. The action runs on release (so a press can
/// still become a drag); only tiles grabbed by their body can drag.
enum GrabTarget {
    Tile {
        id: SessionId,
        /// The card action button under the press, if any (`None` for the
        /// header/preview body — including a dead card, whose activation is
        /// its relaunch).
        button: Option<Button>,
    },
    Chip {
        group: usize,
        button: GroupButton,
    },
}

/// An armed pointer press: where it landed, where the pointer is now, and
/// whether it has travelled past [`DRAG_SLOP`] into a drag. Coordinates are
/// view space (`rect` is the grabbed tile's rect at press time), so the
/// floating card follows the pointer even if the grid scrolls under it.
struct Grab {
    target: GrabTarget,
    press: (f32, f32),
    pos: (f32, f32),
    rect: RectPx,
    dragging: bool,
}

impl FleetModel {
    pub fn new(metrics: CellMetrics, size_px: (u32, u32), mine: HashSet<SessionId>) -> Self {
        FleetModel {
            tiles: Vec::new(),
            focused: None,
            metrics,
            scale: 1.0,
            size_px,
            mine,
            next_handle: 0,
            frames: CacheCounters::default(),
            pending: None,
            renaming: None,
            preedit: String::new(),
            scroll_y: 0.0,
            theme: ThemeColors::default(),
            observing: HashSet::new(),
            marked: HashSet::new(),
            groups: Vec::new(),
            naming_group: None,
            grab: None,
        }
    }

    /// Set the scheme's default fg/bg (for OSC 10/11 color-query replies) on
    /// every tile model, current and future. Returns the mode-2031 dark/light
    /// notifications a real change owes subscribed sessions.
    pub fn set_theme(&mut self, theme: ThemeColors) -> Vec<Cmd> {
        self.theme = theme;
        let mut cmds = Vec::new();
        for tile in &mut self.tiles {
            cmds.extend(tile.model.set_theme(theme));
        }
        cmds
    }

    /// The user-defined groups, for [`RootModel`](crate::RootModel) to carry
    /// across fleet close/reopen (the fleet model is rebuilt each opening).
    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    /// Seed the groups on a freshly built fleet (carry-over or startup load).
    pub fn set_groups(&mut self, groups: Vec<Group>) {
        self.groups = groups;
    }

    /// Physical cell metrics: the base metrics scaled by the device scale factor.
    fn effective_metrics(&self) -> CellMetrics {
        CellMetrics {
            advance: self.metrics.advance * self.scale,
            line_height: self.metrics.line_height * self.scale,
        }
    }

    /// Render scale for the overview — device scale only (tiles auto-size to the
    /// grid, so the single view's user zoom doesn't apply here).
    pub fn render_scale(&self) -> f32 {
        self.scale
    }

    /// Start a fleet that already holds `primary` as its focused tile, so its
    /// screen state survives a toggle from the single-terminal view.
    pub fn adopting(
        primary: TerminalModel,
        warm: Vec<TerminalModel>,
        metrics: CellMetrics,
        size_px: (u32, u32),
        scale: f32,
        mine: HashSet<SessionId>,
    ) -> (Self, Vec<Cmd>) {
        let mut f = FleetModel::new(metrics, size_px, mine);
        f.scale = scale;
        let id = primary.session().to_string();
        f.focused = Some(id.clone());
        // The primary and the window's other driven sessions are all already
        // live; add each as a fed tile so every preview is warm, not "starting…".
        // Command/pid fill in on the next reconcile.
        for model in std::iter::once(primary).chain(warm) {
            let id = model.session().to_string();
            let locality = locality_for(&f.mine, &id, true);
            // No SessionInfo yet (these are live models handed over on a toggle);
            // creation time fills in on the next reconcile. Until then they sort
            // last — which is correct, they are the window's current sessions.
            f.push_tile(id, model, false, locality, Vec::new(), 0, None);
            if let Some(t) = f.tiles.last_mut() {
                t.fed = true;
            }
        }
        // The fleet never resizes its tiles' sessions; just repaint.
        (f, vec![Cmd::Redraw])
    }

    #[allow(clippy::too_many_arguments)]
    fn push_tile(
        &mut self,
        id: SessionId,
        model: TerminalModel,
        bell: bool,
        locality: Locality,
        command: Vec<String>,
        pid: i32,
        created_at: Option<i64>,
    ) {
        let handle = self.next_handle;
        self.next_handle += 1;
        self.tiles.push(Tile {
            handle,
            id,
            model,
            bell,
            locality,
            command,
            pid,
            created_at,
            activity: 0,
            fed: false,
            frame: None,
            frame_dirty: true,
            progress: None,
            cwd: None,
            dead: false,
        });
    }

    // ---- projections (for the shell + tests) ----

    pub fn focused(&self) -> Option<&str> {
        self.focused.as_deref()
    }

    /// Whether a modal (inline rename, confirm dialog, or the group-name
    /// prompt) is capturing input — keys like Escape belong to it, not to
    /// whoever hosts the fleet.
    pub fn modal_open(&self) -> bool {
        self.renaming.is_some() || self.pending.is_some() || self.naming_group.is_some()
    }

    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }

    /// The screen text of a tile, for assertions.
    pub fn tile_text(&self, id: &str) -> Option<Vec<String>> {
        self.tiles
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.model.screen().text())
    }

    pub fn locality_of(&self, id: &str) -> Option<Locality> {
        self.tiles.iter().find(|t| t.id == id).map(|t| t.locality)
    }

    /// The on-screen preview rect of a tile (post-scroll), exactly as [`view`](Self::view)
    /// draws the little terminal — the box the session's frame is fit into.
    /// `None` if the tile isn't present.
    pub fn preview_rect(&self, id: &str) -> Option<RectPx> {
        let (_, placements, band, _) = self.sections_layout();
        let (_, _, mut rect) = placements.into_iter().find(|(_, i, _)| i == id)?;
        rect.y -= self.scroll_y;
        Some(card_layout(rect, band).1)
    }

    /// The on-screen rect the session's *content* actually occupies in its tile: its
    /// frame contain-fit into the preview box, anchored at the box's top-left, exactly
    /// as the renderer draws the preview. This — not the preview box, which is shaped
    /// to a fixed aspect — is the camera target for the fleet-zoom, so a full zoom
    /// lands the session at native size and matches the live single view, with no
    /// scale jump at the dive boundary. `None` if the tile isn't present.
    pub fn dive_target_rect(&self, id: &str) -> Option<RectPx> {
        let preview = self.preview_rect(id)?;
        let tile = self.tiles.iter().find(|t| t.id == id)?;
        let (cols, rows) = tile.model.dims();
        let (fw, fh) = (
            cols as f32 * self.metrics.advance,
            rows as f32 * self.metrics.line_height,
        );
        if fw <= 0.0 || fh <= 0.0 {
            return Some(preview);
        }
        // Contain-fit, never magnifying — matching the renderer's preview scale.
        let s = (preview.w / fw).min(preview.h / fh).min(1.0);
        Some(RectPx {
            x: preview.x,
            y: preview.y,
            w: fw * s,
            h: fh * s,
        })
    }

    /// The full-zoom camera for a dive into/out of `id`: it maps the tile's drawn
    /// frame onto its NATIVE size at the window origin — exactly how the single view
    /// draws it (top-left, no cover-stretch). Using this (rather than `zoom_to`,
    /// which fills the window and so stretches a frame whose native width is a few
    /// pixels shy of the window) makes the dive's endpoint line up with the single
    /// view pixel-for-pixel. `None` if the tile isn't present.
    pub fn dive_camera(&self, id: &str) -> Option<Transform> {
        let from = self.dive_target_rect(id)?;
        let tile = self.tiles.iter().find(|t| t.id == id)?;
        let (cols, rows) = tile.model.dims();
        let to = RectPx {
            x: 0.0,
            y: 0.0,
            w: cols as f32 * self.metrics.advance,
            h: rows as f32 * self.metrics.line_height,
        };
        Some(Transform::map_rect(from, to))
    }

    /// Whether `id`'s tile is showing a live preview (it has had output fed in).
    /// `false` if the tile is a cold placeholder, or absent.
    pub fn tile_fed(&self, id: &str) -> bool {
        self.tiles.iter().any(|t| t.id == id && t.fed)
    }

    /// Prepare a cold tile (a detached session we're taking over, with no live
    /// preview yet) for a deferred dive-in: size its session to the window so its
    /// preview loads — and the dive lands — at full size, returning the resize
    /// commands for the shell to forward. `None` if the tile is already live (its
    /// preview is ready, so the caller should dive immediately) or absent.
    pub fn prepare_takeover(
        &mut self,
        id: &str,
        size_px: (u32, u32),
        scale: f32,
    ) -> Option<Vec<Cmd>> {
        let tile = self.tiles.iter_mut().find(|t| t.id == id)?;
        if tile.fed {
            return None;
        }
        Some(tile.model.update(UiEvent::Resize {
            w_px: size_px.0.max(1),
            h_px: size_px.1.max(1),
            scale: scale as f64,
        }))
    }

    /// Extract a single terminal for a toggle back to the single view, detaching
    /// every other tile's session. The window's *owned* session is kept (stable
    /// identity), falling back to the focused tile, then any tile — never a
    /// foreign session we merely previewed, so the single view always returns to
    /// what the window actually drives.
    pub fn into_single(
        self,
        size_px: (u32, u32),
        scale: f32,
    ) -> (TerminalModel, Vec<TerminalModel>, Vec<Cmd>) {
        self.into_single_keeping(None, size_px, scale)
    }

    /// Like [`into_single`](Self::into_single) but, when `target` names a present
    /// tile, keeps *that* session (a take-over of a specific tile). Otherwise it
    /// falls back to the owned session, then the focused tile, then any tile.
    /// Returns the kept model, the *other driven sessions'* models (to keep warm
    /// in the single view so their previews and Ctrl-Tab switches stay live), and
    /// the resize commands. Placeholder tiles for sessions this window doesn't
    /// drive are dropped.
    pub fn into_single_keeping(
        self,
        target: Option<SessionId>,
        size_px: (u32, u32),
        scale: f32,
    ) -> (TerminalModel, Vec<TerminalModel>, Vec<Cmd>) {
        let keep = target
            .filter(|id| self.tiles.iter().any(|t| &t.id == id))
            .or_else(|| {
                self.tiles
                    .iter()
                    .find(|t| self.mine.contains(&t.id))
                    .map(|t| t.id.clone())
            })
            .or_else(|| {
                self.tiles
                    .iter()
                    .find(|t| Some(&t.id) == self.focused.as_ref())
                    .map(|t| t.id.clone())
            })
            .or_else(|| self.tiles.first().map(|t| t.id.clone()));
        self.extract(keep.clone(), keep.unwrap_or_default(), size_px, scale)
    }

    /// Leave the overview showing `id` *specifically* — a spawn or take-over.
    /// Keeps `id`'s tile if it has one (preserving its screen); otherwise builds
    /// a fresh terminal for it (the just-spawned session has no tile yet). The
    /// other driven sessions are returned to be kept warm. Unlike
    /// [`into_single_keeping`](Self::into_single_keeping) there is no fallback to
    /// a different session: the caller asked for `id`.
    pub fn into_single_adopting(
        self,
        id: SessionId,
        size_px: (u32, u32),
        scale: f32,
    ) -> (TerminalModel, Vec<TerminalModel>, Vec<Cmd>) {
        self.extract(Some(id.clone()), id, size_px, scale)
    }

    /// Consume the fleet, returning `keep`'s model (or a fresh one named `fresh`),
    /// every other *driven* (this-window) session's model to keep warm, and the
    /// resize commands sizing them all to the window.
    fn extract(
        self,
        keep: Option<SessionId>,
        fresh: SessionId,
        size_px: (u32, u32),
        scale: f32,
    ) -> (TerminalModel, Vec<TerminalModel>, Vec<Cmd>) {
        let metrics = self.metrics;
        let theme = self.theme;
        let mine = self.mine.clone();
        // Leaving the overview closes every live mirror; the single view's
        // sessions are fed by their own clients, and a re-opened fleet
        // re-observes from a fresh snapshot.
        let mut cmds: Vec<Cmd> = self.observing.iter().cloned().map(Cmd::Unobserve).collect();
        let mut kept = None;
        let mut warm = Vec::new();
        for tile in self.tiles {
            if Some(&tile.id) == keep.as_ref() {
                kept = Some(tile.model);
            } else if mine.contains(&tile.id) {
                warm.push(tile.model); // a driven session: keep it warm
            }
            // else: a placeholder for a session we don't drive — drop it.
        }
        let resize = UiEvent::Resize {
            w_px: size_px.0.max(1),
            h_px: size_px.1.max(1),
            scale: scale as f64,
        };
        let mut model = kept.unwrap_or_else(|| {
            let mut m = TerminalModel::new(fresh, 1, 1, metrics);
            m.set_theme(theme);
            m
        });
        cmds.append(&mut model.update(resize.clone()));
        for m in &mut warm {
            cmds.append(&mut m.update(resize.clone()));
        }
        (model, warm, cmds)
    }

    // ---- update ----

    pub fn update(&mut self, ev: UiEvent) -> Vec<Cmd> {
        let cmds = match ev {
            UiEvent::SessionList(infos) => self.reconcile(infos),
            UiEvent::DeadSessions(dead) => self.dead_sessions(dead),
            UiEvent::SessionPush { name, push } => self.session_push(&name, push),
            // Authoritative groups from the shell (startup load, or another
            // window saved): replace ours without echoing a save back.
            UiEvent::GroupsLoaded(groups) => {
                if self.groups == groups {
                    Vec::new()
                } else {
                    self.groups = groups;
                    vec![Cmd::Redraw]
                }
            }
            UiEvent::SessionData { name, bytes, ended } => self.session_data(&name, bytes, ended),
            UiEvent::Resize { w_px, h_px, scale } => {
                self.size_px = (w_px, h_px);
                if scale > 0.0 && self.scale != scale as f32 {
                    self.scale = scale as f32;
                    // Effective metrics changed for every tile, so every preview
                    // must be re-laid-out even if its grid size is unchanged.
                    for tile in &mut self.tiles {
                        tile.frame_dirty = true;
                    }
                }
                // Tiles never resize their sessions; previews are scaled to fit
                // at draw time, so a window resize just repaints.
                vec![Cmd::Redraw]
            }
            // Re-enumerate on the scheduled refresh tick.
            UiEvent::Tick { .. } => vec![Cmd::ListSessions],
            // Track IME composition so an inline rename can suppress the raw keys
            // driving it (the commit arrives separately as `Text`).
            UiEvent::Preedit(s) => {
                let changed = self.preedit != s;
                self.preedit = s;
                if changed {
                    vec![Cmd::Redraw]
                } else {
                    Vec::new()
                }
            }
            // Input goes through the modal router (rename / confirm / normal).
            ev @ (UiEvent::Key { .. } | UiEvent::Text(_) | UiEvent::Pointer { .. }) => {
                self.input(ev)
            }
            _ => Vec::new(),
        };
        // Rebuild any preview whose content or size this event changed, so `view`
        // can stay a pure read of cached frames.
        self.refresh_dirty_frames();
        // A resize, or tiles appearing/disappearing, can change the content height
        // or viewport — keep the scroll offset valid (nav/wheel clamp themselves).
        self.clamp_scroll();
        cmds
    }

    /// Re-lay-out the previews of tiles marked [`Tile::frame_dirty`] and clear the
    /// flag, leaving unchanged tiles' cached frames untouched. Effective metrics
    /// are the same for every tile, so they're computed once.
    fn refresh_dirty_frames(&mut self) {
        let metrics = self.effective_metrics();
        for tile in &mut self.tiles {
            if tile.frame_dirty {
                tile.frame = Some(Rc::new(layout_frame(tile.model.screen().vt(), metrics)));
                tile.frame_dirty = false;
                self.frames.miss();
                self.frames.insert();
            } else {
                // Unchanged tile: its cached `Rc<Frame>` is reused as-is.
                self.frames.hit();
            }
        }
    }

    /// Total tile-frame (re)builds (cache misses); an unchanged tile adds none.
    pub fn frame_builds(&self) -> u32 {
        self.frames.misses as u32
    }

    /// Hit/miss tallies for the per-tile preview-frame cache.
    pub fn frame_cache(&self) -> CacheCounters {
        self.frames
    }

    fn reconcile(&mut self, infos: Vec<SessionInfo>) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        let mut dirty = false;
        let new_ids: HashSet<&str> = infos.iter().map(|i| i.name.as_str()).collect();
        let grouped: HashSet<&str> = self
            .groups
            .iter()
            .flat_map(|g| g.members.iter().map(|s| s.as_str()))
            .collect();

        // Sessions that disappeared: a group member is remembered as a dead
        // tile (its content stays; the shell refreshes it from the recording);
        // anything else is dropped — including a dead tile whose group was
        // dissolved out from under it.
        let mut gone = Vec::new();
        for t in &mut self.tiles {
            if !new_ids.contains(t.id.as_str()) && grouped.contains(t.id.as_str()) && !t.dead {
                t.dead = true;
                t.locality = Locality::Detached;
                t.frame_dirty = true; // the card meta now says "exited"
                gone.push(t.id.clone()); // still release its client + mirror
                dirty = true;
            }
        }
        self.tiles.retain(|t| {
            let keep =
                new_ids.contains(t.id.as_str()) || (t.dead && grouped.contains(t.id.as_str()));
            if !keep {
                gone.push(t.id.clone());
                dirty = true;
            }
            keep
        });
        for id in gone {
            if self.observing.remove(&id) {
                cmds.push(Cmd::Unobserve(id.clone()));
            }
            self.marked.remove(&id);
            cmds.push(Cmd::Detach(id));
        }

        // Add placeholder tiles; refresh bell/locality on existing ones. The
        // fleet never attaches sessions itself — only sessions this window
        // already drives (fed by the shell) get a live preview; the rest stay
        // placeholders until the snapshot follow-up.
        for info in &infos {
            let locality = locality_for(&self.mine, &info.name, info.attached);
            if let Some(tile) = self.tiles.iter_mut().find(|t| t.id == info.name) {
                if tile.dead
                    || tile.bell != info.bell
                    || tile.locality != locality
                    || tile.command != info.command
                    || tile.pid != info.pid
                    || tile.created_at != info.created_at
                    || tile.cwd != info.cwd
                    || tile.model.display_name() != info.display_name
                {
                    // A creation-time change reorders the grid (it's the sort key),
                    // so it warrants a repaint just like locality/metadata changes.
                    dirty = true;
                }
                // A listing naming a dead tile revives it (a recreate landed).
                tile.dead = false;
                tile.bell = info.bell;
                tile.locality = locality;
                tile.command = info.command.clone();
                tile.pid = info.pid;
                tile.created_at = info.created_at;
                tile.cwd = info.cwd.clone();
                tile.model.set_display_name(info.display_name.clone());
            } else {
                // Born at the session's listed grid, so the tile has its real
                // aspect before the observer's first snapshot lands — a dive-out
                // freezes the layout at listing time, and the grid must not
                // reshuffle under the animation when the mirrors catch up.
                let (cols, rows) = info.size.unwrap_or((PREVIEW_COLS, PREVIEW_ROWS));
                let mut model = TerminalModel::new(info.name.clone(), cols, rows, self.metrics);
                model.set_theme(self.theme);
                model.set_display_name(info.display_name.clone());
                self.push_tile(
                    info.name.clone(),
                    model,
                    info.bell,
                    locality,
                    info.command.clone(),
                    info.pid,
                    info.created_at,
                );
                self.tiles.last_mut().expect("just pushed").cwd = info.cwd.clone();
                dirty = true;
            }
        }

        // Live mirrors: observe every session this window doesn't drive, and
        // drop the observation of any it now does (its own client feeds it).
        // A dead tile has no session to observe; its preview comes from the
        // recording, fed by the shell.
        for tile in &self.tiles {
            let foreign = !tile.dead && tile.locality != Locality::ThisWindow;
            if foreign && !self.observing.contains(&tile.id) {
                self.observing.insert(tile.id.clone());
                cmds.push(Cmd::Observe(tile.id.clone()));
            } else if !foreign && self.observing.remove(&tile.id) {
                cmds.push(Cmd::Unobserve(tile.id.clone()));
            }
        }

        // Keep focus valid; default to the visually-first tile.
        if self
            .focused
            .as_ref()
            .is_none_or(|f| !self.tiles.iter().any(|t| &t.id == f))
        {
            self.focused = self.layout().into_iter().next().map(|(_, id, _)| id);
        }

        if dirty {
            cmds.push(Cmd::Redraw);
        }
        // Re-arm the periodic refresh.
        cmds.push(Cmd::ScheduleTick {
            after_ms: REFRESH_MS,
        });
        cmds
    }

    /// Seed/refresh dead tiles from the shell's descriptor sweep: a group
    /// member that died before this fleet ever saw it alive still gets its
    /// tile (metadata from the durable descriptor; the shell follows up with
    /// the recording's last screen as ordinary tile output). Only group
    /// members are remembered — a stray descriptor seeds nothing.
    fn dead_sessions(&mut self, dead: Vec<crate::event::DeadSession>) -> Vec<Cmd> {
        let mut dirty = false;
        for d in dead {
            if !self.groups.iter().any(|g| g.members.contains(&d.name)) {
                continue;
            }
            if let Some(tile) = self.tiles.iter_mut().find(|t| t.id == d.name) {
                if tile.dead && tile.model.display_name() != d.display_name {
                    tile.model.set_display_name(d.display_name);
                    dirty = true;
                }
            } else {
                let mut model =
                    TerminalModel::new(d.name.clone(), PREVIEW_COLS, PREVIEW_ROWS, self.metrics);
                model.set_theme(self.theme);
                model.set_display_name(d.display_name);
                self.push_tile(
                    d.name.clone(),
                    model,
                    false,
                    Locality::Detached,
                    d.command,
                    0,
                    None,
                );
                let t = self.tiles.last_mut().expect("just pushed");
                t.dead = true;
                t.cwd = d.cwd;
                dirty = true;
            }
        }
        if dirty { vec![Cmd::Redraw] } else { Vec::new() }
    }

    /// Apply a pushed subscription state change to its tile. This is how tile
    /// state moves between reconciles: the badge/section updates land the
    /// moment the host pushes them instead of on the next list. A push for a
    /// session with no tile yet is dropped — the reconcile seeds tiles, and
    /// the list it reads carries the same state.
    fn session_push(&mut self, id: &str, push: SessionPush) -> Vec<Cmd> {
        let focused = self.focused.as_deref() == Some(id);
        let observed = self.observing.contains(id);
        let mine = &self.mine;
        let Some(tile) = self.tiles.iter_mut().find(|t| t.id == id) else {
            return Vec::new();
        };
        let mut dirty = false;
        match push {
            SessionPush::Snapshot(SessionState {
                attached,
                bell,
                title: _, // cards don't show the OSC title (it can't retitle the window)
                display_name,
            }) => {
                let locality = locality_for(mine, id, attached.is_some());
                dirty |= tile.bell != bell
                    || tile.locality != locality
                    || tile.model.display_name() != display_name;
                tile.bell = bell;
                tile.locality = locality;
                tile.model.set_display_name(display_name);
            }
            SessionPush::Event(SessionEvent::Bell) => {
                // Marker parity: only a bell nobody was attached to witness is
                // an unseen notification — the push just removes the poll
                // latency. A live reaction for attached sessions layers on
                // this event separately.
                if tile.locality == Locality::Detached && !tile.bell {
                    tile.bell = true;
                    dirty = true;
                }
            }
            SessionPush::Event(SessionEvent::Attached(_)) => {
                let locality = locality_for(mine, id, true);
                // Attaching is "switching to" the session: the bell is seen.
                dirty |= tile.locality != locality || tile.bell;
                tile.bell = false;
                tile.locality = locality;
            }
            SessionPush::Event(SessionEvent::Detached) => {
                let locality = locality_for(mine, id, false);
                dirty |= tile.locality != locality;
                tile.locality = locality;
            }
            SessionPush::Event(SessionEvent::Renamed(name)) => {
                dirty |= tile.model.display_name() != name;
                tile.model.set_display_name(name);
            }
            SessionPush::Event(SessionEvent::Activity) => {
                if !focused {
                    tile.activity = tile.activity.saturating_add(1);
                    // Only the badge's appearance changes pixels.
                    dirty |= tile.activity == 1;
                }
            }
            SessionPush::Event(SessionEvent::TitleChanged(_)) => {}
            // The observed session's real grid (observation start, or the
            // display client resized it). Rebuild the mirror at that size —
            // the resync that follows the event re-seeds its content. Driven
            // tiles size through their own client, never through this. The
            // shell also uses this for a dead tile's recording playback (the
            // recording's grid, then its last screen as ordinary output).
            SessionPush::Event(SessionEvent::Resized { cols, rows }) => {
                if (observed || tile.dead) && tile.model.dims() != (cols, rows) {
                    let mut model = TerminalModel::new(tile.id.clone(), cols, rows, self.metrics);
                    model.set_theme(self.theme);
                    model.set_display_name(tile.model.display_name().to_string());
                    tile.model = model;
                    tile.fed = false; // placeholder until the resync lands
                    tile.frame_dirty = true;
                    dirty = true;
                }
            }
        }
        if dirty { vec![Cmd::Redraw] } else { Vec::new() }
    }

    /// Lay out section headers and tile placements in *content* space (unscrolled,
    /// origin at the top of the grid): a labelled band per non-empty attach-state
    /// section, each followed by its tiles in a fixed-width grid sized to the
    /// terminal's aspect ratio so previews look like little terminals. Returns the
    /// headers, the tile placements (full card rects), the chrome `band` height (so
    /// `card_layout` and the view split each card the same way), and the total
    /// content height. The grid is sized for readability and simply grows past the
    /// viewport, which then scrolls (`view`/`pointer` apply [`Self::scroll_y`]).
    fn sections_layout(&self) -> (Vec<SectionHeader>, Vec<Placement>, f32, f32) {
        let (w, h) = (self.size_px.0 as f32, self.size_px.1 as f32);
        let metrics = self.effective_metrics();
        let base_band = metrics.line_height + 6.0;
        // Group blocks come first, in creation order, each holding its present
        // members in their stored order — regardless of attach state (a group
        // renders together; the sections govern only the ungrouped remainder).
        // Dead members simply don't render; an all-dead group shows no block.
        let grouped: HashSet<&str> = self
            .groups
            .iter()
            .flat_map(|g| g.members.iter().map(|s| s.as_str()))
            .collect();
        let mut segments: Vec<(Band, Vec<&Tile>)> = Vec::new();
        for (idx, g) in self.groups.iter().enumerate() {
            let ts: Vec<&Tile> = g
                .members
                .iter()
                .filter_map(|id| self.tiles.iter().find(|t| &t.id == id))
                .collect();
            if !ts.is_empty() {
                segments.push((
                    Band::Group {
                        idx,
                        block: RectPx {
                            x: 0.0,
                            y: 0.0,
                            w: 0.0,
                            h: 0.0,
                        }, // filled in during placement
                    },
                    ts,
                ));
            }
        }
        // Then the attach-state sections of ungrouped tiles, preserving the
        // stable spatial order (see [`tile_order_key`]); empty sections are
        // dropped so they get no header.
        for loc in [
            Locality::ThisWindow,
            Locality::Elsewhere,
            Locality::Detached,
        ] {
            let mut ts: Vec<&Tile> = self
                .tiles
                .iter()
                .filter(|t| t.locality == loc && !grouped.contains(t.id.as_str()))
                .collect();
            ts.sort_by(|a, b| tile_order_key(a).cmp(&tile_order_key(b)));
            if !ts.is_empty() {
                segments.push((Band::Section(loc), ts));
            }
        }
        if segments.is_empty() {
            return (Vec::new(), Vec::new(), base_band, 0.0);
        }

        let (band, gap) = (base_band, GAP);
        let avail_w = (w - 2.0 * gap).max(1.0);
        // Each preview keeps ITS OWN grid's aspect (width : height): an observed
        // mirror has the session's real shape, which needn't match this window's;
        // driven tiles are window-sized and placeholders keep the 80×24 default.
        // Clamped so one degenerate grid can't blow up its row.
        let aspect = |t: &Tile| -> f32 {
            let (cols, rows) = t.model.dims();
            ((cols.max(1) as f32 * metrics.advance) / (rows.max(1) as f32 * metrics.line_height))
                .clamp(MIN_TILE_ASPECT, MAX_TILE_ASPECT)
        };
        let card_w = |t: &Tile, ch: f32| -> f32 { ((ch - 2.0 * band) * aspect(t)).min(avail_w) };

        // A card is an aspect-locked little terminal (the preview) plus two chrome
        // bands. Rows share a HEIGHT — the size that adapts to the session count:
        // a crowded grid uses the compact thumbnail size and scrolls, while a few
        // sessions GROW (up to native 1:1 — past that the preview can't get any
        // sharper) to use the space. Each card's width then follows its own
        // aspect rather than stretching to a uniform column (which would distort
        // the preview); a narrow window shrinks it to fit.
        let min_card_h = 2.0 * band + MIN_PREVIEW_LINES * metrics.line_height;
        let native_card_h = 2.0 * band + PREVIEW_ROWS as f32 * metrics.line_height;
        let compact_card_h =
            2.0 * band + PREVIEW_ROWS as f32 * metrics.line_height * PREVIEW_COMPACT_SCALE;
        // Grow no taller than native, and on a short window no taller than a bit
        // under half the viewport (so a header + other cards stay visible); never
        // below the readable floor.
        let cap = native_card_h.min((h * MAX_CARD_VIEWPORT_FRAC).max(min_card_h));
        let floor = compact_card_h.clamp(min_card_h, cap);

        // Total content height for a candidate card height, re-packing the rows
        // (cards are aspect-locked, so a taller card is wider and fewer fit).
        let content_for = |ch: f32| -> f32 {
            let mut yy = gap;
            for (_, ts) in &segments {
                let widths: Vec<f32> = ts.iter().map(|t| card_w(t, ch)).collect();
                let nrows = pack_rows(&widths, avail_w, gap).len();
                yy += band + nrows as f32 * (ch + gap);
            }
            yy
        };
        // Largest card height in [floor, cap] whose whole grid still fits the
        // viewport (few sessions enlarge to use the space); if even the compact grid
        // overflows, use the compact size and let it scroll (crowded stays dense).
        let card_h = if content_for(floor) >= h {
            floor
        } else {
            let (mut lo, mut hi) = (floor, cap);
            for _ in 0..24 {
                let mid = 0.5 * (lo + hi);
                if content_for(mid) <= h {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            lo
        };

        // Pack each segment's cards into rows of the shared height, left-aligned
        // at the segment grid's centred edge so the leftover width is shared as
        // margins (the scroll, when the grid overflows, is vertical only).
        // A sparse grid floats to the vertical centre: when the whole content
        // fits the viewport, the leftover splits into top/bottom margins
        // instead of hugging the top (max_scroll stays 0, so this never
        // interacts with scrolling).
        let mut headers = Vec::new();
        let mut placements = Vec::new();
        // (`content_for` counts a gap on both edges; centring the visible
        // block means placing its top at half the true leftover, plus the
        // leading gap that the loop below does not re-add.)
        let mut y = gap.max((h - content_for(card_h)) * 0.5 + gap);
        for (kind, ts) in &segments {
            let widths: Vec<f32> = ts.iter().map(|t| card_w(t, card_h)).collect();
            let rows = pack_rows(&widths, avail_w, gap);
            let max_row_w = rows.iter().map(|r| r.2).fold(1.0f32, f32::max);
            let left = ((w - max_row_w) / 2.0).max(gap);
            let header = RectPx {
                x: left,
                y,
                w: max_row_w,
                h: band,
            };
            y += band;
            for (start, end, _) in &rows {
                let mut x = left;
                for i in *start..*end {
                    placements.push((
                        ts[i].handle,
                        ts[i].id.clone(),
                        RectPx {
                            x,
                            y,
                            w: widths[i],
                            h: card_h,
                        },
                    ));
                    x += widths[i] + gap;
                }
                y += card_h + gap;
            }
            // A group's block outline hugs its header + rows (the trailing gap
            // stays outside).
            let kind = match *kind {
                Band::Group { idx, .. } => Band::Group {
                    idx,
                    block: RectPx {
                        x: (left - BLOCK_PAD).max(2.0),
                        y: header.y - BLOCK_PAD,
                        w: max_row_w + 2.0 * BLOCK_PAD,
                        h: (y - gap - header.y) + 2.0 * BLOCK_PAD,
                    },
                },
                sec => sec,
            };
            headers.push((kind, header));
        }
        (headers, placements, band, y)
    }

    /// Tile placements `(handle, id, rect)` in section order, content space.
    fn layout(&self) -> Vec<Placement> {
        self.sections_layout().1
    }

    /// Greatest valid scroll offset: how far the content extends past the viewport
    /// (0 when everything fits).
    fn max_scroll(&self) -> f32 {
        let content_h = self.sections_layout().3;
        (content_h - self.size_px.1 as f32).max(0.0)
    }

    /// Keep [`Self::scroll_y`] within `[0, max_scroll]` after anything that changes
    /// the content height or viewport (resize, tile add/remove, wheel, navigation).
    fn clamp_scroll(&mut self) {
        self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll());
    }

    /// Scroll a mouse-wheel notch. Sign only (magnitude ignored, like the
    /// terminal's scrollback); wheel up reveals tiles above. Returns a redraw iff
    /// the offset actually moved.
    fn wheel(&mut self, dy: f64) -> Vec<Cmd> {
        if dy == 0.0 {
            return Vec::new();
        }
        let step = SCROLL_LINES * self.effective_metrics().line_height;
        let before = self.scroll_y;
        self.scroll_y += if dy > 0.0 { -step } else { step };
        self.clamp_scroll();
        if self.scroll_y == before {
            Vec::new()
        } else {
            vec![Cmd::Redraw]
        }
    }

    /// After moving focus with the arrows, scroll just enough to bring the focused
    /// tile fully into view (with a small margin).
    fn scroll_to_focused(&mut self) {
        let (_, placements, _, _) = self.sections_layout();
        let Some((_, _, rect)) = placements
            .iter()
            .find(|(_, id, _)| Some(id.as_str()) == self.focused.as_deref())
        else {
            return;
        };
        let view_h = self.size_px.1 as f32;
        if rect.y - GAP < self.scroll_y {
            self.scroll_y = (rect.y - GAP).max(0.0);
        } else if rect.y + rect.h + GAP > self.scroll_y + view_h {
            self.scroll_y = rect.y + rect.h + GAP - view_h;
        }
        self.clamp_scroll();
    }

    fn session_data(&mut self, name: &str, bytes: Vec<u8>, ended: bool) -> Vec<Cmd> {
        let background = self.focused.as_deref() != Some(name);
        // A dead mirror reverts its tile to a placeholder; the next reconcile
        // re-observes if the session still exists.
        let observation_ended = ended && self.observing.remove(name);
        let Some(tile) = self.tiles.iter_mut().find(|t| t.id == name) else {
            return Vec::new();
        };
        if observation_ended {
            tile.fed = false;
            tile.frame_dirty = true;
        }
        let had_output = !bytes.is_empty();
        let cmds = tile.model.update(UiEvent::SessionData {
            name: name.to_string(),
            bytes,
            ended,
        });
        if had_output {
            tile.fed = true; // attached and live: stop re-attaching it
            tile.frame_dirty = true; // its screen changed; preview is stale
            // A dead tile's feed is its recording played back — history,
            // not activity; no badge for it.
            if background && !tile.dead {
                tile.activity = tile.activity.saturating_add(1);
            }
        }
        // A progress report dirties no screen rows, so the model won't ask for
        // a redraw — but the card header shows it, so a change repaints here.
        let progress = tile.model.screen().vt().progress();
        let progress_changed = progress != std::mem::replace(&mut tile.progress, progress);
        // The overview doesn't drive the window title; a tile changing its OSC
        // title must not retitle the window out from under the single view.
        let mut cmds: Vec<Cmd> = cmds
            .into_iter()
            .filter(|c| !matches!(c, Cmd::SetTitle(_)))
            .collect();
        if progress_changed && !cmds.contains(&Cmd::Redraw) {
            cmds.push(Cmd::Redraw);
        }
        cmds
    }

    /// Tab/Shift+Tab: step linearly through tiles in layout order, wrapping.
    fn move_focus_linear(&mut self, delta: i32, wrap: bool) {
        let order: Vec<SessionId> = self.layout().into_iter().map(|(_, id, _)| id).collect();
        if order.is_empty() {
            return;
        }
        let cur = self
            .focused
            .as_ref()
            .and_then(|f| order.iter().position(|id| id == f))
            .unwrap_or(0) as i32;
        let n = order.len() as i32;
        let next = if wrap {
            (cur + delta).rem_euclid(n)
        } else {
            (cur + delta).clamp(0, n - 1)
        };
        self.set_focus(order[next as usize].clone());
    }

    /// Arrow keys: move focus to the nearest tile in `dir`, using laid-out tile
    /// centres so it tracks the visual grid and crosses section boundaries
    /// naturally. Stays put when there is no tile that way.
    fn move_focus_dir(&mut self, dir: Dir) {
        let placements = self.layout();
        let Some((_, _, cur)) = placements
            .iter()
            .find(|(_, id, _)| Some(id.as_str()) == self.focused.as_deref())
            .cloned()
        else {
            // No valid focus yet: fall back to the first tile.
            if let Some((_, id, _)) = placements.first() {
                self.set_focus(id.clone());
            }
            return;
        };
        let (cx, cy) = (cur.x + cur.w / 2.0, cur.y + cur.h / 2.0);
        let mut best: Option<(f32, SessionId)> = None;
        for (_, id, r) in &placements {
            if Some(id.as_str()) == self.focused.as_deref() {
                continue;
            }
            let (dx, dy) = (r.x + r.w / 2.0 - cx, r.y + r.h / 2.0 - cy);
            // Keep only tiles in the half-plane of `dir`; score = distance along
            // the axis plus a perpendicular penalty so aligned tiles win.
            let score = match dir {
                Dir::Down if dy > 0.5 => dy + dx.abs(),
                Dir::Up if dy < -0.5 => -dy + dx.abs(),
                Dir::Right if dx > 0.5 => dx + dy.abs(),
                Dir::Left if dx < -0.5 => -dx + dy.abs(),
                _ => continue,
            };
            if best.as_ref().is_none_or(|(b, _)| score < *b) {
                best = Some((score, id.clone()));
            }
        }
        if let Some((_, id)) = best {
            self.set_focus(id);
        }
    }

    fn set_focus(&mut self, id: SessionId) {
        if let Some(t) = self.tiles.iter_mut().find(|t| t.id == id) {
            t.activity = 0; // focusing clears the activity badge
        }
        self.focused = Some(id);
    }

    /// Route an input event. An active inline rename or confirm dialog swallows
    /// input until resolved; otherwise arrows/Tab navigate, Enter activates the
    /// focused tile, and a press hits a button or activates a tile.
    fn input(&mut self, ev: UiEvent) -> Vec<Cmd> {
        if self.renaming.is_some() {
            return self.rename_input(ev);
        }
        if self.pending.is_some() {
            return self.pending_input(ev);
        }
        if self.naming_group.is_some() {
            return self.group_name_input(ev);
        }
        match ev {
            UiEvent::Key {
                key, mods, kind, ..
            } if kind.is_down() && nav(&key, mods).is_some() => {
                match nav(&key, mods).unwrap() {
                    Nav::Dir(d) => self.move_focus_dir(d),
                    Nav::Step(delta, wrap) => self.move_focus_linear(delta, wrap),
                }
                // Keep the newly-focused tile on screen when the grid scrolls.
                self.scroll_to_focused();
                vec![Cmd::Redraw]
            }
            UiEvent::Key {
                key, mods, kind, ..
            } if kind.is_down() && matches!(key, Key::Named(NamedKey::Enter)) => {
                // Ctrl-Enter opens the focused tile's whole group; plain Enter
                // (or an ungrouped tile) opens just the tile.
                let group = self.focused.as_deref().and_then(|id| self.group_of(id));
                match group.filter(|_| mods.ctrl) {
                    Some(idx) => self.open_group(idx),
                    None => self.activate(self.focused.clone()),
                }
            }
            // Space marks/unmarks the focused tile (multi-select for grouping).
            UiEvent::Key { key, kind, .. }
                if kind.is_down() && matches!(key, Key::Named(NamedKey::Space)) =>
            {
                match self.focused.clone() {
                    Some(id) => {
                        self.toggle_mark(&id);
                        vec![Cmd::Redraw]
                    }
                    None => Vec::new(),
                }
            }
            // Escape clears the marks (it reaches here only when they claimed
            // it — see [`Self::consumes_escape`]; the root otherwise turns
            // Esc into the fleet toggle before delegating).
            UiEvent::Key { key, kind, .. }
                if kind.is_down() && matches!(key, Key::Named(NamedKey::Escape)) =>
            {
                if self.grab.as_ref().is_some_and(|g| g.dragging) {
                    // Cancel the drag: the card snaps home, nothing changes.
                    self.grab = None;
                    vec![Cmd::Redraw]
                } else if self.marked.is_empty() {
                    Vec::new()
                } else {
                    self.marked.clear();
                    vec![Cmd::Redraw]
                }
            }
            // F2 renames the focused tile, the keyboard twin of its rename button.
            UiEvent::Key { key, kind, .. }
                if kind.is_down() && matches!(key, Key::Named(NamedKey::F2)) =>
            {
                match self.focused.clone() {
                    Some(id) => self.button(Button::Rename, id),
                    None => Vec::new(),
                }
            }
            // `g` groups the marked tiles: opens the name prompt.
            UiEvent::Key {
                key, mods, kind, ..
            } if kind.is_down()
                && !mods.ctrl
                && !mods.sup
                && matches!(&key, Key::Char(s) if s == "g") =>
            {
                if self.marked.is_empty() {
                    Vec::new()
                } else {
                    self.naming_group = Some(TextInput::new(String::new()));
                    vec![Cmd::Redraw]
                }
            }
            UiEvent::Pointer {
                phase: PointerPhase::Wheel,
                wheel_dy,
                ..
            } => self.wheel(wheel_dy),
            UiEvent::Pointer {
                phase, pos, mods, ..
            } => self.pointer(phase, pos, mods.ctrl),
            // Otherwise view-only: text and ordinary keys are dropped.
            _ => Vec::new(),
        }
    }

    /// Open `id` into this window's single view. A session held by another window
    /// is confirmed first, since taking it over steals its display client.
    fn activate(&mut self, id: Option<SessionId>) -> Vec<Cmd> {
        let Some(id) = id else {
            return Vec::new();
        };
        // A dead tile has nothing to attach to: opening it brings the
        // session back (respawned from its descriptor, seeded from its
        // recording); the adopt follows once the listing shows it alive.
        if self.tiles.iter().any(|t| t.id == id && t.dead) {
            return vec![Cmd::Recreate(id), Cmd::Redraw];
        }
        match self.locality_of(&id) {
            Some(Locality::Elsewhere) => {
                self.pending = Some(Pending {
                    target: PendingTarget::Session(id),
                    action: PendingAction::TakeOver,
                    selected: Choice::Cancel,
                });
                vec![Cmd::Redraw]
            }
            Some(_) => vec![Cmd::TakeOver(id), Cmd::Redraw],
            None => Vec::new(),
        }
    }

    /// The group `id` belongs to, if any.
    fn group_of(&self, id: &str) -> Option<usize> {
        self.groups
            .iter()
            .position(|g| g.members.iter().any(|m| m == id))
    }

    /// Group `idx`'s members that are alive as tiles, in stored order — what
    /// the group operations act on (dead members render, but there is nothing
    /// to attach, kill, or detach in them).
    fn present_members(&self, idx: usize) -> Vec<SessionId> {
        self.groups[idx]
            .members
            .iter()
            .filter(|id| self.tiles.iter().any(|t| &t.id == *id && !t.dead))
            .cloned()
            .collect()
    }

    /// Group `idx`'s members whose tiles are dead — what the relaunch chip
    /// brings back.
    fn dead_members(&self, idx: usize) -> Vec<SessionId> {
        self.groups[idx]
            .members
            .iter()
            .filter(|id| self.tiles.iter().any(|t| &t.id == *id && t.dead))
            .cloned()
            .collect()
    }

    /// The action chips group `idx`'s header offers, left to right: each only
    /// when it has something to act on — relaunch needs dead members,
    /// attach-all a living member not yet driven here, kill any living one,
    /// detach a member this window drives. Ungroup always applies (the
    /// grouping itself exists).
    fn group_chipset(&self, idx: usize) -> Vec<GroupButton> {
        let present = self.present_members(idx);
        let mut set = Vec::new();
        if !self.dead_members(idx).is_empty() {
            set.push(GroupButton::Relaunch);
        }
        if present
            .iter()
            .any(|id| self.locality_of(id) != Some(Locality::ThisWindow))
        {
            set.push(GroupButton::Open);
        }
        if present
            .iter()
            .any(|id| self.locality_of(id) == Some(Locality::ThisWindow))
        {
            set.push(GroupButton::Detach);
        }
        set.push(GroupButton::Ungroup);
        if !present.is_empty() {
            set.push(GroupButton::Kill);
        }
        set
    }

    /// Open the whole group into this window (Ctrl-Enter on a member, or the
    /// header's open-all button): attach every member, foreground the first.
    /// Confirmed once if any member is held by another window.
    fn open_group(&mut self, idx: usize) -> Vec<Cmd> {
        let members = self.present_members(idx);
        if members.is_empty() {
            return Vec::new();
        }
        if members
            .iter()
            .any(|id| self.locality_of(id) == Some(Locality::Elsewhere))
        {
            self.pending = Some(Pending {
                target: PendingTarget::Group(idx),
                action: PendingAction::TakeOver,
                selected: Choice::Cancel,
            });
            return vec![Cmd::Redraw];
        }
        let mut cmds = self.open_group_cmds(idx);
        cmds.push(Cmd::Redraw);
        cmds
    }

    /// Run a group-header button's action: open all immediately (confirming a
    /// take-over), detach our members immediately, dissolve the grouping, or
    /// confirm killing every member.
    fn group_button(&mut self, idx: usize, button: GroupButton) -> Vec<Cmd> {
        match button {
            GroupButton::Relaunch => {
                // Background respawns only — no confirm (no commands run: the
                // hosts come back with seeded screens, children start on first
                // attach) and no adopt (the next listing revives the tiles as
                // detached, ready to open here or elsewhere).
                let mut cmds: Vec<Cmd> = self
                    .dead_members(idx)
                    .into_iter()
                    .map(Cmd::Resurrect)
                    .collect();
                cmds.push(Cmd::Redraw);
                cmds
            }
            GroupButton::Open => self.open_group(idx),
            GroupButton::Detach => {
                let ours: Vec<SessionId> = self
                    .present_members(idx)
                    .into_iter()
                    .filter(|id| self.locality_of(id) == Some(Locality::ThisWindow))
                    .collect();
                let mut cmds: Vec<Cmd> =
                    ours.iter().flat_map(|id| self.detach_session(id)).collect();
                cmds.push(Cmd::Redraw);
                cmds
            }
            GroupButton::Ungroup => {
                self.groups.remove(idx);
                vec![Cmd::SaveGroups(self.groups.clone()), Cmd::Redraw]
            }
            GroupButton::Kill => {
                self.pending = Some(Pending {
                    target: PendingTarget::Group(idx),
                    action: PendingAction::Kill,
                    selected: Choice::Cancel,
                });
                vec![Cmd::Redraw]
            }
        }
    }

    /// Release `id` from this window: drop ownership, flip its tile to
    /// Detached, and observe it so the preview stays a live mirror — the
    /// inverse of the claim in [`Self::open_group_cmds`]. Returns the shell
    /// commands (the client drop and the observation).
    fn detach_session(&mut self, id: &SessionId) -> Vec<Cmd> {
        let mut cmds = vec![Cmd::Detach(id.clone())];
        self.mine.remove(id);
        if let Some(t) = self.tiles.iter_mut().find(|t| &t.id == id) {
            t.locality = Locality::Detached;
        }
        if self.observing.insert(id.clone()) {
            cmds.push(Cmd::Observe(id.clone()));
        }
        cmds
    }

    /// Run a card button's action: detach immediately, confirm a kill, or open an
    /// inline rename.
    fn button(&mut self, button: Button, id: SessionId) -> Vec<Cmd> {
        match button {
            Button::Detach => {
                // Only a session this window drives can be released; the chip
                // is drawn insensitive otherwise and the click is inert (it
                // does NOT fall through to opening the tile).
                if self.locality_of(&id) != Some(Locality::ThisWindow) {
                    return Vec::new();
                }
                let mut cmds = self.detach_session(&id);
                cmds.push(Cmd::Redraw);
                cmds
            }
            Button::Kill => {
                self.pending = Some(Pending {
                    target: PendingTarget::Session(id),
                    action: PendingAction::Kill,
                    selected: Choice::Cancel,
                });
                vec![Cmd::Redraw]
            }
            Button::Rename => {
                // Edit the human-facing display name, starting from what the card
                // shows; the id stays the immutable routing key.
                let seed = self
                    .tiles
                    .iter()
                    .find(|t| t.id == id)
                    .map(|t| t.model.display().to_string())
                    .unwrap_or_else(|| id.clone());
                self.renaming = Some(Renaming {
                    buffer: TextInput::new(seed),
                    id,
                });
                vec![Cmd::Redraw]
            }
        }
    }

    /// Input for the confirm dialog. The arrows (or Tab) move the selection
    /// between the two buttons and Enter chooses the selected one; Space and
    /// Escape remain the direct confirm/cancel chords; a click chooses the
    /// button under the pointer.
    fn pending_input(&mut self, ev: UiEvent) -> Vec<Cmd> {
        match ev {
            UiEvent::Key { key, kind, .. } if kind.is_down() => match key {
                Key::Named(NamedKey::Enter) => {
                    let p = self.pending.as_ref().expect("pending checked by caller");
                    match p.selected {
                        Choice::Confirm => self.run_pending(),
                        Choice::Cancel => {
                            self.pending = None;
                            vec![Cmd::Redraw]
                        }
                    }
                }
                Key::Named(NamedKey::Space) => self.run_pending(),
                Key::Named(NamedKey::Escape) => {
                    self.pending = None;
                    vec![Cmd::Redraw]
                }
                Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowRight | NamedKey::Tab) => {
                    let p = self.pending.as_mut().expect("pending checked by caller");
                    p.selected = p.selected.other();
                    vec![Cmd::Redraw]
                }
                _ => Vec::new(),
            },
            UiEvent::Pointer {
                phase: PointerPhase::Release,
                pos,
                ..
            } => {
                let p = self.pending.as_ref().expect("pending checked by caller");
                let (message, confirm_label) = self.confirm_texts(p);
                let l = self.confirm_layout(&message, confirm_label);
                let (x, y) = (pos.x as f32, pos.y as f32);
                if l.confirm.contains(x, y) {
                    self.run_pending()
                } else if l.cancel.contains(x, y) {
                    self.pending = None;
                    vec![Cmd::Redraw]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    /// Execute the pending action and dismiss the modal.
    fn run_pending(&mut self) -> Vec<Cmd> {
        let p = self.pending.take().expect("pending checked by caller");
        let mut cmds = match (&p.target, p.action) {
            (PendingTarget::Session(id), PendingAction::TakeOver) => {
                vec![Cmd::TakeOver(id.clone())]
            }
            (PendingTarget::Session(id), PendingAction::Kill) => vec![Cmd::Kill(id.clone())],
            (PendingTarget::Group(idx), PendingAction::TakeOver) => self.open_group_cmds(*idx),
            (PendingTarget::Group(idx), PendingAction::Kill) => self
                .present_members(*idx)
                .into_iter()
                .map(Cmd::Kill)
                .collect(),
        };
        cmds.push(Cmd::Redraw);
        cmds
    }

    /// Commands opening group `idx`: take over the FIRST member (the adopt
    /// path, so the window lands in its single view) and plainly attach the
    /// rest as this window's background sessions. The rest are claimed as
    /// driven NOW — tiles flip to ThisWindow and their observations close
    /// (the window's own clients feed them from here) — so leaving the fleet
    /// carries them out as warm mirrors, live content and all.
    fn open_group_cmds(&mut self, idx: usize) -> Vec<Cmd> {
        let members = self.present_members(idx);
        let mut cmds = Vec::new();
        for id in members.iter().skip(1) {
            if self.observing.remove(id) {
                cmds.push(Cmd::Unobserve(id.clone()));
            }
            self.mine.insert(id.clone());
            if let Some(t) = self.tiles.iter_mut().find(|t| &t.id == id) {
                t.locality = Locality::ThisWindow;
            }
            cmds.push(Cmd::Attach(id.clone()));
        }
        cmds.extend(members.first().map(|id| Cmd::TakeOver(id.clone())));
        cmds
    }

    /// The confirm modal's message and confirm-button label, naming the
    /// session as the user knows it (its display name) — or the group.
    fn confirm_texts(&self, p: &Pending) -> (String, &'static str) {
        let id = match &p.target {
            PendingTarget::Session(id) => id,
            PendingTarget::Group(idx) => {
                let g = &self.groups[*idx];
                let n = self.present_members(*idx).len();
                return match p.action {
                    PendingAction::Kill => {
                        (format!("Kill the {} group ({n} sessions)?", g.name), "Kill")
                    }
                    PendingAction::TakeOver => (
                        format!(
                            "{} has sessions open in another window \u{2014} take them over?",
                            g.name
                        ),
                        "Take over",
                    ),
                };
            }
        };
        let shown = self
            .tiles
            .iter()
            .find(|t| &t.id == id)
            .map(|t| t.model.display().to_string())
            .unwrap_or_else(|| id.clone());
        match p.action {
            PendingAction::Kill => (format!("Kill {shown}?"), "Kill"),
            PendingAction::TakeOver => (
                format!("{shown} is open in another window \u{2014} take it over?"),
                "Take over",
            ),
        }
    }

    /// Cell metrics of modal text: the terminal cell scaled by [`MODAL_SCALE`].
    fn modal_metrics(&self) -> CellMetrics {
        let m = self.effective_metrics();
        CellMetrics {
            advance: m.advance * MODAL_SCALE,
            line_height: m.line_height * MODAL_SCALE,
        }
    }

    /// Geometry of the confirm modal, shared by the view and the pointer
    /// hit-test: the message line with the confirm/cancel buttons centred on
    /// the line below, the whole block centred in the window.
    fn confirm_layout(&self, message: &str, confirm_label: &str) -> ConfirmLayout {
        let m = self.modal_metrics();
        let (w, h) = (self.size_px.0 as f32, self.size_px.1 as f32);
        let msg_w = message.chars().count() as f32 * m.advance;
        let message = RectPx {
            x: ((w - msg_w) * 0.5).max(0.0),
            y: ((h - m.line_height * 3.8) * 0.5).max(0.0),
            w: msg_w.max(1.0),
            h: m.line_height,
        };
        // A chip is its label padded a cell each side; the pair sits a
        // half-line under the message with a two-cell gap between them.
        let chip_w = |label: &str| (label.chars().count() as f32 + 2.0) * m.advance;
        let chip_h = m.line_height * 1.4;
        let (cw, xw) = (chip_w(confirm_label), chip_w("Cancel"));
        let gap = m.advance * 2.0;
        let x0 = ((w - (cw + gap + xw)) * 0.5).max(0.0);
        let by = message.y + m.line_height * 1.5;
        ConfirmLayout {
            message,
            confirm: RectPx {
                x: x0,
                y: by,
                w: cw,
                h: chip_h,
            },
            cancel: RectPx {
                x: x0 + cw + gap,
                y: by,
                w: xw,
                h: chip_h,
            },
        }
    }

    /// Keyboard for an inline rename: text inserts at the caret, the
    /// [`TextInput`] chords navigate and edit, Enter commits (a no-op for an
    /// empty/unchanged name), Escape cancels.
    ///
    /// Ordinary typing arrives as `UiEvent::Key`/`Key::Char` (the shell only makes
    /// `UiEvent::Text` for IME commits and pastes), so printable keys insert too —
    /// otherwise a name could be deleted but never typed. Ctrl/Cmd chords are
    /// shortcuts, not text, and are ignored; while an IME composition is active the
    /// raw keys belong to it, so they are swallowed until the `Text` commit lands.
    fn rename_input(&mut self, ev: UiEvent) -> Vec<Cmd> {
        match ev {
            UiEvent::Text(s) => {
                // A commit ends any composition; the committed text is the insertion.
                self.preedit.clear();
                if let Some(r) = &mut self.renaming {
                    r.buffer.insert(&s);
                }
                vec![Cmd::Redraw]
            }
            UiEvent::Key {
                key, mods, kind, ..
            } if kind.is_down() => match key {
                Key::Char(s) if !mods.ctrl && !mods.sup && self.preedit.is_empty() => {
                    if let Some(r) = &mut self.renaming {
                        r.buffer.insert(&s);
                    }
                    vec![Cmd::Redraw]
                }
                // The space bar is a Named key, not a Char: type it as text.
                Key::Named(NamedKey::Space)
                    if !mods.ctrl && !mods.sup && self.preedit.is_empty() =>
                {
                    if let Some(r) = &mut self.renaming {
                        r.buffer.insert(" ");
                    }
                    vec![Cmd::Redraw]
                }
                Key::Named(NamedKey::Enter) => {
                    let r = self.renaming.take().expect("renaming checked by caller");
                    let name = r.buffer.into_text();
                    let tile = self.tiles.iter_mut().find(|t| t.id == r.id);
                    let unchanged = tile.as_ref().is_some_and(|t| t.model.display() == name);
                    if name.is_empty() || unchanged {
                        vec![Cmd::Redraw]
                    } else {
                        // Show the new display name immediately; the host is the
                        // authority and the next reconcile confirms (or reverts,
                        // if it refused the name).
                        if let Some(t) = tile {
                            t.model.set_display_name(if name == r.id {
                                String::new() // renaming back to the id unlabels
                            } else {
                                name.clone()
                            });
                        }
                        vec![
                            Cmd::Rename {
                                session: r.id,
                                name,
                            },
                            Cmd::Redraw,
                        ]
                    }
                }
                Key::Named(NamedKey::Escape) => {
                    self.renaming = None;
                    vec![Cmd::Redraw]
                }
                // Everything else is offered to the entry's editing chords
                // (arrows, Home/End, Backspace/Delete and their word/line
                // variants); unhandled keys fall through untouched.
                key => {
                    if let Some(r) = &mut self.renaming
                        && r.buffer.key(&key, mods)
                    {
                        vec![Cmd::Redraw]
                    } else {
                        Vec::new()
                    }
                }
            },
            _ => Vec::new(),
        }
    }

    /// Input for the group-name prompt — the same editing surface as the
    /// inline rename (typed chars, IME commits, word/line chords), committed
    /// with Enter (an empty name cancels) and cancelled with Escape, which
    /// keeps the marks so the selection isn't lost to a typo.
    fn group_name_input(&mut self, ev: UiEvent) -> Vec<Cmd> {
        match ev {
            UiEvent::Text(s) => {
                self.preedit.clear();
                if let Some(b) = &mut self.naming_group {
                    b.insert(&s);
                }
                vec![Cmd::Redraw]
            }
            UiEvent::Key {
                key, mods, kind, ..
            } if kind.is_down() => match key {
                Key::Char(s) if !mods.ctrl && !mods.sup && self.preedit.is_empty() => {
                    if let Some(b) = &mut self.naming_group {
                        b.insert(&s);
                    }
                    vec![Cmd::Redraw]
                }
                Key::Named(NamedKey::Space)
                    if !mods.ctrl && !mods.sup && self.preedit.is_empty() =>
                {
                    if let Some(b) = &mut self.naming_group {
                        b.insert(" ");
                    }
                    vec![Cmd::Redraw]
                }
                Key::Named(NamedKey::Enter) => {
                    let name = self
                        .naming_group
                        .take()
                        .expect("prompt checked by caller")
                        .into_text();
                    if name.is_empty() {
                        return vec![Cmd::Redraw];
                    }
                    self.create_group(name)
                }
                Key::Named(NamedKey::Escape) => {
                    self.naming_group = None;
                    vec![Cmd::Redraw]
                }
                key => {
                    if let Some(b) = &mut self.naming_group
                        && b.key(&key, mods)
                    {
                        vec![Cmd::Redraw]
                    } else {
                        Vec::new()
                    }
                }
            },
            _ => Vec::new(),
        }
    }

    /// Turn the current marks into a new group: members in display order, the
    /// next palette color, persisted immediately.
    fn create_group(&mut self, name: String) -> Vec<Cmd> {
        let members: Vec<SessionId> = self
            .layout()
            .into_iter()
            .map(|(_, id, _)| id)
            .filter(|id| self.marked.contains(id))
            .collect();
        self.marked.clear();
        if members.is_empty() {
            return vec![Cmd::Redraw];
        }
        let color = (self.groups.len() % crate::group::GROUP_PALETTE.len()) as u8;
        self.groups.push(Group {
            name,
            color,
            members,
        });
        vec![Cmd::SaveGroups(self.groups.clone()), Cmd::Redraw]
    }

    fn pointer(&mut self, phase: PointerPhase, pos: PointPx, ctrl: bool) -> Vec<Cmd> {
        let (vx, vy) = (pos.x as f32, pos.y as f32);
        match phase {
            PointerPhase::Press => self.pointer_press(vx, vy, ctrl),
            PointerPhase::Motion => self.pointer_motion(vx, vy),
            PointerPhase::Release => self.pointer_release(vx, vy),
            PointerPhase::Wheel => Vec::new(),
        }
    }

    /// A press arms: it focuses and remembers what it landed on, and the
    /// action runs on the release — unless the pointer travels past the slop
    /// first and the press becomes a drag. The exception is Ctrl-click:
    /// marking is immediate (a selection gesture never drags, and delayed
    /// mark feedback reads as a miss).
    fn pointer_press(&mut self, vx: f32, vy: f32, ctrl: bool) -> Vec<Cmd> {
        // Hit-test in content space: the viewport point plus the scroll offset.
        let (px, py) = (vx, vy + self.scroll_y);
        let (headers, placements, band, _) = self.sections_layout();
        // Group-header action chips first (they sit on no tile).
        for (kind, header) in &headers {
            if let Band::Group { idx, .. } = kind
                && let Some((b, brect)) =
                    group_buttons(*header, self.effective_metrics(), &self.group_chipset(*idx))
                        .into_iter()
                        .find(|(_, r)| r.contains(px, py))
            {
                self.grab = Some(Grab {
                    target: GrabTarget::Chip {
                        group: *idx,
                        button: b,
                    },
                    press: (vx, vy),
                    pos: (vx, vy),
                    rect: brect,
                    dragging: false,
                });
                return Vec::new();
            }
        }
        let hit = placements.into_iter().find(|(_, _, r)| r.contains(px, py));
        let Some((_, id, rect)) = hit else {
            return Vec::new();
        };
        self.set_focus(id.clone());
        // Ctrl-click multi-selects (marks) rather than opening.
        if ctrl {
            self.toggle_mark(&id);
            return vec![Cmd::Redraw];
        }
        // A dead card has no live-session buttons — its whole footer is the
        // relaunch chip, and activation IS the relaunch.
        let dead = self.tiles.iter().any(|t| t.id == id && t.dead);
        let button = if dead {
            None
        } else {
            let (_, _, buttons) = card_layout(rect, band);
            buttons
                .into_iter()
                .find(|(_, r)| r.contains(px, py))
                .map(|(b, _)| b)
        };
        self.grab = Some(Grab {
            target: GrabTarget::Tile { id, button },
            press: (vx, vy),
            pos: (vx, vy),
            rect: RectPx {
                x: rect.x,
                y: rect.y - self.scroll_y,
                w: rect.w,
                h: rect.h,
            },
            dragging: false,
        });
        vec![Cmd::Redraw] // the focus ring moved
    }

    fn pointer_motion(&mut self, vx: f32, vy: f32) -> Vec<Cmd> {
        let Some(g) = &mut self.grab else {
            return Vec::new();
        };
        g.pos = (vx, vy);
        if !g.dragging {
            let (dx, dy) = (vx - g.press.0, vy - g.press.1);
            if dx * dx + dy * dy <= DRAG_SLOP * DRAG_SLOP {
                return Vec::new(); // still a click in the making
            }
            match g.target {
                // Past the slop a tile press becomes a drag of its card.
                GrabTarget::Tile { .. } => g.dragging = true,
                // A group chip doesn't drag; wandering off just abandons it.
                GrabTarget::Chip { .. } => {
                    self.grab = None;
                    return Vec::new();
                }
            }
        }
        vec![Cmd::Redraw] // the floating card follows the pointer
    }

    /// The release completes the gesture: a drag drops the card, a click runs
    /// what the press armed (a card button, a group chip, or opening the tile).
    fn pointer_release(&mut self, vx: f32, vy: f32) -> Vec<Cmd> {
        let Some(g) = self.grab.take() else {
            return Vec::new();
        };
        if g.dragging {
            return match g.target {
                GrabTarget::Tile { id, .. } => self.drop_tile(&id, vx, vy + self.scroll_y),
                GrabTarget::Chip { .. } => vec![Cmd::Redraw],
            };
        }
        match g.target {
            GrabTarget::Chip { group, button } => self.group_button(group, button),
            GrabTarget::Tile {
                id,
                button: Some(b),
            } => self.button(b, id),
            GrabTarget::Tile { id, button: None } => self.activate(Some(id)),
        }
    }

    /// Drop a dragged tile at `(px, py)` (content space). Inside a group
    /// block it joins — or moves within — that group, at the slot under the
    /// pointer; anywhere else it leaves its group. A group emptied either way
    /// dissolves, and a dead tile dragged out of its group is forgotten on
    /// the spot (only its membership was keeping it around).
    fn drop_tile(&mut self, id: &str, px: f32, py: f32) -> Vec<Cmd> {
        let before = self.groups.clone();
        let (headers, placements, _, _) = self.sections_layout();
        let target = headers.iter().find_map(|(kind, _)| match kind {
            Band::Group { idx, block } if block.contains(px, py) => Some(*idx),
            _ => None,
        });
        match target {
            Some(mut gi) => {
                // The slot under the pointer, from the pre-drop layout minus
                // the dragged card: a member counts as passed when the drop
                // point is below its row, or on it and past its centre.
                let members = self.groups[gi].members.clone();
                let slot = placements
                    .iter()
                    .filter(|(_, tid, _)| tid != id && members.iter().any(|m| m == tid))
                    .filter(|(_, _, r)| py > r.y + r.h || (py >= r.y && px > r.x + r.w * 0.5))
                    .count();
                if let Some(fi) = self.group_of(id) {
                    self.groups[fi].members.retain(|m| m != id);
                    if fi != gi && self.groups[fi].members.is_empty() {
                        self.groups.remove(fi);
                        if fi < gi {
                            gi -= 1;
                        }
                    }
                }
                self.groups[gi].members.insert(slot, id.to_string());
            }
            None => {
                if let Some(fi) = self.group_of(id) {
                    self.groups[fi].members.retain(|m| m != id);
                    if self.groups[fi].members.is_empty() {
                        self.groups.remove(fi);
                    }
                    if self.tiles.iter().any(|t| t.id == id && t.dead) {
                        self.tiles.retain(|t| t.id != id);
                        if self.focused.as_deref() == Some(id) {
                            self.focused = self.tiles.first().map(|t| t.id.clone());
                        }
                    }
                }
            }
        }
        if self.groups == before {
            return vec![Cmd::Redraw];
        }
        vec![Cmd::SaveGroups(self.groups.clone()), Cmd::Redraw]
    }

    fn toggle_mark(&mut self, id: &str) {
        if !self.marked.remove(id) {
            self.marked.insert(id.to_string());
        }
    }

    /// Whether the fleet claims an Escape press ahead of the root's
    /// Esc-leaves-the-overview shortcut: an open modal, marks to clear, or a
    /// drag to cancel.
    pub fn consumes_escape(&self) -> bool {
        self.modal_open()
            || !self.marked.is_empty()
            || self.grab.as_ref().is_some_and(|g| g.dragging)
    }

    // ---- view ----

    pub fn view(&self) -> Scene {
        let (headers, placements, band, _content_h) = self.sections_layout();
        let metrics = self.effective_metrics();
        let view_h = self.size_px.1 as f32;
        let sy = self.scroll_y;
        let mut items = Vec::new();
        for (kind, mut rect) in headers {
            rect.y -= sy;
            match kind {
                Band::Section(loc) => items.push(SceneItem::Text {
                    id: SceneId::Section(loc.rank()),
                    rect: text_line(rect, metrics, GAP * 0.5),
                    runs: vec![label_run(section_label(loc))],
                    metrics,
                    color: SECTION_LABEL_COLOR,
                    scale: 1.0,
                }),
                Band::Group { idx, mut block } => {
                    block.y -= sy;
                    let group = &self.groups[idx];
                    // Group ranks live above the three locality ranks.
                    let id = SceneId::Section(GROUP_SECTION_RANK_BASE + idx as u8);
                    items.push(SceneItem::Border {
                        id,
                        rect: block,
                        color: group.rgba(),
                        width: 1.0,
                    });
                    items.push(SceneItem::Text {
                        id,
                        rect: text_line(rect, metrics, GAP * 0.5),
                        runs: vec![label_run(&group.name)],
                        metrics,
                        color: group.rgba(),
                        scale: 1.0,
                    });
                    // The group's action chips, right-aligned on the band.
                    for (b, brect) in group_buttons(rect, metrics, &self.group_chipset(idx)) {
                        let chip = inset(brect, 2.0);
                        items.push(SceneItem::Rect {
                            id,
                            rect: chip,
                            color: BUTTON_BG,
                            radius: 3.0,
                        });
                        items.push(SceneItem::Text {
                            id,
                            rect: centered_line(chip, metrics, b.label()),
                            runs: vec![label_run(b.label())],
                            metrics,
                            color: BUTTON_FG,
                            scale: 1.0,
                        });
                    }
                }
            }
        }
        // A card being dragged floats: it renders at the pointer (keeping the
        // grab offset) instead of its slot, and last, so it rides above the
        // grid. Everything else about it — header, preview, buttons — is the
        // ordinary card, just relocated.
        let drag = self.grab.as_ref().filter(|g| g.dragging).and_then(|g| {
            let GrabTarget::Tile { id, .. } = &g.target else {
                return None;
            };
            Some((
                id.clone(),
                RectPx {
                    x: g.rect.x + (g.pos.0 - g.press.0),
                    y: g.rect.y + (g.pos.1 - g.press.1),
                    w: g.rect.w,
                    h: g.rect.h,
                },
            ))
        });
        let mut float: Vec<SceneItem> = Vec::new();
        for (handle, id, mut rect) in placements {
            rect.y -= sy;
            let floated = drag.as_ref().filter(|(did, _)| *did == id).map(|(_, r)| *r);
            if let Some(fr) = floated {
                rect = fr;
            }
            // Cull tiles fully outside the viewport: otherwise their previews are
            // re-rendered to textures (costly with many sessions) only to be
            // scissored away. Headers above stay, so the section structure shows.
            // (A floating card is under the pointer by construction — never culled.)
            if floated.is_none() && (rect.y + rect.h <= 0.0 || rect.y >= view_h) {
                continue;
            }
            let out = if floated.is_some() {
                &mut float
            } else {
                &mut items
            };
            let Some(tile) = self.tiles.iter().find(|t| t.id == id) else {
                continue;
            };
            let focused = self.focused.as_deref() == Some(id.as_str());
            let (header, preview, buttons) = card_layout(rect, band);

            // The whole card on a solid panel, so it reads as one unit.
            out.push(SceneItem::Rect {
                id: SceneId::Tile(handle),
                rect,
                color: CARD_BG,
                radius: 5.0,
            });

            // Metadata header — or the live buffer of an in-progress rename.
            let header_text = match self.renaming.as_ref().filter(|r| r.id == id) {
                Some(r) => {
                    // Caret block at the cursor, splitting the edited text.
                    let (before, after) = r.buffer.halves();
                    format!("{before}\u{2588}{after}")
                }
                // A dead card states its fate where the pid would be; a stale
                // pid or progress report would only pretend it still runs.
                None if tile.dead => format!(
                    "{} \u{b7} exited",
                    card_meta(
                        tile.model.display(),
                        &tile.command,
                        0,
                        tile.cwd.clone(),
                        None
                    )
                ),
                None => card_meta(
                    tile.model.display(),
                    &tile.command,
                    tile.pid,
                    tile.cwd.clone(),
                    tile.model.screen().vt().progress(),
                ),
            };
            // Clipped to the card: a narrow (aspect-locked) card cannot show a
            // long command, and overflow would bleed into the neighbours.
            let meta_rect = text_line(header, metrics, 6.0);
            let header_text = clip_text(&header_text, (meta_rect.w / metrics.advance) as usize);
            out.push(SceneItem::Text {
                id: SceneId::Label(handle),
                rect: meta_rect,
                runs: vec![label_run(&header_text)],
                metrics,
                color: CARD_META_COLOR,
                scale: 1.0,
            });

            // Preview area: a live, scaled terminal, or a placeholder + hint.
            if tile.fed {
                // Laid out at the session's real size; the renderer scales it to
                // the preview rect. Cloning the cached `Rc` (not re-wrapping a fresh
                // one) preserves pointer identity across presents, so an unchanged
                // tile is an `Rc::ptr_eq` cache hit in the renderer. The fallback only
                // fires before the first refresh.
                let frame = tile
                    .frame
                    .clone()
                    .unwrap_or_else(|| Rc::new(layout_frame(tile.model.screen().vt(), metrics)));
                out.push(SceneItem::Terminal {
                    id: SceneId::Tile(handle),
                    session: ghost_render::session_key(&tile.id),
                    rect: preview,
                    frame,
                    selection: if focused {
                        tile.model.selection()
                    } else {
                        None
                    },
                    dim: !focused,
                    // A preview is downscaled (contain_scale < 1), so when its cached
                    // frame DOES change the renderer re-rasters the whole Surface (no
                    // row band applies); an UNCHANGED tile short-circuits on `Rc`
                    // identity before this is consulted.
                    damage: ghost_render::TermDamage::All,
                });
            } else {
                out.push(SceneItem::Rect {
                    id: SceneId::Label(handle),
                    rect: preview,
                    color: PLACEHOLDER_BG,
                    radius: 3.0,
                });
                let hint = if tile.dead {
                    "exited \u{b7} \u{21b5} relaunches"
                } else {
                    placeholder_hint(tile.locality)
                };
                out.push(SceneItem::Text {
                    id: SceneId::Badge(handle),
                    rect: centered_line(preview, metrics, hint),
                    runs: vec![label_run(hint)],
                    metrics,
                    color: PLACEHOLDER_FG,
                    scale: 1.0,
                });
            }

            // Action buttons — a centred label on its own inset chip. A dead
            // card replaces them with one full-width relaunch chip (kill/
            // detach/rename have no live session to act on).
            if tile.dead {
                let footer = RectPx {
                    x: buttons[0].1.x,
                    y: buttons[0].1.y,
                    w: rect.w,
                    h: buttons[0].1.h,
                };
                let chip = inset(footer, 3.0);
                out.push(SceneItem::Rect {
                    id: SceneId::Tile(handle),
                    rect: chip,
                    color: BUTTON_BG,
                    radius: 3.0,
                });
                out.push(SceneItem::Text {
                    id: SceneId::Label(handle),
                    rect: centered_line(chip, metrics, "relaunch"),
                    runs: vec![label_run("relaunch")],
                    metrics,
                    color: BUTTON_FG,
                    scale: 1.0,
                });
            } else {
                for (button, brect) in buttons {
                    let chip = inset(brect, 3.0);
                    // Detach applies only to a session this window drives;
                    // elsewhere the chip is insensitive (dimmed, click inert).
                    let insensitive =
                        button == Button::Detach && tile.locality != Locality::ThisWindow;
                    out.push(SceneItem::Rect {
                        id: SceneId::Tile(handle),
                        rect: chip,
                        color: BUTTON_BG,
                        radius: 3.0,
                    });
                    out.push(SceneItem::Text {
                        id: SceneId::Label(handle),
                        rect: centered_line(chip, metrics, button.label()),
                        runs: vec![label_run(button.label())],
                        metrics,
                        color: if insensitive {
                            BUTTON_DISABLED_FG
                        } else {
                            BUTTON_FG
                        },
                        scale: 1.0,
                    });
                }
            }

            if focused {
                out.push(SceneItem::Border {
                    id: SceneId::Tile(handle),
                    rect,
                    color: FOCUS_COLOR,
                    width: FOCUS_BORDER,
                });
            }
            // A multi-select mark rings the card inside any focus ring, so a
            // tile can show both (focused AND marked) without ambiguity.
            if self.marked.contains(&id) {
                out.push(SceneItem::Border {
                    id: SceneId::Tile(handle),
                    rect: RectPx {
                        x: rect.x + FOCUS_BORDER + 1.0,
                        y: rect.y + FOCUS_BORDER + 1.0,
                        w: (rect.w - 2.0 * (FOCUS_BORDER + 1.0)).max(1.0),
                        h: (rect.h - 2.0 * (FOCUS_BORDER + 1.0)).max(1.0),
                    },
                    color: MARK_COLOR,
                    width: FOCUS_BORDER,
                });
            }
            if let Some(kind) = badge_kind(tile, focused) {
                // Clamp the badge into the tile so a tiny preview can't float it
                // outside (negative x / oversized).
                let bw = BADGE_PX.min(rect.w);
                let bh = BADGE_PX.min(rect.h);
                out.push(SceneItem::Badge {
                    id: SceneId::Badge(handle),
                    rect: RectPx {
                        x: (rect.x + rect.w - bw - 2.0).max(rect.x),
                        y: rect.y + 2.0,
                        w: bw,
                        h: bh,
                    },
                    kind,
                });
            }
        }
        items.extend(float);

        // A pending action scrims the whole grid with a confirm dialog: the
        // question in emphasized (1.5x) text, the two choice buttons centred
        // on the line below, the selected one ringed like a focused tile.
        if let Some(p) = &self.pending {
            let (w, h) = (self.size_px.0 as f32, self.size_px.1 as f32);
            items.push(SceneItem::Rect {
                id: SceneId::Sidebar,
                rect: RectPx {
                    x: 0.0,
                    y: 0.0,
                    w,
                    h,
                },
                color: OVERLAY_BG,
                radius: 0.0,
            });
            let mm = self.modal_metrics();
            let (message, confirm_label) = self.confirm_texts(p);
            let l = self.confirm_layout(&message, confirm_label);
            items.push(SceneItem::Text {
                id: SceneId::NavBar,
                rect: l.message,
                runs: vec![label_run(&message)],
                metrics: mm,
                color: OVERLAY_FG,
                scale: MODAL_SCALE,
            });
            let confirm_bg = match p.action {
                PendingAction::Kill => DESTRUCTIVE_BUTTON_BG,
                PendingAction::TakeOver => AFFIRM_BUTTON_BG,
            };
            let buttons = [
                (l.confirm, confirm_label, confirm_bg, Choice::Confirm),
                (l.cancel, "Cancel", CANCEL_BUTTON_BG, Choice::Cancel),
            ];
            for (rect, label, bg, choice) in buttons {
                items.push(SceneItem::Rect {
                    id: SceneId::NavBar,
                    rect,
                    color: bg,
                    radius: 5.0,
                });
                if p.selected == choice {
                    items.push(SceneItem::Border {
                        id: SceneId::NavBar,
                        rect,
                        color: FOCUS_COLOR,
                        width: FOCUS_BORDER,
                    });
                }
                items.push(SceneItem::Text {
                    id: SceneId::NavBar,
                    rect: centered_line(rect, mm, label),
                    runs: vec![label_run(label)],
                    metrics: mm,
                    color: OVERLAY_FG,
                    scale: MODAL_SCALE,
                });
            }
        }

        // The group-name prompt scrims the grid like the confirm dialog: an
        // emphasized label with the live edit buffer (caret block at the
        // cursor) centred beneath it. Enter commits, Escape cancels.
        if let Some(b) = &self.naming_group {
            let (w, h) = (self.size_px.0 as f32, self.size_px.1 as f32);
            items.push(SceneItem::Rect {
                id: SceneId::Sidebar,
                rect: RectPx {
                    x: 0.0,
                    y: 0.0,
                    w,
                    h,
                },
                color: OVERLAY_BG,
                radius: 0.0,
            });
            let mm = self.modal_metrics();
            let (before, after) = b.halves();
            let lines = [
                ("Group name:".to_string(), h * 0.5 - 1.5 * mm.line_height),
                (format!("{before}\u{2588}{after}"), h * 0.5),
            ];
            for (text, y) in lines {
                let tw = text.chars().count() as f32 * mm.advance;
                items.push(SceneItem::Text {
                    id: SceneId::NavBar,
                    rect: RectPx {
                        x: ((w - tw) * 0.5).max(GAP),
                        y,
                        w: tw.max(1.0),
                        h: mm.line_height,
                    },
                    runs: vec![label_run(&text)],
                    metrics: mm,
                    color: OVERLAY_FG,
                    scale: MODAL_SCALE,
                });
            }
        }

        let mut scene = Scene::new(self.size_px);
        scene.layers.push(Layer::new(0, items));
        scene
    }
}

/// Inset a rect by `pad` on every side (clamped to a positive size).
fn inset(rect: RectPx, pad: f32) -> RectPx {
    RectPx {
        x: rect.x + pad,
        y: rect.y + pad,
        w: (rect.w - 2.0 * pad).max(1.0),
        h: (rect.h - 2.0 * pad).max(1.0),
    }
}

/// A single left-aligned text line, vertically centred in `band` with `pad_x`
/// horizontal padding. The renderer draws the baseline at 0.8·line_height below
/// the rect top, so we offset the rect to centre the line within the band.
fn text_line(band: RectPx, m: CellMetrics, pad_x: f32) -> RectPx {
    RectPx {
        x: band.x + pad_x,
        y: band.y + ((band.h - m.line_height) * 0.5).max(0.0),
        w: (band.w - 2.0 * pad_x).max(1.0),
        h: m.line_height,
    }
}

/// A single text line centred both horizontally and vertically within `area`.
fn centered_line(area: RectPx, m: CellMetrics, text: &str) -> RectPx {
    let tw = text.chars().count() as f32 * m.advance;
    RectPx {
        x: area.x + ((area.w - tw) * 0.5).max(0.0),
        y: area.y + ((area.h - m.line_height) * 0.5).max(0.0),
        w: tw.max(1.0),
        h: m.line_height,
    }
}

/// The muted hint shown in a tile that has no live preview yet.
fn placeholder_hint(loc: Locality) -> &'static str {
    match loc {
        Locality::ThisWindow => "starting\u{2026}",
        Locality::Elsewhere => "attached elsewhere",
        Locality::Detached => "detached",
    }
}

/// One-line card metadata: `name · command · cwd · pid`. The command is omitted
/// when the session just runs the user's `$SHELL` (an empty command) — it's
/// always the shell there, so it's noise; the cwd and pid are omitted when
/// unknown.
fn card_meta(
    id: &str,
    command: &[String],
    pid: i32,
    cwd: Option<String>,
    progress: Option<ghost_term::Progress>,
) -> String {
    let mut s = id.to_string();
    if !command.is_empty() {
        s.push_str(" \u{b7} ");
        s.push_str(&command.join(" "));
    }
    if let Some(cwd) = cwd {
        s.push_str(" \u{b7} ");
        s.push_str(&cwd);
    }
    if pid > 0 {
        s.push_str(" \u{b7} ");
        s.push_str(&pid.to_string());
    }
    // The task's own OSC 9;4 progress report, tail position so it reads as
    // status: percentage, ✗ = error, … = busy, ⏸ = paused.
    if let Some(p) = progress {
        use ghost_term::Progress::*;
        s.push_str(" \u{b7} ");
        match p {
            Normal(pct) => s.push_str(&format!("{pct}%")),
            Error(pct) => s.push_str(&format!("\u{2717} {pct}%")),
            Indeterminate => s.push('\u{2026}'),
            Paused(pct) => s.push_str(&format!("\u{23f8} {pct}%")),
        }
    }
    s
}

/// Fit `text` into `cap` cells, marking a cut with a trailing ellipsis.
fn clip_text(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let mut s: String = text.chars().take(cap.saturating_sub(1)).collect();
    s.push('\u{2026}');
    s
}

/// The header label for an attach-state section.
fn section_label(loc: Locality) -> &'static str {
    match loc {
        Locality::ThisWindow => "Attached",
        Locality::Elsewhere => "Attached elsewhere",
        Locality::Detached => "Detached",
    }
}

/// A single left-aligned chrome label run. The renderer draws chrome text in the
/// item's colour and ignores per-run style, so a default `Style` is fine.
fn label_run(text: &str) -> Run {
    Run {
        start_col: 0,
        width_cols: text.chars().count(),
        text: text.to_string(),
        style: Style::default(),
    }
}

fn badge_kind(tile: &Tile, focused: bool) -> Option<BadgeKind> {
    if tile.bell {
        Some(BadgeKind::Bell)
    } else if !focused && tile.activity > 0 {
        Some(BadgeKind::Activity)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::KeyEventKind;
    use ghost_vt::protocol::AttachInfo;

    const METRICS: CellMetrics = CellMetrics {
        advance: 9.0,
        line_height: 18.0,
    };
    const SIZE: (u32, u32) = (400, 200);

    /// A detached session — safe for the fleet to attach and preview.
    fn info(name: &str) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            pid: 1,
            created_at: None,
            title: name.to_string(),
            command: vec![],
            attached: false,
            bell: false,
            display_name: String::new(),
            cwd: None,
            size: None,
        }
    }

    fn fleet() -> FleetModel {
        FleetModel::new(METRICS, SIZE, HashSet::new())
    }

    /// A window wide enough for a multi-column grid, for tests that exercise
    /// horizontal arrow nav or 2-D layout (the narrow default fits one column).
    const WIDE: (u32, u32) = (1000, 700);

    fn widen(m: &mut FleetModel) {
        m.update(UiEvent::Resize {
            w_px: WIDE.0,
            h_px: WIDE.1,
            scale: 1.0,
        });
    }

    fn list(m: &mut FleetModel, names: &[&str]) -> Vec<Cmd> {
        m.update(UiEvent::SessionList(
            names.iter().map(|n| info(n)).collect(),
        ))
    }

    fn data(m: &mut FleetModel, name: &str, bytes: &[u8]) -> Vec<Cmd> {
        m.update(UiEvent::SessionData {
            name: name.to_string(),
            bytes: bytes.to_vec(),
            ended: false,
        })
    }

    fn key(m: &mut FleetModel, k: Key) -> Vec<Cmd> {
        m.update(UiEvent::Key {
            key: k,
            mods: crate::Mods::NONE,
            kind: KeyEventKind::Press,
            alts: None,
        })
    }

    fn wheel(m: &mut FleetModel, dy: f64) -> Vec<Cmd> {
        m.update(UiEvent::Pointer {
            phase: PointerPhase::Wheel,
            button: None,
            pos: PointPx { x: 0.0, y: 0.0 },
            mods: crate::Mods::NONE,
            wheel_dy: dy,
            clicks: 1,
        })
    }

    /// List `n` detached sessions named `s0..sn`.
    fn list_many(m: &mut FleetModel, n: usize) {
        let infos: Vec<SessionInfo> = (0..n).map(|i| info(&format!("s{i}"))).collect();
        m.update(UiEvent::SessionList(infos));
    }

    fn sinfo(name: &str, attached: bool) -> SessionInfo {
        SessionInfo {
            attached,
            ..info(name)
        }
    }

    /// `(label, top-y)` for each section header in the rendered scene (the
    /// Section-id Text items, not placeholder name labels).
    fn headers(m: &FleetModel) -> Vec<(String, f32)> {
        m.view().layers[0]
            .items
            .iter()
            .filter_map(|it| match it {
                SceneItem::Text {
                    id: SceneId::Section(_),
                    runs,
                    rect,
                    ..
                } => Some((runs.iter().map(|r| r.text.as_str()).collect(), rect.y)),
                _ => None,
            })
            .collect()
    }

    /// The laid-out top-y of a tile (tests reach into the private layout).
    fn tile_y(m: &FleetModel, id: &str) -> f32 {
        m.layout()
            .into_iter()
            .find(|(_, i, _)| i == id)
            .unwrap()
            .2
            .y
    }

    /// A detached session with a recorded creation time (Unix seconds).
    fn info_at(name: &str, created_at: i64) -> SessionInfo {
        SessionInfo {
            created_at: Some(created_at),
            ..info(name)
        }
    }

    /// Session ids in laid-out order (section order, then within-section order).
    fn order(m: &FleetModel) -> Vec<String> {
        m.layout().into_iter().map(|(_, id, _)| id).collect()
    }

    fn push(m: &mut FleetModel, name: &str, p: SessionPush) -> Vec<Cmd> {
        m.update(UiEvent::SessionPush {
            name: name.to_string(),
            push: p,
        })
    }

    fn tile<'a>(m: &'a FleetModel, id: &str) -> &'a Tile {
        m.tiles.iter().find(|t| t.id == id).unwrap()
    }

    fn press_ctrl(m: &mut FleetModel, pos: PointPx) -> Vec<Cmd> {
        m.update(UiEvent::Pointer {
            phase: PointerPhase::Press,
            button: Some(crate::PointerButton::Left),
            pos,
            mods: Mods {
                ctrl: true,
                ..Mods::NONE
            },
            wheel_dy: 0.0,
            clicks: 1,
        })
    }

    fn centre_of(m: &FleetModel, id: &str) -> PointPx {
        let r = m
            .layout()
            .into_iter()
            .find(|(_, i, _)| i == id)
            .expect("tile placed")
            .2;
        PointPx {
            x: (r.x + r.w / 2.0) as f64,
            y: (r.y + r.h / 2.0) as f64,
        }
    }

    #[test]
    fn space_toggles_a_mark_on_the_focused_tile() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        key(&mut m, Key::Named(NamedKey::Space));
        assert!(
            m.marked.contains("a"),
            "focus defaults to a; Space marks it"
        );
        key(&mut m, Key::Named(NamedKey::Space));
        assert!(m.marked.is_empty(), "Space again unmarks");
    }

    #[test]
    fn ctrl_click_marks_without_activating() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b"]);
        let pos = centre_of(&m, "b");
        let cmds = press_ctrl(&mut m, pos);
        assert!(m.marked.contains("b"));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::TakeOver(_))),
            "a marking click must not open the tile"
        );
        press_ctrl(&mut m, pos);
        assert!(m.marked.is_empty(), "ctrl-click again unmarks");
    }

    #[test]
    fn escape_clears_marks_before_leaving_the_fleet() {
        let mut m = fleet();
        list(&mut m, &["a"]);
        key(&mut m, Key::Named(NamedKey::Space));
        assert!(m.consumes_escape(), "marks claim Esc ahead of the toggle");
        key(&mut m, Key::Named(NamedKey::Escape));
        assert!(m.marked.is_empty());
        assert!(!m.consumes_escape());
    }

    fn type_str(m: &mut FleetModel, s: &str) {
        for ch in s.chars() {
            key(m, Key::Char(ch.to_string()));
        }
    }

    #[test]
    fn g_with_marks_creates_a_named_group() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b", "c"]);
        key(&mut m, Key::Named(NamedKey::Space)); // mark "a" (default focus)
        let pos = centre_of(&m, "c");
        press_ctrl(&mut m, pos); // mark "c"
        key(&mut m, Key::Char("g".into()));
        assert!(m.modal_open(), "the group-name prompt is a modal");
        type_str(&mut m, "web");
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert_eq!(m.groups.len(), 1);
        assert_eq!(m.groups[0].name, "web");
        assert_eq!(
            m.groups[0].members,
            vec!["a".to_string(), "c".to_string()],
            "members in display order"
        );
        assert!(m.marked.is_empty(), "grouping consumes the marks");
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::SaveGroups(g) if g.len() == 1)),
            "the new group is persisted; got {cmds:?}"
        );
    }

    #[test]
    fn g_without_marks_or_name_is_inert() {
        let mut m = fleet();
        list(&mut m, &["a"]);
        key(&mut m, Key::Char("g".into()));
        assert!(!m.modal_open(), "no marks, no prompt");
        // With a mark but an empty name, Enter cancels rather than creating.
        key(&mut m, Key::Named(NamedKey::Space));
        key(&mut m, Key::Char("g".into()));
        key(&mut m, Key::Named(NamedKey::Enter));
        assert!(m.groups.is_empty());
    }

    #[test]
    fn escape_cancels_the_group_prompt_keeping_marks() {
        let mut m = fleet();
        list(&mut m, &["a"]);
        key(&mut m, Key::Named(NamedKey::Space));
        key(&mut m, Key::Char("g".into()));
        key(&mut m, Key::Named(NamedKey::Escape));
        assert!(!m.modal_open());
        assert!(m.groups.is_empty());
        assert!(
            m.marked.contains("a"),
            "cancelling the prompt keeps the selection"
        );
    }

    /// Press Enter with Ctrl held.
    fn ctrl_enter(m: &mut FleetModel) -> Vec<Cmd> {
        m.update(UiEvent::Key {
            key: Key::Named(NamedKey::Enter),
            mods: crate::Mods {
                ctrl: true,
                ..crate::Mods::NONE
            },
            kind: KeyEventKind::Press,
            alts: None,
        })
    }

    /// Focus `id` without leaving a mark (Ctrl-click toggles it on and off).
    fn focus(m: &mut FleetModel, id: &str) {
        for _ in 0..2 {
            let pos = centre_of(m, id);
            press_ctrl(m, pos);
        }
        assert_eq!(m.focused.as_deref(), Some(id));
        assert!(!m.marked.contains(id));
    }

    /// The rect of `button` on group `idx`'s header band.
    fn group_button_rect(m: &FleetModel, idx: usize, button: GroupButton) -> RectPx {
        let (headers, _, _, _) = m.sections_layout();
        let header = headers
            .iter()
            .find_map(|(b, r)| match b {
                Band::Group { idx: i, .. } if *i == idx => Some(*r),
                _ => None,
            })
            .expect("the group has a header band");
        group_buttons(header, m.effective_metrics(), &m.group_chipset(idx))
            .into_iter()
            .find(|(b, _)| *b == button)
            .expect("the chip applies to this group")
            .1
    }

    #[test]
    fn ctrl_enter_opens_the_whole_group_with_the_first_member_foreground() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b", "c"]);
        make_group(&mut m, "web", &["a", "c"]);
        focus(&mut m, "c");
        let cmds = ctrl_enter(&mut m);
        // The first member is adopted (single view); the rest attach as this
        // window's background sessions — claimed as driven right away (their
        // mirrors close, their tiles flip). The ungrouped "b" is untouched.
        assert_eq!(
            cmds,
            vec![
                Cmd::Unobserve("c".into()),
                Cmd::Attach("c".into()),
                Cmd::TakeOver("a".into()),
                Cmd::Redraw
            ]
        );
        assert_eq!(m.locality_of("c"), Some(Locality::ThisWindow));
    }

    #[test]
    fn ctrl_enter_on_an_ungrouped_tile_activates_it_alone() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b", "c"]);
        make_group(&mut m, "web", &["a", "c"]);
        focus(&mut m, "b");
        assert_eq!(
            ctrl_enter(&mut m),
            vec![Cmd::TakeOver("b".into()), Cmd::Redraw]
        );
    }

    #[test]
    fn opening_a_group_held_elsewhere_confirms_once_then_opens_all() {
        let mut m = fleet();
        widen(&mut m);
        m.update(UiEvent::SessionList(vec![
            sinfo("a", true), // held by another window
            info("b"),
            info("c"),
        ]));
        make_group(&mut m, "web", &["a", "c"]);
        focus(&mut m, "c");
        let cmds = ctrl_enter(&mut m);
        assert!(m.modal_open(), "a member held elsewhere needs a confirm");
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::TakeOver(_))),
            "nothing is taken over before the user confirms: {cmds:?}"
        );
        // Space is the direct confirm chord.
        let cmds = key(&mut m, Key::Named(NamedKey::Space));
        assert_eq!(
            cmds,
            vec![
                Cmd::Unobserve("c".into()),
                Cmd::Attach("c".into()),
                Cmd::TakeOver("a".into()),
                Cmd::Redraw
            ]
        );
    }

    #[test]
    fn the_groups_kill_button_confirms_then_kills_every_member() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b", "c"]);
        make_group(&mut m, "web", &["a", "c"]);
        let r = group_button_rect(&m, 0, GroupButton::Kill);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert!(m.modal_open(), "killing a group is confirmed first");
        let cmds = key(&mut m, Key::Named(NamedKey::Space));
        assert_eq!(
            cmds,
            vec![Cmd::Kill("a".into()), Cmd::Kill("c".into()), Cmd::Redraw]
        );
    }

    #[test]
    fn the_groups_ungroup_button_dissolves_only_the_grouping() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b", "c"]);
        make_group(&mut m, "web", &["a", "c"]);
        let r = group_button_rect(&m, 0, GroupButton::Ungroup);
        let cmds = press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert!(m.groups.is_empty(), "the grouping is gone");
        assert_eq!(m.tiles.len(), 3, "the sessions themselves are untouched");
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::SaveGroups(g) if g.is_empty())),
            "the dissolution is persisted: {cmds:?}"
        );
    }

    #[test]
    fn the_groups_detach_button_detaches_only_members_this_window_drives() {
        let mut m = FleetModel::new(METRICS, SIZE, HashSet::from(["a".to_string()]));
        widen(&mut m);
        m.update(UiEvent::SessionList(vec![
            sinfo("a", true), // driven by this window
            sinfo("c", true), // held elsewhere — not ours to detach
        ]));
        make_group(&mut m, "web", &["a", "c"]);
        let r = group_button_rect(&m, 0, GroupButton::Detach);
        let cmds = press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert_eq!(
            cmds,
            vec![
                Cmd::Detach("a".into()),
                Cmd::Observe("a".into()),
                Cmd::Redraw
            ]
        );
        assert_eq!(
            m.locality_of("a"),
            Some(Locality::Detached),
            "the released member's tile leaves This window at once"
        );
    }

    #[test]
    fn the_group_header_draws_the_chips_that_apply() {
        // A group with a member this window drives shows the full live set;
        // nothing is dead, so there is no relaunch chip.
        let mut m = FleetModel::new(METRICS, SIZE, HashSet::from(["a".to_string()]));
        widen(&mut m);
        m.update(UiEvent::SessionList(vec![sinfo("a", true), info("c")]));
        make_group(&mut m, "web", &["a", "c"]);
        assert_eq!(
            m.group_chipset(0),
            vec![
                GroupButton::Open,
                GroupButton::Detach,
                GroupButton::Ungroup,
                GroupButton::Kill
            ]
        );
        let scene = m.view();
        for b in m.group_chipset(0) {
            assert!(
                scene.layers[0].items.iter().any(|it| matches!(it,
                    SceneItem::Text { runs, .. } if runs[0].text == b.label())),
                "the {} chip is drawn",
                b.label()
            );
        }
        assert!(
            !scene.layers[0].items.iter().any(|it| matches!(it,
                SceneItem::Text { runs, .. } if runs[0].text == "relaunch")),
            "no relaunch chip while every member lives"
        );
        // Nothing driven by this window — the detach chip goes too.
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "c"]);
        make_group(&mut m, "web", &["a", "c"]);
        assert_eq!(
            m.group_chipset(0),
            vec![GroupButton::Open, GroupButton::Ungroup, GroupButton::Kill]
        );
    }

    fn dead_info(name: &str, display: &str, command: &[&str]) -> crate::event::DeadSession {
        crate::event::DeadSession {
            name: name.to_string(),
            display_name: display.to_string(),
            command: command.iter().map(|s| s.to_string()).collect(),
            cwd: None,
        }
    }

    #[test]
    fn a_dead_group_member_stays_as_a_dead_tile_an_ungrouped_one_vanishes() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b", "c"]);
        make_group(&mut m, "web", &["a", "c"]);
        data(&mut m, "c", b"LAST-WORDS");
        // b (ungrouped) and c (grouped) both die.
        let cmds = list(&mut m, &["a"]);
        assert!(
            !m.tiles.iter().any(|t| t.id == "b"),
            "an ungrouped dead session is not remembered"
        );
        let c = m
            .tiles
            .iter()
            .find(|t| t.id == "c")
            .expect("a grouped dead session keeps its tile");
        assert!(c.dead, "the kept tile is marked dead");
        assert!(c.fed, "the reconcile itself does not clear its content");
        assert!(
            m.layout().iter().any(|(_, id, _)| id == "c"),
            "the dead member still renders in its group's block"
        );
        assert!(
            !cmds.contains(&Cmd::Observe("c".into())),
            "a dead session cannot be observed: {cmds:?}"
        );
        // The card says so instead of showing a stale pid.
        let scene = m.view();
        assert!(
            scene.layers[0].items.iter().any(|it| matches!(it,
                SceneItem::Text { runs, .. } if runs[0].text.contains("exited"))),
            "the dead card is labelled exited"
        );
    }

    #[test]
    fn the_attach_all_chip_hides_when_every_member_is_already_attached_here() {
        let mut m = FleetModel::new(
            METRICS,
            SIZE,
            HashSet::from(["a".to_string(), "c".to_string()]),
        );
        widen(&mut m);
        m.update(UiEvent::SessionList(vec![
            sinfo("a", true),
            sinfo("c", true),
        ]));
        make_group(&mut m, "web", &["a", "c"]);
        assert!(
            !m.group_chipset(0).contains(&GroupButton::Open),
            "with every member driven here there is nothing left to attach: {:?}",
            m.group_chipset(0)
        );
        assert_eq!(GroupButton::Open.label(), "attach all");
        // Releasing a member brings the chip back.
        let r = button_rect(&m, "a", Button::Detach);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert!(
            m.group_chipset(0).contains(&GroupButton::Open),
            "a detached member makes attach-all applicable again"
        );
    }

    #[test]
    fn the_groups_relaunch_chip_respawns_dead_members_in_the_background() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b", "c"]);
        make_group(&mut m, "web", &["a", "b", "c"]);
        assert!(
            !m.group_chipset(0).contains(&GroupButton::Relaunch),
            "a fully-alive group offers nothing to relaunch"
        );
        list(&mut m, &["b"]); // a and c die
        let r = group_button_rect(&m, 0, GroupButton::Relaunch);
        let cmds = press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert_eq!(
            cmds,
            vec![
                Cmd::Resurrect("a".into()),
                Cmd::Resurrect("c".into()),
                Cmd::Redraw
            ],
            "every dead member respawns, in stored order, with no adopt"
        );
        assert!(!m.modal_open(), "relaunching runs no commands — no confirm");
        assert!(
            m.tiles.iter().filter(|t| t.id != "b").all(|t| t.dead),
            "tiles revive on the next listing (claim on success), not optimistically"
        );
    }

    #[test]
    fn an_all_dead_groups_chips_shrink_to_what_still_applies() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b", "c"]);
        make_group(&mut m, "web", &["a", "c"]);
        list(&mut m, &["b"]); // the whole group dies
        assert_eq!(
            m.group_chipset(0),
            vec![GroupButton::Relaunch, GroupButton::Ungroup],
            "open/detach/kill have no living member to act on"
        );
    }

    #[test]
    fn the_detach_button_releases_the_session_and_keeps_a_live_preview() {
        let mut m = FleetModel::new(METRICS, SIZE, HashSet::from(["a".to_string()]));
        widen(&mut m);
        m.update(UiEvent::SessionList(vec![sinfo("a", true), info("b")]));
        assert_eq!(m.locality_of("a"), Some(Locality::ThisWindow));
        let r = button_rect(&m, "a", Button::Detach);
        let cmds = press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert!(
            cmds.contains(&Cmd::Detach("a".into())),
            "the button detaches: {cmds:?}"
        );
        assert!(
            cmds.contains(&Cmd::Observe("a".into())),
            "the released session is observed so its preview stays live: {cmds:?}"
        );
        assert_eq!(
            m.locality_of("a"),
            Some(Locality::Detached),
            "the tile moves out of This window immediately"
        );
        // The next listing (host confirms the client is gone) keeps it there.
        m.update(UiEvent::SessionList(vec![info("a"), info("b")]));
        assert_eq!(m.locality_of("a"), Some(Locality::Detached));
    }

    #[test]
    fn the_detach_button_is_insensitive_on_sessions_this_window_does_not_drive() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a"]); // detached: nothing to release
        let r = button_rect(&m, "a", Button::Detach);
        let cmds = press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert!(
            !cmds.contains(&Cmd::Detach("a".into())),
            "nothing to detach: {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::TakeOver(_))),
            "the dead click does not fall through to opening the tile: {cmds:?}"
        );
        // And the chip reads as insensitive.
        let scene = m.view();
        let dimmed = scene.layers[0].items.iter().any(|it| {
            matches!(it,
            SceneItem::Text { runs, color, .. }
                if runs[0].text == "detach" && *color == BUTTON_DISABLED_FG)
        });
        assert!(dimmed, "the detach label is drawn dimmed");
    }

    #[test]
    fn a_dead_tile_relaunches_on_activation() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b", "c"]);
        make_group(&mut m, "web", &["a", "c"]);
        list(&mut m, &["a", "b"]); // c dies
        let cmds = press(&mut m, "c");
        assert_eq!(
            cmds,
            vec![Cmd::Recreate("c".into()), Cmd::Redraw],
            "opening a dead tile recreates its session"
        );
        // The keyboard twin.
        focus(&mut m, "c");
        assert_eq!(
            key(&mut m, Key::Named(NamedKey::Enter)),
            vec![Cmd::Recreate("c".into()), Cmd::Redraw]
        );
        // A dead card has no live-session buttons — a press where "kill"
        // would be just relaunches like anywhere else on the card — and its
        // footer offers a single relaunch chip instead.
        let r = button_rect(&m, "c", Button::Kill);
        assert_eq!(
            press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0),
            vec![Cmd::Recreate("c".into()), Cmd::Redraw]
        );
        assert!(!m.modal_open(), "no kill confirm for a dead session");
        let scene = m.view();
        let labels: Vec<&str> = scene.layers[0]
            .items
            .iter()
            .filter_map(|it| match it {
                SceneItem::Text { runs, .. } => Some(runs[0].text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            labels.contains(&"relaunch"),
            "the dead card offers relaunch"
        );
    }

    #[test]
    fn dead_sessions_seed_tiles_for_members_dead_before_the_fleet_opened() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a"]);
        m.set_groups(vec![Group {
            name: "web".into(),
            color: 0,
            members: vec!["a".into(), "x".into()],
        }]);
        m.update(UiEvent::DeadSessions(vec![dead_info(
            "x",
            "worker",
            &["npm", "run", "dev"],
        )]));
        let x = m
            .tiles
            .iter()
            .find(|t| t.id == "x")
            .expect("the dead member gets a tile");
        assert!(x.dead);
        assert_eq!(x.model.display_name(), "worker");
        assert_eq!(x.command, vec!["npm", "run", "dev"]);
        assert!(
            m.layout().iter().any(|(_, id, _)| id == "x"),
            "it renders in its group's block"
        );
        // The recording playback that follows is history, not activity: the
        // dead card must not light an activity badge over it.
        data(&mut m, "x", b"prod=# select 1;");
        let x = m.tiles.iter().find(|t| t.id == "x").unwrap();
        assert!(x.fed, "the playback feeds the preview");
        assert_eq!(x.activity, 0, "history is not activity");
        // A dead session NOT in any group is never seeded.
        m.update(UiEvent::DeadSessions(vec![dead_info("stray", "", &[])]));
        assert!(!m.tiles.iter().any(|t| t.id == "stray"));
    }

    #[test]
    fn a_revived_session_stops_being_dead_and_is_observed_again() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "c"]);
        make_group(&mut m, "web", &["a", "c"]);
        list(&mut m, &["a"]); // c dies
        assert!(m.tiles.iter().find(|t| t.id == "c").unwrap().dead);
        let cmds = list(&mut m, &["a", "c"]); // c returns
        let c = m.tiles.iter().find(|t| t.id == "c").unwrap();
        assert!(!c.dead, "a live listing revives the tile");
        assert!(
            cmds.contains(&Cmd::Observe("c".into())),
            "the revived session is mirrored again: {cmds:?}"
        );
    }

    #[test]
    fn group_ops_skip_dead_members() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b", "c"]);
        make_group(&mut m, "web", &["a", "c"]);
        list(&mut m, &["a", "b"]); // c dies
        focus(&mut m, "a");
        assert_eq!(
            ctrl_enter(&mut m),
            vec![Cmd::TakeOver("a".into()), Cmd::Redraw],
            "open-all attaches only the living"
        );
        let r = group_button_rect(&m, 0, GroupButton::Kill);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        let cmds = key(&mut m, Key::Named(NamedKey::Space));
        assert_eq!(
            cmds,
            vec![Cmd::Kill("a".into()), Cmd::Redraw],
            "kill has nothing to kill in a dead member"
        );
    }

    #[test]
    fn a_click_opens_on_release_and_a_wiggle_within_the_slop_still_clicks() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b"]);
        let (cx, cy) = centre(&tile_rect(&m, "a"));
        let cmds = pointer_phase(&mut m, PointerPhase::Press, cx, cy);
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::TakeOver(_))),
            "the press alone opens nothing (it may become a drag): {cmds:?}"
        );
        assert_eq!(m.focused(), Some("a"), "focus lands on the press");
        let cmds = pointer_phase(&mut m, PointerPhase::Release, cx, cy);
        assert!(
            cmds.contains(&Cmd::TakeOver("a".into())),
            "the release opens the tile: {cmds:?}"
        );
        // A couple of pixels of wobble is still a click, not a drag.
        let (cx, cy) = centre(&tile_rect(&m, "b"));
        pointer_phase(&mut m, PointerPhase::Press, cx, cy);
        pointer_phase(&mut m, PointerPhase::Motion, cx + 2.0, cy + 2.0);
        let cmds = pointer_phase(&mut m, PointerPhase::Release, cx + 2.0, cy + 2.0);
        assert!(
            cmds.contains(&Cmd::TakeOver("b".into())),
            "slop-sized wobble still clicks: {cmds:?}"
        );
    }

    #[test]
    fn dragging_a_tile_into_a_group_block_adds_it_at_the_pointer() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b", "c"]);
        make_group(&mut m, "web", &["a"]);
        // Drag the ungrouped "c" onto the RIGHT half of a's card: it joins
        // the group after "a".
        let from = centre(&tile_rect(&m, "c"));
        let a = tile_rect(&m, "a");
        let cmds = drag(&mut m, from, (a.x + a.w * 0.9, a.y + a.h / 2.0));
        assert_eq!(m.groups[0].members, vec!["a".to_string(), "c".to_string()]);
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::SaveGroups(_))),
            "membership changes persist: {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::TakeOver(_))),
            "a drag is not a click: {cmds:?}"
        );
        // Dragging "b" onto the LEFT half of a's card puts it before "a".
        let from = centre(&tile_rect(&m, "b"));
        let a = tile_rect(&m, "a");
        drag(&mut m, from, (a.x + a.w * 0.1, a.y + a.h / 2.0));
        assert_eq!(
            m.groups[0].members,
            vec!["b".to_string(), "a".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn dragging_within_a_group_reorders_its_members() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b", "c"]);
        make_group(&mut m, "web", &["a", "c"]);
        // Drag "c" onto the left half of "a": [c, a].
        let from = centre(&tile_rect(&m, "c"));
        let a = tile_rect(&m, "a");
        let cmds = drag(&mut m, from, (a.x + a.w * 0.1, a.y + a.h / 2.0));
        assert_eq!(m.groups[0].members, vec!["c".to_string(), "a".to_string()]);
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveGroups(_))));
    }

    #[test]
    fn dragging_a_member_out_removes_it_and_an_emptied_group_dissolves() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b", "c"]);
        make_group(&mut m, "web", &["a", "c"]);
        // Drop "c" onto empty space well below every block.
        let from = centre(&tile_rect(&m, "c"));
        let cmds = drag(&mut m, from, (WIDE.0 as f32 - 20.0, WIDE.1 as f32 - 10.0));
        assert_eq!(m.groups[0].members, vec!["a".to_string()]);
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveGroups(_))));
        // The last member leaving dissolves the group entirely.
        let from = centre(&tile_rect(&m, "a"));
        drag(&mut m, from, (WIDE.0 as f32 - 20.0, WIDE.1 as f32 - 10.0));
        assert!(m.groups.is_empty(), "an empty group has no reason to live");
    }

    #[test]
    fn a_drag_floats_the_card_under_the_pointer_and_escape_cancels() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b"]);
        let r = tile_rect(&m, "a");
        let (cx, cy) = centre(&r);
        pointer_phase(&mut m, PointerPhase::Press, cx, cy);
        pointer_phase(&mut m, PointerPhase::Motion, cx + 200.0, cy + 100.0);
        let scene = m.view();
        let floated = scene.layers[0].items.iter().any(|it| match it {
            SceneItem::Rect { rect, .. } => {
                // The card body follows the pointer, keeping the grab offset:
                // its centre sits under the pointer.
                (rect.x + rect.w / 2.0 - (cx + 200.0)).abs() < 1.0
                    && (rect.y + rect.h / 2.0 - (cy + 100.0)).abs() < 1.0
                    && rect.w == r.w
            }
            _ => false,
        });
        assert!(floated, "the dragged card floats under the pointer");
        assert!(m.consumes_escape(), "a live drag claims Escape");
        key(&mut m, Key::Named(NamedKey::Escape));
        let scene = m.view();
        let still_floating = scene.layers[0].items.iter().any(|it| {
            matches!(it,
            SceneItem::Rect { rect, .. } if (rect.x + rect.w / 2.0 - (cx + 200.0)).abs() < 1.0
                && rect.w == r.w && (rect.y + rect.h / 2.0 - (cy + 100.0)).abs() < 1.0)
        });
        assert!(!still_floating, "Escape cancels the drag");
        // The release after a cancelled drag does nothing.
        let cmds = pointer_phase(&mut m, PointerPhase::Release, cx + 200.0, cy + 100.0);
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::TakeOver(_))));
    }

    #[test]
    fn the_group_prompt_draws_a_scrim_and_the_typed_name_with_a_caret() {
        let mut m = fleet();
        list(&mut m, &["a"]);
        key(&mut m, Key::Named(NamedKey::Space));
        key(&mut m, Key::Char("g".into()));
        type_str(&mut m, "we");
        let scene = m.view();
        let items = &scene.layers[0].items;
        let (w, h) = (m.size_px.0 as f32, m.size_px.1 as f32);
        assert!(
            items
                .iter()
                .any(|it| matches!(it, SceneItem::Rect { rect, color, .. }
                if *color == OVERLAY_BG && rect.w == w && rect.h == h)),
            "the prompt scrims the whole grid"
        );
        let texts: Vec<(&str, f32)> = items
            .iter()
            .filter_map(|it| match it {
                SceneItem::Text { runs, scale, .. } => Some((runs[0].text.as_str(), *scale)),
                _ => None,
            })
            .collect();
        assert!(
            texts
                .iter()
                .any(|(t, s)| t.contains("Group name") && *s == MODAL_SCALE),
            "an emphasized prompt labels the modal; got {texts:?}"
        );
        assert!(
            texts.iter().any(|(t, _)| t.contains("we\u{2588}")),
            "the typed buffer renders with the caret; got {texts:?}"
        );
    }

    /// Mark `ids` and group them under `name`.
    fn make_group(m: &mut FleetModel, name: &str, ids: &[&str]) {
        for id in ids {
            let pos = centre_of(m, id);
            press_ctrl(m, pos);
        }
        key(m, Key::Char("g".into()));
        type_str(m, name);
        key(m, Key::Named(NamedKey::Enter));
    }

    #[test]
    fn grouped_tiles_render_in_their_groups_block_not_their_section() {
        let mut m = fleet();
        widen(&mut m);
        m.update(UiEvent::SessionList(vec![
            sinfo("a", true), // would be Elsewhere
            info("b"),        // detached
            info("c"),        // detached, ungrouped
        ]));
        make_group(&mut m, "web", &["a", "b"]);

        // The group block comes first, holding both members side by side even
        // though their attach states differ; "c" stays in its section below.
        let order = order(&m);
        assert_eq!(order, ["a", "b", "c"]);
        let (ya, yb, yc) = (tile_y(&m, "a"), tile_y(&m, "b"), tile_y(&m, "c"));
        assert_eq!(ya, yb, "group members share the block's row");
        assert!(yc > ya, "the ungrouped remainder renders below the group");

        // The view carries the group's name and its color-accented block.
        let scene = m.view();
        let has_label = scene.layers[0].items.iter().any(|it| {
            matches!(it, SceneItem::Text { runs, .. }
                if runs.iter().any(|r| r.text.contains("web")))
        });
        assert!(has_label, "the group block is labelled with its name");
        let accent = m.groups[0].rgba();
        let has_block_border = scene.layers[0]
            .items
            .iter()
            .any(|it| matches!(it, SceneItem::Border { color, .. } if *color == accent));
        assert!(has_block_border, "the block is outlined in the group color");
    }

    #[test]
    fn a_dead_member_keeps_its_groups_block_on_screen() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b"]);
        make_group(&mut m, "web", &["a"]);
        // "a" dies: the block stays, showing the dead-but-remembered tile.
        list(&mut m, &["b"]);
        assert_eq!(order(&m), ["a", "b"]);
        assert!(m.tiles.iter().find(|t| t.id == "a").unwrap().dead);
        assert_eq!(m.groups.len(), 1, "the group persists");
        // It returns (a recreate landed): the tile is live in its block again.
        list(&mut m, &["a", "b"]);
        assert!(!m.tiles.iter().find(|t| t.id == "a").unwrap().dead);
        let (ya, yb) = (tile_y(&m, "a"), tile_y(&m, "b"));
        assert!(ya < yb, "the group block renders above the sections");
    }

    #[test]
    fn group_colors_cycle_the_palette() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b"]);
        key(&mut m, Key::Named(NamedKey::Space));
        key(&mut m, Key::Char("g".into()));
        type_str(&mut m, "one");
        key(&mut m, Key::Named(NamedKey::Enter));
        let pos = centre_of(&m, "b");
        press_ctrl(&mut m, pos);
        key(&mut m, Key::Char("g".into()));
        type_str(&mut m, "two");
        key(&mut m, Key::Named(NamedKey::Enter));
        assert_eq!(m.groups.len(), 2);
        assert_ne!(
            m.groups[0].color, m.groups[1].color,
            "consecutive groups take distinct palette colors"
        );
    }

    #[test]
    fn marked_tiles_show_a_mark_ring() {
        let mut m = fleet();
        list(&mut m, &["a"]);
        key(&mut m, Key::Named(NamedKey::Space));
        let ring = m.view().layers[0]
            .items
            .iter()
            .any(|it| matches!(it, SceneItem::Border { color, .. } if *color == MARK_COLOR));
        assert!(ring, "a marked card carries the mark-colored ring");
    }

    #[test]
    fn a_tiles_card_follows_its_own_grids_aspect() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["a", "b"]);
        // "b"'s observed mirror reports a square-ish real grid: 60×30 at the
        // 9×18 test metrics is exactly 1:1, unlike the 80×24 default (5:3).
        push(
            &mut m,
            "b",
            SessionPush::Event(SessionEvent::Resized { cols: 60, rows: 30 }),
        );
        let (_, placements, band, _) = m.sections_layout();
        let rect = |id: &str| {
            placements
                .iter()
                .find(|(_, i, _)| i == id)
                .expect("tile placed")
                .2
        };
        let (ra, rb) = (rect("a"), rect("b"));
        assert_eq!(ra.h, rb.h, "cards share the row height");
        let aspect = |r: RectPx| r.w / (r.h - 2.0 * band);
        assert!(
            (aspect(rb) - 1.0).abs() < 0.05,
            "the square grid gets a square preview box, got {}",
            aspect(rb)
        );
        assert!(
            aspect(ra) > 1.5,
            "the default tile keeps the terminal aspect, got {}",
            aspect(ra)
        );
    }

    #[test]
    fn a_listed_size_shapes_the_tile_before_its_preview_lands() {
        // A dive-out freezes the fleet's layout the moment the listing
        // completes; the observers' first grid events land milliseconds later,
        // mid-animation. The listing carries each session's real grid, so a
        // tile is born with its true aspect and the observation changes
        // content, never geometry — otherwise the settled fleet wouldn't match
        // the frozen dive world (the end-of-dive layout jump).
        let mut m = fleet();
        widen(&mut m);
        m.update(UiEvent::SessionList(vec![SessionInfo {
            size: Some((120, 60)),
            ..info("a")
        }]));
        assert_eq!(
            tile(&m, "a").model.dims(),
            (120, 60),
            "the placeholder is born at the listed grid, not the 80×24 guess"
        );
        let born = tile_rect(&m, "a");
        // The observation's grid event confirms what the listing already said:
        // nothing moves, nothing repaints.
        let cmds = push(
            &mut m,
            "a",
            SessionPush::Event(SessionEvent::Resized {
                cols: 120,
                rows: 60,
            }),
        );
        assert_eq!(cmds, Vec::new(), "a confirming grid event is a no-op");
        assert_eq!(
            tile_rect(&m, "a"),
            born,
            "the grid must not reshuffle when the preview lands"
        );
    }

    #[test]
    fn the_fleet_observes_sessions_it_does_not_drive() {
        let mut m = FleetModel::new(METRICS, SIZE, HashSet::from(["a".to_string()]));
        let cmds = m.update(UiEvent::SessionList(vec![sinfo("a", true), info("b")]));
        assert!(
            cmds.contains(&Cmd::Observe("b".to_string())),
            "the foreign tile gets a live mirror; got {cmds:?}"
        );
        assert!(
            !cmds.contains(&Cmd::Observe("a".to_string())),
            "a driven session is already live — observing it would double-feed"
        );
        // A second reconcile doesn't re-observe.
        let cmds = m.update(UiEvent::SessionList(vec![sinfo("a", true), info("b")]));
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::Observe(_))));
    }

    #[test]
    fn a_vanished_session_is_unobserved() {
        let mut m = fleet();
        list(&mut m, &["b"]);
        let cmds = list(&mut m, &[]);
        assert!(cmds.contains(&Cmd::Unobserve("b".to_string())));
        // Re-listing it re-observes.
        let cmds = list(&mut m, &["b"]);
        assert!(cmds.contains(&Cmd::Observe("b".to_string())));
    }

    #[test]
    fn leaving_the_fleet_drops_every_observation() {
        let mut m = fleet();
        widen(&mut m);
        list(&mut m, &["b", "c"]);
        let (_, _, cmds) = m.into_single_keeping(None, WIDE, 1.0);
        assert!(cmds.contains(&Cmd::Unobserve("b".to_string())));
        assert!(cmds.contains(&Cmd::Unobserve("c".to_string())));
    }

    #[test]
    fn a_resized_push_regrids_an_observed_tile() {
        let mut m = fleet();
        list(&mut m, &["b"]);
        let cmds = push(
            &mut m,
            "b",
            SessionPush::Event(SessionEvent::Resized {
                cols: 100,
                rows: 30,
            }),
        );
        assert_eq!(tile(&m, "b").model.dims(), (100, 30));
        assert!(cmds.contains(&Cmd::Redraw));
    }

    #[test]
    fn an_ended_observation_reverts_the_tile_to_a_placeholder() {
        let mut m = fleet();
        list(&mut m, &["b"]);
        data(&mut m, "b", b"live");
        assert!(tile(&m, "b").fed);
        m.update(UiEvent::SessionData {
            name: "b".to_string(),
            bytes: vec![],
            ended: true,
        });
        assert!(!tile(&m, "b").fed, "a dead mirror is a placeholder again");
        // The next reconcile re-observes it (the session may still exist).
        let cmds = list(&mut m, &["b"]);
        assert!(cmds.contains(&Cmd::Observe("b".to_string())));
    }

    #[test]
    fn a_pushed_bell_badges_a_detached_tile_immediately() {
        let mut m = fleet();
        list(&mut m, &["a"]);
        let cmds = push(&mut m, "a", SessionPush::Event(SessionEvent::Bell));
        assert!(
            tile(&m, "a").bell,
            "the badge lights without a list refresh"
        );
        assert!(cmds.contains(&Cmd::Redraw));
    }

    #[test]
    fn a_bell_witnessed_by_an_attached_client_does_not_badge() {
        // Marker parity: a bell someone was attached to see is not an unseen
        // notification. The live *reaction* for the focused window is separate.
        let mut m = fleet();
        m.update(UiEvent::SessionList(vec![sinfo("a", true)]));
        let cmds = push(&mut m, "a", SessionPush::Event(SessionEvent::Bell));
        assert!(!tile(&m, "a").bell);
        assert!(cmds.is_empty());
    }

    #[test]
    fn attach_and_detach_events_rebucket_the_tile_without_a_refresh() {
        let mut m = fleet();
        list(&mut m, &["a"]);
        push(&mut m, "a", SessionPush::Event(SessionEvent::Bell));
        let cmds = push(
            &mut m,
            "a",
            SessionPush::Event(SessionEvent::Attached(AttachInfo { client: None })),
        );
        assert_eq!(tile(&m, "a").locality, Locality::Elsewhere);
        assert!(!tile(&m, "a").bell, "attaching witnesses the bell");
        assert!(cmds.contains(&Cmd::Redraw));

        let cmds = push(&mut m, "a", SessionPush::Event(SessionEvent::Detached));
        assert_eq!(tile(&m, "a").locality, Locality::Detached);
        assert!(cmds.contains(&Cmd::Redraw));
    }

    #[test]
    fn a_pushed_rename_relabels_the_card() {
        let mut m = fleet();
        list(&mut m, &["a"]);
        let cmds = push(
            &mut m,
            "a",
            SessionPush::Event(SessionEvent::Renamed("otter".to_string())),
        );
        assert_eq!(tile(&m, "a").model.display_name(), "otter");
        assert!(cmds.contains(&Cmd::Redraw));
    }

    #[test]
    fn pushed_activity_badges_only_unfocused_tiles() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]); // focus defaults to the first tile ("a")
        push(&mut m, "a", SessionPush::Event(SessionEvent::Activity));
        push(&mut m, "b", SessionPush::Event(SessionEvent::Activity));
        assert_eq!(tile(&m, "a").activity, 0, "the focused tile never badges");
        assert!(tile(&m, "b").activity > 0);
    }

    #[test]
    fn a_snapshot_refreshes_a_tiles_state() {
        let mut m = fleet();
        list(&mut m, &["a"]);
        let cmds = push(
            &mut m,
            "a",
            SessionPush::Snapshot(ghost_vt::protocol::SessionState {
                attached: Some(AttachInfo { client: None }),
                bell: false,
                title: String::new(),
                display_name: "box".to_string(),
            }),
        );
        assert_eq!(tile(&m, "a").locality, Locality::Elsewhere);
        assert_eq!(tile(&m, "a").model.display_name(), "box");
        assert!(cmds.contains(&Cmd::Redraw));
    }

    #[test]
    fn a_push_for_an_unlisted_session_is_ignored() {
        // The reconcile seeds tiles; a push racing ahead of it is dropped (the
        // subscription's snapshot re-arrives semantically via the list).
        let mut m = fleet();
        let cmds = push(&mut m, "ghost", SessionPush::Event(SessionEvent::Bell));
        assert!(cmds.is_empty());
    }

    #[test]
    fn within_a_section_tiles_order_by_creation_time_not_enumeration() {
        let mut m = fleet();
        // Enumerated in a scrambled order; creation time is the intended spatial
        // order, so the grid must not follow how the host happened to list them.
        m.update(UiEvent::SessionList(vec![
            info_at("c", 30),
            info_at("a", 10),
            info_at("b", 20),
        ]));
        assert_eq!(
            order(&m),
            vec!["a", "b", "c"],
            "oldest session first, regardless of enumeration order"
        );
        // A later reconcile in yet another order must not reshuffle the grid.
        m.update(UiEvent::SessionList(vec![
            info_at("b", 20),
            info_at("c", 30),
            info_at("a", 10),
        ]));
        assert_eq!(
            order(&m),
            vec!["a", "b", "c"],
            "ordering is stable across reconciles"
        );
        // A brand-new (newest) session lands at the end; existing tiles keep slots.
        m.update(UiEvent::SessionList(vec![
            info_at("d", 40),
            info_at("b", 20),
            info_at("a", 10),
            info_at("c", 30),
        ]));
        assert_eq!(order(&m), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn tied_creation_times_break_the_tie_by_name_not_enumeration() {
        // Sessions spawned in the same millisecond tie on `created_at`. The
        // tiebreak must be deterministic — the session name — so the grid order is
        // identical however the host happened to enumerate them. Every F9 /
        // dive-back builds a *fresh* fleet, so a tiebreak that depends on
        // enumeration (or a per-fleet handle assigned in enumeration order) lets
        // the tied tiles swap slots between rebuilds.
        //
        // Mirrors the real report: four sessions where the middle two share a
        // creation instant. The first and last (distinct times) stay put; the tied
        // pair must not swap.
        let build = |enumerated: &[(&str, i64)]| {
            let mut m = fleet();
            m.update(UiEvent::SessionList(
                enumerated.iter().map(|(n, t)| info_at(n, *t)).collect(),
            ));
            order(&m)
        };
        let want = vec!["s1", "s2", "s3", "s4"];
        // s2 and s3 share creation second 200; enumerated in creation order...
        assert_eq!(
            build(&[("s1", 100), ("s2", 200), ("s3", 200), ("s4", 300)]),
            want
        );
        // ...and scrambled, as a different directory-read order would deliver them:
        // the name tiebreak keeps s2 before s3 either way.
        assert_eq!(
            build(&[("s4", 300), ("s3", 200), ("s2", 200), ("s1", 100)]),
            want,
            "tied tiles must order by name, not by how the host listed them"
        );
    }

    #[test]
    fn tiles_are_split_into_attach_state_sections() {
        let mut m = FleetModel::new(METRICS, SIZE, HashSet::from(["a".to_string()]));
        m.update(UiEvent::SessionList(vec![
            sinfo("a", false), // ours -> Attached
            sinfo("b", true),  // attached elsewhere
            sinfo("c", false), // detached
        ]));
        assert_eq!(m.locality_of("a"), Some(Locality::ThisWindow));
        assert_eq!(m.locality_of("b"), Some(Locality::Elsewhere));
        assert_eq!(m.locality_of("c"), Some(Locality::Detached));
        // Three headers, in attach-state order, stacked top to bottom.
        let hs = headers(&m);
        let labels: Vec<&str> = hs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["Attached", "Attached elsewhere", "Detached"]);
        assert!(
            hs[0].1 < hs[1].1 && hs[1].1 < hs[2].1,
            "sections stack downward: {hs:?}"
        );
        // Each tile sits in its section's vertical band.
        assert!(tile_y(&m, "a") < tile_y(&m, "b"));
        assert!(tile_y(&m, "b") < tile_y(&m, "c"));
    }

    #[test]
    fn only_nonempty_sections_get_a_header() {
        let mut m = fleet(); // empty mine
        list(&mut m, &["x", "y"]); // both detached
        let labels: Vec<String> = headers(&m).into_iter().map(|(l, _)| l).collect();
        assert_eq!(labels, vec!["Detached".to_string()]);
    }

    #[test]
    fn arrow_down_moves_to_the_tile_below_not_the_next() {
        let mut m = fleet();
        list(&mut m, &["a", "b", "c", "d"]); // one section, 2x2 grid: a b / c d
        widen(&mut m);
        assert_eq!(m.focused(), Some("a")); // top-left
        key(&mut m, Key::Named(NamedKey::ArrowDown));
        assert_eq!(
            m.focused(),
            Some("c"),
            "Down moves to the tile below, not the next in order"
        );
        // Right is the horizontal neighbour.
        let mut m = fleet();
        list(&mut m, &["a", "b", "c", "d"]);
        widen(&mut m);
        key(&mut m, Key::Named(NamedKey::ArrowRight));
        assert_eq!(m.focused(), Some("b"));
    }

    #[test]
    fn arrow_down_crosses_into_the_next_section() {
        let mut m = FleetModel::new(
            METRICS,
            SIZE,
            HashSet::from(["a1".to_string(), "a2".to_string()]),
        );
        m.update(UiEvent::SessionList(vec![
            sinfo("a1", false),
            sinfo("a2", false),
            sinfo("d1", false),
            sinfo("d2", false),
        ]));
        widen(&mut m);
        assert_eq!(m.focused(), Some("a1"));
        assert_eq!(m.locality_of("a1"), Some(Locality::ThisWindow));
        key(&mut m, Key::Named(NamedKey::ArrowDown));
        assert_eq!(
            m.locality_of(m.focused().unwrap()),
            Some(Locality::Detached),
            "Down from the attached row enters the detached section"
        );
    }

    #[test]
    fn only_the_tile_that_changed_rebuilds_its_preview_frame() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        // Both tiles laid out once by the reconcile; baseline the counter there.
        let base = m.frame_builds();

        // Output to one tile rebuilds exactly that tile's frame.
        data(&mut m, "a", b"hello");
        assert_eq!(m.frame_builds(), base + 1, "only tile a is re-laid-out");

        // Rendering reads cached frames — it never re-lays-out.
        let _ = m.view();
        let _ = m.view();
        assert_eq!(m.frame_builds(), base + 1, "view reuses cached frames");

        // A nav keypress changes focus, not content: no frame is rebuilt.
        key(&mut m, Key::Named(NamedKey::ArrowRight));
        assert_eq!(m.frame_builds(), base + 1, "focus change rebuilds nothing");

        // Output to the other tile rebuilds only it.
        data(&mut m, "b", b"world");
        assert_eq!(m.frame_builds(), base + 2);
    }

    #[test]
    fn an_unchanged_tile_is_a_frame_cache_hit_not_a_rebuild() {
        // The frame cache expressed on hit/miss counters (the `RUST_LOG=ghost::cache`
        // view and the general regression guard): when one tile changes, the others
        // must register as hits, not rebuilds. A change that over-invalidates the
        // fleet — re-laying-out unchanged tiles — shows up here as misses > 1.
        let mut m = fleet();
        list(&mut m, &["a", "b", "c"]);
        let base = m.frame_cache();

        data(&mut m, "a", b"hello");
        let d = m.frame_cache().since(base);
        assert_eq!(
            d.misses, 1,
            "only the changed tile re-lays-out (misses={})",
            d.misses
        );
        assert!(
            d.hits >= 2,
            "the two unchanged tiles are cache hits, not rebuilds (hits={})",
            d.hits
        );
        // For that one-tile change, most tile lookups were served from cache.
        assert!(
            d.hit_rate() > 0.5,
            "a one-tile change should be mostly hits, got {:.2}",
            d.hit_rate()
        );
    }

    #[test]
    fn a_scale_change_re_lays_out_every_tile() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        let base = m.frame_builds();
        // A DPI change alters effective metrics for all tiles even if grids match.
        m.update(UiEvent::Resize {
            w_px: SIZE.0,
            h_px: SIZE.1,
            scale: 2.0,
        });
        assert_eq!(
            m.frame_builds(),
            base + 2,
            "both previews re-laid-out on rescale"
        );
    }

    fn rects_overlap(a: &RectPx, b: &RectPx) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    #[test]
    fn reconcile_creates_tiles_and_detaches_gone() {
        let mut m = fleet();
        let cmds = list(&mut m, &["a", "b"]);
        // The fleet never attaches sessions itself.
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::Attach(_))));
        assert_eq!(m.tile_count(), 2);
        assert_eq!(m.focused(), Some("a")); // first tile focused by default

        let cmds = list(&mut m, &["a"]);
        assert!(cmds.contains(&Cmd::Detach("b".into())));
        assert_eq!(m.tile_count(), 1);
    }

    #[test]
    fn the_fleet_never_resizes_a_previewed_session() {
        let mut m = fleet();
        let cmds = list(&mut m, &["a", "b"]);
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Resize { .. })),
            "reconcile must not resize sessions: {cmds:?}"
        );
        // A window resize repaints but never resizes the previewed sessions.
        let cmds = m.update(UiEvent::Resize {
            w_px: 800,
            h_px: 600,
            scale: 1.0,
        });
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::Resize { .. })));
        assert!(cmds.contains(&Cmd::Redraw));
    }

    #[test]
    fn the_dive_target_matches_the_session_aspect_not_the_card_box() {
        // A window-sized session has the window's aspect (1400:900 ≈ 1.56), which is
        // not the card's fixed 80×24 preview-box aspect (≈ 1.67).
        let win = (1400u32, 900u32);
        let mut primary = TerminalModel::new("alpha".to_string(), 80, 24, METRICS);
        primary.update(UiEvent::Resize {
            w_px: win.0,
            h_px: win.1,
            scale: 1.0,
        });
        let (cols, rows) = primary.dims();
        let session_aspect = (cols as f32 * METRICS.advance) / (rows as f32 * METRICS.line_height);
        let mine = HashSet::from(["alpha".to_string()]);
        let (f, _) = FleetModel::adopting(primary, Vec::new(), METRICS, win, 1.0, mine);

        let aspect = |r: RectPx| r.w / r.h;
        let target = f.dive_target_rect("alpha").expect("the tile is present");
        let preview = f.preview_rect("alpha").expect("the tile is present");

        // The dive aims at where the content is actually drawn, so a cover-zoom lands
        // the session at native size (matching the live single view) — no boundary pop.
        assert!(
            (aspect(target) - session_aspect).abs() < 0.02,
            "dive target aspect {} should match the session aspect {session_aspect}",
            aspect(target)
        );
        // And the preview BOX itself now follows the session's aspect (cards are
        // shaped by their own grid), so the contain-fit is exact — no letterbox.
        assert!(
            (aspect(preview) - aspect(target)).abs() < 0.02,
            "the preview box matches the target's aspect ({} vs {})",
            aspect(preview),
            aspect(target)
        );
        // It is the frame's drawn sub-rect: same top-left, contained within the box.
        assert!(
            target.x == preview.x
                && target.y == preview.y
                && target.w <= preview.w + 0.5
                && target.h <= preview.h + 0.5,
            "the target is the contain-fit sub-rect of the preview box"
        );
    }

    #[test]
    fn an_unfed_tile_is_a_placeholder_and_a_fed_tile_is_a_live_preview() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        // Neither has produced output: both render as placeholders, no Terminal.
        assert_eq!(
            m.view().terminals().count(),
            0,
            "unfed tiles are placeholders"
        );
        // Feeding output to "a" promotes it to a live Terminal preview.
        data(&mut m, "a", b"hello");
        assert_eq!(
            m.view().terminals().count(),
            1,
            "only the fed tile is a live preview"
        );
    }

    #[test]
    fn view_lays_tiles_in_a_non_overlapping_grid_with_one_focus_border() {
        let mut m = fleet();
        list(&mut m, &["a", "b", "c"]);
        // A viewport tall enough to show all three readable tiles (otherwise the
        // grid scrolls and culls the off-screen ones — exercised separately).
        const TALL: (u32, u32) = (400, 1000);
        m.update(UiEvent::Resize {
            w_px: TALL.0,
            h_px: TALL.1,
            scale: 1.0,
        });
        // Distinct content per tile, so each previews real routed output.
        data(&mut m, "a", b"AAA");
        data(&mut m, "b", b"BBB");
        data(&mut m, "c", b"CCC");

        let scene = m.view();
        let items = &scene.layers[0].items;

        // One Terminal preview per session.
        let terminals: Vec<RectPx> = items
            .iter()
            .filter_map(|it| match it {
                SceneItem::Terminal { rect, .. } => Some(*rect),
                _ => None,
            })
            .collect();
        assert_eq!(terminals.len(), 3, "one preview tile per session");

        // Tiles tile the grid — no two overlap, and each fits the viewport.
        for (i, a) in terminals.iter().enumerate() {
            assert!(
                a.x >= 0.0
                    && a.y >= 0.0
                    && a.x + a.w <= TALL.0 as f32
                    && a.y + a.h <= TALL.1 as f32,
                "tile {a:?} must fit the {TALL:?} viewport"
            );
            for b in &terminals[i + 1..] {
                assert!(
                    !rects_overlap(a, b),
                    "tiles must not overlap: {a:?} vs {b:?}"
                );
            }
        }

        // Exactly one tile is focused: the only one bordered and the only one not
        // dimmed, and the border tracks that tile's rect.
        let borders: Vec<RectPx> = items
            .iter()
            .filter_map(|it| match it {
                SceneItem::Border { rect, .. } => Some(*rect),
                _ => None,
            })
            .collect();
        assert_eq!(borders.len(), 1, "only the focused tile is bordered");
        let undimmed: Vec<RectPx> = items
            .iter()
            .filter_map(|it| match it {
                SceneItem::Terminal {
                    rect, dim: false, ..
                } => Some(*rect),
                _ => None,
            })
            .collect();
        assert_eq!(undimmed.len(), 1, "exactly one focused (undimmed) tile");
        // The border outlines the whole focused card; its live preview sits
        // inside that card (the preview is a sub-rect, below the metadata header).
        let (b, t) = (borders[0], undimmed[0]);
        assert!(
            t.x >= b.x && t.y >= b.y && t.x + t.w <= b.x + b.w && t.y + t.h <= b.y + b.h,
            "the focused preview {t:?} must sit within its card border {b:?}"
        );
    }

    #[test]
    fn reconcile_schedules_a_refresh_tick() {
        let mut m = fleet();
        let cmds = list(&mut m, &["a"]);
        assert!(cmds.contains(&Cmd::ScheduleTick {
            after_ms: REFRESH_MS
        }));
        // The refresh tick asks for a fresh listing.
        assert_eq!(
            m.update(UiEvent::Tick { now_ms: 1 }),
            vec![Cmd::ListSessions]
        );
    }

    #[test]
    fn session_data_routes_to_its_tile() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        data(&mut m, "a", b"hello");
        assert!(m.tile_text("a").unwrap()[0].starts_with("hello"));
        assert!(m.tile_text("b").unwrap()[0].trim().is_empty());
    }

    #[test]
    fn tile_title_change_does_not_retitle_the_window() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        // A tile setting its OSC title must not drive the overview window title.
        let cmds = data(&mut m, "a", b"\x1b]2;tile-a\x07");
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::SetTitle(_))),
            "the fleet overview does not retitle the window for a tile"
        );
    }

    #[test]
    fn previews_are_read_only() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]); // focus defaults to "a"
        // Text and ordinary keys must NOT reach a tile as input — fleet previews
        // are view-only, so neither forwards anything to the focused session.
        assert_eq!(
            m.update(UiEvent::Text("x".into())),
            vec![],
            "typed text is not forwarded to a preview"
        );
        assert_eq!(
            key(&mut m, Key::Char("a".into())),
            vec![],
            "ordinary keys are not forwarded to a preview"
        );
    }

    #[test]
    fn arrow_moves_focus_without_sending_input() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        widen(&mut m);
        assert_eq!(m.focused(), Some("a"));
        let cmds = key(&mut m, Key::Named(NamedKey::ArrowRight));
        assert_eq!(m.focused(), Some("b"));
        assert_eq!(cmds, vec![Cmd::Redraw]); // focus only, nothing forwarded
        // Clamped at the end (no wrap on arrows).
        key(&mut m, Key::Named(NamedKey::ArrowRight));
        assert_eq!(m.focused(), Some("b"));
        key(&mut m, Key::Named(NamedKey::ArrowLeft));
        assert_eq!(m.focused(), Some("a"));
    }

    /// Click (press + release in place) at the centre of `id`'s tile.
    fn press(m: &mut FleetModel, id: &str) -> Vec<Cmd> {
        let (_, _, rect) = m.layout().into_iter().find(|(_, i, _)| i == id).unwrap();
        press_at(m, rect.x + rect.w / 2.0, rect.y + rect.h / 2.0)
    }

    /// Click (press + release in place) at `(x, y)`. Returns the release's
    /// commands — where click actions live now that a press may become a drag.
    fn press_at(m: &mut FleetModel, x: f32, y: f32) -> Vec<Cmd> {
        pointer_phase(m, PointerPhase::Press, x, y);
        pointer_phase(m, PointerPhase::Release, x, y)
    }

    fn pointer_phase(m: &mut FleetModel, phase: PointerPhase, x: f32, y: f32) -> Vec<Cmd> {
        m.update(UiEvent::Pointer {
            phase,
            button: Some(crate::PointerButton::Left),
            pos: PointPx {
                x: x as f64,
                y: y as f64,
            },
            mods: crate::Mods::NONE,
            wheel_dy: 0.0,
            clicks: 1,
        })
    }

    /// Drag from `(fx, fy)` to `(tx, ty)` (press, motion, release), returning
    /// the release's commands.
    fn drag(m: &mut FleetModel, (fx, fy): (f32, f32), (tx, ty): (f32, f32)) -> Vec<Cmd> {
        pointer_phase(m, PointerPhase::Press, fx, fy);
        pointer_phase(m, PointerPhase::Motion, tx, ty);
        pointer_phase(m, PointerPhase::Release, tx, ty)
    }

    fn centre(r: &RectPx) -> (f32, f32) {
        (r.x + r.w / 2.0, r.y + r.h / 2.0)
    }

    fn tile_rect(m: &FleetModel, id: &str) -> RectPx {
        m.layout()
            .into_iter()
            .find(|(_, i, _)| i == id)
            .map(|(_, _, r)| r)
            .unwrap()
    }

    /// The pixel rect of `id`'s `button` (centre is a good press target).
    fn button_rect(m: &FleetModel, id: &str, button: Button) -> RectPx {
        let (_, placements, band, _) = m.sections_layout();
        let (_, _, rect) = placements.into_iter().find(|(_, i, _)| i == id).unwrap();
        let (_, _, buttons) = card_layout(rect, band);
        buttons.iter().find(|(b, _)| *b == button).unwrap().1
    }

    #[test]
    fn clicking_a_detached_tile_focuses_and_opens_it() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]); // both detached
        let cmds = press(&mut m, "b");
        assert_eq!(m.focused(), Some("b"));
        assert!(
            cmds.contains(&Cmd::TakeOver("b".into())),
            "clicking a detached tile opens it: {cmds:?}"
        );
    }

    #[test]
    fn clicking_the_detach_button_detaches_instead_of_opening() {
        let mut m = FleetModel::new(METRICS, SIZE, HashSet::from(["b".to_string()]));
        m.update(UiEvent::SessionList(vec![info("a"), sinfo("b", true)]));
        let r = button_rect(&m, "b", Button::Detach);
        let cmds = press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert!(
            cmds.contains(&Cmd::Detach("b".into())),
            "the detach button detaches: {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::TakeOver(_))),
            "a button press must not also open the tile"
        );
    }

    #[test]
    fn the_kill_button_confirms_then_kills() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        let r = button_rect(&m, "b", Button::Kill);
        let cmds = press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Kill(_))),
            "kill is confirmed, not immediate: {cmds:?}"
        );
        let cmds = key(&mut m, Key::Named(NamedKey::Space));
        assert!(
            cmds.contains(&Cmd::Kill("b".into())),
            "Space confirms the kill: {cmds:?}"
        );
    }

    #[test]
    fn escape_cancels_a_pending_confirmation() {
        let mut m = fleet();
        list(&mut m, &["a"]);
        let r = button_rect(&m, "a", Button::Kill);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        let cmds = key(&mut m, Key::Named(NamedKey::Escape));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Kill(_))),
            "Esc cancels the confirmation: {cmds:?}"
        );
    }

    #[test]
    fn the_rename_button_opens_an_inline_edit_committed_on_enter() {
        let mut m = fleet();
        list(&mut m, &["a"]);
        let r = button_rect(&m, "a", Button::Rename);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        // Typing buffers into the rename; nothing is committed yet.
        let cmds = m.update(UiEvent::Text("X".into()));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Rename { .. })),
            "rename buffers until Enter: {cmds:?}"
        );
        // Enter commits the edited name.
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert!(
            cmds.contains(&Cmd::Rename {
                session: "a".into(),
                name: "aX".into()
            }),
            "Enter commits the rename: {cmds:?}"
        );
    }

    #[test]
    fn a_rename_accepts_characters_typed_through_the_key_channel() {
        let mut m = fleet();
        list(&mut m, &["a"]);
        let r = button_rect(&m, "a", Button::Rename);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        // Ordinary typing reaches the core as `UiEvent::Key`/`Key::Char` — the shell
        // only synthesizes `UiEvent::Text` for IME commits — so the editor must grow
        // its buffer from key presses, not just Text. Otherwise a name can be deleted
        // (Backspace is a Named key) but never typed.
        let cmds = key(&mut m, Key::Char("X".into()));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Rename { .. })),
            "typing buffers until Enter: {cmds:?}"
        );
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert!(
            cmds.contains(&Cmd::Rename {
                session: "a".into(),
                name: "aX".into()
            }),
            "a typed character must reach the rename buffer: {cmds:?}"
        );
    }

    #[test]
    fn a_modifier_chord_does_not_type_into_a_rename() {
        let mut m = fleet();
        list(&mut m, &["a"]);
        let r = button_rect(&m, "a", Button::Rename);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        // Ctrl-/Cmd- chords are shortcuts, not text: they must not land in the buffer,
        // or e.g. Ctrl-C would append a stray "c" to the name.
        for mods in [crate::Mods::CTRL, crate::Mods::SUPER] {
            m.update(UiEvent::Key {
                key: Key::Char("c".into()),
                mods,
                kind: KeyEventKind::Press,
                alts: None,
            });
        }
        // The buffer is still the original name, so Enter commits nothing.
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Rename { .. })),
            "a modifier chord must not type into the rename: {cmds:?}"
        );
    }

    fn key_mods(m: &mut FleetModel, k: Key, mods: crate::Mods) -> Vec<Cmd> {
        m.update(UiEvent::Key {
            key: k,
            mods,
            kind: KeyEventKind::Press,
            alts: None,
        })
    }

    #[test]
    fn f2_opens_an_inline_rename_of_the_focused_tile() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        // Focus the first tile with the keyboard, then rename it with F2 — no
        // pointer trip to the card's rename button required.
        key(&mut m, Key::Named(NamedKey::ArrowRight));
        key(&mut m, Key::Named(NamedKey::F2));
        key(&mut m, Key::Char("X".into()));
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert!(
            cmds.contains(&Cmd::Rename {
                session: "a".into(),
                name: "aX".into()
            }),
            "F2 opens the focused tile's rename: {cmds:?}"
        );
    }

    #[test]
    fn f2_in_an_empty_fleet_is_a_no_op() {
        // The reconcile defaults focus to the visually-first tile, so the only
        // focusless fleet is an empty one; F2 must not open a rename there.
        let mut m = fleet();
        list(&mut m, &[]);
        key(&mut m, Key::Named(NamedKey::F2));
        key(&mut m, Key::Char("X".into()));
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Rename { .. })),
            "F2 with nothing to rename must not open a rename: {cmds:?}"
        );
    }

    #[test]
    fn a_rename_supports_word_navigation_and_editing() {
        let mut m = fleet();
        let mut i = info("sess-1");
        i.display_name = "build box".into();
        m.update(UiEvent::SessionList(vec![i]));
        let r = button_rect(&m, "sess-1", Button::Rename);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        // Alt-Backspace deletes the trailing word: "build box" -> "build ".
        key_mods(&mut m, Key::Named(NamedKey::Backspace), crate::Mods::ALT);
        key(&mut m, Key::Char("web".into())); // "build web", cursor at end
        // Alt-Left steps back over "web"; typing lands at the cursor.
        key_mods(&mut m, Key::Named(NamedKey::ArrowLeft), crate::Mods::ALT);
        key(&mut m, Key::Char("cob".into())); // "build cobweb"
        // The caret block renders at the cursor, not glued to the end.
        let texts = view_texts(&m);
        assert!(
            texts.iter().any(|t| t.contains("build cob\u{2588}web")),
            "the caret follows the cursor into the text: {texts:?}"
        );
        // Home + Delete drops the leading character: "uild cobweb".
        key(&mut m, Key::Named(NamedKey::Home));
        key(&mut m, Key::Named(NamedKey::Delete));
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert!(
            cmds.contains(&Cmd::Rename {
                session: "sess-1".into(),
                name: "uild cobweb".into()
            }),
            "word ops edit at the cursor: {cmds:?}"
        );
    }

    /// All card/header text currently in the view.
    fn view_texts(m: &FleetModel) -> Vec<String> {
        m.view().layers[0]
            .items
            .iter()
            .filter_map(|it| match it {
                SceneItem::Text { runs, .. } => {
                    Some(runs.iter().map(|r| r.text.as_str()).collect())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn cards_show_the_display_name_not_the_id() {
        let mut m = fleet();
        let mut i = info("sess-1");
        i.display_name = "build box".into();
        m.update(UiEvent::SessionList(vec![i]));
        let texts = view_texts(&m);
        assert!(
            texts.iter().any(|t| t.contains("build box")),
            "the card header shows the display name: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("sess-1")),
            "the immutable id is plumbing, not card text: {texts:?}"
        );
    }

    #[test]
    fn rename_edits_the_display_name_and_commits_against_the_id() {
        let mut m = fleet();
        let mut i = info("sess-1");
        i.display_name = "old label".into();
        m.update(UiEvent::SessionList(vec![i]));
        let r = button_rect(&m, "sess-1", Button::Rename);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        // The edit starts from the current display name, not the internal id.
        let texts = view_texts(&m);
        assert!(
            texts.iter().any(|t| t.contains("old label\u{2588}")),
            "the rename buffer seeds with the display name: {texts:?}"
        );
        // Committing the unchanged display name is a no-op.
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Rename { .. })),
            "an unchanged display name commits nothing: {cmds:?}"
        );
        // Editing and committing targets the session by its immutable id.
        let r = button_rect(&m, "sess-1", Button::Rename);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        key(&mut m, Key::Char("2".into()));
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert!(
            cmds.contains(&Cmd::Rename {
                session: "sess-1".into(),
                name: "old label2".into()
            }),
            "the rename addresses the id and carries the new display name: {cmds:?}"
        );
        // The card reflects the new display name immediately, without waiting
        // for the next reconcile.
        let texts = view_texts(&m);
        assert!(
            texts.iter().any(|t| t.contains("old label2")),
            "the committed display name shows immediately: {texts:?}"
        );
    }

    #[test]
    fn an_ime_composition_does_not_double_type_into_a_rename() {
        let mut m = fleet();
        list(&mut m, &["a"]);
        let r = button_rect(&m, "a", Button::Rename);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        // During IME composition the shell delivers Preedit for the in-progress text,
        // the raw driving keystroke as Key::Char, AND the final Text commit. Only the
        // committed result must land — appending the raw key too would garble the name
        // (e.g. "n" + "你" -> "n你"). Mirrors the terminal's preedit suppression.
        m.update(UiEvent::Preedit("n".into())); // composition in progress
        m.update(UiEvent::Key {
            key: Key::Char("n".into()),
            mods: crate::Mods::NONE,
            kind: KeyEventKind::Press,
            alts: None,
        }); // raw key while composing — must be swallowed
        m.update(UiEvent::Text("\u{4f60}".into())); // commit "你"
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert!(
            cmds.contains(&Cmd::Rename {
                session: "a".into(),
                name: "a\u{4f60}".into()
            }),
            "only committed IME text lands in the rename, not the raw keys: {cmds:?}"
        );
    }

    #[test]
    fn opening_an_elsewhere_tile_asks_for_confirmation_first() {
        let mut m = fleet();
        let mut a = info("a");
        a.attached = true; // attached by another window
        m.update(UiEvent::SessionList(vec![a]));
        assert_eq!(m.locality_of("a"), Some(Locality::Elsewhere));
        let cmds = press(&mut m, "a");
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::TakeOver(_))),
            "must confirm before stealing an elsewhere session: {cmds:?}"
        );
        // Confirming with Space issues the take-over (Enter would pick the
        // selected button, which starts on the safe Cancel).
        let cmds = key(&mut m, Key::Named(NamedKey::Space));
        assert!(
            cmds.contains(&Cmd::TakeOver("a".into())),
            "Space confirms the take-over: {cmds:?}"
        );
    }

    #[test]
    fn enter_chooses_the_selected_confirm_button() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        let r = button_rect(&m, "b", Button::Kill);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        // Cancel is pre-selected (the safe default), so plain Enter dismisses.
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Kill(_))),
            "Enter on the default (cancel) selection must not kill: {cmds:?}"
        );
        assert!(!m.modal_open(), "Enter on cancel dismisses the modal");
        // Reopen; the arrows move the selection onto the confirm button.
        let r = button_rect(&m, "b", Button::Kill);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        key(&mut m, Key::Named(NamedKey::ArrowLeft));
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert!(
            cmds.contains(&Cmd::Kill("b".into())),
            "Enter chooses the selected button: {cmds:?}"
        );
    }

    #[test]
    fn space_confirms_a_pending_action_directly() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        let r = button_rect(&m, "b", Button::Kill);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        // Space is the direct confirm chord (Enter picks the selected button).
        let cmds = key(&mut m, Key::Named(NamedKey::Space));
        assert!(
            cmds.contains(&Cmd::Kill("b".into())),
            "Space confirms directly: {cmds:?}"
        );
    }

    #[test]
    fn a_rename_accepts_spaces() {
        let mut m = fleet();
        list(&mut m, &["a"]);
        let r = button_rect(&m, "a", Button::Rename);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        // The space bar arrives as a Named key, not a Char — it must still type.
        key(&mut m, Key::Named(NamedKey::Space));
        key(&mut m, Key::Char("b".into()));
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert!(
            cmds.contains(&Cmd::Rename {
                session: "a".into(),
                name: "a b".into()
            }),
            "a space types into the rename buffer: {cmds:?}"
        );
    }

    /// The confirm chips' rects, read from the view by their colours; the
    /// confirm chip's expected colour depends on the pending action.
    fn chip_rects(m: &FleetModel, confirm_bg: Rgba) -> (RectPx, RectPx) {
        let items = m.view().layers[0].items.clone();
        let find = |color: Rgba| {
            items
                .iter()
                .find_map(|it| match it {
                    SceneItem::Rect { rect, color: c, .. } if *c == color => Some(*rect),
                    _ => None,
                })
                .expect("a confirm chip with the expected colour")
        };
        (find(confirm_bg), find(CANCEL_BUTTON_BG))
    }

    #[test]
    fn a_takeover_confirmation_uses_a_green_confirm_button() {
        let mut m = fleet();
        let mut a = info("a");
        a.attached = true; // held by another window -> take-over confirm
        m.update(UiEvent::SessionList(vec![a]));
        press(&mut m, "a");
        // A take-over is a simple confirmation, not destruction: green, with
        // the grey cancel beside it (chip_rects panics if either is missing).
        chip_rects(&m, AFFIRM_BUTTON_BG);
    }

    #[test]
    fn the_confirm_modal_emphasizes_text_and_shows_choice_buttons() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        let r = button_rect(&m, "b", Button::Kill);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        let scene = m.view();
        let items = &scene.layers[0].items;
        // The question renders 50% over the terminal font, centred.
        let msg = items
            .iter()
            .find_map(|it| match it {
                SceneItem::Text {
                    rect, scale, runs, ..
                } if runs.iter().any(|r| r.text.starts_with("Kill")) => Some((*rect, *scale)),
                _ => None,
            })
            .expect("the confirm question is in the view");
        assert_eq!(msg.1, MODAL_SCALE, "modal text is emphasized");
        // Red confirm and green cancel chips sit on the line below the
        // question, and the safe cancel is pre-selected (focus ring).
        let (confirm, cancel) = chip_rects(&m, DESTRUCTIVE_BUTTON_BG);
        assert!(
            confirm.y > msg.0.y && cancel.y > msg.0.y,
            "the buttons sit under the question"
        );
        assert!(
            confirm.x < cancel.x,
            "confirm left, cancel right: {confirm:?} {cancel:?}"
        );
        assert!(
            items
                .iter()
                .any(|it| matches!(it, SceneItem::Border { rect, .. } if *rect == cancel)),
            "the selected (cancel) chip carries the focus ring"
        );
        // Moving the selection moves the ring.
        key(&mut m, Key::Named(NamedKey::ArrowLeft));
        let scene = m.view();
        assert!(
            scene.layers[0]
                .items
                .iter()
                .any(|it| matches!(it, SceneItem::Border { rect, .. } if *rect == confirm)),
            "the arrows move the focus ring to the confirm chip"
        );
    }

    #[test]
    fn the_confirm_buttons_are_clickable() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        let r = button_rect(&m, "b", Button::Kill);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        let (confirm, cancel) = chip_rects(&m, DESTRUCTIVE_BUTTON_BG);
        // Clicking cancel dismisses without killing.
        let cmds = press_at(&mut m, cancel.x + cancel.w / 2.0, cancel.y + cancel.h / 2.0);
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Kill(_))),
            "clicking cancel must not kill: {cmds:?}"
        );
        assert!(!m.modal_open(), "clicking cancel dismisses the modal");
        // Reopen and click the confirm chip: the action runs.
        let r = button_rect(&m, "b", Button::Kill);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        let cmds = press_at(
            &mut m,
            confirm.x + confirm.w / 2.0,
            confirm.y + confirm.h / 2.0,
        );
        assert!(
            cmds.contains(&Cmd::Kill("b".into())),
            "clicking the confirm chip kills: {cmds:?}"
        );
    }

    #[test]
    fn enter_opens_the_focused_tile() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]); // focus defaults to "a" (detached)
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert!(
            cmds.contains(&Cmd::TakeOver("a".into())),
            "Enter opens the focused tile: {cmds:?}"
        );
    }

    #[test]
    fn view_dims_unfocused_tiles_and_borders_the_focused_one() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]); // focus "a"
        widen(&mut m); // both tiles on one visible row (else the 2nd scrolls off)
        data(&mut m, "a", b"A"); // make both tiles live previews
        data(&mut m, "b", b"B");
        let scene = m.view();
        let terminals: Vec<_> = scene.terminals().collect();
        assert_eq!(terminals.len(), 2);
        let dimmed = terminals
            .iter()
            .filter(|t| matches!(t, SceneItem::Terminal { dim: true, .. }))
            .count();
        assert_eq!(dimmed, 1, "exactly the one unfocused tile is dimmed");
        // The focused tile carries a border item.
        let borders = scene.layers[0]
            .items
            .iter()
            .filter(|it| matches!(it, SceneItem::Border { .. }))
            .count();
        assert_eq!(borders, 1);
    }

    #[test]
    fn bell_and_background_activity_raise_badges() {
        let mut m = fleet();
        widen(&mut m); // keep both tiles on one visible row
        let mut infos = vec![info("a"), info("b")];
        infos[1].bell = true; // "b" rang the bell
        m.update(UiEvent::SessionList(infos));
        let badges = |m: &FleetModel| {
            m.view().layers[0]
                .items
                .iter()
                .filter(|it| matches!(it, SceneItem::Badge { .. }))
                .count()
        };
        assert_eq!(badges(&m), 1, "bell raises a badge on b");

        // Background output on b (focus is on a) raises an activity badge even
        // without a bell.
        let mut m = fleet();
        list(&mut m, &["a", "b"]); // focus a
        widen(&mut m);
        data(&mut m, "b", b"work");
        assert_eq!(badges(&m), 1);
        // Focusing b clears its activity badge.
        key(&mut m, Key::Named(NamedKey::ArrowRight));
        assert_eq!(m.focused(), Some("b"));
        assert_eq!(badges(&m), 0);
    }

    #[test]
    fn a_sparse_grid_floats_to_the_vertical_centre() {
        let mut m = fleet();
        widen(&mut m); // 1000x700: one card cannot fill it
        list(&mut m, &["a"]);
        let (headers, placements, _, _) = m.sections_layout();
        let top = headers[0].1.y;
        let card = placements[0].2;
        let bottom = WIDE.1 as f32 - (card.y + card.h);
        assert!(
            top > GAP + 1.0,
            "a lone card does not hug the top: header at y={top}"
        );
        assert!(
            (top - bottom).abs() <= GAP + 1.0,
            "the block is vertically centred: {top} above vs {bottom} below"
        );
        // A crowded, scrolling grid still starts at the top.
        list_many(&mut m, 30);
        let (headers, _, _, content_h) = m.sections_layout();
        assert!(content_h > WIDE.1 as f32, "precondition: it overflows");
        assert!(
            (headers[0].1.y - GAP).abs() < 1.0,
            "an overflowing grid starts at the top"
        );
    }

    #[test]
    fn card_metadata_shows_the_working_directory() {
        let mut m = fleet();
        widen(&mut m);
        m.update(UiEvent::SessionList(vec![SessionInfo {
            cwd: Some("~/Projects/ghost".into()),
            ..info("a")
        }]));
        let scene = m.view();
        assert!(
            scene.layers[0].items.iter().any(|it| matches!(it,
                SceneItem::Text { runs, .. }
                    if runs[0].text.contains("~/Projects/ghost"))),
            "the card meta line carries the session's cwd"
        );
        // Between the command and the pid, before any progress tail.
        assert_eq!(
            card_meta(
                "a",
                &["vim".into()],
                12,
                Some("~/x".into()),
                Some(ghost_term::Progress::Normal(3)),
            ),
            "a \u{b7} vim \u{b7} ~/x \u{b7} 12 \u{b7} 3%"
        );
    }

    #[test]
    fn card_metadata_is_clipped_to_its_card() {
        let mut m = fleet();
        widen(&mut m);
        m.update(UiEvent::SessionList(vec![SessionInfo {
            command: vec![
                "journalctl".into(),
                "-f".into(),
                "--unit".into(),
                "some-very-long-daemon.service".into(),
            ],
            pid: 123456,
            ..info("skinny")
        }]));
        // A tall observed grid narrows the card (aspect-locked), so the long
        // command cannot possibly fit its meta line.
        m.update(UiEvent::SessionPush {
            name: "skinny".into(),
            push: crate::SessionPush::Event(ghost_vt::protocol::SessionEvent::Resized {
                cols: 20,
                rows: 80,
            }),
        });
        let scene = m.view();
        let meta = scene.layers[0]
            .items
            .iter()
            .find_map(|it| match it {
                SceneItem::Text { runs, rect, .. } if runs[0].text.starts_with("skinny") => {
                    Some((runs[0].text.clone(), *rect))
                }
                _ => None,
            })
            .expect("the card has a meta line");
        let (text, rect) = meta;
        assert!(
            text.chars().count() as f32 * METRICS.advance <= rect.w + 0.5,
            "the meta line fits its card: {text:?} ({} chars) in {}px",
            text.chars().count(),
            rect.w
        );
        assert!(text.ends_with('\u{2026}'), "the cut is visible: {text:?}");
    }

    #[test]
    fn card_metadata_omits_the_shell_command() {
        // A shell session (empty command) shows just name · pid — no "$SHELL".
        assert_eq!(
            card_meta("build", &[], 4012, None, None),
            "build \u{b7} 4012"
        );
        // A real command is shown.
        assert_eq!(
            card_meta("edit", &["nvim".into(), "x.rs".into()], 40, None, None),
            "edit \u{b7} nvim x.rs \u{b7} 40"
        );
        // Unknown pid is omitted too.
        assert_eq!(card_meta("s", &[], 0, None, None), "s");
    }

    #[test]
    fn card_metadata_shows_reported_progress() {
        use ghost_term::Progress;
        // The suffix formats per OSC 9;4 state.
        assert_eq!(
            card_meta("b", &[], 0, None, Some(Progress::Normal(42))),
            "b \u{b7} 42%"
        );
        assert_eq!(
            card_meta("b", &[], 0, None, Some(Progress::Error(90))),
            "b \u{b7} \u{2717} 90%"
        );
        assert_eq!(
            card_meta("b", &[], 0, None, Some(Progress::Indeterminate)),
            "b \u{b7} \u{2026}"
        );
        assert_eq!(
            card_meta("b", &[], 0, None, Some(Progress::Paused(10))),
            "b \u{b7} \u{23f8} 10%"
        );

        // End to end: a session reporting progress shows it on its card;
        // clearing the report removes it.
        let texts = |m: &FleetModel| -> Vec<String> {
            m.view().layers[0]
                .items
                .iter()
                .filter_map(|it| match it {
                    SceneItem::Text { runs, .. } => {
                        Some(runs.iter().map(|r| r.text.as_str()).collect())
                    }
                    _ => None,
                })
                .collect()
        };
        let mut m = fleet();
        list(&mut m, &["a"]);
        let cmds = data(&mut m, "a", b"\x1b]9;4;1;42\x07");
        assert!(
            cmds.contains(&Cmd::Redraw),
            "a pure progress report (no printable output) must repaint the card"
        );
        assert!(
            texts(&m).iter().any(|t| t.contains("42%")),
            "progress missing from the card: {:?}",
            texts(&m)
        );
        data(&mut m, "a", b"\x1b]9;4;0\x07");
        assert!(
            !texts(&m).iter().any(|t| t.contains("42%")),
            "cleared progress still shown"
        );
    }

    #[test]
    fn cards_stay_within_the_window_width_keep_a_min_height_and_do_not_overlap() {
        // A spread of session counts. Cards fit the window WIDTH and never overlap,
        // and every card keeps a readable minimum height — the grid grows past the
        // viewport (and scrolls) rather than collapsing previews to fit.
        let min_card_h =
            2.0 * (METRICS.line_height + 6.0) + MIN_PREVIEW_LINES * METRICS.line_height;
        for n in 1..=12usize {
            let mut m = fleet();
            let infos: Vec<SessionInfo> = (0..n).map(|i| info(&format!("s{i}"))).collect();
            m.update(UiEvent::SessionList(infos));
            let (_, placements, _, content_h) = m.sections_layout();
            assert_eq!(placements.len(), n);
            let w = SIZE.0 as f32;
            for (_, _, r) in &placements {
                assert!(r.x >= 0.0 && r.y >= 0.0, "n {n}: {r:?}");
                assert!(r.x + r.w <= w + 0.5, "width overflow n {n}: {r:?}");
                assert!(
                    r.h >= min_card_h - 0.5,
                    "card collapsed below the minimum height n {n}: {r:?}"
                );
            }
            for (i, (_, _, a)) in placements.iter().enumerate() {
                for (_, _, b) in &placements[i + 1..] {
                    assert!(!rects_overlap(a, b), "overlap n {n}: {a:?} vs {b:?}");
                }
            }
            // With enough sessions the grid overflows the viewport (then scrolls).
            if n >= 2 {
                assert!(
                    content_h > SIZE.1 as f32,
                    "n {n}: {content_h} should overflow the {}px viewport",
                    SIZE.1
                );
            }
        }
    }

    #[test]
    fn cards_keep_the_terminal_aspect_ratio() {
        // The preview area must keep the terminal's width:height ratio, not stretch
        // to full column width once the card height is capped.
        let aspect =
            (PREVIEW_COLS as f32 * METRICS.advance) / (PREVIEW_ROWS as f32 * METRICS.line_height);
        // A range of windows, including the default that surfaced the bug.
        for (w, h) in [(720, 432), (1400, 900), (1000, 700), (500, 1000)] {
            let mut m = fleet();
            m.update(UiEvent::Resize {
                w_px: w,
                h_px: h,
                scale: 1.0,
            });
            list_many(&mut m, 4);
            let (_, placements, band, _) = m.sections_layout();
            for (_, _, r) in &placements {
                let preview_w = r.w; // the preview spans the full card width
                let preview_h = r.h - 2.0 * band; // minus the header + footer bands
                let got = preview_w / preview_h;
                assert!(
                    (got - aspect).abs() < 0.02,
                    "card {w}x{h}: preview aspect {got:.3} != terminal {aspect:.3} ({r:?})"
                );
            }
        }
    }

    #[test]
    fn crowded_grids_use_the_compact_card_size() {
        let band = METRICS.line_height + 6.0;
        let compact =
            2.0 * band + PREVIEW_ROWS as f32 * METRICS.line_height * PREVIEW_COMPACT_SCALE;

        // Far more sessions than fit: cards settle at the compact thumbnail size and
        // the grid scrolls, rather than each card growing.
        let mut m = fleet();
        m.update(UiEvent::Resize {
            w_px: 2000,
            h_px: 1200,
            scale: 1.0,
        });
        list_many(&mut m, 40);
        let ch = m.sections_layout().1[0].2.h;
        assert!(
            (ch - compact).abs() < 1.0,
            "a crowded grid should use the compact card ({compact}), got {ch}"
        );
    }

    #[test]
    fn a_few_sessions_get_larger_previews() {
        let band = METRICS.line_height + 6.0;
        let native = 2.0 * band + PREVIEW_ROWS as f32 * METRICS.line_height;
        let size = UiEvent::Resize {
            w_px: 2000,
            h_px: 1200,
            scale: 1.0,
        };

        // A couple of sessions grow to use the space; a crowded grid stays compact.
        let mut few = fleet();
        few.update(size.clone());
        list_many(&mut few, 2);
        let few_h = few.sections_layout().1[0].2.h;

        let mut many = fleet();
        many.update(size);
        list_many(&mut many, 40);
        let many_h = many.sections_layout().1[0].2.h;

        assert!(
            few_h > many_h + 1.0,
            "a couple of sessions should preview larger than a crowded grid ({few_h} vs {many_h})"
        );
        assert!(
            few_h <= native + 1.0,
            "previews should not grow past native size ({few_h} > {native})"
        );
    }

    #[test]
    fn the_grid_scrolls_with_the_wheel_and_clamps_to_the_ends() {
        let mut m = fleet();
        list_many(&mut m, 6); // overflows the 400x200 viewport
        assert!(
            m.max_scroll() > 0.0,
            "the grid must overflow to be scrollable"
        );
        assert_eq!(m.scroll_y, 0.0, "starts pinned to the top");

        // Wheel up at the top is a no-op (already clamped).
        assert_eq!(wheel(&mut m, 1.0), vec![], "no scroll past the top");
        assert_eq!(m.scroll_y, 0.0);

        // Wheel down scrolls toward the bottom.
        assert_eq!(wheel(&mut m, -1.0), vec![Cmd::Redraw]);
        assert!(m.scroll_y > 0.0, "wheel down scrolled");

        // Many notches clamp at the bottom; further ones do nothing.
        for _ in 0..50 {
            wheel(&mut m, -1.0);
        }
        assert_eq!(m.scroll_y, m.max_scroll(), "clamps at the bottom");
        assert_eq!(wheel(&mut m, -1.0), vec![], "no scroll past the bottom");

        // And back up to the top.
        for _ in 0..50 {
            wheel(&mut m, 1.0);
        }
        assert_eq!(m.scroll_y, 0.0, "returns to the top");
    }

    #[test]
    fn arrow_navigation_scrolls_the_focused_tile_into_view() {
        let mut m = fleet();
        // A viewport taller than one tile but shorter than the whole column, so
        // walking down must scroll while keeping the focused tile fully visible.
        m.update(UiEvent::Resize {
            w_px: 400,
            h_px: 500,
            scale: 1.0,
        });
        list_many(&mut m, 6); // single column (narrow), taller than 500px
        assert_eq!(m.focused(), Some("s0"));
        assert_eq!(m.scroll_y, 0.0);

        let view_h = 500.0;
        for _ in 0..5 {
            key(&mut m, Key::Named(NamedKey::ArrowDown));
            let (_, placements, _, _) = m.sections_layout();
            let (_, _, r) = placements
                .into_iter()
                .find(|(_, id, _)| Some(id.as_str()) == m.focused())
                .unwrap();
            let (top, bottom) = (r.y - m.scroll_y, r.y + r.h - m.scroll_y);
            assert!(
                top >= -0.5 && bottom <= view_h + 0.5,
                "focused tile {top}..{bottom} must stay within the {view_h}px viewport"
            );
        }
        assert!(m.scroll_y > 0.0, "navigating down scrolled the grid");
    }

    #[test]
    fn offscreen_tiles_are_culled_but_the_section_header_stays() {
        let mut m = fleet();
        m.update(UiEvent::Resize {
            w_px: 400,
            h_px: 400,
            scale: 1.0,
        });
        list_many(&mut m, 6);
        for i in 0..6 {
            data(&mut m, &format!("s{i}"), b"live"); // every tile a live preview
        }
        let visible = m.view().terminals().count();
        assert!(
            (1..6).contains(&visible),
            "only on-screen tiles render previews, got {visible}/6"
        );
        // The section header is still emitted even though most tiles are culled.
        assert_eq!(headers(&m).len(), 1, "the section header survives culling");
    }

    #[test]
    fn enlarging_the_window_clamps_the_scroll_offset() {
        let mut m = fleet();
        list_many(&mut m, 6);
        for _ in 0..50 {
            wheel(&mut m, -1.0); // scroll to the bottom
        }
        assert!(m.scroll_y > 0.0 && m.scroll_y == m.max_scroll());

        // A viewport tall enough to hold everything leaves nothing to scroll.
        m.update(UiEvent::Resize {
            w_px: 400,
            h_px: 5000,
            scale: 1.0,
        });
        assert_eq!(m.max_scroll(), 0.0, "a tall window fits the whole grid");
        assert_eq!(m.scroll_y, 0.0, "scroll clamps back to the top");
    }

    #[test]
    fn shift_tab_moves_focus_backward() {
        let mut m = fleet();
        list(&mut m, &["a", "b", "c"]); // focus "a"
        key(&mut m, Key::Named(NamedKey::Tab)); // a -> b
        assert_eq!(m.focused(), Some("b"));
        // Shift+Tab steps back and wraps.
        let cmds = m.update(UiEvent::Key {
            key: Key::Named(NamedKey::Tab),
            mods: crate::Mods::SHIFT,
            kind: KeyEventKind::Press,
            alts: None,
        });
        assert_eq!(m.focused(), Some("a"));
        assert_eq!(cmds, vec![Cmd::Redraw]); // focus only, never SendInput
        // Wrapping backward past the start lands on the last tile.
        m.update(UiEvent::Key {
            key: Key::Named(NamedKey::Tab),
            mods: crate::Mods::SHIFT,
            kind: KeyEventKind::Press,
            alts: None,
        });
        assert_eq!(m.focused(), Some("c"));
    }

    #[test]
    fn the_fleet_never_attaches_sessions_itself() {
        // No auto-attach for anyone — neither a detached session nor one owned by
        // another window. Live previews come only from sessions this window drives.
        let mut m = fleet(); // mine is empty
        let mut elsewhere = info("foreign");
        elsewhere.attached = true; // attached by some other window
        let cmds = m.update(UiEvent::SessionList(vec![info("mine-detached"), elsewhere]));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Attach(_))),
            "the fleet must not attach any session: {cmds:?}"
        );
        assert_eq!(m.locality_of("foreign"), Some(Locality::Elsewhere));
        assert_eq!(m.locality_of("mine-detached"), Some(Locality::Detached));
        assert_eq!(m.tile_count(), 2); // both shown, as placeholder tiles
    }

    #[test]
    fn into_single_keeps_the_owned_session_even_when_focus_moved() {
        // The window owns "alpha"; the fleet also previews a foreign "beta".
        let mine = HashSet::from(["alpha".to_string()]);
        let primary = TerminalModel::new("alpha".to_string(), 80, 24, METRICS);
        let (mut f, _) = FleetModel::adopting(primary, Vec::new(), METRICS, SIZE, 1.0, mine);
        f.update(UiEvent::SessionList(vec![info("alpha"), info("beta")]));
        // Move focus onto the foreign tile (it's in the section below ours).
        f.update(UiEvent::Key {
            key: Key::Named(NamedKey::ArrowDown),
            mods: crate::Mods::NONE,
            kind: KeyEventKind::Press,
            alts: None,
        });
        assert_eq!(f.focused(), Some("beta"));
        // Toggling back returns the OWNED session and detaches nothing — the
        // other sessions stay attached (warm) for Ctrl-Tab and live previews.
        let (model, _warm, cmds) = f.into_single(SIZE, 1.0);
        assert_eq!(model.session(), "alpha", "keeps the window's own session");
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Detach(_))),
            "no session is detached on toggle-back: {cmds:?}"
        );
    }

    #[test]
    fn idle_reconcile_with_no_changes_does_not_redraw() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        // A re-list of the same sessions changes nothing visible.
        let cmds = list(&mut m, &["a", "b"]);
        assert!(
            !cmds.contains(&Cmd::Redraw),
            "an idle reconcile must not emit a redraw"
        );
        // It still re-arms the refresh tick.
        assert!(cmds.contains(&Cmd::ScheduleTick {
            after_ms: REFRESH_MS
        }));
    }
}
