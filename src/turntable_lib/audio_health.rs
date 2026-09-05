//! Audio-thread health metrics.
//!
//! The audio callback cannot log: `env_logger`'s file target formats, takes a
//! mutex and issues an unbuffered `write()` per record, which blocks for
//! milliseconds whenever the filesystem commits its journal. So the callback
//! only bumps relaxed atomics here and pushes the occasional detail event onto a
//! lock-free queue; [`spawn_monitor`] does the logging on its own thread.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering::Relaxed},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crossbeam::atomic::AtomicCell;
use rtrb::{Consumer, Producer, RingBuffer};

/// Capacity of the detail-event queue. Events are rate limited at the source, so
/// this only has to absorb a burst.
const EVENT_QUEUE_CAPACITY: usize = 64;

/// Minimum spacing between detail events from the audio thread. They are
/// examples, not a rate - the counters carry the rate.
const EVENT_MIN_INTERVAL_NANOS: u64 = 1_000_000_000;

/// How often the monitor emits a line.
const REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// How often the monitor wakes to drain events and notice shutdown.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Percentage of the budget above which a callback counts as "near budget".
const NEAR_BUDGET_PERCENT: u64 = 80;

/// Playback lag past which a deck's line is worth a warning on its own. Below
/// this the sync loop is doing its job.
const LAG_WARN_NANOS: i64 = 20_000_000;

/// Longest gap between callbacks still worth reading as lost audio. Beyond this
/// the device clock has jumped rather than the stream stalled: cpal stamps
/// callbacks from PipeWire's graph clock and falls back to `CLOCK_MONOTONIC`
/// when that is unavailable, and the two have different epochs.
const MAX_PLAUSIBLE_GAP_NANOS: u64 = 1_000_000_000;

/// Callbacks lost in one second at or above which the engine is failing rather
/// than glitching. One or two is a click you can play through; three in a single
/// second is the sound breaking up.
const MANY_LOST_CALLBACKS: u64 = 3;

/// Whether audio is being lost, judged on the last second.
///
/// Damage only. Thin margin and sync trouble go to the log; neither is audio
/// anyone has lost yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HealthLevel {
    /// Nothing lost.
    Clean,
    /// A few callbacks lost - clicks, not a breakdown.
    Glitching,
    /// [`MANY_LOST_CALLBACKS`] or more lost in one second.
    Failing,
}

impl HealthLevel {
    /// Classifies one second's worth of loss.
    fn of(lost: u64) -> Self {
        match lost {
            0 => Self::Clean,
            n if n < MANY_LOST_CALLBACKS => Self::Glitching,
            _ => Self::Failing,
        }
    }
}

/// The verdict a display shows, computed once a second by the monitor.
#[derive(Debug, Clone, Copy)]
pub struct HealthDigest {
    pub level: HealthLevel,
    /// Callbacks lost in the last second.
    pub lost: u64,
    /// When the monitor started, so "clean for" has a floor.
    pub started: Instant,
    /// End of the last second in which anything was lost.
    pub last_loss: Option<Instant>,
}

impl HealthDigest {
    fn new(started: Instant) -> Self {
        Self {
            level: HealthLevel::Clean,
            lost: 0,
            started,
            last_loss: None,
        }
    }

    /// How long the engine has been losing nothing.
    pub fn clean_for(&self) -> Duration {
        self.last_loss.unwrap_or(self.started).elapsed()
    }
}

/// Per-deck sound-quality health. Lives inside [`AudioHealth`]; one instance per
/// deck, because a single shared lag gauge would be overwritten by whichever
/// deck ran last.
pub struct DeckHealth {
    /// Callbacks this deck rendered. Not health on its own; it's what the rest of
    /// this struct divides by.
    pub callbacks_rendered: AtomicU64,

