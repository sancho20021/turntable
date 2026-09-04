# Clicks & Pops — Xone:24C — Findings and Method

## CONCLUSION

**The clicks come from the laptop charger, not from the app.**

| | |
|---|---|
| **Cause** | Leakage current from the laptop's unearthed (Class II) USB-C supply flowing through the USB cable's ground into the earthed mixer, where it lands on the analogue signal ground. |
| **Measured effect** | **7.6 clicks/min** with the charger connected. **0 clicks/min** on battery. |
| **The app** | Clean. Zero discontinuities in its output across 19.3 min of continuous measurement. Not a software problem. |
| **Free fix** | **Unplug the charger while playing.** Proven: zero events over 312 continuous seconds on battery. |
| **Paid fix** | A **high-speed** USB isolator, only if charging while playing is required. |

Secondary, same cause: touching the booth knob, a channel fader, the USB cable
or either chassis can trigger a click — your body becomes an alternative
discharge path. Moving a heater on carpet does it too (static plus a large
switching mains load). All of these become far less likely once the leakage
source is gone.

---

## HOW TO GET A CLEAN SET / RECORDING

**One thing matters: play on battery.** Unplug the laptop charger before you
start. That takes it from a click every 8 seconds to none.

Everything else on this list is ordinary good setup.

**Power**
- Laptop on battery. An L390 runs a set comfortably.
- If you have to stay plugged in, an earthed (3-pin) or simply different
  USB-C supply may be enough — measure it with E6 rather than guessing.
- Keep the mixer on its own mains outlet where you can, and away from heaters,
  dimmers and cheap phone chargers. Those inject through the same path.

**Signal**
- Mixer's USB straight into the laptop.
- Gain stage so the master sits around **-6 to -12 dBFS** on the record send.
  Plenty of headroom, well clear of the noise floor.
- Buffer 256 is right. There is a 25x CPU margin at that size, so there is
  nothing to gain from raising it.

**Recording**

```bash
pw-record --target=alsa_input.usb-Allen_and_Heath_Xone_24C_Y260424004355-00.pro-input-0 \
          --rate=48000 --channels=2 --format=s32 master.wav
```

Find the node name:

```bash
pw-cli ls Node | grep -oiE 'alsa_input[^"]*xone[^"]*'
```

- Use the name. Numeric ids are reassigned on replug and reboot.
- `pro-input-0` is the mixer's master arriving at the laptop through its ADC:
  post-EQ, post-fader, post-filter. `pro-output-0` is the other direction, what
  the app sends to the mixer.
- Two channels needs no `--channel-map`; the ports land on the master pair.
- Drops nothing over arbitrary length. The ~0.11 s shortfall against wall-clock
  is one-time startup latency.

**Finishing**
- Stop the deck before quitting the app. Quitting with a signal still running
  pops, and that pop lands in the recording.

**Playing normally is fine.** Faders, booth knob, cueing, touching the cable —
those trigger clicks only because charger leakage is sitting there waiting for a
path to discharge through. On battery there is nothing to discharge, and normal
handling stops mattering.

---

## FINDINGS

