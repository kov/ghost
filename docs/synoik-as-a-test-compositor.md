# What ghost's tests want from a compositor

ghost's window-level bugs — the ones that only exist because a compositor said
something specific about a surface — can only be caught by running the real
binary against a real compositor. The suite does that twice today, with two
different compositors, because neither can do the whole job:

| | headless weston | headless mutter | why it matters |
|---|---|---|---|
| runs with no seat/logind session | yes | yes | it has to work under plain `cargo test` |
| hands out a working Vulkan surface | with `--renderer=gl` | yes (gbm on `/dev/dri/renderD128`) | a software compositor advertises no dmabuf, and ghost cannot present at all |
| fractional output scale | **no** | yes (1.25 / 1.333 / 1.667 out of the box) | weston 15 implements no `wp_fractional_scale_v1`, and its `--scale` is an integer |
| set the scale without a UI | n/a | `org.gnome.Mutter.DisplayConfig.ApplyMonitorsConfig` | the rig has to choose the scale, and change it mid-run |
| drive input | not used | not used | our tests reach for a second `ghost` launch instead (single-instance forwarding) |

`ghost-ui-harness/tests/windowed.rs` uses weston for the real-swapchain path;
`ghost-ui/tests/fractional_scale.rs` uses mutter for everything that only goes
wrong at 1.25. Both start and reap their own compositor, on a private socket,
so the developer's desktop is never involved.

synoik could replace both — it is the compositor ghost is actually used on, it
already does fractional scaling, and `synoik msg` is a far better control
surface than mutter's serial-guarded D-Bus call or weston's nothing. This is
what it would need, in the order that decides whether a test can exist at all.

## Required

1. **A GPU surface in headless mode.** The one hard blocker today: a headless
   synoik advertises no dmabuf, so no Vulkan adapter is compatible with the
   surface and ghost's swapchain dies at creation (`ERROR_SURFACE_LOST_KHR`,
   "no surface-compatible adapter"). Rendering to a DRM render node the way
   mutter's gbm renderer does is enough — nothing has to reach a screen.
2. **No seat, no logind session, no TTY.** A test process is not a session
   leader and cannot become one. (The "Failed to open session: Function not
   implemented (os error 38)" we first hit was our own mistake: running plain
   `synoik` picks the tty backend, which is the only thing that touches
   libseat. `--headless` never does.)
3. **A private Wayland socket, named by a flag.** Tests run concurrently and
   must never touch each other or the live desktop: `--wayland-display=NAME`
   (or a path) plus honouring `XDG_RUNTIME_DIR`, like weston's `--socket` and
   mutter's `--wayland-display`.
4. **A virtual output of a size we choose**, at a scale we choose, set from the
   command line — `--output 1600x1000@1.25` would remove the entire
   `ApplyMonitorsConfig` dance from the rig.
5. **Quiet, killable, and no bus-activated orphans.** The rig kills the
   compositor by the pid it captured and sets `PR_SET_PDEATHSIG` on it; anything
   the compositor *activates* on a session bus is nobody's child and outlives
   the test (we had to switch mutter's GIO to `GIO_USE_VFS=local` for exactly
   this). Log to stderr, exit on SIGTERM, spawn nothing that survives you.

## Wanted, in rough order of what it would buy

6. **Change the output scale at runtime** — `synoik msg action set-scale --output
   N 1.3333`. Half the fractional-scale bugs are in the *transition*: the
   surface resizes with no configure to announce it, and everything measured
   from the old scale has to be re-measured. The current test only gets this by
   re-applying a whole monitor config.
7. **Maximize, tile and resize a window from IPC.** ghost drops its shadow
   margins when maximized or tiled, which changes the surface size with no
   configure — the same class of bug as the abort we just fixed, and completely
   untested today because no rig can maximize a window. `synoik msg action
   maximize/tile-left/set-size --id N` would close that gap.
8. **Two outputs at different scales**, and a way to move a window between them.
   Nothing in the suite covers a window crossing a scale boundary, which is
   where per-window caches (glyph atlas, shadow, blur region) get to be wrong.
9. **Key and text injection with held modifiers.** `synoik msg input key-press
   alt` / `input key n` / `input key-release alt` already works on the live
   compositor and is what caught the Alt+N abort. In a headless build it would
   let tests drive the *real* keyboard path (kitty protocol, IME, chords)
   instead of the shortcuts we reach for now.
10. **Screenshot a single window to a file.** `action screenshot-window --id N`
    exists but lands on the clipboard, which needs `wl-paste` and a running
    clipboard manager. A `--output <path>` would let a test assert on pixels
    directly — the only way to check what the user actually sees.
11. **A window listing that includes state.** `msg -j windows` already gives id,
    pid, app_id, title, workspace and geometry — adding maximized/tiled/focused/
    activated, and the surface *and* geometry sizes as the compositor sees them,
    would let tests assert the compositor's view against ghost's own
    ("the shadow ring is surface minus geometry" is currently checked only from
    inside ghost).
12. **A frame/commit counter per window.** "Did anything actually reach the
    screen, and how often" is the missing half of every render test we have:
    our assertions are on what ghost *decided* to draw, not on what the
    compositor received.
