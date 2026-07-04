//! The `ghost` command-line interface: the `new`/`ls`/`attach`/`kill`/`rename`/
//! `export` subcommands over the `ghost-vt` engine. The `ghost` binary (the GUI
//! crate) calls [`run_subcommand`] right after the session-host re-exec check — a
//! present subcommand runs here and the process exits; no subcommand falls through
//! to the windowed UI.

use clap::{Parser, Subcommand};
use ghost_vt::client;
use ghost_vt::connection::ConnectionSpec;
use ghost_vt::server::{self, SpawnOpts};
use ghost_vt::session;

/// The `ghost` command line: background terminals you can reattach to, plus —
/// with no subcommand — the windowed GPU terminal.
#[derive(Parser)]
#[command(
    name = "ghost",
    version,
    about = "Run terminals in the background and reattach without losing state. \
             With no subcommand, ghost opens its windowed GPU terminal."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Skip restoring the windows open at last quit; start fresh. Only meaningful
    /// for a bare launch (with a subcommand there is nothing to restore).
    #[arg(long, global = true)]
    fresh: bool,
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
        /// Start the session in this directory instead of the current one.
        #[arg(long)]
        cwd: Option<std::path::PathBuf>,
        /// Seed the session's screen and scrollback from a predecessor's
        /// recording (recreating a dead session). An implementation detail
        /// used by front-ends, hidden from help.
        #[arg(long, hide = true)]
        seed_from: Option<std::path::PathBuf>,
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
    /// Connect to a host over SSH in a new session and attach to it. The
    /// session's child is `ssh <target>`; it records and reattaches like any
    /// session, and a new session spawned in its window/group inherits the same
    /// connection (see the SSH sessions feature).
    Ssh {
        /// The host to connect to, as `[user@]host`.
        target: String,
        /// Session name (defaults to `ssh-<host>`, uniquified).
        #[arg(long)]
        name: Option<String>,
        /// Start the session in the background without attaching to it.
        #[arg(short = 'd', long)]
        detached: bool,
        /// Seed the session's screen and scrollback from a predecessor's
        /// recording — reconnecting a dead ssh session with its history in
        /// place. An implementation detail (mirrors `new`'s `--seed-from`),
        /// hidden from help.
        #[arg(long, hide = true)]
        seed_from: Option<std::path::PathBuf>,
        /// Port to connect to (`ssh -p`).
        #[arg(short = 'p', long)]
        port: Option<u16>,
        /// Identity file (`ssh -i`).
        #[arg(short = 'i', long)]
        identity: Option<std::path::PathBuf>,
        /// Jump host (`ssh -J`).
        #[arg(short = 'J', long)]
        jump: Option<String>,
        /// Extra arguments passed through to ssh verbatim (everything after `--`).
        #[arg(last = true)]
        extra: Vec<String>,
    },
    /// List running sessions.
    Ls {
        /// Emit the listing as a JSON array of session objects (one line),
        /// instead of the human-readable table. Used by the remote-fleet
        /// initiator to enumerate a host's sessions over the ssh transport.
        #[arg(long)]
        json: bool,
    },
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
    /// Rename a running session (sets its display name; the session itself —
    /// socket, recording, attach state — is untouched).
    Rename {
        /// Current session name.
        old: String,
        /// New session name.
        new: String,
    },
    /// Search recorded session output for text — a grep over what your sessions
    /// rendered (recordings are compressed, so a plain `grep` can't). Prints one
    /// `session:line: text` per matching line, in `session` order.
    Search {
        /// Text to look for (substring match against each rendered line).
        pattern: String,
        /// Search only this session's recording instead of all of them.
        #[arg(long)]
        session: Option<String>,
        /// Match case-insensitively.
        #[arg(short = 'i', long = "ignore-case")]
        ignore_case: bool,
    },
    /// Export a session's recording as an asciicast (asciinema) stream.
    Export {
        /// Name of the recorded session.
        name: String,
        /// Output file; writes to stdout if omitted.
        output: Option<std::path::PathBuf>,
    },
    /// Relay stdin/stdout to a local session's control socket — the far end of
    /// the SSH transport, run on the host machine as
    /// `ssh <host> -- ghost __pipe <name>`. An internal plumbing command; not
    /// for direct use.
    #[command(name = "__pipe", hide = true)]
    Pipe {
        /// Immutable id of the session to relay to.
        name: String,
    },
    /// Print a machine-readable marker identifying this as a ghost that can host
    /// sessions over the SSH transport (with its protocol level). The initiator
    /// runs it over ssh to decide transport-vs-ssh-child. Internal.
    #[command(name = "__probe", hide = true)]
    Probe,
}