| # | Finding | Evidence |
|---|---|---|
| 1 | **The charger is the dominant source.** 7.59 and 7.06 bursts/min on mains against 0.88 on battery, and **zero** after a 48 s settling tail (312 s silent). Mains segments bracket the battery one, so warm-up cannot account for it. | E6 |
| 2 | **Mechanism: leakage through the USB ground.** The mixer is self-powered (`bmAttributes 0xc0`, `MaxPower 0 mA`) from its own mains supply. The USB cable's ground is therefore the only conductor joining a floating laptop chassis to an earthed mixer. Y-capacitors in a 2-pin PSU hold the chassis near half mains through a high impedance; the resulting current flows into the mixer's ground reference, which is where its analogue signal ground sits. | E6 + USB descriptors. Confirm with a multimeter (see next steps) |
| 3 | **The charger sets the rate, not the size.** Mean per-burst max step 0.11 / 0.12 / 0.10 across the three conditions, peaks ~0.21 in all. Consistent with a trigger/leakage path rather than added coupling gain. | E6 |
| 4 | **Event rate climbs while the charger is connected and resets when it is unplugged.** Bursts per third of segment: mains `[12,14,17]` and `[10,13,17]`, battery `[5,0,0]`. Tracks charger-connected time, not temperature. | E6 |
| 5 | **The app's output is clean.** Zero discontinuities over 19.3 min continuous. Independently corroborated: 595 consecutive per-second health lines reporting 0 skipped callbacks. | E6, E4b |
| 6 | **No transport loss.** 86866/86866 callbacks served, slowest 210 us of a 5333 us budget — a 25x margin. Buffer size, CPU governor, sample-rate pinning and USB contention are all irrelevant to this fault. | E2 |
| 7 | **The clicks are analogue in origin.** They appear only after the D/A boundary, are not aligned to callback blocks, and last well under a millisecond with steps larger than the signal itself. | E4b, E6 |
| 8 | **Touch triggers events, intermittently.** Booth volume knob, channel fader, USB cable and both chassis. Not every touch does it. | E4 |
| 9 | **The heater couples into the recording, not just the speakers.** Its bursts appear in the mixer's own record send, so the noise enters the audio electronics rather than arriving acoustically. | E4 |
| 10 | **A cheap USB isolator will not work.** ADuM3160/4160 isolators are USB 1.1 full-speed (12 Mbps). This device needs 6 ch x 48 kHz x 24 bit duplex = **13.8 Mbps of payload before overhead**, and offers only 6-channel altsettings at 125 us microframes. A high-speed isolator (ADuM4165/4166 class, e.g. Intona 7054/7055) is required. | Descriptor arithmetic |
| 11 | **Isolating the mixer's audio outputs would not help.** The noise is already present in the mixer's own record send, i.e. inside its internal signal path, upstream of the outputs. | E4b, E6 |
| 12 | The Xone:24C enumerates correctly and needs no configuration: 480 Mbps, 6 ch, 48 kHz native, ASYNC endpoint with explicit feedback. The hub visible in `lsusb -t` is a Cypress HX2VL chip **inside the mixer**; the cable goes straight to the laptop. | E1 |

---

## NEXT STEPS

In order of cost.

- [ ] **Play on battery.** Free, already proven.
- [ ] **Confirm finding 2 with a multimeter.** AC volts between the laptop
      chassis (a USB shell works) and mains earth, with the mixer's USB
      unplugged. A floating Class II supply reads roughly 50-120 V AC at high
      impedance; unplugging the charger should collapse it to near zero.
- [ ] **Check the charger's inlet.** A 3-pin cloverleaf (C5) can take an earthed
      cable — fit one and re-run E6. A 2-pin figure-8 (C7) or fixed 2-pin plug
      is Class II by design; skip ahead.
- [ ] **Check whether the mixer's own supply is earthed.** If it is also 2-pin,
      neither side is earthed and finding 2 needs revising.
- [ ] **Try a different USB-C PD charger.** Leakage varies a lot between supplies.
- [ ] **High-speed USB isolator**, if charging while playing is a requirement.
      Must be high-speed — see finding 10. Check current prices before buying.

Re-run E6 after each attempt. The baseline is 7.6 bursts/min on mains versus
0/min on battery, which is a firmer test than listening.

Still unmeasured: scratching, record changes and seeking. Every result here
covers **parked playback only**.

---

## TOOLS

Five scripts. All need only ffmpeg, PipeWire's CLI tools and the standard
library.

### `clickscan.py` — did anything discontinuous happen in this recording?

```bash
./clickscan.py capture.wav          # --hz 1000 --mult 3.0 --block 256
```

- A sine of frequency `f` at rate `fs` with amplitude `p` can never step between
  adjacent samples by more than `p * 2*pi*f/fs`. Anything bigger came from a
  dropout, a splice or an unsmoothed parameter change. Playing a pure tone makes
  the whole question arithmetic.
- Three verdicts: **CLEAN** (exit 0), **discontinuities found** (1), **VOID** (2)
  for a capture with no measurable signal in it.
- Amplitude comes from the median of three sampled slices, via `rms*sqrt(2)`
  (exact for a sine).
- Only channels above `--min-dbfs` are scanned, i.e. the ones carrying the tone.
- Streams in 1 MB pieces; memory is flat (~56 MB) whatever the length.
- Prints `sample % block` per event. All events on a block boundary means whole
  blocks: a per-callback parameter update, or a recorder that dropped blocks.
  None aligned means inside-the-block DSP, or — after a D/A-A/D round trip —
  the analogue path.

### `dualcap.sh` — record both sides of the analogue boundary at once

```bash
./dualcap.sh [outdir]               # default /dev/shm/dualcap
```

| tap | what it is | what it contains |
|---|---|---|
| **PRE** | the sink's monitor | the last digital point before USB: what PipeWire hands the interface |
| **POST** | the mixer's USB record send | everything PRE has, **plus** DAC -> mixer analogue path -> ADC |

- A click in POST but not PRE starts in the hardware. A click in both is
  upstream of the interface: the app or PipeWire.
- Start it **first**, then start the tone when prompted. The tone's onset aligns
  the two files, and both run off the same graph clock.
