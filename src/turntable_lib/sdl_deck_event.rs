//! Input source: the SDL window (mouse, keyboard, drag & drop).

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
    deck_controller::{AppStatus, DeckId},
    deck_event::{self, AppEvent, DeckEvent, Direction, InputEvent},
};

pub struct DeckEventMapper<const DECKS: usize> {
    /// Deck that keyboard and mouse gestures are addressed to, picked with the
    /// number keys. Owned by this source: it is a keyboard concept, and a MIDI
    /// controller (which names its deck per message) will not have one.
    pub active_deck: Arc<AtomicUsize>,
    app_status: AppStatus,
}

impl<const DECKS: usize> DeckEventMapper<DECKS> {
    pub fn new(app_status: AppStatus) -> Self {
        Self {
            active_deck: Arc::new(AtomicUsize::new(0)),
            app_status,
        }
    }

    pub fn to_input_event(&mut self, event: Event, timestamp: Instant) -> Option<InputEvent> {
        let deck_event = match event {
            Event::Quit { .. } => return Some(InputEvent::App(AppEvent::Quit)),

            // A dropped file goes in the record tray, not on a deck: which deck it
            // ends up on is decided by whoever loads it out of the tray.
            Event::DropFile { filename, .. } => {
                return Some(InputEvent::App(AppEvent::PrepareRecord(filename)));
            }

            // The touchpad's input unit is one screen pixel of horizontal travel,
            // see `InputProfile::touchpad`.
            Event::MouseMotion { x, .. } => deck_event::Event::ScratchMove(x as i64),

            Event::MouseButtonDown {
                mouse_btn: MouseButton::Left,
                x,
                ..
            } => deck_event::Event::ScratchStart(x as i64),

            Event::MouseButtonUp {
                mouse_btn: MouseButton::Left,
                ..
            } => deck_event::Event::ScratchEnd,

            Event::MouseWheel { x, .. } => {
                let direction = if x < 0 {
                    Direction::Forward
                } else {
                    Direction::Backward
                };
                deck_event::Event::Nudge(direction)
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
            DeckEvent {
                event: deck_event,
                timestamp,
            },
        ))
    }

    /// Keys that are not bound to a deck command switch the active deck instead,
    /// which is why this needs `&mut self` and can return nothing.
    fn key_down(&mut self, key: Keycode, keymod: Mod) -> Option<deck_event::Event> {
        let is_shift = keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);

        match key {
            Keycode::R => Some(deck_event::Event::ResetPitch),
            Keycode::Up => Some(deck_event::Event::PitchUp),
            Keycode::Down => Some(deck_event::Event::PitchDown),
            Keycode::Space => Some(deck_event::Event::StartStop),
            Keycode::Return | Keycode::KpEnter => Some(deck_event::Event::LoadRecord),
            Keycode::Right => Some(deck_event::Event::PlayheadFF),
            Keycode::Left => {
                if is_shift {
                    Some(deck_event::Event::PlayheadReset)
                } else {
                    Some(deck_event::Event::PlayheadRewind)
                }
            }
            k => {
                if let Some(deck_idx) = keycode_to_deck_idx(k) {
                    if deck_idx < DECKS {
                        self.active_deck.store(deck_idx, Ordering::Relaxed);
                        log::info!("Active Deck = {}", deck_idx + 1);
                    } else {
                        // todo: change it from status to temporary warning
                        self.app_status.set(format!(
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
