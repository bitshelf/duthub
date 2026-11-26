use super::*;

// ---------------------------------------------------------------------------
// 设置
// ---------------------------------------------------------------------------

pub fn render_settings(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let mut page = div()
        .flex()
        .flex_col()
        .min_h_0()
        .max_w(px(980.))
        .mx_auto()
        .w_full()
        .px(px(24.))
        .pb(px(22.));
    page = page.child(
        div().pt(px(20.)).pb(px(8.)).child(
            div()
                .text_size(px(21.))
                .font_weight(gpui_kit::FontWeight(650.0))
                .text_color(p.text)
                .child(SharedString::from(
                    crate::i18n::t(w.lang, "settings").to_string(),
                )),
        ),
    );
    let mut grid = div().flex().flex_col();
    grid = grid.child(appearance_panel(w, p, window, cx));
    grid = grid.child(terminal_font_panel(w, p, window, cx));
    grid = grid.child(workspace_dir_panel(w, p, window, cx));
    grid = grid.child(commands_panel(w, p, cx));
    grid = grid.child(config_files_panel(w, p, cx));

    page.child(grid)
}

/// Standalone "Tools download" page (its own nav entry, settings-page shell):
/// the install rows plus the download-proxy policy.
pub fn render_tool_downloads(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let mut page = div()
        .flex()
        .flex_col()
        .min_h_0()
        .max_w(px(980.))
        .mx_auto()
        .w_full()
        .px(px(24.))
        .pb(px(22.));
    page = page.child(
        div().pt(px(20.)).pb(px(8.)).child(
            div()
                .text_size(px(21.))
                .font_weight(gpui_kit::FontWeight(650.0))
                .text_color(p.text)
                .child(crate::i18n::label("toolsDownload")),
        ),
    );
    let mut grid = div().flex().flex_col();
    grid = grid.child(tools_download_panel(w, p, window, cx));
    grid = grid.child(proxy_panel(w, p, window, cx));
    page.child(grid)
}

/// "Tools download" panel: rows iterate tooldown's built-in spec table (new
/// tools appear automatically, no hardcoded list). Status, buttons and progress
/// come from the real runtime: manifest facts plus the install center's
/// per-tool progress entries — no fake progress, and every tool downloads
/// independently (one row's run never disables another row). Pinned archives
/// are verified by the kernel after download (a mismatch fails the run and
/// discards the staging dir, so nothing is installed) and failures surface as
/// a toast; the row itself only carries the durable status line.
pub(super) fn tools_download_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    use crate::i18n;
    use duthub_desktop::tool_ui::{InstallFinishedOutcome, ToolPurpose};
    use duthub_desktop::tool_ui::{format_bytes, progress_percent};

    let mut body = div();
    // Copy the live facts out before the row loop (the loop borrows w immutably).
    let progress_view = w.tool_progress_view.clone();
    // Only rows with a spec render; the bottom separator belongs between rows,
    // not after the last one (the panel shell already draws its own line).
    let visible_rows = w
        .tool_rows
        .iter()
        .filter(|row| duthub_tooldown::spec_by_id(row.spec_id).is_some())
        .count();
    let mut shown_rows = 0usize;
    for row in w.tool_rows.clone() {
        let Some(spec) = duthub_tooldown::spec_by_id(row.spec_id) else {
            continue;
        };
        shown_rows += 1;
        let last_row = shown_rows == visible_rows;
        let spec_id = row.spec_id;
        let active_here = w.tool_installs.active(spec_id);
        let busy_here = active_here.is_some();
        let purpose_here = active_here.map(|run| run.purpose);
        let progress = progress_view.get(spec_id).copied();

        // ---- Status line (downloading / last result / installed / absent) ----
        let mut status_color = p.muted;
        let status = if busy_here {
            status_color = p.accent;
            let downloading = purpose_here != Some(ToolPurpose::Update);
            match progress.and_then(|pr| progress_percent(pr.bytes, pr.total)) {
                Some(pct) => {
                    let pct = pct.to_string();
                    let key = if downloading {
                        "toolDownloading"
                    } else {
                        "toolUpdating"
                    };
                    i18n::message(key, &[("pct", pct.as_str())])
                }
                None => {
                    let bytes = format_bytes(progress.map(|pr| pr.bytes).unwrap_or(0));
                    let key = if downloading {
                        "toolDownloadingBytes"
                    } else {
                        "toolUpdatingBytes"
                    };
                    i18n::message(key, &[("bytes", bytes.as_str())])
                }
            }
        } else if let Some(last) = w.tool_last_results.get(spec_id) {
            match &last.outcome {
                Err(failure) => {
                    status_color = p.red;
                    i18n::label(crate::workspace::tool_failure_label(failure.kind)).to_string()
                }
                Ok(InstallFinishedOutcome::Cancelled) => {
                    i18n::label(if last.purpose == ToolPurpose::Update {
                        "toolUpdateCancelled"
                    } else {
                        "toolDownloadCancelled"
                    })
                    .to_string()
                }
                Ok(_) => {
                    status_color = p.accent;
                    i18n::label(if last.purpose == ToolPurpose::Update {
                        "toolUpdateDone"
                    } else {
                        "toolInstallDone"
                    })
                    .to_string()
                }
            }
        } else if row.installed {
            status_color = p.accent;
            i18n::label("toolInstalled").to_string()
        } else {
            i18n::label("toolStatusNotInstalled").to_string()
        };

        // ---- Left column: name -> status ----
        let mut left = div().flex().flex_col().gap(px(3.)).min_w_0().flex_1();
        left = left.child(
            div()
                .text_size(px(12.5))
                .font_weight(gpui_kit::FontWeight(600.0))
                .font_family(crate::theme::MONO_FONT)
                .text_color(p.text)
                .child(SharedString::from(spec_id)),
        );
        left = left.child(
            div()
                .text_size(px(11.))
                .text_color(status_color)
                .child(SharedString::from(status)),
        );
        // Progress bar (hand-drawn, existing style): percentage width only
        // when the server reported a total length; without a total no bar is
        // drawn (no fake progress) — the status line shows received bytes.
        if busy_here && let Some(pct) = progress.and_then(|pr| progress_percent(pr.bytes, pr.total))
        {
            let bar_width = px(125.0);
            let width = (bar_width * (f32::from(pct) / 100.0)).max(px(2.0));
            left = left.child(
                div()
                    .w(bar_width)
                    .h(px(4.0))
                    .mt(px(2.0))
                    .rounded(px(2.0))
                    .bg(p.raised)
                    .overflow_hidden()
                    .child(div().w(width).h_full().rounded(px(2.0)).bg(p.accent)),
            );
        }

        let source_input = In::ToolDownloadUrl(spec_id);
        let source_value = w.input_value(source_input);
        let source = div()
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(px(6.))
            .child(div().min_w_0().flex_1().child(w.text_field(
                source_input,
                &source_value,
                spec.url,
                true,
                p,
                window,
                cx,
            )))
            .child(
                div().flex_none().child(
                    btn(
                        SharedString::from(format!("tool-save-url-{spec_id}")),
                        i18n::label("save"),
                        &BtnVariant::Tiny,
                        p,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.save_tool_download_url(spec);
                        cx.notify();
                    })),
                ),
            );

        // ---- Right column buttons: this row's run shows "Cancel"; every other
        // row keeps its own download/update actions (tools run in parallel) ----
        let mut buttons = div().flex().flex_none().items_center().gap(px(6.0));
        if busy_here {
            buttons = buttons.child(
                btn(
                    SharedString::from(format!("tool-cancel-{spec_id}")),
                    i18n::label("toolBtnCancel"),
                    &BtnVariant::Tiny,
                    p,
                )
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    this.cancel_tool_install(spec_id);
                    cx.notify();
                })),
            );
        } else if row.installed {
            buttons = buttons.child(
                btn(
                    SharedString::from(format!("tool-update-{spec_id}")),
                    i18n::label("toolBtnUpdate"),
                    &BtnVariant::TinyPrimary,
                    p,
                )
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    this.start_tool_install(spec, ToolPurpose::Update);
                    cx.notify();
                })),
            );
            buttons = buttons.child(
                btn(
                    SharedString::from(format!("tool-redownload-{spec_id}")),
                    i18n::label("toolBtnRedownload"),
                    &BtnVariant::Tiny,
                    p,
                )
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    this.start_tool_install(spec, ToolPurpose::Redownload);
                    cx.notify();
                })),
            );
        } else {
            buttons = buttons.child(
                btn(
                    SharedString::from(format!("tool-download-{spec_id}")),
                    i18n::label("toolBtnDownload"),
                    &BtnVariant::TinyPrimary,
                    p,
                )
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    this.start_tool_install(spec, ToolPurpose::Download);
                    cx.notify();
                })),
            );
        }

        body = body.child(
            div()
                .flex()
                .items_center()
                .gap(px(14.0))
                .py(px(12.0))
                .when(!last_row, |el| el.border_b_1().border_color(p.line))
                .child(left.flex_none().w(px(145.)))
                .child(source)
                .child(buttons),
        );
    }
    settings_panel("download", i18n::label("toolsDownload"), None, body, p)
}