13. **Protocols worth having a rig for**, none of which weston or mutter give us
    together: `ext-background-effect-v1` (ghost's preferred frosted-background
    path, currently unverified end to end), `xdg-session-management` (window and
    group restore — shipped, but only ever tested by hand), and fractional-scale
    plus viewporter behaving exactly as the live desktop does, since that is the
    combination our users run.
14. **A deterministic mode**: animations off (or a fixed clock), no idle
    timeout, no compositor-side crossfade. Real-time compositor animation is
    what made the last render bug hard to see, and a test that races a
    crossfade is a flaky test.

## What we do not need

A window manager UI, workspaces, an X server, input devices, DPMS, or anything
that requires a real display. Nor do we need synoik to be *fast* — the current
fractional rig runs in half a second, and even a slow start-up beats not being
able to write the test at all.

If synoik grows 1–5 it replaces mutter in `fractional_scale.rs` immediately, and
weston in `windowed.rs` right after; 6–14 are each worth a test that cannot be
written today.

## Status — everything above is answered

synoik implemented this list on 2026-08-10 (rationale in that repo's
`docs/fork/headless-test-compositor.md`). 6, 7, 9, 10 and 13 already worked;
1 (`df659a12`, headless dmabuf), 14 (`562edd3e`), 4 + 8 + 5 (`5004eb8d`,
`--output WxH[@SCALE]`, repeatable, and no more xwayland-satellite in
`/tmp/.X11-unix`), 11 (`4ce53800`) and 3 (`bcde724e`) landed. 12, the per-window
frame counter, was declined in favour of the existing `synoik msg frame-perf` —
two instruments measuring adjacent things is how one silently omits its own
event.

**Verified against a real ghost run**, not just reported: a headless synoik at
`--output 1600x1000@1.25` hands ghost a working swapchain, and across
maximize → runtime scale change to 1.3333 → unmaximize the frame invariant
holds every time (`surface − geometry == inset`: 65/65 floating at 1.25, 0/0
maximized, 69/70 floating at 1.3333) with zero dropped frames. `msg -j windows`
reports `surface_size` beside `window_size`, so the shadow ring can finally be
checked from *outside* ghost — 780×527 against 728×475 logical is the same 65px
ghost measured for itself. Maximize-at-fractional-scale, called untestable
above, is now writable. Input injection, screenshot-to-file and
`ext-background-effect-v1` are reported working but we have not exercised them.

Two caveats gate a synoik rig: the headless dmabuf path is **LINEAR 8888 only**
(a Venus constraint), and on a driver without `VK_EXT_physical_device_drm` —
lavapipe — no dmabuf global is advertised at all and the client silently falls
back to shm. So a synoik rig needs a real GPU, where weston's `--renderer=gl`
did not. Traps: isolate `XDG_CONFIG_HOME` or the developer's `monitors.xml`
overrides the requested scale with no error, keep `XDG_RUNTIME_DIR` short (the
108-byte `sockaddr_un` limit), pass `SYNOIK_SOCKET` explicitly on every `msg`
call so an ambient one cannot aim at the live desktop, and script against
`Maximize`/`ToggleTiledLeft` rather than the niri column verbs this fork is
replacing.

**Ported, 2026-08-10.** Both rigs now run on synoik and nothing else:
`ghost-ui/tests/fractional_scale.rs` (mutter, a private session bus, `gdbus`
and a serial-guarded `ApplyMonitorsConfig` all gone) and
`ghost-ui-harness/tests/windowed.rs` (weston gone). The compositor half lives
in `ghost-test-compositor/`, one crate both tests dev-depend on.

**The suite builds its own synoik.** It is not whatever is installed: the rig
clones `https://github.com/kov/synoik.git` at the commit in
`ghost-test-compositor/synoik.rev`, builds it `--no-default-features` (no
systemd, no pipewire) into `target/synoik/<rev>/`, and runs that. A developer's
installed synoik is the one drawing their desktop and moves when they upgrade
it; a suite whose compositor changes underneath it reports on something nobody
chose. First run costs one clone and ~2 minutes of build, once per pinned rev
and once more after a `cargo clean`; every run after is instant. No network (or
a build failure) is a skip naming the log. `$SYNOIK` still overrides, which is
how the pin gets moved.

One cost to know about: the windowed dive takes ~12s under headless synoik
where it took ~0.5s under headless weston, for the same 13 presented frames.
Root-caused upstream: headless never sets a primary scanout output, so frame
callbacks come only from synoik's 995ms overdue timer and Fifo waits it out on
every present. Fixed in synoik `65c0cfaa`, which pins headless's scanout
state to a real element pass: measured against that build the same dive takes
0.34–0.45s, the weston number. It is not on the remote yet, so the pin — and
this paragraph — move when it is.

**Headless synoik is not a frame-rate reference**, and that fix is why. It
moved a headless client from 1 fps to *unpaced*: callbacks go out once per
redraw and the output then idles, so a self-pacing client cycles
commit→redraw→callback as fast as the CPU allows, whatever the 60 Hz the output
advertises. That is what our rigs want — and most of why 0.34s beats weston —
but it means nothing measured here in fps or frames-per-unit-time means
anything: the denominator is the CPU, not a clock. Frames *presented* and frame
*geometry*, which is all this suite asserts, are unaffected. Take pacing
numbers on a real display, never here.
