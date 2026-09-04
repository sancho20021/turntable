#!/usr/bin/env bash
# Prompts a scripted sequence of physical interventions and logs the exact time
# of each, so correlating them against a capture needs no hand-written notes.
#
# Design:
#  * Each intervention repeats 3x, since touches inject only intermittently.
#  * CONTROL windows (do nothing) are interleaved, to establish the rate that
#    holds when nothing is touched.
#  * Actions are 20 s apart, keeping bursts attributable to one window.
#
# Run this in its own terminal AFTER dualcap.sh is recording and the tone is
# playing. Wall-clock times are printed and written to <outdir>/provoke.log.

set -uo pipefail
OUT="${1:-/dev/shm/dualcap}"
mkdir -p "$OUT"
LOG="$OUT/provoke.log"
: > "$LOG"

# t_seconds : instruction
SCHEDULE=(
  "20:touch the BOOTH VOLUME knob (grip it, do not turn it)"
  "40:touch the CHANNEL FADER cap (grip it, do not move it)"
  "60:touch the USB CABLE, mid-cable"
  "80:CONTROL - touch NOTHING, hands in your lap"
  "100:touch the BOOTH VOLUME knob again"
  "120:touch the CHANNEL FADER cap again"
  "140:touch the USB CABLE again"
  "160:CONTROL - touch NOTHING"
  "180:touch the BOOTH VOLUME knob, third time"
  "200:touch the CHANNEL FADER cap, third time"
  "220:touch the USB CABLE, third time"
  "240:CONTROL - touch NOTHING"
  "260:touch the MIXER's metal chassis"
  "280:touch the LAPTOP's metal chassis"
  "300:CONTROL - touch NOTHING"
)

T0=$(date +%s.%N)
start_wall=$(date "+%H:%M:%S.%3N")
echo "provoke start: $start_wall" | tee -a "$LOG"
echo "t0=$T0" >> "$LOG"
echo
echo "  Leave this running. Do ONLY what it says, exactly when it says it."
echo "  Each prompt means: make contact for about 2 seconds, then let go."
echo

for entry in "${SCHEDULE[@]}"; do
  t="${entry%%:*}"
  msg="${entry#*:}"
  # Sleep to an absolute deadline computed from T0, so slop never accumulates
  # across the schedule (one python call, not a polling loop).
  python3 -c "import time; time.sleep(max(0.0, $T0 + $t - time.time()))"
  wall=$(date "+%H:%M:%S.%3N")
  printf "  [t=%3ss  %s]  %s\n" "$t" "$wall" "$msg"
  printf "%s\t%s\t%s\n" "$t" "$wall" "$msg" >> "$LOG"
done

echo
echo "  Schedule complete. Stop the capture (ENTER in the dualcap terminal)."
echo "  Log written to $LOG"