/// Settings page "Download proxy" panel: the tri-state policy selector
/// (off / follow env vars / custom URL) plus the URL draft input. The Select
/// gets an outer width cap (pit #42). Both controls stage drafts (pit #41):
/// the Select writes `tool_proxy_mode_draft` and the URL writes the input
/// draft, and the "save" button is the single commit point — the model is
/// only written and persisted after validation passes.
pub(super) fn proxy_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    use crate::i18n;
    use duthub_core::model::DownloadProxyMode;
    let mode = w
        .tool_proxy_mode_draft
        .unwrap_or(w.settings.tools.download.proxy.mode);
    let options = vec![
        (i18n::label("proxyModeOff").to_string(), 0),
        (i18n::label("proxyModeEnv").to_string(), 1),
        (i18n::label("proxyModeCustom").to_string(), 2),
    ];
    let selected = match mode {
        DownloadProxyMode::Off => 0,
        DownloadProxyMode::Env => 1,
        DownloadProxyMode::Custom => 2,
    };
    let select = w.select_field(
        "tool-proxy-mode",
        options[selected].0.clone(),
        options,
        Some(selected),
        std::sync::Arc::new(
            move |this: &mut Workbench,
                  value: usize,
                  _win: &mut Window,
                  cx: &mut Context<Workbench>| {
                this.tool_proxy_mode_draft = Some(match value {
                    0 => DownloadProxyMode::Off,
                    1 => DownloadProxyMode::Env,
                    _ => DownloadProxyMode::Custom,
                });
                cx.notify();
            },
        ),
        p,
        cx,
    );
    let mut body = div().child(setting_row(
        i18n::label("proxyMode"),
        "",
        div().flex_none().w(px(214.0)).child(select),
        p,
    ));
    // Only `custom` reads the URL; showing the field under `env`/`off` reads as
    // if it were applied — the exact trap that kept downloads direct.
    if mode == DownloadProxyMode::Custom {
        let url_input = w.text_field(
            In::ProxyUrl,
            &w.input_value(In::ProxyUrl),
            i18n::label("proxyUrlPlaceholder"),
            true,
            p,
            window,
            cx,
        );
        body = body.child(field(i18n::label("proxyUrlLabel"), url_input, None, p));
    }
    body = body.child(
        div().flex().items_center().child(
            btn(
                "save-tool-proxy",
                i18n::label("saveProxy"),
                &BtnVariant::TinyPrimary,
                p,
            )
            .flex_none()
            .on_click(cx.listener(|this, _ev, _win, cx| {
                this.save_tool_proxy();
                cx.notify();
            })),
        ),
    );
    settings_panel("network", i18n::label("proxyTitle"), None, body, p)
}

