//! Input source: the SDL window (mouse and keyboard).
//!
//! Records are dropped on the TUI, not here - see [`crate::ratatui`].

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use sdl2::{
    event::Event,
    keyboard::{Keycode, Mod},
    mouse::MouseButton,
};

use crate::{
    deck_controller::DeckId,
    input_event::{AppEvent, DeckCommand, DeckEvent, Direction, InputEvent},
    notices::Notices,
};

pub struct SdlInputMapper<const DECKS: usize> {
    /// Deck that keyboard and mouse gestures are addressed to, picked with the
    /// number keys. A keyboard concept: a MIDI controller names its deck in
    /// every message and has none, which is why the TUI takes it as an option.
    active_deck: Arc<AtomicUsize>,
    notices: Notices,
}

impl<const DECKS: usize> SdlInputMapper<DECKS> {
    pub fn new(active_deck: Arc<AtomicUsize>, notices: Notices) -> Self {
        Self {
            active_deck,
            notices,
        }
    }

    pub fn to_input_event(&mut self, event: Event, timestamp: Instant) -> Option<InputEvent> {
        let command = match event {
            Event::Quit { .. } => return Some(InputEvent::App(AppEvent::Quit)),

            // The touchpad's input unit is one screen pixel of horizontal travel,
            // see `InputProfile::touchpad`.
            Event::MouseMotion { x, .. } => DeckCommand::ScratchMove(x as i64),

            Event::MouseButtonDown {
                mouse_btn: MouseButton::Left,
                x,
                ..
            } => DeckCommand::ScratchStart(x as i64),

            Event::MouseButtonUp {
                mouse_btn: MouseButton::Left,
                ..
            } => DeckCommand::ScratchEnd,

            Event::MouseWheel { x, .. } => {
                let direction = if x < 0 {
                    Direction::Forward
                } else {
                    Direction::Backward
                };
                DeckCommand::Nudge(direction)
            }

            Event::KeyDown {
                keycode: Some(key),
                keymod,
                ..
            } => self.key_down(key, keymod)?,

            _ => return None,
        };

        Some(InputEvent::Deck(
            self.active_deck.load(Ordering::Relaxed),
            DeckEvent { command, timestamp },
        ))
    }

    /// Keys that are not bound to a deck command switch the active deck instead,
    /// which is why this needs `&mut self` and can return nothing.
    fn key_down(&mut self, key: Keycode, keymod: Mod) -> Option<DeckCommand> {
        let is_shift = keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);

        match key {
            Keycode::R => Some(DeckCommand::ResetPitch),
            Keycode::Up => Some(DeckCommand::PitchUp),
            Keycode::Down => Some(DeckCommand::PitchDown),
            Keycode::Space => Some(DeckCommand::StartStop),
            Keycode::Return | Keycode::KpEnter => Some(DeckCommand::LoadRecord),
            Keycode::Right => Some(DeckCommand::PlayheadFF),
            Keycode::Left => {
                if is_shift {
                    Some(DeckCommand::PlayheadReset)
                } else {
                    Some(DeckCommand::PlayheadRewind)
                }
            }
            k => {
                if let Some(deck_idx) = keycode_to_deck_idx(k) {
                    if deck_idx < DECKS {
                        self.active_deck.store(deck_idx, Ordering::Relaxed);
                        log::info!("Active Deck = {}", deck_idx + 1);
                    } else {
                        self.notices.warn(format!(
                            "deck {} doesn't exist (only {} decks are running)",
                            deck_idx + 1,
                            DECKS
                        ));
                    }
                }
                None
            }
        }
    }
}

fn keycode_to_deck_idx(key: Keycode) -> Option<DeckId> {
    match key {
        Keycode::Num1 | Keycode::Kp1 => Some(0),
        Keycode::Num2 | Keycode::Kp2 => Some(1),
        Keycode::Num3 | Keycode::Kp3 => Some(2),
        Keycode::Num4 | Keycode::Kp4 => Some(3),
        Keycode::Num5 | Keycode::Kp5 => Some(4),
        Keycode::Num6 | Keycode::Kp6 => Some(5),
        Keycode::Num7 | Keycode::Kp7 => Some(6),
        Keycode::Num8 | Keycode::Kp8 => Some(7),
        Keycode::Num9 | Keycode::Kp9 => Some(8),
        _ => None,
    }
}
