//! `ghost` — reference CLI for the `ghost-vt` engine.

use clap::{Parser, Subcommand};
use ghost_vt::server::{self, SpawnOpts};
use ghost_vt::session;

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
        Command::New { name, command } => {
            let name = name.unwrap_or_else(default_name);
            let opts = SpawnOpts { name: name.clone(), command, size: (80, 24) };
            match server::spawn(opts) {
                Ok(()) => println!("started session '{name}'"),
                Err(e) => fail(&e.to_string()),
            }
        }
        Command::Ls => match session::list() {
            Ok(sessions) => {
                for s in sessions {
                    println!("{}\t(pid {})", s.name, s.pid);
                }
            }
            Err(e) => fail(&e.to_string()),
        },
        Command::Attach { .. } => unimplemented!("ghost attach"),
        Command::Kill { name } => match session::kill_session(&name) {
            Ok(true) => println!("killed session '{name}'"),
            Ok(false) => fail(&format!("no such session '{name}'")),
            Err(e) => fail(&e.to_string()),
        },
    }
}

fn default_name() -> String {
    format!("ghost-{}", std::process::id())
}

fn fail(msg: &str) -> ! {
    eprintln!("ghost: {msg}");
    std::process::exit(1);
}
