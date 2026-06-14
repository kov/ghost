//! PTY ownership: spawn the child process under a pseudo-terminal, expose its
//! master fd for the host's `poll()` loop, and propagate window-size changes
//! (`TIOCSWINSZ` / `SIGWINCH`) to the child.
