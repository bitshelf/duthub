use super::{Workbench, Palette, Window, Context, Div, div, InteractiveElement, switch_panel_header, StatefulInteractiveElement, toggle, collapsible_panel, Styled, px, ParentElement, subtle, FluentBuilder, SessionStatus, SharedString, bundle, AppContext, btn, BtnVariant, IntoElement, Input, Disableable, TranscriptEntry, TranscriptKind, Stateful, panel_shell, In, field};

pub(super) fn connection_agent_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> Div {
    let Some(dut) = w.duts.selected() else {
        return div();
    };
    let id = dut.id.clone();
    let enabled = w.agent_bindings.is_enabled(&id);
    let header = switch_panel_header(
        "spark",
        crate::i18n::label("agentSessionTitle"),
        toggle("toggle-agent-session-details", enabled, p).on_click(cx.listener(
            move |this, checked, _, cx| {
                this.set_dut_agent_enabled(&id, *checked);
                cx.notify();
            },
        )),
        p,
    );
    let body = enabled.then(|| agent_session_panel(w, p, window, cx));
    collapsible_panel(header, body, p)
}

/// 「Agent 会话」面板（每 DUT）：goose 进程状态、启动/停止/取消、流式 transcript、
/// 每 DUT 独立草稿输入。真实会话在 `agent_host` 工作线程；本面板只读 `SessionView`
/// 并下发命令（停止只发请求，等待/杀进程树在工作线程，F12）。
pub(super) fn agent_session_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let Some(dut) = w.duts.selected().cloned() else {
        return div();
    };
    let mut body = div().flex().flex_col().gap(px(8.));
    if !w.agent_runtime_available() {
        let reason = if w.agent_self_test_mode() {
            crate::i18n::label("agentSessionSelfTest")
        } else {
            crate::i18n::label("agentSessionUnavailable")
        };
        body = body.child(subtle(reason, p).text_color(p.warn));
        return body;
    }
    // 发送成功后清空草稿（`set_value` 需要 Window，延到渲染帧执行）。
    w.clear_agent_prompt_if_requested(&dut.id, window, cx);

    let view = w.agent_host.as_ref().and_then(|host| host.view(&dut.id));
    let has_view = view.is_some();
    let status = view
        .as_ref()
        .map_or(SessionStatus::Stopped, |view| view.status());
    let running = has_view && !status.is_terminal() && status != SessionStatus::Stopped;
    let busy = status == SessionStatus::Busy;
    let ready = status == SessionStatus::Ready;
    let (status_text, status_color) = session_status_text(&status, has_view, p);
    // W4c：徽标/标签需要的当前选择提前取好（badge 行在 meta 组装时消费）。
    let selected_agent = w.agent_id_for_dut(&dut.id);
    let actual_launch = w.agent_host.as_ref().and_then(|host| host.launch(&dut.id));

    // 状态行：点 + 文案 +（已启动时）本地端口/会话 id 前缀。
    let mut meta = div()
        .flex()
        .items_center()
        .gap(px(8.))
        .child(div().size(px(6.)).rounded_full().bg(status_color))
        .child(
            div()
                .text_size(px(12.))
                .text_color(status_color)
                .child(SharedString::from(status_text)),
        );
    if let Some(port) = view.as_ref().and_then(|view| view.port()) {
        meta = meta.child(subtle(
            &crate::i18n::message("agentSessionPort", &[("port", &port.to_string())]),
            p,
        ));
    }
    if let Some(session_id) = view.as_ref().and_then(|view| view.session_id()) {
        let short: String = session_id.chars().take(8).collect();
        meta = meta.child(
            subtle(&short, p)
                .font_family(crate::theme::MONO_FONT)
                .text_size(px(10.)),
        );
    }
    // W4c：Authenticode 徽标按实际程序来源展示——内嵌 goose 显示 manifest
    // 记录状态；外部程序（或选中的外部条目）一律「未验证」，绝不谎称已签名。
    let (badge_source, badge_signature) = if let Some(spec) = &actual_launch { (
        duthub_desktop::agent_ui::program_source(Some(spec), false),
        spec.signature.clone(),
    ) } else {
        let selected_external = w
            .agents
            .agent(&selected_agent)
            .and_then(|agent| agent.program_path())
            .is_some();
        (
            duthub_desktop::agent_ui::program_source(None, selected_external),
            bundle::authenticode_status().to_owned(),
        )
    };
    meta = meta.child(authenticode_badge(
        "agent-session-authenticode",
        badge_source,
        &badge_signature,
        p,
    ));
    body = body.child(meta);
    // OS sandbox badge + trust-level review backed by the real MXC backend
    // status (`duthub_sandbox::status()`).
    let sandbox_status = w.cached_sandbox_status();
    let proof = duthub_desktop::agent_ui::sandbox_boundary_proof(&sandbox_status);
    let trust_level = w
        .agents
        .agents
        .iter()
        .find(|entry| entry.id == w.agent_id_for_dut(&dut.id)).map_or_else(|| "mcp_only".into(), |entry| entry.trust_level.clone());
    let trust_label = localized_trust_label(&trust_level, proof);
    let isolation_text = format_sandbox_status(&sandbox_status);
    body = body.child(
        subtle(
            &crate::i18n::message(
                "agentTrustLevelSummary",
                &[("level", trust_label), ("isolation", &isolation_text)],
            ),
            p,
        )
        .text_size(px(10.5)),
    );

    // 每 DUT 可选 Agent（默认 goose；外部 Agent 来自 <work_dir>/agents.json）。
    // W4b：会话运行中（非终止态）锁定下拉，显示实际 launch 的 agent id 与
    // 「下一次启动生效」——运行中的进程不会因改选而变化。
    let choices = w.acp_agent_choices();
    let selector_locked = duthub_desktop::agent_ui::running_agent_selector_disabled(&status);
    let mut ids: Vec<String> = Vec::new();
    let mut options: Vec<(String, usize)> = Vec::new();
    for (agent_id, name) in &choices {
        let suffix = if agent_id == duthub_core::agent_config::DEFAULT_AGENT_ID {
            crate::i18n::label("agentRegistryEmbeddedOrExternal")
        } else {
            crate::i18n::label("agentRegistryKindAcp")
        };
        options.push((format!("{name} · {suffix}"), ids.len()));
        ids.push(agent_id.clone());
    }
    let selected_index = ids.iter().position(|agent_id| agent_id == &selected_agent);
    let selected_label = selected_index
        .and_then(|index| options.get(index))
        .map_or_else(
            || {
                format!(
                    "{selected_agent} · {}",
                    crate::i18n::label("agentRegistryMissing")
                )
            },
            |(label, _)| label.clone(),
        );
    if selector_locked {
        // Locked selector: a static chip instead of the interactive dropdown.
        body = body.child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(8.))
                .child(subtle(crate::i18n::label("agentSessionAgentLabel"), p))
                .child(
                    div()
                        .min_h(px(33.))
                        .px(px(11.))
                        .py(px(6.))
                        .rounded(px(8.))
                        .bg(p.bg)
                        .border_1()
                        .border_color(p.border)
                        .text_size(px(12.5))
                        .text_color(p.muted)
                        .child(SharedString::from(selected_label)),
                ),
        );
        body = body.child(
            subtle(
                &duthub_desktop::agent_ui::running_launch_line(actual_launch.as_ref()),
                p,
            )
            .text_size(px(10.5)),
        );
        body = body.child(
            subtle(crate::i18n::label("agentSessionSelectorNextStart"), p).text_size(px(10.5)),
        );
    } else {
        let select_dut = dut.id.clone();
        let agent_select = w.select_field(
            "agent-session-agent",
            selected_label,
            options,
            selected_index,
            std::sync::Arc::new(move |this, index, _window, cx| {
                if let Some(agent_id) = ids.get(index) {
                    this.set_dut_agent(&select_dut, agent_id);
                    cx.notify();
                }
            }),
            p,
            cx,
        );
        body = body.child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(8.))
                .child(subtle(crate::i18n::label("agentSessionAgentLabel"), p))
                .child(div().w(px(260.)).min_w_0().child(agent_select)),
        );
    }

    // 动作：启动（未运行/已结束）、停止（运行中）、取消本轮（Busy）。
    let dut_start = dut.id.clone();
    let dut_stop = dut.id.clone();
    let dut_cancel = dut.id.clone();
    let mut actions = div().flex().items_center().gap(px(8.));
    if !running {
        actions = actions.child(
            btn(
                "agent-session-start",
                crate::i18n::label("agentSessionStart"),
                &BtnVariant::TinyPrimary,
                p,
            )
            .on_click(cx.listener(move |this, _ev, _win, cx| {
                this.start_agent_session(&dut_start, cx);
            })),
        );
    }
    if running {
        actions = actions.child(
            btn(
                "agent-session-stop",
                crate::i18n::label("agentSessionStop"),
                &BtnVariant::Tiny,
                p,
            )
            .on_click(cx.listener(move |this, _ev, _win, cx| {
                this.stop_agent_session(&dut_stop);
                cx.notify();
            })),
        );
    }
    if busy {
        actions = actions.child(
            btn(
                "agent-session-cancel",
                crate::i18n::label("agentSessionCancel"),
                &BtnVariant::Tiny,
                p,
            )
            .on_click(cx.listener(move |this, _ev, _win, cx| {
                this.cancel_agent_turn(&dut_cancel, cx);
            })),
        );
    }
    body = body.child(actions);

    // 流式 transcript：uniform_list 只构建可见行（AGENTS #49 同口径）。
    // 贴底跟随（距底 65px 内）与终端同一语义：用户上滚阅读历史时新条目不得拽回底部。
    // W4c：跟随判定改用 `SessionView::total_entries()` 单调序号——内存上限
    // 2000 条截断后 `transcript.len()` 停涨，只有它能在 2000 条后继续贴底。
    let item_count = view
        .as_ref()
        .map_or(0, |view| {
            // A poisoned transcript lock only means some worker panicked while
            // holding it; the render path must degrade, not panic the UI thread.
            view.transcript
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
        });
    let scroll = w
        .agent_transcript_scroll
        .entry(dut.id.clone())
        .or_default()
        .clone();
    let seen = w.agent_transcript_seen.get(&dut.id).copied().unwrap_or(0);
    let total_entries = view.as_ref().map_or(0, |view| view.total_entries());
    if duthub_desktop::agent_ui::should_follow(seen, total_entries) {
        let near_bottom = {
            let state = scroll.0.borrow();
            let offset = state.base_handle.offset().y;
            let max = state.base_handle.max_offset().y;
            // offset 向下滚动为负，到底时 offset.y ≈ -max；距底部距离 = max + offset
            max <= px(0.) || (max + offset) < px(65.)
        };
        if near_bottom {
            scroll.scroll_to_bottom();
        }
        w.agent_transcript_seen
            .insert(dut.id.clone(), total_entries);
    }
    if item_count == 0 {
        body = body.child(subtle(crate::i18n::label("agentSessionEmptyTranscript"), p));
    } else {
        let transcript_dut = dut.id.clone();
        let transcript_tag = w.transcript_agent_tag(&dut.id);
        let weak = cx.entity().downgrade();
        body = body.child(
            gpui_kit::uniform_list("agent-transcript", item_count, move |range, _window, cx| {
                let Some(workbench) = weak.upgrade() else {
                    return Vec::new();
                };
                let this = workbench.read(cx);
                let Some(view) = this
                    .agent_host
                    .as_ref()
                    .and_then(|host| host.view(&transcript_dut))
                else {
                    return Vec::new();
                };
                let entries = view
                    .transcript
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut items = Vec::with_capacity(range.end - range.start);
                for index in range {
                    let Some(entry) = entries.get(index) else {
                        continue;
                    };
                    items.push(agent_transcript_row(entry, &transcript_tag, p).into_any_element());
                }
                items
            })
            .track_scroll(&scroll)
            .h(px(190.))
            .w_full()
            .border_1()
            .border_color(p.line)
            .rounded(px(6.)),
        );
    }

    // 每 DUT 独立草稿（F13）：实体按 DUT id 常驻，切换 DUT 换输入框而非清文本。
    let prompt_state = w.agent_prompt_state(&dut.id, window, cx);
    let dut_send = dut.id.clone();
    body = body.child(
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&prompt_state)
                        .id("agent-session-prompt")
                        .w_full(),
                ),
            )
            .child(
                btn(
                    "agent-session-send",
                    crate::i18n::label("agentSessionSend"),
                    &BtnVariant::TinyPrimary,
                    p,
                )
                .disabled(!ready)
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    this.send_agent_prompt(&dut_send, cx);
                })),
            ),
    );
    body
}

