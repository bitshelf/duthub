use super::{Workbench, In, FluentBuilder, AppContext, goose_path_field_value, boot_reference_path, InteractiveElement, Window, Context, InputState, InputEvent, Dd, BoundEditor, Palette, Input, Styled, StatefulInteractiveElement, px, theme, Div, i18n, IntoElement, Arc, ParentElement, div, SharedString, Textarea, TextareaState, icon, OnChangeFn, UsbDiscovery, FlashPresetTest, store, usb_debug_enabled, ViewMode, ConfigPage, Pixels, workspace_pane_widths, DragMoveEvent, PaneDivider, CENTER_PANE_MIN_WIDTH, demo_dut_template, HashMap, DutsState, JobKind, cx_spawn_job, AdbDevice, JobResult, RelayPress, AlertSource, Duration};

impl Workbench {
    // ------------------------------------------------------------------
    // 输入体系
    // ------------------------------------------------------------------

    pub fn input_value(&self, id: In) -> String {
        let dut = self.duts.selected();
        match id {
            In::RelayBoardName | In::RelayBoardNode => self
                .relay_board_edit()
                .map_or_default(|board| {
                    if id == In::RelayBoardName {
                        board.name
                    } else {
                        board.node
                    }
                }),
            In::Find => self.find_query.clone(),
            In::CfgSearch => self.cfg_search.clone(),
            In::Target | In::AgentModel => String::new(),
            In::GoosePath => goose_path_field_value(
                self.drafts.get(&id).map(String::as_str),
                self.goose_external().map(|(path, _)| path).as_deref(),
            ),
            In::AgentRegName
            | In::AgentRegProgram
            | In::AgentRegArgs
            | In::AgentRegEnvKeys
            | In::AgentRegEnvValue => self.drafts.get(&id).cloned().unwrap_or_default(),
            In::Prompt => self.input.clone(),
            In::Port => dut.map_or_default(|d| d.serial.port.clone()),
            In::DutName => dut.map_or_default(|d| d.name.clone()),
            In::BootReference => dut
                .and_then(|dut| {
                    dut.serial.reference_log.clone().or_else(|| {
                        boot_reference_path(&self.artifact_root, dut)
                            .map(|path| path.to_string_lossy().into_owned())
                    })
                })
                .unwrap_or_default(),
            In::BootSimilarity => dut.map_or_else(String::new, |dut| {
                self.boot_similarity_drafts
                    .get(&dut.id)
                    .cloned()
                    .unwrap_or_else(|| {
                        dut.serial
                            .reference_similarity
                            .unwrap_or(duthub_core::model::DEFAULT_BOOT_SIMILARITY)
                            .to_string()
                    })
            }),
            In::Baud => dut.map_or_default(|d| d.serial.baud.to_string()),
            In::SessionBaud => self
                .sessions
                .get(self.active_session)
                .map_or_default(|s| {
                    s.baud_draft.clone().unwrap_or_else(|| {
                        if s.baud == 0 {
                            String::new()
                        } else {
                            s.baud.to_string()
                        }
                    })
                }),
            In::AdbSerial => dut.map_or_default(|d| d.adb.serial.clone()),
            In::RelayNode => dut
                .map_or_default(|d| match &d.relay {
                    duthub_core::model::RelayConfig::UsbSerial { node, .. } => node.clone(),
                    _ => String::new(),
                }),
            In::PduAddress => dut
                .map_or_default(|d| match &d.relay {
                    duthub_core::model::RelayConfig::SnmpPdu { snmp, .. } => snmp.address.clone(),
                    _ => String::new(),
                }),
            In::PduCredential => dut
                .map_or_default(|d| match &d.relay {
                    duthub_core::model::RelayConfig::SnmpPdu { snmp, .. } => {
                        snmp.credential_ref.clone()
                    }
                    _ => String::new(),
                }),
            In::McpUrl => dut
                .map_or_default(|d| match &d.mcp {
                    duthub_core::model::McpConfig::StreamableHttp { url } => url.clone(),
                    _ => String::new(),
                }),
            In::McpCommand => dut
                .map_or_default(|d| match &d.mcp {
                    duthub_core::model::McpConfig::Stdio { command, .. } => command.clone(),
                    _ => String::new(),
                }),
            In::McpArgs => dut
                .map_or_default(|d| match &d.mcp {
                    duthub_core::model::McpConfig::Stdio { args, .. } => args.join("\n"),
                    _ => String::new(),
                }),
            In::McpCwd => dut
                .map_or_default(|d| match &d.mcp {
                    duthub_core::model::McpConfig::Stdio { cwd, .. } => cwd.clone(),
                    _ => String::new(),
                }),
            In::McpEnv => dut
                .map_or_default(|d| match &d.mcp {
                    duthub_core::model::McpConfig::Stdio { env, .. } => env
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                }),
            In::SerialRuleRegex | In::SerialRuleDescription => self
                .settings
                .serial
                .highlight_rules
                .get(self.serial_rule_selected)
                .map_or_default(|rule| {
                    if id == In::SerialRuleRegex {
                        rule.regex.clone()
                    } else {
                        crate::pages::prompt_rule_description(self.lang, rule).to_owned()
                    }
                }),
            In::SerialScrollback => self.drafts.get(&id).cloned().unwrap_or_else(|| {
                self.settings
                    .serial
                    .scrollback_rows
                    .map_or(duthub_device::term::SCROLLBACK_HISTORY, |rows| {
                        rows as usize
                    })
                    .to_string()
            }),
            // Draft first, saved value second (pit #41): clearing the field
            // must stay visible instead of snapping back to the stored seconds.
            In::CaptureIdle => self.drafts.get(&id).cloned().unwrap_or_else(|| {
                self.settings
                    .tools
                    .capture
                    .idle_secs
                    .unwrap_or(duthub_desktop::capture_backend::DEFAULT_EPHEMERAL_IDLE_SECS)
                    .to_string()
            }),
            In::AdbExe => self.settings.tools.adb_executable.clone(),
            In::HdcExe => self.settings.tools.hdc_executable.clone(),
            In::WorkDir => self
                .drafts
                .get(&id)
                .cloned()
                .unwrap_or_else(|| self.settings.workspace_dir.clone().unwrap_or_default()),
            In::DutStorageGib => self
                .drafts
                .get(&id)
                .cloned()
                .unwrap_or_else(|| self.settings.dut_storage_gib().to_string()),
            In::HeartbeatInterval => dut
                .map_or_default(|d| self.heartbeat_config(&d.id).interval_secs.to_string()),
            In::HeartbeatCommand => dut
                .map_or_default(|d| self.heartbeat_config(&d.id).command),
            In::HeartbeatExpectedResponse => dut
                .and_then(|d| self.heartbeat_config(&d.id).expected_response)
                .unwrap_or_default(),
            In::AgentAccessPort => self
                .drafts
                .get(&In::AgentAccessPort)
                .cloned()
                .unwrap_or_else(|| self.settings.agent_access.http_port.to_string()),
            In::AgentAccessTtl => self
                .drafts
                .get(&In::AgentAccessTtl)
                .cloned()
                .unwrap_or_else(|| self.settings.agent_access.lease_ttl_secs.to_string()),
            In::AgentAccessExtraPort => self.drafts.get(&id).cloned().unwrap_or_default(),
            In::AgentAccessCredName => self
                .drafts
                .get(&In::AgentAccessCredName)
                .cloned()
                .unwrap_or_default(),
            In::DutMcpPort => {
                if let Some(value) = self.drafts.get(&id) {
                    value.clone()
                } else {
                    dut.and_then(|dut| self.settings.agent_access.listener_for_dut(&dut.id))
                        .map_or_default(|listener| listener.port.to_string())
                }
            }
            In::DutA2aPort => {
                if let Some(value) = self.drafts.get(&id) {
                    value.clone()
                } else {
                    dut.and_then(|dut| {
                        (dut.a2a.enabled
                            || dut.a2a.port != duthub_core::model::DutA2aConfig::default().port)
                            .then(|| dut.a2a.port.to_string())
                    })
                    .unwrap_or_default()
                }
            }
            In::DutA2aName => {
                if let Some(value) = self.drafts.get(&id) {
                    value.clone()
                } else {
                    dut.map_or_default(|dut| dut.a2a.agent_name.clone())
                }
            }
            // Draft arm (pit #41): display reads the draft and falls back to
            // the saved value; the model is only written after "save proxy"
            // validation passes (an invalid URL must never reach
            // settings.json).
            In::ToolDownloadUrl(tool) => self.drafts.get(&id).cloned().unwrap_or_else(|| {
                duthub_tooldown::spec_by_id(tool)
                    .map_or_else(String::new, |spec| self.tool_download_url(spec).to_owned())
            }),
            In::ProxyUrl => self
                .drafts
                .get(&id)
                .cloned()
                .unwrap_or_else(|| self.settings.tools.download.proxy.url.clone()),
            In::Media(field) => field.value(&self.settings.tools.capture, dut).to_owned(),
            In::A2aName => self
                .settings
                .a2a
                .as_ref()
                .map_or_default(|a| a.agent_name.clone()),
            In::A2aPort => self.drafts.get(&In::A2aPort).cloned().unwrap_or_else(|| {
                self.settings
                    .a2a
                    .as_ref()
                    .map_or_default(|a| a.port.to_string())
            }),
            In::CmdName
            | In::CmdValue
            | In::NewPresetName
            | In::NewPresetExe
            | In::PeerName
            | In::PeerUrl
            | In::RenameName => self
                .modal
                .as_ref()
                .map_or_default(|m| match id {
                    In::CmdName | In::NewPresetName | In::PeerName | In::RenameName => {
                        m.draft.clone()
                    }
                    _ => m.draft2.clone(),
                }),
            // Third modal field (icon / test args) keeps its own draft so an
            // empty value stays visible instead of snapping back (pit #41).
            In::CmdIcon | In::NewPresetTestArgs => self
                .modal
                .as_ref()
                .map_or_default(|m| m.draft3.clone()),
        }
    }

