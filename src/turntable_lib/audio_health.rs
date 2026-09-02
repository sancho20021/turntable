//! Audio-thread health metrics.
//!
//! The audio callback must not log: `env_logger`'s file target does a `format!`,
//! takes a mutex and issues an unbuffered `write()` per record, which at a
//! 32-frame quantum costs a sizeable fraction of a 667us budget - and blocks for
//! milliseconds whenever the filesystem commits its journal. So the callback
//! only bumps relaxed atomics here and occasionally pushes a detail event onto a
//! lock-free queue. [`spawn_monitor`] does all the logging on its own thread.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::Relaxed},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use rtrb::{Consumer, Producer, RingBuffer};

/// Capacity of the detail-event queue. Events are rate limited at the source, so
/// this only has to absorb a burst.
const EVENT_QUEUE_CAPACITY: usize = 64;

/// Minimum spacing between detail events pushed from the audio thread. A detail
/// event is a concrete example, not a rate: the per-window counters already say
/// how often something happens, so one per second is plenty.
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

/// Per-deck sound-quality health. Lives inside [`AudioHealth`]; one instance per
/// deck, because a single shared lag gauge would be overwritten by whichever
/// deck ran last.
pub struct DeckHealth {
    /// Blocks this deck rendered. Not health on its own; it's what the rest of
    /// this struct divides by.
    pub blocks_processed: AtomicU64,

    /// How far the playback clock currently trails the platter (a gauge, not a
    /// counter). It is both the audible offset and the gain applied to slope
    /// noise by the extrapolation in the audio processor.
    pub playback_lag_nanos: AtomicI64,
    /// Blocks where the lag correction hit its +/-5ms clamp, so it ran at its
    /// ceiling of ~2.5ms/s of catch-up. The lag still moves at that rate; which
    /// way depends on whether skips inject lag faster. Read alongside
    /// `playback_lag_nanos` for the direction.
    pub blocks_with_lag_correction_maxed: AtomicU64,
    /// Blocks that found no fresh platter sample and had to extrapolate from a
    /// stale slope. Should be 0 while the platter thread outruns the audio one.
    pub callbacks_without_fresh_platter_sample: AtomicU64,
    /// Times a whole decoded track was freed inside the callback because the
    /// recycling ring was full. Always causes a stall. Expected value: 0.
    pub tracks_freed_on_audio_thread: AtomicU64,
}