/// DUT-specific notification settings, shared with the alert dispatcher.
pub(super) fn alerts_panel(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    use crate::i18n::label;
    let Some(dut) = w.duts.selected() else {
        return div();
    };
    let id = dut.id.clone();
    let settings = w.dut_alert_settings(&id);
    let enabled = settings.enabled;
    let toggle_id = id.clone();
    let header = switch_panel_header(
        "alert",
        label("alertsTitle"),
        toggle("opt-alerts-enabled", enabled, p).on_click(cx.listener(
            move |this, checked, _, cx| {
                this.edit_dut_alerts(&toggle_id, |cfg| cfg.enabled = *checked);
                cx.notify();
            },
        )),
        p,
    );
    if !enabled {
        return collapsible_panel(header, None, p);
    }
    let garbled_on = w.dut_garbled_detection(&id).enabled;
    let garbled_id = id.clone();
    let mut rows = div().flex().flex_col().gap(px(10.)).child(setting_row(
        label("serialGarbledTitle"),
        label("serialGarbledHint"),
        toggle("opt-serial-garbled", garbled_on, p).on_click(cx.listener(
            move |this, checked, _, cx| {
                this.edit_dut_garbled_detection(&garbled_id, *checked);
                cx.notify();
            },
        )),
        p,
    ));
    let source_toggles = [
        (
            "opt-alert-relay",
            "relayAlertTitle",
            "relayAlertHint",
            crate::alerts::AlertSource::Relay,
        ),
        (
            "opt-alert-media",
            "mediaAlertTitle",
            "mediaAlertHint",
            crate::alerts::AlertSource::Media,
        ),
        (
            "opt-alert-usb-debug",
            "usbDebugAlertTitle",
            "usbDebugAlertHint",
            crate::alerts::AlertSource::UsbDebug,
        ),
        (
            "opt-alert-agent-notice",
            "agentNoticeAlertTitle",
            "agentNoticeAlertHint",
            crate::alerts::AlertSource::AgentNotice,
        ),
    ];
    for (toggle_key, title_key, hint_key, source) in source_toggles {
        let on = !settings
            .suppressed_sources
            .iter()
            .any(|key| key == source.key());
        let dut_id = id.clone();
        rows = rows.child(setting_row(
            label(title_key),
            label(hint_key),
            toggle(toggle_key, on, p).on_click(cx.listener(move |this, checked, _, cx| {
                this.edit_dut_alerts(&dut_id, |cfg| {
                    if *checked {
                        cfg.suppressed_sources.retain(|key| key != source.key());
                    } else if !cfg.suppressed_sources.iter().any(|key| key == source.key()) {
                        cfg.suppressed_sources.push(source.key().into());
                    }
                });
                cx.notify();
            })),
            p,
        ));
    }
    let managed_keys = [
        crate::alerts::AlertSource::Relay.key(),
        crate::alerts::AlertSource::Media.key(),
        crate::alerts::AlertSource::UsbDebug.key(),
        crate::alerts::AlertSource::AgentNotice.key(),
    ];
    let has_other_suppressed = settings
        .suppressed_sources
        .iter()
        .any(|key| !managed_keys.contains(&key.as_str()));
    let mut body = rows;
    if has_other_suppressed {
        body = body.child(
            div().pt(px(8.)).child(
                btn(
                    "opt-alerts-reset",
                    label("restoreAlerts"),
                    &BtnVariant::Tiny,
                    p,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.edit_dut_alerts(&id, |cfg| cfg.suppressed_sources.clear());
                    cx.notify();
                })),
            ),
        );
    }
    collapsible_panel(header, Some(body), p)
}
pub(super) fn settings_panel(
    title_icon: &str,
    title: &str,
    right: Option<&str>,
    body: Div,
    p: &'static Palette,
) -> gpui_kit::Div {
    let mut head = panel_head(title_icon, title, p);
    if let Some(right) = right {
        head = head.child(subtle(right, p));
    }
    settings_panel_with_header(head, body, p)
}

pub(super) fn settings_panel_with_header(head: Div, body: Div, p: &Palette) -> Div {
    div()
        .border_b_1()
        .border_color(p.line)
        .child(head)
        .child(div().pt(px(7.)).pb(px(20.)).child(body))
}

pub(super) fn setting_row(
    title: &str,
    desc: &str,
    control: impl IntoElement,
    p: &'static Palette,
) -> Div {
    let mut labels = div().flex_1().min_w_0().child(
        div()
            .text_size(px(12.))
            .font_weight(gpui_kit::FontWeight(600.0))
            .text_color(p.text)
            .child(SharedString::from(title.to_string())),
    );
    // An empty desc means the row is self-explanatory: no stray gap.
    if !desc.is_empty() {
        labels = labels.child(
            div()
                .mt(px(3.))
                .text_size(px(11.))
                .text_color(p.muted)
                .child(SharedString::from(desc.to_string())),
        );
    }
    div()
        .flex()
        .items_center()
        .gap(px(18.))
        .py(px(14.))
        .border_b_1()
        .border_color(p.line)
        .child(labels)
        .child(div().flex_shrink_0().max_w_full().child(control))
}

/// 设置页「工作目录」面板。
pub(super) fn workspace_dir_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let effective = duthub_core::store::plain_path(&w.artifact_root)
        .display()
        .to_string();
    let limit = w.input_value(In::DutStorageGib);
    let save = btn(
        "save-workdir",
        crate::i18n::label("save"),
        &BtnVariant::Primary,
        p,
    )
    .on_click(cx.listener(|this, _, _, cx| {
        this.save_workspace_dir();
        cx.notify();
    }));
    let export_skill = btn(
        "export-skill",
        crate::i18n::label("export"),
        &BtnVariant::Tiny,
        p,
    )
    .on_click(cx.listener(|this, _, _, cx| {
        this.export_skill_md();
        cx.notify();
    }));
    let body = div()
        .child(
            div()
                .pb(px(12.))
                .border_b_1()
                .border_color(p.line)
                .child(
                    w.path_input(In::WorkDir, &effective, p, window, cx)
                        .child(div().flex_none().child(save)),
                )
                .child(subtle(crate::i18n::label("workDirPersistenceHint"), p).mt(px(6.))),
        )
        .child(setting_row(
            crate::i18n::label("dutStorageLimit"),
            "",
            div()
                .flex()
                .gap(px(8.))
                .items_center()
                .child(div().w(px(112.)).child(w.text_field(
                    In::DutStorageGib,
                    &limit,
                    "10",
                    true,
                    p,
                    window,
                    cx,
                )))
                .child(
                    btn(
                        "save-dut-storage-limit",
                        crate::i18n::label("save"),
                        &BtnVariant::Tiny,
                        p,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_dut_storage_limit();
                        cx.notify();
                    })),
                ),
            p,
        ))
        .child(setting_row("MCP Skill", "", export_skill, p).border_b_0());
    settings_panel(
        "folder",
        crate::i18n::label("workDir"),
        Some("settings.json"),
        body,
        p,
    )
}