    /// How far the playback clock trails the platter (a gauge, not a counter).
    /// Both the audible offset and the gain the extrapolation applies to slope
    /// noise.
    pub playback_lag_nanos: AtomicI64,
    /// Callbacks where the lag correction hit its +/-5ms clamp, so it ran at its
    /// ceiling of ~2.5ms/s of catch-up. The lag still moves at that rate; which
    /// way depends on whether skips inject lag faster. Read alongside
    /// `playback_lag_nanos` for the direction.
    pub callbacks_with_lag_correction_maxed: AtomicU64,
    /// Callbacks that found no fresh platter sample and had to extrapolate from a
    /// stale slope. Zero while the platter thread outruns the audio one.
    pub callbacks_without_fresh_platter_sample: AtomicU64,
    /// Times a record was dropped inside the callback because the recycling ring
    /// was full. Frees the whole decoded track, and stalls, whenever that was its
    /// last reference - and decks share one. Expected value: 0.
    pub records_dropped_on_audio_thread: AtomicU64,

    /// Times the playhead was repositioned: a load, a rewind, a fast-forward.
    pub playhead_jumps: AtomicU64,
}

impl DeckHealth {
    fn new() -> Self {
        Self {
            callbacks_rendered: AtomicU64::new(0),
            playback_lag_nanos: AtomicI64::new(0),
            callbacks_with_lag_correction_maxed: AtomicU64::new(0),
            callbacks_without_fresh_platter_sample: AtomicU64::new(0),
            records_dropped_on_audio_thread: AtomicU64::new(0),
            playhead_jumps: AtomicU64::new(0),
        }
    }

    pub fn playback_lag(&self) -> i64 {
        self.playback_lag_nanos.load(Relaxed)
    }

    #[inline]
    pub fn callback_rendered(&self) {
        self.callbacks_rendered.fetch_add(1, Relaxed);
    }

    #[inline]
    pub fn set_playback_lag(&self, lag_nanos: i64) {
        self.playback_lag_nanos.store(lag_nanos, Relaxed);
    }

    #[inline]
    pub fn lag_correction_maxed(&self) {
        self.callbacks_with_lag_correction_maxed
            .fetch_add(1, Relaxed);
    }

    #[inline]
    pub fn stale_platter_sample(&self) {
        self.callbacks_without_fresh_platter_sample
            .fetch_add(1, Relaxed);
    }

    #[inline]
    pub fn record_dropped_on_audio_thread(&self) {
        self.records_dropped_on_audio_thread.fetch_add(1, Relaxed);
    }

    #[inline]
    pub fn playhead_jumped(&self) {
        self.playhead_jumps.fetch_add(1, Relaxed);
    }
}

/// Stream-level audio health, written only by the audio callback and read by the
/// monitor thread. Every write is a `Relaxed` atomic op: no locks, no
/// allocation, no syscalls.
///
/// Counters are cumulative and never reset by the audio thread; the monitor
/// diffs successive snapshots to get per-window rates. The exceptions are the
/// `_window` fields, which the monitor owns and clears each tick.
pub struct AudioHealth {
    // ---- config: what we asked the device for, set once ---------------------
    /// Time one callback has to finish, from the frame count the device is
    /// actually delivering. PipeWire runs one quantum for the whole graph and
    /// picks the smallest any client asks for, so it changes at runtime.
    ///
    /// Seeded from the request, corrected by the first callback.
    pub callback_budget_nanos: AtomicU64,
    /// Frames per callback we configured, kept for reporting what was asked for.
    pub frames_per_callback_expected: u32,
    /// Frames the device last handed us.
    pub frames_per_callback_observed: AtomicU32,
    sample_rate: u32,
    /// Device clock at the first callback, so event timestamps can be reported
    /// relative to the start of the stream rather than to boot. Not a metric.
    stream_epoch_nanos: AtomicU64,

    // ---- denominator --------------------------------------------------------
    /// Callbacks we were actually called for. Not health on its own; it's what
    /// the rest divide by.
    pub callbacks_served: AtomicU64,

    // ---- damage: did the listener hear a defect? ---------------------------
    /// Callbacks the device expected but never made, derived from the device
    /// clock. Not attributed: we may have been late, or the graph may have
    /// skipped us. `× callback_budget_nanos` is the silence that reached the DAC.
    pub callbacks_skipped: AtomicU64,
    /// How many separate times a skip happened, regardless of how many callbacks
    /// each one swallowed. Against `callbacks_skipped` it separates constant
    /// crackle from a few long stutters.
    pub skip_incidents: AtomicU64,
    /// Longest time between two consecutive callbacks this window. Divide by
    /// `callback_budget_nanos` for the worst single hole, in callbacks. Not
    /// attributed. Cleared by the monitor each tick.
    pub longest_gap_between_callbacks_nanos_window: AtomicU64,
    /// Longest gap of the whole run, for the teardown summary.
    pub longest_gap_between_callbacks_nanos_session: AtomicU64,
    /// Callbacks we filled with silence ourselves because they carried more
    /// frames than the buffers hold. On time, but silent, so the device clock
    /// never sees these.
    pub callbacks_silenced_by_frame_mismatch: AtomicU64,