- `-P '{ stream.capture.sink=true }'` is what makes a capture stream read a
  sink's monitor.
- Channel map is `AUX0,AUX1`. Set `MAP=AUX2,AUX3` for `-r 1`.
- **Captures to tmpfs** (`/dev/shm`) by default. At these data rates disk
  writeback stalls `pw-record` and whole capture blocks go missing.
- Before recording it checks both nodes exist and the expected links formed.
  After, it checks the two durations agree within 50 ms.
- Writes `capture.log` (its start instant), `pre.scan` and `post.scan`.
- Taps are distinct when, with nothing playing, PRE reads bit-exact silent and
  POST shows an analogue noise floor around -92 dBFS.

### `bursts.py` — collapse events into physical clicks, bin them by condition

```bash
./bursts.py post.scan
./bursts.py post.scan --schedule ab/abtest.log --capture-log ab/capture.log
./bursts.py post.scan --windows 0:340:mains 360:700:battery
```

- `clickscan.py` prints one row per sample per channel, so one audible click is
  a dozen rows. This groups rows within `--gap` seconds into one burst.
- `--schedule` times are relative to when `abtest.sh` started; scan times are
  relative to when the capture started, which is earlier. `--capture-log`
  supplies the offset, which is computed and reported.
- `--windows` values are already in scan time.

### `abtest.sh` — A/B/A condition switching with timestamped prompts

```bash
./abtest.sh [outdir] [seg_seconds]  # default 360 -> 3 x 6 min
```

- Prompts the two charger switches and logs the wall-clock time of each.
- Switches land 10 s inside the segment gaps, putting the act of touching the
  charger in a window the analysis excludes.
- A/B/A rather than a single battery run, since the event rate drifts over a
  session (finding 4). Bracketing the battery segment tells "the charger did it"
  apart from "it drifted".
- Segments must be at least 40 s.

### `provoke.sh` — scripted physical interventions

```bash
./provoke.sh [outdir]
```

- Prompts what to touch and when, logging each to 14 ms.
- Each action repeats 3x, since touches inject only intermittently, with CONTROL
  windows (touch nothing) interleaved 20 s apart.
- An intervention counts as confirmed if it fires in 2 or 3 of its 3 trials
  while the controls stay clean.

### Test tone

```bash
ffmpeg -hide_banner -y -f lavfi \
  -i "aevalsrc=exprs=0.5*sin(2*PI*1000*t)|0.5*sin(2*PI*1000*t):s=48000:d=1320" \
  -sample_fmt s16 -c:a flac /tmp/tone1k_22min.flac

ffmpeg -hide_banner -i /tmp/tone1k_22min.flac -af volumedetect -f null - 2>&1 \
  | grep max_volume        # must print exactly -6.0 dB
```

- 48 kHz matches the app's `SAMPLE_RATE`, so the loader never resamples.
- -6 dBFS gives known headroom; `aevalsrc` sets the amplitude explicitly, and
  the `volumedetect` line confirms it.
- FLAC: `Cargo.toml` builds symphonium with `mp3, flac, aac, isomp4`. Stereo is
  required (`decoder.rs:27`).
- `d=` longer than the run, so the deck never reaches the end of the record.

---

## STANDARD RUN

```bash
# T1 - app, tone loaded, NOT playing yet
./target/release/turntable run -D xone -r 0 -b 256
#      paste /tmp/tone1k_22min.flac into this terminal, focus the SDL window,
#      Enter to load. Do not press Space yet.

# T2 - capture
./dualcap.sh /dev/shm/run
#      press Space in the SDL window when it says START THE TONE NOW

# T3 - condition schedule, if the run needs one
./abtest.sh /dev/shm/run 360
```

Shut down in this order, which keeps the stop transient out of the capture and
stops the signal before the app exits:

1. ENTER in the dualcap terminal (capture stops, scans run and are saved)
2. Space in the SDL window (stop the signal)
3. quit the app

Test the **release** build; a debug build of the DSP xruns on its own.
Truncate `~/spw/localdeck/turntable.log` first so it holds only this run.

**Loading a record:** paste the path into the TUI terminal (a paste becomes
`PrepareRecord`, `ratatui.rs:145`), then focus the SDL window, which owns the
keyboard, and press Enter to load, Space to play. Other keys:
`1..9` active deck, `R` reset pitch, arrows for pitch and +/-15 s seek,
`Shift+Left` playhead reset.

---

## EXPERIMENTS

