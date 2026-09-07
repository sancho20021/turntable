//! One deck's track drawn across a strip, filled in up to the playhead.

use ratatui::{
    layout::Rect,
    style::Color,
    symbols::Marker,
    widgets::{
        Widget,
        canvas::{Canvas, Line},
    },
};

use super::palette::{LIT, NEEDLE, SUBMERGED};
use crate::{
    deck_controller::RecordInfo,
    record::{INanos, UNanos},
};

/// Perceived loudness goes roughly as amplitude^0.6, so bending the envelope by
/// this keeps a breakdown visibly taller than an intro.
const LOUDNESS_GAMMA: f32 = 0.5;

/// Quadrants pack two dots per cell across and two down.
const DOTS_PER_COL: u16 = 2;
const DOTS_PER_ROW: u16 = 2;

/// An empty deck draws nothing.
///
/// A cell holds two half-columns but takes one colour, so colour is decided
/// per cell. Otherwise a cell that only the taller half-column reaches would
/// change colour a step before or after the cells below it.
pub fn waveform<'a>(record: Option<&'a RecordInfo>, pos: INanos, area: Rect) -> impl Widget + 'a {
    let cols = area.width * DOTS_PER_COL;
    let rows = area.height * DOTS_PER_ROW;
    let playhead = record.and_then(|record| playhead_column(pos, record.duration, area.width));

    Canvas::default()
        .marker(Marker::Quadrant)
        .x_bounds([0., (cols.max(1) - 1) as f64])
        .y_bounds([0., (rows.max(1) - 1) as f64])
        .paint(move |ctx| {
            let Some(record) = record else { return };
            let envelope = record.envelope.as_slice();

            for x in 0..cols {
                let level = column_peak(envelope, x, cols).powf(LOUDNESS_GAMMA);
                let top = (level * rows as f32).round() - 1.;
                if top < 0. {
                    continue;
                }
                ctx.draw(&Line {
                    x1: x as f64,
                    y1: 0.,
                    x2: x as f64,
                    y2: top as f64,
                    color: colour(x / DOTS_PER_COL, playhead),
                });
            }
        })
}

/// The loudest point of the track under screen column `x`.
///
/// A whole track squeezed into a terminal puts dozens of points in one column,
/// and the peak is what keeps a transient visible.
fn column_peak(envelope: &[f32], x: u16, width: u16) -> f32 {
    if width == 0 {
        return 0.;
    }

    let lo = x as usize * envelope.len() / width as usize;
    let hi = ((x as usize + 1) * envelope.len() / width as usize)
        .max(lo + 1)
        .min(envelope.len());

    envelope[lo..hi].iter().copied().fold(0., f32::max)
}

/// Which column the playhead sits in, or `None` while it is off the record.
fn playhead_column(pos: INanos, duration: UNanos, width: u16) -> Option<u16> {
    if duration.0 == 0 || width == 0 || pos.0 < 0 {
        return None;
    }

    let column = pos.0 as u128 * width as u128 / duration.0 as u128;
    (column < width as u128).then(|| column as u16)
}

fn colour(cell: u16, playhead: Option<u16>) -> Color {
    match playhead {
        Some(head) if cell == head => NEEDLE,
        Some(head) if cell < head => LIT,
        _ => SUBMERGED,
    }
}

#[cfg(test)]
mod tests {
    use super::{column_peak, playhead_column};
    use crate::record::{INanos, UNanos};

    /// Five minutes, as the platter counts it.
    const FIVE_MINUTES: UNanos = UNanos(300_000_000_000);

    /// A whole track squeezed into a terminal covers dozens of points per
    /// column, and the loud moment inside one must still be what gets drawn.
    #[test]
    fn a_transient_survives_a_narrow_terminal() {
        let mut envelope = [0.; 2048];
        envelope[1000] = 1.;

        let columns: Vec<f32> = (0..80).map(|x| column_peak(&envelope, x, 80)).collect();
        let tallest = columns.iter().copied().fold(0., f32::max);

        assert_eq!(tallest, 1., "the transient was averaged away");
        assert_eq!(
            columns.iter().filter(|point| **point > 0.).count(),
            1,
            "the transient smeared across neighbouring columns"
        );
    }

    #[test]
    fn the_playhead_runs_from_the_first_column_to_the_last() {
        assert_eq!(playhead_column(INanos(0), FIVE_MINUTES, 100), Some(0));
        assert_eq!(
            playhead_column(INanos(150_000_000_000), FIVE_MINUTES, 100),
            Some(50)
        );
        assert_eq!(
            playhead_column(INanos(299_999_999_999), FIVE_MINUTES, 100),
            Some(99)
        );
    }

    /// The playhead runs past the end of a record that finished, and sits behind
    /// the start after a scratch back through zero.
    #[test]
    fn a_playhead_off_the_record_lands_in_no_column() {
        assert_eq!(
            playhead_column(INanos(300_000_000_000), FIVE_MINUTES, 100),
            None
        );
        assert_eq!(playhead_column(INanos(-1), FIVE_MINUTES, 100), None);
        assert_eq!(playhead_column(INanos(0), UNanos(0), 100), None);
    }
}
