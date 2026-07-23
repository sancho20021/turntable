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

        Event::KeyDown { keycode, .. } => {
            if let Some(key) = keycode {
                match key {
                    Keycode::R => Some(DeckEvent::KeyReset),
                    Keycode::Up => Some(DeckEvent::KeyUp),
                    Keycode::Down => Some(DeckEvent::KeyDown),
                    Keycode::Space => Some(DeckEvent::StartStop),
                    _ => None,
                }
            } else {
                None
            }
        }
        _ => None,
    }
}
