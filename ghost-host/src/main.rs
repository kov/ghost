//! `ghost-host` — ghost with the GUI removed: the session host and the
//! SSH-transport relay, nothing that draws.
//!
//! This is the binary that staging copies to a remote machine. On the remote,
//! ghost only ever runs headless — a re-exec'd session host (`__host`), the byte
//! relay the client attaches over (`__pipe`), the session-set stream
//! (`__watch`), the transport probe (`__probe`), and the plain admin commands
//! (`ls`/`new`/`kill`/`rename`). None of that needs winit/wgpu/swash/fontconfig,
//! so keeping it in its own crate (deps: only `ghost-vt` + `ghost-cli`) means the
//! staged binary is a fraction of the full GUI build, is pure Rust (no C deps —
//! recordings use brotli), so it cross-compiles with just `rustup target add`,
//! and drags no font/window libraries onto the remote.
//!
//! It shares the *exact* entry points the full `ghost` binary uses
//! ([`ghost_vt::server::run_host_if_invoked`] + [`ghost_cli::run_subcommand`]),
//! so its behaviour for every subcommand is identical — the only thing it can't do
//! is open a window, which a remote never asks for.

fn main() {
    // Consume the internal `__host` argv first (a re-exec'd session host), exactly
    // as the GUI binary does, before clap would reject it.
    ghost_vt::server::run_host_if_invoked();

    match ghost_cli::run_subcommand() {
        ghost_cli::Launch::Handled => {}
        // A remote host has no display; there is nothing to launch. This is only
        // reachable if someone runs the bare binary interactively on the remote.
        ghost_cli::Launch::Gui { .. } => {
            eprintln!("ghost-host is a headless build with no GUI; run a subcommand (see --help)");
            std::process::exit(1);
        }
    }
}