    // ---- cause: whose fault was it? ---------------------------------------
    /// Callbacks that took >= 100% of budget, i.e. we caused a skip ourselves.
    /// Skips with this at zero were the graph skipping us, not our DSP.
    pub callbacks_over_budget: AtomicU64,

    // ---- capacity: is this buffer size viable? ----------------------------
    /// Callbacks that took >= 80% of budget, over-budget ones included. Rises
    /// before audio breaks.
    pub callbacks_near_budget: AtomicU64,
    /// Slowest callback this window. Cleared by the monitor each tick.
    pub longest_callback_nanos_window: AtomicU64,
    /// Slowest callback of the whole run, for the teardown summary.
    pub longest_callback_nanos_session: AtomicU64,

    // ---- instrument integrity, not health ---------------------------------
    /// Detail events dropped because the event queue was full. Guards against
    /// reading a sparse event log as "few problems".
    pub detail_events_dropped: AtomicU64,
    /// Times the device clock jumped further than [`MAX_PLAUSIBLE_GAP_NANOS`],
    /// leaving a hole in the skip accounting rather than a dropout.
    pub clock_discontinuities: AtomicU64,

    /// The verdict for a display, written by the monitor once a second.
    digest: AtomicCell<HealthDigest>,

    /// Per-deck sound-quality health, indexed by deck id.
    decks: Vec<Arc<DeckHealth>>,
}

impl AudioHealth {
    /// `frames_per_callback` is what we request from the device; the budget
    /// follows from it and the stream's sample rate.
    pub fn new(frames_per_callback: u32, sample_rate: u32, decks: usize) -> Arc<Self> {
        let callback_budget_nanos = if sample_rate == 0 {
            0
        } else {
            1_000_000_000u64 * frames_per_callback as u64 / sample_rate as u64
        };

        Arc::new(Self {
            callback_budget_nanos: AtomicU64::new(callback_budget_nanos),
            frames_per_callback_observed: AtomicU32::new(frames_per_callback),
            sample_rate,
            frames_per_callback_expected: frames_per_callback,
            stream_epoch_nanos: AtomicU64::new(0),
            callbacks_served: AtomicU64::new(0),
            callbacks_skipped: AtomicU64::new(0),
            skip_incidents: AtomicU64::new(0),
            longest_gap_between_callbacks_nanos_window: AtomicU64::new(0),
            longest_gap_between_callbacks_nanos_session: AtomicU64::new(0),
            callbacks_silenced_by_frame_mismatch: AtomicU64::new(0),
            callbacks_over_budget: AtomicU64::new(0),
            callbacks_near_budget: AtomicU64::new(0),
            longest_callback_nanos_window: AtomicU64::new(0),
            longest_callback_nanos_session: AtomicU64::new(0),
            detail_events_dropped: AtomicU64::new(0),
            clock_discontinuities: AtomicU64::new(0),
            digest: AtomicCell::new(HealthDigest::new(Instant::now())),
            decks: (0..decks).map(|_| Arc::new(DeckHealth::new())).collect(),
        })
    }

    /// The verdict for a display. See [`HealthDigest`].
    pub fn digest(&self) -> HealthDigest {
        self.digest.load()
    }

    /// Handle for one deck's processor to write its own metrics through.
    pub fn deck(&self, deck_id: usize) -> Arc<DeckHealth> {
        Arc::clone(&self.decks[deck_id])
    }

    pub fn decks(&self) -> &[Arc<DeckHealth>] {
        &self.decks
    }

    /// Device-clock stamp turned into seconds since the stream started.
    fn since_stream_start(&self, at_nanos: u64) -> f64 {
        let epoch = self.stream_epoch_nanos.load(Relaxed);
        at_nanos.saturating_sub(epoch) as f64 / 1_000_000_000.0
    }
}

