# ghost

See `README.md` for what ghost is, the architecture ("How it works"), storage
layout, and usage. This file is only the rules that aren't obvious from the
code.

## Test-first is the law

Every fix or feature starts with a **failing test**, then code to green. No
exceptions without asking. Prefer driving the real `ghost` binary end-to-end
(`ghost-ui/tests/`) over unit tests when the behaviour is observable there. The
`ghost` binary is the GUI with the CLI subcommands folded in (`ghost-ui` crate +
`ghost-cli` library), so its PTY E2E suite lives in the `ghost-ui` crate.

E2E tests drive the binary over a PTY and assert on the **screen** (feed output
into a `vt` emulator), never on raw bytes. Sync is read-until-predicate with a
timeout (`wait_until`), never fixed sleeps. XDG dirs are redirected to a tempdir
(`set_xdg`) so the suite never touches real sessions or recordings. Reuse those
helpers.

## Launching ghost by hand leaks hosts

A session's `ghost __host` outlives its client on purpose — that is the whole
feature — so killing a hand-launched `ghost` leaves the host running, holding an
inotify instance. The per-user cap is 128 (`fs.inotify.max_user_instances`), and
once it is gone the watch/title tests fail **fast** (~0.1s, "never propagated")
and read exactly like a regression. `inotify_init()` returning -1 is the tell.

So end a manual run through the CLI, in the same XDG env, *before* removing its
dirs:

```sh
env XDG_RUNTIME_DIR=/tmp/probe-rt ... ghost kill --all   # and only then rm -rf
```

Wiping the runtime dir first orphans the hosts for good: the socket they would
be reached through is gone. Give each run its own `XDG_RUNTIME_DIR`,
`XDG_CONFIG_HOME` and `XDG_DATA_HOME` so `kill --all` can never touch a real
session, and reuse the same dirs across runs rather than making new ones.

Cleaning up by pattern is the user's call, not ours — `pkill -f ghost` matches
system-wide and kills their live sessions. Report leftovers instead.

## Searching recorded output

Don't `grep` the recording files — they're framed-brotli, so a raw grep finds
nothing. Use `ghost search <pattern>` (`-i` for case-insensitive, `--session
<name>` to scope to one). It replays each recording through the emulator and
greps the *rendered* lines, printing `session:line: text`. Reach for it whenever
you'd otherwise hunt through `~/.local/share/ghost/recordings`.

## `ghost-term` — our owned terminal core (forked from avt)

`ghost-term/` began as a fork of asciinema's `avt`; it is now **ours**. Diverge
freely where it makes ghost's terminal better (cursor shape, hyperlinks,
bytes-feed, damage tracking, …) — it is no longer kept rebase-close to upstream;
cherry-pick upstream fixes by hand when worthwhile.

**License/attribution — do not break.** avt is Apache-2.0 and we cannot
relicense it to MIT, so `ghost-term` keeps `license = "Apache-2.0"` (NOT the
workspace's `MIT OR Apache-2.0`). Keep the `LICENSE` file, the Marcin Kulik /
asciinema attribution, and the fork notice (README + crate docs) recording our
changes (Apache-2.0 §4(b)). The rest of the workspace stays `MIT OR Apache-2.0`
and depends on it — a normal mixed-license tree.

## Lint & format gates

`.githooks/pre-commit` is canonical — enable it with
`git config core.hooksPath .githooks`. It runs, and you should run, exactly:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Clippy is `-D warnings` across the whole workspace, `ghost-term` included — we
own it, so its lints get fixed like any of our code (a scoped `#[allow]` with a
reason only where a lint is genuinely noise).
