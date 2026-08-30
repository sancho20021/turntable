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
    deck_controller::AppStatus,
    deck_event::{self, DeckEvent, Direction},
    deck_thread::DeckId,
};

pub struct DeckEventMapper<const DECKS: usize> {
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

            Event::DropFile { filename, .. } => Some(deck_event::Event::LoadTrack(filename)),
            _ => None,
        }?;

        Some((
            self.active_deck.load(Ordering::Relaxed),
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
