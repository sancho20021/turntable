//! The localdeck library, as a source of track files and titles for scanned
//! cards.
//!
//! The only place that touches `localdeck_storage`.

use localdeck_storage::{
    error::StorageError,
    operations::Storage,
    track::{TrackId, TrackMetadata},
};

use crate::{
    card_reader::{CardResolver, ResolveError},
    record::{TrackMeta, TrackRef},
};

pub struct Library {
    storage: Storage,
}

impl Library {
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }

    /// A title is only ever displayed, so a library that cannot produce one
    /// still plays the track.
    fn title_of(&mut self, track_id: TrackId) -> Option<TrackMeta> {
        match self.storage.get_track_metadata(track_id) {
            Ok(meta) => meta.map(|TrackMetadata { artist, title, .. }| TrackMeta { artist, title }),
            Err(e) => {
                log::warn!("cannot read the title of track {track_id}: {e}");
                None
            }
        }
    }
}

impl CardResolver for Library {
    fn resolve(&mut self, card_id: &str) -> Result<TrackRef, ResolveError> {
        // Two lookups with two meanings. A card the library has never heard of is
        // routine - anything with a QR code on it can end up in front of the gun -
        // whereas a card whose track has no playable file is a library that needs
        // attention.
        let track_id = self
            .storage
            .resolve_track(card_id.to_string())
            .map_err(|_| ResolveError::Unknown)?;

        let (_, path, _) = self
            .storage
            .find_track_file(track_id)
            .map_err(|e| ResolveError::Failed(describe(e)))?;

        Ok(TrackRef {
            path: path.to_string_lossy().into_owned(),
            meta: self.title_of(track_id),
        })
    }
}

/// `StorageError`'s own text names internals a DJ reading a status bar cannot act
/// on.
fn describe(error: StorageError) -> String {
    match error {
        StorageError::TrackNotFound(_) => "no file recorded for this track".to_string(),
        StorageError::InvalidTrackFile { extra, .. } => {
            format!("the track's file is missing or its drive is unplugged ({extra})")
        }
        other => other.to_string(),
    }
}
