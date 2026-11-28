//! Prompt regular expression highlighting for "serial port" log segmentation consumption side ——
//! sermcp-style rendering overlay layer:
//! Read-only snapshot row text, returning segmented styles. The rendering layer processes the
//! segments into internal children.
//! Does not interfere with RX/TX data streams, and does not enter.log/audit (pit #22 hierarchical
//! red line).
//!
//! Rule semantics: Rules take effect in the order they are configured. The one that arrives first
//! will be processed first (matches overlapping with already occupied intervals will be discarded).
//! The style of the cursor unit always takes precedence over others (inverted blocks cannot be
//! overwritten by other styles).

/// Compiled highlighting rules (Workbench caches the compilation results based on pattern strings).
pub struct CompiledRule {
    pub regex: regex::Regex,
    pub color: String,
}

/// Compile a literal find query with offsets into the original text.
pub fn terminal_search(query: &str) -> Option<regex::Regex> {
    if query.is_empty() {
        return None;
    }
    regex::RegexBuilder::new(&regex::escape(query))
        .case_insensitive(true)
        .build()
        .ok()
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
    pub search_match: bool,
}

/// Splits a row into styled segments according to highlight rules and cursor.
/// `cursor_char` is the cursor character offset (`None` if not on this row;
/// beyond `text.chars().count()` appends a virtual trailing space cursor).
#[cfg(test)]
pub fn row_segments(text: &str, rules: &[CompiledRule], cursor_char: Option<u32>) -> Vec<Segment> {
    row_segments_with_selection(text, rules, cursor_char, None, None)
}

