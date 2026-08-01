#[derive(Debug)]
pub enum DeckEvent {
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
