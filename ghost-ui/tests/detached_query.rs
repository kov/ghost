//! E2E: a detached/never-attached session answers terminal queries itself.
//!
//! While attached, the client (a real terminal or VTE) answers queries like
//! cursor-position. With no client attached, nobody would — so a program that
//! queries on startup and blocks on the reply (a shell) stalls. The host fills
//! that gap, replying from its own screen state while detached.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const GHOST: &str = env!("CARGO_BIN_EXE_ghost");

fn ghost(xdg: &Path) -> Command {
    let mut c = Command::new(GHOST);
    c.env("XDG_RUNTIME_DIR", xdg.join("run"));
    c.env("XDG_DATA_HOME", xdg.join("data"));
    c
}

fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if pred() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

struct KillOnDrop<'a> {
    xdg: &'a Path,
    name: &'a str,
}

impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        let _ = ghost(self.xdg).args(["kill", self.name]).output();
    }
}

#[test]
fn detached_session_answers_cursor_position_query() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "cpr-detached";
    let _guard = KillOnDrop { xdg, name };

    // The eager, never-attached child emits a cursor-position query (CSI 6 n) and
    // waits up to 2s for the reply (terminated by `R`). It touches the sentinel
    // only if the reply arrives — and since no client is attached, only the host
    // could have sent it.
    let sentinel = xdg.join("cpr-answered");
    let script = format!(
        "printf '\\033[6n'; if IFS= read -r -s -d R -t 2 _; then touch '{}'; fi; exec sleep 60",
        sentinel.display()
    );
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "bash", "-c", &script])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new -d` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        wait_until(Duration::from_secs(5), || sentinel.exists()),
        "detached session did not answer the cursor-position query"
    );
}

#[test]
fn detached_session_answers_decrqm_mode_query() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "decrqm-detached";
    let _guard = KillOnDrop { xdg, name };

    // The never-attached child enables synchronized output (2026), then asks
    // DECRQM whether it is set. The DECRPM reply ends in `y`; it must report
    // the mode as set (`?2026;1$`) — apps (neovim among them) only use 2026 if
    // this query answers. Only the host could have replied.
    let sentinel = xdg.join("decrqm-answered");
    let script = format!(
        "printf '\\033[?2026h\\033[?2026$p'; \
         if IFS= read -r -s -d y -t 2 reply; then case \"$reply\" in *'?2026;1$'*) touch '{}';; esac; fi; \
         exec sleep 60",
        sentinel.display()
    );
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "bash", "-c", &script])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new -d` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        wait_until(Duration::from_secs(5), || sentinel.exists()),
        "detached session did not answer the DECRQM query"
    );
}

#[test]
fn detached_session_answers_background_color_query() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "osc11-detached";
    let _guard = KillOnDrop { xdg, name };

    // The never-attached child asks for the default background (OSC 11 ;?),
    // the query vim/fzf theme detection rides on. Detached, the host answers
    // with ghost's default scheme; the ST-terminated reply ends in backslash.
    let sentinel = xdg.join("osc11-answered");
    let script = format!(
        "printf '\\033]11;?\\033\\\\'; \
         if IFS= read -r -s -d '\\' -t 2 reply; then case \"$reply\" in *'rgb:1010/1010/1212'*) touch '{}';; esac; fi; \
         exec sleep 60",
        sentinel.display()
    );
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "bash", "-c", &script])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new -d` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        wait_until(Duration::from_secs(5), || sentinel.exists()),
        "detached session did not answer the OSC 11 color query"
    );
}

#[test]
fn detached_session_answers_color_scheme_query() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "scheme-detached";
    let _guard = KillOnDrop { xdg, name };

    // The never-attached child asks dark-or-light (CSI ? 996 n, mode 2031's
    // query form). Detached, the host answers from ghost's default scheme:
    // dark, `CSI ? 997 ; 1 n`.
    let sentinel = xdg.join("scheme-answered");
    let script = format!(
        "printf '\\033[?996n'; \
         if IFS= read -r -s -d 'n' -t 2 reply; then case \"$reply\" in *'[?997;1'*) touch '{}';; esac; fi; \
         exec sleep 60",
        sentinel.display()
    );
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "bash", "-c", &script])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new -d` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        wait_until(Duration::from_secs(5), || sentinel.exists()),
        "detached session did not answer the color-scheme query"
    );
}

#[test]
fn detached_session_answers_xtversion_query() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "xtversion-detached";
    let _guard = KillOnDrop { xdg, name };

    // The never-attached child sends XTVERSION (CSI > 0 q); the DCS reply is
    // ST-terminated (trailing backslash) and must name ghost.
    let sentinel = xdg.join("xtversion-answered");
    let script = format!(
        "printf '\\033[>0q'; \
         if IFS= read -r -s -d '\\' -t 2 reply; then case \"$reply\" in *ghost*) touch '{}';; esac; fi; \
         exec sleep 60",
        sentinel.display()
    );
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "bash", "-c", &script])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new -d` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        wait_until(Duration::from_secs(5), || sentinel.exists()),
        "detached session did not answer the XTVERSION query"
    );
}

#[test]
fn detached_session_answers_kitty_graphics_query() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "graphics-detached";
    let _guard = KillOnDrop { xdg, name };

    // The never-attached child sends the standard kitty-graphics support probe (a
    // 1×1 image with a=q) and reads up to 2s for the reply, whose APC terminator
    // is a trailing backslash. The reply carries `OK` only if the host decoded the
    // image and acknowledged it — and with no client attached, only the host could
    // have answered.
    let sentinel = xdg.join("graphics-answered");
    let script = format!(
        "printf '\\033_Gi=31,a=q,f=24,s=1,v=1;AAAA\\033\\\\'; \
         if IFS= read -r -s -d '\\' -t 2 reply; then case \"$reply\" in *OK*) touch '{}';; esac; fi; \
         exec sleep 60",
        sentinel.display()
    );
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "bash", "-c", &script])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new -d` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        wait_until(Duration::from_secs(5), || sentinel.exists()),
        "detached session did not answer the kitty-graphics query"
    );
}
