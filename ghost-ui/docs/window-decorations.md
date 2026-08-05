# Window decorations & client-side decorations (CSD) for ghost-ui

**Status:** Decided · **Date:** 2026-06-23 · **Scope:** `ghost-ui` frontend chrome
(titlebar / window controls / borders / resize)

## Question

For ghost's custom GPU frontend (winit + wgpu, replacing GTK/VTE) we want a
GTK-`HeaderBar`-style titlebar: the window's title strip holds our own content
(title, tabs, fleet UI) instead of a native title bar. How do we render borders
and drive window move/resize ourselves, and do we go fully custom on macOS too
or keep native window buttons there?

## winit 0.30 — what the toolkit actually gives us

winit hands us a thin set of primitives and expects us to do the hit-testing:

- `WindowAttributes::with_decorations(false)` / `Window::set_decorations(false)`
  — drop the native title bar + borders; we own every pixel.
- `Window::drag_resize_window(ResizeDirection)` — ask the WM/compositor to run
  its own interactive resize from an edge/corner. `ResizeDirection` is the 8-way
  enum (East/West/North/South + the four corners).
- `Window::drag_window()` — ask the WM to run an interactive move (for a custom
  titlebar drag region).
- `Window::set_cursor(CursorIcon::…)` — `NsResize`, `EwResize`, `NwseResize`,
  `NeswResize`, … for edge affordance. (0.30 renamed `set_cursor_icon` →
  `set_cursor`.)

So **we** do the edge/corner hit-testing and cursor feedback; the
compositor/WM does the actual geometry — *on platforms where winit implements
it* (see the macOS caveat below).

### macOS window-attribute extensions (`WindowAttributesExtMacOS`, winit 0.30.13)

Verified present: `with_movable_by_window_background`, `with_titlebar_transparent`,
`with_title_hidden`, `with_titlebar_hidden`, `with_titlebar_buttons_hidden`,
`with_fullsize_content_view`, `with_has_shadow`.

The "native buttons, no bar" recipe (keeps the traffic lights, hides the bar):

```rust
WindowAttributes::default()
    .with_fullsize_content_view(true)  // wgpu surface extends under the titlebar
    .with_titlebar_transparent(true)   // our content shows through the strip
    .with_title_hidden(true)           // drop the native title text
    // leave titlebar_hidden = false, titlebar_buttons_hidden = false → lights stay
```

Crucially this keeps `decorations(true)`: a normal resizable `NSWindow`, just
with a restyled titlebar.

## How GTK/GDK does it (our reference)

GTK is a **full client-side-decorations** implementation on *every* platform,
including macOS — it draws its own titlebar, buttons, borders, and shadow and
never uses native buttons anywhere. That is exactly why GTK apps (GIMP,
Inkscape) feel foreign on a Mac.

The useful insight: **GTK bottoms out at the same primitive winit does.**
`gdk_toplevel_begin_resize(edge, …)` / `gdk_toplevel_begin_move(…)` translate to
`xdg_toplevel.resize`/`.move` on Wayland and `_NET_WM_MOVERESIZE` on X11 — the
same calls `drag_resize_window`/`drag_window` wrap. `GdkSurfaceEdge` is the same
8-way enum as `ResizeDirection`. winit gives us nothing *less* at the bottom;
GTK's value is the decade of polish in the layer above:

1. Edge hit-testing from a CSS resize margin.
2. Per-edge cursors.
3. The shadow + `_GTK_FRAME_EXTENTS` dance — GTK draws a window *larger* than the
   logical one, fills the margin with a drop shadow, and tells the WM (via
   `_GTK_FRAME_EXTENTS`) to exclude the shadow from snapping/tiling/maximize.
   This is why "GTK windows are bigger than they look." Drop the shadow on
   maximize.
4. Buttons as a real widget (`GtkWindowControls`) laid out per the desktop's
   `gtk-decoration-layout` (left/right, order). Drag region = `GtkWindowHandle`.
