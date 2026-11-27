//! Terminal emulation for the serial viewport.
//!
//! Real sessions feed raw bytes to the terminal emulator's ANSI processor.
//! The device controls echo, prompts, and cursor movement.
//! The desktop does not synthesize local TX echo.
//! Extract visible rows and a bounded history from the terminal grid.
//! The UI replaces its previous snapshot with the one published on the event bus.
//! - Scrollback defaults to one million rows and grows as output arrives.
//!   Live snapshots use a separate row budget.

use std::time::Instant;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors as DynColors;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb, StdSyncHandler};

use duthub_core::ipc::{ScreenRow, ScreenSnapshot};

/// Maximum snapshot rows, including visible rows and history.
pub const SNAPSHOT_MAX_ROWS: usize = 400;
/// The live frame budget is independent of stored history.
pub const LIVE_SNAPSHOT_ROW_CAP: usize = 1_000;
/// Default scrollback capacity. The grid allocates rows as they are needed.
pub const SCROLLBACK_HISTORY: usize = 1_000_000;
pub const MIN_SCROLLBACK_ROWS: usize = 100;
/// The terminal engine indexes rows with signed 32-bit line offsets.
pub const MAX_SCROLLBACK_ROWS: usize = i32::MAX as usize;
/// Older rows are read on demand, independently of live frame publication.
pub const HISTORY_PAGE_ROWS: usize = 4_000;
/// Initial session grid. The UI sends a resize after measuring the viewport.
pub const DEFAULT_COLS: usize = 100;
pub const DEFAULT_ROWS: usize = 30;

/// Grid dimensions supplied to the terminal constructor and resize operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermGrid {
    pub cols: usize,
    pub rows: usize,
}

impl TermGrid {
    #[must_use]
    pub fn new(cols: usize, rows: usize) -> Self {
        Self { cols, rows }
    }
}

