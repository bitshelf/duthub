use super::{
    Context, DEFAULT_TERMINAL_FONT_SIZE, LineKind, LinkState, LogLine, MouseDownEvent, Pixels,
    Point, SharedString, Window, Workbench, i18n, prepare_paste_bytes, px, terminal_col_for_x,
};

use std::collections::VecDeque;
use std::sync::Arc;

use duthub_core::ipc::{ScreenRow, ScreenSnapshot};
use duthub_device::term::TerminalHistoryPage;

use super::SessionView;

const HISTORY_OVERLAP: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EarlierHistory {
    /// Tail snapshot is pinned locally; device scrollback has not been queried yet.
    Unknown,
    /// Earlier rows remain in device scrollback beyond `oldest_from_tail`.
    Available(usize),
    /// All available device scrollback rows have been loaded.
    Exhausted,
}

impl EarlierHistory {
    fn from_remaining(remaining: usize) -> Self {
        if remaining > 0 {
            Self::Available(remaining)
        } else {
            Self::Exhausted
        }
    }
}

#[derive(Debug)]
pub(super) struct TerminalScrollback {
    older: VecDeque<ScreenRow>,
    tail: Arc<ScreenSnapshot>,
    oldest_from_tail: usize,
    earlier: EarlierHistory,
    just_prepended: bool,
}

impl SessionView {
    fn older_rows(&self) -> Option<&VecDeque<ScreenRow>> {
        self.scrollback.as_ref().map(|history| &history.older)
    }

    fn terminal_tail(&self) -> Option<&ScreenSnapshot> {
        self.scrollback
            .as_ref()
            .map_or(self.screen.as_deref(), |history| {
                Some(history.tail.as_ref())
            })
    }

    pub(super) fn displayed_rows_len(&self) -> usize {
        self.older_rows().map_or(0, VecDeque::len)
            + self.terminal_tail().map_or(0, |screen| screen.rows.len())
    }

    pub(super) fn displayed_rows(&self) -> impl Iterator<Item = &ScreenRow> {
        self.older_rows().into_iter().flatten().chain(
            self.terminal_tail()
                .into_iter()
                .flat_map(|screen| &screen.rows),
        )
    }

    pub(super) fn displayed_row(&self, index: usize) -> Option<&ScreenRow> {
        if let Some(older) = self.older_rows() {
            if index < older.len() {
                return older.get(index);
            }
            return self.terminal_tail()?.rows.get(index - older.len());
        }
        self.terminal_tail()?.rows.get(index)
    }

    pub(super) fn displayed_cursor(&self, index: usize) -> Option<u32> {
        let index = index.checked_sub(self.older_rows().map_or(0, VecDeque::len))?;
        let screen = self.terminal_tail()?;
        let cursor_row = usize::try_from(screen.cursor_row?).ok()?;
        screen.cursor_char.filter(|_| cursor_row == index)
    }

    /// Pin the currently displayed live snapshot so incoming sliding-window frames cannot shift
    /// visible rows or active text selections while the user is scrolled up or dragging.
    fn freeze_displayed_tail(&mut self) {
        if self.scrollback.is_none()
            && let Some(tail) = self.screen.as_ref()
        {
            self.scrollback = Some(TerminalScrollback {
                older: VecDeque::new(),
                tail: Arc::clone(tail),
                oldest_from_tail: tail.rows.len().saturating_sub(1),
                earlier: EarlierHistory::Unknown,
                just_prepended: false,
            });
        }
    }