/// A single interesting occurrence, for the times a counter is not enough to
/// know what happened. `Copy` and free of heap data so the audio thread can push
/// one without allocating.
#[derive(Debug, Clone, Copy)]
pub enum AudioEvent {
    /// One callback overran its budget.
    SlowCallback {
        at_nanos: u64,
        elapsed_nanos: u64,
        frames: u32,
    },
    /// The device clock jumped by more than one callback period.
    Gap {
        at_nanos: u64,
        skipped: u64,
        gap_nanos: u64,
    },
    /// The callback asked for a frame count we are not configured for, so the
    /// whole block went out as silence.
    FrameMismatch {
        at_nanos: u64,
        got: u32,
        expected: u32,
    },
}

impl AudioEvent {
    fn at_nanos(&self) -> u64 {
        match *self {
            AudioEvent::SlowCallback { at_nanos, .. }
            | AudioEvent::Gap { at_nanos, .. }
            | AudioEvent::FrameMismatch { at_nanos, .. } => at_nanos,
        }
    }
}

/// The audio thread's handle onto [`AudioHealth`].
///
/// Owns the cursor state that must not be shared: two writers of
/// `prev_callback_nanos` would make the gap arithmetic meaningless.
pub struct HealthRecorder {
    health: Arc<AudioHealth>,
    events: Producer<AudioEvent>,
    /// device-clock stamp of the previous callback, 0 before the first one
    prev_callback_nanos: u64,
    /// device-clock stamp of the last event pushed, for rate limiting
    last_event_nanos: u64,
    /// frames the last callback carried, to notice the quantum moving
    observed_frames: u32,
    /// whether the next gap straddles a quantum change
    quantum_changed: bool,
}

/// Builds the audio-thread handle and the monitor's end of the event queue.
pub fn new_recorder(health: Arc<AudioHealth>) -> (HealthRecorder, Consumer<AudioEvent>) {
    let (producer, consumer) = RingBuffer::new(EVENT_QUEUE_CAPACITY);
    (
        HealthRecorder {
            health,
            events: producer,
            prev_callback_nanos: 0,
            observed_frames: 0,
            quantum_changed: false,
            last_event_nanos: 0,
        },
        consumer,
    )
}

impl HealthRecorder {
    pub fn health(&self) -> &Arc<AudioHealth> {
        &self.health
    }

    /// Counts the callback and works out how many the device expected in the
    /// meantime. `callback_nanos` is the device clock, the only one that knows
    /// how many callbacks the graph made while we were away.
    #[inline]
    pub fn on_callback_start(&mut self, callback_nanos: u64, frames: u32) {
        self.health.callbacks_served.fetch_add(1, Relaxed);
        self.follow_quantum(frames);

        let budget = self.health.callback_budget_nanos.load(Relaxed);
        if callback_nanos == 0 || budget == 0 {
            // no usable device clock; counters that do not depend on it still work
            return;
        }

        if self.prev_callback_nanos == 0 {
            self.health
                .stream_epoch_nanos
                .store(callback_nanos, Relaxed);
        }

        if self.prev_callback_nanos != 0 && callback_nanos > self.prev_callback_nanos {
            let gap = callback_nanos - self.prev_callback_nanos;

            if gap > MAX_PLAUSIBLE_GAP_NANOS {
                self.health.clock_discontinuities.fetch_add(1, Relaxed);
                self.prev_callback_nanos = callback_nanos;
                return;
            }

            self.health
                .longest_gap_between_callbacks_nanos_window
                .fetch_max(gap, Relaxed);
            self.health
                .longest_gap_between_callbacks_nanos_session
                .fetch_max(gap, Relaxed);

            // One gap spans the old period and the new one, so it divides by
            // neither.
            if std::mem::take(&mut self.quantum_changed) {
                self.prev_callback_nanos = callback_nanos;
                return;
            }

            // rounded, so ordinary jitter cannot be mistaken for a skip
            let cycles = ((gap + budget / 2) / budget).max(1);
            if cycles > 1 {
                let skipped = cycles - 1;
                self.health.callbacks_skipped.fetch_add(skipped, Relaxed);
                self.health.skip_incidents.fetch_add(1, Relaxed);
                self.push(AudioEvent::Gap {
                    at_nanos: callback_nanos,
                    skipped,
                    gap_nanos: gap,
                });
            }
        }

        self.prev_callback_nanos = callback_nanos;
    }

