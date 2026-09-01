//! Input source: a MIDI controller.
//!
//! Three layers, so it is clear where the device-specific part starts:
//!
//! * [`message`] raw bytes           -> generic MIDI messages
//! * [`flx4`]    generic MIDI message -> named FLX4 control -> deck command
//! * this module: ports, the connection, and the callback that glues them
//!
//! The callback runs on a driver thread owned by midir, so it does the least
//! possible work: parse, decode, map, hand over. It never blocks, because a
//! stalled MIDI callback would back up the driver - and it does not have to,
//! since a scratch position is absolute and a dropped one is corrected by the
//! next message rather than accumulating error.

pub mod flx4;
pub mod message;

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use crossbeam::channel::Sender;
use midir::{Ignore, MidiInput, MidiInputConnection};

use crate::{clock_sync::ExternalClock, input_event::InputEvent, midi::message::MidiMessage};

/// Names a controller port matched by substring, or by index if the query
/// parses as a number. Without a query, the first port that looks like a DDJ.
fn find_port(input: &MidiInput, query: Option<&str>) -> anyhow::Result<midir::MidiInputPort> {
    let ports = input.ports();
    if ports.is_empty() {
        bail!("No MIDI input ports found - is the controller plugged in and powered?");
    }

    let port = match query.map(str::trim).filter(|q| !q.is_empty()) {
        Some(query) => match query.parse::<usize>() {
            Ok(index) => ports.get(index).cloned().with_context(|| {
                format!("No MIDI port with index {index} (have {})", ports.len())
            })?,
            Err(_) => {
                let needle = query.to_lowercase();
                ports
                    .iter()
                    .find(|port| {
                        input
                            .port_name(port)
                            .is_ok_and(|name| name.to_lowercase().contains(&needle))
                    })
                    .cloned()
                    .with_context(|| format!("No MIDI port matching {query:?}"))?
            }
        },
        None => ports
            .iter()
            .find(|port| {
                input.port_name(port).is_ok_and(|name| {
                    let name = name.to_lowercase();
                    name.contains("ddj") || name.contains("flx")
                })
            })
            .cloned()
            .unwrap_or_else(|| ports[0].clone()),
    };

    Ok(port)
}

/// Logs every MIDI input port, for `turntable list-midi`.
pub fn list_ports() -> anyhow::Result<Vec<String>> {
    let input = MidiInput::new("turntable-list")?;
    input
        .ports()
        .iter()
        .map(|port| input.port_name(port).context("Cannot read MIDI port name"))
        .collect()
}

/// Opens the controller and starts feeding `events`.
///
/// The returned connection must stay alive for as long as input is wanted:
/// dropping it closes the port and the callback stops running.
pub fn start(
    port_query: Option<&str>,
    pitch_range: f64,
    events: Sender<InputEvent>,
) -> anyhow::Result<MidiInputConnection<()>> {
    let mut input = MidiInput::new("turntable")?;
    // we want everything, not midir's default filtered subset
    input.ignore(Ignore::None);

    let port = find_port(&input, port_query)?;
    let port_name = input.port_name(&port)?;

    let mut decoder = flx4::Decoder::new();
    let mut clock = ExternalClock::default();

    let connection = input
        .connect(
            &port,
            "turntable-midi-in",
            move |timestamp_micros, bytes, ()| {
                // midir's stamp beats reading the clock here: on ALSA and
                // CoreMIDI the driver applies it when the bytes actually
                // arrive, so it does not carry the jitter of getting this
                // thread scheduled. Scratch speed is derived from the gaps
                // between these, so that jitter would land in the velocity.
                let timestamp =
                    clock.stamp(Duration::from_micros(timestamp_micros), Instant::now());

                let Some(message) = MidiMessage::parse(bytes) else {
                    log::warn!("Not a valid MIDI message: {bytes:02X?}");
                    return;
                };

                let Some(event) = decoder.decode(&message) else {
                    // Unmapped control: still useful, this is how a new button
                    // gets found. Add it to the table in `flx4`.
                    log::debug!("Unmapped control: {}", message.describe());
                    return;
                };

                let Some(input_event) = flx4::to_input_event(event, pitch_range, timestamp) else {
                    return;
                };

                // Never block a driver callback. A lost scratch position is
                // corrected by the next message, since it is absolute.
                if events.try_send(input_event).is_err() {
                    log::warn!("Input queue full, dropped a MIDI event");
                }
            },
            (),
        )
        .map_err(|e| anyhow::anyhow!("Cannot open MIDI port {port_name:?}: {e}"))?;

    log::info!(
        "Listening to MIDI on {port_name:?}, pitch range +/-{:.0}%",
        pitch_range * 100.0
    );
    Ok(connection)
}
