//! The platter dial: a disc with one radial mark, turned to wherever the
//! playhead has got to. Groove position and platter angle are rigidly coupled
//! on a record, so the mark stands still under a stopped deck, sweeps under a
//! playing one, and swings back under a scratch, with nothing to drive it but
//! the playhead.

use ratatui::{
    layout::Rect,
    style::Color,
    widgets::{
        Widget,
        canvas::{Canvas, Circle, Line},
    },
};

use crate::record::INanos;

/// Braille packs two dots per cell across and four down, and a terminal cell is
/// about twice as tall as it is wide, so a 2:1 box of cells is a square grid of
/// dots and the disc comes out round.
pub const DIAL_COLS: u16 = 12;
pub const DIAL_ROWS: u16 = 6;

const NOMINAL_RPM: f64 = 100. / 3.;
const REVOLUTION_NANOS: f64 = 60e9 / NOMINAL_RPM;

/// Leaves a dot of slack inside the dot grid, so the disc does not flatten
/// against the edges.
const RIM: f64 = 0.9;

/// The dial's own rect: [`DIAL_COLS`] by [`DIAL_ROWS`], at the top left of
/// `area`, clipped to it.
pub fn dial_area(area: Rect) -> Rect {
    Rect {
        width: DIAL_COLS.min(area.width),
        height: DIAL_ROWS.min(area.height),
        ..area
    }
}

pub fn platter_dial(pos: INanos, disc: Color, mark: Color) -> impl Widget {
    let angle = phase(pos);

    Canvas::default()
        .x_bounds([-1., 1.])
        .y_bounds([-1., 1.])
        .paint(move |ctx| {
            ctx.draw(&Circle::new(0., 0., RIM, disc));

            // A layer of its own, so a cell holding both the disc and the mark
            // takes the mark's colour.
            ctx.layer();
            ctx.draw(&Line {
                x1: 0.,
                y1: 0.,
                x2: RIM * angle.sin(),
                y2: RIM * angle.cos(),
                color: mark,
            });
        })
}

/// Where in its turn the platter is, as radians clockwise from twelve o'clock.
///
/// A playhead sitting behind the start of the record is negative, which
/// `rem_euclid` folds forwards into the turn.
fn phase(pos: INanos) -> f64 {
    let turns = pos.0 as f64 / REVOLUTION_NANOS;
    std::f64::consts::TAU * turns.rem_euclid(1.)
}
