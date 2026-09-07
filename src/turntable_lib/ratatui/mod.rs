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
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style, Stylize},
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
    card_reader::{CardReaderView, Outcome, Staged},
    deck_controller::DeckState,
    input_event::{AppEvent, InputEvent},
    notices::{Level, Notice, Notices},
    record::{INanos, TrackRef, UNanos},
    tray::TrayState,
    virtual_platter::ReadablePlatter,
};

pub mod platter_dial;
mod waveform;

use platter_dial::{DIAL_COLS, DIAL_ROWS, dial_area, platter_dial};
use waveform::waveform;

/// Green phosphor on a dark machine. One hue carries everything the engine is
/// doing, at four brightnesses, and red appears only when audio is being lost.
mod palette {
    use ratatui::style::Color;

    /// Borders, a stopped deck, an empty tray.
    pub const CHROME: Color = Color::Rgb(74, 84, 76);
    /// Panel names, table headers, a deck sitting there loaded.
    pub const LABEL: Color = Color::Rgb(124, 136, 126);
    /// Powered, with nothing to report.
    pub const DIM: Color = Color::Rgb(44, 122, 60);
    /// Running: a playing deck, a track ready, the part already heard.
    pub const LIT: Color = Color::Rgb(51, 230, 71);
    /// The playhead, the one thing on screen that moves.
    pub const NEEDLE: Color = Color::Rgb(230, 255, 233);
    /// The part of a track still ahead of the needle.
    pub const SUBMERGED: Color = Color::Rgb(30, 42, 34);
    /// Waiting on something, or losing a little audio.
    pub const CAUTION: Color = Color::Rgb(200, 208, 196);
    /// Audio is being lost, or a track will not load.
    pub const ALARM: Color = Color::Rgb(224, 43, 24);
    /// Behind the deck the keyboard is pointed at.
    pub const SELECTED: Color = Color::Rgb(16, 26, 18);
}

use palette::*;

/// A panel in the chassis grey, with its name stamped on the border.
fn panel(title: &'static str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CHROME))
        .title(title)
        .title_style(Style::default().fg(LABEL))
}

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
    notices: Notices,
    health: Arc<AudioHealth>,
    card_reader: Option<CardReaderView>,
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
            let notice = notices.current();
            let tray = tray_state.read().ok().map(|tray| tray.clone());

            terminal
                .draw(|frame| {
                    render_tui(
                        frame,
                        &deck_states,
                        &platters,
                        current_active,
                        tray,
                        notice,
                        &health,
                        card_reader.as_ref(),
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
            Some(InputEvent::App(AppEvent::PrepareRecord(TrackRef {
                path,
                meta: None,
            })))
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

/// Enough of a card id to tell two apart, which is all anyone does with one.
/// They are 64 hex characters, and the reason beside them is the part worth
/// reading.
fn short_card(payload: &str) -> String {
    const SHOWN: usize = 12;

    if payload.chars().count() <= SHOWN {
        return payload.to_string();
    }

    let head: String = payload.chars().take(SHOWN).collect();
    format!("{head}…")
}

/// Just the file name, so a long path does not eat the whole row.
fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// What to call a track on screen. Artists and titles come from the library, so
/// a dropped file falls back to its file name.
fn track_label(track: &TrackRef) -> String {
    match &track.meta {
        Some(meta) => format!("{} — {}", meta.artist, meta.title),
        None => file_name(&track.path),
    }
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
            Style::default().fg(ALARM).add_modifier(Modifier::BOLD),
        );
    };

    match tray {
        TrayState::Empty => (
            "·  empty        scan a card, or drop a track onto this window".to_string(),
            Style::default().fg(CHROME),
        ),

        TrayState::Preparing {
            track,
            since,
            queued,
        } => {
            let elapsed = since.elapsed();
            let next = match &queued {
                Some(next) => format!("   then {}", track_label(next)),
                None => String::new(),
            };
            (
                format!(
                    "{}  preparing    {:<44} {:>6}{next}",
                    spinner(elapsed),
                    track_label(&track),
                    format!("{:.1}s", elapsed.as_secs_f64()),
                ),
                Style::default().fg(CAUTION),
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
                    track_label(&info.track),
                    format_nanos(INanos(info.duration.0 as i64)),
                ),
                Style::default().fg(LIT).add_modifier(Modifier::BOLD),
            )
        }

        TrayState::Failed { track, error } => (
            format!("✗  failed       {:<44} {error}", file_name(&track.path)),
            Style::default().fg(ALARM).add_modifier(Modifier::BOLD),
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
            Style::default().fg(DIM),
        ),

        HealthLevel::Glitching => (
            format!(
                "▲  glitch      {} dropout{} in the last second",
                digest.lost,
                if digest.lost == 1 { "" } else { "s" },
            ),
            Style::default().fg(CAUTION),
        ),

        HealthLevel::Failing => (
            format!(
                "✗  LOSING AUDIO   {} dropouts in the last second - raise --buffer",
                digest.lost,
            ),
            Style::default().fg(ALARM).add_modifier(Modifier::BOLD),
        ),
    }
}

