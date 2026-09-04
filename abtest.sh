#!/usr/bin/env bash
# A/B/A condition switching with timestamped prompts.
#
# The burst rate drifts over a session, so the battery segment is bracketed
# between two mains segments. B quiet with both A segments busy points at the
# charger; a count that simply climbs across all three points at drift.
#
# Usage:  ./abtest.sh [outdir] [seg_seconds]
# Run in its own terminal while dualcap.sh is recording and the tone plays.

set -uo pipefail
OUT="${1:-/dev/shm/ab}"
SEG="${2:-360}"

# Each segment gives up 10 s at each end to the switch gaps, so segments need
# to be at least 4 gaps long.
GAP=10
if [ "$SEG" -lt $((4 * GAP)) ]; then
  echo "FATAL: segment length ${SEG}s is too short (need >= $((4*GAP))s)." >&2
  echo "Each segment loses ${GAP}s at each end to the switch gaps." >&2
  exit 1
fi
mkdir -p "$OUT"
LOG="$OUT/abtest.log"
: > "$LOG"

# Switches happen 10 s before each boundary, putting the act of touching the
# charger in a gap the analysis excludes.
SW1=$((SEG - GAP))
SW2=$((2 * SEG - GAP))
END=$((3 * SEG - 2 * GAP))

T0=$(date +%s.%N)
{
  echo "abtest start $(date '+%H:%M:%S.%3N')"
  echo "t0=$T0"
  echo "seg=$SEG"
  echo "# analysis windows (exclude +/-10s around each switch):"
  echo "windowA1	0	$((SEG - 2*GAP))	charger IN"
  echo "windowB	$SEG	$((2*SEG - 2*GAP))	ON BATTERY"
  echo "windowA2	$((2*SEG))	$END	charger IN"
} >> "$LOG"

say() { printf "  [t=%4ss  %s]  %s\n" "$1" "$(date '+%H:%M:%S.%3N')" "$2";
        printf "%s\t%s\t%s\n" "$1" "$(date '+%H:%M:%S.%3N')" "$2" >> "$LOG"; }
waituntil() { python3 -c "import time; time.sleep(max(0.0, $T0 + $1 - time.time()))"; }

echo
echo "  Segment length ${SEG}s. Total $((3*SEG))s (~$((3*SEG/60)) min)."
echo "  START WITH THE CHARGER PLUGGED IN."
echo "  Hands off except for the two charger switches below."
echo

say 0 "SEGMENT A1 - charger PLUGGED IN. Hands off now."
waituntil $SW1; say $SW1 ">>> UNPLUG THE CHARGER NOW, then hands off <<<"
waituntil $SEG; say $SEG "SEGMENT B - ON BATTERY. Hands off."
waituntil $SW2; say $SW2 ">>> PLUG THE CHARGER BACK IN NOW, then hands off <<<"
waituntil $((2*SEG)); say $((2*SEG)) "SEGMENT A2 - charger PLUGGED IN. Hands off."
waituntil $END; say $END "DONE - stop the capture (ENTER in the dualcap terminal)"

echo
echo "  Then: ./bursts.py <scan-file> --schedule $LOG"
