#!/usr/bin/env python3
"""Finds discontinuities in a recording of a steady test tone.

For a sine of frequency f at sample rate fs with peak amplitude p, the largest
possible step between two adjacent samples is p * 2*pi*f/fs. Anything larger
came from a dropout, a splice, or an unsmoothed parameter change. That makes
"is there a click in here" arithmetic, over a whole 20-minute capture.

Usage:
    ./clickscan.py capture.wav [--hz 1000] [--mult 3.0] [--block 256]

Exit status:  0 = clean,  1 = discontinuities found,  2 = nothing measurable.

Needs only ffmpeg/ffprobe and the standard library.
"""

import argparse
import array
import math
import os
import subprocess
import sys
from collections import Counter

# Bytes of raw audio pulled from ffmpeg per read. Memory stays flat regardless
# of capture length.
CHUNK_BYTES = 1 << 20

# Slices used to estimate the tone's amplitude, and how long each one is.
PROBE_POINTS = 3
PROBE_SECONDS = 20.0

# A capture shorter than this cannot say anything useful about a fault that
# happens once a minute.
MIN_SECONDS = 2.0


class Unreadable(Exception):
    pass


def probe(path):
    """Sample rate, channel count and duration, or a readable error."""
    if not os.path.exists(path):
        raise Unreadable(f"no such file: {path}")
    r = subprocess.run(
        ["ffprobe", "-v", "error", "-select_streams", "a:0", "-show_entries",
         "stream=sample_rate,channels", "-show_entries", "format=duration",
         "-of", "default=nw=1:nk=1", path],
        capture_output=True, text=True)
    if r.returncode != 0:
        raise Unreadable(f"not a readable audio file: {path}")
    vals = [v for v in r.stdout.split() if v]
    if len(vals) < 3:
        raise Unreadable(f"no audio stream found in {path}")
    return int(vals[0]), int(vals[1]), float(vals[2])


def raw_stream(path, start=None, length=None):
    """Yields interleaved f32 chunks decoded by ffmpeg."""
    cmd = ["ffmpeg", "-v", "error"]
    if start is not None:
        cmd += ["-ss", f"{start:.6f}"]
    if length is not None:
        cmd += ["-t", f"{length:.6f}"]
    cmd += ["-i", path, "-f", "f32le", "-c:a", "pcm_f32le", "-"]
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE)
    try:
        leftover = b""
        while True:
            buf = p.stdout.read(CHUNK_BYTES)
            if not buf:
                break
            buf = leftover + buf
            usable = len(buf) - (len(buf) % 4)
            leftover = buf[usable:]
            a = array.array("f")
            a.frombytes(buf[:usable])
            yield a
    finally:
        p.stdout.close()
        p.wait()


