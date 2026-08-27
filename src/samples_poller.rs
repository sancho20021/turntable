use thiserror::Error;

use crate::{platter_audio_processor::PlatterAudioProcessor, stereo_frame::StereoFrame};

pub struct SamplesPoller<const DECKS: usize, const CHANNELS: usize> {
    buffers: [Vec<StereoFrame>; DECKS],
    audio_processors: [PlatterAudioProcessor; DECKS],
    routing: DeckRouting<DECKS, CHANNELS>,
}

impl<const DECKS: usize, const CHANNELS: usize> SamplesPoller<DECKS, CHANNELS> {
    pub fn new(
        samples_n: usize,
        audio_processors: [PlatterAudioProcessor; DECKS],
        routing: DeckRouting<DECKS, CHANNELS>,
    ) -> Self {
        let buf = vec![StereoFrame::default(); samples_n];
        Self {
            buffers: std::array::from_fn(|_| buf.clone()),
            audio_processors,
            routing,
        }
    }

    pub fn write_frames(&mut self, data: &mut [f32]) {
        let total_samples = data.len() / CHANNELS;

        if data.len() % CHANNELS != 0 || total_samples != self.buffers[0].len() {
            data.fill(0.0);
            return;
        }

        // Zero out output buffer so unmapped/extra channels remain silent
        data.fill(0.0);

        for deck_idx in 0..DECKS {
            self.audio_processors[deck_idx]
                .write_frames(&mut self.buffers[deck_idx][..total_samples]);
        }

        for sample_idx in 0..total_samples {
            let output_offset = sample_idx * CHANNELS;

            for deck_idx in 0..DECKS {
                let pair_idx = self.routing.deck_pairs()[deck_idx];
                let ch_left = pair_idx * 2;
                let ch_right = ch_left + 1;

                let frame = self.buffers[deck_idx][sample_idx];
                data[output_offset + ch_left] = frame.l;
                data[output_offset + ch_right] = frame.r;
            }
        }
    }
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum RoutingError {
    #[error(
        "Stereo pair index {pair} is out of bounds: device only has {max_pairs} stereo pair(s) (indices 0..{max_pairs})"
    )]
    PairOutOfBounds { pair: usize, max_pairs: usize },

    #[error("Duplicate channel assignment: stereo pair {pair} is assigned to multiple decks")]
    DuplicatePair { pair: usize },
}

/// Maps each deck to a target stereo pair index `0 .. (CHANNELS / 2)`.
/// Allows `CHANNELS >= DECKS * 2` so extra output channels can remain unassigned.
#[derive(Debug, Clone)]
pub struct DeckRouting<const DECKS: usize, const CHANNELS: usize> {
    deck_pairs: [usize; DECKS],
}

impl<const DECKS: usize, const CHANNELS: usize> DeckRouting<DECKS, CHANNELS> {
    pub fn try_new(deck_pairs: [usize; DECKS]) -> Result<Self, RoutingError> {
        // Compile-time constraints on channel geometry
        const {
            assert!(
                CHANNELS % 2 == 0,
                "Compile error: CHANNELS must be an even number (stereo channel pairs)"
            );
            assert!(
                CHANNELS >= DECKS * 2,
                "Compile error: CHANNELS must be at least DECKS * 2"
            );
        }

        let max_pairs = CHANNELS / 2;
        let mut used_pairs = [false; CHANNELS];

        for &pair in &deck_pairs {
            if pair >= max_pairs {
                return Err(RoutingError::PairOutOfBounds { pair, max_pairs });
            }
            if used_pairs[pair] {
                return Err(RoutingError::DuplicatePair { pair });
            }
            used_pairs[pair] = true;
        }

        Ok(Self { deck_pairs })
    }

    pub fn deck_pairs(&self) -> &[usize; DECKS] {
        &self.deck_pairs
    }
}