/// Transcript 单行（固定行高供 `uniform_list` 统一度量）。
/// W4c：Agent 行的说话者标签用当前 Agent 名（host.launch 或注册表定义名），
/// 不再硬编码 "GOOSE"。
pub(super) fn agent_transcript_row(
    entry: &TranscriptEntry,
    agent_tag: &str,
    p: &'static Palette,
) -> Div {
    let (tag, color) = match entry.kind {
        TranscriptKind::User => ("YOU".to_owned(), p.accent),
        TranscriptKind::Agent => (agent_tag.to_owned(), p.text),
        TranscriptKind::Thought => ("THINK".to_owned(), p.muted),
        TranscriptKind::Tool => ("TOOL".to_owned(), p.blue),
        TranscriptKind::System => ("·".to_owned(), p.faint),
        TranscriptKind::Error => ("ERR".to_owned(), p.red),
    };
    div()
        .h(px(22.))
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(6.))
        .child(
            div()
                .w(px(56.))
                .flex_none()
                .min_w_0()
                .truncate()
                .text_size(px(9.5))
                .font_family(crate::theme::MONO_FONT)
                .text_color(color)
                .child(SharedString::from(tag)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(11.5))
                .text_color(color)
                .child(SharedString::from(entry.text.clone())),
        )
}

