use sdl2::{event::Event, keyboard::Keycode, mouse::MouseButton};

use crate::deck_event::DeckEvent;

pub fn to_deck_event(event: Event) -> Option<DeckEvent> {
    match event {
        Event::MouseMotion { x, .. } => Some(DeckEvent::MouseMotion(x)),

        Event::MouseButtonDown {
            mouse_btn: MouseButton::Left,
            x,
            ..
        } => Some(DeckEvent::MouseDown(x)),

        Event::MouseButtonUp {
            mouse_btn: MouseButton::Left,
            x,
            ..
        } => Some(DeckEvent::MouseUp(x)),

        Event::KeyDown {
            keycode, keymod, ..
        } => {
            if let Some(key) = keycode {
                let is_shift = keymod
                    .intersects(sdl2::keyboard::Mod::LSHIFTMOD | sdl2::keyboard::Mod::RSHIFTMOD);

                match key {
                    Keycode::R => Some(DeckEvent::ResetPitch),
                    Keycode::Up => Some(DeckEvent::PitchUp),
                    Keycode::Down => Some(DeckEvent::PitchDown),
                    Keycode::Space => Some(DeckEvent::StartStop),
                    Keycode::Right => Some(DeckEvent::PlayheadFF),
                    Keycode::Left => {
                        if is_shift {
                            Some(DeckEvent::PlayheadReset)
                        } else {
                            Some(DeckEvent::PlayheadRewind)
                        }
                    }
                    _ => None,
                }
            } else {
                None
            }
        }
        Event::DropFile { filename, .. } => Some(DeckEvent::LoadTrack(filename)),
        _ => None,
    }
}