/// Splits a row into styled segments with an optional character-offset selection
/// range `(start, end)`.
pub fn row_segments_with_selection(
    text: &str,
    rules: &[CompiledRule],
    cursor_char: Option<u32>,
    selection: Option<(usize, usize)>,
    search: Option<&regex::Regex>,
) -> Vec<Segment> {
    let mut hits = search
        .into_iter()
        .flat_map(|query| query.find_iter(text))
        .peekable();
    // Fast path: plain text with no highlight rules, cursor, or selection.
    if rules.is_empty() && cursor_char.is_none() && selection.is_none() && hits.peek().is_none() {
        if text.is_empty() {
            return Vec::new();
        }
        return vec![Segment {
            text: text.to_owned(),
            color: None,
            is_cursor: false,
            selected: false,
            search_match: false,
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
    if ranges.is_empty() && cursor_char.is_none() && selection.is_none() && hits.peek().is_none() {
        if text.is_empty() {
            return Vec::new();
        }
        return vec![Segment {
            text: text.to_owned(),
            color: None,
            is_cursor: false,
            selected: false,
            search_match: false,
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
    for (idx, ch) in chars.into_iter().enumerate() {
        let byte = char_byte_offsets[idx];
        while hits.peek().is_some_and(|hit| hit.end() <= byte) {
            hits.next();
        }
        let is_cursor = Some(idx) == cursor_idx;
        let selected = selection.is_some_and(|(start, end)| idx >= start && idx < end);
        let search_match =
            !is_cursor && !selected && hits.peek().is_some_and(|hit| hit.start() <= byte);
        let color = if is_cursor { None } else { colors[idx] };
        if let Some(last) = segments.last_mut()
            && !is_cursor
            && !last.is_cursor
            && last.color.as_deref() == color
            && last.selected == selected
            && last.search_match == search_match
        {
            last.text.push(ch);
        } else {
            segments.push(Segment {
                text: ch.to_string(),
                color: color.map(str::to_owned),
                is_cursor,
                selected,
                search_match,
            });
        }
    }
    if cursor_idx.is_some_and(|ci| ci >= len) {
        segments.push(Segment {
            text: " ".to_string(),
            color: None,
            is_cursor: true,
            selected: false,
            search_match: false,
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

    /// Helper returning `(text, color, is_cursor)` tuples for table comparison.
    fn cells(segments: &[Segment]) -> Vec<(&str, Option<&str>, bool)> {
        segments
            .iter()
            .map(|s| (s.text.as_str(), s.color.as_deref(), s.is_cursor))
            .collect()
    }

    /// The `prompt` line is colored according to the specified rules. The `root@rk3576:~#` section
    /// uses a specific color scheme, while the command part remains unchanged.
    #[test]
    fn prompt_segment_gets_rule_color() {
        let compiled = rules(&[(r"[^ ]+@[^ :]+:.*[#\$]", "#edbe7a")]);
        let segments = row_segments("root@rk3576:~# dmesg | tail", &compiled, None);
        assert_eq!(texts(&segments), vec!["root@rk3576:~#", " dmesg | tail"]);
        assert_eq!(segments[0].color.as_deref(), Some("#edbe7a"));
        assert_eq!(segments[1].color, None);
        assert!(!segments.iter().any(|s| s.is_cursor));
    }

    /// No match found → The default color of the original text is used.
    #[test]
    fn no_match_keeps_single_normal_segment() {
        let compiled = rules(&[(r"panic", "#ec8291")]);
        let segments = row_segments("hello world", &compiled, None);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "hello world");
        assert_eq!(segments[0].color, None);
    }

    /// Multiple rules take effect in order, with the ones that arrive first taking precedence over
    /// those that arrive later.
    #[test]
    fn rules_apply_in_order_without_overlap() {
        let compiled = rules(&[(r"root@host", "#a"), (r"root", "#b")]);
        let segments = row_segments("root@host ok", &compiled, None);
        assert_eq!(texts(&segments), vec!["root@host", " ok"]);
        assert_eq!(segments[0].color.as_deref(), Some("#a"));
    }

    /// Priority for highlighting the cursor position: When the character is located within the
    /// highlighted area, it will not be colored.
    #[test]
    fn cursor_cell_wins_over_highlight() {
        let compiled = rules(&[(r"abcdef", "#a")]);
        // The cursor is located at idx=3 ('d')
        let segments = row_segments("abcdef", &compiled, Some(3));
        assert_eq!(texts(&segments), vec!["abc", "d", "ef"]);
        assert_eq!(segments[0].color.as_deref(), Some("#a"));
        assert!(segments[1].is_cursor);
        assert_eq!(segments[2].color.as_deref(), Some("#a"));
    }

    /// The cursor has crossed the boundary (fixed by row end virtual space in cell A1) → Add some
    /// spaces to fill in the empty space around the cursor.
    #[test]
    fn cursor_beyond_text_becomes_virtual_space() {
        let compiled = rules(&[(r"x", "#a")]);
        let segments = row_segments("x", &compiled, Some(2));
        assert_eq!(texts(&segments), vec!["x", " "]);
        assert!(segments[1].is_cursor);
    }

    /// Chinese wide characters are segmented by character (not byte).
    #[test]
    fn multibyte_text_splits_by_chars() {
        let compiled = rules(&[(r"中文", "#a")]);
        let segments = row_segments("中文ok", &compiled, None);
        assert_eq!(texts(&segments), vec!["中文", "ok"]);
    }

    // — Below is additional locking for the IO-7 invariants (behavioral aspects; no changes are
    // made to the existing six tests.) —

    /// "First come, first served" approach for "total discard": When subsequent rules overlap with
    /// the already processed areas, all matches are discarded. (No truncation or supplementary
    /// coloring is performed.) Text remaining outside these areas remains in its default color.
    #[test]
    fn later_rule_overlapping_match_discarded_entirely() {
        // Rule 1: The pattern "bcd" matches the range [1,4). Rule 2: The pattern "abcdef" matches
        // the range [0,6); both ranges overlap each other. Therefore, all matches are discarded.
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

    /// Colors every non-overlapping match of a single rule (`find_iter` semantics).
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

    /// Adjacent regions of the same color are merged into a single segment. This occurs when there
    /// are no gaps between the two occurrences of the same color.
    #[test]
    fn adjacent_same_color_matches_merge() {
        let compiled = rules(&[(r"err", "#c")]);
        let segments = row_segments("errerr", &compiled, None);
        assert_eq!(cells(&segments), vec![("errerr", Some("#c"), false)]);
    }

    /// Zero-width matches (i.e., empty hits of the `x*` pattern) do not result in any highlighted
    /// regions. They also do not create any interruptions between actual hits. Empty segments or
    /// breaks are not considered as valid hits. Actual hits are still treated as valid hits.
    #[test]
    fn zero_width_matches_never_highlight() {
        // The find_iter function returns "x"@0..1, ""@1..1, "x"@2..3, ""@3..3 for the input "xax".
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

    /// The cursor is placed on the first/last character of the highlighted section: The
    /// non-highlighted parts remain in their original color, while the highlighted parts are
    /// displayed as separate segments without any specific color indication. The priority order for
    /// styling is as follows: Cursor > Highlight > Default.
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

    /// The cursor rests on an ordinary character without any highlighting. There is exactly one
    /// inverted segment, which contains the character itself, along with segments of the same
    /// color. These segments will not merge together around the cursor. The inverted segments must
    /// be visible independently.
    #[test]
    fn cursor_on_plain_text_stays_isolated() {
        let compiled = rules(&[(r"nomatch", "#a")]);
        let segments = row_segments("hello", &compiled, Some(2));
        assert_eq!(
            cells(&segments),
            vec![("he", None, false), ("l", None, true), ("lo", None, false)]
        );
    }

    /// The cursor offset converges towards the end of the line based on the number of characters
    /// involved: `Some(len)` (one space away from the end of the line) and further beyond the
    /// boundary. This behavior remains consistent—the main text segment is retained as it is, with
    /// an additional single space added at the end to create a virtual space that can be visually
    /// distinguished.
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

    /// Blank lines: No cursor means zero log segmentation (no empty divs are drawn at the rendering
    /// layer); with a cursor, there is only one single space.
    /// Reverse display (blank line cursors also use virtual spaces at the end of the line).
    #[test]
    fn empty_line_segments() {
        let compiled = rules(&[(r".", "#a")]);
        assert!(row_segments("", &compiled, None).is_empty());
        assert_eq!(
            cells(&row_segments("", &compiled, Some(0))),
            vec![(" ", None, true)]
        );
    }

    /// Multi-byte segmentation by character (not byte-based) (enhanced version): A four-byte emoji
    /// represents a single cursor unit. If segmented by bytes, the emoji will be split into
    /// separate segments—the cursor segment must contain complete characters.
    #[test]
    fn multibyte_cursor_segment_keeps_whole_char() {
        let compiled = rules(&[(r"b", "#a")]);
        // "a🎉b" = 3 characters ('a', '🎉', 'b'); cursor_char is set to 1, indicating the emoji
        // itself.
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

    /// Clamps out-of-range cursor columns on multibyte lines by character length
    /// (`cursor_char` is a char offset, not a byte offset).
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

    /// During the rendering period, the observable contract is achieved through pure function
    /// superposition:
    /// ① Repeated calls to the same input result in identical outputs (no hidden states, no side
    /// effects accumulating);
    /// ② The text within each segment is preserved—the cursor within a line is restored to its
    /// original text, and the cursor beyond the boundary is restored to its original text plus one
    /// additional virtual space at the end of the line. The overlay layer only reads snapshot text
    /// without modifying the content. This is precisely what is meant by "not touching RX/TX data
    /// streams, not entering .log/audit" (pit #22, hierarchical red line). In this document, it is
    /// possible to measure this: Pure function signatures (input &str, output Vec<Segment>, no I/O)
    /// structurally isolate the data stream from the logs, ensuring determinism while preserving
    /// the original text and locking the "superposition" semantics.
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
                    "The results of the two calls do not match: line={line:?} cursor={cursor:?}."
                );
                let joined: String = segments.iter().map(|s| s.text.as_str()).collect();
                let expected = match cursor {
                    Some(cc) if (cc as usize) >= char_len => format!("{line} "),
                    _ => line.to_string(),
                };
                assert_eq!(
                    joined, expected,
                    "Log segmentation: concatenating segments without preserving the original text of the lines: line={line:?} cursor={cursor:?}"
                );
            }
        }
    }

    /// The mechanism-based relaxation upper bound for the rendering period: approximately 4.7k
    /// characters per row × 4 rules × 20 calls. This must be completed within 1 second. The actual
    /// cost is in milliseconds, and a budget of 100 times more is reserved—only catastrophic
    /// backtracking or unexpected non-linear complexity should be encountered (highlighted: each
    /// frame and each row requires processing, and rendering freezing occurs when the limit is
    /// exceeded). This is not a narrow timing assertion.
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
        // Perform a correctness test outside of the scheduled time: The same process applies for
        // long-output scenarios.
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
            "Long rendering operations exceed the allowed upper limit of 20 times. Time taken: {elapsed:?}"
        );
    }

    /// The built-in default rules must be compileable, and the prompt should be segmented according
    /// to the semantics of sermcp. The colors for user@host, paths, and prompt endings should be
    /// light gray.
    #[test]
    fn default_rules_compile_and_color_prompt_parts() {
        let compiled: Vec<CompiledRule> = duthub_core::model::default_highlight_rules()
            .into_iter()
            .map(|rule| CompiledRule {
                regex: regex::Regex::new(&rule.regex)
                    .expect("Built-in default rules must be compileable."),
                color: rule.color,
            })
            .collect();
        let segments = row_segments("root@rk3576:/data# ls -l", &compiled, None);
        assert!(
            segments
                .iter()
                .any(|s| s.text == "root@rk3576" && s.color.as_deref() == Some("#7ec699")),
            "User@Host should be indicated in green: {:?}",
            cells(&segments)
        );
        assert!(
            segments
                .iter()
                .any(|s| s.text == ":/data" && s.color.as_deref() == Some("#56a8f5")),
            "The paths should be marked in blue: {:?}",
            cells(&segments)
        );
        // Android mksh prompt: The status is green, and the `$` symbol at the end of the line
        // indicates the prompt color.
        let android = row_segments("130|console:/ $ ", &compiled, None);
        assert!(
            android
                .iter()
                .any(|s| s.text.starts_with("130|console") && s.color.as_deref() == Some("#7ec699")),
            "The Android status should be displayed in green: {:?}",
            cells(&android)
        );
        assert!(
            android
                .iter()
                .any(|s| s.color.as_deref() == Some("#c8ccd4")),
            "The end-of-line prompt should be displayed in a light gray color: {:?}",
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
    fn find_highlights_literal_matches_in_original_text() {
        let search = terminal_search("d.g");
        let segments =
            row_segments_with_selection("中D.Gits d.g dig", &[], None, None, search.as_ref());
        assert_eq!(texts(&segments), ["中", "D.G", "its ", "d.g", " dig"]);
        assert_eq!(
            segments.iter().map(|s| s.search_match).collect::<Vec<_>>(),
            [false, true, false, true, false]
        );
        assert!(terminal_search("").is_none());
    }

    #[test]
    fn find_respects_foreground_selection_and_cursor() {
        let search = terminal_search("DIG");
        let compiled = rules(&[("dig", "#abc")]);
        let segments = row_segments_with_selection(
            "digits",
            &compiled,
            Some(2),
            Some((1, 2)),
            search.as_ref(),
        );
        assert_eq!(texts(&segments), ["d", "i", "g", "its"]);
        assert!(segments[0].search_match);
        assert_eq!(segments[0].color.as_deref(), Some("#abc"));
        assert!(segments[1].selected && !segments[1].search_match);
        assert!(segments[2].is_cursor && !segments[2].search_match);
    }

    #[test]
    fn selection_splits_plain_and_highlighted_runs_while_preserving_text() {
        let compiled = rules(&[(r"root@KC", "#7ec699")]);
        let segments =
            row_segments_with_selection("root@KC:~# ls", &compiled, None, Some((4, 10)), None);
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