/// Authenticode 徽标（W4c）：内嵌 goose 只展示 manifest 记录；外部程序一律
/// 「未验证」（`agent_ui::authenticode_badge` 决定变体，文案在 bin 层翻译），
/// 不宣称已验证。
pub(super) fn authenticode_badge(
    id: &'static str,
    source: duthub_desktop::agent_ui::ProgramSource,
    signature: &str,
    p: &'static Palette,
) -> Stateful<Div> {
    let badge = duthub_desktop::agent_ui::authenticode_badge(source, signature);
    let (text, unverified) = match &badge {
        duthub_desktop::agent_ui::AuthenticodeBadge::ExternalUnverified => (
            crate::i18n::label("agentProgramUnverified").to_owned(),
            true,
        ),
        duthub_desktop::agent_ui::AuthenticodeBadge::EmbeddedNotSigned => (
            crate::i18n::label("gooseAuthenticodeNotSigned").to_owned(),
            true,
        ),
        duthub_desktop::agent_ui::AuthenticodeBadge::EmbeddedStatus(status) => (
            crate::i18n::message("gooseAuthenticodeStatus", &[("status", status)]),
            false,
        ),
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .px(px(7.))
        .py(px(3.))
        .rounded(px(6.))
        .border_1()
        .border_color(p.border)
        .bg(p.raised)
        .text_size(px(10.))
        .font_family(crate::theme::MONO_FONT)
        .text_color(if unverified { p.warn } else { p.muted })
        .child(SharedString::from(text))
}

/// W4c 右栏 Agent Inspector：读取真实 `SessionView`（launch/本地端口/session
/// id/stderr/OS 隔离/permission-denied 系统条目）与审计最近 200 条中本会话
/// 凭据的 `tool.call`。无会话数据时只显示空态——演示工具卡（roles/policy）
/// 已随演示面板一并卸载，绝不再展示。
pub(crate) fn agent_inspector_panel(w: &mut Workbench, p: &'static Palette) -> gpui_kit::Div {
    let Some(dut) = w.panel_dut().cloned() else {
        return div();
    };
    let mut body = div().flex().flex_col().gap(px(8.));
    let view = w.agent_host.as_ref().and_then(|host| host.view(&dut.id));
    let Some(view) = view else {
        body = body.child(subtle(crate::i18n::label("agentInspectorEmpty"), p));
        return panel_shell("spark", crate::i18n::label("agentInspectorTitle"), body, p);
    };
    // 进程事实：实际 launch 的程序（含版本/来源）、本地端口、ACP session id。
    let launch = w.agent_host.as_ref().and_then(|host| host.launch(&dut.id));
    body = body.child(inspector_row(
        crate::i18n::label("agentInspectorLaunch"),
        &launch
            .as_ref().map_or_else(|| crate::i18n::label("agentInspectorNotRecorded").to_owned(), duthub_desktop::agent_host::AgentLaunchSpec::display),
        p,
    ));
    if let Some(port) = view.port() {
        body = body.child(inspector_row(
            crate::i18n::label("agentInspectorPort"),
            &crate::i18n::message("agentSessionPort", &[("port", &port.to_string())]),
            p,
        ));
    }
    if let Some(session_id) = view.session_id() {
        body = body.child(inspector_row(
            crate::i18n::label("agentInspectorSessionId"),
            &session_id,
            p,
        ));
    }
    // OS isolation facts recorded by the worker from `duthub_sandbox::status()`.
    let isolation = view.os_isolation().map_or_else(
        || crate::i18n::label("agentInspectorNotRecorded").to_owned(),
        |status| format_sandbox_status(&status),
    );
    body = body.child(inspector_row(
        crate::i18n::label("agentInspectorOsIsolation"),
        &isolation,
        p,
    ));
    // stderr 尾部（进程启动失败的排障线索；非空才显示）。
    let stderr = view.stderr_tail();
    if !stderr.trim().is_empty() {
        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(p.muted)
                        .child(SharedString::from(
                            crate::i18n::label("agentInspectorStderr").to_owned(),
                        )),
                )
                .child(
                    div()
                        .id("agent-inspector-stderr")
                        .max_h(px(90.))
                        .overflow_y_scroll()
                        .rounded(px(6.))
                        .bg(p.bg)
                        .border_1()
                        .border_color(p.line)
                        .px(px(8.))
                        .py(px(6.))
                        .text_size(px(10.))
                        .font_family(crate::theme::MONO_FONT)
                        .line_height(px(14.))
                        .text_color(p.muted)
                        .child(SharedString::from(stderr)),
                ),
        );
    }
    // permission-denied 系统条目（AcpEvent::PermissionDenied 写入的记录）。
    let denials = duthub_desktop::agent_ui::permission_denial_entries(&view.transcript_snapshot());
    body = body.child(
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(p.muted)
                    .child(SharedString::from(
                        crate::i18n::label("agentInspectorPermissionDenied").to_owned(),
                    )),
            )
            .child(if denials.is_empty() {
                subtle(crate::i18n::label("agentInspectorNoDenials"), p)
            } else {
                div().children(denials.iter().map(|entry| {
                    subtle(entry, p)
                        .text_size(px(10.))
                        .font_family(crate::theme::MONO_FONT)
                }))
            }),
    );
    // 工具调用审计：最近 200 条中 credential_id == 本会话凭据的 tool.call。
    // 无凭据的会话不继承无归属事件（空列表就是诚实的空态）。
    let audit_rows = w
        .device
        .audit_sink()
        .map_or_default(|sink| {
            duthub_desktop::agent_ui::inspector_audit_events(
                &sink.recent(200),
                view.credential_id().as_deref(),
            )
        });
    body = body.child(
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(p.muted)
                    .child(SharedString::from(
                        crate::i18n::label("agentInspectorAudit").to_owned(),
                    )),
            )
            .child(if audit_rows.is_empty() {
                subtle(crate::i18n::label("agentInspectorNoAudit"), p)
            } else {
                div().children(audit_rows.iter().map(|event| {
                    let color = match event.result {
                        duthub_core::audit::AuditOutcome::Ok => p.muted,
                        duthub_core::audit::AuditOutcome::Denied => p.red,
                        duthub_core::audit::AuditOutcome::Error => p.warn,
                    };
                    div()
                        .text_size(px(10.))
                        .font_family(crate::theme::MONO_FONT)
                        .text_color(color)
                        .truncate()
                        .child(SharedString::from(format!(
                            "{} · {} · {}",
                            event.action,
                            event.result.as_str(),
                            event.detail.as_deref().unwrap_or("")
                        )))
                }))
            }),
    );
    panel_shell("spark", crate::i18n::label("agentInspectorTitle"), body, p)
}