impl Dimensions for TermGrid {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Create a terminal for a hardware session and discard emulator-generated events.
#[must_use]
pub fn create_term() -> Term<VoidListener> {
    create_term_with_scrollback(SCROLLBACK_HISTORY)
}

/// Create a terminal with the configured number of scrollback rows.
#[must_use]
pub fn create_term_with_scrollback(scrollback_rows: usize) -> Term<VoidListener> {
    let mut term = create_term_with_grid(TermGrid::new(DEFAULT_COLS, DEFAULT_ROWS));
    term.grid_mut()
        .update_history(scrollback_rows.clamp(MIN_SCROLLBACK_ROWS, MAX_SCROLLBACK_ROWS));
    term
}

fn create_term_with_grid(grid: TermGrid) -> Term<VoidListener> {
    Term::new(
        Config {
            scrolling_history: SCROLLBACK_HISTORY,
            ..Config::default()
        },
        &grid,
        VoidListener,
    )
}

/// Restore the initial screen and input mode while discarding incomplete parser
/// sequences, preserving the configured scrollback capacity.
pub fn reset_terminal(term: &mut Term<VoidListener>, parser: &mut Processor<StdSyncHandler>) {
    use alacritty_terminal::vte::ansi::Handler as _;
    term.reset_state();
    *parser = Processor::new();
}

/// Commit a pending synchronized update when the ANSI synchronization deadline expires.
pub fn flush_expired_sync(
    term: &mut Term<VoidListener>,
    parser: &mut Processor<StdSyncHandler>,
    now: Instant,
) -> bool {
    if parser
        .sync_timeout()
        .sync_timeout()
        .is_some_and(|deadline| now >= deadline)
    {
        parser.stop_sync(term);
        return true;
    }
    false
}

/// The first 16 colors in the standard xterm palette.
const BASE16: [Rgb; 16] = [
    Rgb {
        r: 0x2e,
        g: 0x34,
        b: 0x36,
    }, // black
    Rgb {
        r: 0xcc,
        g: 0x66,
        b: 0x66,
    }, // red
    Rgb {
        r: 0xb5,
        g: 0xbd,
        b: 0x68,
    }, // green
    Rgb {
        r: 0xf0,
        g: 0xc6,
        b: 0x74,
    }, // yellow
    Rgb {
        r: 0x81,
        g: 0xa2,
        b: 0xbe,
    }, // blue
    Rgb {
        r: 0xb2,
        g: 0x94,
        b: 0xbb,
    }, // magenta
    Rgb {
        r: 0x8a,
        g: 0xbe,
        b: 0xb7,
    }, // cyan
    Rgb {
        r: 0xc5,
        g: 0xc8,
        b: 0xc6,
    }, // white
    Rgb {
        r: 0x66,
        g: 0x5e,
        b: 0x54,
    }, // bright black
    Rgb {
        r: 0xd5,
        g: 0x4e,
        b: 0x53,
    }, // bright red
    Rgb {
        r: 0xb9,
        g: 0xca,
        b: 0x4a,
    }, // bright green
    Rgb {
        r: 0xe7,
        g: 0xc5,
        b: 0x47,
    }, // bright yellow
    Rgb {
        r: 0x7a,
        g: 0xa6,
        b: 0xda,
    }, // bright blue
    Rgb {
        r: 0xc3,
        g: 0x97,
        b: 0xd8,
    }, // bright magenta
    Rgb {
        r: 0x70,
        g: 0xc0,
        b: 0xb1,
    }, // bright cyan
    Rgb {
        r: 0xf4,
        g: 0xf5,
        b: 0xf5,
    }, // bright white
];

/// Six intensities along each axis of the ANSI color cube.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Convert an ANSI color to RGB. Unspecified foreground and background use the UI theme.
/// OSC 4, 10, and 11 colors override the standard palette when provided.
fn resolve_color(color: Color, dyn_colors: &DynColors, is_fg: bool) -> Option<u32> {
    let rgb = match color {
        Color::Named(named) => {
            if let Some(rgb) = dyn_colors[named] {
                return Some(rgb_to_u32(rgb));
            }
            match named {
                NamedColor::Foreground
                | NamedColor::BrightForeground
                | NamedColor::DimForeground
                    if is_fg =>
                {
                    return None;
                }
                NamedColor::Background if !is_fg => return None,
                NamedColor::Black | NamedColor::DimBlack | NamedColor::Background => BASE16[0],
                NamedColor::Red | NamedColor::DimRed => BASE16[1],
                NamedColor::Green | NamedColor::DimGreen => BASE16[2],
                NamedColor::Yellow | NamedColor::DimYellow => BASE16[3],
                NamedColor::Blue | NamedColor::DimBlue => BASE16[4],
                NamedColor::Magenta | NamedColor::DimMagenta => BASE16[5],
                NamedColor::Cyan | NamedColor::DimCyan => BASE16[6],
                NamedColor::Cursor
                | NamedColor::White
                | NamedColor::DimWhite
                | NamedColor::Foreground
                | NamedColor::BrightForeground
                | NamedColor::DimForeground => BASE16[7],
                NamedColor::BrightBlack => BASE16[8],
                NamedColor::BrightRed => BASE16[9],
                NamedColor::BrightGreen => BASE16[10],
                NamedColor::BrightYellow => BASE16[11],
                NamedColor::BrightBlue => BASE16[12],
                NamedColor::BrightMagenta => BASE16[13],
                NamedColor::BrightCyan => BASE16[14],
                NamedColor::BrightWhite => BASE16[15],
            }
        }
        Color::Spec(rgb) => rgb,
        Color::Indexed(idx) => {
            if let Some(rgb) = dyn_colors[usize::from(idx)] {
                return Some(rgb_to_u32(rgb));
            }
            match idx {
                0..=15 => BASE16[usize::from(idx)],
                16..=231 => {
                    let i = usize::from(idx) - 16;
                    Rgb {
                        r: CUBE_LEVELS[i / 36],
                        g: CUBE_LEVELS[(i % 36) / 6],
                        b: CUBE_LEVELS[i % 6],
                    }
                }
                232..=255 => {
                    let level = 8 + (idx - 232) * 10;
                    Rgb {
                        r: level,
                        g: level,
                        b: level,
                    }
                }
            }
        }
    };
    Some(rgb_to_u32(rgb))
}

fn rgb_to_u32(rgb: Rgb) -> u32 {
    (u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b)
}

/// Keep the visible grid and cap the amount of history in the snapshot.
/// Use the first explicit cell color on each row and normally trim trailing blanks.
/// The cursor is a row and character offset, or is absent when hidden.
/// Keep trailing blanks up to the cursor when it lies beyond the printed text.
/// This preserves the visible effect of echoed spaces and erasures.
#[tracing::instrument(name = "serial.snapshot", skip_all)]
pub fn snapshot_screen(term: &Term<VoidListener>) -> ScreenSnapshot {
    snapshot_screen_scrollback(term, SNAPSHOT_MAX_ROWS)
}

/// Build a snapshot with a caller-supplied row limit.
#[tracing::instrument(name = "serial.snapshot", skip_all)]
pub fn snapshot_screen_scrollback(term: &Term<VoidListener>, max_rows: usize) -> ScreenSnapshot {
    let grid = term.grid();
    let bottommost = grid.bottommost_line().0;
    let topmost = grid.topmost_line().0;
    let max_rows = i32::try_from(max_rows).unwrap_or(i32::MAX);
    let start = bottommost
        .saturating_add(1)
        .saturating_sub(max_rows)
        .max(topmost);
    snapshot_screen_range(term, start..=bottommost, true)
}

/// Format a rendered VT100 [`ScreenSnapshot`] as plain text for Agent screen inspection.
///
/// Trims trailing horizontal whitespace from each row and removes trailing blank lines
/// while preserving internal vertical spacing.
#[must_use]
pub fn render_screen_text(screen: &ScreenSnapshot) -> String {
    let mut lines: Vec<&str> = screen.rows.iter().map(|row| row.text.trim_end()).collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[derive(Debug)]
pub struct TerminalHistoryPage {
    pub screen: ScreenSnapshot,
    pub skip: usize,
    pub earlier: usize,
}

/// Read a bounded page ending `skip` grid rows before the live screen tail.
#[cfg(any(feature = "real-hardware", test))]
pub(crate) fn snapshot_terminal_history(
    term: &Term<VoidListener>,
    skip: usize,
) -> TerminalHistoryPage {
    let grid = term.grid();
    let top = grid.topmost_line().0;
    let bottom = grid.bottommost_line().0;
    let total_span = usize::try_from(bottom - top).unwrap_or(0);
    if skip > total_span {
        return TerminalHistoryPage {
            screen: ScreenSnapshot::default(),
            skip,
            earlier: 0,
        };
    }
    let skip_i32 = i32::try_from(skip).unwrap_or(i32::MAX);
    let end = bottom - skip_i32;
    let page_rows = i32::try_from(HISTORY_PAGE_ROWS).unwrap_or(i32::MAX);
    let start = (end + 1 - page_rows).max(top);
    TerminalHistoryPage {
        screen: snapshot_screen_range(term, start..=end, false),
        skip,
        earlier: usize::try_from(start - top).unwrap_or(0),
    }
}

fn snapshot_screen_range(
    term: &Term<VoidListener>,
    range: std::ops::RangeInclusive<i32>,
    live: bool,
) -> ScreenSnapshot {
    let grid = term.grid();
    let cols = grid.columns();
    let content = term.renderable_content();
    let cursor = content.cursor.point;
    let cursor_hidden = !live
        || matches!(
            content.cursor.shape,
            alacritty_terminal::vte::ansi::CursorShape::Hidden
        );

    let expected_rows = usize::try_from((*range.end() - *range.start() + 1).max(0)).unwrap_or(0);
    let mut rows: Vec<ScreenRow> = Vec::with_capacity(expected_rows);
    let mut cursor_row: Option<u32> = None;
    let mut cursor_char: Option<u32> = None;
    for line in range {
        let row = &grid[Line(line)];
        let row_idx = u32::try_from(rows.len()).unwrap_or(u32::MAX);
        let mut cursor_here = !cursor_hidden && line == cursor.line.0;
        let mut text = String::with_capacity(cols);
        let mut fg: Option<u32> = None;
        let mut bg: Option<u32> = None;
        let mut char_count: u32 = 0;
        let mut trimmed_bytes: usize = 0;
        let mut trimmed_chars: u32 = 0;
        for col in 0..cols {
            let cell = &row[Column(col)];
            if cursor_here && col == cursor.column.0 {
                cursor_char = Some(char_count);
                cursor_row = Some(row_idx);
                cursor_here = false;
            }
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let inverse = cell.flags.contains(Flags::INVERSE);
            if fg.is_none() && cell.c != ' ' {
                fg = if inverse {
                    resolve_color(cell.bg, content.colors, false)
                } else {
                    resolve_color(cell.fg, content.colors, true)
                };
            }
            if bg.is_none()
                && cell.c != ' '
                && !matches!(cell.bg, Color::Named(NamedColor::Background))
            {
                bg = if inverse {
                    resolve_color(cell.fg, content.colors, true)
                } else {
                    resolve_color(cell.bg, content.colors, false)
                };
            }
            text.push(cell.c);
            char_count += 1;
            if !cell.c.is_whitespace() {
                trimmed_bytes = text.len();
                trimmed_chars = char_count;
            }
        }
        if trimmed_bytes < text.len() {
            let tail_cursor = if cursor_row == Some(row_idx) {
                cursor_char
            } else {
                None
            };
            text.truncate(trimmed_bytes);
            // Preserve trailing spaces up to the cursor column so space/backspace
            // echoes remain visible across frames.
            if let Some(cc) = tail_cursor
                && cc > trimmed_chars
            {
                for _ in 0..(cc - trimmed_chars) {
                    text.push(' ');
                }
            }
        }
        // Skip leading blank history rows in live snapshots.
        if live && rows.is_empty() && text.is_empty() {
            continue;
        }
        rows.push(ScreenRow { text, fg, bg });
    }
    ScreenSnapshot {
        rows,
        cursor_row,
        cursor_char,
        application_cursor: term.mode().contains(TermMode::APP_CURSOR),
        bracketed_paste: term.mode().contains(TermMode::BRACKETED_PASTE),
        // Filled by `SessionCtx::emit_screen` from the raw-byte meter; the
        // snapshot builder itself knows nothing about garbled detection.
        garbled: duthub_core::ipc::GarbledStats::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    fn feed(term: &mut Term<VoidListener>, bytes: &[u8]) {
        let mut parser = Processor::<StdSyncHandler>::new();
        parser.advance(term, bytes);
    }

    #[test]
    fn history_pages_reach_rows_older_than_the_live_frame() {
        let mut term = create_term_with_scrollback(SCROLLBACK_HISTORY);
        let mut parser = Processor::<StdSyncHandler>::new();
        for line in 0..(2 * HISTORY_PAGE_ROWS + 17) {
            parser.advance(&mut term, format!("{line:05}\r\n").as_bytes());
        }

        let live = snapshot_screen_scrollback(&term, HISTORY_PAGE_ROWS);
        assert_eq!(live.rows.len(), HISTORY_PAGE_ROWS);
        assert!(!live.rows.iter().any(|row| row.text == "00000"));

        let first = snapshot_terminal_history(&term, live.rows.len());
        assert_eq!(first.screen.rows.len(), HISTORY_PAGE_ROWS);
        assert!(first.earlier > 0);
        assert_eq!(first.screen.cursor_row, None);

        let oldest = snapshot_terminal_history(&term, first.skip + first.screen.rows.len());
        assert_eq!(oldest.screen.rows[0].text, "00000");
        assert_eq!(oldest.earlier, 0);
        assert_eq!(
            snapshot_terminal_history(&term, SCROLLBACK_HISTORY)
                .screen
                .rows,
            [] as [duthub_core::ipc::ScreenRow; 0]
        );
    }

    #[test]
    fn serial_prompt_replay_keeps_spaces_and_erases_at_the_cursor() {
        let mut terminal = create_term_with_scrollback(SCROLLBACK_HISTORY);
        let mut parser = Processor::<StdSyncHandler>::new();
        for _ in 0..(HISTORY_PAGE_ROWS + 32) {
            parser.advance(&mut terminal, b"previous serial output\r\n");
        }
        // COM31 emitted the prompt with bracketed-paste mode changing across RX chunks.
        for chunk in [
            b"\r\n\x1b[?2004l\r".as_slice(),
            b"\x1b[?2004hroot@fixture:~# ",
            b"ab",
        ] {
            parser.advance(&mut terminal, chunk);
        }
        for (key_echo, expected) in [
            (b" ".as_slice(), "root@fixture:~# ab "),
            (b"\x08 \x08".as_slice(), "root@fixture:~# ab"),
        ] {
            parser.advance(&mut terminal, key_echo);
            let frame = snapshot_screen_scrollback(&terminal, LIVE_SNAPSHOT_ROW_CAP);
            assert_eq!(frame.rows.len(), LIVE_SNAPSHOT_ROW_CAP);
            assert_eq!(
                frame.rows[frame.cursor_row.unwrap() as usize].text,
                expected
            );
            assert_eq!(
                frame.cursor_char,
                Some(u32::try_from(expected.len()).unwrap())
            );
            assert!(frame.bracketed_paste);
        }
    }

    #[test]
    #[ignore = "manual comparison of snapshot budgets on the local machine"]
    fn measure_snapshot_cost_with_long_scrollback() {
        let mut terminal = create_term_with_scrollback(SCROLLBACK_HISTORY);
        let mut parser = Processor::<StdSyncHandler>::new();
        for line in 0..10_000 {
            parser.advance(
                &mut terminal,
                format!("serial output {line:05}\r\n").as_bytes(),
            );
        }
        for rows in [HISTORY_PAGE_ROWS, LIVE_SNAPSHOT_ROW_CAP] {
            let mut samples: Vec<_> = (0..64)
                .map(|_| {
                    let started = Instant::now();
                    std::hint::black_box(snapshot_screen_scrollback(
                        std::hint::black_box(&terminal),
                        rows,
                    ));
                    started.elapsed()
                })
                .collect();
            samples.sort_unstable();
            println!(
                "{rows} rows: median {:.2} ms, p95 {:.2} ms",
                samples[32].as_secs_f64() * 1000.0,
                samples[60].as_secs_f64() * 1000.0
            );
        }
    }

    #[test]
    fn reset_recovers_from_fullscreen_state_and_an_unfinished_escape_sequence() {
        let mut term = create_term_with_grid(TermGrid::new(82, 19));
        let mut parser = Processor::<StdSyncHandler>::new();
        parser.advance(
            &mut term,
            b"old shell\r\n\x1b[?1049h\x1b[?25l\x1b[?1h\x1b[?2004h\x1b[31mtop",
        );
        assert!(term.mode().contains(TermMode::ALT_SCREEN));
        let before = snapshot_screen(&term);
        assert!(before.application_cursor && before.bracketed_paste);
        assert!(before.cursor_row.is_none());
        parser.advance(&mut term, b"\x1b]0;unfinished title");

        reset_terminal(&mut term, &mut parser);
        assert_eq!((term.columns(), term.screen_lines()), (82, 19));
        assert!(!term.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(
            snapshot_screen(&term).rows,
            [] as [duthub_core::ipc::ScreenRow; 0]
        );

        parser.advance(&mut term, b"fresh shell");
        let after = snapshot_screen(&term);
        assert!(!after.application_cursor && !after.bracketed_paste);
        assert_eq!(after.rows[0].text, "fresh shell");
        assert_eq!(after.rows[0].fg, None);
        assert_eq!(after.cursor_row, Some(0));
        assert_eq!(after.cursor_char, Some(11));
    }

    #[test]
    fn synchronized_output_recovers_when_the_end_sequence_is_missing() {
        let mut term = create_term();
        let mut parser = Processor::<StdSyncHandler>::new();
        parser.advance(&mut term, b"first\r\n\x1b[?2026hbuffered");
        let deadline = parser.sync_timeout().sync_timeout().unwrap();
        assert!(!flush_expired_sync(
            &mut term,
            &mut parser,
            deadline
                .checked_sub(std::time::Duration::from_millis(1))
                .unwrap(),
        ));
        assert!(
            snapshot_screen(&term)
                .rows
                .iter()
                .all(|row| row.text != "buffered")
        );

        assert!(flush_expired_sync(&mut term, &mut parser, deadline));
        assert!(!flush_expired_sync(&mut term, &mut parser, deadline));
        parser.advance(&mut term, b"\r\nnext");
        let snapshot = snapshot_screen(&term);
        let lines: Vec<_> = snapshot.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(&lines[..3], ["first", "buffered", "next"]);
    }

    #[test]
    fn snapshots_include_terminal_input_modes() {
        let mut term = create_term();
        feed(&mut term, b"\x1b[?1h\x1b[?2004h");
        let screen = snapshot_screen(&term);
        assert!(screen.application_cursor && screen.bracketed_paste);
        feed(&mut term, b"\x1b[?1l\x1b[?2004l");
        let screen = snapshot_screen(&term);
        assert!(!screen.application_cursor && !screen.bracketed_paste);
    }

    /// Feed a raw serial byte stream through the ANSI processor.
    /// The grid should contain device echo and interpreted ANSI formatting.
    #[test]
    fn term_simulator_renders_remote_echo_and_sgr_colors() {
        let mut term = create_term();
        // Simulate shell echo with CRLF, red text, and cursor positioning.
        feed(
            &mut term,
            b"root@rk3576:~# echo hi\r\nhi\r\n\x1b[31mred\x1b[0m",
        );
        let snap = snapshot_screen(&term);
        assert!(
            snap.rows
                .iter()
                .any(|r| r.text.contains("root@rk3576:~# echo hi")),
            "echoed line must be present in grid: {:?}",
            snap.rows
        );
        assert!(snap.rows.iter().any(|r| r.text.contains("hi")));
        let red = snap
            .rows
            .iter()
            .find(|r| r.text.contains("red"))
            .expect("ANSI output row should exist");
        assert_eq!(
            red.fg,
            Some(0x00cc_6666),
            "SGR 31 must map to red foreground"
        );
        assert!(
            snap.cursor_row.is_some() && snap.cursor_char.is_some(),
            "cursor must be positioned at prompt"
        );
    }

    /// The emulator handles cursor positioning and line or display erasure.
    /// ED2 moves cleared content into scrollback.
    /// Check the new visible text and cursor without expecting the old history to vanish.
    #[test]
    fn term_simulator_handles_clear_and_cursor_positioning() {
        let mut term = create_term();
        feed(&mut term, b"line-one\r\nline-two\r\n\x1b[2J\x1b[Hfresh");
        let snap = snapshot_screen(&term);
        let fresh_idx = snap
            .rows
            .iter()
            .position(|r| r.text.contains("fresh"))
            .expect("new text must be visible after clear");
        assert_eq!(snap.cursor_row, Some(u32::try_from(fresh_idx).unwrap()));
        assert_eq!(snap.cursor_char, Some(5), "cursor should follow \"fresh\"");
    }

    /// When the viewport fills, retain older rows in the snapshot's bounded history.
    #[test]
    fn term_simulator_keeps_scrollback_history() {
        let mut term = create_term();
        let mut stream = Vec::new();
        for i in 0..DEFAULT_ROWS + 20 {
            stream.extend_from_slice(format!("row-{i:04}\r\n").as_bytes());
        }
        feed(&mut term, &stream);
        let snap = snapshot_screen(&term);
        // The snapshot contains scrollback rows as well as the current viewport.
        assert!(
            snap.rows.len() > DEFAULT_ROWS,
            "snapshot must include scrollback history rows"
        );
        assert!(
            snap.rows.iter().any(|r| r.text.contains("row-0000")),
            "earliest row must remain within history limit: {:?}",
            snap.rows.first()
        );
        assert!(
            snap.rows
                .iter()
                .any(|r| r.text.contains(&format!("row-{:04}", DEFAULT_ROWS + 19))),
            "latest row must be present"
        );
    }

    /// Verify indexed and true-color RGB on separate rows.
    #[test]
    fn indexed_and_spec_colors_resolve() {
        let mut term = create_term();
        // Use color index 196 for red and an explicit RGB value for the second row.
        feed(
            &mut term,
            b"\x1b[38;5;196mA\x1b[0m\r\n\x1b[38;2;18;255;52mB",
        );
        let snap = snapshot_screen(&term);
        let row = snap.rows.iter().find(|r| r.text.contains('A')).unwrap();
        assert_eq!(row.fg, Some(0x00ff_0000), "indexed 196 -> color cube red");
        let row = snap.rows.iter().find(|r| r.text.contains('B')).unwrap();
        assert_eq!(row.fg, Some(0x0012_ff34), "SGR 38;2 direct RGB");
    }

    /// Omit the cursor from the snapshot when the device hides it.
    #[test]
    fn hidden_cursor_is_absent() {
        let mut term = create_term();
        feed(&mut term, b"\x1b[?25lhide");
        let snap = snapshot_screen(&term);
        assert!(
            snap.cursor_row.is_none(),
            "hidden cursor must not report coordinates"
        );
        assert!(snap.rows.iter().any(|r| r.text.contains("hide")));
    }

    /// Preserve an echoed space when the cursor moves into trailing blanks.
    /// Otherwise two successive snapshots could be identical.
    #[test]
    fn trailing_space_before_cursor_stays_in_snapshot() {
        let mut term = create_term();
        feed(&mut term, b"abc");
        feed(&mut term, b" "); // Device echoes space and advances cursor.
        let snap = snapshot_screen(&term);
        assert_eq!(snap.rows[0].text, "abc ", "trailing space preserved");
        assert_eq!(
            snap.cursor_char,
            Some(4),
            "cursor advances past trailing space"
        );
    }

    /// A backspace-space-backspace sequence erases the tail character.
    /// Trim the erased cell's text while leaving the cursor at that position.
    #[test]
    fn backspace_echo_keeps_cursor_on_erased_cell() {
        let mut term = create_term();
        feed(&mut term, b"abc\x08 \x08");
        let snap = snapshot_screen(&term);
        assert_eq!(snap.rows[0].text, "ab");
        assert_eq!(snap.cursor_char, Some(2));
    }

    /// Trim trailing blanks on rows without the cursor or with the cursor inside the text.
    #[test]
    fn rows_without_cursor_tail_are_trimmed() {
        let mut term = create_term();
        feed(&mut term, b"line1  \r\nabc  \x1b[2D");
        let snap = snapshot_screen(&term);
        assert_eq!(
            snap.rows[0].text, "line1",
            "non-cursor row trims trailing blanks"
        );
        assert_eq!(snap.rows[1].text, "abc");
        assert_eq!(
            snap.cursor_char,
            Some(3),
            "mid-line cursor offset unchanged"
        );
    }

    /// Count characters when deciding how much whitespace to keep on a multibyte row.
    /// A byte offset cannot be compared directly with the cursor's character offset.
    #[test]
    fn multibyte_tail_boundary_uses_char_offsets() {
        let mut term = create_term();
        feed(&mut term, "中文".as_bytes());
        feed(&mut term, b" ");
        let snap = snapshot_screen(&term);
        assert_eq!(snap.rows[0].text, "中文 ");
        assert_eq!(
            snap.cursor_char,
            Some(3),
            "wide chars count as 1 char per offset"
        );
    }

    // Snapshots from raw serial receive bytes

    /// Each new terminal row must originate from bytes received from the device.
    /// An empty stream must not create a welcome line or prompt.
    /// A device prompt appears on its own when its bytes arrive.
    /// Local TX annotations remain in the log and never enter the terminal grid.
    #[test]
    fn snapshot_carries_only_device_bytes_without_local_fabrication() {
        let mut term = create_term();
        assert!(
            snapshot_screen(&term).rows.is_empty(),
            "snapshot must be empty before the device sends any bytes"
        );
        // Feed only a device prompt with the ANSI green color and trailing space.
        // The snapshot must contain that prompt without extra local lines.
        feed(&mut term, b"\x1b[32mroot@board:~#\x1b[0m ");
        let snap = snapshot_screen(&term);
        let texts: Vec<&str> = snap
            .rows
            .iter()
            .map(|r| r.text.as_str())
            .filter(|t| !t.is_empty())
            .collect();
        assert_eq!(
            &texts[..],
            ["root@board:~# "],
            "snapshot must carry only device bytes (trailing space kept at cursor): {:?}",
            snap.rows
        );
        assert_eq!(
            snap.rows[0].fg,
            Some(0x00b5_bd68),
            "SGR 32 -> palette green"
        );
        assert_eq!(snap.cursor_row, Some(0));
        assert_eq!(
            snap.cursor_char,
            Some(14),
            "cursor stops after prompt space"
        );
    }

    /// Simulate hardware reads split into several byte chunks.
    /// Each snapshot shows device echo, prompts, ANSI colors, and cursor position.
    /// Feed the prompt, individual echoed keystrokes, and command output in sequence.
    /// Each frame reflects the bytes already received.
    #[test]
    fn device_echo_prompt_color_and_cursor_are_carried_by_simulator() {
        let mut term = create_term();
        // Feed the green prompt from the device.
        feed(&mut term, b"\x1b[32mroot@rk3576:~#\x1b[0m ");
        let snap = snapshot_screen(&term);
        assert_eq!(snap.rows[0].text, "root@rk3576:~# ");
        assert_eq!(
            snap.rows[0].fg,
            Some(0x00b5_bd68),
            "prompt SGR 32 -> palette green"
        );
        assert_eq!(snap.cursor_row, Some(0));
        assert_eq!(
            snap.cursor_char,
            Some(15),
            "cursor stops after prompt space"
        );

        // Feed the two echoed keystrokes as separate reads.
        feed(&mut term, b"l");
        let snap = snapshot_screen(&term);
        assert_eq!(snap.rows[0].text, "root@rk3576:~# l");
        feed(&mut term, b"s");
        let snap = snapshot_screen(&term);
        assert_eq!(snap.rows[0].text, "root@rk3576:~# ls");
        assert_eq!(
            snap.cursor_char,
            Some(17),
            "cursor advances with device echo"
        );

        // Feed red command output and the next prompt while keeping the command line in the grid.
        feed(
            &mut term,
            b"\r\n\x1b[31mERR: not found\x1b[0m\r\n\x1b[32mroot@rk3576:~#\x1b[0m ",
        );
        let snap = snapshot_screen(&term);
        let texts: Vec<&str> = snap
            .rows
            .iter()
            .map(|r| r.text.as_str())
            .filter(|t| !t.is_empty())
            .collect();
        assert_eq!(
            &texts[..],
            ["root@rk3576:~# ls", "ERR: not found", "root@rk3576:~# "],
            "echo row, output row, and new prompt must appear in order: {:?}",
            snap.rows
        );
        assert_eq!(snap.rows[1].fg, Some(0x00cc_6666), "SGR 31 -> palette red");
        let cursor_row = snap
            .cursor_row
            .expect("cursor must be visible at input prompt") as usize;
        assert!(
            snap.rows[cursor_row].text.contains("root@rk3576:~#"),
            "cursor row must be the prompt row in the snapshot: {:?}",
            snap.rows[cursor_row]
        );
        assert_eq!(
            snap.cursor_char,
            Some(15),
            "cursor waits for input after prompt"
        );
    }

    // Cursor positions in trailing whitespace

    /// An echoed space or erase should change the terminal snapshot.
    /// The device echo is the only visible confirmation that the keystroke took effect.
    /// Retain an echoed space up to the new cursor position.
    /// An erase trims the text and leaves the cursor in the erased cell.
    #[test]
    fn space_and_backspace_keystroke_frames_differ_in_snapshot() {
        let mut term = create_term();
        feed(&mut term, b"abc");
        let before_space = snapshot_screen(&term);
        assert_eq!(before_space.rows[0].text, "abc");
        assert_eq!(before_space.cursor_char, Some(3));
        feed(&mut term, b" "); // Device echoes space and advances cursor.
        let after_space = snapshot_screen(&term);
        assert_eq!(
            after_space.rows[0].text, "abc ",
            "trailing space must be kept up to cursor column"
        );
        assert_eq!(after_space.cursor_char, Some(4));
        assert_ne!(
            before_space.rows[0].text, after_space.rows[0].text,
            "frames before and after space must differ"
        );
        assert_ne!(before_space.cursor_char, after_space.cursor_char);

        // Erase the trailing space, type a visible letter into that cell,
        // then erase the letter with the same backspace sequence.
        feed(&mut term, b"\x08 \x08");
        feed(&mut term, b"d");
        let before_backspace = snapshot_screen(&term);
        assert_eq!(before_backspace.rows[0].text, "abcd");
        feed(&mut term, b"\x08 \x08");
        let after_backspace = snapshot_screen(&term);
        assert_eq!(after_backspace.rows[0].text, "abc");
        assert_eq!(
            after_backspace.cursor_char,
            Some(3),
            "cursor stays on erased cell"
        );
        assert_ne!(
            before_backspace.rows[0].text, after_backspace.rows[0].text,
            "frames before and after backspace must differ"
        );
    }

    /// When the cursor sits inside trailing blanks, retain the blanks up to its column.
    /// Continue trimming the rest of the row.
    /// Both the number of retained blanks and the cursor position affect the visible frame.
    #[test]
    fn cursor_inside_trailing_whitespace_keeps_up_to_cursor_and_trims_beyond() {
        let mut term = create_term();
        feed(&mut term, b"ab    "); // Device echoes 4 spaces; cursor advances to end of line.
        feed(&mut term, b"\x1b[2D"); // Cursor moves left 2 columns into trailing whitespace.
        let snap = snapshot_screen(&term);
        assert_eq!(
            snap.rows[0].text, "ab  ",
            "keep blanks up to cursor column and trim beyond"
        );
        assert_eq!(
            snap.cursor_char,
            Some(4),
            "cursor rests inside trailing blanks"
        );
    }

    /// Apply cursor-dependent whitespace preservation to the cursor row alone.
    /// Other rows in the same snapshot continue to trim trailing blanks.
    #[test]
    fn only_cursor_row_keeps_trailing_whitespace() {
        let mut term = create_term();
        feed(&mut term, b"aa  \r\nbb  "); // Both rows have trailing blanks; cursor is at end of row 2.
        let snap = snapshot_screen(&term);
        assert_eq!(snap.rows[0].text, "aa", "non-cursor row remains trimmed");
        assert_eq!(
            snap.rows[1].text, "bb  ",
            "cursor row preserves blanks up to cursor column"
        );
        assert_eq!(snap.cursor_row, Some(1));
        assert_eq!(snap.cursor_char, Some(4));
    }

    /// Count characters on a multibyte line where the cursor has advanced into blanks.
    /// Using the UTF-8 byte length would misclassify the cursor's character offset.
    /// That would incorrectly trim every trailing blank.
    #[test]
    fn wide_char_row_cursor_deep_in_whitespace_uses_char_offsets() {
        let mut term = create_term();
        feed(&mut term, "中文".as_bytes());
        feed(&mut term, b"\x1b[2C"); // Cursor advances into blanks without echoing spaces.
        let snap = snapshot_screen(&term);
        assert_eq!(
            snap.rows[0].text, "中文  ",
            "whitespace boundary uses char count (offset 4, not byte offset 8)"
        );
        assert_eq!(
            snap.cursor_char,
            Some(4),
            "wide chars count as 1 char per offset"
        );
    }
}
