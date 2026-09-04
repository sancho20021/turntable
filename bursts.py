#!/usr/bin/env python3
"""Groups clickscan.py events into physical bursts and bins them by condition.

clickscan.py reports one row per sample per channel, so a single audible click
shows up as a dozen rows. What matters is how many distinct physical events
happened, and - for an A/B test - how many fell in each condition window.

Events within --gap seconds of each other count as one burst.

  ./bursts.py post.scan
  ./bursts.py post.scan --schedule /dev/shm/ab/abtest.log
  ./bursts.py post.scan --windows 0:340:chargerIN 360:700:BATTERY 720:1060:chargerIN2

"""

import argparse
import os
import re
import sys


def parse_scan(path):
    """(time, channel, step, mod_block) for every row clickscan printed."""
    if not os.path.exists(path):
        print(f"ERROR: no such scan file: {path}", file=sys.stderr)
        print("  dualcap.sh writes pre.scan and post.scan into its output "
              "directory.", file=sys.stderr)
        sys.exit(2)
    rows = []
    for line in open(path):
        m = re.match(r'\s+([\d.]+)\s+(\d+)\s+(\d+)\s+([+-][\d.]+)\s+(\d+)\s*$', line)
        if m:
            rows.append((float(m.group(1)), int(m.group(3)),
                         float(m.group(4)), int(m.group(5))))
    return sorted(rows)


def group(rows, gap):
    bursts = []
    for t, c, d, b in rows:
        if bursts and t - bursts[-1][-1][0] <= gap:
            bursts[-1].append((t, c, d, b))
        else:
            bursts.append([(t, c, d, b)])
    return bursts


def windows_from_schedule(path):
    """Reads the `windowX<TAB>start<TAB>end<TAB>label` lines abtest.sh writes."""
    out = []
    for line in open(path):
        parts = line.rstrip("\n").split("\t")
        if len(parts) == 4 and parts[0].startswith("window"):
            out.append((float(parts[1]), float(parts[2]), parts[3]))
    return out


def schedule_t0(path):
    """abtest.sh's own epoch, from its `t0=` line."""
    for line in open(path):
        if line.startswith("t0="):
            return float(line[3:])
    return None


def capture_t0(path):
    """dualcap.sh's capture epoch, from its capture.log."""
    for line in open(path):
        parts = line.rstrip("\n").split("\t")
        if parts and parts[0] == "capture_t0":
            return float(parts[1])
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("scan")
    ap.add_argument("--gap", type=float, default=0.5,
                    help="events closer than this are one burst (default 0.5s)")
    ap.add_argument("--schedule", help="abtest.sh log to take windows from")
    ap.add_argument("--windows", nargs="*", default=[],
                    help="start:end:label triples, in SCAN time (no offset is "
                         "applied to these, unlike --schedule windows)")
    ap.add_argument("--capture-log",
                    help="dualcap.sh capture.log; used with --schedule to align "
                         "the two clocks automatically")
    ap.add_argument("--offset", type=float,
                    help="seconds to add to schedule times to reach scan times "
                         "(overrides --capture-log)")
    args = ap.parse_args()

    rows = parse_scan(args.scan)
    if not rows:
        print(f"{args.scan}: no events (a CLEAN or VOID scan). Nothing to group.")
        return 0
    bursts = group(rows, args.gap)

    print(f"{args.scan}: {len(rows)} rows -> {len(bursts)} distinct bursts\n")
    print(f"  {'t(s)':>9}  {'rows':>4}  {'max step':>8}  {'span(ms)':>8}  {'aligned':>7}")
    for bl in bursts:
        t0, t1 = bl[0][0], bl[-1][0]
        al = sum(1 for _, _, _, b in bl if b == 0)
        print(f"  {t0:9.2f}  {len(bl):4}  {max(abs(d) for _, _, d, _ in bl):8.4f}  "
              f"{(t1 - t0) * 1000:8.1f}  {al:3}/{len(bl):<3}")

    # The capture starts before the schedule does, so the two clocks have
    # different origins and schedule windows need shifting into scan time.
    offset = 0.0
    if args.offset is not None:
        offset = args.offset
        print(f"  clock offset: {offset:+.2f}s (given)\n")
    elif args.schedule and args.capture_log:
        st, ct = schedule_t0(args.schedule), capture_t0(args.capture_log)
        if st is None or ct is None:
            print("  WARNING: could not read both epochs; assuming zero offset.\n")
        else:
            offset = st - ct
            print(f"  clock offset: schedule t=0 is at capture t={offset:+.2f}s\n")
    elif args.schedule:
        print("  WARNING: no --capture-log or --offset given, so the schedule\n"
              "  is read as if it shares an origin with the capture. Pass\n"
              "  --capture-log for the real alignment.\n")

    wins = [(a + offset, b + offset, l)
            for a, b, l in (windows_from_schedule(args.schedule) if args.schedule else [])]
    for w in args.windows:
        a, b, label = w.split(":")
        wins.append((float(a), float(b), label))
    if not wins:
        return 0

    print(f"\n  {'condition':<16} {'window(s)':>16} {'bursts':>7} {'per min':>8}")
    for a, b, label in wins:
        n = sum(1 for bl in bursts if a <= bl[0][0] <= b)
        mins = (b - a) / 60.0
        print(f"  {label:<16} {f'{a:.0f} to {b:.0f}':>16} {n:>7} {n / mins:>8.2f}")

    excluded = sum(1 for bl in bursts
                   if not any(a <= bl[0][0] <= b for a, b, _ in wins))
    if excluded:
        print(f"\n  ({excluded} burst(s) fell in the switch gaps between windows "
              f"and are not counted.)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