fn localized_trust_label(
    trust_level: &str,
    proof: duthub_desktop::agent_ui::SandboxBoundaryProof,
) -> &'static str {
    use crate::i18n::label;
    use duthub_desktop::agent_ui::SandboxBoundaryProof;
    match trust_level.trim() {
        "os_sandboxed" if proof == SandboxBoundaryProof::VerifiedByBackend => {
            label("agentTrustLevelOsSandboxed")
        }
        "os_sandboxed" => label("agentTrustLevelUnverified"),
        "verified_readonly" => label("agentTrustLevelVerifiedReadonly"),
        _ => label("agentTrustLevelMcpOnly"),
    }
}

fn format_sandbox_status(status: &duthub_sandbox::SandboxStatus) -> String {
    use crate::i18n::{label, message};
    use duthub_desktop::agent_ui::{SandboxBadgeState, sandbox_badge_state};
    match sandbox_badge_state(status) {
        SandboxBadgeState::Ready {
            backend,
            fs_deny,
            network,
            ui_isolation,
            denial_capture,
        } => {
            let mut caps = Vec::new();
            if fs_deny {
                caps.push(label("agentSandboxCapFs"));
            }
            if network {
                caps.push(label("agentSandboxCapNet"));
            }
            if ui_isolation {
                caps.push(label("agentSandboxCapUi"));
            }
            if denial_capture {
                caps.push(label("agentSandboxCapCapture"));
            }
            message(
                "agentSandboxReady",
                &[("backend", backend), ("caps", &caps.join(" · "))],
            )
        }
        SandboxBadgeState::Unavailable { backend, detail } => {
            let reason = if detail.trim().is_empty() {
                label("agentSandboxUnknownReason")
            } else {
                detail.as_str()
            };
            message(
                "agentSandboxUnavailable",
                &[("backend", backend), ("reason", reason)],
            )
        }
    }
}

