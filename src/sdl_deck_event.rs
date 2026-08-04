use std::time::Instant;

use sdl2::{event::Event, keyboard::Keycode, mouse::MouseButton};

use crate::deck_event::{self, DeckEvent};

pub fn to_deck_event(event: Event, timestamp: Instant) -> Option<DeckEvent> {
    let event = match event {
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

        Event::KeyDown {
            keycode, keymod, ..
        } => {
            if let Some(key) = keycode {
                let is_shift = keymod
                    .intersects(sdl2::keyboard::Mod::LSHIFTMOD | sdl2::keyboard::Mod::RSHIFTMOD);

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
                    _ => None,
                }
            } else {
                None
            }
        }
        Event::DropFile { filename, .. } => Some(deck_event::Event::LoadTrack(filename)),
        _ => None,
    }?;
    Some(DeckEvent { event, timestamp })
}
