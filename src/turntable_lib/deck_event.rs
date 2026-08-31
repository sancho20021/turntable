use std::time::Instant;

#[derive(Debug)]
pub struct DeckEvent {
    pub event: Event,
    pub timestamp: Instant,
}

#[derive(Debug)]
pub enum Direction {
    Forward,
    Backward,
}

/// A device-independent deck command.
///
/// The scratch variants carry an absolute **input position** in *input units*,
/// whose physical meaning is defined by the source device and its
/// [`crate::input_profile::InputProfile`] (touchpad pixels, jog wheel ticks).
/// The position only has to be absolute and monotonic in the direction of
/// travel; its origin is irrelevant, since scratching is anchored on the
/// position seen at [`Event::ScratchStart`].
#[derive(Debug)]
pub enum Event {
    /// The user grabbed the platter (mouse down / jog wheel touched).
    ScratchStart(i64),
    /// New input position while the platter is held.
    ScratchMove(i64),
    /// The user let go of the platter (mouse up / hand off the jog wheel).
    ScratchEnd,
    // pitch nudge
    Nudge(Direction),
    StartStop,
    ResetPitch,
    PitchUp,
    PitchDown,
    /// Absolute pitch, 1.0 = nominal speed (tempo fader).
    SetPitch(f64),
    LoadTrack(String),
    PlayheadReset,
    PlayheadFF,
    PlayheadRewind,
}
