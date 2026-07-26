//! The `ghost` binary: the GUI with the CLI subcommands folded in. Everything
//! lives in the `ghost-ui` library (see its crate docs for why the shell is a
//! library) — this is only its entry point.

fn main() {
    ghost_ui::run();
}
