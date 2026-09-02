//! The status TUI, and the terminal half of the input layer.
//!
//! The terminal is an input source as well as a display: dropping a file on it
//! makes the terminal paste the path, which arrives here as a
//! [`TermEvent::Paste`] and leaves as an [`AppEvent::PrepareRecord`]. Keys are
//! ignored, because the keyboard belongs to the SDL window - except Ctrl-C,
//! which raw mode never turns into SIGINT, so without it there would be no way
//! out of a run with no SDL window.

use ratatui::{
    Frame,
    crossterm::{
        ExecutableCommand,
        event::{
            self, DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, KeyCode,
            KeyModifiers,
        },
    },
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style, Stylize},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};
use std::io::stdout;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crossbeam::channel::Sender;
use percent_encoding::percent_decode_str;

use crate::{
    audio_health::{AudioHealth, HealthLevel},
    deck_controller::{AppStatus, DeckState},
    input_event::{AppEvent, InputEvent},
    record::{INanos, UNanos},
    tray::TrayState,
    virtual_platter::ReadablePlatter,
};

/// Converts nanoseconds to a "mm:ss" string (e.g. 185_000_000_000 -> "03:05")
fn format_nanos(nanos: INanos) -> String {
    let total_secs = nanos.0 / 1_000_000_000;
    let minutes = total_secs / 60;
    let seconds = total_secs % 60;
    format!("{minutes:02}:{seconds:02}")
}

/// `active_deck` is `None` when no input device has such a concept - a MIDI
/// controller addresses decks directly - and the display drops the marker and
/// the keyboard hint accordingly.
pub fn spawn_tui_thread<const DECKS: usize>(
    active_deck: Option<Arc<AtomicUsize>>,
    deck_states: [Arc<DeckState>; DECKS],
    platters: [ReadablePlatter; DECKS],
    tray_state: Arc<RwLock<TrayState>>,
    app_status: AppStatus,
    health: Arc<AudioHealth>,
    events: Sender<InputEvent>,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut terminal = ratatui::init();

        // Without this the terminal injects a dropped path as plain keystrokes
        // and we cannot tell it apart from typing.
        if let Err(e) = stdout().execute(EnableBracketedPaste) {
            log::error!("cannot enable bracketed paste, drag and drop will not work: {e}");
        }

        // Doubles as the terminal poll timeout, so also the worst-case delay
        // before a dropped file is noticed.
        let tick_rate = Duration::from_millis(66); // ~15 FPS

        while !shutdown.load(Ordering::Relaxed) {
            let frame_start = Instant::now();

            let current_active = active_deck
                .as_ref()
                .map(|deck| deck.load(Ordering::Relaxed));
            let status = app_status.get();
            let tray = tray_state.read().ok().map(|tray| tray.clone());

            terminal
                .draw(|frame| {
                    render_tui(
                        frame,
                        &deck_states,
                        &platters,
                        current_active,
                        tray,
                        status,
                        &health,
                    );
                })
                .expect("Failed to draw TUI frame");

            // Waiting for input doubles as the frame delay, so reading the
            // terminal costs neither a thread nor any added latency.
            let remaining = tick_rate
                .saturating_sub(frame_start.elapsed())
                .max(Duration::from_millis(1));

            match poll_terminal(remaining) {
                Ok(input_events) => {
                    for event in input_events {
                        if events.send(event).is_err() {
                            log::error!("Dispatcher is gone, stopping terminal input");
                            break;
                        }
                    }
                }
                Err(e) => {
                    log::error!("Cannot read the terminal, stopping terminal input: {e}");
                    break;
                }
            }
        }

        let _ = stdout().execute(DisableBracketedPaste);
        ratatui::restore();
    })
}

/// Waits up to `timeout` for terminal input, then drains whatever else is
/// already queued. Terminal events we have no use for are dropped.
fn poll_terminal(timeout: Duration) -> std::io::Result<Vec<InputEvent>> {
    let mut input_events = Vec::new();

    if !event::poll(timeout)? {
        return Ok(input_events);
    }

    loop {
        if let Some(event) = to_input_event(event::read()?) {
            input_events.push(event);
        }
        if !event::poll(Duration::ZERO)? {
            return Ok(input_events);
        }
    }
}

