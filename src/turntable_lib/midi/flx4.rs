//! Layer 2: generic MIDI messages -> DDJ-FLX4 controls -> deck commands.
//!
//! Adapted from the midi-test crate. The FLX4 encodes *which* physical control
//! moved in the (channel, note/cc) pair, and *what happened* in the value byte.
//! The channel says which deck:
//!
//!   ch 1 (0x0)  deck 1        ch 2 (0x1)  deck 2        ch 7 (0x6)  mixer
//!
//! So a deck is named by every message, and nothing here needs the notion of an
//! "active deck" that the keyboard source carries.
//!
//! SHIFT is a genuine modifier, not part of the channel: pressing it sends its
//! own note-on/off (note 0x3F) and it is up to us to remember it is down.
//! Controls that do something different while SHIFT is held say so by using a
//! *different note or CC number* on the same channel - e.g. the deck-2 jog wheel
//! sends CC 0x22 normally but CC 0x29 while shifted. So [`Decoder`] keeps the
//! shift state and tags every event with it.
//!
//! (Some controllers in this family also mirror shifted buttons onto channel
//! + 4; `scope_of` accepts those channels too, harmless if unused.)
//!
//! Anything not in the table is logged raw by [`super::start`], which is how the
//! shifted jog numbers above were found - keep extending it that way.

use std::{fmt, time::Instant};

use crate::{
    deck_controller::DeckId,
    input_event::{DeckCommand, DeckEvent, Direction, InputEvent},
    midi::message::MidiMessage,
};

/// Ticks one full revolution of the jog wheel reports, measured on a DDJ-FLX4.
/// Only used to document [`crate::input_profile::InputProfile::jog_wheel`].
pub const JOG_TICKS_PER_REVOLUTION: i64 = 720;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deck {
    One,
    Two,
}

impl Deck {
    /// 0 or 1. Doubles as the index into this decoder's per-deck state and as
    /// the [`DeckId`] the rest of the app knows the deck by.
    fn index(self) -> DeckId {
        match self {
            Deck::One => 0,
            Deck::Two => 1,
        }
    }
}

/// A button we have a binding for. Every other note number on a known channel
/// decodes to [`Button::Unmapped`] rather than being dropped, so pressing an
/// unbound button still shows up in the log with its note number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    PlayPause,
    Cue,
    Shift,
    /// LOAD lives on the shared mixer channel and names its target deck in the
    /// note number, so it carries the deck itself.
    Load(Deck),
    Unmapped(u8),
}

impl Button {
    fn from_note(deck: Option<Deck>, note: u8) -> Self {
        match (deck, note) {
            // Per-deck transport buttons (channels 1/2, or 5/6 with shift).
            (Some(_), 0x0B) => Button::PlayPause,
            (Some(_), 0x0C) => Button::Cue,
            (Some(_), 0x3F) => Button::Shift,
            // LOAD buttons live on the shared mixer channel, one note each.
            (None, 0x46) => Button::Load(Deck::One),
            (None, 0x47) => Button::Load(Deck::Two),
            _ => Button::Unmapped(note),
        }
    }
}

impl fmt::Display for Button {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Button::PlayPause => f.write_str("PLAY/PAUSE"),
            Button::Cue => f.write_str("CUE"),
            Button::Shift => f.write_str("SHIFT"),
            Button::Load(deck) => write!(f, "LOAD -> deck {}", deck.index() + 1),
            Button::Unmapped(note) => write!(f, "unmapped note 0x{note:02X}"),
        }
    }
}

/// What the channel nibble told us: which deck (if any) and whether SHIFT was
/// held when the control was touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Scope {
    deck: Option<Deck>,
    shifted: bool,
}

fn scope_of(channel: u8) -> Option<Scope> {
    let (deck, shifted) = match channel {
        0x0 => (Some(Deck::One), false),
        0x1 => (Some(Deck::Two), false),
        0x4 => (Some(Deck::One), true),
        0x5 => (Some(Deck::Two), true),
        0x6 => (None, false),
        0xA => (None, true),
        _ => return None,
    };
    Some(Scope { deck, shifted })
}