/// What a parsed command line asks the `ghost` binary to do.
pub enum Launch {
    /// A subcommand ran to completion; the process should exit.
    Handled,
    /// No subcommand — launch the windowed UI. `fresh` skips restoring the
    /// windows open at last quit.
    Gui { fresh: bool },
}

/// Parse the command line; if it names a subcommand, run it and return
/// [`Launch::Handled`] (the caller should exit). With no subcommand, return
/// [`Launch::Gui`] so the caller launches the GUI. The session-host re-exec check
/// must already have run (it consumes the internal `__host` argv that clap would
/// otherwise reject).
pub fn run_subcommand() -> Launch {
    let cli = Cli::parse();
    match cli.command {
        Some(command) => {
            dispatch(command);
            Launch::Handled
        }
        None => Launch::Gui { fresh: cli.fresh },
    }
}

fn dispatch(command: Command) {
    match command {
        Command::New {
            name,
            detached,
            defer,
            cwd,
            seed_from,
            no_record,
            scrollback,
            max_recording_size,
            command,
        } => {
            let name = name.unwrap_or_else(default_name);
            // A new id must not shadow a name some session already answers to
            // (as an id — spawn itself refuses that — or as a display name),
            // so `attach`/`kill`/`rename` lookups stay unambiguous.
            if let Ok(sessions) = session::list()
                && sessions.iter().any(|s| s.display() == name)
            {
                fail(&format!("a session named '{name}' already exists"));
            }
            let record = (!no_record).then(|| ghost_vt::paths::recording_path(&name));
            let opts = SpawnOpts {
                name: name.clone(),
                command,
                size: (80, 24),
                cwd,
                record,
                seed_from,
                scrollback,
                max_recording_bytes: Some(max_recording_size),
                // Attached sessions (the default, and `--defer`) start their
                // child on the attach handshake; a plain `-d` starts it now.
                start_on_attach: !detached || defer,
                connection: None,
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
        Command::Ssh {
            target,
            name,
            detached,
            seed_from,
            port,
            identity,
            jump,
            extra,
        } => {
            let Some(mut spec) = ConnectionSpec::parse_target(&target) else {
                fail(&format!("invalid ssh target '{target}'"));
            };
            spec.port = port;
            spec.identity = identity;
            spec.jump = jump;
            spec.extra = extra;
            // A given name must be free (like `ghost new`); a derived name is
            // uniquified instead — repeat connections to one host are routine.
            let taken: Vec<String> = session::list()
                .unwrap_or_default()
                .into_iter()
                .flat_map(|s| [s.display().to_string(), s.name])
                .collect();
            let name = match name {
                Some(n) => {
                    if taken.contains(&n) {
                        fail(&format!("a session named '{n}' already exists"));
                    }
                    n
                }
                None => unique_ssh_name(&spec.host, &taken),
            };

            // Prefer the transport: a real ghost host running on the remote,
            // reached by tunnelling our protocol over ssh. Fall back to the local
            // ssh *child* only when the remote can't host ghost.
            let remote = match ghost_vt::remote::RemoteSsh::new(spec.clone()) {
                Ok(r) => r,
                Err(e) => fail(&e.to_string()),
            };
            match remote.negotiate() {
                Some(remote_ghost) => {
                    if let Err(e) = remote.spawn_host(&remote_ghost, &name) {
                        fail(&format!("failed to start the remote host: {e}"));
                    }
                    if detached {
                        println!("started remote session '{name}' on {}", spec.target());
                    } else if let Err(e) =
                        client::attach_ssh(remote.pipe_command(&remote_ghost, &name))
                    {
                        fail(&format!(
                            "remote session '{name}' started but attach failed: {e}"
                        ));
                    }
                }
                None => ssh_child_fallback(spec, name, detached, seed_from),
            }
        }
        Command::Ls { json } => match session::list() {
            Ok(sessions) if json => match serde_json::to_string(&sessions) {
                Ok(s) => println!("{s}"),
                Err(e) => fail(&e.to_string()),
            },
            Ok(sessions) => {
                for s in sessions {
                    if s.title.is_empty() {
                        println!("{}\t(pid {})", s.display(), s.pid);
                    } else {
                        println!("{}\t(pid {})\t{}", s.display(), s.pid, s.title);
                    }
                }
            }
            Err(e) => fail(&e.to_string()),
        },
        Command::Attach { name } => {
            let Some(name) = name else {
                fail("specify a session to attach to (see `ghost ls`)");
            };
            if let Err(e) = client::attach(&resolve(&name)) {
                fail(&e.to_string());
            }
        }
        Command::Kill { name } => match session::kill_session(&resolve(&name)) {
            Ok(true) => println!("killed session '{name}'"),
            Ok(false) => fail(&format!("no such session '{name}'")),
            Err(e) => fail(&e.to_string()),
        },
        Command::Rename { old, new } => match client::rename(&resolve(&old), &new) {
            Ok(()) => println!("renamed '{old}' to '{new}'"),
            Err(e) => fail(&e.to_string()),
        },
        Command::Search {
            pattern,
            session,
            ignore_case,
        } => {
            let only = session.as_deref().map(resolve);
            match ghost_vt::search::search(&pattern, ignore_case, only.as_deref()) {
                Ok(hits) => {
                    for hit in &hits {
                        println!("{}:{}: {}", hit.session, hit.line, hit.text);
                    }
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Command::Export { name, output } => {
            if let Err(e) = export(&resolve(&name), output.as_deref()) {
                fail(&e.to_string());
            }
        }
        // The transport addresses sessions by immutable id, so relay to the raw
        // name (no display-name resolution).
        Command::Pipe { name } => {
            if let Err(e) = ghost_vt::pipe::run(&name) {
                fail(&e.to_string());
            }
        }
        Command::Probe => println!("{}", ghost_vt::remote::probe_line()),
    }
}

/// Resolve a user-typed name to a session's immutable id. An exact id match
/// wins; otherwise a display-name match (unique by construction — the host
/// refuses colliding renames) maps back to the id it labels. An unknown name
/// passes through so the callee reports its usual error.
fn resolve(name: &str) -> String {
    let Ok(sessions) = session::list() else {
        return name.to_string();
    };
    if sessions.iter().any(|s| s.name == name) {
        return name.to_string();
    }
    match sessions.iter().find(|s| s.display() == name) {
        Some(s) => s.name.clone(),
        None => name.to_string(),
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

/// A session name for an ssh connection to `host`: `ssh-<host>`, or
/// `ssh-<host>-N` if that (or a lower suffix) is already `taken` — repeat
/// connections to one host are routine, so we uniquify rather than refuse.
fn unique_ssh_name(host: &str, taken: &[String]) -> String {
    let base = format!("ssh-{host}");
    if !taken.iter().any(|t| t == &base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|cand| !taken.iter().any(|t| t == cand))
        .expect("an unused suffix always exists")
}

/// The local ssh-*child* realization of a connection (a session whose child is
/// `ssh <target>`): the fallback when the remote can't host a ghost. This is the
/// original `ghost ssh` behaviour (P1–P5); the connection is stored so a dead
/// session reconnects on relaunch and new sessions in its group inherit it.
fn ssh_child_fallback(
    spec: ConnectionSpec,
    name: String,
    detached: bool,
    seed_from: Option<std::path::PathBuf>,
) {
    let opts = SpawnOpts {
        name: name.clone(),
        // Empty: the connection derives the child argv (`ssh …`).
        command: Vec::new(),
        size: (80, 24),
        cwd: None,
        record: Some(ghost_vt::paths::recording_path(&name)),
        seed_from,
        scrollback: ghost_vt::screen::DEFAULT_SCROLLBACK,
        max_recording_bytes: Some(ghost_vt::record::DEFAULT_MAX_RECORDING_BYTES),
        // Attached (the default) starts ssh on the attach handshake so its
        // terminal queries reach a real client; `-d` starts it now.
        start_on_attach: !detached,
        connection: Some(spec),
    };
    if let Err(e) = server::spawn(opts) {
        fail(&e.to_string());
    }
    if detached {
        println!("started session '{name}'");
    } else if let Err(e) = client::attach(&name) {
        fail(&format!("session '{name}' started but attach failed: {e}"));
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("ghost: {msg}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn bare_launch_parses_with_fresh_off() {
        let cli = Cli::try_parse_from(["ghost"]).unwrap();
        assert!(cli.command.is_none());
        assert!(!cli.fresh);
    }

    #[test]
    fn a_bare_fresh_flag_falls_through_to_the_gui() {
        let cli = Cli::try_parse_from(["ghost", "--fresh"]).unwrap();
        assert!(cli.command.is_none(), "--fresh is not a subcommand");
        assert!(cli.fresh);
    }

    #[test]
    fn ssh_parses_target_and_all_flags() {
        let cli = Cli::try_parse_from([
            "ghost",
            "ssh",
            "dev@box",
            "-p",
            "2222",
            "-i",
            "/home/k/id",
            "-J",
            "bastion",
            "--name",
            "work",
            "-d",
            "--",
            "-o",
            "ForwardAgent=yes",
        ])
        .unwrap();
        let Some(Command::Ssh {
            target,
            name,
            detached,
            seed_from,
            port,
            identity,
            jump,
            extra,
        }) = cli.command
        else {
            panic!("expected an ssh command");
        };
        assert_eq!(target, "dev@box");
        assert_eq!(name.as_deref(), Some("work"));
        assert!(detached);
        assert_eq!(seed_from, None, "seed_from is an internal reconnect flag");
        assert_eq!(port, Some(2222));
        assert_eq!(
            identity.as_deref(),
            Some(std::path::Path::new("/home/k/id"))
        );
        assert_eq!(jump.as_deref(), Some("bastion"));
        assert_eq!(extra, vec!["-o", "ForwardAgent=yes"]);
    }

    #[test]
    fn ssh_takes_a_bare_host() {
        let cli = Cli::try_parse_from(["ghost", "ssh", "box"]).unwrap();
        let Some(Command::Ssh {
            target, port, name, ..
        }) = cli.command
        else {
            panic!("expected an ssh command");
        };
        assert_eq!(target, "box");
        assert_eq!(port, None);
        assert_eq!(name, None);
    }

    #[test]
    fn unique_ssh_name_derives_and_uniquifies() {
        assert_eq!(unique_ssh_name("box", &[]), "ssh-box");
        assert_eq!(
            unique_ssh_name("box", &["ssh-box".to_string()]),
            "ssh-box-2"
        );
        assert_eq!(
            unique_ssh_name("box", &["ssh-box".to_string(), "ssh-box-2".to_string()],),
            "ssh-box-3"
        );
    }

    #[test]
    fn search_parses_pattern_flags_and_session() {
        let cli = Cli::try_parse_from(["ghost", "search", "boom"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Search {
                pattern,
                session: None,
                ignore_case: false,
            }) if pattern == "boom"
        ));

        let cli =
            Cli::try_parse_from(["ghost", "search", "-i", "--session", "web", "warn"]).unwrap();
        let Some(Command::Search {
            pattern,
            session,
            ignore_case,
        }) = cli.command
        else {
            panic!("expected a search command");
        };
        assert_eq!(pattern, "warn");
        assert_eq!(session.as_deref(), Some("web"));
        assert!(ignore_case);
    }

    #[test]
    fn fresh_is_global_so_it_can_follow_a_subcommand() {
        // `global = true` means the flag is accepted anywhere; it just has no
        // effect with a subcommand present (nothing to restore).
        let cli = Cli::try_parse_from(["ghost", "ls", "--fresh"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Ls { json: false })));
        assert!(cli.fresh);
    }
}