    /// The callback handed us a frame count we cannot render, so the block is
    /// silence.
    #[inline]
    pub fn on_frame_mismatch(&mut self, callback_nanos: u64, got: u32) {
        self.health
            .callbacks_silenced_by_frame_mismatch
            .fetch_add(1, Relaxed);
        let expected = self.health.frames_per_callback_expected;
        self.push(AudioEvent::FrameMismatch {
            at_nanos: callback_nanos,
            got,
            expected,
        });
    }

    /// Keeps the budget on the quantum the graph is actually running, which
    /// changes whenever another client joins asking for a smaller one.
    #[inline]
    fn follow_quantum(&mut self, frames: u32) {
        if frames == 0 || frames == self.observed_frames {
            return;
        }

        // Not on the first callback, which establishes the quantum rather than
        // moving it.
        self.quantum_changed = self.observed_frames != 0;
        self.observed_frames = frames;
        self.health
            .frames_per_callback_observed
            .store(frames, Relaxed);
        self.health.callback_budget_nanos.store(
            1_000_000_000u64 * frames as u64 / self.health.sample_rate.max(1) as u64,
            Relaxed,
        );
    }

    /// Records how long the whole callback took against its budget.
    #[inline]
    pub fn on_callback_end(&mut self, callback_nanos: u64, elapsed_nanos: u64, frames: u32) {
        self.health
            .longest_callback_nanos_window
            .fetch_max(elapsed_nanos, Relaxed);
        self.health
            .longest_callback_nanos_session
            .fetch_max(elapsed_nanos, Relaxed);

        let budget = self.health.callback_budget_nanos.load(Relaxed);
        if budget == 0 {
            return;
        }

        if elapsed_nanos * 100 >= budget * NEAR_BUDGET_PERCENT {
            self.health.callbacks_near_budget.fetch_add(1, Relaxed);
        }
        if elapsed_nanos >= budget {
            self.health.callbacks_over_budget.fetch_add(1, Relaxed);
            self.push(AudioEvent::SlowCallback {
                at_nanos: callback_nanos,
                elapsed_nanos,
                frames,
            });
        }
    }

    /// Rate limited at the source so a sustained failure cannot flood the queue.
    #[inline]
    fn push(&mut self, event: AudioEvent) {
        let at = event.at_nanos();
        if self.last_event_nanos != 0
            && at.saturating_sub(self.last_event_nanos) < EVENT_MIN_INTERVAL_NANOS
        {
            return;
        }
        match self.events.push(event) {
            Ok(()) => self.last_event_nanos = at,
            Err(rtrb::PushError::Full(_)) => {
                self.health.detail_events_dropped.fetch_add(1, Relaxed);
            }
        }
    }
}

struct DeckSnapshot {
    callbacks_rendered: u64,
    lag_correction_maxed: u64,
    stale_platter_samples: u64,
    records_dropped: u64,
    playhead_jumps: u64,
}

struct Snapshot {
    clock_discontinuities: u64,
    callbacks_served: u64,
    callbacks_skipped: u64,
    skip_incidents: u64,
    callbacks_silenced_by_frame_mismatch: u64,
    callbacks_over_budget: u64,
    callbacks_near_budget: u64,
    detail_events_dropped: u64,
    decks: Vec<DeckSnapshot>,
}

impl Snapshot {
    fn take(health: &AudioHealth) -> Self {
        Self {
            clock_discontinuities: health.clock_discontinuities.load(Relaxed),
            callbacks_served: health.callbacks_served.load(Relaxed),
            callbacks_skipped: health.callbacks_skipped.load(Relaxed),
            skip_incidents: health.skip_incidents.load(Relaxed),
            callbacks_silenced_by_frame_mismatch: health
                .callbacks_silenced_by_frame_mismatch
                .load(Relaxed),
            callbacks_over_budget: health.callbacks_over_budget.load(Relaxed),
            callbacks_near_budget: health.callbacks_near_budget.load(Relaxed),
            detail_events_dropped: health.detail_events_dropped.load(Relaxed),
            decks: health
                .decks()
                .iter()
                .map(|deck| DeckSnapshot {
                    callbacks_rendered: deck.callbacks_rendered.load(Relaxed),
                    lag_correction_maxed: deck.callbacks_with_lag_correction_maxed.load(Relaxed),
                    stale_platter_samples: deck
                        .callbacks_without_fresh_platter_sample
                        .load(Relaxed),
                    records_dropped: deck.records_dropped_on_audio_thread.load(Relaxed),
                    playhead_jumps: deck.playhead_jumps.load(Relaxed),
                })
                .collect(),
        }
    }
}

