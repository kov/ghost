//! `ghost` — reference CLI for the `ghost-vt` engine.

use clap::{Parser, Subcommand};
use ghost_vt::client;
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
        /// Do not record this session (recording is on by default).
        #[arg(long)]
        no_record: bool,
        /// Scrollback lines retained for replay on attach.
        #[arg(long, default_value_t = ghost_vt::screen::DEFAULT_SCROLLBACK)]
        scrollback: usize,
        /// Cap on the recording's on-disk size, in bytes (oldest history is
        /// dropped past this).
        #[arg(long, default_value_t = ghost_vt::record::DEFAULT_MAX_RECORDING_BYTES)]
        max_recording_size: usize,
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
        Command::New {
            name,
            no_record,
            scrollback,
            max_recording_size,
            command,
        } => {
            let name = name.unwrap_or_else(default_name);
            let record = (!no_record).then(|| ghost_vt::paths::recording_path(&name));
            let opts = SpawnOpts {
                name: name.clone(),
                command,
                size: (80, 24),
                record,
                scrollback,
                max_recording_bytes: Some(max_recording_size),
            };
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
        Command::Attach { name } => {
            let Some(name) = name else {
                fail("specify a session to attach to (see `ghost ls`)");
            };
            if let Err(e) = client::attach(&name) {
                fail(&e.to_string());
            }
        }
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