/// 监听器地址标签：局域网开启时同一端口同时接受回环与局域网地址。
pub(super) fn listener_endpoint_label(port: u16, lan_ip: Option<IpAddr>) -> String {
    match lan_ip {
        Some(ip) => format!("127.0.0.1:{port} / {ip}:{port}"),
        None => format!("127.0.0.1:{port}"),
    }
}

/// 当前若开启局域网访问，返回探测到的本机局域网 IP（用于连接管理展示）。
pub(super) fn detected_lan_ip(w: &Workbench) -> Option<IpAddr> {
    if !w.settings.agent_access.lan_access {
        return None;
    }
    let endpoint = duthub_mcp::transport::advertised_endpoint(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        0,
    ));
    (!endpoint.ip().is_loopback()).then_some(endpoint.ip())
}

/// 设置页「Agent 接入」面板（访问控制 M1 GUI，方案 §6 第 4 项；按需求只中文）：
/// auth 总开关与监听配置 → 凭据列表（Token 原文可见 + 复制 + 启停 + 撤销）→
/// 每凭据 DUT 授权编辑（读/写串口/ADB/继电器/电源/复位/烧录）→
/// DUT 控制权面板（状态/持有者/剩余 TTL/队列 + 释放/强制接管/安全恢复）→
/// 审计事件列表（最近 100 条）。
/// 「Agent 接入」面板（Agent 页）：认证开关、监听与租约、凭据、DUT 控制权、审计。
pub(crate) fn agent_access_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    // 面板内只读快照：渲染借用结束前收集全部数据，listener 闭包只捕获 clone。
    let access = w.settings.agent_access.clone();
    let auth_on = w.access_runtime.enabled();
    let lan_access = access.lan_access;
    let lan_ip = detected_lan_ip(w);
    let port_value = w.input_value(In::AgentAccessPort);
    let ttl_value = w.input_value(In::AgentAccessTtl);
    let cred_name_value = w.input_value(In::AgentAccessCredName);
    let port_field = w.text_field(
        In::AgentAccessPort,
        &port_value,
        "8797",
        true,
        p,
        window,
        cx,
    );
    let ttl_field = w.text_field(In::AgentAccessTtl, &ttl_value, "120", true, p, window, cx);
    let extra_port = w.input_value(In::AgentAccessExtraPort);
    let extra_port_field = w.text_field(
        In::AgentAccessExtraPort,
        &extra_port,
        "8788",
        true,
        p,
        window,
        cx,
    );
    let generic_listeners: Vec<_> = access
        .listeners
        .iter()
        .filter(|listener| listener.dut_id.is_none())
        .collect();
    let endpoints: Vec<_> = std::iter::once((None, access.http_port))
        .chain(
            generic_listeners
                .iter()
                .map(|listener| (Some(listener.id.clone()), listener.port)),
        )
        .collect();
    let selected = endpoints
        .iter()
        .position(|(id, _)| *id == w.agent_credential_listener)
        .unwrap_or(0);
    let listener_field = w.select_field(
        "aa-credential-listener",
        listener_endpoint_label(endpoints[selected].1, lan_ip),
        endpoints
            .iter()
            .enumerate()
            .map(|(index, (_, port))| (listener_endpoint_label(*port, lan_ip), index))
            .collect(),
        Some(selected),
        std::sync::Arc::new(move |this, index, _, cx| {
            if let Some((id, _)) = endpoints.get(index) {
                this.agent_credential_listener = id.clone();
                cx.notify();
            }
        }),
        p,
        cx,
    );
    let name_field = w.text_field(
        In::AgentAccessCredName,
        &cred_name_value,
        crate::i18n::label("agentAccessCredNamePlaceholder"),
        false,
        p,
        window,
        cx,
    );
    let duts_list: Vec<(String, String)> = w
        .duts
        .file
        .duts
        .iter()
        .map(|d| (d.id.clone(), d.name.clone()))
        .collect();
    let mut body = div().flex().flex_col();

    body = body.child(setting_row(
        crate::i18n::label("agentAccessAuthSwitch"),
        "",
        toggle("aa-auth-enabled", auth_on, p).on_click(cx.listener(|this, _ev, _win, cx| {
            this.toggle_agent_auth();
            cx.notify();
        })),
        p,
    ));

    body = body.child(setting_row(
        crate::i18n::label("mcpLanAccess"),
        "",
        toggle("aa-lan-access", lan_access, p).on_click(cx.listener(|this, _ev, _win, cx| {
            this.toggle_agent_lan_access();
            cx.notify();
        })),
        p,
    ));
    if lan_access {
        let address = match lan_ip {
            Some(ip) => format!(
                "{} http://{ip}:{}/mcp",
                crate::i18n::label("mcpLanAddress"),
                access.http_port
            ),
            None => crate::i18n::label("mcpLanNoIp").to_owned(),
        };
        body = body.child(subtle(&address, p).text_color(p.warn));
        body =
            body.child(subtle(crate::i18n::label("mcpLanPlaintextWarning"), p).text_color(p.warn));
    }

    body = body.child(
        aa_section(crate::i18n::label("agentAccessListenLeaseSection"), p)
            .child(field(
                crate::i18n::label("agentAccessHttpPort"),
                port_field,
                None,
                p,
            ))
            .child(field(
                crate::i18n::label("agentAccessLeaseTtl"),
                ttl_field,
                Some(crate::i18n::label("agentAccessLeaseTtlHint")),
                p,
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .py(px(6.))
                    .child(
                        btn(
                            "aa-save",
                            crate::i18n::label("agentAccessSave"),
                            &BtnVariant::TinyPrimary,
                            p,
                        )
                        .flex_none()
                        .on_click(cx.listener(|this, _ev, _win, cx| {
                            this.save_agent_access();
                            cx.notify();
                        })),
                    )
                    .child(subtle(crate::i18n::label("agentAccessSaveHint"), p)),
            )
            .child(field(
                crate::i18n::label("mcpExtraPort"),
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().child(extra_port_field))
                    .child(
                        btn(
                            "aa-add-listener",
                            crate::i18n::label("mcpAddListener"),
                            &BtnVariant::Tiny,
                            p,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.add_mcp_listener();
                            cx.notify();
                        })),
                    ),
                Some(crate::i18n::label("mcpListenerRestart")),
                p,
            ))
            .children(generic_listeners.iter().map(|listener| {
                let id = listener.id.clone();
                setting_row(
                    &listener_endpoint_label(listener.port, lan_ip),
                    "MCP",
                    icon_btn(format!("aa-remove-listener-{id}"), "trash", p).on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.remove_mcp_listener(&id);
                            cx.notify();
                        },
                    )),
                    p,
                )
            })),
    );

    if access
        .listeners
        .iter()
        .any(|listener| listener.dut_id.is_some())
    {
        body = body.child(subtle(crate::i18n::label("agentAccessDutEndpointsNote"), p));
    }

    match w.mcp_actual_endpoint() {
        Some(endpoint) => {
            body = body.child(subtle(
                &crate::i18n::message("agentAccessActualListening", &[("endpoint", &endpoint)]),
                p,
            ));
        }
        None if !w.mcp_runtime.failures.is_empty() => {}
        None => {
            body =
                body.child(subtle(crate::i18n::label("mcpNoListenerBound"), p).text_color(p.warn));
        }
    }
    for (port, reason) in &w.mcp_runtime.failures {
        body = body.child(
            subtle(
                &crate::i18n::message(
                    "mcpBindFailure",
                    &[("port", &port.to_string()), ("reason", reason)],
                ),
                p,
            )
            .text_color(p.red),
        );
    }

    body = body.child(
        aa_section(crate::i18n::label("agentAccessCredentialsSection"), p)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .py(px(6.))
                    .child(div().w(px(240.)).flex_none().child(name_field))
                    .child(div().min_w(px(140.)).child(listener_field))
                    .child(
                        btn(
                            "aa-add-cred",
                            crate::i18n::label("agentAccessGenerateAdd"),
                            &BtnVariant::TinyPrimary,
                            p,
                        )
                        .flex_none()
                        .on_click(cx.listener(|this, _ev, _win, cx| {
                            this.add_agent_credential();
                            cx.notify();
                        })),
                    )
                    .child(subtle(crate::i18n::label("agentAccessCredHint"), p)),
            )
            .children(
                access
                    .agents
                    .iter()
                    .filter(|cred| {
                        let Some(id) = cred.listener_id.as_deref() else {
                            return true;
                        };
                        access
                            .listeners
                            .iter()
                            .find(|listener| listener.id == id)
                            .is_none_or(|listener| listener.dut_id.is_none())
                    })
                    .map(|cred| {
                        let endpoint = match cred.listener_id.as_deref() {
                            None => Some(access.http_port),
                            Some(id) => access
                                .listeners
                                .iter()
                                .find(|listener| listener.id == id)
                                .map(|listener| listener.port),
                        }
                        .map(|port| listener_endpoint_label(port, lan_ip));
                        aa_credential_card(cred, endpoint, &duts_list, p, cx)
                    }),
            ),
    );

    settings_panel(
        "shield",
        crate::i18n::label("agentAccessTitle"),
        Some("settings.json · agentAccess"),
        body,
        p,
    )
}