/// 2 borders, a header and its bottom margin. The deck rows are added on top.
const DECK_TABLE_ROWS: u16 = 4;

/// One strip per deck: the whole track across the panel, played part filled in.
fn render_waveforms<const DECKS: usize>(
    frame: &mut Frame,
    area: Rect,
    deck_states: &[Arc<DeckState>; DECKS],
    platters: &[ReadablePlatter; DECKS],
) {
    let block = panel(" Waveforms ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let strips = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Fill(1); DECKS])
        .split(inner);

    for (idx, strip) in strips.iter().enumerate() {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(DIAL_COLS),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(*strip);

        frame.render_widget(
            Paragraph::new(format!("{}", idx + 1)).fg(CHROME),
            columns[0],
        );

        let record = deck_states[idx].cur_record.read().ok();
        let record = record.as_deref().and_then(|record| record.as_ref());
        let pos = platters[idx].get_playhead().record_pos;

        let disk_color = if record.is_some() { LIT } else { CHROME };
        frame.render_widget(platter_dial(pos, disk_color, NEEDLE), dial_area(columns[1]));

        frame.render_widget(waveform(record, pos, columns[3]), columns[3]);
    }
}

fn render_tui<const DECKS: usize>(
    frame: &mut Frame,
    deck_states: &[Arc<DeckState>; DECKS],
    platters: &[ReadablePlatter; DECKS],
    active_deck_idx: Option<usize>,
    tray: Option<TrayState>,
    notice: Option<Notice>,
    health: &AudioHealth,
    card_reader: Option<&CardReaderView>,
) {
    // 1. Split layout vertically into deck table, waveforms, audio health,
    //    record tray and status bar
    //
    // The table has a natural size and the waveforms use every row they are
    // given, so the waveforms are the panel that absorbs the leftover height.
    let mut panels = vec![
        Constraint::Length(DECK_TABLE_ROWS + DECKS as u16),
        Constraint::Min(2 + DECKS as u16 * DIAL_ROWS),
        Constraint::Length(3),
        Constraint::Length(3),
    ];
    if card_reader.is_some() {
        panels.push(Constraint::Length(3));
    }
    // Kept even with nothing to report, so a warning arriving mid-set does not
    // shift every panel above it.
    panels.push(Constraint::Length(3));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(panels)
        .split(frame.area());

    let header_cells = ["", "Deck", "State", "Pitch", "Track", "Position / Duration"]
        .into_iter()
        .map(|h| Cell::from(h).bold().fg(LABEL));
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
                    .bg(SELECTED)
                    .fg(LIT)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            (" ", Style::default().fg(LABEL))
        };

        // 2. Playback state
        let is_playing = state.playing.load(Ordering::Relaxed);
        let (play_str, play_style) = if is_playing {
            ("▶ PLAYING", Style::default().fg(LIT))
        } else {
            ("⏸ STOPPED", Style::default().fg(CHROME))
        };

        // 3. Target pitch/speed
        let pitch = state.pitch.load(Ordering::Relaxed);
        let pitch_str = {
            // A pitch just under 1.0 rounds to -0.0, which prints as "-0.0%".
            // Adding zero turns that back into a plain 0.0.
            let percent = ((pitch - 1.) * 1000.).round() / 10. + 0.;
            format!("{percent:+.1}%")
        };

        // 4. Record Info & Playhead Position
        let (file_display, duration_nanos) = match state.cur_record.read() {
            Ok(guard) => match guard.as_ref() {
                Some(record) => (track_label(&record.track), record.duration),
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
            Constraint::Min(24),    // Track
            Constraint::Length(18), // Playhead / Duration
        ],
    )
    .header(header)
    .block(panel(" Turntable Engine Status "));

    // Render deck status table in top chunk
    frame.render_widget(table, chunks[0]);

    // 2. Where each deck is in its track, and what is coming
    render_waveforms(frame, chunks[1], deck_states, platters);

    // 3. Is the engine actually delivering the audio it computed
    let (health_text, health_style) = health_line(health);
    let health_widget = Paragraph::new(health_text)
        .style(health_style)
        .block(panel(" Audio Health "));
    frame.render_widget(health_widget, chunks[2]);

    // 4. What is waiting to be loaded
    let (tray_text, tray_style) = tray_line(tray, active_deck_idx);
    let tray_widget = Paragraph::new(tray_text)
        .style(tray_style)
        .block(panel(" Record Tray "));
    frame.render_widget(tray_widget, chunks[3]);

    // 5. Whether cards can be scanned at all, when this run asked for them
    let mut next = 4;
    if let Some(reader) = card_reader {
        let (scanner_text, scanner_style) = scanner_line(reader);
        let scanner_widget = Paragraph::new(scanner_text)
            .style(scanner_style)
            .block(panel(" QR Scanner "));
        frame.render_widget(scanner_widget, chunks[next]);
        next += 1;
    }

    // 6. Anything that went wrong, for as long as it is worth reading
    let (title, message, style) = match notice {
        Some(Notice {
            message,
            level: Level::Warning,
        }) => (" Warning ", message, Style::default().fg(CAUTION)),

        Some(Notice {
            message,
            level: Level::Error,
        }) => (
            " Problem ",
            message,
            Style::default().fg(ALARM).add_modifier(Modifier::BOLD),
        ),

        None => (" Notices ", "".to_string(), Style::default().fg(CHROME)),
    };

    let notice_widget = Paragraph::new(message).style(style).block(panel(title));
    frame.render_widget(notice_widget, chunks[next]);
}