fn micros(nanos: u64) -> f64 {
    nanos as f64 / 1_000.0
}

fn millis(nanos: u64) -> f64 {
    nanos as f64 / 1_000_000.0
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

/// Starts the thread that turns the atomics into log lines. Nothing else logs
/// audio health, so the audio thread never has to.
pub fn spawn_monitor(
    health: Arc<AudioHealth>,
    mut events: Consumer<AudioEvent>,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        log::info!(
            "audio health: {} frames per callback, {:.0}us budget",
            health.frames_per_callback_expected,
            micros(health.callback_budget_nanos.load(Relaxed))
        );

        let session_start = Instant::now();
        let mut previous = Snapshot::take(&health);
        let mut window_start = Instant::now();
        let mut last_loss = None;

        while !shutdown.load(Relaxed) {
            std::thread::sleep(POLL_INTERVAL);
            drain_events(&health, &mut events);

            if window_start.elapsed() >= REPORT_INTERVAL {
                let current = Snapshot::take(&health);
                let lost = report_window(&health, &previous, &current);
                publish_digest(&health, &mut last_loss, lost, session_start);
                previous = current;
                window_start = Instant::now();
            }
        }

        drain_events(&health, &mut events);
        report_session(&health, session_start.elapsed());
    })
}

fn drain_events(health: &AudioHealth, events: &mut Consumer<AudioEvent>) {
    let budget = health.callback_budget_nanos.load(Relaxed);
    while let Ok(event) = events.pop() {
        match event {
            AudioEvent::SlowCallback {
                at_nanos,
                elapsed_nanos,
                frames,
            } => log::warn!(
                "audio: callback at {:.3}s took {:.0}us for {frames} frames ({:.0}% of {:.0}us budget)",
                health.since_stream_start(at_nanos),
                micros(elapsed_nanos),
                percent(elapsed_nanos, budget),
                micros(budget),
            ),
            AudioEvent::Gap {
                at_nanos,
                skipped,
                gap_nanos,
            } => log::warn!(
                "audio: gap at {:.3}s of {:.2}ms, {skipped} callback(s) skipped",
                health.since_stream_start(at_nanos),
                millis(gap_nanos),
            ),
            AudioEvent::FrameMismatch {
                at_nanos,
                got,
                expected,
            } => log::warn!(
                "audio: callback at {:.3}s asked for {got} frames, expected {expected}; block silenced",
                health.since_stream_start(at_nanos),
            ),
        }
    }
}

/// Turns one second of counters into the verdict a display shows.
fn publish_digest(
    health: &AudioHealth,
    last_loss: &mut Option<Instant>,
    lost: u64,
    session_start: Instant,
) {
    if lost > 0 {
        *last_loss = Some(Instant::now());
    }

    health.digest.store(HealthDigest {
        level: HealthLevel::of(lost),
        lost,
        started: session_start,
        last_loss: *last_loss,
    });
}

