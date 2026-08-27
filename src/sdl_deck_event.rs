use std::time::Instant;

use sdl2::{
    event::Event,
    keyboard::{Keycode, Mod},
    mouse::MouseButton,
};

use crate::{
    deck_event::{self, DeckEvent, Direction},
    deck_thread::DeckId,
};

pub struct DeckEventMapper<const DECKS: usize> {
    pub active_deck: usize,
}

impl<const DECKS: usize> DeckEventMapper<DECKS> {
    pub fn new() -> Self {
        Self { active_deck: 0 }
    }

    pub fn to_deck_event(
        &mut self,
        event: Event,
        timestamp: Instant,
    ) -> Option<(DeckId, DeckEvent)> {
        let inner_event = match event {
            Event::MouseMotion { x, .. } => Some(deck_event::Event::MouseMotion(x)),

            Event::MouseButtonDown {
                mouse_btn: MouseButton::Left,
                x,
                ..
            } => Some(deck_event::Event::MouseDown(x)),

            Event::MouseButtonUp {
                mouse_btn: MouseButton::Left,
                x,
                ..
            } => Some(deck_event::Event::MouseUp(x)),

            Event::MouseWheel { x, .. } => {
                let direction = if x < 0 {
                    Direction::Forward
                } else {
                    Direction::Backward
                };
                Some(deck_event::Event::Nudge(direction))
            }

            Event::KeyDown {
                keycode: Some(key),
                keymod,
                ..
            } => {
                let is_shift = keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);

                match key {
                    Keycode::R => Some(deck_event::Event::ResetPitch),
                    Keycode::Up => Some(deck_event::Event::PitchUp),
                    Keycode::Down => Some(deck_event::Event::PitchDown),
                    Keycode::Space => Some(deck_event::Event::StartStop),
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
                                self.active_deck = deck_idx;
                                println!("Active Deck = {}", deck_idx + 1);
                            } else {
                                println!(
                                    "deck {} doesn't exist (only {} decks are running)",
                                    deck_idx + 1,
                                    DECKS
                                );
                            }
                        }
                        None
                    }
                }
            }

            Event::DropFile { filename, .. } => Some(deck_event::Event::LoadTrack(filename)),
            _ => None,
        }?;

        Some((
            self.active_deck,
            DeckEvent {
                event: inner_event,
                timestamp,
            },
        ))
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
