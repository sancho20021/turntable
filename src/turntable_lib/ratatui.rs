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

use crate::{
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

        let tick_rate = Duration::from_millis(33); // ~30 FPS polling rate

        while !shutdown.load(Ordering::Relaxed) {
            let frame_start = Instant::now();

            let current_active = active_deck
                .as_ref()
                .map(|deck| deck.load(Ordering::Relaxed));
            let status = app_status.get();
            let tray = tray_state.read().ok().map(|tray| tray.clone());

            terminal
                .draw(|frame| {
                    render_tui(frame, &deck_states, &platters, current_active, tray, status);
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
/// it: the path as text, shell-quoted or backslash-escaped, sometimes as a
/// `file://` URI, usually with a trailing space. Several files dropped at once
/// arrive space separated, and we take the first.
fn parse_dropped_path(paste: &str) -> Option<String> {
    let token = first_shell_token(paste.trim());
    let path = strip_file_uri(&token).unwrap_or(token);
    (!path.is_empty()).then_some(path)
}

/// The first shell-style token in `text`, with quoting and escapes resolved.
///
/// One token can mix quoting styles, which is why this walks the whole string
/// instead of looking at the first character and guessing. A name with an
/// apostrophe is the usual case: `Jesse James - 50's Japan.mp3` arrives as
/// `'Jesse James - 50'\''s Japan.mp3'` - quoted, then an escaped quote, then
/// quoted again - because a single-quoted string cannot contain a single quote.
fn first_shell_token(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();

    while let Some(c) = chars.next() {
        match c {
            // unquoted whitespace ends the token
            c if c.is_whitespace() => break,

            // an escape outside quotes carries the next character through
            '\\' => {
                if let Some(escaped) = chars.next() {
                    out.push(escaped);
                }
            }

            // single quotes are literal all the way to the closing quote
            '\'' => {
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    out.push(c);
                }
            }

            // double quotes still honour escapes
            '"' => {
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                out.push(escaped);
                            }
                        }
                        c => out.push(c),
                    }
                }
            }

            c => out.push(c),
        }
    }

    out
}

/// `file:///home/me/a%20track.mp3` -> `/home/me/a track.mp3`.
fn strip_file_uri(text: &str) -> Option<String> {
    let rest = text.strip_prefix("file://")?;
    // an optional host sits between the scheme and the path
    let slash = rest.find('/')?;
    Some(percent_decode(&rest[slash..]))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(byte) = hex_pair(bytes[i + 1], bytes[i + 2]) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }

    String::from_utf8_lossy(&out).into_owned()
}

fn hex_pair(hi: u8, lo: u8) -> Option<u8> {
    fn digit(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    Some(digit(hi)? * 16 + digit(lo)?)
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

fn render_tui<const DECKS: usize>(
    frame: &mut Frame,
    deck_states: &[Arc<DeckState>; DECKS],
    platters: &[ReadablePlatter; DECKS],
    active_deck_idx: Option<usize>,
    tray: Option<TrayState>,
    status: Option<String>,
) {
    // 1. Split layout vertically into deck table, record tray and status bar
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
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

    // 2. What is waiting to be loaded
    let (tray_text, tray_style) = tray_line(tray, active_deck_idx);
    let tray_widget = Paragraph::new(tray_text).style(tray_style).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Record Tray "),
    );
    frame.render_widget(tray_widget, chunks[1]);

    // 3. Handle status message rendering in bottom chunk
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
    frame.render_widget(status_widget, chunks[2]);
}

#[cfg(test)]
mod tests {
    use super::parse_dropped_path;

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
}
