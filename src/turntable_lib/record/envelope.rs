//! An amplitude envelope of a whole track, for drawing it.

use crate::stereo_frame::StereoFrame;

/// Points in an envelope, whatever the track's length. A terminal is far
/// narrower than this, so the envelope survives being resized.
pub const BUCKETS: usize = 2048;

/// Loudness over time, on a scale where `1.0` is the track's own loudest
/// moment.
pub fn rms(samples: &[StereoFrame]) -> [f32; BUCKETS] {
    if samples.is_empty() {
        return [0.; BUCKETS];
    }

    let mut buckets = std::array::from_fn(|i| bucket_rms(samples, i));
    normalize(&mut buckets);
    buckets
}

/// The `i`th of [`BUCKETS`] contiguous ranges that together cover every frame.
fn bucket_rms(samples: &[StereoFrame], i: usize) -> f32 {
    let lo = i * samples.len() / BUCKETS;
    let hi = ((i + 1) * samples.len() / BUCKETS)
        .max(lo + 1)
        .min(samples.len());

    let frames = &samples[lo..hi];
    let squares: f64 = frames
        .iter()
        .map(|frame| f64::from(frame.l * frame.l + frame.r * frame.r))
        .sum();

    (squares / (2 * frames.len()) as f64).sqrt() as f32
}

fn normalize(buckets: &mut [f32; BUCKETS]) {
    let peak = buckets.iter().copied().fold(0., f32::max);
    if peak > 0. {
        for bucket in buckets.iter_mut() {
            *bucket /= peak;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(count: usize, amplitude: f32) -> Vec<StereoFrame> {
        vec![
            StereoFrame {
                l: amplitude,
                r: amplitude
            };
            count
        ]
    }

    /// The envelope is drawn into a fixed number of columns, so its length must
    /// not depend on how long the track is.
    #[test]
    fn every_track_gets_the_same_number_of_points() {
        assert_eq!(rms(&frames(48_000 * 300, 0.5)).len(), BUCKETS);
        assert_eq!(rms(&frames(7, 0.5)).len(), BUCKETS);
        assert_eq!(rms(&frames(1, 0.5)).len(), BUCKETS);
    }

    /// A track shorter than the envelope means most buckets cover the same
    /// frame, and none of them may cover nothing at all.
    #[test]
    fn a_track_shorter_than_the_envelope_has_no_gaps() {
        let envelope = rms(&frames(7, 0.5));

        assert!(
            envelope.iter().all(|&point| point == 1.),
            "a track at one steady amplitude came back uneven"
        );
    }

    #[test]
    fn the_loudest_moment_is_the_top_of_the_scale() {
        let mut samples = frames(48_000, 0.1);
        samples.extend(frames(48_000, 0.4));

        let envelope = rms(&samples);
        let peak = envelope.iter().copied().fold(0., f32::max);

        assert!((peak - 1.).abs() < 1e-6, "loudest bucket reads {peak}");
    }

    #[test]
    fn a_quiet_half_reads_quieter_than_a_loud_one() {
        let mut samples = frames(48_000, 0.1);
        samples.extend(frames(48_000, 0.8));

        let envelope = rms(&samples);
        let (quiet, loud) = envelope.split_at(BUCKETS / 2);

        assert!(quiet.iter().all(|&point| point < 0.2), "{:?}", &quiet[..4]);
        assert!(loud.iter().all(|&point| point > 0.9), "{:?}", &loud[..4]);
    }

    /// Dividing by the loudest moment of a track that has none.
    #[test]
    fn silence_stays_silent() {
        for envelope in [rms(&frames(48_000, 0.)), rms(&[])] {
            assert!(
                envelope.iter().all(|point| *point == 0.),
                "silence came back with something in it"
            );
        }
    }
}
