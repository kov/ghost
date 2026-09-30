#!/usr/bin/env python3
"""Turn scroll captures into ghost's kinetic-scrolling fixtures.

  to_fixture.py macos <probe.jsonl>...   # scroll-probe logs (macOS host)
  to_fixture.py wev <wev.log>            # `wev -f wl_pointer` (Linux)

Writes to stdout, one gesture per block, times in ms from the gesture's
first event, deltas signed like ghost's WheelDelta (positive = up into
history):

  gesture [label]         start of a gesture (Linux captures are labelled
                          by hand: flick, held — fingers stopped before
                          the lift — or drag)
  f <t> <dy>              finger travel
  lift <t>                fingers lifted (macOS: phase ended; Linux: axis_stop)
  m <t> <dy>              macOS momentum (its coasting after the lift)
"""
import json
import re
import sys


def macos(paths):
    for path in paths:
        print(f"# macOS scroll-probe: {path.split('/')[-1]}")
        gesture = None
        for line in open(path):
            e = json.loads(line)
            phase, mom = e["phase"], e["momentum"]
            if phase == "began":
                gesture = {"t0": e["t"], "f": [], "lift": None, "m": []}
            if gesture is None:
                continue
            ms = lambda t: (t - gesture["t0"]) * 1000
            # scroll-probe logs AppKit's scrollingDeltaY, which winit passes
            # through as PixelDelta.y: already WheelDelta's sign.
            if phase in ("began", "changed"):
                gesture["f"].append((ms(e["t"]), e["dy"]))
            elif phase == "ended":
                gesture["lift"] = ms(e["t"])
                print_gesture(gesture)
            elif mom != "none" and gesture["lift"] is not None:
                print(f"m {ms(e['t']):.1f} {e['dy']:g}")


def print_gesture(g):
    print("gesture")
    for t, dy in g["f"]:
        print(f"f {t:.1f} {dy:g}")
    print(f"lift {g['lift']:.1f}")


def wev(path):
    print(f"# wev -f wl_pointer: {path.split('/')[-1]}")
    fingers = []
    for line in open(path):
        m = re.search(r"axis: time: (\d+); axis: 0 \(vertical\), value: ([-\d.]+)", line)
        if m:
            # Wayland's sign is the inverse of winit's (and so of WheelDelta).
            fingers.append((int(m[1]), -float(m[2])))
            continue
        m = re.search(r"axis_stop: time: (\d+); axis: 0 \(vertical\)", line)
        if m and fingers:
            t0 = fingers[0][0]
            print("gesture")
            for t, dy in fingers:
                print(f"f {t - t0} {dy:g}")
            print(f"lift {int(m[1]) - t0}")
            fingers = []


if __name__ == "__main__":
    kind, *paths = sys.argv[1:]
    if kind == "macos":
        macos(paths)
    else:
        wev(paths[0])