    fn prepend_terminal_history(&mut self, mut page: TerminalHistoryPage) -> usize {
        let Some(tail) = self
            .scrollback
            .as_ref()
            .map(|history| Arc::clone(&history.tail))
            .or_else(|| self.screen.as_ref().map(Arc::clone))
        else {
            return 0;
        };
        let oldest_from_tail = self
            .scrollback
            .as_ref()
            .map_or(tail.rows.len().saturating_sub(1), |history| {
                history.oldest_from_tail
            });
        let page_oldest = page
            .skip
            .saturating_add(page.screen.rows.len().saturating_sub(1));
        let expected = page_oldest.saturating_sub(oldest_from_tail);
        let added = {
            let anchor: smallvec::SmallVec<[&ScreenRow; 3]> =
                self.displayed_rows().take(3).collect();
            history_prefix_len(&page.screen.rows, &anchor, expected).unwrap_or(0)
        };
        if added == 0 {
            let next_earlier = if page_oldest > oldest_from_tail {
                EarlierHistory::from_remaining(page.earlier)
            } else {
                EarlierHistory::Exhausted
            };
            if let Some(history) = &mut self.scrollback {
                // If live output has moved beyond the overlap, continue looking farther back.
                history.earlier = next_earlier;
                history.oldest_from_tail = history.oldest_from_tail.max(page_oldest);
            } else if page_oldest > oldest_from_tail
                && matches!(next_earlier, EarlierHistory::Available(_))
            {
                self.scrollback = Some(TerminalScrollback {
                    older: VecDeque::new(),
                    tail,
                    oldest_from_tail: page_oldest,
                    earlier: next_earlier,
                    just_prepended: false,
                });
            }
            return 0;
        }
        page.screen.rows.truncate(added);
        let earlier = EarlierHistory::from_remaining(page.earlier);
        let history = self.scrollback.get_or_insert_with(|| TerminalScrollback {
            older: VecDeque::new(),
            tail,
            oldest_from_tail: page_oldest,
            earlier,
            just_prepended: false,
        });
        for row in page.screen.rows.into_iter().rev() {
            history.older.push_front(row);
        }
        history.oldest_from_tail = page_oldest;
        history.earlier = earlier;
        history.just_prepended = true;
        if let Some(selection) = self.selection.as_mut() {
            selection.shift_rows(added);
        }
        added
    }

    pub(super) fn follow_terminal_tail(
        &mut self,
        near_bottom: bool,
        scroll_on_output: bool,
    ) -> bool {
        if self
            .scrollback
            .as_mut()
            .is_some_and(|history| std::mem::take(&mut history.just_prepended))
        {
            return false;
        }
        if self.selection.is_some_and(|selection| selection.dragging) {
            self.freeze_displayed_tail();
            return false;
        }
        let forced_jump = std::mem::take(&mut self.scroll_to_bottom_pending);
        let has_new_output = self.output_seq != self.last_followed_seq;
        self.last_followed_seq = self.output_seq;

        let jump_for_output = scroll_on_output && has_new_output;
        if !forced_jump && !near_bottom && !jump_for_output {
            self.freeze_displayed_tail();
            return false;
        }
        let mut resumed_stale_tail = false;
        if let Some(history) = &self.scrollback {
            let tail_stale = self
                .screen
                .as_ref()
                .is_some_and(|screen| !Arc::ptr_eq(&history.tail, screen));
            if forced_jump || jump_for_output || tail_stale {
                let had_older = !history.older.is_empty();
                self.scrollback = None;
                self.scrollback_pending = None;
                if had_older || tail_stale {
                    self.selection = None;
                }
                resumed_stale_tail = tail_stale;
            } else if history.older.is_empty() {
                // Returned to the bottom of an unchanged live snapshot: unpin without clearing
                // any existing selection since displayed row indices did not change.
                self.scrollback = None;
                self.scrollback_pending = None;
            }
        }
        forced_jump || jump_for_output || has_new_output || resumed_stale_tail
    }
}

fn history_prefix_len(rows: &[ScreenRow], anchor: &[&ScreenRow], expected: usize) -> Option<usize> {
    if anchor.is_empty() {
        return None;
    }
    rows.windows(anchor.len())
        .enumerate()
        .filter(|(_, rows)| rows.iter().eq(anchor.iter().copied()))
        .map(|(index, _)| index)
        .min_by_key(|index| index.abs_diff(expected))
}

pub(super) fn terminal_row_height(font_size: Pixels, window: &Window) -> Pixels {
    window.pixel_snap(font_size * (20.6 / DEFAULT_TERMINAL_FONT_SIZE))
}