/// Inspector 的一行「label: value」。
pub(super) fn inspector_row(label: &'static str, value: &str, p: &'static Palette) -> Div {
    div()
        .flex()
        .gap(px(8.))
        .items_start()
        .child(
            div()
                .w(px(72.))
                .flex_none()
                .text_size(px(10.))
                .text_color(p.muted)
                .child(SharedString::from(label)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(10.5))
                .font_family(crate::theme::MONO_FONT)
                .text_color(p.text)
                .child(SharedString::from(value.to_owned())),
        )
}

pub(super) fn session_status_text(
    status: &SessionStatus,
    has_view: bool,
    p: &'static Palette,
) -> (String, gpui_kit::Rgba) {
    if !has_view {
        return (
            crate::i18n::label("agentStatusNotStarted").to_owned(),
            p.faint,
        );
    }
    match status {
        SessionStatus::Stopped => (crate::i18n::label("agentStatusStopped").to_owned(), p.muted),
        SessionStatus::Starting => (crate::i18n::label("agentStatusStarting").to_owned(), p.blue),
        SessionStatus::Ready => (crate::i18n::label("agentStatusReady").to_owned(), p.prompt),
        SessionStatus::Busy => (crate::i18n::label("agentStatusBusy").to_owned(), p.accent),
        SessionStatus::Stopping => (crate::i18n::label("agentStatusStopping").to_owned(), p.warn),
        SessionStatus::Failed(message) => (
            format!("{} · {message}", crate::i18n::label("agentStatusFailed")),
            p.red,
        ),
    }
}

pub(super) fn builtin_goose_status_line(
    state: &duthub_desktop::agent_host::GooseInstallState,
) -> &'static str {
    use duthub_desktop::agent_host::GooseInstallState as State;
    match state {
        State::Downloaded { .. } => crate::i18n::label("gooseRuntimeDownloaded"),
        State::Path { .. } => crate::i18n::label("gooseRuntimePath"),
        State::NotInstalled => crate::i18n::label("gooseRuntimeNotInstalled"),
    }
}