5. Titlebar gestures — double-click-to-maximize (respecting the configured
   action), right-click window menu, edge-snap on drag.
6. Tiled/maximized edge suppression — `gdk_toplevel_get_state` reports tiled
   edges so GTK hides resize handles + shadow on screen/tile boundaries.

**Takeaway: reference GTK for mechanics (1–6), never for appearance.** GTK's look
is Adwaita; copying it onto macOS reproduces the foreign feel.

## The macOS reality that drove the decision

**winit's `drag_resize_window` is a hard `NotSupported` on macOS**
(`winit-0.30.13/src/platform_impl/macos/window_delegate.rs:1179-1180`), whereas
on Wayland it delegates to `xdg_toplevel.resize` and the compositor does the
work (`…/linux/wayland/window/state.rs:432`). `drag_window` *is* implemented on
macOS (via `performWindowDragWithEvent:`).

Consequence: a **borderless** macOS window means we implement resize *ourselves*
— track `mouseDragged`, compute the new frame, call `setFrame`. That is a
separate implementation from Linux (where the compositor does it), and it means
fighting AppKit rather than delegating to it. Going borderless on macOS also
forfeits, and forces us to reimplement by hand:

- edge resize (the `NotSupported` above),
- rounded corners + the system shadow (mask the surface, match the radius),
- `canBecomeKeyWindow` / `canBecomeMainWindow` (borderless `NSWindow`s don't
  become key by default),
- double-click-titlebar-to-zoom, drag-to-screen-edge tiling, Stage Manager.

None of that is shared with the Linux CSD path. So macOS custom CSD is *additive*
to the Linux work, not a delta on it.

## Options considered

- **A — native buttons on macOS, custom CSD on Linux.** macOS keeps decorations
  *on* and just restyles the titlebar (recipe above): native resize, native
  drag, native traffic lights + their behaviors, accessibility all free. Linux
  is `decorations(false)` with the full custom path. Headerbar *content* is
  shared and custom on both.
- **B — full custom CSD everywhere (GTK's approach).** Draw our own buttons,
  borders, shadow, and resize on macOS too. One code path, pixel-identical
  chrome.

### Why not B

1. **It's not "one more mile" — it's a different, larger body of work.** On Linux
   the compositor still does resize/snap/tiling even with decorations off; on
   macOS, borderless forfeits all of that and we reimplement it (see above).
2. **The accessibility / window-management regression hits our own users.**
   Custom-drawn buttons are invisible to the macOS accessibility tree, breaking
   VoiceOver/AX automation **and** tiling tools (yabai, AeroSpace, Rectangle,
   Magnet) that locate windows by their standard AX controls. The audience for a
   hackable GPU terminal is exactly the crowd running a tiling WM on macOS.
   Native traffic lights keep all of this working for free.
3. **GTK-as-reference produces the foreign look on mac** (Adwaita chrome on
   macOS) — the precise thing that makes GTK apps disliked there.
4. **Native is the *more*-complete macOS support, not less.** "Native buttons +
   transparent titlebar" already is full mac support (a11y, tiling tools, system
   gestures intact). Custom CSD on mac would be *less* integrated, traded for
   visual uniformity.
5. **Precedent.** The closest comparable — **Zed** (Rust, wgpu, own GPUI
   renderer, terminal-adjacent) — keeps native traffic lights on macOS and does
   full CSD on Linux (= Option A). The full-custom-everywhere camp is GTK.

## Decision

**Adopt Option A, with the macOS button strategy kept swappable.**

- Build the **Linux custom CSD** path now.
- Default **macOS to native traffic lights** (transparent-titlebar recipe).
- Model the seam so a future uniform-chrome option doesn't require rework, but is
  **not launch scope**.

## Architecture — the seam

Keep the functional-core / imperative-shell split (per the UI-testability
contract):

- **`ghost-ui-core`** models the headerbar as logical zones + interactive
  hit-regions + a **button-source** enum: `Native` vs `Custom`, plus a set of
  resize-edge regions. The branch is *data into the reducer*, not a fork of it,
  so it stays headlessly testable:
  - macOS → button source `Native`, resize-edge regions **empty** (AppKit
    resizes); the core just lays title/tabs out past a left inset for the lights.
  - Linux → button source `Custom`, resize regions **populated**.