    pub(super) fn input_set(&mut self, id: In, value: String) {
        let dut = self.duts.selected_mut();
        let settings = &mut self.settings;
        match id {
            In::Find => self.find_query = value,
            In::CfgSearch => self.cfg_search = value,
            In::Target | In::AgentModel => {}
            In::GoosePath => {
                // 文本草稿保留原文（含空串）：空串是「用户已清空」的显式状态，
                // 不能被 goose_external 的回显覆盖（详见 goose_path_field_value）。
                self.drafts.insert(In::GoosePath, value);
            }
            In::AgentRegName
            | In::AgentRegProgram
            | In::AgentRegArgs
            | In::AgentRegEnvKeys
            | In::AgentRegEnvValue => {
                self.drafts.insert(id, value);
            }
            In::DutName => {
                if let Some(dut) = dut {
                    dut.name = value;
                    self.duts.mark_connection_dirty();
                }
            }
            In::BootReference => {
                if let Some(dut) = dut {
                    dut.serial.reference_log = Some(value);
                    self.duts.mark_connection_dirty();
                }
            }
            In::BootSimilarity => {
                if let Some(dut) = dut {
                    if value.trim().is_empty() {
                        dut.serial.reference_similarity = None;
                    } else if let Ok(percent @ 1..=100) = value.trim().parse::<u8>() {
                        dut.serial.reference_similarity = Some(percent);
                    }
                    self.boot_similarity_drafts.insert(dut.id.clone(), value);
                    self.duts.mark_connection_dirty();
                }
            }
            In::Prompt => self.input = value,
            In::Port => {
                if let Some(d) = dut {
                    d.serial.port = value;
                    self.duts.mark_connection_dirty();
                }
            }
            In::Baud => {
                if let Ok(b) = value.parse::<u32>()
                    && let Some(d) = dut
                {
                    d.serial.baud = b;
                    self.duts.mark_connection_dirty();
                }
            }
            In::SessionBaud => {
                if let Some(view) = self.sessions.get_mut(self.active_session) {
                    view.baud_draft = if value.is_empty() { None } else { Some(value) };
                }
            }
            In::AdbSerial => {
                if let Some(d) = dut {
                    d.adb.serial = value;
                    d.adb.mode = "usb".into();
                    self.duts.mark_connection_dirty();
                }
            }
            In::RelayBoardName | In::RelayBoardNode => {
                // Draft-only (same semantic as the connection form): the board
                // list is written by `save_relay_board`, never per keystroke.
                if let Some(saved) = self.selected_relay_board().cloned() {
                    let draft = self.relay_board_draft.get_or_insert(saved);
                    if id == In::RelayBoardName {
                        draft.name = value;
                    } else {
                        draft.node = value;
                    }
                }
            }
            In::RelayNode => {
                if let Some(d) = dut
                    && let duthub_core::model::RelayConfig::UsbSerial { node, .. } = &mut d.relay
                {
                    *node = value;
                    self.duts.mark_connection_dirty();
                }
            }
            In::PduAddress => {
                if let Some(d) = dut
                    && let duthub_core::model::RelayConfig::SnmpPdu { snmp, .. } = &mut d.relay
                {
                    snmp.address = value;
                    self.duts.mark_connection_dirty();
                }
            }
            In::PduCredential => {
                if let Some(d) = dut
                    && let duthub_core::model::RelayConfig::SnmpPdu { snmp, .. } = &mut d.relay
                {
                    snmp.credential_ref = value;
                    self.duts.mark_connection_dirty();
                }
            }
            In::McpUrl => {
                if let Some(d) = dut
                    && let duthub_core::model::McpConfig::StreamableHttp { url } = &mut d.mcp
                {
                    *url = value;
                    self.duts.mark_connection_dirty();
                }
            }
            In::McpCommand => {
                if let Some(d) = dut
                    && let duthub_core::model::McpConfig::Stdio { command, .. } = &mut d.mcp
                {
                    *command = value;
                    self.duts.mark_connection_dirty();
                }
            }
            In::McpArgs => {
                if let Some(d) = dut
                    && let duthub_core::model::McpConfig::Stdio { args, .. } = &mut d.mcp
                {
                    *args = value
                        .lines()
                        .map(str::to_string)
                        .filter(|l| !l.is_empty())
                        .collect();
                    self.duts.mark_connection_dirty();
                }
            }
            In::McpCwd => {
                if let Some(d) = dut
                    && let duthub_core::model::McpConfig::Stdio { cwd, .. } = &mut d.mcp
                {
                    *cwd = value;
                    self.duts.mark_connection_dirty();
                }
            }
            In::McpEnv => {
                if let Some(d) = dut
                    && let duthub_core::model::McpConfig::Stdio { env, .. } = &mut d.mcp
                {
                    env.clear();
                    for line in value.lines() {
                        let line = line.trim();
                        if let Some((k, v)) = line.split_once('=') {
                            env.insert(k.trim().to_string(), v.trim().to_string());
                        }
                    }
                    self.duts.mark_connection_dirty();
                }
            }
            In::HeartbeatInterval => {
                if let Ok(secs) = value.parse::<u32>()
                    && let Some(d) = dut
                {
                    let cfg = self
                        .heartbeat_drafts
                        .entry(d.id.clone())
                        .or_insert_with(|| self.heartbeat.config(&d.id));
                    cfg.interval_secs = secs;
                    cfg.interval_secs = cfg.normalized_interval();
                    self.duts.mark_connection_dirty();
                }
            }
            In::HeartbeatCommand | In::HeartbeatExpectedResponse => {
                if let Some(d) = dut {
                    let cfg = self
                        .heartbeat_drafts
                        .entry(d.id.clone())
                        .or_insert_with(|| self.heartbeat.config(&d.id));
                    if id == In::HeartbeatCommand {
                        cfg.command = value;
                    } else {
                        cfg.expected_response = (!value.is_empty()).then_some(value);
                    }
                    self.duts.mark_connection_dirty();
                }
            }
            In::SerialRuleRegex | In::SerialRuleDescription => {
                if let Some(rule) = settings
                    .serial
                    .highlight_rules
                    .get_mut(self.serial_rule_selected)
                {
                    if id == In::SerialRuleRegex {
                        rule.regex = value;
                    } else {
                        rule.description = Some(value);
                    }
                }
            }
            In::SerialScrollback => {
                self.drafts.insert(id, value);
            }
            In::CaptureIdle => {
                // Draft only: "save idle" parses and validates before the model
                // is touched, and an empty draft means "use the default".
                self.drafts.insert(id, value);
            }
            In::AdbExe => settings.tools.adb_executable = value,
            In::HdcExe => settings.tools.hdc_executable = value,
            In::WorkDir => {
                self.drafts.insert(id, value);
            }
            In::DutStorageGib => {
                self.drafts.insert(id, value);
            }
            In::Media(field) => {
                if let Some(dest) = field.edit(&mut settings.tools.capture, dut) {
                    *dest = value;
                    if field.is_dut() {
                        self.duts.mark_connection_dirty();
                    }
                }
            }
            In::A2aName => {
                let a2a = settings
                    .a2a
                    .get_or_insert_with(duthub_core::A2aConfig::default);
                a2a.agent_name = value;
            }
            In::A2aPort => {
                // 与 In::Baud 同模式：草稿保原文（可清空），解析成功才写模型
                if value.is_empty() {
                    self.drafts.remove(&In::A2aPort);
                } else {
                    self.drafts.insert(In::A2aPort, value.clone());
                }
                if let Ok(port) = value.parse::<u16>() {
                    let a2a = settings
                        .a2a
                        .get_or_insert_with(duthub_core::A2aConfig::default);
                    a2a.port = port;
                }
            }
            In::AgentAccessPort
            | In::AgentAccessTtl
            | In::AgentAccessCredName
            | In::AgentAccessExtraPort
            | In::DutMcpPort
            | In::DutA2aPort
            | In::DutA2aName => {
                self.drafts.insert(id, value);
            }
            In::ProxyUrl | In::ToolDownloadUrl(_) => {
                // Keep the text draft verbatim (empty string = user cleared
                // the field): the model is only written — and the draft only
                // removed — once the save button's validation passes (same
                // shape as In::GoosePath).
                self.drafts.insert(id, value);
            }
            In::CmdName | In::NewPresetName | In::PeerName | In::RenameName => {
                if let Some(m) = self.modal.as_mut() {
                    m.draft = value;
                }
            }
            In::CmdValue | In::NewPresetExe | In::PeerUrl => {
                if let Some(m) = self.modal.as_mut() {
                    m.draft2 = value;
                }
            }
            In::CmdIcon | In::NewPresetTestArgs => {
                if let Some(m) = self.modal.as_mut() {
                    m.draft3 = value;
                }
            }
        }
    }

