use std::time::Instant;

use crate::deck_thread::DeckId;

/// Anything an input source can produce.
///
/// Input sources (the SDL window today; a MIDI controller and the TUI later)
/// speak only this vocabulary, so the app loop is the single place that knows
/// how a gesture becomes an action.
#[derive(Debug)]
pub enum InputEvent {
    /// Addressed at one deck. How the deck is chosen is the source's business:
    /// the SDL source tags everything with its currently active deck, whereas a
    /// MIDI controller names the deck in the message itself.
    Deck(DeckId, DeckEvent),
    /// Addressed at the app as a whole.
    App(AppEvent),
}

/// A command that belongs to no particular deck.
#[derive(Debug)]
pub enum AppEvent {
    /// A track was handed to the app (drag & drop). It is *staged*, not loaded:
    /// nothing about this event says which deck it will end up on.
    PrepareTrack(String),
    /// Shut the app down.
    Quit,
}

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
    /// Put the track staged in the record tray on this deck.
    CommitStaged,
    PlayheadReset,
    PlayheadFF,
    PlayheadRewind,
}
