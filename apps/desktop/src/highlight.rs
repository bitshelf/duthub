//! Prompt 正则高亮（「串口」设置段消费端）——sermcp 式渲染期叠加层：
//! 只读快照行文本，返回分段样式，由渲染层把分段染进行内 children。
//! 不碰 RX/TX 数据流、不进 .log/审计（坑 #22 分层红线）。
//!
//! 规则语义：按配置顺序生效，先到先得（与已收区间重叠的匹配整体丢弃）；
//! 光标单元样式永远优先（反显块不被染色覆盖）。

/// 编译后的高亮规则（Workbench 按模式串缓存编译结果）。
pub struct CompiledRule {
    pub regex: regex::Regex,
    pub color: String,
}

/// A single styled rendering segment: text + optional foreground override +
/// cursor/selection flags.
pub struct Segment {
    pub text: String,
    /// None = row default foreground; Some(hex) = rule color; cursor segment
    /// uses inverted colors in the renderer.
    pub color: Option<String>,
    /// This segment contains the terminal cursor cell.
    pub is_cursor: bool,
    /// This segment falls inside the active mouse text selection.
    pub selected: bool,
}

/// Splits a row into styled segments according to highlight rules and cursor.
/// `cursor_char` is the cursor character offset (`None` if not on this row;
/// beyond `text.chars().count()` appends a virtual trailing space cursor).
#[cfg(test)]
pub fn row_segments(text: &str, rules: &[CompiledRule], cursor_char: Option<u32>) -> Vec<Segment> {
    row_segments_with_selection(text, rules, cursor_char, None)
}