    pub(crate) fn select_serial_rule(&mut self, index: usize) {
        self.serial_rule_selected = index;
        self.text_editors.remove(&In::SerialRuleRegex);
        self.text_editors.remove(&In::SerialRuleDescription);
    }

    pub(super) fn editor_scope(&self, id: In) -> Option<&str> {
        match id {
            In::RelayBoardNode | In::RelayBoardName => self
                .duts
                .file
                .relay_boards
                .get(self.relay_board_selected)
                .map(|board| board.id.as_str()),
            In::Target | In::SessionBaud => self
                .sessions
                .get(self.active_session)
                .map(|view| view.dut_id.as_str()),
            _ => match id {
                In::Port
                | In::DutName
                | In::BootReference
                | In::BootSimilarity
                | In::Baud
                | In::AdbSerial
                | In::RelayNode
                | In::PduAddress
                | In::PduCredential
                | In::McpUrl
                | In::McpCommand
                | In::McpArgs
                | In::McpCwd
                | In::McpEnv
                | In::DutMcpPort
                | In::DutA2aPort
                | In::DutA2aName
                | In::HeartbeatInterval
                | In::HeartbeatCommand
                | In::HeartbeatExpectedResponse => self.duts.selected().map(|dut| dut.id.as_str()),
                In::Media(field) if field.is_dut() => {
                    self.duts.selected().map(|dut| dut.id.as_str())
                }
                _ => None,
            },
        }
    }

    pub(super) fn line_editor(
        &mut self,
        id: In,
        value: &str,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Entity<InputState> {
        self.line_editor_with(id, value, placeholder, false, window, cx)
    }

    /// Masked single-line editor (W4b env values): rendering hides the text,
    /// the raw value stays readable for the write handler.
    pub(super) fn line_editor_masked(
        &mut self,
        id: In,
        value: &str,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Entity<InputState> {
        self.line_editor_with(id, value, placeholder, true, window, cx)
    }

    pub(super) fn line_editor_with(
        &mut self,
        id: In,
        value: &str,
        placeholder: &str,
        masked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Entity<InputState> {
        let scope = self.editor_scope(id).map(str::to_owned);
        if let Some(editor) = self.text_editors.get_mut(&id)
            && editor.scope == scope
        {
            if editor.model_value != value || editor.placeholder != placeholder {
                editor.state.update(cx, |state, cx| {
                    if editor.placeholder != placeholder {
                        state.set_placeholder(placeholder.to_owned(), window, cx);
                    }
                    if editor.model_value != value && state.value().as_ref() != value {
                        state.set_value(value.to_owned(), window, cx);
                    }
                });
                value.clone_into(&mut editor.model_value);
                placeholder.clone_into(&mut editor.placeholder);
            }
            return editor.state.clone();
        }

        let state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholder.to_owned())
                .default_value(value.to_owned())
                .masked(masked)
        });
        let owner = scope.clone();
        let subscription = cx.subscribe(&state, move |this, state, event, cx| {
            if this.editor_scope(id) != owner.as_deref()
                || this
                    .text_editors
                    .get(&id)
                    .is_none_or(|editor| editor.state != state)
            {
                return;
            }
            match event {
                InputEvent::Change => {
                    let value = state.read(cx).value().to_string();
                    this.input_set(id, value);
                    let model = this.input_value(id);
                    if let Some(editor) = this.text_editors.get_mut(&id) {
                        // Keep the editor's raw text for incomplete numbers and composition.
                        editor.model_value = model;
                    }
                }
                InputEvent::Focus => {
                    let dropdown = match id {
                        In::Port => Some(Dd::Port),
                        In::RelayBoardNode => Some(Dd::RelayBoardPort),
                        In::Baud => Some(Dd::Baud),
                        In::AdbSerial => Some(Dd::AdbSerial),
                        In::Media(
                            field @ (crate::media_page::MediaField::Size(_)
                            | crate::media_page::MediaField::Rate(_)),
                        ) => Some(Dd::CaptureMode(field)),
                        _ => None,
                    };
                    if let Some(dd) = dropdown {
                        this.open_combo(dd);
                    } else {
                        this.open_dd = None;
                    }
                }
                InputEvent::PressEnter { .. } if id == In::SessionBaud => this.apply_session_baud(),
                _ => return,
            }
            cx.notify();
        });
        self.text_editors.insert(
            id,
            BoundEditor {
                state: state.clone(),
                model_value: value.to_owned(),
                placeholder: placeholder.to_owned(),
                scope,
                _subscription: subscription,
            },
        );
        state
    }