- **Headerbar *content* rendering is shared** — on both platforms the wgpu
  surface covers the whole window (fullsize content view on mac; we own it all on
  Linux), so title/tabs draw with the same code.
- **winit shell** holds the only real `#[cfg]`: window-creation attrs, and on
  Linux the button→`set_minimized`/`set_maximized`/exit glue +
  `drag_resize_window`/`drag_window` + cursors. macOS gets that from AppKit.

### Decision to make up front

GNOME-style headerbars are tall (~46 px); the macOS native titlebar is ~28 px
with traffic lights pinned to the top, so a tall bar won't vertically center
them. winit 0.30 exposes **no traffic-light repositioning** (that's the bit Tauri
added native ObjC for). Start with a **per-platform bar-height constant** (tall on
Linux, near-native on macOS — everything else is data-driven); revisit with
objc2 nudging only if it bugs us.

## Work breakdown

**Linux CSD (where the work lives), roughly in bite order:**

1. Edge/corner hit-test (DPI-scaled margin) + per-edge `set_cursor`.
2. `drag_resize_window` / `drag_window` wiring from the core's hit-regions.
3. Own window buttons + `set_minimized`/`set_maximized`/exit glue.
4. Tiled/maximized edge suppression (looks broken without it).
5. Drop shadow + `_GTK_FRAME_EXTENTS` (X11) — only if we want a shadow.
6. Titlebar gestures (double-click maximize, right-click menu) — nice-to-have.

**macOS (native):** the transparent-titlebar attrs recipe + a left inset for the
lights. Resize/drag/zoom/a11y come free.

**Deferred (not launch scope):** `Custom` button source on macOS — only if a
uniform ghost-branded chrome becomes a real product goal. Would need
hand-drawn, mac-*shaped* traffic lights (three states, hover-reveal glyphs, the
macOS-15 green-button tiling popover), manual `setFrame` resize, rounded
corners + shadow, `canBecomeKeyWindow`, and would knowingly forfeit a11y +
tiling-tool integration.

## Implementation plan (2026-08-05)

The decision above stands unchanged; this is the sequence to build it, written
once the surrounding pieces existed. Since it was taken we gained
`ghost_shaper::paint_text` (chrome text with fallback, shaping and color
glyphs), `Scene::hit` as the canonical pointer router, and the window edge —
corners, hairline, outline ring, shadow — in `ghost-renderer`.

### Why much of this is deletion

Today the frame is split between two owners: sctk-adwaita draws the titlebar,
the top corners and the shadow, and we draw the bottom corners, the hairline,
the outline ring, and a hand-sampled shadow laid into the notch its subsurfaces
cannot reach (`WindowEdge::corner_shadow`). Owning the whole surface collapses
that seam rather than adding to it.

### What winit gives us, and the two patches it needs

Verified in the vendored winit (0.30.13):

- **Present on Wayland:** `drag_resize_window` → `xdg_toplevel.resize`,
  `drag_window`, `show_window_menu`. The compositor still performs the geometry;
  we only hit-test and set cursors.
- **Missing 1 — tiled edges.** winit consumes the xdg tiled state internally
  (`…/wayland/window/state.rs:446`) and never surfaces it. Needed to square off
  and un-shadow a snapped window.
- **Missing 2 — surface margins / input region.** With `decorations(false)` the
  surface *is* the window, so there is nowhere outside it to cast a shadow. A
  shadow costs a vendored patch (below). No patch, no shadow — this is the whole
  reason P4 exists as its own phase.

### Phases

- **P0 — seam + flag.** `[window] decorations = system | ghost`, default
  `system`, so the daily driver never rides a half-built frame. Wayland-only:
  see the settings note below.