/// Which way a jog wheel movement was reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JogMode {
    /// Top plate is being touched: "scratch" ticks (CC 0x22).
    Scratch,
    /// Wheel turned by the side / untouched: pitch-bend ticks (CC 0x21).
    Bend,
    /// SHIFT held while turning: fast track search (CC 0x29).
    Search,
}

/// A decoded, human-meaningful controller event.
#[derive(Debug, Clone)]
pub enum Event {
    /// PLAY / CUE / LOAD / SHIFT: a plain press or release.
    Button {
        deck: Option<Deck>,
        button: Button,
        shifted: bool,
        pressed: bool,
    },
    /// Capacitive touch sensor on top of the jog wheel. Carries the running tick
    /// total so a scratch can be anchored at the moment the hand lands.
    JogTouch {
        deck: Deck,
        shifted: bool,
        touching: bool,
        total: i64,
    },
    /// Relative rotation. `ticks` is signed: + is forward, - is backward.
    JogTurn {
        deck: Deck,
        shifted: bool,
        mode: JogMode,
        ticks: i16,
        /// Running sum of ticks for this wheel since the app started. This is
        /// the wheel's absolute position, and what the scratch engine consumes.
        total: i64,
    },
    /// Tempo (pitch) fader: a 14-bit absolute position.
    Tempo {
        deck: Deck,
        raw: u16,
        /// Position as -1.0 .. +1.0, with 0.0 at the centre detent.
        /// (raw 0 = -1.0, raw 8192 = 0.0, raw 16383 = +1.0.)
        offset: f32,
    },
    /// The MSB half of the tempo fader, kept only so it can be logged.
    TempoPartial { deck: Deck, msb: u8 },
}

/// Stateful decoder: needed because a couple of controls span more than one MIDI
/// message (14-bit fader) or are only interesting relative to history (jog
/// wheels, shift state).
#[derive(Default)]
pub struct Decoder {
    /// Pending high 7 bits of each deck's tempo fader.
    tempo_msb: [Option<u8>; 2],
    /// Accumulated jog ticks per deck: the wheel's absolute position.
    jog_total: [i64; 2],
    /// SHIFT currently held, per deck.
    shift_held: [bool; 2],
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while that deck's SHIFT key is down. Not needed for decoding
    /// (shifted presses arrive on their own channel) but handy for status.
    pub fn shift_held(&self, deck: Deck) -> bool {
        self.shift_held[deck.index()]
    }

    /// Try to turn one MIDI message into an [`Event`]. `None` means the message
    /// is not a control at all - a channel that is not a deck or the mixer, a CC
    /// we have no mapping for, or a [`MidiMessage::Other`]. An unrecognised
    /// *note* is not dropped here: it comes back as [`Button::Unmapped`].
    pub fn decode(&mut self, msg: &MidiMessage) -> Option<Event> {
        match *msg {
            MidiMessage::NoteOn { channel, note, .. } => self.note(channel, note, true),
            MidiMessage::NoteOff { channel, note } => self.note(channel, note, false),
            MidiMessage::ControlChange {
                channel,
                controller,
                value,
            } => self.cc(channel, controller, value),
            _ => None,
        }
    }

    fn note(&mut self, channel: u8, note: u8, pressed: bool) -> Option<Event> {
        let Scope { deck, shifted } = scope_of(channel)?;
        let shifted = shifted || deck.is_some_and(|d| self.shift_held(d));

        // Jog wheel touch sensor is a note, not a CC. 0x67 is the same sensor
        // reported while SHIFT is held.
        if note == 0x36 || note == 0x67 {
            let deck = deck?;
            return Some(Event::JogTouch {
                deck,
                shifted,
                touching: pressed,
                total: self.jog_total[deck.index()],
            });
        }

        let button = Button::from_note(deck, note);

        // The SHIFT key press itself: remember it, and don't label it
        // "SHIFT + SHIFT".
        let shifted = if button == Button::Shift {
            if let Some(d) = deck {
                self.shift_held[d.index()] = pressed;
            }
            false
        } else {
            shifted
        };

        Some(Event::Button {
            deck,
            button,
            shifted,
            pressed,
        })
    }

