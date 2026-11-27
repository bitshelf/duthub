use super::{Workbench, store, i18n, In, configured_terminal_font_size, FluentBuilder, serial_backspace_byte, serial_enter_bytes, DEFAULT_TERMINAL_FONT_SIZE, Window, SharedString, Pixels, Styled, theme, px, Context, IntoElement, SearchableVec, AppContext, SelectState, SelectEvent, FontPicker, ParentElement, div, Select, KeyDownEvent, MAX_TERMINAL_FONT_SIZE, MIN_TERMINAL_FONT_SIZE, Duration, InteractiveElement, DutAction, DutGrant, HashMap, boot_reference_path, LinkState, GarbleLevel, Lang, DutsState};

impl Workbench {
    // ------------------------------------------------------------------
    // 设置持久化
    // ------------------------------------------------------------------

    pub fn persist_settings(&mut self) {
        if let Ok(mut cfg) = self.mcp_tools_config.lock() {
            cfg.flash_tools = self.settings.flash_tools.clone();
            cfg.adb_executable = self.settings.tools.adb_executable.clone();
        }
        if let Err(err) = store::save_settings_secured(&self.data_dir, &self.settings) {
            self.notify_toast(i18n::message(
                "saveSettingsFailed",
                &[("error", &err.to_string())],
            ));
        }
    }

    pub(crate) fn save_workspace_dir(&mut self) {
        let value = self.input_value(In::WorkDir);
        let value = value.trim();
        let mut settings = self.settings.clone();
        settings.workspace_dir = (!value.is_empty()).then(|| value.to_owned());
        if let Err(error) = store::save_settings_secured(&self.data_dir, &settings) {
            self.notify_toast(format!("{} {error}", i18n::label("saveFailed")));
            return;
        }
        self.settings = settings;
        self.drafts.remove(&In::WorkDir);
    }

    pub(crate) fn save_dut_storage_limit(&mut self) {
        let input = self.input_value(In::DutStorageGib);
        let Ok(limit) = input.trim().parse::<std::num::NonZeroU32>() else {
            self.notify_toast(crate::i18n::label("invalidDutStorageLimit"));
            return;
        };
        let mut settings = self.settings.clone();
        settings.dut_storage_gib = Some(limit.get());
        if let Err(error) = store::save_settings_secured(&self.data_dir, &settings) {
            self.notify_toast(format!("{} {error}", crate::i18n::label("saveFailed")));
            return;
        }
        self.settings = settings;
        self.drafts.remove(&In::DutStorageGib);
        self.artifact_quota
            .set_limit(u64::from(limit.get()) * duthub_core::dut_quota::GIB);
        self.notify_toast(crate::i18n::label("dutStorageLimitSaved"));
    }

    pub(crate) fn save_model_providers(
        &mut self,
        providers: Vec<duthub_core::model::ModelProvider>,
    ) -> Result<(), String> {
        let mut candidate = self.settings.clone();
        candidate.agent.providers = providers;
        self.save_model_catalog(candidate)
    }

    pub(crate) fn save_model_catalog(
        &mut self,
        candidate: duthub_core::SettingsFile,
    ) -> Result<(), String> {
        store::save_settings_secured(&self.data_dir, &candidate).map_err(|err| err.to_string())?;
        self.agent_model_name = candidate.agent.active_model_id.clone().unwrap_or_default();
        self.settings = candidate;
        Ok(())
    }

    pub(crate) fn terminal_font_size(&self) -> f32 {
        configured_terminal_font_size(&self.settings.terminal)
    }

    /// 串口设置页「滚动缓冲行数」保存（Baud 式：草稿解析成功才落盘）。
    /// 新会话生效（Term 配置在会话创建时读取）。
    pub(crate) fn save_serial_scrollback(&mut self) {
        let input = self.input_value(In::SerialScrollback);
        let trimmed = input.trim();
        let rows = if trimmed.is_empty() {
            None
        } else {
            match trimmed.parse::<u32>() {
                Ok(rows)
                    if (duthub_device::term::MIN_SCROLLBACK_ROWS
                        ..=duthub_device::term::MAX_SCROLLBACK_ROWS)
                        .contains(&(rows as usize)) =>
                {
                    Some(rows)
                }
                _ => {
                    self.notify_toast(crate::i18n::label("invalidScrollback"));
                    return;
                }
            }
        };
        let mut settings = self.settings.clone();
        settings.serial.scrollback_rows = rows;
        if let Err(error) = store::save_settings_secured(&self.data_dir, &settings) {
            self.notify_toast(format!("{} {error}", crate::i18n::label("saveFailed")));
            return;
        }
        self.settings = settings;
        self.drafts.remove(&In::SerialScrollback);
        // 即时下发：后续新建会话按新缓冲打开（已开会话不受影响）
        self.device
            .configure_scrollback(rows.map(|rows| rows as usize));
        self.notify_toast(crate::i18n::label("serialSaved"));
    }

    /// Re-resolve `tools.capture` (ffmpeg through `tool_resolver`: explicit
    /// setting > downloaded install > PATH) and hand the copy to `MediaView`.
    /// A resolution miss keeps the configured value so the existing
    /// `executable()` reports it. Running captures keep their process; later
    /// starts use the new path (§3.7).
    pub(crate) fn refresh_capture_settings(&mut self) {
        let resolved = duthub_desktop::capture_backend::resolved_capture_settings(
            &self.settings.tools.capture,
        );
        self.media.set_capture_settings(&resolved);
    }

    /// Capture settings page: idle-reclaim seconds (`tools.capture.idleSecs`).
    ///
    /// Empty clears the key (= desktop default); an out-of-range value is
    /// rejected here, never clamped into settings.json. Validation lives in
    /// this save action only — `persist_settings` has 20+ call sites and must
    /// not be blocked by an idle typo (§3.4).
    pub(crate) fn save_capture_idle(&mut self) {
        let raw = self.input_value(In::CaptureIdle);
        let trimmed = raw.trim();
        let idle = if trimmed.is_empty() {
            None
        } else if let Some(secs) = trimmed
            .parse::<u32>()
            .ok()
            .and_then(duthub_desktop::capture_backend::validate_idle_secs) { Some(secs) } else {
            self.notify_toast(crate::i18n::label("invalidCaptureIdle"));
            return;
        };
        let mut candidate = self.settings.clone();
        candidate.tools.capture.idle_secs = idle;
        if let Err(error) = store::save_settings_secured(&self.data_dir, &candidate) {
            self.notify_toast(format!("{} {error}", crate::i18n::label("saveFailed")));
            return;
        }
        self.settings = candidate;
        self.drafts.remove(&In::CaptureIdle);
        // Effective immediately: the idle window is read per tick, so the new
        // value applies to captures that have not idled out yet.
        self.refresh_capture_settings();
    }

    /// 键盘退格字节（设置驱动；默认 DEL 0x7f）。
    pub(crate) fn backspace_byte(&self) -> u8 {
        serial_backspace_byte(self.settings.serial.backspace)
    }

