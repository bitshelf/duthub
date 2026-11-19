use super::*;

impl Workbench {
    // ------------------------------------------------------------------
    // 终端输入
    // ------------------------------------------------------------------

    /// demo 会话输入行（旧行为：本地缓冲 + [TX] 回显演示）。
    pub(super) fn send_input(&mut self, cx: &mut Context<Self>) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
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
            }
        }
        self.input.clear();
        cx.notify();
    }

    /// real 会话键盘路径（PuTTY 行为）：字节原样下发，无行尾追加、无本地回显
    /// （回显/提示符由设备 shell 经仿真器渲染）。失败转注记行。
    #[tracing::instrument(name = "serial.key_enqueue", skip_all, fields(bytes = bytes.len()))]
    pub(super) fn send_bytes(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
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
            cx.notify();
        }
    }

    pub(super) fn paste_terminal(&mut self, cx: &mut Context<Self>) {
        if self
            .sessions
            .get(self.active_session)
            .is_some_and(|session| session.history.is_some())
        {
            return;
        }
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
        let annotations = if session.history.is_some() {
            0
        } else {
            session.lines.len()
        };
        let rows_len = session.displayed_screen().map_or(0, |sc| sc.rows.len());
        let item_count = annotations + rows_len + usize::from(!is_device);
        Some((annotations, rows_len, item_count, is_device))
    }

    pub(super) fn terminal_item_text<'a>(
        &'a self,
        session_idx: usize,
        item_idx: usize,
    ) -> Option<std::borrow::Cow<'a, str>> {
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
                .displayed_screen()
                .and_then(|screen| screen.rows.get(item_idx - annotations))
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
        let row_height = font_size * (20.6 / DEFAULT_TERMINAL_FONT_SIZE);
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
            .map(std::borrow::Cow::into_owned)
            .unwrap_or_default();
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
            .map(std::borrow::Cow::into_owned)
            .unwrap_or_default();
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
    // 会话状态条动作（H1.2 换行 / H1.3 波特率热切换 / H1.4 手动重连）
    // ------------------------------------------------------------------

    /// 波特率热切换（SetBaud 关闭重开路径）：输入草稿为空时用当前确认值。
    /// 确认以"[serial] … @ N 已打开"行回填 view.baud 为准（状态条实时显示）。
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
        view.history = None;
        view.history_pending = None;
        view.selection = None;
        self.terminal_scroll.scroll_to_bottom();
        window.focus(&self.input_focus, cx);
        cx.notify();
    }

    /// 手动重连：唤醒退避等待立即重试（连接正常时为无害唤醒）。
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