pub(super) fn aa_section(title: &str, p: &'static Palette) -> Div {
    div().pt(px(14.)).pb(px(4.)).child(
        div()
            .text_size(px(12.))
            .font_weight(gpui_kit::FontWeight(600.0))
            .text_color(p.text)
            .child(SharedString::from(title.to_string())),
    )
}

pub(super) fn grant_check(
    id: impl Into<SharedString>,
    checked: bool,
    p: &'static Palette,
) -> Stateful<Div> {
    div()
        .id(id.into())
        .size(px(16.))
        .rounded(px(4.))
        .border_1()
        .border_color(if checked { p.accent } else { p.border })
        .bg(if checked { p.accent } else { p.bg })
        .flex()
        .items_center()
        .justify_center()
        .text_color(p.accent_contrast)
        .text_size(px(11.))
        .cursor_pointer()
        .hover(|s| s.border_color(p.accent))
        .when(checked, |el| el.child("✓"))
}

const GRANT_ACTIONS: [(DutAction, &str); 2] = [
    (DutAction::Read, "agentAccessGrantRead"),
    (DutAction::Capture, "agentAccessGrantCapture"),
];

pub(super) fn aa_credential_card(
    cred: &duthub_core::AgentCredential,
    endpoint: Option<String>,
    duts_list: &[(String, String)],
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> Div {
    let cred_id = cred.id.clone();
    let token = cred.token.clone();
    let mut card = div()
        .mt(px(8.))
        .rounded(px(10.))
        .border_1()
        .border_color(p.line)
        .bg(p.surface)
        .px(px(12.))
        .py(px(10.))
        .flex()
        .flex_col()
        .gap(px(6.));

    let toggle_id = cred_id.clone();
    let copy_id = cred_id.clone();
    let revoke_id = cred_id.clone();
    let copy_token = token.clone();
    card = card.child(
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(12.5))
                    .font_weight(gpui_kit::FontWeight(600.0))
                    .text_color(p.text)
                    .child(SharedString::from(cred.agent_name.clone())),
            )
            .child(subtle(
                &endpoint
                    .unwrap_or_else(|| crate::i18n::label("mcpListenerUnavailable").to_owned()),
                p,
            ))
            .child(
                chip(
                    format!("aa-cred-toggle-{cred_id}"),
                    if cred.enabled {
                        crate::i18n::label("agentAccessEnabled")
                    } else {
                        crate::i18n::label("agentAccessDisabled")
                    },
                    p,
                )
                .when(cred.enabled, |el| el.text_color(p.accent))
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    this.toggle_agent_credential(&toggle_id);
                    cx.notify();
                })),
            )
            .child(div().flex_1())
            .child(
                chip(
                    format!("aa-cred-copy-{copy_id}"),
                    crate::i18n::label("mcpCopyToken"),
                    p,
                )
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    copy_text(cx, &copy_token);
                    this.notify_toast(crate::i18n::label("tokenCopied"));
                    cx.notify();
                })),
            )
            .child(
                chip(
                    format!("aa-cred-revoke-{revoke_id}"),
                    crate::i18n::label("agentAccessRevoke"),
                    p,
                )
                .text_color(p.red)
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    this.revoke_agent_credential(&revoke_id);
                    cx.notify();
                })),
            ),
    );

    card = card.child(
        div()
            .text_size(px(11.))
            .font_family(crate::theme::MONO_FONT)
            .text_color(p.muted)
            .child(SharedString::from(token)),
    );

    if duts_list.is_empty() {
        card = card.child(subtle(crate::i18n::label("agentAccessNoSavedDuts"), p));
    } else {
        let mut matrix = div().flex().flex_col().mt(px(2.));
        let mut header = div().flex().items_center().gap(px(4.)).py(px(2.));
        header = header.child(
            div()
                .w(px(110.))
                .flex_none()
                .text_size(px(10.5))
                .text_color(p.faint)
                .child(crate::i18n::label("agentAccessDutActionHeader")),
        );
        for (_, label_key) in GRANT_ACTIONS {
            header = header.child(
                div()
                    .w(px(56.))
                    .flex_none()
                    .text_size(px(10.5))
                    .text_color(p.faint)
                    .child(SharedString::from(crate::i18n::label(label_key))),
            );
        }
        matrix = matrix.child(header);
        for (dut_id, dut_name) in duts_list {
            let mut row = div()
                .flex()
                .items_center()
                .gap(px(4.))
                .py(px(3.))
                .border_t_1()
                .border_color(p.line);
            row = row.child(
                div()
                    .w(px(110.))
                    .flex_none()
                    .text_size(px(11.5))
                    .text_color(p.text)
                    .child(SharedString::from(dut_name.clone())),
            );
            let granted = |action: DutAction| {
                cred.grants
                    .iter()
                    .find(|g| g.dut_id == *dut_id)
                    .is_some_and(|g| g.actions.contains(&action))
            };
            for (action, _) in GRANT_ACTIONS {
                let lid = cred_id.clone();
                let ldut = dut_id.clone();
                row = row.child(
                    div().w(px(56.)).flex_none().flex().justify_center().child(
                        grant_check(
                            format!("aa-g-{}-{}-{}", cred_id, dut_id, action.as_str()),
                            granted(action),
                            p,
                        )
                        .on_click(cx.listener(
                            move |this, _ev, _win, cx| {
                                this.toggle_agent_grant(&lid, &ldut, action);
                                cx.notify();
                            },
                        )),
                    ),
                );
            }
            matrix = matrix.child(row);
        }
        card = card.child(matrix);
    }
    card
}