impl Workbench {
    pub(super) fn effective_terminal_row_height(
        &self,
        item_count: usize,
        fallback: Pixels,
    ) -> Pixels {
        if item_count == 0 {
            return fallback;
        }
        let measured = self
            .terminal_scroll
            .0
            .borrow()
            .last_item_size
            .and_then(|size| {
                let height = f32::from(size.contents.height);
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "terminal row counts fit cleanly in f32 for uniform_list item height division"
                )]
                let count = item_count as f32;
                (height > 0.0 && count > 0.0).then(|| px(height / count))
            });
        measured.unwrap_or(fallback)
    }

    pub(super) fn scroll_terminal_history(
        &mut self,
        delta: Pixels,
        row_height: Pixels,
        cx: &mut Context<Self>,
    ) {
        let wheel_offset = self.terminal_scroll.0.borrow().base_handle.offset().y;
        if delta <= px(0.) || wheel_offset < -px(1.) {
            return;
        }
        let Some(session) = self.sessions.get_mut(self.active_session) else {
            return;
        };
        if session.source != "device"
            || session.scrollback_pending.is_some()
            || session
                .selection
                .is_some_and(|selection| selection.dragging)
        {
            return;
        }
        let live_cap = self.device.terminal_buffer().1;
        let skip = match &session.scrollback {
            Some(history) => match history.earlier {
                EarlierHistory::Available(_) => {
                    history.oldest_from_tail.saturating_sub(HISTORY_OVERLAP - 1)
                }
                EarlierHistory::Unknown if history.tail.rows.len() >= live_cap => 0,
                EarlierHistory::Unknown | EarlierHistory::Exhausted => return,
            },
            None if session
                .screen
                .as_ref()
                .is_some_and(|screen| screen.rows.len() >= live_cap) =>
            {
                0
            }
            _ => return,
        };
        let request = match self.device.read_terminal_history(&session.dut_id, skip) {
            Ok(request) => request,
            Err(error) => {
                self.notify_toast(i18n::message(
                    "terminalHistoryFailed",
                    &[("error", &error.to_string())],
                ));
                cx.notify();
                return;
            }
        };
        let id = session.session_id.clone();
        let request_id = uuid::Uuid::new_v4();
        session.scrollback_pending = Some(request_id);
        cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |this, cx| {
                let Some(index) = this
                    .sessions
                    .iter()
                    .position(|session| session.session_id == id)
                else {
                    return;
                };
                if this.sessions[index].scrollback_pending != Some(request_id) {
                    return;
                }
                this.sessions[index].scrollback_pending = None;
                if index != this.active_session {
                    return;
                }
                let scroll = this.terminal_scroll.0.borrow().base_handle.clone();
                let mut offset = scroll.offset();
                if offset.y < -px(1.)
                    || this.sessions[index]
                        .selection
                        .is_some_and(|selection| selection.dragging)
                {
                    return;
                }
                match result {
                    Ok(page) => {
                        let item_count_before = this
                            .terminal_item_layout(index)
                            .map_or(0, |(_, _, count, _)| count);
                        let effective_row_height =
                            this.effective_terminal_row_height(item_count_before, row_height);
                        let added = this.sessions[index].prepend_terminal_history(page);
                        if added > 0 {
                            // Compensate for the inserted height in the same scroll list.
                            offset.y = offset.y.max(wheel_offset).min(delta)
                                - effective_row_height
                                    * f32::from(u16::try_from(added).unwrap_or(u16::MAX));
                            offset.y = offset.y.min(px(0.));
                            scroll.set_offset(offset);
                            cx.notify();
                        }
                    }
                    Err(error) => {
                        this.notify_toast(i18n::message(
                            "terminalHistoryFailed",
                            &[("error", &error.to_string())],
                        ));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    pub(super) fn scroll_terminal_on_input(&mut self, cx: &mut Context<Self>) {
        if !self.settings.terminal.jump_to_latest {
            return;
        }
        if let Some(session) = self.sessions.get_mut(self.active_session) {
            let had_scrollback = session.scrollback.take().is_some();
            if session.scrollback_pending.take().is_some() || had_scrollback {
                session.selection = None;
            }
            session.scroll_to_bottom_pending = true;
        }
        self.terminal_scroll.scroll_to_bottom();
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Terminal input
    // ------------------------------------------------------------------

    /// Demo session input line (old behavior: local buffer + [TX] echo demonstration).
    pub(super) fn send_input(&mut self, cx: &mut Context<Self>) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.scroll_terminal_on_input(cx);
        if text == "clear" {
            if let Some(view) = self.sessions.get_mut(self.active_session) {
                view.lines.clear();
            }
        } else {
            let Some(view) = self.sessions.get(self.active_session) else {
                return;
            };
            let dut_id = view.dut_id.clone();
            if let Err(err) =
                self.device
                    .write_line(&dut_id, duthub_core::lease::WriteIdentity::gui(), &text)
                && let Some(view) = self.sessions.get_mut(self.active_session)
            {
                view.lines.push(LogLine {
                    text: SharedString::from(i18n::message(
                        "writeFailedNote",
                        &[("error", &err.to_string())],
                    )),
                    kind: LineKind::Error,
                });
                view.output_seq = view.output_seq.wrapping_add(1);
            }
        }
        self.input.clear();
        cx.notify();
    }

    /// Real session keyboard path (`PuTTY` behavior): Bytes are sent as they are, without any
    /// trailing lines or local echo. (Echo/prompt is rendered by the device shell through the
    /// emulator.) If there is a failure, it is logged as an annotation.
    #[tracing::instrument(name = "serial.key_enqueue", skip_all, fields(bytes = bytes.len()))]
    pub(super) fn send_bytes(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        self.scroll_terminal_on_input(cx);
        let Some(view) = self.sessions.get(self.active_session) else {
            return;
        };
        let dut_id = view.dut_id.clone();
        if let Err(err) =
            self.device
                .write_bytes(&dut_id, duthub_core::lease::WriteIdentity::gui(), bytes)
            && let Some(view) = self.sessions.get_mut(self.active_session)
        {
            view.lines.push(LogLine {
                text: SharedString::from(i18n::message(
                    "writeFailedNote",
                    &[("error", &err.to_string())],
                )),
                kind: LineKind::Error,
            });
            view.output_seq = view.output_seq.wrapping_add(1);
            cx.notify();
        }
    }

    pub(super) fn paste_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let Some(view) = self.sessions.get(self.active_session) else {
            return;
        };
        if view.source != "device" {
            // Demo session: append printable clipboard text into the local input line.
            let mut sanitized = text.replace(['\r', '\n'], " ");
            sanitized.retain(|ch| !ch.is_control());
            if !sanitized.is_empty() {
                self.scroll_terminal_on_input(cx);
                let remaining = 4096usize.saturating_sub(self.input.len());
                for ch in sanitized.chars() {
                    if self.input.len() + ch.len_utf8() > 4096 || remaining == 0 {
                        break;
                    }
                    self.input.push(ch);
                }
                cx.notify();
            }
            return;
        }
        // Wrap with bracketed-paste sequences only when the device enabled DECSET ?2004.
        let bracketed_paste = view
            .screen
            .as_ref()
            .is_some_and(|screen| screen.bracketed_paste);
        let Some(bytes) = prepare_paste_bytes(&text, bracketed_paste) else {
            return;
        };
        self.send_bytes(&bytes, cx);
    }

    pub(super) fn terminal_item_layout(
        &self,
        session_idx: usize,
    ) -> Option<(usize, usize, usize, bool)> {
        let session = self.sessions.get(session_idx)?;
        let is_device = session.source == "device";
        let annotations = session.lines.len();
        let rows_len = session.displayed_rows_len();
        let item_count = annotations + rows_len + usize::from(!is_device);
        Some((annotations, rows_len, item_count, is_device))
    }

    pub(super) fn terminal_item_text(
        &self,
        session_idx: usize,
        item_idx: usize,
    ) -> Option<std::borrow::Cow<'_, str>> {
        let session = self.sessions.get(session_idx)?;
        let (annotations, rows_len, item_count, is_device) =
            self.terminal_item_layout(session_idx)?;
        if item_idx >= item_count {
            return None;
        }
        if item_idx < annotations {
            return session
                .lines
                .get(item_idx)
                .map(|line| std::borrow::Cow::Borrowed(line.text.as_ref()));
        }
        if item_idx < annotations + rows_len {
            return session
                .displayed_row(item_idx - annotations)
                .map(|row| std::borrow::Cow::Borrowed(row.text.as_str()));
        }
        if !is_device {
            return Some(std::borrow::Cow::Owned(format!("root:~# {}", self.input)));
        }
        None
    }

    pub(super) fn hit_test_terminal_point(
        &self,
        session_idx: usize,
        position: Point<Pixels>,
        closest: bool,
        window: &Window,
    ) -> Option<crate::terminal_selection::TextPoint> {
        use crate::terminal_selection::{RowHit, TextPoint, hit_test_row};

        let (_, _, item_count, _) = self.terminal_item_layout(session_idx)?;
        if item_count == 0 {
            return None;
        }
        let (bounds, offset) = {
            let state = self.terminal_scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        if bounds.size.height <= px(0.) {
            return None;
        }
        let (font_family, font_size, _) = self.terminal_font(window);
        let row_height =
            self.effective_terminal_row_height(item_count, terminal_row_height(font_size, window));
        let rel_y = f32::from(position.y - bounds.origin.y) - f32::from(offset.y);
        match hit_test_row(rel_y, f32::from(row_height), item_count)? {
            RowHit::Above => Some(TextPoint { row: 0, col: 0 }),
            RowHit::Below => {
                let last_row = item_count - 1;
                let last_len = self
                    .terminal_item_text(session_idx, last_row)
                    .map_or(0, |text| text.chars().count());
                Some(TextPoint {
                    row: last_row,
                    col: last_len,
                })
            }
            RowHit::Row(row) => {
                let row_text = self
                    .terminal_item_text(session_idx, row)
                    .unwrap_or(std::borrow::Cow::Borrowed(""));
                let rel_x = position.x - bounds.origin.x - offset.x;
                let col = terminal_col_for_x(
                    row_text.as_ref(),
                    rel_x,
                    &font_family,
                    font_size,
                    closest,
                    window,
                );
                Some(TextPoint { row, col })
            }
        }
    }

    pub(super) fn begin_terminal_selection(
        &mut self,
        session_idx: usize,
        ev: &MouseDownEvent,
        window: &Window,
    ) {
        use crate::terminal_selection::TerminalSelection;

        let closest = ev.click_count <= 1;
        let Some(point) = self.hit_test_terminal_point(session_idx, ev.position, closest, window)
        else {
            if let Some(session) = self.sessions.get_mut(session_idx) {
                session.selection = None;
            }
            return;
        };
        let row_text = self
            .terminal_item_text(session_idx, point.row)
            .map_or_default(std::borrow::Cow::into_owned);
        let selection = match ev.click_count {
            0 | 1 => TerminalSelection::start_char(point),
            2 => TerminalSelection::start_word(point.row, point.col, &row_text),
            _ => TerminalSelection::start_line(point.row, &row_text),
        };
        if let Some(session) = self.sessions.get_mut(session_idx) {
            session.selection = Some(selection);
        }
    }

    pub(super) fn update_terminal_selection(
        &mut self,
        position: Point<Pixels>,
        left_pressed: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let active = self.active_session;
        let Some(granularity) = self
            .sessions
            .get(active)
            .and_then(|session| session.selection)
            .filter(|selection| selection.dragging)
            .map(|selection| selection.granularity)
        else {
            return;
        };
        if !left_pressed {
            self.finish_terminal_selection(cx);
            return;
        }
        let closest = granularity == crate::terminal_selection::SelectionGranularity::Char;
        let Some(point) = self.hit_test_terminal_point(active, position, closest, window) else {
            return;
        };
        let row_text = self
            .terminal_item_text(active, point.row)
            .map_or_default(std::borrow::Cow::into_owned);
        if let Some(session) = self.sessions.get_mut(active)
            && let Some(selection) = session.selection.as_mut()
            && selection.update(point, &row_text)
        {
            cx.notify();
        }
    }

    pub(super) fn finish_terminal_selection(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        let mut active_finished = false;
        for (index, session) in self.sessions.iter_mut().enumerate() {
            if let Some(selection) = session.selection.as_mut()
                && selection.dragging
            {
                selection.dragging = false;
                if selection.is_empty() {
                    session.selection = None;
                } else if index == self.active_session {
                    active_finished = true;
                }
                changed = true;
            }
        }
        if changed {
            if active_finished && self.settings.serial.copy_on_select {
                self.copy_terminal_selection(self.active_session, false, cx);
            }
            cx.notify();
        }
    }

    pub(super) fn has_terminal_selection(&self, session_idx: usize) -> bool {
        self.sessions
            .get(session_idx)
            .and_then(|session| session.selection)
            .is_some_and(|selection| !selection.is_empty())
    }

    pub(super) fn clear_terminal_selection(&mut self, session_idx: usize) -> bool {
        self.sessions
            .get_mut(session_idx)
            .is_some_and(|session| session.selection.take().is_some())
    }

    pub(super) fn copy_terminal_selection(
        &mut self,
        session_idx: usize,
        clear_after: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(selection) = self
            .sessions
            .get(session_idx)
            .and_then(|session| session.selection)
            .filter(|selection| !selection.is_empty())
        else {
            return false;
        };
        let Some((_, _, item_count, _)) = self.terminal_item_layout(session_idx) else {
            return false;
        };
        let Some(text) =
            selection.extract_text(item_count, |row| self.terminal_item_text(session_idx, row))
        else {
            return false;
        };
        crate::components::copy_text(cx, &text);
        if clear_after && let Some(session) = self.sessions.get_mut(session_idx) {
            session.selection = None;
        }
        true
    }

    // ------------------------------------------------------------------
    // Session status actions: H1.2 Newline / H1.3 Manual rate change / H1.4 Manual reconnection
    // ------------------------------------------------------------------

    /// Rate switching (`SetBaud` to turn the setting back on): Use the current confirmed value when
    /// the draft input is empty.
    /// Confirmation is based on the line "[serial] … @ N is turned on" in `view.baud` (displayed in
    /// the status bar in real time).
    pub(super) fn apply_session_baud(&mut self) {
        let Some(view) = self.sessions.get(self.active_session) else {
            return;
        };
        let dut_id = view.dut_id.clone();
        let draft = view.baud_draft.clone().unwrap_or_default();
        let current = view.baud;
        let text = if draft.is_empty() {
            if current == 0 {
                String::new()
            } else {
                current.to_string()
            }
        } else {
            draft
        };
        match text.parse::<u32>() {
            Ok(b) if (1..=10_000_000).contains(&b) => match self.device.set_baud(&dut_id, b) {
                Ok(()) => {
                    if let Some(view) = self.sessions.get_mut(self.active_session) {
                        view.baud_draft = None;
                    }
                    self.notify_toast(i18n::message("baudSwitchSent", &[("baud", &b.to_string())]));
                }
                Err(err) => self.notify_toast(i18n::message(
                    "baudSwitchFailed",
                    &[("error", &err.to_string())],
                )),
            },
            _ => self.notify_toast(i18n::label("baudInvalidRange")),
        }
    }

    pub(super) fn reset_serial_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.sessions.get_mut(self.active_session) else {
            return;
        };
        if view.source == "device"
            && let Err(error) = self.device.reset_terminal(&view.dut_id)
        {
            self.notify_toast(format!("{} {error}", i18n::label("resetTerminalFailed")));
            cx.notify();
            return;
        }
        view.lines.clear();
        view.scrollback = None;
        view.scrollback_pending = None;
        view.scroll_to_bottom_pending = true;
        view.selection = None;
        self.terminal_scroll.scroll_to_bottom();
        window.focus(&self.input_focus, cx);
        cx.notify();
    }

    /// Manual reconnection: Activate backoff waiting and retry immediately (harmless activation
    /// when the connection is normal).
    pub(super) fn request_manual_reconnect(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.sessions.get(self.active_session) else {
            return;
        };
        let dut_id = view.dut_id.clone();
        let reconnecting = view.link == LinkState::Reconnecting;
        match self.device.manual_reconnect(&dut_id) {
            Ok(()) => {
                self.notify_toast(if reconnecting {
                    i18n::label("reconnectNowRequested")
                } else {
                    i18n::label("reconnectArmedNextDrop")
                });
            }
            Err(err) => self.notify_toast(i18n::message(
                "reconnectRequestFailed",
                &[("error", &err.to_string())],
            )),
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(numbers: std::ops::Range<usize>) -> Vec<ScreenRow> {
        numbers
            .map(|number| ScreenRow {
                text: number.to_string(),
                fg: None,
                bg: None,
            })
            .collect()
    }

    #[test]
    fn terminal_history_prepends_before_live_rows_without_replacing_them() {
        let mut session =
            SessionView::new(String::new(), String::new(), String::new(), "device", 0);
        session.screen = Some(Arc::new(ScreenSnapshot {
            rows: rows(100..180),
            cursor_row: Some(79),
            cursor_char: Some(1),
            ..ScreenSnapshot::default()
        }));
        let page = TerminalHistoryPage {
            screen: ScreenSnapshot {
                rows: rows(50..190),
                ..ScreenSnapshot::default()
            },
            skip: 0,
            earlier: 50,
        };
        assert_eq!(session.prepend_terminal_history(page), 50);
        let older = TerminalHistoryPage {
            screen: ScreenSnapshot {
                rows: rows(0..70),
                ..ScreenSnapshot::default()
            },
            skip: 120,
            earlier: 0,
        };
        assert_eq!(session.prepend_terminal_history(older), 50);
        assert!(session.displayed_rows().eq(rows(0..180).iter()));
        assert_eq!(session.displayed_cursor(179), Some(1));
    }

    #[test]
    fn prepend_shifts_existing_selection_and_records_progress_on_overlap_miss() {
        use crate::terminal_selection::{TerminalSelection, TextPoint};

        let mut session =
            SessionView::new(String::new(), String::new(), String::new(), "device", 0);
        session.screen = Some(Arc::new(ScreenSnapshot {
            rows: rows(100..180),
            ..ScreenSnapshot::default()
        }));
        let mut selection = TerminalSelection::start_char(TextPoint { row: 10, col: 0 });
        selection.update(TextPoint { row: 12, col: 2 }, "112");
        selection.dragging = false;
        session.selection = Some(selection);

        let page = TerminalHistoryPage {
            screen: ScreenSnapshot {
                rows: rows(50..190),
                ..ScreenSnapshot::default()
            },
            skip: 0,
            earlier: 50,
        };
        assert_eq!(session.prepend_terminal_history(page), 50);
        assert_eq!(
            session.selection.unwrap().normalized(),
            (TextPoint { row: 60, col: 0 }, TextPoint { row: 62, col: 2 })
        );

        // When live output advanced past the first page overlap, fallback records earlier history.
        let mut drifted =
            SessionView::new(String::new(), String::new(), String::new(), "device", 0);
        drifted.screen = Some(Arc::new(ScreenSnapshot {
            rows: rows(0..10),
            ..ScreenSnapshot::default()
        }));
        let disjoint = TerminalHistoryPage {
            screen: ScreenSnapshot {
                rows: rows(500..600),
                ..ScreenSnapshot::default()
            },
            skip: 0,
            earlier: 500,
        };
        assert_eq!(drifted.prepend_terminal_history(disjoint), 0);
        let history = drifted.scrollback.as_ref().expect("scrollback initialized");
        assert_eq!(history.earlier, EarlierHistory::Available(500));
        assert_eq!(history.oldest_from_tail, 99);
    }

    #[test]
    fn follow_terminal_tail_respects_scroll_on_output_freezes_while_scrolled_up_and_honors_drag() {
        use crate::terminal_selection::{TerminalSelection, TextPoint};

        let mut session =
            SessionView::new(String::new(), String::new(), String::new(), "device", 0);
        let first = Arc::new(ScreenSnapshot {
            rows: rows(0..100),
            ..ScreenSnapshot::default()
        });
        session.absorb_serial_screen(Arc::clone(&first), 1);

        // Initial arrival at bottom follows output.
        assert!(session.follow_terminal_tail(true, false));
        // Idle frame at bottom does not keep scheduling redundant bottom scrolls.
        assert!(!session.follow_terminal_tail(true, false));

        // Scrolled up with scroll_on_output = false: freezes tail so new live frames do not shift rows.
        let second = Arc::new(ScreenSnapshot {
            rows: rows(10..110),
            ..ScreenSnapshot::default()
        });
        assert!(!session.follow_terminal_tail(false, false));
        session.absorb_serial_screen(Arc::clone(&second), 2);
        assert!(!session.follow_terminal_tail(false, false));
        assert_eq!(session.displayed_row(0).unwrap().text, "0");

        // Scrolling back down to bottom catches up to the latest live snapshot.
        assert!(session.follow_terminal_tail(true, false));
        assert!(session.scrollback.is_none());
        assert_eq!(session.displayed_row(0).unwrap().text, "10");

        // With scroll_on_output = true: idle scrolled-up view stays put until new output arrives.
        assert!(!session.follow_terminal_tail(false, true));
        let third = Arc::new(ScreenSnapshot {
            rows: rows(20..120),
            ..ScreenSnapshot::default()
        });
        session.absorb_serial_screen(Arc::clone(&third), 3);

        // Active mouse drag blocks snapping to bottom even when scroll_on_output = true.
        session.selection = Some(TerminalSelection::start_char(TextPoint { row: 5, col: 0 }));
        assert!(!session.follow_terminal_tail(false, true));
        assert_eq!(session.displayed_row(0).unwrap().text, "10");

        // Once dragging ends and new output arrives, scroll_on_output = true jumps to bottom.
        session.selection = None;
        let fourth = Arc::new(ScreenSnapshot {
            rows: rows(30..130),
            ..ScreenSnapshot::default()
        });
        session.absorb_serial_screen(fourth, 4);
        assert!(session.follow_terminal_tail(false, true));
        assert!(session.scrollback.is_none());
        assert_eq!(session.displayed_row(0).unwrap().text, "30");
    }
}
