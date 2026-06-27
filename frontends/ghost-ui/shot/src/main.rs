//! `ghost-shot` — render a ghost-ui scene to a PNG, headlessly, so we can *look*
//! at what the renderer draws without launching the windowed app.
//!
//! It drives the real models (`FleetModel`, `RootModel`, `TerminalModel`) with
//! synthetic-but-representative sessions, asks them for a `Scene`, and rasterizes
//! it through the same `ghost-renderer` the app uses (software adapter). This is
//! the first-class visual-debugging path: change the UI, run the tool, eyeball
//! the image.
//!
//! Usage: `ghost-shot <fleet|single> [out.png]` (default `ghost-shot.png`).

use std::collections::HashSet;

use ghost_render::CellMetrics;
use ghost_renderer::{Renderer, Theme};
use ghost_ui_core::{FleetModel, RootModel, TerminalModel, UiEvent};
use ghost_vt::session::SessionInfo;

/// A bundled font so the tool needs no system font lookup.
const FIRA: &[u8] = include_bytes!("../../shaper/tests/assets/FiraCode-Regular.ttf");

/// Proven metrics/size pairing from the renderer's golden tests (FiraCode at
/// `SIZE_PX` advances ~9px and is ~18px tall).
const METRICS: CellMetrics = CellMetrics {
    advance: 9.0,
    line_height: 18.0,
};
const SIZE_PX: f32 = 15.0;

fn main() {
    let mut args = std::env::args().skip(1);
    let which = args.next().unwrap_or_else(|| "fleet".to_string());
    let out = args.next().unwrap_or_else(|| "ghost-shot.png".to_string());

    let (scene, w, h) = match which.as_str() {
        "fleet" => fleet_scene(),
        "single" => single_scene(),
        other => {
            eprintln!("unknown scene '{other}' (expected: fleet | single)");
            std::process::exit(2);
        }
    };

    let font = ghost_shaper::font_from_bytes(FIRA).expect("bundled font loads");
    let mut renderer = Renderer::headless(Theme::default());
    let img = renderer.render_offscreen_scene(&scene, font, SIZE_PX);
    img.save_png(&out).expect("write png");
    println!("wrote {which} scene ({w}x{h}) to {out}");
}

/// A representative fleet overview: two sessions this window drives (live),
/// one held by another window, and one detached — covering all three sections,
/// cards, buttons, the focus border, and scaled live previews.
fn fleet_scene() -> (ghost_render::Scene, u32, u32) {
    let size = (1400u32, 900u32);
    let mine: HashSet<String> = ["edit", "build"].into_iter().map(String::from).collect();

    // The focused/primary tile carries real content via `adopting`.
    let primary = TerminalModel::new("edit".to_string(), 80, 24, METRICS);
    let (mut fleet, _) = FleetModel::adopting(primary, METRICS, size, 1.0, mine);

    fleet.update(UiEvent::SessionList(vec![
        info("edit", true, &["nvim", "src/fleet.rs"], 4011),
        info("build", true, &[], 4012),
        info("logs", true, &["journalctl", "-f"], 4099), // attached elsewhere
        info("prod", false, &["ssh", "prod-web-1"], 3777), // detached
    ]));

    // Live previews for the sessions this window drives.
    feed(&mut fleet, "edit", EDIT);
    feed(&mut fleet, "build", BUILD);

    let scene = fleet.view();
    (scene, size.0, size.1)
}

/// The single-terminal view, for comparison / regression on the same content.
fn single_scene() -> (ghost_render::Scene, u32, u32) {
    let size = (1100u32, 700u32);
    let mut model = TerminalModel::new("edit".to_string(), 80, 24, METRICS);
    model.update(UiEvent::Resize {
        w_px: size.0,
        h_px: size.1,
        scale: 1.0,
    });
    model.update(UiEvent::SessionData {
        name: "edit".to_string(),
        bytes: EDIT.as_bytes().to_vec(),
        ended: false,
    });
    let root = RootModel::single(model, METRICS, size);
    (root.view(), size.0, size.1)
}

fn feed(fleet: &mut FleetModel, name: &str, content: &str) {
    fleet.update(UiEvent::SessionData {
        name: name.to_string(),
        bytes: content.as_bytes().to_vec(),
        ended: false,
    });
}

fn info(name: &str, attached: bool, command: &[&str], pid: i32) -> SessionInfo {
    SessionInfo {
        name: name.to_string(),
        pid,
        created_at: None,
        title: name.to_string(),
        command: command.iter().map(|s| s.to_string()).collect(),
        attached,
        bell: false,
    }
}

const EDIT: &str = "\x1b[1;34m~/ghost/frontends/ghost-ui\x1b[0m\r\n\
\x1b[38;5;240m  1\x1b[0m \x1b[35mfn\x1b[0m \x1b[33mfleet_scene\x1b[0m() -> Scene {\r\n\
\x1b[38;5;240m  2\x1b[0m     \x1b[35mlet\x1b[0m mine = [\x1b[32m\"edit\"\x1b[0m, \x1b[32m\"build\"\x1b[0m];\r\n\
\x1b[38;5;240m  3\x1b[0m     \x1b[35mlet\x1b[0m (fleet, _) = FleetModel::adopting(..);\r\n\
\x1b[38;5;240m  4\x1b[0m     fleet.view()\r\n\
\x1b[38;5;240m  5\x1b[0m }\r\n";

const BUILD: &str = "\x1b[1;32m~/ghost\x1b[0m $ cargo test -p ghost-ui-core\r\n\
\x1b[32m   Compiling\x1b[0m ghost-ui-core v0.1.0\r\n\
\x1b[32m    Finished\x1b[0m test profile in 1.39s\r\n\
\x1b[1mrunning 134 tests\x1b[0m\r\n\
\x1b[32mtest result: ok.\x1b[0m 134 passed; 0 failed\r\n\
$ \x1b[5m_\x1b[0m\r\n";