pub(super) fn connection_lease_action(
    w: &Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> Option<gpui_kit::component::button::Button> {
    let dut_id = w.duts.selected()?.id.clone();
    let lease = w.device.lease_manager()?.status(&dut_id);
    let held_by_gui = lease
        .holder
        .as_deref()
        .is_some_and(|holder| holder.starts_with("GUI"));
    let (label, act): (&str, fn(&mut Workbench, &str)) = match lease.phase {
        LeasePhase::NeedsInspection => (
            crate::i18n::label("leaseActionRecover"),
            Workbench::gui_resolve_inspection,
        ),
        LeasePhase::Held if held_by_gui => (
            crate::i18n::label("leaseActionRelease"),
            Workbench::gui_release_lease,
        ),
        LeasePhase::Held => (
            crate::i18n::label("leaseActionTakeover"),
            Workbench::gui_force_takeover,
        ),
        LeasePhase::Free | LeasePhase::Protected => return None,
    };
    Some(
        btn("connection-lease-action", label, &BtnVariant::Tiny, p).on_click(cx.listener(
            move |this, _, _, cx| {
                act(this, &dut_id);
                cx.notify();
            },
        )),
    )
}

pub(super) fn appearance_panel(
    w: &mut Workbench,
    p: &'static Palette,
    _window: &mut Window,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let locales = crate::i18n::language_options(w.lang);
    let selected = locales
        .iter()
        .position(|(code, _)| *code == w.settings.appearance.language)
        .unwrap_or_default();
    let locale_options = locales
        .iter()
        .enumerate()
        .map(|(index, (_, name))| (name.to_string(), index))
        .collect();
    let body = div()
        .child(setting_row(
            crate::i18n::t(w.lang, "language"),
            crate::i18n::t(w.lang, "languageHint"),
            w.select_field(
                "opt-language",
                locales[selected].1.to_string(),
                locale_options,
                Some(selected),
                std::sync::Arc::new(
                    move |this: &mut Workbench,
                          value: usize,
                          _win: &mut Window,
                          cx: &mut Context<Workbench>| {
                        let Some(&(code, _)) = locales.get(value) else {
                            return;
                        };
                        this.settings.appearance.language = code.to_string();
                        this.lang = crate::i18n::Lang::from_setting(code);
                        this.lang.activate();
                        this.persist_settings();
                        cx.notify();
                    },
                ),
                p,
                cx,
            ),
            p,
        ))
        .child(setting_row(
            crate::i18n::t(w.lang, "theme"),
            crate::i18n::t(w.lang, "themeHint"),
            w.select_field(
                "opt-theme",
                match w.theme_mode {
                    crate::theme::ThemeMode::Dark => crate::i18n::label("themeDark").into(),
                    crate::theme::ThemeMode::Light => crate::i18n::label("themeLight").into(),
                },
                vec![
                    (crate::i18n::label("themeDark").to_string(), 0),
                    (crate::i18n::label("themeLight").to_string(), 1),
                ],
                Some(match w.theme_mode {
                    crate::theme::ThemeMode::Dark => 0,
                    crate::theme::ThemeMode::Light => 1,
                }),
                std::sync::Arc::new(
                    move |this: &mut Workbench,
                          value: usize,
                          _win: &mut Window,
                          cx: &mut Context<Workbench>| {
                        let code = if value == 1 { "light" } else { "dark" };
                        this.settings.appearance.theme = code.to_string();
                        this.theme_mode = crate::theme::ThemeMode::from_setting(code);
                        crate::theme::apply_brand(cx, this.theme_mode);
                        this.persist_settings();
                        cx.notify();
                    },
                ),
                p,
                cx,
            ),
            p,
        ))
        .child(setting_row(
            crate::i18n::t(w.lang, "jumpLatest"),
            crate::i18n::t(w.lang, "jumpHint"),
            toggle("opt-jump", w.settings.terminal.jump_to_latest, p).on_click(cx.listener(
                |this, _ev, _win, cx| {
                    this.settings.terminal.jump_to_latest = !this.settings.terminal.jump_to_latest;
                    this.persist_settings();
                    cx.notify();
                },
            )),
            p,
        ))
        .child(setting_row(
            crate::i18n::t(w.lang, "commandBar"),
            crate::i18n::t(w.lang, "commandBarHint"),
            toggle("opt-commandbar", w.settings.terminal.show_command_bar, p).on_click(
                cx.listener(|this, _ev, _win, cx| {
                    this.settings.terminal.show_command_bar =
                        !this.settings.terminal.show_command_bar;
                    this.persist_settings();
                    cx.notify();
                }),
            ),
            p,
        ));
    settings_panel(
        "settings",
        crate::i18n::t(w.lang, "appearance"),
        None,
        body,
        p,
    )
}

pub(super) fn terminal_font_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> Div {
    use crate::i18n::label;
    use crate::workspace::{MAX_TERMINAL_FONT_SIZE, MIN_TERMINAL_FONT_SIZE};

    let size = w.terminal_font_size();
    let size_controls = div()
        .flex()
        .items_center()
        .gap(px(6.))
        .child(
            btn("terminal-font-smaller", "−", &BtnVariant::Normal, p).on_click(cx.listener(
                |this, _, _, cx| {
                    let next = (this.terminal_font_size() - 1.).max(MIN_TERMINAL_FONT_SIZE);
                    this.set_terminal_font_size(Some(next));
                    cx.notify();
                },
            )),
        )
        .child(
            div()
                .min_w(px(46.))
                .text_center()
                .text_color(p.text)
                .child(format!("{} px", (size * 10.).round() / 10.)),
        )
        .child(
            btn("terminal-font-larger", "+", &BtnVariant::Normal, p).on_click(cx.listener(
                |this, _, _, cx| {
                    let next = (this.terminal_font_size() + 1.).min(MAX_TERMINAL_FONT_SIZE);
                    this.set_terminal_font_size(Some(next));
                    cx.notify();
                },
            )),
        )
        .child(
            btn(
                "terminal-font-reset",
                label("terminalFontReset"),
                &BtnVariant::Normal,
                p,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.set_terminal_font_size(None);
                cx.notify();
            })),
        );
    let (family, _, _) = w.terminal_font(window);
    let body = div()
        .child(setting_row(
            label("terminalFontFamily"),
            label("terminalFontFamilyHint"),
            w.terminal_font_picker(window, cx),
            p,
        ))
        .child(setting_row(
            label("terminalFontSize"),
            if cfg!(target_os = "macos") {
                label("terminalFontShortcutsMac")
            } else {
                label("terminalFontShortcuts")
            },
            size_controls,
            p,
        ))
        .child(
            div()
                .mt(px(10.))
                .p(px(10.))
                .rounded(px(6.))
                .bg(p.term)
                .text_color(p.text)
                .font_family(family)
                .text_size(px(size))
                .child("$ ls -l /dev/ttyUSB0"),
        );
    settings_panel("terminal", label("terminalFont"), None, body, p)
}