pub(crate) fn agent_runtime_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> Div {
    use crate::i18n::{label, message};

    let agent = &w.settings.agent;
    let mut models = agent
        .providers
        .iter()
        .filter(|provider| {
            provider.enabled
                && duthub_desktop::agent_host::model_env::protocol_supported(provider.protocol)
        })
        .flat_map(|provider| {
            provider
                .models
                .iter()
                .filter(|model| model.enabled)
                .map(move |model| (provider, model))
        })
        .peekable();
    let any_model = models.peek().is_some();
    let active = models.find(|(provider, model)| {
        agent.active_provider_id.as_deref() == Some(provider.id.as_str())
            && agent.active_model_id.as_deref() == Some(model.id.as_str())
    });
    let model_status = match active {
        Some((provider, model)) => message(
            "agentRuntimeActiveModel",
            &[("provider", &provider.name), ("model", &model.id)],
        ),
        None if any_model => label("agentRuntimeNoSelection").to_owned(),
        None => label("agentRuntimeNoModel").to_owned(),
    };

    let external = w.goose_external();
    let goose_state = w.agent.goose_install_state();
    let status = match (&external, &w.goose_probe) {
        (Some(_), Some(probe)) if !w.goose_probe_pending => message(
            "agentRuntimeVersionExternal",
            &[("version", probe.version.as_deref().unwrap_or("—"))],
        ),
        (Some(_), _) => label("agentRuntimeVersionExternalPending").to_owned(),
        (None, _) => builtin_goose_status_line(&goose_state).to_owned(),
    };
    let installed_path = match &goose_state {
        duthub_desktop::agent_host::GooseInstallState::Downloaded { path, .. }
        | duthub_desktop::agent_host::GooseInstallState::Path { path } => {
            Some(duthub_core::store::plain_path(path).display().to_string())
        }
        duthub_desktop::agent_host::GooseInstallState::NotInstalled => None,
    };
    let path_field = w.path_input(
        In::GoosePath,
        installed_path
            .as_deref()
            .unwrap_or_else(|| label("goosePathPlaceholder")),
        p,
        window,
        cx,
    );
    let mut status_row = div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap(px(10.))
        .child(div().text_size(px(13.)).text_color(p.text).child(status));
    if external.is_none() && goose_state.needs_install() {
        status_row = status_row.child(
            btn(
                "goose-install-open-downloads",
                label("gooseInstallOpenDownloads"),
                &BtnVariant::Normal,
                p,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.set_config_page(Some(crate::workspace::ConfigPage::ToolDownloads));
                cx.notify();
            })),
        );
    }
    let body = div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .child(status_row)
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap(px(10.))
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(p.text)
                        .child(model_status),
                )
                .child(
                    btn(
                        "goose-model-save",
                        label("gooseModelSave"),
                        &BtnVariant::Normal,
                        p,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_goose_model();
                        cx.notify();
                    })),
                ),
        )
        .child(field(label("goosePath"), path_field, None, p).mb_0())
        .child(
            div()
                .flex()
                .flex_wrap()
                .justify_end()
                .gap(px(8.))
                .child(
                    btn(
                        "goose-external-clear",
                        label("goosePathClear"),
                        &BtnVariant::Normal,
                        p,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.clear_goose_external();
                        cx.notify();
                    })),
                )
                .child(
                    btn(
                        "goose-external-save",
                        label("goosePathSave"),
                        &BtnVariant::Primary,
                        p,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_goose_external(cx);
                        cx.notify();
                    })),
                ),
        )
        .child(agent_registry_section(w, p, window, cx));
    panel_shell("spark", label("agentRuntimeTitle"), body, p)
}