    #[allow(clippy::too_many_arguments)]
    pub fn text_field(
        &mut self,
        id: In,
        value: &str,
        placeholder: &str,
        mono: bool,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Input {
        let state = self.line_editor(id, value, placeholder, window, cx);
        let input = Input::new(&state)
            .id(id.key())
            .accessibility_id(id.key())
            .w_full()
            .bg(p.bg)
            .text_size(px(13.));
        if mono {
            input.font_family(theme::MONO_FONT)
        } else {
            input
        }
    }

    /// Password-masked input with a reveal toggle (gpui-kit component Input
    /// `.mask_toggle()`); used for env values so secrets never render in
    /// clear text (W4b).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn masked_field(
        &mut self,
        id: In,
        value: &str,
        placeholder: &str,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Input {
        let state = self.line_editor_masked(id, value, placeholder, window, cx);
        Input::new(&state)
            .id(id.key())
            .accessibility_id(id.key())
            .w_full()
            .bg(p.bg)
            .text_size(px(13.))
            .mask_toggle()
    }

    pub(crate) fn path_input(
        &mut self,
        id: In,
        placeholder: &str,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let input = self.text_field(id, &self.input_value(id), placeholder, true, p, window, cx);
        let label = i18n::label("browsePath");
        let button_id = format!("browse-{}", id.key());
        let button = if id == In::Target {
            self.select_field(
                "browse-target",
                label.into(),
                vec![
                    (i18n::label("selectPathFile").into(), 0),
                    (i18n::label("selectPathDirectory").into(), 1),
                ],
                None,
                Arc::new(|this, index, _, cx| this.browse_path(In::Target, index == 1, cx)),
                p,
                cx,
            )
            .min_w(px(110.))
            .into_any_element()
        } else {
            let choose = cx.listener(move |this, _, _, cx| {
                this.browse_path(id, matches!(id, In::WorkDir | In::McpCwd), cx);
            });
            if id == In::NewPresetExe {
                // 跟随弹窗内其他按钮的绘制方式。
                div()
                    .id(SharedString::from(button_id))
                    .min_h(px(32.))
                    .px(px(12.))
                    .rounded(px(8.))
                    .bg(p.raised)
                    .text_color(p.text)
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .hover(|s| s.bg(p.hover))
                    .on_click(choose)
                    .child(label)
                    .into_any_element()
            } else {
                crate::components::btn(button_id, label, &crate::components::BtnVariant::Normal, p)
                    .on_click(choose)
                    .into_any_element()
            }
        };
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(div().flex_1().min_w_0().child(input))
            .child(div().flex_none().child(button))
    }

    pub fn multiline_field(
        &mut self,
        id: In,
        value: &str,
        placeholder: &str,
        p: &'static Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Textarea {
        let scope = self.editor_scope(id).map(str::to_owned);
        if self
            .textareas
            .get(&id)
            .is_none_or(|editor| editor.scope != scope)
        {
            let state = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(3)
                    .placeholder(placeholder.to_owned())
                    .default_value(value.to_owned())
            });
            let owner = scope.clone();
            let subscription = cx.subscribe(&state, move |this, state, event, cx| {
                if this.editor_scope(id) != owner.as_deref()
                    || this
                        .textareas
                        .get(&id)
                        .is_none_or(|editor| editor.state != state)
                {
                    return;
                }
                match event {
                    InputEvent::Change => {
                        this.input_set(id, state.read(cx).value().to_string());
                        let model = this.input_value(id);
                        if let Some(editor) = this.textareas.get_mut(&id) {
                            editor.model_value = model;
                        }
                    }
                    InputEvent::Focus => this.open_dd = None,
                    _ => return,
                }
                cx.notify();
            });
            self.textareas.insert(
                id,
                BoundEditor {
                    state,
                    model_value: value.to_owned(),
                    placeholder: placeholder.to_owned(),
                    scope,
                    _subscription: subscription,
                },
            );
        }
        let editor = self
            .textareas
            .get_mut(&id)
            .expect("textarea editor initialized");
        if editor.model_value != value || editor.placeholder != placeholder {
            editor.state.update(cx, |state, cx| {
                if editor.placeholder != placeholder {
                    state.set_placeholder(placeholder.to_owned(), window, cx);
                }
                if editor.model_value != value && state.value().as_ref() != value {
                    state.set_value(value.to_owned(), window, cx);
                }
            });
            value.clone_into(&mut editor.model_value);
            placeholder.clone_into(&mut editor.placeholder);
        }
        Textarea::new(&editor.state)
            .accessibility_id(id.key())
            .w_full()
            .bg(p.bg)
            .font_family(theme::MONO_FONT)
            .text_size(px(13.))
    }

    pub(super) fn open_combo(&mut self, dd: Dd) {
        if self.overlay.is_some() && dd != Dd::RelayBoardPort {
            return;
        }
        match dd {
            Dd::AdbSerial => {
                if let Some(dut) = self.duts.selected() {
                    self.scan_usb_devices(dut.adb.tool == "hdc");
                }
            }
            Dd::Port | Dd::RelayBoardPort => self.serial_ports = self.device.list_ports(),
            Dd::CaptureMode(_) => self.media.retry_mode_probe(),
            Dd::Baud => {}
        }
        self.open_dd = Some(dd.key().into());
    }

    /// 下拉选择（连接页 combobox）：input + chevron + 展开列表。
    #[allow(clippy::too_many_arguments)]
    pub fn combobox(
        &mut self,
        id: In,
        dd: Dd,
        value: &str,
        placeholder: &str,
        mono: bool,
        options: &[(String, String)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Div {
        let p = self.p;
        let open = self.open_dd.as_deref() == Some(dd.key().as_ref());
        let media_dut = matches!(dd, Dd::CaptureMode(_))
            .then(|| self.duts.selected().map(|dut| dut.id.clone()))
            .flatten();
        let key = id.key();
        let mut container = div().relative().w_full();
        let mut row = div().flex().items_center().gap(px(4.)).child(
            div().flex_1().min_w_0().child(self.text_field(
                id,
                value,
                placeholder,
                mono,
                p,
                window,
                cx,
            )),
        );
        row = row.child(
            div()
                .id(SharedString::from(format!("{key}-toggle")))
                .flex_none()
                .size(px(34.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .text_color(p.muted)
                .hover(|s| s.bg(p.hover).text_color(p.text))
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    if this.open_dd.as_deref() == Some(dd.key().as_ref()) {
                        this.open_dd = None;
                    } else {
                        this.open_combo(dd);
                    }
                    cx.notify();
                }))
                .child(icon("chevron").size(px(16.)).text_color(p.muted)),
        );
        container = container.child(row);
        if open {
            // Floating list (same shape as `select_field`): an inline list would
            // push the rows below it, moving this row's separators. `deferred`
            // paints it above later siblings, so nothing occludes it.
            let mut list = div()
                .id(SharedString::from(format!("{key}-list")))
                .absolute()
                .top_full()
                .left_0()
                .mt(px(4.))
                .w_full()
                .max_h(px(250.))
                .overflow_y_scroll()
                .bg(p.surface)
                .border_1()
                .border_color(p.border)
                .rounded(px(8.))
                .shadow_md()
                .p(px(4.))
                .occlude();
            if options.is_empty() {
                list = list.child(
                    div()
                        .px(px(10.))
                        .py(px(8.))
                        .text_color(p.muted)
                        .child(crate::i18n::label("noCandidates")),
                );
            }
            for (value_label, detail) in options {
                let active = value_label == value;
                let v = value_label.clone();
                let media_dut = media_dut.clone();
                let (label, detail) = if value_label.is_empty() {
                    (detail.as_str(), "")
                } else {
                    (value_label.as_str(), detail.as_str())
                };
                list = list.child(
                    div()
                        .id(SharedString::from(format!(
                            "{key}-opt-{}",
                            value_label.replace(['\\', '/'], "-")
                        )))
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            if let Some(ref target) = media_dut
                                && this.duts.selected().is_none_or(|dut| &dut.id != target)
                            {
                                return;
                            }
                            this.input_set(id, v.clone());
                            let model = this.input_value(id);
                            if let Some(editor) = this.text_editors.get_mut(&id) {
                                editor.state.update(cx, |state, cx| {
                                    if state.value().as_ref() != v {
                                        state.set_value(v.clone(), window, cx);
                                    }
                                });
                                editor.model_value = model;
                            }
                            this.open_dd = None;
                            cx.notify();
                        }))
                        .flex()
                        .items_center()
                        .gap(px(11.))
                        .px(px(10.))
                        .py(px(8.))
                        .rounded(px(5.))
                        .cursor_pointer()
                        .text_color(p.text)
                        .hover(|s| s.bg(p.hover))
                        .when(active, |el| el.bg(p.accent_weak))
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_family(crate::theme::MONO_FONT)
                                .font_weight(gpui_kit::FontWeight(600.0))
                                .child(SharedString::from(label.to_owned())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(11.))
                                .font_family(crate::theme::MONO_FONT)
                                .text_color(p.faint)
                                .overflow_hidden()
                                .child(SharedString::from(detail.to_owned())),
                        )
                        .when(active, |el| {
                            el.child(icon("check").size(px(14.)).text_color(p.accent))
                        }),
                );
            }
            container = container.child(gpui_kit::deferred(list).with_priority(1));
        }
        container
    }

    /// 下拉选择（固定选项集 + 回调）：agent/settings 使用。
    #[allow(clippy::too_many_arguments)]
    pub fn select_field(
        &mut self,
        key: &'static str,
        value: String,
        options: Vec<(String, usize)>,
        selected: Option<usize>,
        on_change: OnChangeFn,
        p: &'static Palette,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Div {
        let open = self.open_dd.as_deref() == Some(key);
        let mut container = div().relative().min_w_0();
        container = container.child(
            div()
                .id(SharedString::from(format!("{key}-btn")))
                .on_click(cx.listener(move |this, _ev, _win, cx| {
                    this.open_dd = if this.open_dd.as_deref() == Some(key) {
                        None
                    } else {
                        Some(key.to_string())
                    };
                    cx.notify();
                }))
                .min_h(px(33.))
                .w_full()
                .px(px(11.))
                .py(px(6.))
                .rounded(px(8.))
                .bg(p.bg)
                .border_1()
                .border_color(p.border)
                .text_size(px(12.5))
                .text_color(p.text)
                .overflow_hidden()
                .flex()
                .items_center()
                .child(SharedString::from(value))
                .child(
                    div()
                        .ml_auto()
                        .pl(px(6.))
                        .child(icon("chevron").size(px(14.)).text_color(p.muted)),
                ),
        );
        if open {
            let mut list = div()
                .absolute()
                .top_full()
                .left_0()
                .mt(px(4.))
                .w_full()
                .max_h(px(240.))
                .id(SharedString::from(format!("{key}-list")))
                .overflow_y_scroll()
                .bg(p.surface)
                .border_1()
                .border_color(p.border)
                .rounded(px(8.))
                .shadow_md()
                .p(px(4.))
                .occlude();
            for (label, index) in options {
                let active = selected == Some(index);
                let on_change = on_change.clone();
                list = list.child(
                    div()
                        .id(SharedString::from(format!("{key}-opt-{index}")))
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            this.open_dd = None;
                            on_change(this, index, window, cx);
                            cx.notify();
                        }))
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(10.))
                        .py(px(8.))
                        .rounded(px(5.))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(p.text)
                        .hover(|s| s.bg(p.hover))
                        .when(active, |el| el.bg(p.accent_weak))
                        .child(SharedString::from(label))
                        .when(active, |el| {
                            el.child(icon("check").size(px(14.)).text_color(p.accent))
                        }),
                );
            }
            container = container.child(gpui_kit::deferred(list).with_priority(1));
        }
        container
    }

    // ------------------------------------------------------------------
    // USB 调试设备扫描
    // ------------------------------------------------------------------

    pub fn usb_discovery(&self, hdc: bool) -> &UsbDiscovery {
        &self.usb_discovery[usize::from(hdc)]
    }

    pub(crate) fn test_flash_preset(&mut self, id: &str, cx: &mut Context<Self>) {
        if matches!(
            self.flash_preset_tests.get(id),
            Some(FlashPresetTest::Running)
        ) {
            return;
        }
        let Some(preset) = self
            .settings
            .flash_tools
            .iter()
            .find(|preset| preset.id == id)
        else {
            return;
        };
        let preset = preset.clone();
        self.flash_preset_tests
            .insert(id.to_owned(), FlashPresetTest::Running);
        let id = preset.id;
        let probe = cx.background_executor().spawn(async move {
            duthub_flash::FlashTester::real().run_preset_test(&preset.executable, &preset.test_args)
        });
        cx.spawn(async move |view, cx| {
            let result = probe.await;
            let _ = view.update(cx, |this, cx| {
                this.flash_preset_tests
                    .insert(id, FlashPresetTest::Finished(result));
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn set_dut_flash_preset(&mut self, dut_id: &str, preset_id: &str) {
        if let Some(dut) = self.duts.connection_mut(dut_id) {
            dut.flash = crate::data::selected_flash_config(
                Some(&dut.flash),
                Some(preset_id),
                &self.settings.flash_tools,
            );
            self.duts.mark_connection_dirty();
        }
    }

    pub(crate) fn save_usb_tools(&mut self) {
        let message = match store::save_settings_secured(&self.data_dir, &self.settings) {
            Ok(()) => crate::i18n::label("debugToolsSaved").to_owned(),
            Err(err) => format!("{} {err}", crate::i18n::label("saveFailed")),
        };
        self.notify_toast(message);
    }

    pub(super) fn browse_path(&mut self, id: In, directory: bool, cx: &mut Context<Self>) {
        let Some(editor) = self.text_editors.get(&id) else {
            return;
        };
        let state = editor.state.clone();
        let scope = editor.scope.clone();
        let selection = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: !directory,
            directories: directory,
            multiple: false,
            prompt: Some(i18n::label("selectPath").into()),
        });
        cx.spawn(async move |view, cx| match selection.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.first() {
                    let _ = view.update(cx, |this, cx| {
                        if this.editor_scope(id) == scope.as_deref()
                            && this
                                .text_editors
                                .get(&id)
                                .is_some_and(|editor| editor.state == state)
                        {
                            this.input_set(id, path.to_string_lossy().into_owned());
                            cx.notify();
                        }
                    });
                }
            }
            Ok(Err(err)) => {
                let _ = view.update(cx, |this, cx| {
                    this.notify_toast(format!("{} {err}", i18n::label("browsePathFailed")));
                    cx.notify();
                });
            }
            _ => {}
        })
        .detach();
    }

    pub(crate) fn select_usb_debug_tool(&mut self, tool: Option<&str>) {
        let Some(dut) = self.duts.selected_mut() else {
            return;
        };
        if let Some(tool) = tool {
            if !usb_debug_enabled(&dut.adb) || dut.adb.tool != tool {
                dut.adb.tool = tool.into();
                dut.adb.mode = "usb".into();
                dut.adb.serial.clear();
                self.duts.mark_connection_dirty();
            }
            let hdc = tool == "hdc";
            self.usb_debug_view = Some(hdc);
            self.scan_usb_devices(hdc);
            self.open_dd = Some(Dd::AdbSerial.key().into());
        } else {
            dut.adb.mode = "disabled".into();
            dut.adb.serial.clear();
            self.duts.mark_connection_dirty();
        }
    }

    pub(crate) fn show_dut_connections(&mut self, dut_id: &str) {
        if let Some(index) = self.duts.file.duts.iter().position(|dut| dut.id == dut_id) {
            self.select_connection(index);
            self.connection_dut_scope = None;
            self.connection_open = true;
        }
    }

    pub(super) fn active_serial_dut(&self) -> Option<&str> {
        if self.view != ViewMode::Serial {
            return None;
        }
        self.sessions
            .get(self.active_session)
            .map(|view| view.dut_id.as_str())
    }

    pub(super) fn toggle_connection_dock(&mut self) {
        self.menu_open = false;
        self.open_dd = None;
        if self.connection_open {
            self.connection_open = false;
            self.connection_dut_scope = None;
        } else if self.view == ViewMode::Serial {
            let Some(dut_id) = self.active_serial_dut().map(str::to_owned) else {
                return;
            };
            let Some(index) = self.duts.file.duts.iter().position(|dut| dut.id == dut_id) else {
                self.notify_toast(i18n::label("connectionMissing"));
                return;
            };
            self.select_connection(index);
            self.connection_dut_scope = Some(dut_id);
            self.connection_open = true;
        } else {
            self.connection_dut_scope = None;
            self.connection_open = true;
            self.ensure_current_a2a_token();
        }
    }

    pub(super) fn clear_dut_endpoint_drafts(&mut self) {
        self.drafts.remove(&In::DutMcpPort);
        self.drafts.remove(&In::DutA2aPort);
        self.drafts.remove(&In::DutA2aName);
    }

    pub(crate) fn select_connection(&mut self, index: usize) {
        self.duts.editing_new_connection = false;
        self.duts.selected = index;
        self.open_dd = None;
        self.clear_dut_endpoint_drafts();
        // F13：Agent prompt 草稿按 DUT id 保存在 `agent_prompts`，切换标签直接换实体，
        // 不复制/不清空文本。W4c：「发送后清空」请求也按 DUT 键控——切换标签
        // 不再丢弃其他 DUT 的挂起清空请求（旧实现的全局 Option 会误清）。
        if let Some(dut_id) = self.duts.selected().map(|dut| dut.id.clone()) {
            self.cache_a2a_token(&dut_id);
        }
    }

    pub(crate) fn set_config_page(&mut self, page: Option<ConfigPage>) {
        self.overlay = page;
        self.open_dd = None;
        // Leaving the control-device page discards an unsaved board draft (the
        // same way the connection form drops its drafts); edits only persist
        // through "save device".
        if page != Some(ConfigPage::Relay) {
            self.relay_board_draft = None;
            self.relay_board_name = None;
        }
        // 打开设置主页或工具下载页时刷新「工具下载」行快照（读盘事实，避免每帧 stat）。
        if matches!(page, Some(ConfigPage::Settings | ConfigPage::ToolDownloads)) {
            self.refresh_tool_rows();
        }
    }

    pub(super) fn pane_widths(&self, viewport: Pixels) -> (Pixels, Pixels) {
        let connection = self.connection_open.then(|| {
            self.connection_pane_width
                .unwrap_or_else(|| (viewport * 0.4).clamp(px(300.), px(520.)))
        });
        let tools =
            (self.view == ViewMode::Serial && self.side_panel.visible.is_some()).then(|| {
                self.tools_pane_width
                    .unwrap_or_else(|| (viewport * 0.42).clamp(px(260.), px(440.)))
            });
        workspace_pane_widths(viewport, connection, tools)
    }

    pub(super) fn drag_pane(&mut self, event: &DragMoveEvent<PaneDivider>, cx: &mut Context<Self>) {
        let viewport = event.bounds.size.width;
        let (left, right) = self.pane_widths(viewport);
        let x = event.event.position.x - event.bounds.origin.x;
        let divider = *event.drag(cx);
        match divider {
            PaneDivider::Connection => {
                let next = x.min(viewport - CENTER_PANE_MIN_WIDTH - right);
                self.connection_pane_width = Some(next);
                self.connection_pane_width = Some(self.pane_widths(viewport).0);
            }
            PaneDivider::Tools => {
                let next = (viewport - x).min(viewport - CENTER_PANE_MIN_WIDTH - left);
                self.tools_pane_width = Some(next);
                self.tools_pane_width = Some(self.pane_widths(viewport).1);
            }
        }
        cx.notify();
    }

    pub(crate) fn begin_new_connection(
        &mut self,
        connect_after_save: bool,
        cx: &mut Context<Self>,
    ) {
        if self.duts.new_connection.is_none() {
            let name = format!("DUT-{}", self.duts.file.duts.len() + 1);
            let mut dut = demo_dut_template(&name, "");
            dut.id = format!("dut-{}", uuid::Uuid::new_v4());
            dut.mcp = duthub_core::model::McpConfig::StreamableHttp {
                url: format!(
                    "http://127.0.0.1:{}/mcp",
                    self.settings.agent_access.http_port
                ),
            };
            self.duts.new_connection = Some(crate::data::NewConnection {
                dut,
                alerts: None,
                garbled: None,
                connect_after_save,
            });
        }
        self.show_new_connection();
        self.serial_ports = self.device.list_ports();
        cx.notify();
    }

    pub(crate) fn show_new_connection(&mut self) {
        self.view = ViewMode::Home;
        self.connection_dut_scope = None;
        self.duts.editing_new_connection = true;
        self.connection_open = true;
        self.open_dd = None;
        self.clear_dut_endpoint_drafts();
    }

    pub(crate) fn cancel_new_connection(&mut self) {
        if self.duts.editing_new_connection {
            self.connection_open = false;
        }
        if let Some(draft) = self.duts.new_connection.take() {
            self.heartbeat_drafts.remove(&draft.dut.id);
            self.boot_similarity_drafts.remove(&draft.dut.id);
        }
        self.duts.editing_new_connection = false;
        self.open_dd = None;
    }

    pub(super) fn persist_new_connection(&mut self) -> Result<(usize, bool), String> {
        let draft = self
            .duts
            .new_connection
            .as_ref()
            .ok_or_else(|| crate::i18n::label("connectionMissing").to_owned())?;
        let mut dut = draft.config_to_save().map_err(crate::i18n::label)?;
        if dut.serial.split_logs {
            self.boot_similarity(&dut.id)?;
        }
        let connect = draft.connect_after_save;
        let alert_settings = draft.alerts.clone();
        let garbled = draft.garbled.clone();
        let heartbeat = HashMap::from([(dut.id.clone(), self.heartbeat_config(&dut.id))]);
        self.heartbeat.commit_configs(&heartbeat, |configs| {
            let mut file = store::load_duts(&self.data_dir)
                .map_err(|err| err.to_string())?
                .unwrap_or_else(|| DutsState::empty().file);
            file.duts.push(dut.clone());
            if let Some(alerts) = &alert_settings {
                file.alerts_by_dut.insert(dut.id.clone(), alerts.clone());
            }
            if let Some(garbled) = &garbled {
                if garbled.enabled {
                    file.garbled_by_dut.insert(dut.id.clone(), garbled.clone());
                } else {
                    file.garbled_by_dut.remove(&dut.id);
                }
            }
            Self::validate_bindings(&mut file)?;
            Self::apply_heartbeat_configs(&mut file, configs);
            store::save_duts_atomic(&self.data_dir, &file).map_err(|err| err.to_string())?;
            dut = file.duts.pop().expect("the new connection was appended");
            Ok(())
        })?;
        if let Some(alerts) = alert_settings {
            self.duts.file.alerts_by_dut.insert(dut.id.clone(), alerts);
            self.alerts
                .lock()
                .unwrap()
                .set_dut_settings(&self.duts.file.alerts_by_dut);
        }
        if let Some(garbled) = garbled {
            if garbled.enabled {
                self.duts
                    .file
                    .garbled_by_dut
                    .insert(dut.id.clone(), garbled);
            } else {
                self.duts.file.garbled_by_dut.remove(&dut.id);
            }
        }
        let mut resolved = dut.clone();
        resolved.relay = self.duts.file.resolve_relay(&dut.relay);
        self.control.register_dut(resolved);
        self.duts.add_saved_connection(dut);
        self.cancel_new_connection();
        // The DUT is now in `duts.file`, so the runtime gate can be derived.
        self.sync_garbled_runtime();
        let index = self.duts.file.duts.len() - 1;
        self.select_connection(index);
        self.ensure_dut_mcp_endpoints();
        Ok((index, connect))
    }

    pub(crate) fn save_connection(&mut self, cx: &mut Context<Self>) {
        if self.duts.editing_new_connection {
            match self.persist_new_connection() {
                Ok((index, true)) => {
                    self.connection_open = false;
                    self.connect_dut(index, cx);
                }
                Ok((_, false)) => self.notify_toast(crate::i18n::label("saveBinding")),
                Err(err) => {
                    self.notify_toast(format!("{} {err}", crate::i18n::label("saveFailed")));
                }
            }
        } else {
            if self.save_duts() {
                self.connection_open = false;
                let index = self.duts.selected;
                if let Some(dut) = self.duts.file.duts.get(index) {
                    if let Some(session_index) = self
                        .sessions
                        .iter()
                        .position(|session| session.dut_id == dut.id && session.source == "device")
                    {
                        self.active_session = session_index;
                        self.view = ViewMode::Serial;
                        self.terminal_focus_pending = true;
                    } else if !dut.serial.port.trim().is_empty() {
                        self.connect_dut(index, cx);
                    }
                }
            }
        }
        cx.notify();
    }

    pub fn scan_usb_devices(&mut self, hdc: bool) {
        let (kind, configured, fallback) = if hdc {
            (JobKind::Hdc, &self.settings.tools.hdc_executable, "hdc")
        } else {
            (JobKind::Adb, &self.settings.tools.adb_executable, "adb")
        };
        // Downloaded tool first: explicit path > .duthub/tools install > PATH.
        let exe = duthub_desktop::tool_resolver::effective_spec(configured, fallback);
        let job = self.next_job;
        self.next_job += 1;
        self.usb_discovery[usize::from(hdc)] = UsbDiscovery { job, result: None };
        let client = self.adb_client.clone();
        let queue = self.job_queue.clone();
        cx_spawn_job(move || {
            let outcome = if hdc {
                duthub_adb::HdcClient::real()
                    .list_targets(&exe)
                    .map(|targets| {
                        targets
                            .into_iter()
                            .map(|target| AdbDevice {
                                serial: target.key,
                                state: target.state,
                                model: None,
                            })
                            .collect()
                    })
            } else {
                client.devices_l(&exe)
            }
            .map_err(|err| err.to_string());
            queue.lock().unwrap().push(JobResult { job, kind, outcome });
        });
    }

    pub fn press_relay(
        &mut self,
        dut_id: &str,
        binding: &'static str,
        channel: u8,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.relay_press.is_some() {
            return;
        }
        let Some(mut target) = self
            .duts
            .file
            .duts
            .iter()
            .find(|dut| dut.id == dut_id)
            .cloned()
        else {
            return;
        };
        target.relay = self.duts.file.resolve_relay(&target.relay);
        let (release, released) = tokio::sync::oneshot::channel();
        let control = self.control.clone();
        let worker = std::thread::spawn(move || {
            control
                .hold_channel(&target, channel, released)
                .map_err(|error| error.to_string())
        });
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.release_relay(cx);
            }
        });
        self.relay_press = Some(RelayPress {
            dut_id: dut_id.into(),
            binding,
            channel,
            release: Some(release),
            worker: Some(worker),
            _activation: activation,
        });
        cx.notify();
    }

    pub fn release_relay(&mut self, cx: &mut Context<Self>) {
        if let Some(press) = self.relay_press.as_mut()
            && press.release.take().is_some()
        {
            cx.notify();
        }
    }

    pub(super) fn poll_relay_press(&mut self) -> bool {
        if !self
            .relay_press
            .as_ref()
            .and_then(|press| press.worker.as_ref())
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            return false;
        }
        let mut press = self.relay_press.take().expect("checked worker");
        let worker = press.worker.take().expect("checked worker");
        let result = worker
            .join()
            .unwrap_or_else(|_| Err(crate::i18n::label("relayWorkerFailed").into()));
        if let Err(err) = result {
            let name = self
                .duts
                .file
                .duts
                .iter()
                .find(|dut| dut.id == press.dut_id)
                .map_or(press.dut_id.as_str(), |dut| dut.name.as_str());
            let action = format!("{} CH{}", crate::i18n::label("relayHold"), press.channel);
            let (title, detail) = crate::alerts::control_action_alert_texts(name, &action, &err);
            self.alerts.lock().unwrap().report_for(
                &press.dut_id,
                AlertSource::Control,
                title,
                detail,
            );
            self.notify_toast(err);
        }
        true
    }

    /// Sync all saved DUT relay/control configurations into `ControlService`.
    /// Called on startup and DUT mutations (never on the 250 ms snapshot tick).
    pub(crate) fn sync_control_duts(&self) {
        if self.duts.dirty {
            return;
        }
        for dut in &self.duts.file.duts {
            let mut resolved = dut.clone();
            resolved.relay = self.duts.file.resolve_relay(&dut.relay);
            self.control.register_dut(resolved);
        }
    }

    /// Cached host MXC sandbox capability probe (10s TTL) so GPUI render
    /// frames never execute OS probes at 60 FPS.
    pub(crate) fn cached_sandbox_status(&self) -> duthub_sandbox::SandboxStatus {
        const TTL: Duration = Duration::from_secs(10);
        let now = std::time::Instant::now();
        if let Some((probed_at, status)) = self.sandbox_status_cache.borrow().as_ref()
            && now.duration_since(*probed_at) < TTL
        {
            return status.clone();
        }
        let status = duthub_sandbox::status();
        *self.sandbox_status_cache.borrow_mut() = Some((now, status.clone()));
        status
    }

    /// Push current DUT summaries and recent serial chunks into the shared MCP
    /// snapshot (`SnapshotWriter` flushes to disk only when content changes).
    pub(super) fn refresh_snapshot(&mut self) {
        let media_bindings = self
            .duts
            .file
            .duts
            .iter()
            .filter(|dut| {
                crate::media::Source::sources(&dut.media).any(|source| {
                    crate::media_page::saved_capture(&self.duts, &dut.id, source).is_ok()
                })
            })
            .map(|dut| dut.id.clone())
            .collect();
        let ts_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let duts = self
            .duts
            .file
            .duts
            .iter()
            .map(|d| {
                let source = self
                    .device
                    .session_source(&d.id)
                    .unwrap_or("idle")
                    .to_string();
                let session = format!("sess-{}", d.id);
                // Keep the newest window so the stdio sequence cursor keeps advancing.
                let lines = self
                    .device
                    .read_log_tail(&session, 150)
                    .into_iter()
                    .filter_map(|e| match e.event {
                        duthub_core::ipc::UiEvent::SerialChunk { text, .. } => {
                            Some(duthub_mcp::snapshot::SnapshotLine { seq: e.seq, text })
                        }
                        _ => None,
                    })
                    .collect();
                duthub_mcp::snapshot::SnapshotDut {
                    dut_id: d.id.clone(),
                    name: self.duts.saved_name(&d.id).unwrap_or(&d.name).to_owned(),
                    source,
                    serial: duthub_mcp::state::SerialBindInfo {
                        host: d.serial.host.clone(),
                        port: d.serial.port.clone(),
                        baud: d.serial.baud,
                        format: d.serial.format.clone(),
                    },
                    adb_serial: self
                        .duts
                        .saved_usb(&d.id)
                        .filter(|adb| adb.tool != "hdc" && !adb.serial.trim().is_empty())
                        .map(|adb| adb.serial.clone()),
                    relay: relay_brief_for_dut(d, &self.duts.file.relay_boards),
                    mcp_label: None,
                    lines,
                }
            })
            .collect();
        *self.snapshot_ref.lock().unwrap() = duthub_mcp::snapshot::StateSnapshot {
            version: 1,
            ts_unix_ms,
            duts,
            media_bindings,
        };
    }

    pub(super) fn drain_jobs(&mut self) -> bool {
        let drained: Vec<JobResult> = std::mem::take(&mut *self.job_queue.lock().unwrap());
        if drained.is_empty() {
            return false;
        }
        for result in drained {
            self.apply_job_result(result);
        }
        true
    }

    /// 只接收对应扫描的最新结果。
    pub(super) fn apply_job_result(&mut self, result: JobResult) {
        let scan = &mut self.usb_discovery[usize::from(result.kind == JobKind::Hdc)];
        if result.job == scan.job {
            scan.result = Some(result.outcome);
        }
    }
}

