//! Layer 1: raw MIDI bytes -> generic MIDI messages.
//!
//! Nothing in here knows anything about Pioneer/AlphaTheta hardware. This is
//! just the MIDI 1.0 wire format:
//!
//!   [status byte] [data byte] [data byte]?
//!
//! * A status byte always has its top bit set (0x80..=0xFF).
//! * A data byte always has its top bit clear (0x00..=0x7F), i.e. 7 bits,
//!   which is why every MIDI value you see is in the range 0..=127.
//! * For "channel voice" messages the status byte splits into two nibbles:
//!       high nibble = message kind (0x8 note off, 0x9 note on, 0xB CC, ...)
//!       low  nibble = channel 0..=15  (shown as "channel 1..16" in most UIs)
//!
//! A DJ controller only ever speaks two of those kinds: notes for buttons and
//! pads, control changes for everything continuous (faders, knobs, jog wheels).
//! Aftertouch, program change, channel pressure, MIDI pitch bend, SysEx and the
//! system realtime bytes carry no control we could bind, so they collapse into
//! [`MidiMessage::Other`] - kept only so an unexpected one can be logged.

/// A parsed MIDI 1.0 message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiMessage {
    NoteOff {
        channel: u8,
        note: u8,
    },
    NoteOn {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    ControlChange {
        channel: u8,
        controller: u8,
        value: u8,
    },
    /// Anything a DJ controller does not use as a control. Only the status byte
    /// and the payload length are kept, which is all the log needs.
    Other {
        status: u8,
        data_len: usize,
    },
}

impl MidiMessage {
    /// Parse one complete message. `bytes` is what the driver handed us;
    /// midir always delivers whole messages, so we don't have to deal with
    /// running status or partial packets here.
    pub fn parse(bytes: &[u8]) -> Option<MidiMessage> {
        let (&status, data) = bytes.split_first()?;
        if status < 0x80 {
            return None; // not a status byte -> we can't interpret it
        }

        let channel = status & 0x0F;
        let other = MidiMessage::Other {
            status,
            data_len: data.len(),
        };

        // Small helpers so the match arms stay readable.
        let d0 = data.first().copied();
        let d1 = data.get(1).copied();

        Some(match status & 0xF0 {
            0x80 => MidiMessage::NoteOff { channel, note: d0? },
            0x90 => {
                let (note, velocity) = (d0?, d1?);
                // Convention: note-on with velocity 0 means note-off.
                if velocity == 0 {
                    MidiMessage::NoteOff { channel, note }
                } else {
                    MidiMessage::NoteOn {
                        channel,
                        note,
                        velocity,
                    }
                }
            }
            0xB0 => MidiMessage::ControlChange {
                channel,
                controller: d0?,
                value: d1?,
            },
            _ => other,
        })
    }

    /// One-line generic description, e.g. `CC ch1 cc 0x00 val 64`. Only used
    /// when logging a control we have no mapping for, which is how new buttons
    /// get found - see the table in [`super::flx4`].
    pub fn describe(&self) -> String {
        match *self {
            MidiMessage::NoteOn {
                channel,
                note,
                velocity,
            } => format!("NoteOn ch{} note 0x{note:02X} vel {velocity}", channel + 1),
            MidiMessage::NoteOff { channel, note } => {
                format!("NoteOff ch{} note 0x{note:02X}", channel + 1)
            }
            MidiMessage::ControlChange {
                channel,
                controller,
                value,
            } => format!("CC ch{} cc 0x{controller:02X} val {value}", channel + 1),
            MidiMessage::Other { status, data_len } => {
                format!("status 0x{status:02X} with {data_len} data bytes")
            }
        }
    }
}
