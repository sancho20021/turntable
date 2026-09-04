#!/usr/bin/env bash
# Captures both sides of the Xone:24C's analogue boundary at once.
#
#   PRE  = the sink's monitor: the last digital point before USB. Exactly what
#          PipeWire hands the interface.
#   POST = the mixer's USB record send, back through its own ADC. Everything
#          PRE has, plus DAC -> analogue -> ADC.
#
# A click in POST but not PRE starts inside the interface or the mixer. A click
# in both is upstream of the interface: the app or PipeWire.
#
# Usage:  ./dualcap.sh [outdir]
# Start this FIRST, then start the tone when it tells you to - the tone's onset
# is the landmark that aligns the two files.

set -uo pipefail

# Capture to tmpfs. Disk writeback stalls pw-record at these data rates and
# whole capture blocks go missing; RAM has no writeback.
OUT="${1:-/dev/shm/dualcap}"
RATE=48000
PRE_NODE="alsa_output.usb-Allen_and_Heath_Xone_24C_Y260424004355-00.pro-output-0"
POST_NODE="alsa_input.usb-Allen_and_Heath_Xone_24C_Y260424004355-00.pro-input-0"

mkdir -p "$OUT"
PRE="$OUT/pre.wav"
POST="$OUT/post.wav"

for n in "$PRE_NODE" "$POST_NODE"; do
  if ! pw-cli ls Node 2>/dev/null | grep -qF "$n"; then
    echo "FATAL: node not found: $n" >&2
    echo "The mixer may be unplugged, or on a different profile. Listing what" >&2
    echo "is actually there:" >&2
    pw-cli ls Node 2>/dev/null | grep -oE 'alsa_(output|input)[^"]*' | sort -u >&2
    exit 1
  fi
done

rm -f "$PRE" "$POST"
# Ports on these nodes are named AUX0..AUX5. The tone sits on AUX0/AUX1 for
# both taps when the deck is routed to stereo pair 0 (`-r 0`); override for
# other routings:  MAP=AUX2,AUX3 ./dualcap.sh
#
# `stream.capture.sink=true` is what makes a capture stream read a sink's
# monitor.
MAP="${MAP:-AUX0,AUX1}"
pw-record -P '{ stream.capture.sink=true }' --target="$PRE_NODE" \
          --rate=$RATE --channel-map=$MAP --format=f32 "$PRE"  & PRE_PID=$!
pw-record --target="$POST_NODE" \
          --rate=$RATE --channel-map=$MAP --format=f32 "$POST" & POST_PID=$!

cleanup() {
  kill "$PRE_PID" "$POST_PID" 2>/dev/null
  wait "$PRE_PID" "$POST_PID" 2>/dev/null
}
trap cleanup EXIT INT TERM

CAP_T0=$(date +%s.%N)
printf 'capture_t0\t%s\t%s\n' "$CAP_T0" "$(date '+%H:%M:%S.%3N')" > "$OUT/capture.log"

sleep 2
for pid in "$PRE_PID" "$POST_PID"; do
  kill -0 "$pid" 2>/dev/null || { echo "FATAL: a recorder died on startup" >&2; exit 1; }
done

# Verify each tap is linked to the node it was pointed at, and to as many ports
# as the channel map asks for.
NCH=$(awk -F, '{print NF}' <<<"$MAP")
links=$(pw-link -l 2>/dev/null)
mon=$(grep -c "monitor_AUX[0-9]$" <<<"$links")
cap=$(grep -c "capture_AUX[0-9]$" <<<"$links")
if [ "$mon" -lt "$NCH" ] || [ "$cap" -lt "$NCH" ]; then
  echo "FATAL: taps not fully linked (monitor lines $mon, capture lines $cap;" >&2
  echo "expected >= $NCH each for MAP=$MAP)." >&2
  echo "Refusing to record a misleading capture." >&2
  exit 1