/// 「Agent 协作 · A2A」面板（Agent 页）：本机 Agent Card 配置与远端 peer 列表。
pub(crate) fn a2a_panel(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let default_a2a = duthub_core::A2aConfig::default();
    let a2a = w
        .settings
        .a2a
        .clone()
        .unwrap_or_else(|| default_a2a.clone());
    let mut body = div();
    // W4a：代理状态/失败原因逐 DUT 可见（来源 = A2aRuntime::status）。
    let a2a_rows: Vec<(String, duthub_desktop::agent_ui::A2aEndpointView)> = w
        .duts
        .file
        .duts
        .iter()
        .filter(|dut| dut.a2a.enabled)
        .map(|dut| {
            let status = w
                .a2a
                .as_ref()
                .map_or(A2aStatus::Disabled, |a2a| a2a.status(&dut.id));
            (
                dut.name.clone(),
                duthub_desktop::agent_ui::a2a_endpoint_view(&dut.a2a, &status),
            )
        })
        .collect();
    let a2a_running = if w.a2a.as_ref().is_some_and(|a2a| a2a.any_running()) {
        a2a_rows
            .iter()
            .filter(|(_, view)| view.card_url.is_some())
            .count()
    } else {
        0
    };
    let a2a_summary = if a2a_rows.is_empty() {
        crate::i18n::label("a2aNoDutEndpoints").to_owned()
    } else {
        crate::i18n::message(
            "a2aRunningSummary",
            &[
                ("running", &a2a_running.to_string()),
                ("total", &a2a_rows.len().to_string()),
            ],
        )
    };
    body = body.child(
        div()
            .text_size(px(13.))
            .text_color(p.text)
            .child(a2a_summary.clone()),
    );
    for (name, view) in &a2a_rows {
        body = body.child(
            subtle(&format!("{name}：{}", view.status_text), p).text_color(if view.failed {
                p.red
            } else {
                p.muted
            }),
        );
    }
    let grid = div()
        .grid()
        .grid_cols(2)
        .gap(px(16.))
        .mt(px(12.))
        .child(field(
            crate::i18n::label("a2aAgentName"),
            w.text_field(
                In::A2aName,
                &a2a.agent_name,
                "Duthub Agent",
                false,
                p,
                window,
                cx,
            ),
            None,
            p,
        ))
        .child(field(
            crate::i18n::label("a2aPort"),
            w.text_field(
                In::A2aPort,
                &w.input_value(In::A2aPort),
                "8790",
                true,
                p,
                window,
                cx,
            ),
            None,
            p,
        ))
        .child(field(
            crate::i18n::label("listenAddress"),
            div()
                .min_h(px(36.))
                .px(px(11.))
                .py(px(8.))
                .rounded(px(8.))
                .bg(p.bg)
                .border_1()
                .border_color(p.border)
                .text_size(px(12.5))
                .font_family(crate::theme::MONO_FONT)
                .text_color(p.muted)
                .child("127.0.0.1"),
            None,
            p,
        ));
    body = body
        .child(
            div()
                .mt(px(12.))
                .text_size(px(13.))
                .text_color(p.text)
                .child(crate::i18n::label("a2aGlobalDeprecated")),
        )
        .child(grid);
    body = body.child(
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .mt(px(4.))
            .child(
                btn(
                    "a2a-save",
                    crate::i18n::label("saveConfig"),
                    &BtnVariant::TinyPrimary,
                    p,
                )
                .on_click(cx.listener(|this, _ev, _win, cx| {
                    this.save_a2a_settings();
                    cx.notify();
                })),
            )
            .child(
                btn(
                    "a2a-export",
                    crate::i18n::label("exportServerConfig"),
                    &BtnVariant::Tiny,
                    p,
                )
                .on_click(cx.listener(|this, _ev, _win, cx| {
                    this.export_a2a_config();
                    cx.notify();
                })),
            )
            .child(
                btn(
                    "a2a-check",
                    crate::i18n::label("checkService"),
                    &BtnVariant::Tiny,
                    p,
                )
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    // W4a：按钮回执真实运行时状态（不再固定「未运行」）。
                    this.notify_toast(a2a_summary.clone());
                    cx.notify();
                })),
            ),
    );
    body = body.child(
        div()
            .border_t_1()
            .border_color(p.line)
            .mt(px(16.))
            .pt(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui_kit::FontWeight(600.0))
                            .text_color(p.text)
                            .child(crate::i18n::label("peerAgents")),
                    )
                    .child(
                        btn(
                            "a2a-add-peer",
                            crate::i18n::label("addServer"),
                            &BtnVariant::Tiny,
                            p,
                        )
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.open_modal(crate::workspace::ModalKind::NewPeer, window, cx);
                            cx.notify();
                        }))
                        .child(
                            icon("plus")
                                .size(px(14.))
                                .text_color(p.muted)
                                .into_any_element(),
                        ),
                    ),
            ),
    );
    for (index, peer) in a2a.peers.iter().enumerate() {
        body = body.child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .py(px(8.))
                .border_b_1()
                .border_color(p.line)
                .child(
                    div()
                        .font_weight(gpui_kit::FontWeight(600.0))
                        .text_size(px(12.))
                        .text_color(p.text)
                        .child(SharedString::from(peer.name.clone())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(11.))
                        .font_family(crate::theme::MONO_FONT)
                        .text_color(p.muted)
                        .overflow_hidden()
                        .child(SharedString::from(peer.card_url.clone())),
                )
                .child(
                    icon_btn(SharedString::from(format!("peer-del-{index}")), "trash", p).on_click(
                        cx.listener(move |this, _ev, _win, cx| {
                            if let Some(a2a) = this.settings.a2a.as_mut() {
                                a2a.peers.remove(index);
                            }
                            this.persist_settings();
                            cx.notify();
                        }),
                    ),
                ),
        );
    }
    settings_panel(
        "network",
        crate::i18n::label("agentCollaboration"),
        Some("A2A 1.0 · Agent Card"),
        body,
        p,
    )
}

