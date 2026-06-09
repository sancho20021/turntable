#[derive(Debug, Clone, Copy)]
pub enum DeckEvent {
    MouseMotion(i32),
    MouseDown(i32),
    MouseUp(i32),
    KeyReset,
    KeyUp,
    KeyDown,
}