def amplitude(path, rate, channels, duration):
    """Per-channel amplitude, from a few slices rather than the whole file.

    A sine's peak is rms*sqrt(2) exactly, and rms stays steady in the presence
    of the transients being hunted, which keeps the threshold meaningful.
    """
    points = [duration * (i + 1) / (PROBE_POINTS + 1) for i in range(PROBE_POINTS)]
    span = min(PROBE_SECONDS, duration / (PROBE_POINTS + 1))
    per_point = []
    for t in points:
        sq = [0.0] * channels
        n = 0
        for chunk in raw_stream(path, max(t - span / 2, 0.0), span):
            for i, x in enumerate(chunk):
                sq[i % channels] += x * x
            n += len(chunk) // channels
        if n:
            per_point.append([math.sqrt(s / n) * math.sqrt(2.0) for s in sq])
    if not per_point:
        return [0.0] * channels, True
    amps = []
    for c in range(channels):
        vals = sorted(p[c] for p in per_point)
        amps.append(vals[len(vals) // 2])
    # A level that moved between slices makes a single threshold less apt.
    steady = True
    for c in range(channels):
        vals = [p[c] for p in per_point if p[c] > 0]
        if len(vals) > 1 and max(vals) / min(vals) > 1.4:
            steady = False
    return amps, steady


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("path")
    ap.add_argument("--hz", type=float, default=1000.0,
                    help="frequency of the test tone (default 1000)")
    ap.add_argument("--mult", type=float, default=3.0,
                    help="flag steps this many times the largest legitimate "
                         "step (default 3.0)")
    ap.add_argument("--block", type=int, default=256,
                    help="audio callback size, for boundary alignment "
                         "(default 256)")
    ap.add_argument("--limit", type=int, default=60,
                    help="max events to list (default 60)")
    ap.add_argument("--min-dbfs", type=float, default=-40.0,
                    help="only scan channels at least this loud, i.e. the ones "
                         "carrying the tone (default -40)")
    args = ap.parse_args()

    try:
        rate, channels, duration = probe(args.path)
    except Unreadable as e:
        print(f"ERROR: {e}", file=sys.stderr)
        return 2

    print(f"{args.path}: {rate} Hz, {channels} ch, {duration:.1f} s")
    if duration < MIN_SECONDS:
        print(f"\nVOID: {duration:.2f} s is too short to measure anything. "
              f"A fault that happens once a minute needs minutes of capture.")
        return 2

    amps, steady = amplitude(args.path, rate, channels, duration)
    if not steady:
        print("  note: the tone's level is not constant across the capture, so "
              "one threshold fits it less well.")

    # Which channels carry the tone, and what counts as too big a step in each.
    limits, live = [], []
    for c, a in enumerate(amps):
        dbfs = 20 * math.log10(a) if a > 0 else -999.0
        if dbfs < args.min_dbfs:
            where = "bit-exact silent" if a == 0 else f"{dbfs:.1f} dBFS"
            print(f"  ch{c}: no signal ({where}), skipped")
            limits.append(None)
            continue
        legit = a * 2 * math.pi * args.hz / rate
        limits.append(legit * args.mult)
        live.append(c)
        print(f"  ch{c}: amplitude {a:.4f} ({dbfs:.1f} dBFS), "
              f"max legit step {legit:.5f}, flagging > {legit * args.mult:.5f}")

    # Distinct verdict for "nothing measurable", separate from "measured clean".
    if not live:
        print("\nVOID: no channel carried a signal, so nothing was checked. "
              "Not a clean result. Check the capture target, that the "
              "tone was playing, and the mixer's gain, then run it again.")
        return 2

    # One streaming pass, constant memory.
    found = []
    prev = [None] * channels
    index = 0
    for chunk in raw_stream(args.path):
        for i, x in enumerate(chunk):
            c = (index + i) % channels
            lim = limits[c]
            if lim is not None and prev[c] is not None:
                d = x - prev[c]
                if abs(d) > lim:
                    found.append(((index + i) // channels, c, d))
            prev[c] = x
        index += len(chunk)

    if not found:
        chans = ", ".join(f"ch{c}" for c in live)
        print(f"\nCLEAN: no discontinuities in {chans} over {duration:.1f} s. "
              f"Those samples are smooth, so any click heard during this "
              f"capture was added downstream of this tap.")
        return 0

    print(f"\n{len(found)} discontinuit(ies):\n")
    print(f"  {'time':>10}  {'sample':>10}  {'ch':>2}  {'step':>9}  {'mod block':>9}")
    aligned = 0
    for n, (s, c, d) in enumerate(found):
        if s % args.block == 0:
            aligned += 1
        if n < args.limit:
            print(f"  {s / rate:10.4f}  {s:10}  {c:2}  {d:+9.5f}  "
                  f"{s % args.block:9}")
    if len(found) > args.limit:
        print(f"  ... and {len(found) - args.limit} more")

    print(f"\n{aligned}/{len(found)} sit exactly on a {args.block}-frame boundary.")
    if aligned == len(found):
        print("ALL aligned: whole blocks. In the app's own digital output that "
              "means a per-callback parameter update with no ramp. In a "
              "recording it can also mean the recorder dropped blocks; check "
              "whether the file is short by a whole number of them.")
    elif aligned == 0:
        print("NONE aligned: not whole-block events. In the app's own digital "
              "output that points inside the block - interpolator, filter or "
              "playhead. After a D/A-A/D round trip it points at the analogue "
              "path, since analogue transients have no reason to respect a "
              "callback grid.")

    times = sorted({s for s, _, _ in found})
    gaps = Counter(round((b - a) / rate, 2) for a, b in zip(times, times[1:]))
    if gaps and gaps.most_common(1)[0][1] > 1:
        print(f"Spacing between events (s): {gaps.most_common(3)} - "
              f"a repeating interval means something periodic.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