    fn cc(&mut self, channel: u8, controller: u8, value: u8) -> Option<Event> {
        let Scope { deck, shifted } = scope_of(channel)?;
        let shifted = shifted || deck.is_some_and(|d| self.shift_held(d));

        match controller {
            // Jog wheels send *relative* 7-bit ticks centred on 0x40:
            // 0x41.. = forward, ..0x3F = backward, and no message at all while
            // the wheel is still. We accumulate them, so what leaves here is an
            // absolute position that a dropped message cannot corrupt.
            0x21 | 0x22 | 0x29 => {
                let deck = deck?;
                let ticks = i16::from(value) - 0x40;
                let total = &mut self.jog_total[deck.index()];
                *total += i64::from(ticks);
                Some(Event::JogTurn {
                    deck,
                    shifted,
                    mode: match controller {
                        0x22 => JogMode::Scratch,
                        0x21 => JogMode::Bend,
                        // 0x29: only ever seen with SHIFT held (= search).
                        _ => JogMode::Search,
                    },
                    ticks,
                    total: *total,
                })
            }
            // Tempo fader is 14-bit: CC 0x00 carries the high 7 bits, then CC
            // 0x20 (= 0x00 + 32, the standard "LSB of CC 0" slot) the low 7
            // bits. We buffer the MSB and emit once the LSB lands.
            0x00 => {
                let deck = deck?;
                self.tempo_msb[deck.index()] = Some(value);
                Some(Event::TempoPartial { deck, msb: value })
            }
            0x20 => {
                let deck = deck?;
                let msb = self.tempo_msb[deck.index()].take()?;
                let raw = (u16::from(msb) << 7) | u16::from(value);
                Some(Event::Tempo {
                    deck,
                    raw,
                    // The fader is bipolar: rescale 0..16383 to -1..+1 so the
                    // centre detent reads as zero instead of 50%.
                    offset: f32::from(raw) / 16383.0 * 2.0 - 1.0,
                })
            }
            _ => None,
        }
    }
}

