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

## Searching recorded output

Don't `grep` the recording files — they're framed-zstd, so a raw grep finds
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