/// A dead gun is shown as dead and nothing else. Naming the card it last read
/// would be naming a track the next press is not going to load.
/// A health light for the gun, and nothing else. Whether a card reached the tray
/// is the tray's line to say, and whether a scan just registered is answered by
/// the tray changing - this panel can only tell you that once.
fn scanner_line(reader: &CardReaderView) -> (String, Style) {
    let ready = || ("●  ready".to_string(), Style::default().fg(DIM));
    let problem = Style::default().fg(ALARM).add_modifier(Modifier::BOLD);

    match reader.staged() {
        // A gun that has read nothing and a gun whose last card reached the tray
        // look the same on purpose: working, with nothing here to act on.
        Staged::Empty => ready(),

        Staged::Card(card) => match &card.outcome {
            Outcome::SentToTray => ready(),

            Outcome::Resolving => (
                format!("●  looking up   {}", short_card(&card.payload)),
                Style::default().fg(CAUTION),
            ),

            Outcome::Unknown => (
                format!(
                    "✗  unknown card  {}   not in the library",
                    short_card(&card.payload)
                ),
                problem,
            ),

            Outcome::Failed(reason) => (
                format!("✗  {}   {reason}", short_card(&card.payload)),
                problem,
            ),
        },

        Staged::Unavailable(fault) => (format!("✗  {fault}"), problem),
    }
}

#[cfg(test)]
mod tests {
    use super::{format_duration, parse_dropped_path, short_card, track_label};
    use crate::record::{TrackMeta, TrackRef};
    use std::time::Duration;

    /// A real card id, which has to leave room for the reason beside it.
    #[test]
    fn a_long_card_id_is_cut_short() {
        let shortened =
            short_card("003778873f9d0ca7c5c1a9519dafdd6eadec051f26aa0f7ab83e17eeba3032a4");

        assert_eq!(shortened, "003778873f9d…");
        assert!(shortened.chars().count() < 20, "{shortened} is still long");
    }

    /// Newer cards carry a bare track id, which is short enough to read whole.
    #[test]
    fn a_short_card_id_is_left_alone() {
        assert_eq!(short_card("1701"), "1701");
    }

    /// Anything with a QR code on it can reach the gun, and a stray payload must
    /// not be cut mid-character.
    #[test]
    fn a_multibyte_payload_is_cut_on_a_character() {
        assert_eq!(short_card("ααααααααααααααα"), "αααααααααααα…");
    }

    #[test]
    fn a_track_from_the_library_is_named_by_its_artist_and_title() {
        let track = TrackRef {
            path: "/music/04 - untitled_master_v3.flac".to_string(),
            meta: Some(TrackMeta {
                artist: "Loraine James".to_string(),
                title: "Glitch the System".to_string(),
            }),
        };

        assert_eq!(track_label(&track), "Loraine James — Glitch the System");
    }

    /// A dropped file arrives as a bare path, so its file name is what shows.
    #[test]
    fn a_dropped_track_is_named_by_its_file() {
        let track = TrackRef {
            path: "/music/04 - untitled_master_v3.flac".to_string(),
            meta: None,
        };

        assert_eq!(track_label(&track), "04 - untitled_master_v3.flac");
    }

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