/// Agent 注册表（`agents.json`）：添加/编辑/删除 ACP Agent。
///
/// 外部程序只允许从原生文件选择器显式添加；保存时计算并固定 SHA-256，goose
/// 还要求 `--version >= 1.60.2`。`envKeys` 只写键名，值在系统凭据库。
pub(super) fn agent_registry_section(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let agents = w.agents.agents.clone();
    let editing = w
        .agent_registry_edit
        .as_ref()
        .and_then(|edit| edit.id.clone());
    let mut section = div().flex().flex_col().gap(px(6.)).pt(px(12.));
    section = section.child(
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(p.text)
                    .child(SharedString::from(crate::i18n::label("agentRegistryTitle"))),
            )
            .child(
                btn(
                    "agent-registry-add",
                    crate::i18n::label("agentRegistryAdd"),
                    &BtnVariant::Tiny,
                    p,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.begin_agent_registry_new();
                    cx.notify();
                })),
            ),
    );
    if let Some(error) = &w.agents_error {
        section = section.child(subtle(error, p).text_color(p.warn));
    }
    if w.agent_registry_edit.is_some() {
        let title = editing.as_ref().map_or_else(
            || crate::i18n::label("agentRegistryNewTitle").to_owned(),
            |id| crate::i18n::message("agentRegistryEditTitle", &[("id", id)]),
        );
        let name_value = w.input_value(In::AgentRegName);
        let args_value = w.input_value(In::AgentRegArgs);
        let env_value = w.input_value(In::AgentRegEnvKeys);
        let name_field = w.text_field(
            In::AgentRegName,
            &name_value,
            crate::i18n::label("agentRegistryNamePlaceholder"),
            false,
            p,
            window,
            cx,
        );
        let program_field = w.path_input(
            In::AgentRegProgram,
            crate::i18n::label("agentRegistryProgramPlaceholder"),
            p,
            window,
            cx,
        );
        let args_field = w.text_field(
            In::AgentRegArgs,
            &args_value,
            crate::i18n::label("agentRegistryArgsPlaceholder"),
            true,
            p,
            window,
            cx,
        );
        let env_field = w.text_field(
            In::AgentRegEnvKeys,
            &env_value,
            crate::i18n::label("agentRegistryEnvPlaceholder"),
            true,
            p,
            window,
            cx,
        );
        let mut editor = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(8.))
            .bg(p.raised)
            .rounded(px(6.))
            .child(subtle(&title, p))
            .child(field(
                crate::i18n::label("agentRegistryName"),
                name_field,
                None,
                p,
            ))
            .child(field(
                crate::i18n::label("agentRegistryProgram"),
                program_field,
                None,
                p,
            ))
            .child(field(
                crate::i18n::label("agentRegistryArgs"),
                args_field,
                None,
                p,
            ));
        // W4b：args 必须是 JSON 数组——解析错误即时显示；保存时会再次校验，
        // 非法草稿不落盘。
        if let Err(error) = duthub_desktop::agent_ui::parse_args_json(&args_value) {
            editor = editor.child(subtle(&error, p).text_color(p.warn));
        }
        editor = editor.child(field(
            crate::i18n::label("agentRegistryEnvKeys"),
            env_field,
            None,
            p,
        ));
        // W4b：env 键名解析错误即时显示（保留键/重复键在保存前就被拒绝）。
        if let Err(error) = duthub_desktop::agent_ui::parse_env_keys_draft(&env_value) {
            editor = editor.child(subtle(&error, p).text_color(p.warn));
        }
        editor = editor.child(agent_env_value_editor(w, p, window, cx));
        editor = editor.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    btn(
                        "agent-registry-save",
                        crate::i18n::label("agentRegistrySave"),
                        &BtnVariant::TinyPrimary,
                        p,
                    )
                    .disabled(w.pending_registry_save.is_some())
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.save_agent_registry(cx);
                        cx.notify();
                    })),
                )
                .child(
                    btn(
                        "agent-registry-cancel",
                        crate::i18n::label("agentRegistryCancel"),
                        &BtnVariant::Tiny,
                        p,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.cancel_agent_registry_edit();
                        cx.notify();
                    })),
                ),
        );
        section = section.child(editor);
    }
    if agents.is_empty() {
        section = section.child(subtle(crate::i18n::label("agentRegistryEmpty"), p));
    }
    for agent in &agents {
        let agent_id = agent.id.clone();
        let edit_id = agent.id.clone();
        let delete_id = agent.id.clone();
        let toggle_id = agent.id.clone();
        let source = agent.program_path().map_or_else(
            || crate::i18n::label("agentRegistryEmbedded").to_owned(),
            std::borrow::ToOwned::to_owned,
        );
        let hash = agent
            .expected_sha256
            .as_deref().map_or_else(|| crate::i18n::label("agentRegistryHashUnpinned").to_owned(), |hash| format!("{}…", hash.get(..12).unwrap_or(hash)));
        let state = if agent.enabled {
            crate::i18n::label("agentRegistryEnabled")
        } else {
            crate::i18n::label("agentRegistryDisabled")
        };
        let builtin = agent.id == duthub_core::agent_config::DEFAULT_AGENT_ID;
        section = section.child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .p(px(8.))
                .bg(p.raised)
                .rounded(px(6.))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .justify_between()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .child(div().text_size(px(12.)).text_color(p.text).child(
                                    SharedString::from(format!(
                                        "{} · {} · {}",
                                        agent.name,
                                        agent.kind.label(),
                                        state
                                    )),
                                ))
                                .child(
                                    subtle(&format!("{} · {source} · {hash}", agent.id), p)
                                        .font_family(crate::theme::MONO_FONT)
                                        .text_size(px(10.)),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .child(
                                    btn(
                                        format!("agent-registry-edit-{agent_id}"),
                                        crate::i18n::label("agentRegistryEdit"),
                                        &BtnVariant::Tiny,
                                        p,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _event, _window, cx| {
                                            this.begin_agent_registry_edit(&edit_id);
                                            cx.notify();
                                        },
                                    )),
                                )
                                .child(
                                    btn(
                                        format!("agent-registry-toggle-{agent_id}"),
                                        if agent.enabled {
                                            crate::i18n::label("agentRegistryDisable")
                                        } else {
                                            crate::i18n::label("agentRegistryEnable")
                                        },
                                        &BtnVariant::Tiny,
                                        p,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _event, _window, cx| {
                                            this.toggle_agent_registry(&toggle_id);
                                            cx.notify();
                                        },
                                    )),
                                )
                                .child(
                                    btn(
                                        format!("agent-registry-delete-{agent_id}"),
                                        crate::i18n::label("agentRegistryDelete"),
                                        &BtnVariant::Tiny,
                                        p,
                                    )
                                    .disabled(builtin)
                                    .on_click(cx.listener(
                                        move |this, _event, _window, cx| {
                                            this.delete_agent_registry(&delete_id);
                                            cx.notify();
                                        },
                                    )),
                                ),
                        ),
                ),
        );
    }
    section
}