    /// 键盘回车行尾（设置驱动；默认 CR）。
    pub(crate) fn enter_bytes(&self) -> &'static [u8] {
        serial_enter_bytes(self.settings.serial.enter_ending)
    }

    pub(crate) fn set_terminal_font_size(&mut self, size: Option<f32>) {
        self.terminal_zoom_size = None;
        self.settings.terminal.font_size = size.filter(|size| *size != DEFAULT_TERMINAL_FONT_SIZE);
        self.persist_settings();
    }

    pub(crate) fn terminal_font(&self, window: &Window) -> (SharedString, Pixels, Pixels) {
        let family = self
            .settings
            .terminal
            .font_family
            .as_deref()
            .filter(|family| !family.trim().is_empty())
            .unwrap_or(theme::MONO_FONT);
        let size = self
            .terminal_zoom_size
            .unwrap_or_else(|| self.terminal_font_size());
        let text = window.text_system();
        let font_id = text.resolve_font(&gpui_kit::font(family.to_owned()));
        let cell_width = text
            .ch_advance(font_id, px(size))
            .unwrap_or(px(size * 0.6))
            .max(px(1.));
        (SharedString::from(family.to_owned()), px(size), cell_width)
    }

    pub(crate) fn terminal_font_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let default = i18n::t(self.lang, "terminalFontDefault").to_owned();
        if self
            .terminal_font_picker
            .as_ref()
            .is_none_or(|picker| picker.lang != self.lang)
        {
            let text = window.text_system();
            let mut fonts = text.all_font_names();
            fonts.retain(|family| {
                let id = text.resolve_font(&gpui_kit::font(family.clone()));
                let advances = ['i', 'M', '0', ' '].map(|ch| text.advance(id, px(20.), ch).ok());
                let Some(first) = advances[0] else {
                    return false;
                };
                first.width > px(0.)
                    && advances.into_iter().all(|advance| {
                        advance.is_some_and(|advance| {
                            (f32::from(advance.width - first.width)).abs() < 0.05
                        })
                    })
            });
            fonts.insert(0, default.clone());
            let items = SearchableVec::new(fonts.clone());
            let state = cx.new(|cx| SelectState::new(items, None, window, cx).searchable(true));
            let default_label = default.clone();
            let subscription = cx.subscribe(&state, move |this, _, event, cx| {
                if let SelectEvent::Confirm(Some(family)) = event {
                    this.settings.terminal.font_family =
                        (family != &default_label).then(|| family.clone());
                    this.persist_settings();
                    cx.notify();
                }
            });
            self.terminal_font_picker = Some(FontPicker {
                lang: self.lang,
                available: fonts,
                state,
                _subscription: subscription,
            });
        }
        let picker = self
            .terminal_font_picker
            .as_ref()
            .expect("font picker initialized");
        let state = &picker.state;
        let selected = self
            .settings
            .terminal
            .font_family
            .as_ref()
            .filter(|family| picker.available.contains(family))
            .unwrap_or(&default);
        if state.read(cx).selected_value() != Some(selected) {
            state.update(cx, |state, cx| {
                state.set_selected_value(selected, window, cx);
            });
        }
        div().flex_none().w(px(214.)).child(
            Select::new(state)
                .w_full()
                .accessibility_label(i18n::t(self.lang, "terminalFontFamily").to_owned())
                .search_placeholder(i18n::t(self.lang, "terminalFontSearch").to_owned()),
        )
    }

    pub(crate) fn terminal_zoom_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let k = &event.keystroke;
        let primary = if cfg!(target_os = "macos") {
            k.modifiers.platform && !k.modifiers.control
        } else {
            k.modifiers.control && !k.modifiers.platform
        };
        if !primary || k.modifiers.alt {
            return false;
        }
        let base = self.terminal_font_size();
        let current = self.terminal_zoom_size.unwrap_or(base);
        let next = match k.key.as_str() {
            "+" | "=" => (current + 1.).min(MAX_TERMINAL_FONT_SIZE),
            "-" | "_" => (current - 1.).max(MIN_TERMINAL_FONT_SIZE),
            "0" => base,
            _ => return false,
        };
        if next != current {
            self.terminal_zoom_size = (next != base).then_some(next);
            cx.notify();
        }
        cx.stop_propagation();
        true
    }

    // ------------------------------------------------------------------
    // Agent 接入（设置页面板动作；认证运行时热替换 + settings.json 落盘）
    // ------------------------------------------------------------------

    /// 先原子保存，再发布运行时配置；写盘失败时恢复已提交配置。
    pub(super) fn sync_agent_access(&mut self) -> bool {
        if let Err(err) = store::save_settings_secured(&self.data_dir, &self.settings) {
            self.settings.agent_access = self.access_runtime.settings();
            self.notify_toast(i18n::message(
                "saveSettingsFailed",
                &[("error", &err.to_string())],
            ));
            return false;
        }
        self.access_runtime
            .update(self.settings.agent_access.clone());
        if let Some(lease) = self.device.lease_manager() {
            lease.set_auth_enabled(self.access_runtime.enabled());
            lease.set_ttl(Duration::from_secs(u64::from(
                self.access_runtime.lease_ttl_secs().max(1),
            )));
        }
        true
    }

    /// 设置页「Agent 接入」保存：提交端口/TTL 草稿（解析失败保留原值）并同步。
    /// `监听端口重启生效（mcp_runtime` 启动时绑定）。
    pub fn save_agent_access(&mut self) {
        let Ok(port @ 1..=u16::MAX) = self.input_value(In::AgentAccessPort).trim().parse::<u16>()
        else {
            self.notify_toast(crate::i18n::label("mcpListenerInvalidPort"));
            return;
        };
        let Ok(ttl @ 1..=u32::MAX) = self.input_value(In::AgentAccessTtl).trim().parse::<u32>()
        else {
            self.notify_toast(crate::i18n::label("mcpInvalidLeaseTtl"));
            return;
        };
        let access = &self.settings.agent_access;
        let a2a_conflict = self
            .settings
            .a2a
            .as_ref()
            .is_some_and(|a2a| a2a.port == port)
            || self
                .duts
                .file
                .duts
                .iter()
                .any(|dut| dut.a2a.enabled && dut.a2a.port == port);
        if access
            .listeners
            .iter()
            .any(|listener| listener.port == port)
            || a2a_conflict
        {
            self.notify_toast(crate::i18n::label("mcpListenerPortInUse"));
            return;
        }
        if port != access.http_port && !self.port_is_free(port) {
            self.notify_toast(crate::i18n::label("mcpListenerPortInUse"));
            return;
        }
        self.settings.agent_access.http_port = port;
        self.settings.agent_access.lease_ttl_secs = ttl;
        if self.sync_agent_access() {
            self.drafts.remove(&In::AgentAccessPort);
            self.drafts.remove(&In::AgentAccessTtl);
            self.notify_toast(i18n::label("agentAccessSaved"));
        }
    }

    /// 全局 A2A 配置保存：端口需与 MCP / 每 DUT A2A 互不冲突；W4a 起该段
    /// 仅为兼容保留（运行时按每 DUT 端口绑定），port 0 / 保留端口一律拒绝。
    pub fn save_a2a_settings(&mut self) {
        if let Some(a2a) = self.settings.a2a.as_ref() {
            let port = a2a.port;
            let reserved = crate::a2a_runtime::combined_reserved_ports(&self.settings, &self.duts);
            if let Err(reason) = duthub_desktop::agent_ui::validate_a2a_port(port, &reserved) {
                self.notify_toast(reason);
                return;
            }
            let conflict = self.settings.agent_access.http_port == port
                || self
                    .settings
                    .agent_access
                    .listeners
                    .iter()
                    .any(|listener| listener.port == port)
                || self
                    .duts
                    .file
                    .duts
                    .iter()
                    .any(|dut| dut.a2a.enabled && dut.a2a.port == port);
            if conflict || !self.port_is_free(port) {
                self.notify_toast(crate::i18n::label("mcpListenerPortInUse"));
                return;
            }
        }
        self.persist_settings();
        self.notify_toast(crate::i18n::label("a2aSaved"));
    }

    /// auth 总开关翻转（开启即 deny by default；关闭恢复现状兼容免 token）。
    pub fn toggle_agent_auth(&mut self) {
        if self.settings.agent_access.enabled && self.settings.agent_access.lan_access {
            self.notify_toast(crate::i18n::label("mcpAuthRequiredForLan"));
            return;
        }
        self.settings.agent_access.enabled = !self.settings.agent_access.enabled;
        if self.sync_agent_access() && self.access_runtime.enabled() {
            if let Some(lease) = self.device.lease_manager() {
                lease.revoke_credential("local-admin");
            }
            self.ensure_dut_mcp_endpoints();
        }
    }

    /// 局域网访问开关：开启时自动打开认证（否则监听器全部 401）。
    /// 绑定 0.0.0.0 需重启生效；关闭只改配置，认证保持现状。
    pub fn toggle_agent_lan_access(&mut self) {
        let enabling = !self.settings.agent_access.lan_access;
        self.settings.agent_access.lan_access = enabling;
        if enabling {
            self.settings.agent_access.enabled = true;
        }
        if self.sync_agent_access() {
            if enabling
                && self.access_runtime.enabled()
                && let Some(lease) = self.device.lease_manager()
            {
                lease.revoke_credential("local-admin");
            }
            self.notify_toast(if enabling {
                i18n::label("lanAccessEnabledToast")
            } else {
                i18n::label("lanAccessDisabledToast")
            });
        }
    }

    /// 「生成并添加」凭据：token 由服务端 CSPRNG 生成（`dth-` 前缀 ≥256bit），
    /// 默认启用、未授权任何 DUT（deny by default）。
    pub fn add_agent_credential(&mut self) {
        let name = self
            .drafts
            .get(&In::AgentAccessCredName)
            .cloned()
            .unwrap_or_default()
            .trim()
            .to_string();
        if name.is_empty() {
            self.notify_toast(i18n::label("agentNameRequired"));
            return;
        }
        let id = format!("cred-{}", uuid::Uuid::new_v4());
        let token = duthub_core::agent_access::generate_token();
        let listener_id = self.agent_credential_listener.clone().filter(|id| {
            self.settings
                .agent_access
                .listeners
                .iter()
                .any(|listener| listener.id == *id && listener.dut_id.is_none())
        });
        self.settings
            .agent_access
            .agents
            .push(duthub_core::AgentCredential {
                id,
                listener_id,
                agent_name: name,
                token,
                enabled: true,
                grants: Vec::new(),
            });
        if self.sync_agent_access() {
            self.drafts.remove(&In::AgentAccessCredName);
            self.notify_toast(i18n::label("credentialAddedSaved"));
        }
    }

    pub(crate) fn add_mcp_listener(&mut self) {
        if !self.access_runtime.enabled() {
            self.notify_toast(crate::i18n::label("mcpEnableAuthFirst"));
            return;
        }
        let port = self
            .input_value(In::AgentAccessExtraPort)
            .trim()
            .parse::<u16>();
        let Ok(port @ 1..=u16::MAX) = port else {
            self.notify_toast(crate::i18n::label("mcpListenerInvalidPort"));
            return;
        };
        if self.port_configured(port, None) || !self.port_is_free(port) {
            self.notify_toast(crate::i18n::label("mcpListenerPortInUse"));
            return;
        }
        let access = &mut self.settings.agent_access;
        let id = uuid::Uuid::new_v4().to_string();
        access
            .listeners
            .push(duthub_core::agent_access::AgentListener {
                id: id.clone(),
                port,
                dut_id: None,
            });
        access.enabled = true;
        if self.sync_agent_access() {
            self.agent_credential_listener = Some(id);
            self.drafts.remove(&In::AgentAccessExtraPort);
            self.notify_toast(crate::i18n::label("mcpListenerRestart"));
        }
    }

    pub(crate) fn remove_mcp_listener(&mut self, id: &str) {
        let access = &mut self.settings.agent_access;
        if access
            .listeners
            .iter()
            .any(|listener| listener.id == id && listener.dut_id.is_some())
        {
            self.notify_toast(i18n::label("dutMcpDisableInConnection"));
            return;
        }
        if access
            .agents
            .iter()
            .any(|cred| cred.listener_id.as_deref() == Some(id))
        {
            self.notify_toast(crate::i18n::label("mcpListenerRevokeFirst"));
            return;
        }
        access.listeners.retain(|listener| listener.id != id);
        if self.sync_agent_access() {
            if self.agent_credential_listener.as_deref() == Some(id) {
                self.agent_credential_listener = None;
            }
            self.notify_toast(crate::i18n::label("mcpListenerRestart"));
        }
    }

    /// 凭据启用/停用（停用后运行时立即拒绝其 token）。
    pub fn toggle_agent_credential(&mut self, cred_id: &str) {
        let Some(cred) = self
            .settings
            .agent_access
            .agents
            .iter_mut()
            .find(|c| c.id == cred_id)
        else {
            return;
        };
        cred.enabled = !cred.enabled;
        let disabled = !cred.enabled;
        if self.sync_agent_access()
            && disabled
            && let Some(lease) = self.device.lease_manager()
        {
            lease.revoke_credential(cred_id);
        }
    }

    /// Revoke an MCP credential, releasing active leases and clearing its keyring secret.
    pub fn revoke_agent_credential(&mut self, cred_id: &str) {
        self.settings
            .agent_access
            .agents
            .retain(|c| c.id != cred_id);
        if self.sync_agent_access() {
            let _ = duthub_core::delete_mcp_token(cred_id);
            if let Some(lease) = self.device.lease_manager() {
                lease.revoke_credential(cred_id);
            }
            self.notify_toast(i18n::label("credentialRevoked"));
        }
    }

    /// 授权勾选翻转（deny by default：无该 DUT/动作即拒绝；全空动作回收授权行）。
    pub fn toggle_agent_grant(&mut self, cred_id: &str, dut_id: &str, action: DutAction) {
        let Some(cred) = self
            .settings
            .agent_access
            .agents
            .iter_mut()
            .find(|c| c.id == cred_id)
        else {
            return;
        };
        let existing = cred
            .grants
            .iter_mut()
            .find(|g| g.dut_id == dut_id)
            .map(|g| {
                if g.actions.contains(&action) {
                    g.actions.retain(|a| *a != action);
                } else {
                    g.actions.push(action);
                }
            });
        if existing.is_none() {
            cred.grants.push(DutGrant {
                dut_id: dut_id.to_string(),
                actions: vec![action],
            });
        }
        cred.grants.retain(|g| !g.actions.is_empty());
        self.sync_agent_access();
    }

    // ------------------------------------------------------------------
    // 连接管理：每 DUT 的 MCP / A2A 端点
    // ------------------------------------------------------------------

    /// 端口是否已被本进程配置占用（MCP/A2A；可忽略指定监听器）。
    pub(super) fn port_configured(&self, port: u16, ignore_listener: Option<&str>) -> bool {
        if self.settings.agent_access.http_port == port {
            return true;
        }
        if self
            .settings
            .agent_access
            .listeners
            .iter()
            .any(|listener| listener.port == port && Some(listener.id.as_str()) != ignore_listener)
        {
            return true;
        }
        if self
            .settings
            .a2a
            .as_ref()
            .is_some_and(|a2a| a2a.port == port)
        {
            return true;
        }
        self.duts
            .file
            .duts
            .iter()
            .any(|dut| dut.a2a.enabled && dut.a2a.port == port)
    }

    pub(super) fn port_is_free(&self, port: u16) -> bool {
        let ip = if self.settings.agent_access.lan_access {
            std::net::Ipv4Addr::UNSPECIFIED
        } else {
            std::net::Ipv4Addr::LOCALHOST
        };
        std::net::TcpListener::bind((ip, port)).is_ok()
    }

    /// 从 `start` 起找第一个未配置且可绑定的端口（MCP 8797 / A2A 8790）。
    pub(crate) fn allocate_port_from(
        &self,
        start: u16,
        ignore_listener: Option<&str>,
    ) -> Option<u16> {
        (start..=u16::MAX)
            .find(|&port| !self.port_configured(port, ignore_listener) && self.port_is_free(port))
    }

    /// DUT 专用 MCP 端点（端口 + 可选 Token；凭据缺失时 UI 可重新生成）。
    pub(crate) fn dut_mcp_endpoint(&self, dut_id: &str) -> Option<(u16, Option<String>)> {
        let listener = self.settings.agent_access.listener_for_dut(dut_id)?;
        let token = self
            .settings
            .agent_access
            .credential_for_listener(&listener.id)
            .map(|credential| credential.token.clone());
        Some((listener.port, token))
    }

    /// 主 MCP 端点实际监听地址（W4a：启动后由 `mcp_runtime` 填充；绑定失败
    /// 时为 None，失败原因在 [`McpRuntimeHandles::failures`]）。
    pub(crate) fn mcp_actual_endpoint(&self) -> Option<String> {
        self.mcp_bound_endpoint
            .get()
            .map(|addr| format!("http://{addr}/mcp"))
    }

    /// Provision a read-only credential and port for each saved DUT. Binding
    /// changes are applied when the MCP runtime starts.
    pub(super) fn ensure_dut_mcp_endpoints(&mut self) {
        if !self.settings.agent_access.enabled {
            return;
        }
        let mut added = false;
        for dut in &self.duts.file.duts {
            if self
                .settings
                .agent_access
                .listener_for_dut(&dut.id)
                .is_some()
            {
                continue;
            }
            let Some(port) = self.allocate_port_from(8797, None) else {
                self.notify_toast(i18n::label("noAvailableMcpPort"));
                break;
            };
            let listener_id = format!("listener-dut-{}", uuid::Uuid::new_v4());
            self.settings
                .agent_access
                .listeners
                .push(duthub_core::agent_access::AgentListener {
                    id: listener_id.clone(),
                    port,
                    dut_id: Some(dut.id.clone()),
                });
            self.settings
                .agent_access
                .agents
                .push(duthub_core::AgentCredential {
                    id: format!("cred-dut-{}", uuid::Uuid::new_v4()),
                    listener_id: Some(listener_id),
                    agent_name: format!("{} MCP", dut.name),
                    token: duthub_core::agent_access::generate_token(),
                    enabled: true,
                    grants: vec![DutGrant {
                        dut_id: dut.id.clone(),
                        actions: vec![DutAction::Read],
                    }],
                });
            added = true;
        }
        if added {
            self.sync_agent_access();
        }
    }

    /// 保存 DUT 专用 MCP 端口（重启生效）。
    pub fn save_dut_mcp_port(&mut self, dut_id: &str) {
        let Some(listener) = self.settings.agent_access.listener_for_dut(dut_id).cloned() else {
            return;
        };
        let raw = self.input_value(In::DutMcpPort);
        let Ok(port @ 1..=u16::MAX) = raw.trim().parse::<u16>() else {
            self.notify_toast(crate::i18n::label("mcpListenerInvalidPort"));
            return;
        };
        if port == listener.port {
            self.drafts.remove(&In::DutMcpPort);
            return;
        }
        if self.port_configured(port, Some(&listener.id)) || !self.port_is_free(port) {
            self.notify_toast(crate::i18n::label("mcpListenerPortInUse"));
            return;
        }
        if let Some(target) = self
            .settings
            .agent_access
            .listeners
            .iter_mut()
            .find(|target| target.id == listener.id)
        {
            target.port = port;
        }
        if self.sync_agent_access() {
            self.drafts.remove(&In::DutMcpPort);
            self.notify_toast(i18n::label("mcpPortSavedRestart"));
        }
    }

    /// 重新生成 DUT 专用 MCP Token（旧 Token 立即失效并回收租约）。
    pub fn regenerate_dut_mcp_token(&mut self, dut_id: &str) {
        let Some(listener) = self.settings.agent_access.listener_for_dut(dut_id).cloned() else {
            return;
        };
        let token = duthub_core::agent_access::generate_token();
        let credential_id;
        if let Some(credential) = self
            .settings
            .agent_access
            .agents
            .iter_mut()
            .find(|credential| credential.listener_id.as_deref() == Some(&listener.id))
        {
            credential.token = token;
            credential.enabled = true;
            credential_id = Some(credential.id.clone());
        } else {
            let Some(dut) = self.duts.file.duts.iter().find(|dut| dut.id == dut_id) else {
                return;
            };
            let id = format!("cred-dut-{}", uuid::Uuid::new_v4());
            self.settings
                .agent_access
                .agents
                .push(duthub_core::AgentCredential {
                    id: id.clone(),
                    listener_id: Some(listener.id),
                    agent_name: format!("{} MCP", dut.name),
                    token,
                    enabled: true,
                    grants: vec![DutGrant {
                        dut_id: dut_id.to_owned(),
                        actions: vec![DutAction::Read],
                    }],
                });
            credential_id = Some(id);
        }
        if self.sync_agent_access() {
            if let (Some(id), Some(lease)) = (credential_id, self.device.lease_manager()) {
                lease.revoke_credential(&id);
            }
            self.notify_toast(i18n::label("mcpTokenRegenerated"));
        }
    }

    pub(super) fn strip_dut_mcp(&mut self, dut_id: &str) -> bool {
        let Some(listener) = self.settings.agent_access.listener_for_dut(dut_id).cloned() else {
            return false;
        };
        let removed: Vec<String> = self
            .settings
            .agent_access
            .agents
            .iter()
            .filter(|credential| credential.listener_id.as_deref() == Some(&listener.id))
            .map(|credential| credential.id.clone())
            .collect();
        self.settings
            .agent_access
            .agents
            .retain(|credential| credential.listener_id.as_deref() != Some(&listener.id));
        self.settings
            .agent_access
            .listeners
            .retain(|target| target.id != listener.id);
        if !self.sync_agent_access() {
            self.notify_toast(i18n::label("mcpSyncEndpointsFailed"));
            return false;
        }
        for id in &removed {
            let _ = duthub_core::delete_mcp_token(id);
        }
        if let Some(lease) = self.device.lease_manager() {
            for id in removed {
                lease.revoke_credential(&id);
            }
        }
        true
    }

    /// DUT 专用 A2A Token 缓存（选中 DUT 时读取一次 keyring）。
    pub(crate) fn cached_a2a_token(&self, dut_id: &str) -> Option<&Result<Option<String>, String>> {
        self.a2a_tokens.get(dut_id)
    }

    pub(super) fn cache_a2a_token(&mut self, dut_id: &str) {
        let needs_load = match self.a2a_tokens.get(dut_id) {
            None | Some(Err(_)) => true,
            Some(Ok(_)) => false,
        };
        if needs_load {
            let value = duthub_core::get_a2a_token(dut_id).map_err(|error| error.to_string());
            self.a2a_tokens.insert(dut_id.to_owned(), value);
        }
    }

    /// 打开连接管理（底栏按钮等路径）时补读当前 DUT 的 A2A Token。
    pub(crate) fn ensure_current_a2a_token(&mut self) {
        if let Some(dut_id) = self.duts.selected().map(|dut| dut.id.clone()) {
            self.cache_a2a_token(&dut_id);
        }
    }

    pub(super) fn set_cached_a2a_token(&mut self, dut_id: &str, token: String) {
        self.a2a_tokens.insert(dut_id.to_owned(), Ok(Some(token)));
    }

    /// 独立保存每 DUT A2A 配置：只加载磁盘版本并改目标 DUT，避免连带保存未完成的表单。
    pub(super) fn edit_dut_a2a(
        &mut self,
        dut_id: &str,
        edit: impl FnOnce(&mut duthub_core::model::DutA2aConfig),
    ) -> Result<(), String> {
        if let Some(draft) = &mut self.duts.new_connection
            && draft.dut.id == dut_id
        {
            edit(&mut draft.dut.a2a);
            return Ok(());
        }
        let Some(current) = self
            .duts
            .file
            .duts
            .iter()
            .find(|dut| dut.id == dut_id)
            .map(|dut| dut.a2a.clone())
        else {
            return Err("DUT does not exist".into());
        };
        let mut config = current;
        edit(&mut config);
        let next = config.clone();
        self.heartbeat.commit_configs(&HashMap::new(), |_| {
            let Some(mut file) = store::load_duts(&self.data_dir).map_err(|e| e.to_string())?
            else {
                return Err("duts.json does not exist".into());
            };
            let Some(dut) = file.duts.iter_mut().find(|dut| dut.id == dut_id) else {
                return Err("DUT does not exist".into());
            };
            dut.a2a = next.clone();
            store::save_duts_atomic(&self.data_dir, &file).map_err(|e| e.to_string())
        })?;
        if let Some(dut) = self.duts.file.duts.iter_mut().find(|dut| dut.id == dut_id) {
            dut.a2a = config;
        }
        Ok(())
    }

    pub fn enable_dut_a2a(&mut self, dut_id: &str) {
        if self.duts.editing_new_connection {
            self.notify_toast(i18n::label("a2aSaveConnectionFirst"));
            return;
        }
        let Some(dut) = self
            .duts
            .file
            .duts
            .iter()
            .find(|dut| dut.id == dut_id)
            .cloned()
        else {
            return;
        };
        if dut.a2a.enabled {
            return;
        }
        let requested = self.input_value(In::DutA2aPort);
        let port = match requested.trim() {
            "" => self.allocate_port_from(8790, None),
            raw => if let Ok(port) = raw.parse::<u16>() { Some(port) } else {
                self.notify_toast(crate::i18n::label("mcpListenerInvalidPort"));
                return;
            },
        };
        let Some(port) = port else {
            self.notify_toast(i18n::label("noAvailableA2aPort"));
            return;
        };
        // W4a：0 与其他子系统保留端口在保存前拒绝（运行时只会给
        // PortConflict，提前拦截给出明确原因）。
        let reserved = crate::a2a_runtime::combined_reserved_ports(&self.settings, &self.duts);
        if let Err(reason) = duthub_desktop::agent_ui::validate_a2a_port(port, &reserved) {
            self.notify_toast(reason);
            return;
        }
        if self.port_configured(port, None) || !self.port_is_free(port) {
            self.notify_toast(crate::i18n::label("mcpListenerPortInUse"));
            return;
        }
        let token = duthub_core::agent_access::generate_token();
        if let Err(error) = duthub_core::set_a2a_token(dut_id, &token) {
            self.notify_toast(i18n::message(
                "a2aTokenKeyringWriteFailed",
                &[("error", &error.to_string())],
            ));
            return;
        }
        let requested_name = self.input_value(In::DutA2aName).trim().to_owned();
        let agent_name = if !requested_name.is_empty() {
            requested_name
        } else if !dut.a2a.agent_name.trim().is_empty() {
            dut.a2a.agent_name.clone()
        } else {
            dut.name.clone()
        };
        if let Err(error) = self.edit_dut_a2a(dut_id, |config| {
            config.enabled = true;
            config.port = port;
            config.agent_name = agent_name;
        }) {
            let _ = duthub_core::delete_a2a_token(dut_id);
            self.notify_toast(i18n::message(
                "a2aConfigSaveFailed",
                &[("error", &error.clone())],
            ));
            return;
        }
        self.set_cached_a2a_token(dut_id, token);
        self.drafts.remove(&In::DutA2aPort);
        self.drafts.remove(&In::DutA2aName);
        // W4a：即时拉起监听器（无需重启应用）；失败降级为 per-DUT 状态行。
        if let Some(a2a) = self.a2a.as_ref() {
            a2a.start_duts(&self.duts, self.mcp_state.clone());
        }
        self.notify_toast(i18n::message(
            "a2aEnabledToast",
            &[("port", &port.to_string())],
        ));
    }

    pub fn disable_dut_a2a(&mut self, dut_id: &str) {
        if let Err(error) = self.edit_dut_a2a(dut_id, |config| config.enabled = false) {
            self.notify_toast(i18n::message(
                "a2aConfigSaveFailed",
                &[("error", &error.clone())],
            ));
            return;
        }
        // W4a：运行中的监听器即时停止并释放端口。
        if let Some(a2a) = self.a2a.as_ref() {
            a2a.stop_dut(dut_id);
        }
        if let Err(error) = duthub_core::delete_a2a_token(dut_id) {
            self.notify_toast(i18n::message(
                "a2aTokenDeleteFailed",
                &[("error", &error.to_string())],
            ));
        }
        self.a2a_tokens.remove(dut_id);
        self.notify_toast(i18n::label("a2aDisabledToast"));
    }

    pub fn save_dut_a2a(&mut self, dut_id: &str) {
        let Some(dut) = self
            .duts
            .file
            .duts
            .iter()
            .find(|dut| dut.id == dut_id)
            .cloned()
        else {
            return;
        };
        let requested = self.input_value(In::DutA2aPort);
        let port = match requested.trim() {
            "" => self.allocate_port_from(8790, None),
            raw => if let Ok(port) = raw.parse::<u16>() { Some(port) } else {
                self.notify_toast(crate::i18n::label("mcpListenerInvalidPort"));
                return;
            },
        };
        let Some(port) = port else {
            self.notify_toast(i18n::label("noAvailableA2aPort"));
            return;
        };
        // W4a：0 / 保留端口拒绝；本 DUT 当前已启用的端口从保留集中豁免
        //（保存同一端口是合法操作）。
        let mut reserved = crate::a2a_runtime::combined_reserved_ports(&self.settings, &self.duts);
        if dut.a2a.enabled {
            reserved.retain(|reserved_port| *reserved_port != dut.a2a.port);
        }
        if let Err(reason) = duthub_desktop::agent_ui::validate_a2a_port(port, &reserved) {
            self.notify_toast(reason);
            return;
        }
        let agent_name = self.input_value(In::DutA2aName).trim().to_owned();
        let same_port = dut.a2a.enabled && dut.a2a.port == port;
        if !same_port && (self.port_configured(port, None) || !self.port_is_free(port)) {
            self.notify_toast(crate::i18n::label("mcpListenerPortInUse"));
            return;
        }
        if let Err(error) = self.edit_dut_a2a(dut_id, |config| {
            config.port = port;
            config.agent_name = agent_name;
        }) {
            self.notify_toast(i18n::message(
                "a2aConfigSaveFailed",
                &[("error", &error.clone())],
            ));
            return;
        }
        self.drafts.remove(&In::DutA2aPort);
        self.drafts.remove(&In::DutA2aName);
        if dut.a2a.enabled && !same_port {
            // 端口变更 = 重启生效（与 MCP 监听器同口径）：运行中的旧监听器
            // 立即停止，避免 UI 继续按旧端口宣称 Running。
            if let Some(a2a) = self.a2a.as_ref() {
                a2a.stop_dut(dut_id);
            }
            self.notify_toast(i18n::label("a2aSavedPortRestart"));
        } else {
            self.notify_toast(i18n::label("a2aSaved"));
        }
    }

    pub fn regenerate_dut_a2a_token(&mut self, dut_id: &str) {
        let enabled = self
            .duts
            .file
            .duts
            .iter()
            .find(|dut| dut.id == dut_id)
            .is_some_and(|dut| dut.a2a.enabled);
        if !enabled {
            return;
        }
        let token = duthub_core::agent_access::generate_token();
        if let Err(error) = duthub_core::set_a2a_token(dut_id, &token) {
            self.notify_toast(i18n::message(
                "a2aTokenKeyringWriteFailed",
                &[("error", &error.to_string())],
            ));
            return;
        }
        // W4a：热轮换——运行中监听器立即换用新 Token，旧 Token 下一个请求
        // 即 401；失败只提示，不谎报轮换成功（重启后新 Token 生效）。
        // 先借用热轮换拿结果，再 move 进缓存（坑 #14 同形状：move 放最后
        // 一次用）。
        let rotation = match self.a2a.as_ref() {
            Some(a2a) => a2a.set_token(dut_id, &token),
            None => Err(i18n::label("a2aRuntimeNotStarted").into()),
        };
        self.set_cached_a2a_token(dut_id, token);
        match rotation {
            Ok(()) => self.notify_toast(i18n::label("a2aTokenRegenerated")),
            Err(error) => {
                self.notify_toast(i18n::message("a2aTokenHotSwapFailed", &[("error", &error)]));
            }
        }
    }

    /// DUT 改名后同步专用端点凭据的展示名（不影响 Token/端口）。
    pub(super) fn refresh_dut_endpoint_names(&mut self) {
        let names: HashMap<String, String> = self
            .duts
            .file
            .duts
            .iter()
            .map(|dut| (dut.id.clone(), dut.name.clone()))
            .collect();
        let listener_duts: HashMap<String, String> = self
            .settings
            .agent_access
            .listeners
            .iter()
            .filter_map(|listener| Some((listener.id.clone(), listener.dut_id.clone()?)))
            .collect();
        let mut changed = false;
        for credential in &mut self.settings.agent_access.agents {
            let Some(listener_id) = credential.listener_id.as_deref() else {
                continue;
            };
            let Some(dut_id) = listener_duts.get(listener_id) else {
                continue;
            };
            let Some(name) = names.get(dut_id) else {
                continue;
            };
            let next = format!("{name} MCP");
            if credential.agent_name != next {
                credential.agent_name = next;
                changed = true;
            }
        }
        if changed {
            let _ = self.sync_agent_access();
        }
    }

    /// DUT 控制权面板「释放」：仅回收 GUI 自己持有的租约（Agent 租约走强制接管）。
    pub fn gui_release_lease(&mut self, dut_id: &str) {
        if let Some(lease) = self.device.lease_manager() {
            lease.release_for_holder(dut_id, &duthub_core::LeaseHolder::Gui);
            self.notify_toast(i18n::message("guiLeaseReleased", &[("dut", dut_id)]));
        }
    }

    /// DUT 控制权面板「强制接管」：撤销旧租约（generation+1）并授予 GUI；
    /// `Protected/待检查期拒绝（err.message()` 带原因）。
    pub fn gui_force_takeover(&mut self, dut_id: &str) {
        if let Some(lease) = self.device.lease_manager() {
            match lease.force_takeover(dut_id) {
                Ok(grant) => {
                    self.notify_toast(i18n::message(
                        "guiLeaseTakenOver",
                        &[
                            ("dut", dut_id),
                            ("gen", &grant.generation.to_string()),
                            (
                                "ttl",
                                &self.settings.agent_access.lease_ttl_secs.to_string(),
                            ),
                        ],
                    ));
                }
                Err(err) => self.notify_toast(i18n::message(
                    "guiLeaseTakeoverFailed",
                    &[("error", &err.message())],
                )),
            }
        }
    }

    /// DUT 控制权面板「安全恢复」：NeedsInspection 的人工确认入口。
    pub fn gui_resolve_inspection(&mut self, dut_id: &str) {
        if let Some(lease) = self.device.lease_manager() {
            lease.resolve_inspection(dut_id, true);
            self.notify_toast(i18n::message("dutMarkedSafe", &[("dut", dut_id)]));
        }
    }

    pub fn save_duts(&mut self) -> bool {
        let mut file = self.duts.file.clone();
        let mut drafts = self.heartbeat_drafts.clone();
        if let Some(connection) = &self.duts.new_connection {
            drafts.remove(&connection.dut.id);
        }
        let result = self.persist_bindings(&mut file, &drafts);
        match result {
            Ok(()) => {
                self.duts.file = file;
                for dut in &self.duts.file.duts {
                    self.boot_similarity_drafts.remove(&dut.id);
                }
                self.duts.sync_saved_media();
                self.media.reconcile_bindings(&self.duts);
                self.sync_serial_log_modes();
                self.duts.dirty = false;
                self.sync_control_duts();
                self.refresh_dut_endpoint_names();
                // Bound DUTs may have been added/removed by this save; the
                // detection gate is derived from the saved state.
                self.sync_garbled_runtime();
                self.heartbeat_drafts
                    .retain(|id, _| !drafts.contains_key(id));
                for session in &mut self.sessions {
                    if let Some(dut) = self
                        .duts
                        .file
                        .duts
                        .iter()
                        .find(|dut| dut.id == session.dut_id)
                    {
                        session.title = format!("{} · {}", dut.name, dut.serial.port);
                    }
                }
                self.notify_toast(i18n::t(self.lang, "saveBinding"));
                true
            }
            Err(err) => {
                self.notify_toast(format!("{} {err}", i18n::label("saveFailed")));
                false
            }
        }
    }

    fn sync_serial_log_modes(&self) {
        for dut in &self.duts.file.duts {
            let reference = dut
                .serial
                .split_logs
                .then(|| boot_reference_path(&self.artifact_root, dut))
                .flatten();
            let options = duthub_device::SerialLogOptions {
                split: dut.serial.split_logs,
                reference: reference.as_deref(),
            };
            if let Err(error) = self.device.configure_serial_log(&dut.id, options) {
                tracing::warn!(dut_id = %dut.id, %error, "Could not update the serial log mode");
            }
        }
    }

    pub(crate) fn can_learn_boot_reference(&self, dut_id: &str) -> bool {
        self.boot_learning_dut.is_none()
            && self.duts.saved_name(dut_id).is_some()
            && self.device.real_session_opened(dut_id)
            && self.sessions.iter().any(|view| {
                view.dut_id == dut_id
                    && view.source == "device"
                    && view.link == LinkState::Connected
            })
            && self.control.power_configured(dut_id)
            && self.boot_similarity(dut_id).is_ok()
    }

    pub(super) fn boot_similarity(&self, dut_id: &str) -> Result<u8, String> {
        use duthub_core::model::DEFAULT_BOOT_SIMILARITY;

        let value = self
            .duts
            .file
            .duts
            .iter()
            .find(|dut| dut.id == dut_id)
            .and_then(|dut| dut.serial.reference_similarity)
            .unwrap_or(DEFAULT_BOOT_SIMILARITY);
        let parsed = match self
            .boot_similarity_drafts
            .get(dut_id)
            .map(|draft| draft.trim())
        {
            None => Some(value),
            Some("") => Some(DEFAULT_BOOT_SIMILARITY),
            Some(draft) => draft.parse::<u8>().ok(),
        };
        parsed
            .filter(|percent| (1..=100).contains(percent))
            .ok_or_else(|| i18n::label("invalidBootSimilarity").into())
    }

    pub(crate) fn start_boot_learning(&mut self, dut_id: &str, cx: &mut Context<Self>) {
        if !self.can_learn_boot_reference(dut_id) {
            return;
        }
        let Some(name) = self.duts.saved_name(dut_id).map(str::to_owned) else {
            return;
        };
        let Ok(minimum_percent) = self.boot_similarity(dut_id) else {
            return;
        };
        let target = dut_id.to_owned();
        let device = self.device.clone();
        let control = self.control.clone();
        let root = self.artifact_root.clone();
        let (complete, result) = tokio::sync::oneshot::channel();
        let worker_target = target.clone();
        let worker = std::thread::Builder::new()
            .name("duthub-boot-learning".into())
            .spawn(move || {
                let result = duthub_device::boot_reference::learn(
                    &device,
                    &control,
                    &worker_target,
                    minimum_percent,
                )
                .and_then(|(reference, score)| {
                    crate::boot_learning::save(&root, &worker_target, &name, &reference)
                        .map_err(|error| error.to_string())
                        .and_then(|path| {
                            device
                                .apply_boot_reference(&worker_target, &path)
                                .map_err(|error| error.to_string())?;
                            Ok((path, score))
                        })
                });
                let _ = complete.send(result);
            });
        if let Err(error) = worker {
            self.notify_toast(error.to_string());
            return;
        }
        self.boot_learning_dut = Some(target.clone());
        cx.spawn(async move |view, cx| {
            let result = result.await.unwrap_or_else(|error| Err(error.to_string()));
            let _ = view.update(cx, |w, cx| {
                w.boot_learning_dut = None;
                match result {
                    Ok((path, similarity)) => {
                        if let Some(dut) = w.duts.connection_mut(&target) {
                            dut.serial.reference_log = None;
                            w.duts.mark_connection_dirty();
                        }
                        let score = format!("{:.2}%", similarity * 100.0);
                        w.notify_toast(i18n::message(
                            "bootLearningDone",
                            &[("score", &score), ("path", &path.to_string_lossy())],
                        ));
                    }
                    Err(error) => {
                        w.notify_toast(i18n::message("bootLearningFailed", &[("error", &error)]));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Per-DUT pop-up alert settings: the in-progress draft wins, otherwise
    /// the saved `alertsByDut` entry, otherwise the default (enabled, nothing
    /// suppressed). There is no global fallback any more — the former global
    /// switch only kept a suppression list.
    pub fn dut_alert_settings(&self, dut_id: &str) -> duthub_core::model::AlertSettings {
        if let Some(draft) = &self.duts.new_connection
            && draft.dut.id == dut_id
        {
            return draft.alerts.clone().unwrap_or_default();
        }
        self.duts
            .file
            .alerts_by_dut
            .get(dut_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn edit_dut_alerts(
        &mut self,
        dut_id: &str,
        edit: impl FnOnce(&mut duthub_core::model::AlertSettings),
    ) {
        if let Some(draft) = &mut self.duts.new_connection
            && draft.dut.id == dut_id
        {
            edit(
                draft
                    .alerts
                    .get_or_insert_with(duthub_core::model::AlertSettings::default),
            );
            return;
        }
        if !self.duts.file.duts.iter().any(|dut| dut.id == dut_id) {
            return;
        }
        let mut settings = self.dut_alert_settings(dut_id);
        edit(&mut settings);
        let result = self.heartbeat.commit_configs(&HashMap::new(), |_| {
            let mut file = store::load_duts(&self.data_dir)
                .map_err(|err| err.to_string())?
                .unwrap_or_else(|| self.duts.file.clone());
            if !file.duts.iter().any(|dut| dut.id == dut_id) {
                return Err(crate::i18n::label("saveDutBeforeAlerts").into());
            }
            file.alerts_by_dut
                .insert(dut_id.to_owned(), settings.clone());
            store::save_duts_atomic(&self.data_dir, &file).map_err(|err| err.to_string())
        });
        match result {
            Ok(()) => {
                self.duts
                    .file
                    .alerts_by_dut
                    .insert(dut_id.to_owned(), settings);
                self.alerts
                    .lock()
                    .unwrap()
                    .set_dut_settings(&self.duts.file.alerts_by_dut);
                // The detection switch only runs while this DUT's pop-up
                // alerts are on, so a switch change re-derives the runtime gate.
                self.sync_garbled_runtime();
            }
            Err(err) => self.notify_toast(format!("{} {err}", crate::i18n::label("saveFailed"))),
        }
    }

    /// Per-DUT garbled-byte detection switch (`duts.json` `garbledByDut`).
    /// Same precedence as [`Workbench::dut_alert_settings`]: the in-progress
    /// draft wins, then the saved entry, then the default (off).
    pub fn dut_garbled_detection(&self, dut_id: &str) -> duthub_core::model::GarbledDetection {
        if let Some(draft) = &self.duts.new_connection
            && draft.dut.id == dut_id
        {
            return draft.garbled.clone().unwrap_or_default();
        }
        self.duts
            .file
            .garbled_by_dut
            .get(dut_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Toggle the per-DUT detection switch. Drafts keep it in the draft; a
    /// saved DUT persists it immediately (the same shape as
    /// [`Workbench::edit_dut_alerts`]). Disabling removes the entry instead of
    /// writing `{"enabled": false}` so the "empty map is not serialized"
    /// invariant of `garbledByDut` holds.
    pub fn edit_dut_garbled_detection(&mut self, dut_id: &str, enabled: bool) {
        let is_draft = self
            .duts
            .new_connection
            .as_ref()
            .is_some_and(|draft| draft.dut.id == dut_id);
        if is_draft {
            if let Some(draft) = &mut self.duts.new_connection {
                draft
                    .garbled
                    .get_or_insert_with(duthub_core::model::GarbledDetection::default)
                    .enabled = enabled;
            }
            self.sync_garbled_runtime();
            return;
        }
        if !self.duts.file.duts.iter().any(|dut| dut.id == dut_id) {
            return;
        }
        let result = self.heartbeat.commit_configs(&HashMap::new(), |_| {
            let mut file = store::load_duts(&self.data_dir)
                .map_err(|err| err.to_string())?
                .unwrap_or_else(|| self.duts.file.clone());
            if !file.duts.iter().any(|dut| dut.id == dut_id) {
                return Err(crate::i18n::label("saveDutBeforeAlerts").into());
            }
            if enabled {
                file.garbled_by_dut.insert(
                    dut_id.to_owned(),
                    duthub_core::model::GarbledDetection { enabled: true },
                );
            } else {
                file.garbled_by_dut.remove(dut_id);
            }
            store::save_duts_atomic(&self.data_dir, &file).map_err(|err| err.to_string())
        });
        match result {
            Ok(()) => {
                if enabled {
                    self.duts.file.garbled_by_dut.insert(
                        dut_id.to_owned(),
                        duthub_core::model::GarbledDetection { enabled: true },
                    );
                } else {
                    self.duts.file.garbled_by_dut.remove(dut_id);
                }
                self.sync_garbled_runtime();
            }
            Err(err) => self.notify_toast(format!("{} {err}", crate::i18n::label("saveFailed"))),
        }
    }

    /// Push the effective per-DUT gate into the device layer: detection runs
    /// only when both switches are on (the form requires the alert switch, and
    /// this keeps a running session honest when it is turned off later).
    ///
    /// Turning the gate off also clears the last snapshot's garbled stats and
    /// the tab dot in one go: the device only republishes a snapshot when new
    /// output arrives, so a quiet session would otherwise keep a stale amber
    /// dot until the next byte.
    pub(super) fn sync_garbled_runtime(&mut self) {
        let gates: Vec<(String, bool)> = self
            .duts
            .file
            .duts
            .iter()
            .map(|dut| {
                let effective = self.dut_garbled_detection(&dut.id).enabled
                    && self.dut_alert_settings(&dut.id).enabled;
                (dut.id.clone(), effective)
            })
            .collect();
        for (dut_id, effective) in gates {
            self.device.set_garbled_detection_for(&dut_id, effective);
            if effective {
                continue;
            }
            for view in &mut self.sessions {
                if view.dut_id != dut_id {
                    continue;
                }
                view.last_garbled_level = GarbleLevel::Clean;
                if let Some(screen) = view.screen.as_mut() {
                    std::sync::Arc::make_mut(screen).garbled = Default::default();
                }
            }
        }
    }

    pub(super) fn persist_bindings(
        &self,
        file: &mut duthub_core::DutsFile,
        heartbeat_drafts: &HashMap<String, duthub_core::heartbeat::HeartbeatConfig>,
    ) -> Result<(), String> {
        for dut in file.duts.iter().filter(|dut| dut.serial.split_logs) {
            self.boot_similarity(&dut.id)?;
        }
        Self::validate_bindings(file)?;
        let mut renamed = Vec::new();
        let result = self.heartbeat.commit_configs(heartbeat_drafts, |configs| {
            Self::apply_heartbeat_configs(file, configs);
            for dut in &file.duts {
                if let Some(old) = self.duts.saved_name(&dut.id)
                    && duthub_core::dut_artifacts::rename_owned(
                        &self.artifact_root,
                        &dut.id,
                        old,
                        &dut.name,
                    )
                    .map_err(|error| {
                        i18n::message(
                            "dutDirRenameFailed",
                            &[("name", &dut.name), ("error", &error.to_string())],
                        )
                    })?
                {
                    renamed.push((dut.id.clone(), old.to_owned(), dut.name.clone()));
                }
            }
            store::save_duts_atomic(&self.data_dir, file).map_err(|err| err.to_string())
        });
        if let Err(mut error) = result {
            for (id, old, new) in renamed.into_iter().rev() {
                if let Err(rollback) =
                    duthub_core::dut_artifacts::rename_owned(&self.artifact_root, &id, &new, &old)
                {
                    error.push_str(&i18n::message(
                        "dutDirRollbackFailed",
                        &[("old", &old), ("error", &rollback.to_string())],
                    ));
                }
            }
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn validate_bindings(file: &mut duthub_core::DutsFile) -> Result<(), String> {
        file.migrate_usb_relays();
        crate::media_page::validate_bindings(&file.duts)?;
        let mut ports = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        let mut names = std::collections::HashSet::new();
        for dut in &file.duts {
            if !ids.insert(&dut.id) {
                return Err(i18n::message("duplicateDutId", &[("id", &dut.id)]));
            }
            if dut.name.trim().is_empty() {
                return Err(crate::i18n::label("enterDutName").into());
            }
            duthub_core::dut_artifacts::name_component(&dut.name)
                .map_err(|error| format!("{}: {error}", dut.name))?;
            if !names.insert(dut.name.to_lowercase()) {
                return Err(i18n::message("dutDirDuplicate", &[("name", &dut.name)]));
            }
            let port = if cfg!(windows) {
                dut.serial.port.to_ascii_lowercase()
            } else {
                dut.serial.port.clone()
            };
            if !port.is_empty() && !ports.insert((&dut.serial.host, port)) {
                return Err(i18n::message(
                    "serialPortBoundElsewhere",
                    &[("port", &dut.serial.port)],
                ));
            }
        }
        for board in &file.relay_boards {
            let node = board.node.trim();
            if node.is_empty() {
                continue;
            }
            if board.name.trim().is_empty() {
                return Err(crate::i18n::label("relayNameAndPort").into());
            }
            let port = if cfg!(windows) {
                node.to_ascii_lowercase()
            } else {
                node.to_owned()
            };
            if !ports.insert((&board.host, port)) {
                return Err(i18n::message("relayPortOccupied", &[("node", node)]));
            }
        }
        for dut in &file.duts {
            if let duthub_core::model::RelayConfig::UsbBoard { board_id, channels } = &dut.relay {
                let board = file
                    .relay_boards
                    .iter()
                    .find(|board| board.id == *board_id)
                    .ok_or_else(|| {
                        i18n::message("relayBoardMissingForDut", &[("name", &dut.name)])
                    })?;
                if board.node.trim().is_empty() {
                    return Err(crate::i18n::label("relayNameAndPort").into());
                }
                let limit = board.channel_count;
                if let Some(channel) = channels
                    .assigned()
                    .find(|&channel| channel == 0 || channel > limit)
                {
                    return Err(i18n::message(
                        "relayChannelOutOfRange",
                        &[("name", &dut.name), ("channel", &channel.to_string())],
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn apply_heartbeat_configs(
        file: &mut duthub_core::DutsFile,
        configs: &HashMap<String, duthub_core::heartbeat::HeartbeatConfig>,
    ) {
        for dut in &mut file.duts {
            if let Some(config) = configs.get(&dut.id) {
                dut.heartbeat = config.clone();
            }
        }
    }

    pub(crate) fn heartbeat_config(&self, dut_id: &str) -> duthub_core::heartbeat::HeartbeatConfig {
        self.heartbeat_drafts
            .get(dut_id)
            .cloned()
            .unwrap_or_else(|| self.heartbeat.config(dut_id))
    }

    pub(crate) fn toggle_heartbeat(&mut self) {
        if let Some(dut) = self.duts.selected() {
            let mut config = self.heartbeat_config(&dut.id);
            config.enabled = !config.enabled;
            config.paused_until_unix_ms = None;
            self.heartbeat_drafts.insert(dut.id.clone(), config);
            self.duts.mark_connection_dirty();
        }
    }

    pub fn import_config(&mut self, base: &'static str, cx: &mut Context<Self>) {
        let path = self.data_dir.join(format!("{base}.json"));
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                if base == "settings" {
                    match serde_json::from_str::<duthub_core::SettingsFile>(&raw) {
                        Ok(s) => {
                            self.lang = Lang::from_setting(&s.appearance.language);
                            self.lang.activate();
                            self.theme_mode = theme::ThemeMode::from_setting(&s.appearance.theme);
                            theme::apply_brand(cx, self.theme_mode);
                            self.settings = s;
                            // The center mirrors the imported suppression list,
                            // otherwise the settings page would show the imported
                            // chips while the runtime keeps the pre-import set
                            // (both directions: added and removed entries).
                            self.alerts
                                .lock()
                                .unwrap()
                                .set_suppressed_sources(&self.settings.alerts.suppressed_sources);
                            self.drafts.remove(&In::DutStorageGib);
                            self.artifact_quota.set_limit(
                                u64::from(self.settings.dut_storage_gib())
                                    * duthub_core::dut_quota::GIB,
                            );
                            self.provider_models = crate::provider_models::Catalog::default();
                            self.terminal_zoom_size = None;
                            self.notify_toast(i18n::label("settingsImported"));
                        }
                        Err(err) => self.notify_toast(i18n::message(
                            "importFailed",
                            &[("error", &err.to_string())],
                        )),
                    }
                } else {
                    match serde_json::from_str::<duthub_core::DutsFile>(&raw) {
                        Ok(d) => {
                            self.duts = DutsState::from_file(d);
                            self.media.reconcile_bindings(&self.duts);
                            self.sync_serial_log_modes();
                            self.relay_board_selected = 0;
                            self.relay_board_name = None;
                            self.alerts
                                .lock()
                                .unwrap()
                                .set_dut_settings(&self.duts.file.alerts_by_dut);
                            // Same mirror rule as the settings import above:
                            // `garbledByDut` is a shared runtime gate now, so a
                            // wholesale DUT replacement must re-derive it or the
                            // imported switches stay invisible to the device.
                            self.sync_garbled_runtime();
                            self.notify_toast(i18n::label("dutsImported"));
                        }
                        Err(err) => self.notify_toast(i18n::message(
                            "importFailed",
                            &[("error", &err.to_string())],
                        )),
                    }
                }
            }
            Err(err) => self.notify_toast(i18n::message(
                "importFailedPath",
                &[("path", &format!("{path:?}")), ("error", &err.to_string())],
            )),
        }
    }

    pub(crate) fn config_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// Write the current typed configuration to the work directory.
    pub fn export_config(&mut self, base: &'static str) {
        let exports = self.work_dir.join("exports");
        let result = match base {
            "settings" => store::export_settings_atomic(&exports, &self.settings),
            "duts" => store::save_duts_atomic(&exports, &self.duts.file),
            _ => return,
        };
        match result {
            Ok(()) => self.notify_toast(i18n::message(
                "exportedTo",
                &[(
                    "path",
                    &exports.join(format!("{base}.json")).display().to_string(),
                )],
            )),
            Err(err) => self.notify_toast(i18n::message(
                "exportFailed",
                &[("error", &err.to_string())],
            )),
        }
    }

    pub fn export_a2a_config(&mut self) {
        let a2a = self.settings.a2a.clone().unwrap_or_default();
        let exports = self.work_dir.join("exports");
        let _ = std::fs::create_dir_all(&exports);
        let path = exports.join("a2a_server.json");
        // W4a 秘密扫描 fail closed：peers 序列化后过 export_contains_secret，
        // 命中即拒绝写盘（如 card URL 里走私 ?token=…）。
        let peers = match duthub_desktop::agent_ui::a2a_export_payload(&a2a.peers) {
            Ok(peers) => peers,
            Err(reason) => {
                self.notify_toast(reason);
                return;
            }
        };
        let payload = serde_json::json!({
            "agentName": a2a.agent_name,
            "port": a2a.port,
            "peers": peers,
        });
        if duthub_core::keys::export_contains_secret(&payload) {
            self.notify_toast(i18n::label("exportBlockedSecret"));
            return;
        }
        match serde_json::to_string_pretty(&payload)
            .map_err(|e| e.to_string())
            .and_then(|s| std::fs::write(&path, s).map_err(|e| e.to_string()))
        {
            Ok(()) => self.notify_toast(i18n::message(
                "exportedTo",
                &[("path", &path.display().to_string())],
            )),
            Err(err) => self.notify_toast(i18n::message("exportFailed", &[("error", &err)])),
        }
    }

    /// 导出 MCP 使用技能文档（需求⑥）：`<工作目录>/skill/duthub/SKILL.md`。
    /// 与 MCP 工具 `skill_export` `同一生成器（duthub_mcp::skill`）。
    pub fn export_skill_md(&mut self) {
        match duthub_mcp::skill::export_skill_for_endpoint(
            &self.work_dir,
            self.mcp_bound_endpoint.get().copied(),
        ) {
            Ok(path) => self.notify_toast(i18n::message(
                "exportedTo",
                &[("path", &path.display().to_string())],
            )),
            Err(err) => self.notify_toast(i18n::message(
                "exportSkillFailed",
                &[("error", &err.to_string())],
            )),
        }
    }
}