fn report_window(health: &AudioHealth, previous: &Snapshot, current: &Snapshot) -> u64 {
    let budget = health.callback_budget_nanos.load(Relaxed);

    let served = current.callbacks_served - previous.callbacks_served;
    let skipped = current.callbacks_skipped - previous.callbacks_skipped;
    let incidents = current.skip_incidents - previous.skip_incidents;
    let silenced = current.callbacks_silenced_by_frame_mismatch
        - previous.callbacks_silenced_by_frame_mismatch;
    let over = current.callbacks_over_budget - previous.callbacks_over_budget;
    let near = current.callbacks_near_budget - previous.callbacks_near_budget;
    let dropped = current.detail_events_dropped - previous.detail_events_dropped;
    let clock_jumps = current.clock_discontinuities - previous.clock_discontinuities;
    let expected = served + skipped;

    let slowest = health.longest_callback_nanos_window.swap(0, Relaxed);
    let longest_gap = health
        .longest_gap_between_callbacks_nanos_window
        .swap(0, Relaxed);

    let damaged = skipped > 0 || silenced > 0 || over > 0;
    let summary = format!(
        "audio: {served}/{expected} callbacks, {skipped} skipped in {incidents} incident(s) \
         ({:.1}%, {:.1}ms silence), longest gap {:.2}ms | slowest {:.0}us of {:.0}us budget \
         ({:.0}%) | near-budget {near}, over-budget {over} | silenced {silenced}",
        percent(skipped, expected),
        millis(skipped * budget),
        millis(longest_gap),
        micros(slowest),
        micros(budget),
        percent(slowest, budget),
    );

    if damaged {
        log::warn!("{summary}");
    } else {
        log::info!("{summary}");
    }

    if dropped > 0 {
        log::warn!("audio: {dropped} detail event(s) dropped, event log is incomplete");
    }

    // The numbers above are measured against the quantum that arrived.
    let observed = health.frames_per_callback_observed.load(Relaxed);
    if observed != 0 && observed != health.frames_per_callback_expected {
        log::warn!(
            "audio: the graph is running {observed} frames per callback, not the {} asked for",
            health.frames_per_callback_expected
        );
    }

    if clock_jumps > 0 {
        log::warn!(
            "audio: device clock jumped {clock_jumps} time(s); skips are unmeasured across those"
        );
    }

    for (deck_id, (deck, before)) in health.decks().iter().zip(previous.decks.iter()).enumerate() {
        let after = &current.decks[deck_id];
        let rendered = after.callbacks_rendered - before.callbacks_rendered;
        let maxed = after.lag_correction_maxed - before.lag_correction_maxed;
        let stale = after.stale_platter_samples - before.stale_platter_samples;
        let dropped = after.records_dropped - before.records_dropped;
        let jumps = after.playhead_jumps - before.playhead_jumps;
        let lag = deck.playback_lag();

        if jumps > 0 {
            log::info!("audio deck{deck_id}: {jumps} playhead jump(s)");
        }

        let line = format!(
            "audio deck{deck_id}: lag {:.0}ms | correction maxed {maxed}/{rendered} callbacks | \
             {stale} stale platter sample(s) | {dropped} record(s) dropped on audio thread",
            lag as f64 / 1_000_000.0,
        );

        if dropped > 0 || stale > 0 || maxed > 0 || lag.abs() > LAG_WARN_NANOS {
            log::warn!("{line}");
        } else {
            log::info!("{line}");
        }
    }

    // A silenced callback was served but carried no audio, so it counts as lost.
    skipped + silenced
}