- **P1 — the edge, ours.** `WindowEdge` grows from bottom-only to all four
  corners, with our own values instead of alphas sampled off sctk's theme.
  Rounding suppressed when maximized or tiled (needs patch 1). Tested as the
  edge already is, with `ghost-shot` pixel assertions.
- **P2 — resize.** The eight resize edges, per-edge cursors, and
  `drag_resize_window`. A pure `ghost_ui_core::resize_edge_at` decides which
  edge (if any) a point grabs; the shell only reads the window's state and makes
  the call.

  Not `Scene` items after all, as first sketched: the frame's regions are not
  content and must always win, so they are hit-tested *before* the pointer
  reaches the model rather than competing with it inside `Scene::hit`. This also
  keeps invisible border items out of every scene the fleet and terminal build.

  The band lies *inside* the window until P4 gives us margins, which is what
  makes swallowing the motion necessary — it overlaps the inner padding and the
  first pixels of the grid.
- **P3 — the bar, and its gestures.** Height, focus-dependent colors, title via
  `paint_text`, and our own buttons laid out from GNOME's `button-layout` —
  order *and* side, both of which are the classic CSD tell when wrong. The
  titlebar gestures land here rather than in P2 because they need a bar to
  happen on: drag-to-move, double-click-to-maximize honouring
  `action-double-click-titlebar`, right-click window menu. Making the top strip
  of the terminal a drag handle before there is a bar would only take away the
  ability to select text there.
  Known gap found while building it: `SceneItem::Rect` carries a `radius` the
  renderer **ignores** — every rounded rect in the UI (fleet cards, the toast,
  now the buttons' hover circle) is drawn square. Fixing it means a per-instance
  radius and a rounded-box SDF in the glyph shader, which is the hottest one we
  have; worth its own change with its own before/after, not a rider on this one.
- **P4 — shadow, and the deletion.** Vendored winit gains decoration margins:
  inflate the surface, set `xdg_surface.set_window_geometry` to the content
  rect, offset pointer coordinates, set the input region to content + resize
  handles. That is the GTK model (see `_GTK_FRAME_EXTENTS` above) and it buys
  back the libadwaita-fitted shadow, drawn by us into the margin and dropped
  when maximized or tiled. `EDGE_SHADOW_STEPS`, `corner_shadow` and the notch
  apparatus then go, along with `ghost-renderer`'s dev-dependency on
  sctk-adwaita for shadow-profile pinning.
- **P5 — retire the frame.** Flip the default, drop vendored sctk-adwaita and
  the title hook. `ghost_shaper::paint_text` stays; it was always the reusable
  half.

### Settled scope

- **The bar starts minimal** — title and window buttons, parity with what the
  frame draws today. Tabs and fleet affordances in the headerbar are a product
  change; bolting them onto the parity work means neither can be judged on its
  own, and it keeps macOS out of scope for longer (the bar is the only reason
  macOS would re-enter, and its backing-scale issue is unresolved).
- **Ghost CSD stays behind the flag until P4 lands.** The shadow was fitted
  against a measured GTK4 window; a shadowless interim default is the kind of
  temporary that stays.
- **Wayland only, and `system` is a real setting.** Mutter never offers
  server-side decorations, but KDE does, and X11 has no shadow without
  `_GTK_FRAME_EXTENTS`. `decorations = system` is supported configuration, not a
  debug escape hatch.

## Open question (revisit only if pursuing Custom-on-mac)

Is the desire for custom-on-mac an **aesthetic** goal (ghost wants its own chrome
identity on every platform) or just the appeal of **one uniform codebase**? If
the latter, the swappable seam already delivers the clean architecture without
the costs. If the former, scope what a mac-shaped custom button set really needs
before committing.

## References

- winit `0.30.13`; macOS resize `NotSupported`:
  `src/platform_impl/macos/window_delegate.rs:1179-1180`; Wayland delegates:
  `src/platform_impl/linux/wayland/window/state.rs:432`; macOS ext methods:
  `src/platform/macos.rs:282-294`.
- Existing winit-0.30 spike with a live window: `experiments/winit-ime-spike/`.