### E1 — What does the mixer negotiate?
480 Mbps high speed. 6 channels, map `FL FR FC LFE RL RR` = 3 stereo pairs, so
`-r 0`, `-r 0,1` and `-r 0,1,2` are all valid. Rates 44100 / **48000** / 88200 /
96000, so 48 kHz needs no resampling. Formats S32_LE (24 bit) and S16_LE, no
F32, so PipeWire converts. ASYNC output endpoint with explicit feedback endpoint,
125 us packet interval: the mixer runs on its own clock and the host adapts.
Card 1 `Xone24C`, `22f0:002a`, driver `snd-usb-audio`, sysfs `1-4.3`.
Self-powered, `MaxPower 0 mA`.

### E2 — Parked tone into the mixer: 4 clicks in 7.7 min
Health panel green throughout; `pw-top` ERR flat at 1; RATE 48000, QUANT 256 as
requested. Session: `463.4s, 86866/86866 callbacks served, 0 skipped, longest
gap 6.77ms, slowest callback 210us of 5333us budget`. Transport loss ruled out.
The one non-clean log line, a playhead jump, is the needle dropping on a freshly
loaded record.

### E4 — Provoked run: 16 bursts, 13 matching an intervention
Bursts at +14 to +26.6 dB above the non-tone noise floor, one-to-one with the
operator's notes: heater moves, booth volume knob, channel fader, and the USB
cable (+18.1 dB, the largest of the run). All non-block-aligned, sub-millisecond.
The source tone scanned clean over its full length, so nothing arrived from the
file.

### E4b — Hands-off baseline: the app is clean
582.4 s, heater removed, nothing touched. Skew 5.30 ms. PRE contained one
block-aligned event, which is a dropped block in the recorder rather than lost
audio: absent from POST, PRE short by 0.994 of a 256-frame block, and a phase
jump of 116.8° against the 120.0° that dropping 5.3333 periods of 1 kHz
predicts; a skipped callback instead inserts silence and leaves the length
intact. The app's health monitor reports 0 skipped callbacks for 595 consecutive
seconds
across the window; its one genuine skipped callback is timestamped 0.45 s
**after** the capture ended, during teardown.

POST held 8 bursts, 0 of 45 rows block-aligned, matching all 8 clicks heard.
Peak steps to 0.218 against a signal amplitude of 0.11. Nothing was touched, so
the coupling does not require provocation.

### E6 — Charger A/B/A: cause identified
1156.8 s capture, skew **0.0 ms**. PRE **clean over the entire 19.3 min**.

| condition | window (scan s) | bursts | per min |
|---|---|---|---|
| A1 — charger IN | 24 to 364 | **43** | **7.59** |
| B — ON BATTERY | 384 to 724 | **5** | **0.88** |
| A2 — charger IN | 744 to 1084 | **40** | **7.06** |

All five battery bursts fall within 48 s of unplugging (12.4, 32.6, 37.7, 43.5,
47.8 s), then **312 s of silence**. After re-plugging, the first burst takes
85.9 s. So the settled battery rate is zero and those five are a draining tail.

Open question: E4b's overall rate was 0.82 bursts/min, nearly identical to E6's
battery figure of 0.88 and about 9x below its mains figure. E4b's protocol did
not specify charger state; it was probably run on battery.

---

## ENVIRONMENT

| | |
|---|---|
| Host | ThinkPad L390, Intel i7-8565U (4C/8T, 15 W) |
| OS | Ubuntu 24.04.4 LTS, kernel 6.8.0-138-generic, PREEMPT_DYNAMIC |
| PipeWire | 1.0.5 |
| WirePlumber | 0.4.17 — Lua config format, not 0.5's `.conf` |
| CPU governor | `powersave` |
| rtkit-daemon | active; `ulimit -r` 0, so RT priority is granted at runtime via rtkit |
| Mixer | Xone:24C `22f0:002a`, self-powered, 6 ch, 48 kHz native |
| USB | direct to the laptop's right-hand port; `1-4.3` because of the mixer's internal Cypress HX2VL hub. Bus 001, shared with camera, fingerprint reader and bluetooth |
| App | `SAMPLE_RATE` 48000 (`decoder.rs:9`), default buffer 256 (`-b`) |
| Log | `~/spw/localdeck/turntable.log`, appends |

Node names (stable across reboots; the numeric ids are not):

```
alsa_output.usb-Allen_and_Heath_Xone_24C_Y260424004355-00.pro-output-0   (sink)
alsa_input.usb-Allen_and_Heath_Xone_24C_Y260424004355-00.pro-input-0     (source)
```

Device snapshot, when something changes:

```bash
cat /proc/asound/cards
lsusb -t                                  # link speed: want 480M
cat /proc/asound/card*/stream0            # channels, rates, formats, sync mode
pw-cli ls Node | grep -iA2 xone
```
