use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style, Stylize},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::{
    deck_controller::{AppStatus, DeckState}, record::{INanos, UNanos}, virtual_platter::ReadablePlatter,
};

/// Converts nanoseconds to a "mm:ss" string (e.g. 185_000_000_000 -> "03:05")
fn format_nanos(nanos: INanos) -> String {
    let total_secs = nanos.0 / 1_000_000_000;
    let minutes = total_secs / 60;
    let seconds = total_secs % 60;
    format!("{minutes:02}:{seconds:02}")
}

pub fn spawn_tui_thread<const DECKS: usize>(
    active_deck: Arc<AtomicUsize>,
    deck_states: [Arc<DeckState>; DECKS],
    platters: [ReadablePlatter; DECKS],
    app_status: AppStatus,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut terminal = ratatui::init();
        let tick_rate = Duration::from_millis(33); // ~30 FPS polling rate

        while !shutdown.load(Ordering::Relaxed) {
            let frame_start = Instant::now();

            let current_active = active_deck.load(Ordering::Relaxed);
            let status = app_status.get();

            terminal
                .draw(|frame| {
                    render_tui(frame, &deck_states, &platters, current_active, status);
                })
                .expect("Failed to draw TUI frame");

            let elapsed = frame_start.elapsed();
            if elapsed < tick_rate {
                std::thread::sleep(tick_rate - elapsed);
            }
        }

        ratatui::restore();
    })
}

fn render_tui<const DECKS: usize>(
    frame: &mut Frame,
    deck_states: &[Arc<DeckState>; DECKS],
    platters: &[ReadablePlatter; DECKS],
    active_deck_idx: usize,
    status: Option<String>,
) {
    // 1. Split layout vertically into main deck table and bottom status bar
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(frame.area());

    let header_cells = [
        "",
        "Deck",
        "State",
        "Pitch",
        "Track File",
        "Position / Duration",
    ]
    .into_iter()
    .map(|h| Cell::from(h).bold().fg(Color::Cyan));
    let header = Row::new(header_cells).height(1).bottom_margin(1);

    let rows = (0..DECKS).map(|idx| {
        let state = &deck_states[idx];
        let platter = &platters[idx];

        let is_target = idx == active_deck_idx;

        // 1. Highlight active control deck
        let (prefix, row_style) = if is_target {
            (
                ">",
                Style::default()
                    .bg(Color::Rgb(20, 35, 20))
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            (" ", Style::default().fg(Color::Gray))
        };

        // 2. Playback state
        let is_playing = state.playing.load(Ordering::Relaxed);
        let (play_str, play_style) = if is_playing {
            ("▶ PLAYING", Style::default().fg(Color::Green))
        } else {
            ("⏸ STOPPED", Style::default().fg(Color::DarkGray))
        };

        // 3. Target pitch/speed
        let pitch = state.pitch.load(Ordering::Relaxed);
        let pitch_str = format!("{pitch:.3}");

        // 4. Record Info & Playhead Position
        let (file_display, duration_nanos) = match state.cur_record.read() {
            Ok(guard) => match guard.as_ref() {
                Some(record) => (record.path.clone(), record.duration),
                None => ("[ No Record Loaded ]".to_string(), UNanos(0)),
            },
            Err(_) => (
                "[ Lock Contended, deck worker thread could be dead ]".to_string(),
                UNanos(0),
            ),
        };

        let time_display = format!(
            "{} / {}",
            format_nanos(platter.get_playhead().record_pos),
            format_nanos(INanos(duration_nanos.0 as i64))
        );

        Row::new(vec![
            Cell::from(prefix),
            Cell::from(format!("Deck {}", idx + 1)),
            Cell::from(play_str).style(play_style),
            Cell::from(pitch_str),
            Cell::from(file_display),
            Cell::from(time_display),
        ])
        .style(row_style)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),  // '>' indicator
            Constraint::Length(8),  // Deck label
            Constraint::Length(11), // Play state
            Constraint::Length(9),  // Pitch
            Constraint::Min(24),    // Track file path
            Constraint::Length(18), // Playhead / Duration
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Turntable Engine Status "),
    );

    // Render deck status table in top chunk
    frame.render_widget(table, chunks[0]);

    // 2. Handle status message rendering in bottom chunk
    let (status_text, status_style) = match status {
        Some(msg) => (msg, Style::default().fg(Color::Yellow)),
        None => (
            "status could not be retrieved, status lock potentially poisoned".to_string(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    };

    let status_widget = Paragraph::new(status_text).style(status_style).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" System Status "),
    );

    // Render system status box in bottom chunk
    frame.render_widget(status_widget, chunks[1]);
}