fi
echo "  taps verified: $mon monitor + $cap capture links for $NCH channels ($MAP)"

echo
echo "  Recording both taps."
echo "  -> pre : $PRE"
echo "  -> post: $POST"
echo
echo "  START THE TONE NOW (focus the SDL window, press Space)."
echo "  Its onset is what aligns the two captures, so start it AFTER this line."
echo
echo "  Then: hands off. Note the wall-clock time of every click you hear, and"
echo "  of every heater move you make."
echo
echo "  Press ENTER here when you are done."
read -r

cleanup
trap - EXIT INT TERM
sleep 0.3

echo
# Both taps run off the same graph clock and stop at the same instant, so equal
# durations mean neither lost audio. A skew means one did, and its timestamps
# are then unreliable.
dpre=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$PRE")
dpost=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$POST")
skew=$(python3 -c "print(abs($dpre-$dpost))")
echo "  durations: pre=${dpre}s post=${dpost}s  skew=${skew}s"
if python3 -c "import sys; sys.exit(0 if abs($dpre-$dpost) > 0.05 else 1)"; then
  echo
  echo "  ##########################################################"
  echo "  # WARNING: the two taps differ by ${skew}s, so one dropped"
  echo "  # capture blocks. Block-aligned discontinuities below are"
  echo "  # then the recorder's, and timestamps in the short file"
  echo "  # drift. Re-run with the output on tmpfs (/dev/shm)."
  echo "  ##########################################################"
  echo
fi

for f in "$PRE" "$POST"; do
  [ -s "$f" ] || { echo "FATAL: $f is empty - nothing was captured" >&2; exit 1; }
  printf "%-6s %s  " "$(basename "$f" .wav)" \
    "$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$f" | cut -d. -f1)s"
  ffmpeg -hide_banner -i "$f" -af volumedetect -f null - 2>&1 |
    grep -oE "max_volume: .*" | tr '\n' ' '
  echo
done

echo
# Saved as well as shown, so bursts.py and any later re-analysis can use them
# without re-scanning.
echo "=============================== PRE ==============================="
python3 "$(dirname "$0")/clickscan.py" --limit 600 "$PRE" | tee "$OUT/pre.scan"
pre_rc=${PIPESTATUS[0]}
echo
echo "=============================== POST =============================="
python3 "$(dirname "$0")/clickscan.py" --limit 600 "$POST" | tee "$OUT/post.scan"
post_rc=${PIPESTATUS[0]}

echo
echo "===================== WHERE IT STARTS FAILING ====================="
case "$pre_rc/$post_rc" in
  2/*|*/2) echo "  VOID - one tap caught no signal. The result means nothing;" \
                "fix the routing or the gain and run it again." ;;
  0/0)     echo "  Both taps clean. A click heard during this run came from" \
                "after the mixer's record-send tap - final output stage, amp," \
                "speakers, cables, headphones." ;;
  0/1)     echo "  POST dirty, PRE clean => the fault STARTS IN THE HARDWARE:" \
                "the interface's DAC, the mixer's analogue path, or its ADC." \
                "The app's output was provably smooth. Go to Phase 3." ;;
  1/1)     echo "  Both dirty => the fault is UPSTREAM of the interface: the" \
                "app or PipeWire. Compare the sample offsets in the two" \
                "listings; go to Phase 4." ;;
  1/0)     echo "  PRE dirty, POST clean => suspect a capture or threshold" \
                "artifact." ;;
esac
echo
echo "  Scans saved: $OUT/pre.scan  $OUT/post.scan"
if [ -f "$OUT/abtest.log" ]; then
  echo "  Condition schedule found. Bin the bursts by condition with:"
  echo "    ./bursts.py $OUT/post.scan --schedule $OUT/abtest.log \\"
  echo "                --capture-log $OUT/capture.log"
fi
echo
echo "  Covers the chain up to the record-send tap, for the clicks that"
echo "  occurred during this capture."
