//! The PipeWire nodes turntable can record from, as `pw-dump` reports them.
//!
//! This is the one list of what turntable records: `list-devices` prints it,
//! the startup check looks the recording source up in it, and every recording
//! starts from a node in it.

use std::process::Command;

use anyhow::Context;

/// What a recording is taken from. A sink is recorded through its monitor,
/// which carries what it is playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Source,
    Sink,
}

/// A PipeWire node `pw-record` can record from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordableNode {
    pub name: String,
    pub description: String,
    pub kind: NodeKind,
}

/// Every node that can be recorded from.
pub fn recordable_nodes() -> anyhow::Result<Vec<RecordableNode>> {
    let dump = Command::new("pw-dump")
        .output()
        .context("Cannot run pw-dump")?;
    if !dump.status.success() {
        anyhow::bail!(
            "pw-dump cannot reach PipeWire: {}",
            String::from_utf8_lossy(&dump.stderr).trim()
        );
    }
    parse_recordable(&String::from_utf8_lossy(&dump.stdout)).context("Cannot read pw-dump output")
}

/// The `Audio/Source` and `Audio/Sink` nodes in `pw-dump` output. `pw-record`
/// gets `target not found` for `Audio/Source/Virtual`, so that class is left out.
///
/// While the graph changes, `pw-dump` prints more arrays after the first: each
/// object replaces the one with its id, and `"info": null` removes it. Ids are
/// reused, so the arrays are applied in order.
fn parse_recordable(dump: &str) -> serde_json::Result<Vec<RecordableNode>> {
    let mut objects: Vec<serde_json::Value> = Vec::new();
    for batch in serde_json::Deserializer::from_str(dump).into_iter::<Vec<serde_json::Value>>() {
        for object in batch? {
            objects.retain(|known| known["id"] != object["id"]);
            if !object["info"].is_null() {
                objects.push(object);
            }
        }
    }

    Ok(objects
        .iter()
        .filter(|object| object["type"] == "PipeWire:Interface:Node")
        .filter_map(|object| {
            let props = &object["info"]["props"];
            let kind = match props["media.class"].as_str()? {
                "Audio/Source" => NodeKind::Source,
                "Audio/Sink" => NodeKind::Sink,
                _ => return None,
            };
            let name = props["node.name"].as_str()?.to_string();
            let description = props["node.description"]
                .as_str()
                .unwrap_or(&name)
                .to_string();
            Some(RecordableNode {
                name,
                description,
                kind,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `pw-dump` prints when a sink goes away and a new source takes its
    /// id while it runs.
    #[test]
    fn later_arrays_update_the_dump() {
        let dump = r#"[
            { "id": 40, "type": "PipeWire:Interface:Node", "info": { "props":
                { "node.name": "alsa_input.mic", "node.description": "Mic", "media.class": "Audio/Source" } } },
            { "id": 41, "type": "PipeWire:Interface:Node", "info": { "props":
                { "node.name": "alsa_output.speakers", "node.description": "Speakers", "media.class": "Audio/Sink" } } }
        ]
        [ { "id": 41, "info": null } ]
        [ { "id": 41, "type": "PipeWire:Interface:Node", "info": { "props":
                { "node.name": "alsa_input.usb", "node.description": "USB", "media.class": "Audio/Source" } } } ]"#;
        let names: Vec<_> = parse_recordable(dump)
            .expect("valid JSON")
            .into_iter()
            .map(|node| node.name)
            .collect();
        assert_eq!(names, ["alsa_input.mic", "alsa_input.usb"]);
    }

    #[test]
    fn only_sources_and_sinks_are_recordable() {
        let dump = r#"[
            { "id": 1, "type": "PipeWire:Interface:Node", "info": { "props":
                { "node.name": "alsa_input.mic", "node.description": "Mic", "media.class": "Audio/Source" } } },
            { "id": 2, "type": "PipeWire:Interface:Node", "info": { "props":
                { "node.name": "alsa_output.speakers", "node.description": "Speakers", "media.class": "Audio/Sink" } } },
            { "id": 3, "type": "PipeWire:Interface:Node", "info": { "props":
                { "node.name": "virtual-mic", "media.class": "Audio/Source/Virtual" } } },
            { "id": 4, "type": "PipeWire:Interface:Node", "info": { "props":
                { "node.name": "turntable-recorder", "media.class": "Stream/Input/Audio" } } },
            { "id": 5, "type": "PipeWire:Interface:Node", "info": { "props":
                { "node.name": "Midi-Bridge", "media.class": "Midi/Bridge" } } },
            { "id": 6, "type": "PipeWire:Interface:Link", "info": { "props":
                { "node.name": "not-a-node", "media.class": "Audio/Source" } } },
            { "id": 7, "type": "PipeWire:Interface:Node", "info": null }
        ]"#;
        assert_eq!(
            parse_recordable(dump).expect("valid JSON"),
            vec![
                RecordableNode {
                    name: "alsa_input.mic".into(),
                    description: "Mic".into(),
                    kind: NodeKind::Source,
                },
                RecordableNode {
                    name: "alsa_output.speakers".into(),
                    description: "Speakers".into(),
                    kind: NodeKind::Sink,
                },
            ]
        );
    }
}
