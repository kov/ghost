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
    /// Start a new session and attach to it (runs $SHELL, or a command given after `--`).
    New {
        /// Name for the session.
        name: Option<String>,
        /// Start the session in the background without attaching to it.
        #[arg(short = 'd', long)]
        detached: bool,
        /// Create the session *deferred but unattached*: its child starts on the
        /// first attach rather than now. An implementation detail used by GUI
        /// front-ends (which create a session, then attach to it), hidden from
        /// help. Without it, `-d` starts the child eagerly.
        #[arg(long, hide = true)]
        defer: bool,
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
    /// List running sessions.
    Ls,
    /// Attach to a session.
    Attach {
        /// Name of the session to attach to.
        name: Option<String>,
    },
    /// Kill a session and its process.
    Kill {
        /// Name of the session to kill.
        name: String,
    },
    /// Rename a running session.
    Rename {
        /// Current session name.
        old: String,
        /// New session name.
        new: String,
    },
    /// Export a session's recording as an asciicast (asciinema) stream.
    Export {
        /// Name of the recorded session.
        name: String,
        /// Output file; writes to stdout if omitted.
        output: Option<std::path::PathBuf>,
    },
}

fn main() {
    // If we were re-exec'd as a session host, become it and never return here.
    // Must run before clap, which would reject the internal `__host` argv.
    server::run_host_if_invoked();

    let cli = Cli::parse();
    match cli.command {
        Command::New {
            name,
            detached,
            defer,
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
                // Attached sessions (the default, and `--defer`) start their
                // child on the attach handshake; a plain `-d` starts it now.
                start_on_attach: !detached || defer,
            };
            // `spawn` forks the session off and returns here in the launching
            // process. By default we then attach to it (the common case: start a
            // session and start using it); `-d` leaves it running in the
            // background, like the underlying daemon model.
            if let Err(e) = server::spawn(opts) {
                fail(&e.to_string());
            }
            if detached {
                println!("started session '{name}'");
            } else if let Err(e) = client::attach(&name) {
                fail(&format!("session '{name}' started but attach failed: {e}"));
            }
        }
        Command::Ls => match session::list() {
            Ok(sessions) => {
                for s in sessions {
                    if s.title.is_empty() {
                        println!("{}\t(pid {})", s.name, s.pid);
                    } else {
                        println!("{}\t(pid {})\t{}", s.name, s.pid, s.title);
                    }
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
        Command::Rename { old, new } => match client::rename(&old, &new) {
            Ok(()) => println!("renamed '{old}' to '{new}'"),
            Err(e) => fail(&e.to_string()),
        },
        Command::Export { name, output } => {
            if let Err(e) = export(&name, output.as_deref()) {
                fail(&e.to_string());
            }
        }
    }
}

fn export(name: &str, output: Option<&std::path::Path>) -> std::io::Result<()> {
    use ghost_vt::{paths, record};
    let path = paths::recording_path(name);
    let rec = record::read(&path).map_err(|e| {
        std::io::Error::new(e.kind(), format!("no recording for session '{name}': {e}"))
    })?;
    match output {
        Some(p) => record::write_asciicast(&rec, &mut std::fs::File::create(p)?),
        None => record::write_asciicast(&rec, &mut std::io::stdout().lock()),
    }
}

fn default_name() -> String {
    format!("ghost-{}", std::process::id())
}

fn fail(msg: &str) -> ! {
    eprintln!("ghost: {msg}");
    std::process::exit(1);
}