fn to_input_event(event: TermEvent) -> Option<InputEvent> {
    match event {
        // A file dropped on the terminal arrives as a paste of its path.
        TermEvent::Paste(text) => {
            let path = parse_dropped_path(&text)?;
            log::info!("Track dropped on the terminal: {path}");
            Some(InputEvent::App(AppEvent::PrepareRecord(path)))
        }

        // Raw mode means the tty never turns this into SIGINT, so it is on us.
        TermEvent::Key(key)
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            Some(InputEvent::App(AppEvent::Quit))
        }

        // the keyboard belongs to the SDL window
        _ => None,
    }
}

/// Pulls a usable path out of what a terminal sends when a file is dropped on
/// it. Terminals disagree on the format, so both real ones are handled: the
/// path as text, shell-quoted or backslash-escaped, or a `file://` URI with the
/// awkward characters percent-encoded. Several files dropped at once arrive
/// space separated, and we take the first.
///
/// The shell half is [`shlex`]'s job rather than ours because one token can mix
/// quoting styles: a name with an apostrophe arrives as
/// `'Jesse James - 50'\''s Japan.mp3'` - quoted, then an escaped quote, then
/// quoted again, because a single-quoted string cannot contain a single quote.
///
/// Returns `None` for a paste that is empty or not lexable at all (an
/// unterminated quote, say), which loads nothing rather than a guessed path.
fn parse_dropped_path(paste: &str) -> Option<String> {
    let token = shlex::split(paste)?.into_iter().next()?;

    let path = match token.strip_prefix("file://") {
        // an optional host sits between the scheme and the path
        Some(rest) => percent_decode_str(&rest[rest.find('/')?..])
            .decode_utf8_lossy()
            .into_owned(),
        None => token,
    };

    (!path.is_empty()).then_some(path)
}

/// Just the file name, so a long path does not eat the whole row. The full path
/// stays visible in the status bar.
fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

fn spinner(elapsed: Duration) -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    FRAMES[(elapsed.as_millis() / 80) as usize % FRAMES.len()]
}

/// One line describing what is in the tray, and how it should be coloured.
fn tray_line(tray: Option<TrayState>, active_deck_idx: Option<usize>) -> (String, Style) {
    let Some(tray) = tray else {
        return (
            "tray state unavailable, lock poisoned (tray thread may be dead)".to_string(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        );
    };

    match tray {
        TrayState::Empty => (
            "·  empty        drop a track onto this window".to_string(),
            Style::default().fg(Color::DarkGray),
        ),

        TrayState::Preparing { path, since } => {
            let elapsed = since.elapsed();
            (
                format!(
                    "{}  preparing    {:<44} {:>6}",
                    spinner(elapsed),
                    file_name(&path),
                    format!("{:.1}s", elapsed.as_secs_f64()),
                ),
                Style::default().fg(Color::Yellow),
            )
        }

        TrayState::Ready { info } => {
            let hint = match active_deck_idx {
                Some(idx) => format!("Enter → load on Deck {}", idx + 1),
                None => "press LOAD on a deck".to_string(),
            };
            (
                format!(
                    "●  ready        {:<44} {}   {hint}",
                    file_name(&info.path),
                    format_nanos(INanos(info.duration.0 as i64)),
                ),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        }

        TrayState::Failed { path, error } => (
            format!("✗  failed       {:<44} {error}", file_name(&path)),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    }
}

/// Renders a duration the way you would say it out loud.
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s:02}s"),
        (h, m, _) => format!("{h}h {m:02}m"),
    }
}