fn report_session(health: &AudioHealth, elapsed: Duration) {
    let budget = health.callback_budget_nanos.load(Relaxed);
    let served = health.callbacks_served.load(Relaxed);
    let skipped = health.callbacks_skipped.load(Relaxed);
    let expected = served + skipped;

    log::info!(
        "audio session: {:.1}s, {served}/{expected} callbacks served, {skipped} skipped \
         ({:.2}%, {:.0}ms silence) in {} incident(s), longest gap {:.2}ms, \
         slowest callback {:.0}us of {:.0}us budget, {} silenced callback(s), \
         {} over budget, {} near budget, {} detail event(s) dropped",
        elapsed.as_secs_f64(),
        percent(skipped, expected),
        millis(skipped * budget),
        health.skip_incidents.load(Relaxed),
        millis(
            health
                .longest_gap_between_callbacks_nanos_session
                .load(Relaxed)
        ),
        micros(health.longest_callback_nanos_session.load(Relaxed)),
        micros(budget),
        health.callbacks_silenced_by_frame_mismatch.load(Relaxed),
        health.callbacks_over_budget.load(Relaxed),
        health.callbacks_near_budget.load(Relaxed),
        health.detail_events_dropped.load(Relaxed),
    );

    let clock_jumps = health.clock_discontinuities.load(Relaxed);
    if clock_jumps > 0 {
        log::info!("audio session: device clock jumped {clock_jumps} time(s)");
    }

    for (deck_id, deck) in health.decks().iter().enumerate() {
        log::info!(
            "audio session deck{deck_id}: {} callbacks, lag {:.0}ms at exit, \
             correction maxed on {} callback(s), {} stale platter sample(s), \
             {} record(s) dropped on audio thread",
            deck.callbacks_rendered.load(Relaxed),
            deck.playback_lag_nanos.load(Relaxed) as f64 / 1_000_000.0,
            deck.callbacks_with_lag_correction_maxed.load(Relaxed),
            deck.callbacks_without_fresh_platter_sample.load(Relaxed),
            deck.records_dropped_on_audio_thread.load(Relaxed),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioHealth, HealthLevel, new_recorder};
    use std::sync::atomic::Ordering::Relaxed;

    /// Drives callbacks of `frames` at their natural spacing, starting from
    /// device-clock `from`, and returns where the clock ended up.
    fn play(recorder: &mut super::HealthRecorder, from: u64, frames: u32, count: usize) -> u64 {
        let period = 1_000_000_000u64 * frames as u64 / 48_000;
        let mut at = from;

        for _ in 0..count {
            at += period;
            recorder.on_callback_start(at, frames);
            recorder.on_callback_end(at, 100_000, frames);
        }

        at
    }

    /// Another client joining the graph moves the quantum under a running
    /// stream. Nothing was lost, so nothing may be reported as lost.
    #[test]
    fn a_quantum_change_is_not_a_dropout() {
        let health = AudioHealth::new(256, 48_000, 1);
        let (mut recorder, _events) = new_recorder(health.clone());

        let at = play(&mut recorder, 1_000_000_000, 256, 20);
        play(&mut recorder, at, 512, 20);

        assert_eq!(
            health.callbacks_skipped.load(Relaxed),
            0,
            "a quantum change was counted as lost audio"
        );
    }

    /// 64 frames at 48kHz: a 1333us budget, as `--buffer 64` gives.
    fn recorder() -> super::HealthRecorder {
        new_recorder(AudioHealth::new(64, 48_000, 1)).0
    }

    #[test]
    fn a_missed_callback_counts_as_skipped() {
        let mut r = recorder();
        r.on_callback_start(1_000_000_000, 64);
        r.on_callback_start(1_000_000_000 + 4 * 1_333_333, 64);
        assert_eq!(r.health().callbacks_skipped.load(Relaxed), 3);
        assert_eq!(r.health().clock_discontinuities.load(Relaxed), 0);
    }

    #[test]
    fn a_clock_jump_is_not_millions_of_dropouts() {
        let mut r = recorder();
        r.on_callback_start(1_000_000_000, 64);
        // cpal falls back from PipeWire's graph clock to CLOCK_MONOTONIC, so the
        // stamp leaps epochs. It is not 7389 seconds of lost audio.
        r.on_callback_start(1_000_000_000 + 7_389_843_500_000, 64);
        assert_eq!(r.health().callbacks_skipped.load(Relaxed), 0);
        assert_eq!(r.health().clock_discontinuities.load(Relaxed), 1);
    }

    #[test]
    fn ordinary_jitter_is_not_a_skip() {
        let mut r = recorder();
        r.on_callback_start(1_000_000_000, 64);
        r.on_callback_start(1_000_000_000 + 1_700_000, 64);
        assert_eq!(r.health().callbacks_skipped.load(Relaxed), 0);
    }

    #[test]
    fn losing_nothing_is_clean() {
        assert_eq!(HealthLevel::of(0), HealthLevel::Clean);
    }

    /// Literal on purpose: written against [`MANY_LOST_CALLBACKS`] it would
    /// hold for any value of it.
    #[test]
    fn one_or_two_dropouts_is_a_glitch() {
        assert_eq!(HealthLevel::of(1), HealthLevel::Glitching);
        assert_eq!(HealthLevel::of(2), HealthLevel::Glitching);
    }

    #[test]
    fn three_or_more_is_failing() {
        assert_eq!(HealthLevel::of(3), HealthLevel::Failing);
        assert_eq!(HealthLevel::of(1500), HealthLevel::Failing);
    }
}
