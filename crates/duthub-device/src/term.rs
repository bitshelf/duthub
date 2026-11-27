//! 终端仿真网格（PuTTY `模式，alacritty_terminal` 0.26）。
//!
//! - real 会话持 `Term` + `vte::ansi::Processor`：串口原始字节直接
//!   `processor.advance(&mut term, bytes)`——设备 shell 自己负责回显/提示符/
//!   光标（无本地 [TX] 伪造行，与 `PuTTY` 行为一致）。
//! - UI 渲染从 Term 网格提取：[`snapshot_screen`] 把可见区 + 适度回滚历史
//!   转成文本行 + 前景/背景色（`ScreenSnapshot`），经总线整体替换到 UI。
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

/// 快照携带的最大行数（屏幕 + 回滚历史合计；超过只保留最近行）。
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
/// 新会话的初始网格（打开后 UI 会按视口尺寸下发 Resize）。
pub const DEFAULT_COLS: usize = 100;
pub const DEFAULT_ROWS: usize = 30;

/// [`Term::new`] / [`Term::resize`] 的尺寸描述（alacritty 的 `Dimensions`）。
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

/// 新建 real 会话的仿真终端（VoidListener：驱动事件全部丢弃）。
#[must_use]
pub fn create_term() -> Term<VoidListener> {
    create_term_with_scrollback(SCROLLBACK_HISTORY)
}

/// 按设置的回滚行数建终端（「串口」设置段；下限 100，默认 [`SCROLLBACK_HISTORY`]）。
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

/// 在同步更新结束序列丢失时，按 VTE 的时限提交已收到的画面。
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

/// xterm-256 标准调色板的前 16 色（暗色主题友好的常规取值）。
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

/// Indexed(16..231) 的 6×6×6 色立方层级。
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// ANSI 色值 → 0xRRGGBB；默认前景/背景返回 `None`（UI 用主题默认）。
/// `dyn_colors` 是 OSC 4/10/11 动态覆写（未覆写为 `None` 时走标准调色板）。
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

/// 从 Term 网格提取快照：最近 [`SNAPSHOT_MAX_ROWS`] 行（屏幕行全保留，历史行
/// 按需截断），行文本去除尾部空白；前景/背景取行内第一个有色单元格。
/// 光标映射为（快照行索引, 行内字符偏移）；光标隐藏（DECSET 25）时为 `None`。
/// 例外：光标落在尾随空白区时保留空白至光标列（否则键入空格/退格的回显在
/// 前后两帧快照间完全一致，UI 上表现为"按键无效果"）。
#[tracing::instrument(name = "serial.snapshot", skip_all)]
pub fn snapshot_screen(term: &Term<VoidListener>) -> ScreenSnapshot {
    snapshot_screen_scrollback(term, SNAPSHOT_MAX_ROWS)
}

