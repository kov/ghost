# ghost

See `README.md` for what ghost is, the architecture ("How it works"), storage
layout, and usage. This file is only the rules that aren't obvious from the
code.

## Test-first is the law

Every fix or feature starts with a **failing test**, then code to green. No
exceptions without asking. Prefer driving the real `ghost` binary end-to-end
(`ghost-cli/tests/`) over unit tests when the behaviour is observable there.

E2E tests drive the binary over a PTY and assert on the **screen** (feed output
into a `vt` emulator), never on raw bytes. Sync is read-until-predicate with a
timeout (`wait_until`), never fixed sleeps. XDG dirs are redirected to a tempdir
(`set_xdg`) so the suite never touches real sessions or recordings. Reuse those
helpers.

## `vt/` is a vendored fork — treat it as upstream

`vt/` is a fork of asciinema's `avt` (package name `avt`, used here as the dep
`vt`). Keep changes **minimal and structurally close to upstream** so rebases
stay cheap; don't refactor or restyle it. Anything worth upstreaming goes in the
contribution tracker (see memory) before it drifts.

## Lint & format gates

`.githooks/pre-commit` is canonical — enable it with
`git config core.hooksPath .githooks`. It runs, and you should run, exactly:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --exclude avt --all-targets -- -D warnings
cargo clippy -p avt --lib -- -D warnings
```

Clippy is `-D warnings`. The split is intentional: lint our crates fully, but
avt only as a **library** — its benches/tests trip newer lints upstream ignores,
and linting them just causes rebase churn. Don't "fix" those.
