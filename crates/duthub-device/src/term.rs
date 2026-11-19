//! 终端仿真网格（PuTTY 模式，alacritty_terminal 0.26）。
//!
//! - real 会话持 `Term` + `vte::ansi::Processor`：串口原始字节直接
//!   `processor.advance(&mut term, bytes)`——设备 shell 自己负责回显/提示符/
//!   光标（无本地 [TX] 伪造行，与 PuTTY 行为一致）。
//! - UI 渲染从 Term 网格提取：[`snapshot_screen`] 把可见区 + 适度回滚历史
//!   转成文本行 + 前景/背景色（`ScreenSnapshot`），经总线整体替换到 UI。
//! - 滚动上限：`scrolling_history = 1000`（适度上限，快照最多带
//!   [`SNAPSHOT_MAX_ROWS`] 行，避免大输出刷屏时快照体积失控）。

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
/// Term 回滚历史上限（适度：桌面应用内存可控，PuTTY 默认 2000 行）。
pub const SCROLLBACK_HISTORY: usize = 1000;
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
pub fn create_term() -> Term<VoidListener> {
    create_term_with_grid(TermGrid::new(DEFAULT_COLS, DEFAULT_ROWS))
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

/// 恢复初始屏幕和输入模式，同时丢弃解析器里未完成的控制序列。
pub fn reset_terminal(term: &mut Term<VoidListener>, parser: &mut Processor<StdSyncHandler>) {
    let grid = TermGrid::new(term.columns(), term.screen_lines());
    *term = create_term_with_grid(grid);
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
                NamedColor::Cursor => BASE16[7],
                NamedColor::Black | NamedColor::DimBlack => BASE16[0],
                NamedColor::Red | NamedColor::DimRed => BASE16[1],
                NamedColor::Green | NamedColor::DimGreen => BASE16[2],
                NamedColor::Yellow | NamedColor::DimYellow => BASE16[3],
                NamedColor::Blue | NamedColor::DimBlue => BASE16[4],
                NamedColor::Magenta | NamedColor::DimMagenta => BASE16[5],
                NamedColor::Cyan | NamedColor::DimCyan => BASE16[6],
                NamedColor::White | NamedColor::DimWhite => BASE16[7],
                NamedColor::BrightBlack => BASE16[8],
                NamedColor::BrightRed => BASE16[9],
                NamedColor::BrightGreen => BASE16[10],
                NamedColor::BrightYellow => BASE16[11],
                NamedColor::BrightBlue => BASE16[12],
                NamedColor::BrightMagenta => BASE16[13],
                NamedColor::BrightCyan => BASE16[14],
                NamedColor::BrightWhite => BASE16[15],
                // 默认色被 INVERSE 换到对侧：前景当白、背景当黑
                NamedColor::Foreground
                | NamedColor::BrightForeground
                | NamedColor::DimForeground => BASE16[7],
                NamedColor::Background => BASE16[0],
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
                    let level = 8 + (usize::from(idx) - 232) * 10;
                    Rgb {
                        r: level as u8,
                        g: level as u8,
                        b: level as u8,
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
#[tracing::instrument(name = "serial.snapshot", skip_all)]
pub fn snapshot_screen(term: &Term<VoidListener>) -> ScreenSnapshot {
    let grid = term.grid();
    let cols = grid.columns();
    let bottommost = grid.bottommost_line().0;
    let topmost = grid.topmost_line().0;
    let start = (bottommost + 1 - SNAPSHOT_MAX_ROWS as i32).max(topmost);

    let content = term.renderable_content();
    let cursor = content.cursor.point;
    let cursor_hidden = matches!(
        content.cursor.shape,
        alacritty_terminal::vte::ansi::CursorShape::Hidden
    );

    let mut rows: Vec<ScreenRow> = Vec::new();
    let mut cursor_row: Option<u32> = None;
    let mut cursor_char: Option<u32> = None;
    for line in start..=bottommost {
        let row = &grid[Line(line)];
        let mut text = String::with_capacity(cols);
        let mut fg: Option<u32> = None;
        let mut bg: Option<u32> = None;
        let mut cursor_here = !cursor_hidden && line == cursor.line.0;
        for col in 0..cols {
            let cell = &row[Column(col)];
            if cursor_here && col == cursor.column.0 {
                // 光标落在本行该列：字符偏移 = 已收集字符数
                cursor_char = Some(text.chars().count() as u32);
                cursor_row = Some(rows.len() as u32);
                cursor_here = false;
            }
            // 宽字符占位单元不产文本（字符在前导单元）
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
        }
        let trimmed_len = text.trim_end().len();
        if trimmed_len < text.len() {
            text.truncate(trimmed_len);
            // 光标若落在被裁掉的尾随空白区，吸附到行尾
            if cursor_row == Some(rows.len() as u32)
                && cursor_char.is_some_and(|cc| cc > trimmed_len as u32)
            {
                cursor_char = Some(trimmed_len as u32);
            }
        }
        // 快照头部的历史空行跳过（老输出上方的空白没有信息量）
        if rows.is_empty() && text.is_empty() {
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
        assert!(snapshot_screen(&term).rows.is_empty());

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
            deadline - std::time::Duration::from_millis(1),
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

    /// 终端仿真重构（alacritty_terminal）单元级验证：字节流进 Processor →
    /// Term 网格承载设备侧回显与 ANSI（代替旧 LineAssembler 行重组测试）。
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
        assert_eq!(red.fg, Some(0xcc6666), "SGR 31 必须映射为红色前景");
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
        assert_eq!(snap.cursor_row, Some(fresh_idx as u32));
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
        assert_eq!(row.fg, Some(0xff0000), "indexed 196 → 立方红");
        let row = snap.rows.iter().find(|r| r.text.contains('B')).unwrap();
        assert_eq!(row.fg, Some(0x12ff34), "SGR 38;2 直写 RGB");
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
}
