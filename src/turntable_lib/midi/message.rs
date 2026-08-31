//! Layer 1: raw MIDI bytes -> generic MIDI messages.
//!
//! Copied from the midi-test crate. Knows nothing about any hardware.
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

/// A parsed MIDI 1.0 message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MidiMessage {
    NoteOff {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    NoteOn {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    /// Polyphonic key pressure (rare on controllers).
    Aftertouch {
        channel: u8,
        note: u8,
        pressure: u8,
    },
    ControlChange {
        channel: u8,
        controller: u8,
        value: u8,
    },
    ProgramChange {
        channel: u8,
        program: u8,
    },
    ChannelPressure {
        channel: u8,
        pressure: u8,
    },
    /// 14-bit value, 0..=16383, centre 8192.
    PitchBend {
        channel: u8,
        value: u16,
    },
    /// 0xF0 .. 0xF7 blob (device inquiry replies, vendor-specific stuff).
    SystemExclusive {
        data: Vec<u8>,
    },
    /// Any other 0xF_ byte (clock, start/stop, active sensing, ...).
    System {
        status: u8,
        data: Vec<u8>,
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
        let kind = status & 0xF0;

        // Small helpers so the match arms stay readable.
        let d0 = data.first().copied();
        let d1 = data.get(1).copied();

        Some(match kind {
            0x80 => MidiMessage::NoteOff {
                channel,
                note: d0?,
                velocity: d1?,
            },
            0x90 => {
                let (note, velocity) = (d0?, d1?);
                // Convention: note-on with velocity 0 means note-off.
                if velocity == 0 {
                    MidiMessage::NoteOff {
                        channel,
                        note,
                        velocity,
                    }
                } else {
                    MidiMessage::NoteOn {
                        channel,
                        note,
                        velocity,
                    }
                }
            }
            0xA0 => MidiMessage::Aftertouch {
                channel,
                note: d0?,
                pressure: d1?,
            },
            0xB0 => MidiMessage::ControlChange {
                channel,
                controller: d0?,
                value: d1?,
            },
            0xC0 => MidiMessage::ProgramChange {
                channel,
                program: d0?,
            },
            0xD0 => MidiMessage::ChannelPressure {
                channel,
                pressure: d0?,
            },
            0xE0 => MidiMessage::PitchBend {
                channel,
                // LSB first, then MSB - 7 bits each.
                value: u16::from(d0?) | (u16::from(d1?) << 7),
            },
            _ if status == 0xF0 => MidiMessage::SystemExclusive {
                data: data.to_vec(),
            },
            _ => MidiMessage::System {
                status,
                data: data.to_vec(),
            },
        })
    }
    /// One-line generic description, e.g. `CC ch1 cc#0 val 64`. Only used when
    /// logging a control we have no mapping for, which is how new buttons get
    /// found - see the table in [`super::flx4`].
    pub fn describe(&self) -> String {
        match *self {
            MidiMessage::NoteOn {
                channel,
                note,
                velocity,
            } => format!("NoteOn ch{} note 0x{note:02X} vel {velocity}", channel + 1),
            MidiMessage::NoteOff {
                channel,
                note,
                velocity,
            } => format!("NoteOff ch{} note 0x{note:02X} vel {velocity}", channel + 1),
            MidiMessage::ControlChange {
                channel,
                controller,
                value,
            } => format!("CC ch{} cc 0x{controller:02X} val {value}", channel + 1),
            MidiMessage::Aftertouch {
                channel,
                note,
                pressure,
            } => format!(
                "Aftertouch ch{} note 0x{note:02X} prs {pressure}",
                channel + 1
            ),
            MidiMessage::ProgramChange { channel, program } => {
                format!("ProgramChange ch{} program {program}", channel + 1)
            }
            MidiMessage::ChannelPressure { channel, pressure } => {
                format!("ChannelPressure ch{} pressure {pressure}", channel + 1)
            }
            MidiMessage::PitchBend { channel, value } => {
                format!("PitchBend ch{} value {value}", channel + 1)
            }
            MidiMessage::SystemExclusive { ref data } => {
                format!("SysEx {} bytes", data.len() + 1)
            }
            MidiMessage::System { status, ref data } => {
                format!("System status 0x{status:02X} {} bytes", data.len())
            }
        }
    }
}
