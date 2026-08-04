use std::time::Instant;

#[derive(Debug)]
pub struct DeckEvent {
    pub event: Event,
    pub timestamp: Instant,
}

#[derive(Debug)]
pub enum Event {
    MouseMotion(i32),
    MouseDown(i32),
    MouseUp(i32),
    StartStop,
    ResetPitch,
    PitchUp,
    PitchDown,
    LoadTrack(String),
    PlayheadReset,
    PlayheadFF,
    PlayheadRewind,
}
