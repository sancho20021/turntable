use std::time::Instant;

use thiserror::Error;

use crate::{
    audio_health::HealthRecorder, platter_audio_processor::PlatterAudioProcessor,
    stereo_frame::StereoFrame,
};

pub struct SamplesPoller<const DECKS: usize> {
    buffers: [Vec<StereoFrame>; DECKS],
    audio_processors: [PlatterAudioProcessor; DECKS],
    routing: DeckRouting<DECKS>,
    /// audio-thread end of the health metrics, see [`crate::audio_health`]
    health: HealthRecorder,
}

impl<const DECKS: usize> SamplesPoller<DECKS> {
    pub fn new(
        samples_n: usize,
        audio_processors: [PlatterAudioProcessor; DECKS],
        routing: DeckRouting<DECKS>,
        health: HealthRecorder,
    ) -> Self {
        let buf = vec![StereoFrame::default(); samples_n];
        Self {
            buffers: std::array::from_fn(|_| buf.clone()),
            audio_processors,
            routing,
            health,
        }
    }

    /// `callback_nanos` is the device clock stamp of this callback, the only
    /// source that knows how much audio actually left the DAC while we were
    /// away. See [`crate::audio_health::HealthRecorder::on_callback_start`].
    pub fn write_frames(&mut self, data: &mut [f32], callback_nanos: u64) {
        let started = Instant::now();
        self.health.on_callback_start(callback_nanos);

        let channels = self.routing.channels();
        let total_samples = data.len() / channels;

        if data.len() % channels != 0 || total_samples != self.buffers[0].len() {
            data.fill(0.0);
            self.health
                .on_frame_mismatch(callback_nanos, total_samples as u32);
            self.finish(callback_nanos, started, total_samples);
            return;
        }

        // Zero out output buffer so unmapped/extra channels remain silent
        data.fill(0.0);

        for deck_idx in 0..DECKS {
            self.audio_processors[deck_idx]
                .write_frames(&mut self.buffers[deck_idx][..total_samples]);
        }

        for sample_idx in 0..total_samples {
            let output_offset = sample_idx * channels;

            for deck_idx in 0..DECKS {
                let pair_idx = self.routing.deck_pairs()[deck_idx];
                let ch_left = pair_idx * 2;
                let ch_right = ch_left + 1;

                let frame = self.buffers[deck_idx][sample_idx];
                data[output_offset + ch_left] = frame.l;
                data[output_offset + ch_right] = frame.r;
            }
        }

        self.finish(callback_nanos, started, total_samples);
    }

    /// Closes out the health bookkeeping for one callback. Called on every exit
    /// path, so a silenced block still shows up as time spent.
    fn finish(&mut self, callback_nanos: u64, started: Instant, frames: usize) {
        let elapsed_nanos = started.elapsed().as_nanos() as u64;
        self.health
            .on_callback_end(callback_nanos, elapsed_nanos, frames as u32);
    }
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum RoutingError {
    #[error(
        "Output device has an odd channel count ({actual}); stereo pairs require an even number"
    )]
    OddChannelCount { actual: usize },

    #[error(
        "Not enough output channels on device: required at least {required_min} channels for {decks} decks, but device only provides {actual}"
    )]
    NotEnoughChannels {
        required_min: usize,
        actual: usize,
        decks: usize,
    },

    #[error(
        "Stereo pair index {pair} is out of bounds: device only has {max_pairs} stereo pair(s) (indices 0..{max_pairs})"
    )]
    PairOutOfBounds { pair: usize, max_pairs: usize },

    #[error("Duplicate channel assignment: stereo pair {pair} is assigned to multiple decks")]
    DuplicatePair { pair: usize },
}

/// Maps each deck to a target stereo pair index `0 .. (channels / 2)`.
/// `DECKS` is a const generic, but `channels` is determined at runtime from the audio device.
#[derive(Debug, Clone)]
pub struct DeckRouting<const DECKS: usize> {
    deck_pairs: [usize; DECKS],
    channels: usize,
}

impl<const DECKS: usize> DeckRouting<DECKS> {
    /// Validates requested deck routing against the actual audio device channel count.
    pub fn try_new(deck_pairs: [usize; DECKS], channels: usize) -> Result<Self, RoutingError> {
        if channels % 2 != 0 {
            return Err(RoutingError::OddChannelCount { actual: channels });
        }

        let required_min = DECKS * 2;
        if channels < required_min {
            return Err(RoutingError::NotEnoughChannels {
                required_min,
                actual: channels,
                decks: DECKS,
            });
        }

        let max_pairs = channels / 2;
        // Stack array to avoid heap allocation during setup (supports up to 64 hardware channels)
        let mut used_pairs = [false; 32];

        for &pair in &deck_pairs {
            if pair >= max_pairs {
                return Err(RoutingError::PairOutOfBounds { pair, max_pairs });
            }
            if used_pairs[pair] {
                return Err(RoutingError::DuplicatePair { pair });
            }
            used_pairs[pair] = true;
        }

        Ok(Self {
            deck_pairs,
            channels,
        })
    }

    #[inline]
    pub fn deck_pairs(&self) -> &[usize; DECKS] {
        &self.deck_pairs
    }

    #[inline]
    pub fn channels(&self) -> usize {
        self.channels
    }
}
