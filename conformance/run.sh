#!/usr/bin/env bash
#
# Run the esctest2 terminal-conformance suite against ghost's headless model.
#
# esctest2 (conformance/esctest2, GPLv2) runs as a SEPARATE child process — it
# links into nothing that ships. It writes control sequences to its stdout and
# reads ghost's replies (CPR, text-area size, …) from its stdin; the `ghost`
# binary, gated by GHOST_ESCTEST, wires it to the same TerminalModel the GUI
# uses (see ghost-ui/src/main.rs::esctest_host). See conformance/README.md.
#
# Usage:
#   conformance/run.sh                 # default cursor/CPR include-set
#   INCLUDE='CUP|CUU' conformance/run.sh
#   MAX_VT_LEVEL=3 INCLUDE='CHA' conformance/run.sh
#   GHOST_BIN=target/release/ghost conformance/run.sh   # your binary, never built
#   GHOST_BUILD=0 conformance/run.sh                    # skip the rebuild
#
# Exit status: 0 if the suite ran and nothing failed; 1 if any test FAILED or
# the harness could not produce a report; 0 with a SKIP note if python3 is
# absent (so CI can opt in without hard-failing where python3 is unavailable).

set -u

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
esctest="$here/esctest2/esctest/esctest.py"

# The binary under test. An explicit GHOST_BIN is the caller's own artifact (a
# release build, a CI download) — run it as given, never rebuild it. The default
# debug binary we *do* build ourselves: this is a dev loop, and a stale
# target/debug/ghost silently tests code you no longer have (an easy hour to
# lose chasing a phantom pass or fail). GHOST_BUILD=0 opts out; a missing cargo
# just falls through to the not-found check below.
GHOST_BIN="${GHOST_BIN:-}"
if [ -z "$GHOST_BIN" ]; then
  GHOST_BIN="$repo/target/debug/ghost"
  if [ "${GHOST_BUILD:-1}" != "0" ] && command -v cargo >/dev/null 2>&1; then
    echo "building ghost…"
    (cd "$repo" && cargo build -p ghost-ui) || exit 1
  fi
fi
# Default: a small SMOKE set of families that currently pass clean (measured
# without --force, so the assertions really run), giving a bare `run.sh` a
# green exit. Class-anchored (`re.search` over `Class.test_name`, so a loose
# `EL` would also match `CIELab`). The broader suite has many tracked real
# failures — see README "Known findings"; widen this set as they're fixed.
INCLUDE="${INCLUDE:-CUUTests|CUDTests|CBTTests|HPATests}"
MAX_VT_LEVEL="${MAX_VT_LEVEL:-4}"
TIMEOUT_SECS="${GHOST_ESCTEST_TIMEOUT_SECS:-120}"

if ! command -v python3 >/dev/null 2>&1; then
  echo "SKIP: python3 not found — esctest needs it (stdlib only)."
  exit 0
fi
if [ ! -x "$GHOST_BIN" ]; then
  echo "ERROR: ghost binary not found at $GHOST_BIN"
  echo "       build it:  cargo build -p ghost-ui"
  exit 1
fi
if [ ! -f "$esctest" ]; then
  echo "ERROR: vendored esctest2 missing at $esctest"
  exit 1
fi

# Isolate the XDG dirs so the harness never touches real sessions or recordings
# (ghost's server::spawn reads these). A tempdir, torn down on exit. The runtime
# dir holds the control socket, whose path must fit `sockaddr_un` (~104 bytes),
# so keep it short (/tmp/ges.XXXXXX/r) rather than nested under $TMPDIR, which
# on macOS is already long.
work="$(mktemp -d /tmp/ges.XXXXXX)"
trap 'rm -rf "$work"' EXIT
export XDG_DATA_HOME="$work/data"
export XDG_STATE_HOME="$work/state"
export XDG_CONFIG_HOME="$work/config"
export XDG_CACHE_HOME="$work/cache"
export XDG_RUNTIME_DIR="$work/r"
mkdir -p "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

log="$work/esctest.log"

# The esctest invocation. NOTE: do NOT pass --force — in esctest2 that turns
# every assertion into a no-op (`Raise()` only raises when force is off), so
# tests "pass" without checking anything. Failures still don't stop the suite:
# RunTest catches each test's exception and moves on, tallying pass/fail.
# --no-print-logs keeps esctest from dumping the whole log back into the PTY on
# exit (we read $log directly). Single-quote paths for the host's `sh -c`.
esctest_cmd="python3 '$esctest' --expected-terminal xterm --max-vt-level $MAX_VT_LEVEL --include '$INCLUDE' --logfile '$log' --no-print-logs"

echo "ghost:   $GHOST_BIN"
echo "include: $INCLUDE   (max-vt-level $MAX_VT_LEVEL)"
echo "running esctest…"

GHOST_ESCTEST="$esctest_cmd" GHOST_ESCTEST_TIMEOUT_SECS="$TIMEOUT_SECS" "$GHOST_BIN" || true

if [ ! -f "$log" ]; then
  echo "ERROR: no esctest logfile produced (harness never ran a test)."
  exit 1
fi

# esctest2 exits 0 even on failure, so decide from the summary line:
#   "*** N tests passed, K known bugs, M TESTS FAILED ***"  (some failed)
#   "*** N tests passed, K known bugs, 0 test failed ***"   (clean)
summary="$(grep -aE '\*\*\* .*(passed|failed|FAILED).*\*\*\*' "$log" | tail -1)"
echo "----------------------------------------"
if [ -z "$summary" ]; then
  echo "ERROR: no summary line in $log — tail follows:"
  tail -20 "$log"
  exit 1
fi
echo "$summary"
echo "----------------------------------------"

# esctest capitalises "FAILED" only in the failed>0 branch ("1 TEST FAILED" /
# "N TESTS FAILED"); the clean branch says "0 test failed" (lowercase). So an
# uppercase FAILED in the summary is the reliable failure signal.
if printf '%s' "$summary" | grep -q 'FAILED'; then
  echo "Failing tests:"
  sed -n '/Failing tests:/,$p' "$log" | sed '1d' | sed '/^$/d' | head -60
  exit 1
fi
echo "PASS"
exit 0