/// Splits a row into styled segments with an optional character-offset selection
/// range `(start, end)`.
pub fn row_segments_with_selection(
    text: &str,
    rules: &[CompiledRule],
    cursor_char: Option<u32>,
    selection: Option<(usize, usize)>,
) -> Vec<Segment> {
    // Fast path: plain text with no highlight rules, cursor, or selection.
    if rules.is_empty() && cursor_char.is_none() && selection.is_none() {
        if text.is_empty() {
            return Vec::new();
        }
        return vec![Segment {
            text: text.to_owned(),
            color: None,
            is_cursor: false,
            selected: false,
        }];
    }

    // Build a byte-offset -> char-index lookup once so regex matches map in O(log N)
    // instead of rescanning from byte 0 on every match.
    let mut char_byte_offsets: Vec<usize> = Vec::with_capacity(text.len());
    let mut chars: Vec<char> = Vec::with_capacity(text.len());
    for (byte_idx, ch) in text.char_indices() {
        char_byte_offsets.push(byte_idx);
        chars.push(ch);
    }
    let len = chars.len();
    let byte_to_char = |byte_pos: usize| -> usize {
        if byte_pos >= text.len() {
            len
        } else {
            char_byte_offsets.binary_search(&byte_pos).unwrap_or(len)
        }
    };

    // 1) Collect non-overlapping highlight spans in rule priority order.
    let mut ranges: Vec<(usize, usize, &str)> = Vec::new();
    for rule in rules {
        let add_match = |m: regex::Match<'_>| {
            let start = byte_to_char(m.start());
            let end = byte_to_char(m.end());
            if start >= end || end > len {
                return;
            }
            if ranges.iter().any(|(s, e, _)| start < *e && *s < end) {
                return;
            }
            ranges.push((start, end, rule.color.as_str()));
        };
        if rule
            .regex
            .capture_names()
            .any(|name| name == Some("highlight"))
        {
            rule.regex
                .captures_iter(text)
                .filter_map(|captures| captures.name("highlight"))
                .for_each(add_match);
        } else {
            rule.regex.find_iter(text).for_each(add_match);
        }
    }
    if ranges.is_empty() && cursor_char.is_none() && selection.is_none() {
        if text.is_empty() {
            return Vec::new();
        }
        return vec![Segment {
            text: text.to_owned(),
            color: None,
            is_cursor: false,
            selected: false,
        }];
    }
    ranges.sort_by_key(|(start, _, _)| *start);

    // 2) Assign per-character foreground colors (cursor overrides highlight).
    let mut colors: Vec<Option<&str>> = vec![None; len];
    for (start, end, color) in &ranges {
        for cell in colors.iter_mut().take(*end).skip(*start) {
            *cell = Some(color);
        }
    }
    let cursor_idx = cursor_char.map(|cc| (cc as usize).min(len));

    // 3) Merge adjacent characters with matching style; append virtual cursor
    // space when cursor lies at or past the trimmed line end.
    let mut segments: Vec<Segment> = Vec::new();
    let push = |segments: &mut Vec<Segment>,
                text: String,
                color: Option<&str>,
                cursor: bool,
                selected: bool| {
        if text.is_empty() && !cursor {
            return;
        }
        if let Some(last) = segments.last_mut()
            && !cursor
            && last.color.as_deref() == color
            && !last.is_cursor
            && last.selected == selected
        {
            last.text.push_str(&text);
            return;
        }
        segments.push(Segment {
            text,
            color: color.map(str::to_string),
            is_cursor: cursor,
            selected,
        });
    };
    let mut run = String::new();
    let mut run_color: Option<&str> = None;
    let mut run_selected = false;
    for (idx, ch) in chars.iter().enumerate() {
        let cell_selected = selection.is_some_and(|(start, end)| idx >= start && idx < end);
        if Some(idx) == cursor_idx {
            // Always flush any preceding run before emitting the isolated cursor cell.
            push(
                &mut segments,
                std::mem::take(&mut run),
                run_color,
                false,
                run_selected,
            );
            push(&mut segments, ch.to_string(), None, true, cell_selected);
            run_color = None;
            run_selected = false;
            continue;
        }
        let cell_color = colors[idx];
        if cell_color != run_color || cell_selected != run_selected {
            push(
                &mut segments,
                std::mem::take(&mut run),
                run_color,
                false,
                run_selected,
            );
            run_color = cell_color;
            run_selected = cell_selected;
        }
        run.push(*ch);
    }
    push(&mut segments, run, run_color, false, run_selected);
    if cursor_idx.is_some_and(|ci| ci >= len) {
        segments.push(Segment {
            text: " ".to_string(),
            color: None,
            is_cursor: true,
            selected: false,
        });
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(patterns: &[(&str, &str)]) -> Vec<CompiledRule> {
        patterns
            .iter()
            .map(|(pattern, color)| CompiledRule {
                regex: regex::Regex::new(pattern).unwrap(),
                color: (*color).to_string(),
            })
            .collect()
    }

    fn texts(segments: &[Segment]) -> Vec<&str> {
        segments.iter().map(|s| s.text.as_str()).collect()
    }

    /// 分段三元组（text, color, is_cursor）整表比较辅助。
    fn cells(segments: &[Segment]) -> Vec<(&str, Option<&str>, bool)> {
        segments
            .iter()
            .map(|s| (s.text.as_str(), s.color.as_deref(), s.is_cursor))
            .collect()
    }

    /// prompt 行按规则染色：`root@rk3576:~#` 段拿规则色，命令部分保持默认。
    #[test]
    fn prompt_segment_gets_rule_color() {
        let compiled = rules(&[(r"[^ ]+@[^ :]+:.*[#\$]", "#edbe7a")]);
        let segments = row_segments("root@rk3576:~# dmesg | tail", &compiled, None);
        assert_eq!(texts(&segments), vec!["root@rk3576:~#", " dmesg | tail"]);
        assert_eq!(segments[0].color.as_deref(), Some("#edbe7a"));
        assert_eq!(segments[1].color, None);
        assert!(!segments.iter().any(|s| s.is_cursor));
    }

    /// 无匹配 → 单段原文默认色。
    #[test]
    fn no_match_keeps_single_normal_segment() {
        let compiled = rules(&[(r"panic", "#ec8291")]);
        let segments = row_segments("hello world", &compiled, None);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "hello world");
        assert_eq!(segments[0].color, None);
    }

    /// 多条规则按序生效、先到先得不重叠。
    #[test]
    fn rules_apply_in_order_without_overlap() {
        let compiled = rules(&[(r"root@host", "#a"), (r"root", "#b")]);
        let segments = row_segments("root@host ok", &compiled, None);
        assert_eq!(texts(&segments), vec!["root@host", " ok"]);
        assert_eq!(segments[0].color.as_deref(), Some("#a"));
    }

    /// 光标单元反显优先：落在高亮区间内时该字符不被染色。
    #[test]
    fn cursor_cell_wins_over_highlight() {
        let compiled = rules(&[(r"abcdef", "#a")]);
        // 光标在 idx=3（'d'）
        let segments = row_segments("abcdef", &compiled, Some(3));
        assert_eq!(texts(&segments), vec!["abc", "d", "ef"]);
        assert_eq!(segments[0].color.as_deref(), Some("#a"));
        assert!(segments[1].is_cursor);
        assert_eq!(segments[2].color.as_deref(), Some("#a"));
    }

    /// 光标越界（A1 修复的行尾虚拟空格）→ 补一段空格光标。
    #[test]
    fn cursor_beyond_text_becomes_virtual_space() {
        let compiled = rules(&[(r"x", "#a")]);
        let segments = row_segments("x", &compiled, Some(2));
        assert_eq!(texts(&segments), vec!["x", " "]);
        assert!(segments[1].is_cursor);
    }

    /// 中文宽字符按字符（非字节）分段。
    #[test]
    fn multibyte_text_splits_by_chars() {
        let compiled = rules(&[(r"中文", "#a")]);
        let segments = row_segments("中文ok", &compiled, None);
        assert_eq!(texts(&segments), vec!["中文", "ok"]);
    }

    // ---- 以下为 IO-7 不变量的补充锁定（行为面，不改既有六测） ----

    /// 先到先得的「整体丢弃」语义：后规则与已收区间重叠时整个匹配弃用
    ///（不是截断补染），区间外剩余文本保持默认色。
    #[test]
    fn later_rule_overlapping_match_discarded_entirely() {
        // 规则一 "bcd" 先收 [1,4)；规则二 "abcdef" 匹配 [0,6) 与之重叠 → 整体丢弃
        let compiled = rules(&[(r"bcd", "#b"), (r"abcdef", "#a")]);
        let segments = row_segments("abcdef", &compiled, None);
        assert_eq!(
            cells(&segments),
            vec![
                ("a", None, false),
                ("bcd", Some("#b"), false),
                ("ef", None, false),
            ]
        );
    }

    /// 单条规则多处命中全部染色（find_iter 语义），中间默认色段隔开。
    #[test]
    fn single_rule_matches_everywhere() {
        let compiled = rules(&[(r"err", "#c")]);
        let segments = row_segments("err x err", &compiled, None);
        assert_eq!(
            cells(&segments),
            vec![
                ("err", Some("#c"), false),
                (" x ", None, false),
                ("err", Some("#c"), false),
            ]
        );
    }

    /// 相邻同色区间（无间隔的两处命中）合并为单段。
    #[test]
    fn adjacent_same_color_matches_merge() {
        let compiled = rules(&[(r"err", "#c")]);
        let segments = row_segments("errerr", &compiled, None);
        assert_eq!(cells(&segments), vec![("errerr", Some("#c"), false)]);
    }

    /// 零宽匹配（`x*` 的空命中）不产生高亮区间，也不在真实命中之间插出
    /// 空段或打断染色；真实命中照常染色。
    #[test]
    fn zero_width_matches_never_highlight() {
        // find_iter 对 "xax" 给出 "x"@0..1、""@1..1、"x"@2..3、""@3..3
        let compiled = rules(&[(r"x*", "#a")]);
        let segments = row_segments("xax", &compiled, None);
        assert_eq!(
            cells(&segments),
            vec![
                ("x", Some("#a"), false),
                ("a", None, false),
                ("x", Some("#a"), false),
            ]
        );
    }

    /// 光标落在高亮区间首/尾字符上：反显单元独立成段且不带规则色，
    /// 区间其余部分各自保留规则色（样式优先级：光标 > 高亮 > 默认）。
    #[test]
    fn cursor_at_highlight_edges_splits_range() {
        let compiled = rules(&[(r"abcdef", "#a")]);
        let head = row_segments("abcdef", &compiled, Some(0));
        assert_eq!(
            cells(&head),
            vec![("a", None, true), ("bcdef", Some("#a"), false)]
        );
        let tail = row_segments("abcdef", &compiled, Some(5));
        assert_eq!(
            cells(&tail),
            vec![("abcde", Some("#a"), false), ("f", None, true)]
        );
    }

    /// 光标落在无高亮的普通字符上：恰好一个反显段（内容就是光标字符、
    /// 无规则色），且两侧同色默认段不会横跨光标合并——反显块必须独立可见。
    #[test]
    fn cursor_on_plain_text_stays_isolated() {
        let compiled = rules(&[(r"nomatch", "#a")]);
        let segments = row_segments("hello", &compiled, Some(2));
        assert_eq!(
            cells(&segments),
            vec![("he", None, false), ("l", None, true), ("lo", None, false)]
        );
    }

    /// 光标偏移按字符数收敛到行尾：`Some(len)`（行尾下一格）与更远的越界
    /// 行为一致——正文段原样保留，末尾补一段单空格行尾虚拟空格反显。
    #[test]
    fn cursor_at_end_and_beyond_both_get_virtual_space() {
        let compiled = rules(&[(r"x", "#a")]);
        for cc in [1u32, 7] {
            let segments = row_segments("x", &compiled, Some(cc));
            assert_eq!(
                cells(&segments),
                vec![("x", Some("#a"), false), (" ", None, true)]
            );
        }
    }

    /// 空行：无光标时零分段（渲染层不画空 div）；有光标时只有一段单空格
    /// 反显（空行光标同样走行尾虚拟空格）。
    #[test]
    fn empty_line_segments() {
        let compiled = rules(&[(r".", "#a")]);
        assert!(row_segments("", &compiled, None).is_empty());
        assert_eq!(
            cells(&row_segments("", &compiled, Some(0))),
            vec![(" ", None, true)]
        );
    }

    /// 多字节按字符（非字节）分段（强化版）：四字节 emoji 是一个光标单元，
    /// 按字节切会把 emoji 撕进反显段——光标段必须包含完整字符。
    #[test]
    fn multibyte_cursor_segment_keeps_whole_char() {
        let compiled = rules(&[(r"b", "#a")]);
        // "a🎉b" = 3 个字符（'a'、'🎉'、'b'）；cursor_char=1 指向 emoji 本身
        let segments = row_segments("a🎉b", &compiled, Some(1));
        assert_eq!(
            cells(&segments),
            vec![
                ("a", None, false),
                ("🎉", None, true),
                ("b", Some("#a"), false)
            ]
        );
    }

    /// 多字节行的越界光标按字符长度收敛（坑 #46 同源：cursor_char 是字符
    /// 偏移，与字节长度混比会在多字节行尾误判）——传入字节视角下"合法"的
    /// 偏移 9（"中文ok" 是 4 字符 / 12 字节），实际收敛为行尾虚拟空格。
    #[test]
    fn multibyte_beyond_clamps_to_char_len() {
        let compiled = rules(&[(r"中文", "#a")]);
        let segments = row_segments("中文ok", &compiled, Some(9));
        assert_eq!(
            cells(&segments),
            vec![
                ("中文", Some("#a"), false),
                ("ok", None, false),
                (" ", None, true),
            ]
        );
    }

    /// 渲染期纯函数叠加的可观测契约：
    /// ① 同一输入重复调用输出完全一致（无隐藏状态、无副作用积累）；
    /// ② 分段拼接保全行文本——行内光标拼回原文，越界光标拼回原文 + 一个
    ///    行尾虚拟空格。叠加层只读快照行文本、不改写内容，这正是「不碰
    ///    RX/TX 数据流、不进 .log/审计」（坑 #22 分层红线）在本文件的可测
    ///    面：纯函数签名（&str 入、Vec<Segment> 出、无 I/O）从结构上隔离
    ///    数据流与日志，确定性 + 原文保全锁住「叠加」语义本身。
    #[test]
    fn overlay_is_deterministic_and_preserves_text() {
        let compiled = rules(&[
            (r"root@[^ :]+", "#a"),
            (r"中文", "#b"),
            (r"panic", "#c"),
            (r"err", "#d"),
        ]);
        let lines = [
            "root@rk3576:~# dmesg | tail",
            "中文ok 中文 errerr",
            "panic:oops err",
            "a🎉b",
            "",
        ];
        for line in lines {
            let char_len = line.chars().count();
            for cursor in [None, Some(0u32), Some(3u32)] {
                let segments = row_segments(line, &compiled, cursor);
                let again = row_segments(line, &compiled, cursor);
                assert_eq!(
                    cells(&segments),
                    cells(&again),
                    "同输入两次调用结果不一致：line={line:?} cursor={cursor:?}"
                );
                let joined: String = segments.iter().map(|s| s.text.as_str()).collect();
                let expected = match cursor {
                    Some(cc) if (cc as usize) >= char_len => format!("{line} "),
                    _ => line.to_string(),
                };
                assert_eq!(
                    joined, expected,
                    "分段拼接未保全行文本：line={line:?} cursor={cursor:?}"
                );
            }
        }
    }

    /// 渲染期开销的机制性宽松上界：单行 ~4.7k 字符 × 4 条规则 × 20 次调用
    /// 须在 1 秒内完成。真实成本是毫秒级，预算留百倍余量——只拦灾难性
    /// 回溯或意外的非线性复杂度（高亮每帧每行都要跑，超界即渲染冻结），
    /// 不是窄时序断言。
    #[test]
    fn long_line_rendering_stays_cheap() {
        let compiled = rules(&[
            (r"\[[ 0-9]+\.[0-9]+\]", "#a"),
            (r"[0-9a-f]{8}", "#b"),
            (r"panic|Oops", "#c"),
            (r"root@[^ :]+", "#d"),
        ]);
        let chunk = "[   12.345678] root@rk3576:~# deadbeef payload 你好 ";
        let mut line = String::new();
        for _ in 0..100 {
            line.push_str(chunk);
        }
        // 计时外先做一次正确性抽样：长行输出同样保全原文
        let probe = row_segments(&line, &compiled, None);
        let joined: String = probe.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, line);

        let start = std::time::Instant::now();
        for _ in 0..20 {
            let _ = row_segments(&line, &compiled, Some(3));
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "长行渲染叠加超出宽松上界：20 次耗时 {elapsed:?}"
        );
    }

    /// 内置默认规则必须可编译，并按 sermcp 语义给 prompt 分段上色
    /// （user@host 绿、路径 蓝、行尾提示符 浅灰）。
    #[test]
    fn default_rules_compile_and_color_prompt_parts() {
        let compiled: Vec<CompiledRule> = duthub_core::model::default_highlight_rules()
            .into_iter()
            .map(|rule| CompiledRule {
                regex: regex::Regex::new(&rule.regex).expect("内置默认规则必须可编译"),
                color: rule.color,
            })
            .collect();
        let segments = row_segments("root@rk3576:/data# ls -l", &compiled, None);
        assert!(
            segments
                .iter()
                .any(|s| s.text == "root@rk3576" && s.color.as_deref() == Some("#7ec699")),
            "user@host 应着绿色：{:?}",
            cells(&segments)
        );
        assert!(
            segments
                .iter()
                .any(|s| s.text == ":/data" && s.color.as_deref() == Some("#56a8f5")),
            "路径应着蓝色：{:?}",
            cells(&segments)
        );
        // Android mksh 提示符：状态+host 绿色，行尾 `$ ` 着提示符色。
        let android = row_segments("130|console:/ $ ", &compiled, None);
        assert!(
            android
                .iter()
                .any(|s| s.text.starts_with("130|console") && s.color.as_deref() == Some("#7ec699")),
            "Android 状态+host 应着绿色：{:?}",
            cells(&android)
        );
        assert!(
            android
                .iter()
                .any(|s| s.color.as_deref() == Some("#c8ccd4")),
            "行尾提示符应着浅灰：{:?}",
            cells(&android)
        );
    }

    #[test]
    fn bare_android_prompt_colors_host_and_path_but_not_a_plain_line() {
        let rules: Vec<_> = duthub_core::model::default_highlight_rules()
            .into_iter()
            .map(|rule| CompiledRule {
                regex: regex::Regex::new(&rule.regex).unwrap(),
                color: rule.color,
            })
            .collect();
        let prompt = row_segments("console:/ $ ", &rules, Some(12));
        assert!(
            prompt
                .iter()
                .any(|s| s.text == "console" && s.color.as_deref() == Some("#7ec699"))
        );
        assert!(
            prompt
                .iter()
                .any(|s| s.text == ":/" && s.color.as_deref() == Some("#56a8f5"))
        );
        assert!(
            row_segments("console:/ boot complete", &rules, None)
                .iter()
                .all(|s| s.color.as_deref() != Some("#7ec699"))
        );
    }

    #[test]
    fn selection_splits_plain_and_highlighted_runs_while_preserving_text() {
        let compiled = rules(&[(r"root@KC", "#7ec699")]);
        let segments = row_segments_with_selection("root@KC:~# ls", &compiled, None, Some((4, 10)));
        let summary: Vec<(&str, Option<&str>, bool)> = segments
            .iter()
            .map(|s| (s.text.as_str(), s.color.as_deref(), s.selected))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("root", Some("#7ec699"), false),
                ("@KC", Some("#7ec699"), true),
                (":~#", None, true),
                (" ls", None, false),
            ]
        );
    }
}