impl DeckHealth {
    fn new() -> Self {
        Self {
            blocks_processed: AtomicU64::new(0),
            playback_lag_nanos: AtomicI64::new(0),
            blocks_with_lag_correction_maxed: AtomicU64::new(0),
            callbacks_without_fresh_platter_sample: AtomicU64::new(0),
            tracks_freed_on_audio_thread: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn block_processed(&self) {
        self.blocks_processed.fetch_add(1, Relaxed);
    }

    #[inline]
    pub fn set_playback_lag(&self, lag_nanos: i64) {
        self.playback_lag_nanos.store(lag_nanos, Relaxed);
    }

    #[inline]
    pub fn lag_correction_maxed(&self) {
        self.blocks_with_lag_correction_maxed.fetch_add(1, Relaxed);
    }

    #[inline]
    pub fn stale_platter_sample(&self) {
        self.callbacks_without_fresh_platter_sample
            .fetch_add(1, Relaxed);
    }

    #[inline]
    pub fn track_freed_on_audio_thread(&self) {
        self.tracks_freed_on_audio_thread.fetch_add(1, Relaxed);
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
    /// Time one callback has to finish: `frames_per_callback_expected / SAMPLE_RATE`.
    pub callback_budget_nanos: u64,
    /// Frames per callback we configured. The negotiated PipeWire quantum can
    /// differ from this; that divergence is `blocks_silenced_by_frame_mismatch`.
    pub frames_per_callback_expected: u32,
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
    /// each one swallowed. `callbacks_skipped / skip_incidents` is the average
    /// hole size: many small holes are constant crackle, few large ones are
    /// occasional stutters.
    pub skip_incidents: AtomicU64,
    /// Longest time between two consecutive callbacks this window. Divide by
    /// `callback_budget_nanos` for the worst single hole, in callbacks. Not
    /// attributed. Cleared by the monitor each tick.
    pub longest_gap_between_callbacks_nanos_window: AtomicU64,
    /// Longest gap of the whole run, for the teardown summary.
    pub longest_gap_between_callbacks_nanos_session: AtomicU64,
    /// Blocks we zero-filled ourselves because the callback's frame count did
    /// not match `frames_per_callback_expected`. On time, but silent, so the
    /// device clock never sees these.
    pub blocks_silenced_by_frame_mismatch: AtomicU64,

    // ---- cause: whose fault was it? ---------------------------------------
    /// Callbacks that took >= 100% of budget, i.e. we definitely caused a skip.
    /// Both this and `callbacks_skipped` high means we are too slow; skips with
    /// this at zero means the graph skipped us and our DSP is not the problem.
    pub callbacks_over_budget: AtomicU64,

    // ---- capacity: is this buffer size viable? ----------------------------
    /// Callbacks that took >= 80% of budget, over-budget ones included. Rises
    /// before audio breaks, so it is the warning ahead of
    /// `callbacks_over_budget`.
    pub callbacks_near_budget: AtomicU64,
    /// Slowest callback this window. Cleared by the monitor each tick.
    pub longest_callback_nanos_window: AtomicU64,
    /// Slowest callback of the whole run, for the teardown summary.
    pub longest_callback_nanos_session: AtomicU64,

    // ---- instrument integrity, not health ---------------------------------
    /// Detail events dropped because the event queue was full. Guards against
    /// reading a sparse event log as "few problems".
    pub detail_events_dropped: AtomicU64,

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
            callback_budget_nanos,
            frames_per_callback_expected: frames_per_callback,
            stream_epoch_nanos: AtomicU64::new(0),
            callbacks_served: AtomicU64::new(0),
            callbacks_skipped: AtomicU64::new(0),
            skip_incidents: AtomicU64::new(0),
            longest_gap_between_callbacks_nanos_window: AtomicU64::new(0),
            longest_gap_between_callbacks_nanos_session: AtomicU64::new(0),
            blocks_silenced_by_frame_mismatch: AtomicU64::new(0),
            callbacks_over_budget: AtomicU64::new(0),
            callbacks_near_budget: AtomicU64::new(0),
            longest_callback_nanos_window: AtomicU64::new(0),
            longest_callback_nanos_session: AtomicU64::new(0),
            detail_events_dropped: AtomicU64::new(0),
            decks: (0..decks).map(|_| Arc::new(DeckHealth::new())).collect(),
        })
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
/// Owns the cursor state that must not be shared: two decks writing the same
/// `prev_callback_nanos` would make the gap arithmetic meaningless, so it lives
/// here rather than in the shared struct.
pub struct HealthRecorder {
    health: Arc<AudioHealth>,
    events: Producer<AudioEvent>,
    /// device-clock stamp of the previous callback, 0 before the first one
    prev_callback_nanos: u64,
    /// device-clock stamp of the last event pushed, for rate limiting
    last_event_nanos: u64,
}

/// Builds the audio-thread handle and the monitor's end of the event queue.
pub fn new_recorder(health: Arc<AudioHealth>) -> (HealthRecorder, Consumer<AudioEvent>) {
    let (producer, consumer) = RingBuffer::new(EVENT_QUEUE_CAPACITY);
    (
        HealthRecorder {
            health,
            events: producer,
            prev_callback_nanos: 0,
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
    /// meantime. `callback_nanos` is the device clock, not ours: it is the only
    /// source that knows how much audio actually left the DAC.
    #[inline]
    pub fn on_callback_start(&mut self, callback_nanos: u64) {
        self.health.callbacks_served.fetch_add(1, Relaxed);

        let budget = self.health.callback_budget_nanos;
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
            self.health
                .longest_gap_between_callbacks_nanos_window
                .fetch_max(gap, Relaxed);
            self.health
                .longest_gap_between_callbacks_nanos_session
                .fetch_max(gap, Relaxed);

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
            .blocks_silenced_by_frame_mismatch
            .fetch_add(1, Relaxed);
        let expected = self.health.frames_per_callback_expected;
        self.push(AudioEvent::FrameMismatch {
            at_nanos: callback_nanos,
            got,
            expected,
        });
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

        let budget = self.health.callback_budget_nanos;
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

    /// Rate limited at the source: a sustained failure must not flood the queue,
    /// the counters already carry its rate.
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
    blocks_processed: u64,
    lag_correction_maxed: u64,
    stale_platter_samples: u64,
    tracks_freed: u64,
}

struct Snapshot {
    callbacks_served: u64,
    callbacks_skipped: u64,
    skip_incidents: u64,
    blocks_silenced_by_frame_mismatch: u64,
    callbacks_over_budget: u64,
    callbacks_near_budget: u64,
    detail_events_dropped: u64,
    decks: Vec<DeckSnapshot>,
}

impl Snapshot {
    fn take(health: &AudioHealth) -> Self {
        Self {
            callbacks_served: health.callbacks_served.load(Relaxed),
            callbacks_skipped: health.callbacks_skipped.load(Relaxed),
            skip_incidents: health.skip_incidents.load(Relaxed),
            blocks_silenced_by_frame_mismatch: health
                .blocks_silenced_by_frame_mismatch
                .load(Relaxed),
            callbacks_over_budget: health.callbacks_over_budget.load(Relaxed),
            callbacks_near_budget: health.callbacks_near_budget.load(Relaxed),
            detail_events_dropped: health.detail_events_dropped.load(Relaxed),
            decks: health
                .decks()
                .iter()
                .map(|deck| DeckSnapshot {
                    blocks_processed: deck.blocks_processed.load(Relaxed),
                    lag_correction_maxed: deck.blocks_with_lag_correction_maxed.load(Relaxed),
                    stale_platter_samples: deck
                        .callbacks_without_fresh_platter_sample
                        .load(Relaxed),
                    tracks_freed: deck.tracks_freed_on_audio_thread.load(Relaxed),
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
            micros(health.callback_budget_nanos)
        );

        let session_start = Instant::now();
        let mut previous = Snapshot::take(&health);
        let mut window_start = Instant::now();

        while !shutdown.load(Relaxed) {
            std::thread::sleep(POLL_INTERVAL);
            drain_events(&health, &mut events);

            if window_start.elapsed() >= REPORT_INTERVAL {
                let current = Snapshot::take(&health);
                report_window(&health, &previous, &current);
                previous = current;
                window_start = Instant::now();
            }
        }

        drain_events(&health, &mut events);
        report_session(&health, session_start.elapsed());
    })
}

fn drain_events(health: &AudioHealth, events: &mut Consumer<AudioEvent>) {
    let budget = health.callback_budget_nanos;
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

fn report_window(health: &AudioHealth, previous: &Snapshot, current: &Snapshot) {
    let budget = health.callback_budget_nanos;

    let served = current.callbacks_served - previous.callbacks_served;
    let skipped = current.callbacks_skipped - previous.callbacks_skipped;
    let incidents = current.skip_incidents - previous.skip_incidents;
    let silenced =
        current.blocks_silenced_by_frame_mismatch - previous.blocks_silenced_by_frame_mismatch;
    let over = current.callbacks_over_budget - previous.callbacks_over_budget;
    let near = current.callbacks_near_budget - previous.callbacks_near_budget;
    let dropped = current.detail_events_dropped - previous.detail_events_dropped;
    let expected = served + skipped;

    let slowest = health.longest_callback_nanos_window.swap(0, Relaxed);
    let longest_gap = health
        .longest_gap_between_callbacks_nanos_window
        .swap(0, Relaxed);

    let damaged = skipped > 0 || silenced > 0 || over > 0;
    let summary = format!(
        "audio: {served}/{expected} callbacks, {skipped} skipped in {incidents} incident(s) \
         ({:.1}%, {:.1}ms silence), longest gap {:.2}ms | slowest {:.0}us of {:.0}us budget \
         ({:.0}%) | near-budget {near}, over-budget {over} | silenced blocks {silenced}",
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

    for (deck_id, (deck, before)) in health
        .decks()
        .iter()
        .zip(previous.decks.iter())
        .enumerate()
    {
        let after = &current.decks[deck_id];
        let blocks = after.blocks_processed - before.blocks_processed;
        let maxed = after.lag_correction_maxed - before.lag_correction_maxed;
        let stale = after.stale_platter_samples - before.stale_platter_samples;
        let freed = after.tracks_freed - before.tracks_freed;
        let lag = deck.playback_lag_nanos.load(Relaxed);

        let line = format!(
            "audio deck{deck_id}: lag {:.0}ms | correction maxed {maxed}/{blocks} blocks | \
             {stale} stale platter sample(s) | {freed} track(s) freed on audio thread",
            lag as f64 / 1_000_000.0,
        );

        // A saturated correction or a large lag is the whole point of this line,
        // so it must not be hidden at info while only stale/freed promote it.
        if freed > 0 || stale > 0 || maxed > 0 || lag.abs() > LAG_WARN_NANOS {
            log::warn!("{line}");
        } else {
            log::info!("{line}");
        }
    }
}

fn report_session(health: &AudioHealth, elapsed: Duration) {
    let budget = health.callback_budget_nanos;
    let served = health.callbacks_served.load(Relaxed);
    let skipped = health.callbacks_skipped.load(Relaxed);
    let expected = served + skipped;

    log::info!(
        "audio session: {:.1}s, {served}/{expected} callbacks served, {skipped} skipped \
         ({:.2}%, {:.0}ms silence) in {} incident(s), longest gap {:.2}ms, \
         slowest callback {:.0}us of {:.0}us budget, {} silenced block(s), \
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
        health.blocks_silenced_by_frame_mismatch.load(Relaxed),
        health.callbacks_over_budget.load(Relaxed),
        health.callbacks_near_budget.load(Relaxed),
        health.detail_events_dropped.load(Relaxed),
    );

    for (deck_id, deck) in health.decks().iter().enumerate() {
        log::info!(
            "audio session deck{deck_id}: {} blocks, lag {:.0}ms at exit, \
             correction maxed on {} block(s), {} stale platter sample(s), \
             {} track(s) freed on audio thread",
            deck.blocks_processed.load(Relaxed),
            deck.playback_lag_nanos.load(Relaxed) as f64 / 1_000_000.0,
            deck.blocks_with_lag_correction_maxed.load(Relaxed),
            deck.callbacks_without_fresh_platter_sample.load(Relaxed),
            deck.tracks_freed_on_audio_thread.load(Relaxed),
        );
    }
}
