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