/// Turns one controller event into a deck command.
///
/// `pitch_range` is how far the tempo fader bends playback speed, as a fraction
/// (0.08 = the usual +/-8%).
pub fn to_input_event(event: Event, pitch_range: f64, timestamp: Instant) -> Option<InputEvent> {
    let (deck_id, command) = match event {
        // The touch sensor is the scratch gate, and the tick total it carries is
        // the position the scratch is anchored on.
        Event::JogTouch {
            deck,
            touching,
            total,
            ..
        } => (
            deck.index(),
            if touching {
                DeckCommand::ScratchStart(total)
            } else {
                DeckCommand::ScratchEnd
            },
        ),

        Event::JogTurn {
            deck,
            mode,
            ticks,
            total,
            ..
        } => {
            let command = match mode {
                // Absolute wheel position, straight through.
                JogMode::Scratch => DeckCommand::ScratchMove(total),

                // Turned by the side without touching the top: pitch bend. One
                // nudge per message, like one detent of a mouse wheel.
                JogMode::Bend => DeckCommand::Nudge(if ticks >= 0 {
                    Direction::Forward
                } else {
                    Direction::Backward
                }),

                // Would need a playhead jump proportional to the ticks; reusing
                // the fixed fast-forward would skip minutes per flick.
                JogMode::Search => {
                    log::debug!("SHIFT + jog (track search) is not supported yet");
                    return None;
                }
            };
            (deck.index(), command)
        }

        // Deck 1 is nominal speed at the centre detent, faster above it.
        Event::Tempo { deck, offset, .. } => (
            deck.index(),
            DeckCommand::SetPitch(1.0 + f64::from(offset) * pitch_range),
        ),

        Event::Button {
            deck,
            button,
            pressed: true,
            ..
        } => match button {
            Button::PlayPause => (deck?.index(), DeckCommand::StartStop),
            Button::Cue => (deck?.index(), DeckCommand::PlayheadReset),
            // The LOAD buttons name their target deck, so they are the only
            // controls that reach across from the mixer channel to a deck.
            Button::Load(target) => (target.index(), DeckCommand::LoadRecord),
            // SHIFT is a modifier the decoder already tracked.
            Button::Shift => return None,
            // Not bound to anything - logged here because the decoder no longer
            // drops it, which is how a new binding gets found.
            Button::Unmapped(note) => {
                log::debug!("Unmapped button on note 0x{note:02X}");
                return None;
            }
        },

        // releases and the buffered half of a fader move do nothing
        Event::Button { pressed: false, .. } | Event::TempoPartial { .. } => return None,
    };

    Some(InputEvent::Deck(deck_id, DeckEvent { command, timestamp }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::midi::message::MidiMessage;

    /// Feed the two halves of a 14-bit fader move and check the result.
    fn tempo(raw: u16) -> Event {
        let mut d = Decoder::new();
        let msb = (raw >> 7) as u8;
        let lsb = (raw & 0x7F) as u8;
        d.decode(&MidiMessage::ControlChange {
            channel: 1,
            controller: 0x00,
            value: msb,
        });
        d.decode(&MidiMessage::ControlChange {
            channel: 1,
            controller: 0x20,
            value: lsb,
        })
        .expect("tempo event")
    }

    fn jog(decoder: &mut Decoder, channel: u8, value: u8) -> Event {
        decoder
            .decode(&MidiMessage::ControlChange {
                channel,
                controller: 0x22,
                value,
            })
            .expect("jog event")
    }

    #[test]
    fn tempo_fader_is_bipolar() {
        for (raw, want) in [(0u16, -100.0f32), (8192, 0.0), (16383, 100.0)] {
            let Event::Tempo { offset, .. } = tempo(raw) else {
                panic!("not a tempo event");
            };
            assert!(
                (offset * 100.0 - want).abs() < 0.05,
                "raw {raw} -> {:.2}%, wanted {want}%",
                offset * 100.0
            );
        }
    }

    #[test]
    fn shift_is_tracked_across_messages() {
        let mut d = Decoder::new();
        d.decode(&MidiMessage::NoteOn {
            channel: 1,
            note: 0x3F,
            velocity: 127,
        });
        let ev = d
            .decode(&MidiMessage::NoteOn {
                channel: 1,
                note: 0x0B,
                velocity: 127,
            })
            .expect("play event");
        let Event::Button {
            shifted, button, ..
        } = ev
        else {
            panic!("not a button");
        };
        assert!(shifted, "{button} should be flagged as shifted");
    }

    #[test]
    fn jog_ticks_accumulate_into_an_absolute_position() {
        let mut d = Decoder::new();

        // +3, then -1: 0x40 is the centre, so 0x43 is +3 and 0x3F is -1
        assert!(matches!(
            jog(&mut d, 0, 0x43),
            Event::JogTurn { total: 3, .. }
        ));
        assert!(matches!(
            jog(&mut d, 0, 0x3F),
            Event::JogTurn { total: 2, .. }
        ));

        // the other deck keeps its own position
        assert!(matches!(
            jog(&mut d, 1, 0x41),
            Event::JogTurn { total: 1, .. }
        ));
    }

    #[test]
    fn touch_anchors_on_the_current_position() {
        let mut d = Decoder::new();
        jog(&mut d, 0, 0x45); // +5

        let touch = d
            .decode(&MidiMessage::NoteOn {
                channel: 0,
                note: 0x36,
                velocity: 127,
            })
            .expect("jog touch");

        assert!(matches!(
            touch,
            Event::JogTouch {
                touching: true,
                total: 5,
                ..
            }
        ));
    }

    #[test]
    fn scratch_moves_carry_the_absolute_position() {
        let mut d = Decoder::new();
        jog(&mut d, 0, 0x42); // +2
        let event = jog(&mut d, 0, 0x42); // +2 again

        let Some(InputEvent::Deck(deck_id, deck_event)) =
            to_input_event(event, 0.08, Instant::now())
        else {
            panic!("not a deck event");
        };
        assert_eq!(deck_id, 0);
        assert!(matches!(deck_event.command, DeckCommand::ScratchMove(4)));
    }
}
