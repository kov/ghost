//! `ghost` — reference CLI for the `ghost-vt` engine.

use clap::{Parser, Subcommand};

/// Run terminals in the background and reattach without losing state.
#[derive(Parser)]
#[command(name = "ghost", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start a new background session (runs $SHELL, or a command given after `--`).
    New {
        /// Name for the session.
        name: Option<String>,
        /// Command to run instead of $SHELL (everything after `--`).
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// List background sessions.
    Ls,
    /// Attach to a background session.
    Attach {
        /// Name of the session to attach to.
        name: Option<String>,
    },
    /// Kill a background session and its process.
    Kill {
        /// Name of the session to kill.
        name: String,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::New { .. } => unimplemented!("ghost new"),
        Command::Ls => unimplemented!("ghost ls"),
        Command::Attach { .. } => unimplemented!("ghost attach"),
        Command::Kill { .. } => unimplemented!("ghost kill"),
    }
}