/// W4b env 值编辑器：选中键 + 掩码输入（keyring 写入/删除）。
///
/// 只在编辑**既有**条目时可用——新增条目的 id 在保存时才生成，先写值会留下
/// 孤儿凭据。掩码输入的原始值只进 keyring（`acp/<id>/env/<KEY>`），永不写进
/// agents.json；状态行只显示「已保存/未保存/读取失败」，不回显明文。
pub(super) fn agent_env_value_editor(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let mut block = div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .pt(px(4.))
        .child(subtle(crate::i18n::label("agentEnvValueTitle"), p));
    let Some(_agent_id) = w
        .agent_registry_edit
        .as_ref()
        .and_then(|edit| edit.id.clone())
    else {
        return block.child(subtle(crate::i18n::label("agentEnvNeedsSavedAgent"), p));
    };
    let keys =
        match duthub_desktop::agent_ui::parse_env_keys_draft(&w.input_value(In::AgentRegEnvKeys)) {
            Ok(keys) => keys,
            Err(_error) => return block,
        };
    if keys.is_empty() {
        return block.child(subtle(crate::i18n::label("agentEnvKeysFirst"), p));
    }
    // 键选择下拉（跟随当前键名草稿；选中键被移出草稿时自动取消选中）。
    let selected = w
        .agent_env_selected
        .clone()
        .filter(|key| keys.contains(key));
    let selected_index = selected
        .as_ref()
        .and_then(|key| keys.iter().position(|existing| existing == key));
    let selected_label =
        selected.unwrap_or_else(|| crate::i18n::label("agentEnvPickKey").to_owned());
    let options: Vec<(String, usize)> = keys
        .iter()
        .enumerate()
        .map(|(index, key)| (key.clone(), index))
        .collect();
    let picker_keys = keys.clone();
    let picker = w.select_field(
        "agent-env-key",
        selected_label,
        options,
        selected_index,
        std::sync::Arc::new(move |this, index, _window, cx| {
            if let Some(key) = picker_keys.get(index) {
                this.select_agent_env_key(key, cx);
                cx.notify();
            }
        }),
        p,
        cx,
    );
    block = block.child(
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .child(subtle(crate::i18n::label("agentEnvKeyLabel"), p))
            .child(div().w(px(220.)).min_w_0().child(picker)),
    );
    let value_draft = w.input_value(In::AgentRegEnvValue);
    let value_field = w.masked_field(
        In::AgentRegEnvValue,
        &value_draft,
        crate::i18n::label("agentEnvValuePlaceholder"),
        p,
        window,
        cx,
    );
    // 读取状态机：已保存 / 未保存 / 读取失败 / 读取中（W4c 后台 keyring 读）。
    let status_line = match &w.agent_env_value_state {
        Some(duthub_desktop::agent_ui::EnvValueState::Stored) => {
            subtle(crate::i18n::label("agentEnvStateStored"), p)
        }
        Some(duthub_desktop::agent_ui::EnvValueState::NotSet) => {
            subtle(crate::i18n::label("agentEnvStateNotSet"), p)
        }
        Some(duthub_desktop::agent_ui::EnvValueState::Pending) => {
            subtle(crate::i18n::label("agentEnvStatePending"), p).text_color(p.faint)
        }
        Some(duthub_desktop::agent_ui::EnvValueState::LookupFailed(error)) => subtle(
            &format!("{} {error}", crate::i18n::label("agentEnvReadFailed")),
            p,
        )
        .text_color(p.warn),
        None => subtle(crate::i18n::label("agentEnvStatePickKey"), p),
    };
    block = block
        .child(field(
            crate::i18n::label("agentEnvValueLabel"),
            value_field,
            None,
            p,
        ))
        .child(status_line)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    btn(
                        "agent-env-save",
                        crate::i18n::label("agentEnvWrite"),
                        &BtnVariant::TinyPrimary,
                        p,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.save_agent_env_value(cx);
                        cx.notify();
                    })),
                )
                .child(
                    btn(
                        "agent-env-delete",
                        crate::i18n::label("agentEnvDelete"),
                        &BtnVariant::Tiny,
                        p,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.delete_agent_env_value();
                        cx.notify();
                    })),
                ),
        );
    block
}