pub(super) fn commands_panel(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let mut body = div();
    for (index, command) in w.settings.commands.iter().enumerate() {
        body = body.child(
            div()
                .flex()
                .items_center()
                .gap(px(9.))
                .py(px(9.))
                .px(px(11.))
                .mb(px(8.))
                .rounded(px(8.))
                .border_1()
                .border_color(p.border)
                .bg(p.bg)
                .child(
                    div()
                        .min_w(px(105.))
                        .font_weight(gpui_kit::FontWeight(600.0))
                        .text_size(px(12.))
                        .text_color(p.text)
                        .child(SharedString::from(command.name.clone())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(11.))
                        .font_family(crate::theme::MONO_FONT)
                        .text_color(p.accent)
                        .overflow_hidden()
                        .child(SharedString::from(command.value.clone())),
                )
                .child(
                    icon_btn(SharedString::from(format!("cmd-del-{index}")), "trash", p).on_click(
                        cx.listener(move |this, _ev, _win, cx| {
                            this.settings.commands.remove(index);
                            this.persist_settings();
                            cx.notify();
                        }),
                    ),
                ),
        );
    }
    let add = btn(
        "add-command",
        crate::i18n::label("addCommand"),
        &BtnVariant::Tiny,
        p,
    )
    .icon(crate::icons::kit_icon("plus").size(px(14.)))
    .on_click(cx.listener(|this, _ev, window, cx| {
        this.open_modal(crate::workspace::ModalKind::NewCommand, window, cx);
        cx.notify();
    }));
    let header = panel_head("bolt", crate::i18n::t(w.lang, "customCommands"), p)
        .child(div().flex_none().child(add));
    settings_panel_with_header(header, body, p)
}

pub(super) fn config_files_panel(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Div {
    let row = |title: &'static str,
               detail: &'static str,
               base: &'static str,
               p: &'static Palette,
               cx: &mut Context<Workbench>| {
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(22.))
            .py(px(13.))
            .border_b_1()
            .border_color(p.line)
            .child(
                div()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_family(crate::theme::MONO_FONT)
                            .font_weight(gpui_kit::FontWeight(600.0))
                            .text_color(p.text)
                            .child(title),
                    )
                    .child(div().mt(px(5.)).child(subtle(detail, p))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        btn(
                            SharedString::from(format!("{base}-import")),
                            crate::i18n::label("import"),
                            &BtnVariant::Tiny,
                            p,
                        )
                        .on_click(cx.listener(
                            move |this, _ev, _win, cx| {
                                this.import_config(base, cx);
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        btn(
                            SharedString::from(format!("{base}-export")),
                            crate::i18n::label("export"),
                            &BtnVariant::Tiny,
                            p,
                        )
                        .on_click(cx.listener(
                            move |this, _ev, _win, cx| {
                                this.export_config(base);
                                cx.notify();
                            },
                        )),
                    ),
            )
    };
    let body = div()
        .child(subtle(&w.config_dir().display().to_string(), p))
        .child(row(
            "settings.json",
            crate::i18n::label("personalSettings"),
            "settings",
            p,
            cx,
        ))
        .child(
            row(
                "duts.json",
                crate::i18n::label("deviceSettings"),
                "duts",
                p,
                cx,
            )
            .border_b_0(),
        );
    settings_panel("code", crate::i18n::label("configFiles"), None, body, p)
}

#[cfg(test)]
mod usb_discovery_tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn absent_saved_adb_binding_is_not_a_discovery_candidate() {
        let lang = crate::i18n::Lang::from_setting("zh-CN");
        let empty = Ok(Vec::new());
        let (options, status) = usb_debug_display(Some(&empty), "RK3576USB003", false, lang);
        assert!(options.is_empty());
        assert_eq!(status, "当前未检测到 ADB 设备");

        let connected = Ok(vec![duthub_core::AdbDevice {
            serial: "different-device".into(),
            state: "device".into(),
            model: None,
        }]);
        let (options, status) = usb_debug_display(Some(&connected), "RK3576USB003", false, lang);
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].0, "different-device");
        assert_eq!(status, "当前未检测到绑定的设备");
    }
}