/// 快照行数上限可配置版（「串口」设置段滚动缓冲；决定 UI 可回看的历史行数）。
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
                .rows, [] as [duthub_core::ipc::ScreenRow; 0]
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
            assert_eq!(frame.cursor_char, Some(u32::try_from(expected.len()).unwrap()));
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
        assert_eq!(snapshot_screen(&term).rows, [] as [duthub_core::ipc::ScreenRow; 0]);

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
            deadline.checked_sub(std::time::Duration::from_millis(1)).unwrap(),
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

    /// `终端仿真重构（alacritty_terminal）单元级验证：字节流进` Processor →
    /// Term 网格承载设备侧回显与 ANSI（代替旧 `LineAssembler` 行重组测试）。
    #[test]
    fn term_simulator_renders_remote_echo_and_sgr_colors() {
        let mut term = create_term();
        // 模拟设备 shell 回显（含 CRLF）+ SGR 红色 + 光标定位
        feed(
            &mut term,
            b"root@rk3576:~# echo hi\r\nhi\r\n\x1b[31mred\x1b[0m",
        );
        let snap = snapshot_screen(&term);
        assert!(
            snap.rows
                .iter()
                .any(|r| r.text.contains("root@rk3576:~# echo hi")),
            "回显行必须在网格里：{:?}",
            snap.rows
        );
        assert!(snap.rows.iter().any(|r| r.text.contains("hi")));
        let red = snap
            .rows
            .iter()
            .find(|r| r.text.contains("red"))
            .expect("ANSI 输出行存在");
        assert_eq!(red.fg, Some(0x00cc_6666), "SGR 31 必须映射为红色前景");
        assert!(
            snap.cursor_row.is_some() && snap.cursor_char.is_some(),
            "光标必须定位（提示符在等输入）"
        );
    }

    /// 光标定位（CUP）与擦除（ED/EL）由仿真器处理：清屏后新内容在提示位置。
    /// 注：alacritty 0.26 的 ED2 会把被清内容卷入回滚历史（与真实终端一致），
    /// 所以断言落在"新内容 + 光标位置"，而不是"旧内容从快照消失"。
    #[test]
    fn term_simulator_handles_clear_and_cursor_positioning() {
        let mut term = create_term();
        feed(&mut term, b"line-one\r\nline-two\r\n\x1b[2J\x1b[Hfresh");
        let snap = snapshot_screen(&term);
        let fresh_idx = snap
            .rows
            .iter()
            .position(|r| r.text.contains("fresh"))
            .expect("清屏后新内容必须可见");
        assert_eq!(snap.cursor_row, Some(u32::try_from(fresh_idx).unwrap()));
        assert_eq!(snap.cursor_char, Some(5), "光标在 fresh 之后");
    }

    /// 回滚历史：行数超过屏幕高度后旧行进 history，快照按上限携带。
    #[test]
    fn term_simulator_keeps_scrollback_history() {
        let mut term = create_term();
        let mut stream = Vec::new();
        for i in 0..DEFAULT_ROWS + 20 {
            stream.extend_from_slice(format!("row-{i:04}\r\n").as_bytes());
        }
        feed(&mut term, &stream);
        let snap = snapshot_screen(&term);
        // 屏幕外的历史行也在快照里（最多 SNAPSHOT_MAX_ROWS）
        assert!(snap.rows.len() > DEFAULT_ROWS, "快照必须含回滚历史行");
        assert!(
            snap.rows.iter().any(|r| r.text.contains("row-0000")),
            "最早的行必须仍在（历史上限内）：{:?}",
            snap.rows.first()
        );
        assert!(
            snap.rows
                .iter()
                .any(|r| r.text.contains(&format!("row-{:04}", DEFAULT_ROWS + 19))),
            "最新行必须在"
        );
    }

    /// 256 色与 RGB 直写色都映射为 0xRRGGBB（分两行避免行锚色互相覆盖）。
    #[test]
    fn indexed_and_spec_colors_resolve() {
        let mut term = create_term();
        // 256 色板：indexed 196 = 立方红 (255,0,0)；spec 直写 #12ff34
        feed(
            &mut term,
            b"\x1b[38;5;196mA\x1b[0m\r\n\x1b[38;2;18;255;52mB",
        );
        let snap = snapshot_screen(&term);
        let row = snap.rows.iter().find(|r| r.text.contains('A')).unwrap();
        assert_eq!(row.fg, Some(0x00ff_0000), "indexed 196 → 立方红");
        let row = snap.rows.iter().find(|r| r.text.contains('B')).unwrap();
        assert_eq!(row.fg, Some(0x0012_ff34), "SGR 38;2 直写 RGB");
    }

    /// 光标隐藏（DECTCEM ?25）时快照不携带光标。
    #[test]
    fn hidden_cursor_is_absent() {
        let mut term = create_term();
        feed(&mut term, b"\x1b[?25lhide");
        let snap = snapshot_screen(&term);
        assert!(snap.cursor_row.is_none(), "光标隐藏时不得有光标坐标");
        assert!(snap.rows.iter().any(|r| r.text.contains("hide")));
    }

    /// 键入尾随空格的回显必须可见：光标落在尾随空白区时快照保留空白至
    /// 光标列，否则前后两帧快照逐字节相同，UI 表现为"按键无效果"。
    #[test]
    fn trailing_space_before_cursor_stays_in_snapshot() {
        let mut term = create_term();
        feed(&mut term, b"abc");
        feed(&mut term, b" "); // 设备回显：写入空格并推进光标
        let snap = snapshot_screen(&term);
        assert_eq!(snap.rows[0].text, "abc ", "尾随空格保留");
        assert_eq!(snap.cursor_char, Some(4), "光标推进到空格之后");
    }

    /// 退格（BS SP BS）擦除行尾字符后，光标停在原字符位置：文本裁剪到
    /// 非空白前缀，光标偏移不变（UI 在被擦除单元上画反显光标块）。
    #[test]
    fn backspace_echo_keeps_cursor_on_erased_cell() {
        let mut term = create_term();
        feed(&mut term, b"abc\x08 \x08");
        let snap = snapshot_screen(&term);
        assert_eq!(snap.rows[0].text, "ab");
        assert_eq!(snap.cursor_char, Some(2));
    }

    /// 无光标行维持裁剪；行中光标不受尾随裁剪影响。
    #[test]
    fn rows_without_cursor_tail_are_trimmed() {
        let mut term = create_term();
        feed(&mut term, b"line1  \r\nabc  \x1b[2D");
        let snap = snapshot_screen(&term);
        assert_eq!(snap.rows[0].text, "line1", "无光标行裁掉尾随空白");
        assert_eq!(snap.rows[1].text, "abc");
        assert_eq!(snap.cursor_char, Some(3), "行中光标偏移不变");
    }

    /// 多字节行尾：空白边界按字符数计，宽字符行尾 + 尾随空格 + 光标
    /// 偏移之间不再混用字节长度。
    #[test]
    fn multibyte_tail_boundary_uses_char_offsets() {
        let mut term = create_term();
        feed(&mut term, "中文".as_bytes());
        feed(&mut term, b" ");
        let snap = snapshot_screen(&term);
        assert_eq!(snap.rows[0].text, "中文 ");
        assert_eq!(snap.cursor_char, Some(3), "宽字符按 1 字符计偏移");
    }

    // ===== IO-1：串口 RX 原始字节 → vte Processor → snapshot_screen =====

    /// IO-1（本地不伪造）：快照里的每一行都必须来自设备发送的字节。
    /// 新会话设备什么都没发时快照必须为空（本地不得伪造欢迎语/提示符）；
    /// 只喂提示符字节时快照内容逐字节等于设备发送内容——TX 注记等本地
    /// 伪造行只进 .log，永不进终端流（AGENTS.md 坑 #22）。
    #[test]
    fn snapshot_carries_only_device_bytes_without_local_fabrication() {
        let mut term = create_term();
        assert!(
            snapshot_screen(&term).rows.is_empty(),
            "设备未发送任何字节时快照必须为空"
        );
        // 只喂设备侧提示符字节（SGR 绿 + 冒号结尾 + 尾随空格）：快照必须
        // 恰好等于发送内容，无任何本地补写的行。
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
            "快照只能携带设备发送的字节（尾随空格因光标在其上而保留）：{:?}",
            snap.rows
        );
        assert_eq!(snap.rows[0].fg, Some(0x00b5_bd68), "SGR 32 → 调色板绿");
        assert_eq!(snap.cursor_row, Some(0));
        assert_eq!(snap.cursor_char, Some(14), "光标停在提示符空格之后");
    }

    /// IO-1（主场景，模拟 real.rs 读循环分块到达）：设备侧回显行、提示符、
    /// SGR 颜色与光标位置全部由仿真器承载并出现在 `snapshot_screen` 的快照
    /// 行中，本地不伪造任何回显。逐帧喂入（提示符 → 逐字节回显键入 →
    /// 命令输出 + 新提示符），每帧快照必须如实反映已收字节。
    #[test]
    fn device_echo_prompt_color_and_cursor_are_carried_by_simulator() {
        let mut term = create_term();
        // 帧一：设备发送绿色提示符（真实串口依赖设备回显，本地不补 prompt）
        feed(&mut term, b"\x1b[32mroot@rk3576:~#\x1b[0m ");
        let snap = snapshot_screen(&term);
        assert_eq!(snap.rows[0].text, "root@rk3576:~# ");
        assert_eq!(snap.rows[0].fg, Some(0x00b5_bd68), "提示符 SGR 32 → 调色板绿");
        assert_eq!(snap.cursor_row, Some(0));
        assert_eq!(snap.cursor_char, Some(15), "光标停在提示符空格之后");

        // 帧二：键入逐字节回显（'l'、's' 各一次读循环，模拟分块 RX）
        feed(&mut term, b"l");
        let snap = snapshot_screen(&term);
        assert_eq!(snap.rows[0].text, "root@rk3576:~# l");
        feed(&mut term, b"s");
        let snap = snapshot_screen(&term);
        assert_eq!(snap.rows[0].text, "root@rk3576:~# ls");
        assert_eq!(snap.cursor_char, Some(17), "光标随设备回显推进");

        // 帧三：命令输出（红色）+ 新提示符；已回显的命令行必须保留在网格
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
            "回显行、输出行、新提示符必须按序出现在快照中：{:?}",
            snap.rows
        );
        assert_eq!(snap.rows[1].fg, Some(0x00cc_6666), "SGR 31 → 调色板红");
        let cursor_row = snap.cursor_row.expect("光标必须可见（提示符在等输入）") as usize;
        assert!(
            snap.rows[cursor_row].text.contains("root@rk3576:~#"),
            "光标行必须是快照中的提示符行：{:?}",
            snap.rows[cursor_row]
        );
        assert_eq!(snap.cursor_char, Some(15), "光标停在提示符之后等输入");
    }

    // ===== IO-2：尾随空白 × 光标（坑 #46「按键无效果」回归）=====

    /// IO-2（聚合不变量）：空格/退格这类「光标推进型」按键，前后两帧快照
    /// 不得逐字节相同——否则叠加无本地回显时，用户看到「按键无效果」。
    /// 空格回显：文本多出保留到光标列的尾随空格 + 光标推进；退格擦除
    /// （BS SP BS）：文本裁到被擦字符前，光标停在擦除单元。
    #[test]
    fn space_and_backspace_keystroke_frames_differ_in_snapshot() {
        let mut term = create_term();
        feed(&mut term, b"abc");
        let before_space = snapshot_screen(&term);
        assert_eq!(before_space.rows[0].text, "abc");
        assert_eq!(before_space.cursor_char, Some(3));
        feed(&mut term, b" "); // 设备回显空格并推进光标
        let after_space = snapshot_screen(&term);
        assert_eq!(after_space.rows[0].text, "abc ", "尾随空格必须保留到光标列");
        assert_eq!(after_space.cursor_char, Some(4));
        assert_ne!(
            before_space.rows[0].text, after_space.rows[0].text,
            "空格按键前后两帧文本不得逐字节相同"
        );
        assert_ne!(before_space.cursor_char, after_space.cursor_char);

        // 退格擦除可见字符：先 BS SP BS 擦掉刚键入的空格（光标停在擦除
        // 单元），键入 d 覆写擦除位 → "abcd"，再 BS SP BS 擦掉 d。
        feed(&mut term, b"\x08 \x08");
        feed(&mut term, b"d");
        let before_backspace = snapshot_screen(&term);
        assert_eq!(before_backspace.rows[0].text, "abcd");
        feed(&mut term, b"\x08 \x08");
        let after_backspace = snapshot_screen(&term);
        assert_eq!(after_backspace.rows[0].text, "abc");
        assert_eq!(after_backspace.cursor_char, Some(3), "光标停在擦除单元");
        assert_ne!(
            before_backspace.rows[0].text, after_backspace.rows[0].text,
            "退格按键前后两帧文本不得逐字节相同"
        );
    }

    /// IO-2：「保留空白至光标列」的双向精确性——光标落在尾随空白区中间
    /// 时，光标列之前（含）的空白保留，光标之后的空白仍裁剪；多保留或
    /// 少保留都会让空格/退格帧间对比失真。
    #[test]
    fn cursor_inside_trailing_whitespace_keeps_up_to_cursor_and_trims_beyond() {
        let mut term = create_term();
        feed(&mut term, b"ab    "); // 设备回显 4 个空格，光标推进到行尾
        feed(&mut term, b"\x1b[2D"); // 光标左移 2 列，落在空白区中间
        let snap = snapshot_screen(&term);
        assert_eq!(
            snap.rows[0].text, "ab  ",
            "保留空白至光标列，光标之后的空白仍裁剪"
        );
        assert_eq!(snap.cursor_char, Some(4), "光标停在空白区内");
    }

    /// IO-2：保留规则按行生效——只有光标所在行保留尾随空白，其余行一律
    /// 维持裁剪，多行画面下不得放宽整帧。
    #[test]
    fn only_cursor_row_keeps_trailing_whitespace() {
        let mut term = create_term();
        feed(&mut term, b"aa  \r\nbb  "); // 两行都带尾随空白，光标在第二行行尾
        let snap = snapshot_screen(&term);
        assert_eq!(snap.rows[0].text, "aa", "无光标行维持裁剪");
        assert_eq!(snap.rows[1].text, "bb  ", "光标所在行保留空白至光标列");
        assert_eq!(snap.cursor_row, Some(1));
        assert_eq!(snap.cursor_char, Some(4));
    }

    /// IO-2：多字节行 + 光标深入空白区——空白边界必须按字符数计。若实现
    /// 把字节长度（「中文」= 6 字节）与字符偏移混比，光标偏移 4 会被误判
    /// 为落在非空白区，尾随空白被整体裁掉（坑 #46 的多字节形态）。
    #[test]
    fn wide_char_row_cursor_deep_in_whitespace_uses_char_offsets() {
        let mut term = create_term();
        feed(&mut term, "中文".as_bytes());
        feed(&mut term, b"\x1b[2C"); // 光标推进到空白区深处（无回显写入）
        let snap = snapshot_screen(&term);
        assert_eq!(
            snap.rows[0].text, "中文  ",
            "空白边界按字符数计：保留至字符偏移 4（非字节偏移 8）"
        );
        assert_eq!(snap.cursor_char, Some(4), "宽字符按 1 字符计偏移");
    }
}