/// Build the MCP `RelayBrief` from a DUT's configured relay binding and shared
/// board list so external MCP clients can inspect exact bound channels (such as
/// `reset` only vs `maskrom` / `recovery` / `power`).
fn relay_brief_for_dut(
    dut: &duthub_core::model::DutConfig,
    boards: &[duthub_core::model::RelayBoard],
) -> Option<duthub_mcp::state::RelayBrief> {
    use duthub_core::model::RelayConfig;
    use duthub_mcp::state::{RelayBrief, RelayChannelsBrief};

    match &dut.relay {
        RelayConfig::Disabled => None,
        RelayConfig::UsbBoard { board_id, channels } => {
            let board = boards.iter().find(|board| board.id == *board_id);
            let brief_channels = RelayChannelsBrief {
                power: channels.power,
                reset: channels.reset,
                recovery: channels.recovery,
                maskrom: channels.maskrom,
                power_key: channels.power_key,
            };
            let capabilities = brief_channels.capabilities();
            Some(RelayBrief {
                mode: "usb-board".into(),
                board_id: Some(board_id.clone()),
                board_name: board.map(|b| b.name.clone()),
                node: board
                    .map(|b| b.node.clone())
                    .filter(|node| !node.trim().is_empty()),
                driver: board
                    .map(|b| b.driver.clone())
                    .filter(|driver| !driver.trim().is_empty()),
                power_outlet: None,
                channels: brief_channels,
                capabilities,
            })
        }
        RelayConfig::UsbSerial {
            node,
            driver,
            channels,
            ..
        } => {
            let brief_channels = RelayChannelsBrief {
                power: channels.power,
                reset: channels.reset,
                recovery: channels.recovery,
                maskrom: channels.maskrom,
                power_key: channels.power_key,
            };
            let capabilities = brief_channels.capabilities();
            Some(RelayBrief {
                mode: "usb-serial".into(),
                board_id: None,
                board_name: None,
                node: (!node.trim().is_empty()).then(|| node.clone()),
                driver: (!driver.trim().is_empty()).then(|| driver.clone()),
                power_outlet: None,
                channels: brief_channels,
                capabilities,
            })
        }
        RelayConfig::SnmpPdu { snmp, outlets } => Some(RelayBrief {
            mode: "snmp-pdu".into(),
            board_id: None,
            board_name: None,
            node: (!snmp.address.trim().is_empty()).then(|| snmp.address.clone()),
            driver: Some("snmp-pdu".into()),
            power_outlet: Some(outlets.power),
            channels: RelayChannelsBrief::default(),
            capabilities: vec!["power".into()],
        }),
    }
}