/// One line saying whether audio is being lost, and how it should be coloured.
fn health_line(health: &AudioHealth) -> (String, Style) {
    let digest = health.digest();

    match digest.level {
        HealthLevel::Clean => (
            format!(
                "●  clean       nothing lost in {}",
                format_duration(digest.clean_for())
            ),
            Style::default().fg(Color::Green),
        ),

        HealthLevel::Glitching => (
            format!(
                "▲  glitch      {} dropout{} in the last second",
                digest.lost,
                if digest.lost == 1 { "" } else { "s" },
            ),
            Style::default().fg(Color::Yellow),
        ),

        HealthLevel::Failing => (
            format!(
                "✗  LOSING AUDIO   {} dropouts in the last second - raise --buffer",
                digest.lost,
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    }
}

fn render_tui<const DECKS: usize>(
    frame: &mut Frame,
    deck_states: &[Arc<DeckState>; DECKS],
    platters: &[ReadablePlatter; DECKS],
    active_deck_idx: Option<usize>,
    tray: Option<TrayState>,
    status: Option<String>,
    health: &AudioHealth,
) {
    // 1. Split layout vertically into deck table, audio health, record tray and
    //    status bar
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
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

        let is_target = active_deck_idx == Some(idx);

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
                Some(record) => (file_name(&record.path), record.duration),
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

    // 2. Is the engine actually delivering the audio it computed
    let (health_text, health_style) = health_line(health);
    let health_widget = Paragraph::new(health_text).style(health_style).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Audio Health "),
    );
    frame.render_widget(health_widget, chunks[1]);

    // 3. What is waiting to be loaded
    let (tray_text, tray_style) = tray_line(tray, active_deck_idx);
    let tray_widget = Paragraph::new(tray_text).style(tray_style).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Record Tray "),
    );
    frame.render_widget(tray_widget, chunks[2]);

    // 4. Handle status message rendering in bottom chunk
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
    frame.render_widget(status_widget, chunks[3]);
}

#[cfg(test)]
mod tests {
    use super::{format_duration, parse_dropped_path};
    use std::time::Duration;

    #[test]
    fn durations_read_the_way_you_say_them() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(42)), "42s");
        assert_eq!(format_duration(Duration::from_secs(62)), "1m 02s");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1h 02m");
    }

    #[test]
    fn plain_path() {
        assert_eq!(
            parse_dropped_path("/home/me/track.mp3 ").as_deref(),
            Some("/home/me/track.mp3")
        );
    }

    #[test]
    fn backslash_escaped_spaces() {
        assert_eq!(
            parse_dropped_path("/home/me/my\\ track.mp3").as_deref(),
            Some("/home/me/my track.mp3")
        );
    }

    #[test]
    fn single_quoted_keeps_backslashes_literal() {
        assert_eq!(
            parse_dropped_path("'/home/me/back\\slash.mp3'").as_deref(),
            Some("/home/me/back\\slash.mp3")
        );
    }

    #[test]
    fn double_quoted_with_space() {
        assert_eq!(
            parse_dropped_path("\"/home/me/my track.mp3\"").as_deref(),
            Some("/home/me/my track.mp3")
        );
    }

    #[test]
    fn file_uri_is_decoded() {
        assert_eq!(
            parse_dropped_path("file:///home/me/a%20track%2B1.mp3\n").as_deref(),
            Some("/home/me/a track+1.mp3")
        );
    }

    #[test]
    fn several_files_take_the_first() {
        assert_eq!(
            parse_dropped_path("'/a/one.mp3' '/a/two.mp3' ").as_deref(),
            Some("/a/one.mp3")
        );
        assert_eq!(
            parse_dropped_path("/a/one.mp3 /a/two.mp3").as_deref(),
            Some("/a/one.mp3")
        );
    }

    #[test]
    fn apostrophe_in_single_quotes() {
        // how a shell-quoting terminal sends "Jesse James - 50's Japan.mp3"
        assert_eq!(
            parse_dropped_path("'/music/Jesse James - 50'\\''s Japan.mp3' ").as_deref(),
            Some("/music/Jesse James - 50's Japan.mp3")
        );
    }

    #[test]
    fn apostrophe_backslash_escaped() {
        assert_eq!(
            parse_dropped_path("/music/Jesse\\ James\\ -\\ 50\\'s\\ Japan.mp3").as_deref(),
            Some("/music/Jesse James - 50's Japan.mp3")
        );
    }

    #[test]
    fn apostrophe_in_file_uri() {
        assert_eq!(
            parse_dropped_path("file:///music/50%27s%20Japan.mp3").as_deref(),
            Some("/music/50's Japan.mp3")
        );
    }

    #[test]
    fn nothing_usable() {
        assert_eq!(parse_dropped_path(""), None);
        assert_eq!(parse_dropped_path("   \n"), None);
    }

    #[test]
    fn unlexable_paste_loads_nothing() {
        // No terminal sends these; loading nothing beats loading a guess.
        assert_eq!(parse_dropped_path("'/music/unterminated.mp3"), None);
        assert_eq!(parse_dropped_path("/music/trailing\\"), None);
    }
}
