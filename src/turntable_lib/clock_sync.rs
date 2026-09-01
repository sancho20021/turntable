//! Putting an input source's own clock onto the same timeline as [`Instant`].
//!
//! An input device that timestamps its own events gives us better timing than
//! reading the clock when we notice them: the device stamp is applied when the
//! event happened, ours is applied when the scheduler got around to our thread.
//! For scratching that difference is not cosmetic - velocity is
//! `delta position / delta t` (see [`crate::deck_controller`]), so jitter in the
//! gap between events is jitter in the speed the platter is driven at.
//!
//! What such a clock never gives us is an origin we can relate to anything else.
//! midir's contract is typical: microseconds "since some unspecified point in
//! the past", fixed for the lifetime of the connection.

use std::time::{Duration, Instant};

/// Recovers the offset between an external monotonic clock and [`Instant`].
///
/// This is the offset problem NTP and PTP solve, and the same estimator: every
/// event is observed at
///
/// ```text
/// arrived = origin + elapsed + delay,    delay >= 0
/// ```
///
/// where `elapsed` is what the device's own clock says has passed since the
/// first event we saw. So `arrived - elapsed` always *over*-estimates `origin`,
/// by exactly this event's delay - and therefore the smallest value ever seen is
/// the best estimate available. It converges within the first few events, can
/// only improve, and needs no state beyond a running minimum.
///
/// The estimate is only ever as good as the luckiest event so far, so the very
/// first stamp carries that event's full delay. Everything after it is bounded
/// by the smallest delay seen to date.
///
/// # Assumptions
///
/// The external clock must be monotonic and must not drift in *rate* against
/// `Instant`. On Linux both are `CLOCK_MONOTONIC` underneath, so a running
/// minimum stays correct for a whole run - measured against ALSA over 20 s it
/// held to about a microsecond. A source whose clock ran at a genuinely
/// different rate would need this minimum to decay, or a fit of rate as well as
/// offset.
///
/// # When not to use it
///
/// Only when the device's timestamp is *finer* than the jitter it replaces.
/// SDL2 is the counter-example: it stamps events with `SDL_GetTicks()`, in whole
/// milliseconds, while mouse motion arrives every 1-8 ms. Feeding that through
/// here would quantise `dt` to the millisecond and make the scratch velocity
/// worse than simply reading `Instant::now()` on the event loop, which is why
/// [`crate::sdl_input`] does not use this. ALSA and CoreMIDI stamp in
/// microseconds, at the driver, which is what makes it worth doing there.
#[derive(Debug, Default)]
pub struct ExternalClock {
    /// The device's reading for the first event, i.e. the point `origin` is the
    /// local time of. Everything is measured as an elapsed time from here, so a
    /// device clock counting from boot cannot overflow anything.
    base: Option<Duration>,
    /// Smallest - i.e. best - estimate so far of the local instant of `base`.
    origin: Option<Instant>,
}

impl ExternalClock {
    /// Places one event on the local timeline.
    ///
    /// `device_time` is the source's own clock reading. The unit and the origin
    /// are the caller's business - microseconds from midir, milliseconds from
    /// SDL, sample frames from JACK - as long as it satisfies the assumptions
    /// above. `arrived` is when we noticed the event.
    ///
    /// The returned instant is never later than `arrived`: an event cannot be
    /// stamped after the moment we saw it.
    pub fn stamp(&mut self, device_time: Duration, arrived: Instant) -> Instant {
        let base = *self.base.get_or_insert(device_time);
        let elapsed = device_time.saturating_sub(base);

        // Only fails if the platform cannot represent an instant that far back,
        // which would need a device clock older than the process itself.
        let candidate = arrived.checked_sub(elapsed).unwrap_or(arrived);

        let origin = match self.origin {
            // This event got through with less delay than anything before it, so
            // it is a tighter bound on where the timeline really starts.
            Some(previous) if candidate < previous => {
                log::debug!(
                    "external clock origin pulled back by {:?}",
                    previous.duration_since(candidate)
                );
                candidate
            }
            Some(previous) => previous,
            None => candidate,
        };
        self.origin = Some(origin);

        origin + elapsed
    }

    /// The current estimate of the local instant the external clock's first
    /// observed reading corresponds to, once there has been an event to derive
    /// it from. Only useful for watching it settle.
    pub fn origin(&self) -> Option<Instant> {
        self.origin
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replays events one device-millisecond apart, each noticed `delay` after
    /// it really happened, and reports how far each recovered stamp is from the
    /// truth, in microseconds.
    fn errors_micros(delays_micros: &[u64]) -> Vec<i64> {
        let base = Instant::now();
        let mut clock = ExternalClock::default();

        delays_micros
            .iter()
            .enumerate()
            .map(|(i, delay)| {
                let real = base + Duration::from_micros(i as u64 * 1000);
                let noticed = real + Duration::from_micros(*delay);
                // an arbitrary device origin, as midir warns it may be
                let device_time = Duration::from_micros(987_654_321 + i as u64 * 1000);

                let stamped = clock.stamp(device_time, noticed);
                assert!(
                    stamped <= noticed,
                    "an event cannot be stamped after we saw it"
                );

                stamped.duration_since(base).as_micros() as i64 - i as i64 * 1000
            })
            .collect()
    }

    #[test]
    fn origin_converges_on_the_least_delayed_event() {
        // The first event is the worst one, which is what stops a single reading
        // from being good enough.
        let errors = errors_micros(&[5000, 60, 55, 50, 52, 51, 49, 50]);

        // Nothing to compare it against yet, so it carries its whole delay.
        assert_eq!(errors[0], 5000);
        // One better sample is enough to collapse it.
        assert!(errors[1] <= 60, "{errors:?}");
        // From then on the error is the smallest delay seen so far.
        assert!(errors.last().is_some_and(|e| *e <= 50), "{errors:?}");
    }

    #[test]
    fn a_late_observation_does_not_move_the_timeline() {
        // 500us and 3000us hiccups in the middle: reading the clock ourselves
        // would put these in the stamps, and from there in the scratch velocity.
        let errors = errors_micros(&[60, 50, 500, 51, 3000, 49]);

        assert!(
            errors[2] <= 60 && errors[4] <= 60,
            "late observations leaked into the stamps: {errors:?}"
        );
    }

    #[test]
    fn a_stalled_device_clock_does_not_go_backwards() {
        let base = Instant::now();
        let mut clock = ExternalClock::default();

        let first = clock.stamp(Duration::from_micros(1_000_000), base);
        // Same device reading again - two events inside one driver tick.
        let second = clock.stamp(
            Duration::from_micros(1_000_000),
            base + Duration::from_micros(300),
        );

        assert_eq!(first, second);
    }

    #[test]
    fn the_device_clock_unit_is_the_callers_business() {
        // The same run described in two units has to produce the same timeline;
        // this is the whole reason the argument is a Duration.
        let replay = |scale: fn(u64) -> Duration| {
            let base = Instant::now();
            let mut clock = ExternalClock::default();
            [0u64, 3, 4, 9, 11]
                .iter()
                .map(|tick| {
                    let noticed = base + scale(*tick) + Duration::from_micros(70);
                    let stamped = clock.stamp(scale(1_000 + tick), noticed);
                    stamped.duration_since(base)
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            replay(Duration::from_millis),
            replay(|t| Duration::from_micros(t * 1000))
        );
    }
}
