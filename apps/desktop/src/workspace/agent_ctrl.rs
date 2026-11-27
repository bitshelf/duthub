use super::{
    AgentHostEvents, AgentPromptEditor, AgentRegistryEdit, AlertSource, AppContext, Arc,
    BindingSource, Context, Entity, In, InputEvent, InputState, Mutex, PendingGooseSave,
    PendingRegistrySave, Window, Workbench, agent_host, i18n, provider_setting, store,
};

impl Workbench {
    // ------------------------------------------------------------------
    // Agent host lifecycle and per-DUT prompts and transcripts
    // ------------------------------------------------------------------

    /// Whether the Agent host is available. Self-test and missing lease or audit dependencies disable it.
    pub(crate) fn agent_runtime_available(&self) -> bool {
        !self.agent_self_test && self.agent_access.is_some()
    }

    /// Whether self-test is active, for the disabled state in the Agent panel.
    pub(crate) fn agent_self_test_mode(&self) -> bool {
        self.agent_self_test
    }

    /// The selected Agent ID for the DUT. Default to goose and persist the choice in the sidecar.
    pub(crate) fn agent_id_for_dut(&self, dut_id: &str) -> String {
        self.agent_bindings.agent_id_for(dut_id)
    }

    /// Use the launched Agent's name for transcript labels when available.
    /// Before launch, use the selected Agent's registry name.
    pub(crate) fn transcript_agent_tag(&self, dut_id: &str) -> String {
        let launch = self
            .agent_host
            .as_ref()
            .and_then(|host| host.launch(dut_id));
        let agent_id = self.agent_id_for_dut(dut_id);
        let selected_name = self.agents.agent(&agent_id).map(|agent| agent.name.clone());
        duthub_desktop::agent_ui::transcript_agent_tag(launch.as_ref(), selected_name.as_deref())
    }

    /// List the launchable ACP Agents for the selector.
    pub(crate) fn acp_agent_choices(&self) -> Vec<(String, String)> {
        self.agents
            .acp_agents()
            .map(|agent| (agent.id.clone(), agent.name.clone()))
            .collect()
    }

    pub(super) fn agent_definition(
        &self,
        agent_id: &str,
    ) -> Option<duthub_core::agent_config::AgentDefinition> {
        self.agents.agent(agent_id).cloned()
    }

    /// Save the DUT's new Agent selection before replacing the in-memory value.
    pub(crate) fn set_dut_agent_enabled(&mut self, dut_id: &str, enabled: bool) {
        if enabled && self.duts.saved_name(dut_id).is_none() {
            return;
        }
        let mut bindings = self.agent_bindings.clone();
        bindings.set_enabled(dut_id, enabled);
        match duthub_core::agent_config::save_agent_bindings_atomic(&self.work_dir, &bindings) {
            Ok(()) => self.agent_bindings = bindings,
            Err(error) => {
                self.notify_toast(format!("{} {error}", i18n::label("saveFailed")));
                return;
            }
        }
        if !enabled {
            self.stop_agent_session(dut_id);
        }
    }

    pub(crate) fn set_dut_agent(&mut self, dut_id: &str, agent_id: &str) {
        if self.agents.agent(agent_id).is_none() {
            self.notify_toast(crate::i18n::label("agentRegistryMissing"));
            return;
        }
        let mut bindings = self.agent_bindings.clone();
        bindings.set_agent_id(dut_id, agent_id);
        match duthub_core::agent_config::save_agent_bindings_atomic(&self.work_dir, &bindings) {
            Ok(()) => {
                self.agent_bindings = bindings;
                self.notify_toast(crate::i18n::label("agentSelectionSaved"));
            }
            Err(error) => self.notify_toast(format!(
                "{} {error}",
                crate::i18n::label("agentSelectionSaveFailed")
            )),
        }
    }

    /// Read the external goose path and hash from the registry.
    pub(crate) fn goose_external(&self) -> Option<(String, String)> {
        let goose = self.agents.goose()?;
        let path = goose.program_path()?.to_owned();
        let sha = goose.expected_sha256.clone().unwrap_or_default();
        Some((path, sha))
    }

    pub(crate) fn begin_agent_registry_new(&mut self) {
        self.agent_registry_edit = Some(AgentRegistryEdit { id: None });
        self.drafts.insert(In::AgentRegName, String::new());
        self.drafts.insert(In::AgentRegProgram, String::new());
        self.drafts.insert(In::AgentRegArgs, String::new());
        self.drafts.insert(In::AgentRegEnvKeys, String::new());
        self.drafts.insert(In::AgentRegEnvValue, String::new());
        self.agent_env_selected = None;
        self.agent_env_value_state = None;
        self.open_dd = None;
    }

    pub(crate) fn begin_agent_registry_edit(&mut self, agent_id: &str) {
        let Some(agent) = self.agent_definition(agent_id) else {
            self.notify_toast(crate::i18n::label("agentRegistryMissing"));
            return;
        };
        self.agent_registry_edit = Some(AgentRegistryEdit {
            id: Some(agent.id.clone()),
        });
        self.drafts.insert(In::AgentRegName, agent.name);
        self.drafts
            .insert(In::AgentRegProgram, agent.program.unwrap_or_default());
        // A JSON array preserves argument spaces and quotes through editing.
        self.drafts.insert(
            In::AgentRegArgs,
            duthub_desktop::agent_ui::args_to_json(&agent.args),
        );
        self.drafts
            .insert(In::AgentRegEnvKeys, agent.env_keys.join(", "));
        self.drafts.insert(In::AgentRegEnvValue, String::new());
        self.agent_env_selected = None;
        self.agent_env_value_state = None;
        self.open_dd = None;
    }

    pub(crate) fn cancel_agent_registry_edit(&mut self) {
        self.agent_registry_edit = None;
        self.reset_agent_env_editor();
        for id in [
            In::AgentRegName,
            In::AgentRegProgram,
            In::AgentRegArgs,
            In::AgentRegEnvKeys,
            In::AgentRegEnvValue,
        ] {
            self.drafts.remove(&id);
        }
    }

    pub(super) fn clear_agent_registry_drafts(&mut self) {
        self.agent_registry_edit = None;
        self.reset_agent_env_editor();
        for id in [
            In::AgentRegName,
            In::AgentRegProgram,
            In::AgentRegArgs,
            In::AgentRegEnvKeys,
            In::AgentRegEnvValue,
        ] {
            self.drafts.remove(&id);
        }
    }

    /// Reset the W4b env value editor state (selection + keystore read state).
    pub(super) fn reset_agent_env_editor(&mut self) {
        self.agent_env_selected = None;
        self.agent_env_value_state = None;
    }

    /// Save a new or edited ACP Agent registry entry.
    ///
    /// Validate the arguments, allowed environment keys, and absolute program path.
    /// An invalid draft is never persisted.
    /// Hash an external executable and probe its version on the background executor.
    /// Commit the pending draft only when its current generation's probe succeeds.
    /// The default goose entry needs no executable probe. Persist environment key names
    /// and keep the values in the system credential store.
    pub(crate) fn save_agent_registry(&mut self, cx: &mut Context<Self>) {
        if self.pending_registry_save.is_some() {
            // A probe is already in flight; ignore re-entrant saves until it
            // completes (the button is disabled meanwhile).
            return;
        }
        let name = self.input_value(In::AgentRegName).trim().to_owned();
        if name.is_empty() {
            self.notify_toast(crate::i18n::label("agentRegistryNameRequired"));
            return;
        }
        let editing_id = self
            .agent_registry_edit
            .as_ref()
            .and_then(|edit| edit.id.clone());
        let agent_id = editing_id
            .clone()
            .unwrap_or_else(|| self.unique_agent_id(&name));
        if duthub_core::agent_config::validate_agent_id(&agent_id).is_err() {
            self.notify_toast(crate::i18n::label("agentRegistryInvalid"));
            return;
        }

        let existing = self.agent_definition(&agent_id);
        let kind = existing
            .as_ref()
            .map_or(duthub_core::agent_config::AgentKind::Acp, |agent| {
                agent.kind
            });
        let program = self.input_value(In::AgentRegProgram).trim().to_owned();
        if program.is_empty() && agent_id != duthub_core::agent_config::DEFAULT_AGENT_ID {
            self.notify_toast(crate::i18n::label("agentRegistryProgramRequired"));
            return;
        }
        // W4b save gate: args JSON / env key rules / absolute program path all
        // fail closed before any write — invalid drafts never reach agents.json.
        let draft = match duthub_desktop::agent_ui::validate_agent_editor_draft(
            &program,
            &self.input_value(In::AgentRegArgs),
            &self.input_value(In::AgentRegEnvKeys),
        ) {
            Ok(draft) => draft,
            Err(error) => {
                self.notify_toast(error);
                return;
            }
        };

        let mut definition =
            existing.unwrap_or_else(|| duthub_core::agent_config::AgentDefinition {
                id: agent_id.clone(),
                name: String::new(),
                kind: duthub_core::agent_config::AgentKind::Acp,
                program: None,
                args: Vec::new(),
                env_keys: Vec::new(),
                cli_profile: None,
                enabled: true,
                auth_mode: "inherit".into(),
                trust_level: "mcp_only".into(),
                expected_sha256: None,
            });
        definition.name = name;
        definition.kind = kind;
        definition.program.clone_from(&draft.program);
        // The hash is filled in by the background probe before the save lands.
        definition.expected_sha256 = None;
        definition.args = draft.args;
        definition.env_keys = draft.env_keys;

        let goose = agent_id == duthub_core::agent_config::DEFAULT_AGENT_ID;
        if definition.program.is_none() {
            // Embedded program: nothing to probe; persist right away.
            self.commit_agent_registry(definition, None);
            cx.notify();
            return;
        }
        let Some(program) = definition.program.clone() else {
            return;
        };
        self.goose_probe_generation += 1;
        let generation = self.goose_probe_generation;
        self.pending_registry_save = Some(PendingRegistrySave {
            definition,
            goose,
            generation,
        });
        let probe = cx.background_executor().spawn(async move {
            agent_host::registry::probe_program(std::path::Path::new(&program), goose)
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = probe.await;
            let _ = this.update(cx, |this, cx| {
                this.finish_agent_registry_save(generation, result);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Background-probe completion for a registry save. Stale results (older
    /// generation than the pending save) are dropped without touching state.
    pub(super) fn finish_agent_registry_save(
        &mut self,
        generation: u64,
        result: Result<agent_host::registry::ProgramProbe, String>,
    ) {
        let Some(pending) = self.pending_registry_save.as_ref() else {
            return;
        };
        if pending.generation != generation {
            return;
        }
        let PendingRegistrySave {
            definition,
            goose,
            generation: _,
        } = pending.clone();
        self.pending_registry_save = None;
        match result {
            Ok(probe) => {
                let goose_probe = goose.then(|| probe.clone());
                self.commit_agent_registry(definition, Some(probe.sha256));
                if let Some(probe) = goose_probe {
                    self.goose_probe = Some(probe);
                }
            }
            Err(error) => {
                self.notify_toast(format!(
                    "{} {error}",
                    crate::i18n::label("agentRegistryProbeFailed")
                ));
            }
        }
    }

    /// Persist one validated registry definition (hash already resolved) and
    /// refresh the editor state; no probing happens here.
    pub(super) fn commit_agent_registry(
        &mut self,
        mut definition: duthub_core::agent_config::AgentDefinition,
        expected_sha256: Option<String>,
    ) {
        definition.expected_sha256 = expected_sha256;
        let agent_id = definition.id.clone();
        let mut file = self.agents.clone();
        match file.agent_mut(&agent_id) {
            Some(existing) => *existing = definition,
            None => file.agents.push(definition),
        }
        match duthub_core::agent_config::save_agents_atomic(&self.work_dir, &file) {
            Ok(()) => {
                self.agents = file;
                self.agents_error = None;
                self.clear_agent_registry_drafts();
                self.notify_toast(crate::i18n::label("agentRegistrySaved"));
            }
            Err(error) => {
                self.notify_toast(format!("{} {error}", crate::i18n::label("saveFailed")));
            }
        }
    }

    pub(super) fn unique_agent_id(&self, name: &str) -> String {
        let base: String = name
            .trim()
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let base = base.trim_matches('-');
        let base = if base.is_empty() { "agent" } else { base };
        let mut candidate = base.to_owned();
        let mut suffix = 2;
        while self.agents.agent(&candidate).is_some() {
            candidate = format!("{base}-{suffix}");
            suffix += 1;
        }
        candidate
    }

    // ------------------------------------------------------------------
    // Masked environment value editor backed by the system credential store.
    // ------------------------------------------------------------------

    /// Clear the masked draft and look up the selected key in the background.
    /// Require a saved Agent ID so the credential has an owner.
    /// Set the pending state and apply the result only for the current lookup generation.
    pub(crate) fn select_agent_env_key(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(agent_id) = self
            .agent_registry_edit
            .as_ref()
            .and_then(|edit| edit.id.clone())
        else {
            self.notify_toast(crate::i18n::label("agentEnvNeedsSavedAgent"));
            return;
        };
        let key = key.to_owned();
        self.agent_env_selected = Some(key.clone());
        self.drafts.remove(&In::AgentRegEnvValue);
        self.agent_env_read_generation += 1;
        let generation = self.agent_env_read_generation;
        self.agent_env_value_state = Some(duthub_desktop::agent_ui::EnvValueState::Pending);
        let read_key = key.clone();
        let read = cx.background_executor().spawn(async move {
            duthub_core::agent_config::get_agent_env(&agent_id, &read_key)
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, _cx| {
                this.finish_agent_env_read(generation, &key, &result);
            });
        })
        .detach();
        cx.notify();
    }

    /// Background keystore-read completion (W4c). Results from an older
    /// generation (the selection moved on) or for a different key are dropped.
    pub(super) fn finish_agent_env_read(
        &mut self,
        generation: u64,
        key: &str,
        read: &Result<Option<String>, String>,
    ) {
        if self.agent_env_read_generation != generation
            || self.agent_env_selected.as_deref() != Some(key)
        {
            return;
        }
        self.agent_env_value_state = Some(duthub_desktop::agent_ui::env_value_source(read));
    }

    /// Save the masked draft to the credential store, replacing any prior value for a key still in the current draft.
    pub(crate) fn save_agent_env_value(&mut self, cx: &mut Context<Self>) {
        let Some(agent_id) = self
            .agent_registry_edit
            .as_ref()
            .and_then(|edit| edit.id.clone())
        else {
            self.notify_toast(crate::i18n::label("agentEnvNeedsSavedAgent"));
            return;
        };
        let Some(key) = self.agent_env_selected.clone() else {
            self.notify_toast(crate::i18n::label("agentEnvSelectKeyFirst"));
            return;
        };
        let keys = match duthub_desktop::agent_ui::parse_env_keys_draft(
            &self.input_value(In::AgentRegEnvKeys),
        ) {
            Ok(keys) => keys,
            Err(error) => {
                self.notify_toast(error);
                return;
            }
        };
        if !keys.iter().any(|existing| existing == &key) {
            // The key was removed from the draft after selection: stale target.
            self.reset_agent_env_editor();
            self.notify_toast(crate::i18n::label("agentEnvSelectKeyFirst"));
            return;
        }
        let value = self
            .drafts
            .get(&In::AgentRegEnvValue)
            .cloned()
            .unwrap_or_default();
        if value.trim().is_empty() {
            self.notify_toast(crate::i18n::label("agentEnvValueRequired"));
            return;
        }
        match duthub_core::agent_config::set_agent_env(&agent_id, &key, &value) {
            Ok(()) => {
                // Drop the secret draft immediately; the state refresh re-reads
                // the keystore so the status line reflects what was stored.
                self.drafts.remove(&In::AgentRegEnvValue);
                self.refresh_agent_env_value_state(cx);
                self.notify_toast(crate::i18n::message("agentEnvValueSaved", &[("key", &key)]));
            }
            Err(error) => self.notify_toast(format!(
                "{} {error}",
                crate::i18n::label("agentEnvWriteFailed")
            )),
        }
    }

    /// Delete the credential and remove its key from the draft.
    /// This prevents a registry entry from referencing a missing credential at launch.
    pub(crate) fn delete_agent_env_value(&mut self) {
        let Some(agent_id) = self
            .agent_registry_edit
            .as_ref()
            .and_then(|edit| edit.id.clone())
        else {
            self.notify_toast(crate::i18n::label("agentEnvNeedsSavedAgent"));
            return;
        };
        let Some(key) = self.agent_env_selected.clone() else {
            self.notify_toast(crate::i18n::label("agentEnvSelectKeyFirst"));
            return;
        };
        match duthub_core::agent_config::delete_agent_env(&agent_id, &key) {
            Ok(()) => {
                if let Ok(keys) = duthub_desktop::agent_ui::parse_env_keys_draft(
                    &self.input_value(In::AgentRegEnvKeys),
                ) {
                    let kept: Vec<String> = keys
                        .into_iter()
                        .filter(|existing| existing != &key)
                        .collect();
                    self.drafts.insert(In::AgentRegEnvKeys, kept.join(", "));
                }
                self.reset_agent_env_editor();
                self.drafts.remove(&In::AgentRegEnvValue);
                self.notify_toast(crate::i18n::message(
                    "agentEnvValueDeleted",
                    &[("key", &key)],
                ));
            }
            Err(error) => self.notify_toast(format!(
                "{} {error}",
                crate::i18n::label("agentEnvDeleteFailed")
            )),
        }
    }

    /// Refresh the selected key's credential state after writing or deleting it.
    pub(super) fn refresh_agent_env_value_state(&mut self, cx: &mut Context<Self>) {
        let Some((agent_id, key)) = self.agent_env_selected.clone().and_then(|key| {
            self.agent_registry_edit
                .as_ref()
                .and_then(|edit| edit.id.clone())
                .map(|agent_id| (agent_id, key))
        }) else {
            return;
        };
        self.agent_env_read_generation += 1;
        let generation = self.agent_env_read_generation;
        self.agent_env_value_state = Some(duthub_desktop::agent_ui::EnvValueState::Pending);
        let read_key = key.clone();
        let read = cx.background_executor().spawn(async move {
            duthub_core::agent_config::get_agent_env(&agent_id, &read_key)
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, _cx| {
                this.finish_agent_env_read(generation, &key, &result);
            });
        })
        .detach();
    }

    /// Delete a registry entry. The default goose is retained and becomes the fallback for affected DUTs.
    ///
    /// Persist the registry deletion before removing credentials from the system store.
    /// Queue any failed credential deletions for retry on startup.
    /// Report incomplete cleanup in an alert.
    pub(crate) fn delete_agent_registry(&mut self, agent_id: &str) {
        if agent_id == duthub_core::agent_config::DEFAULT_AGENT_ID {
            self.notify_toast(crate::i18n::label("agentRegistryBuiltin"));
            return;
        }
        let Some(removed) = self.agents.agent(agent_id).cloned() else {
            self.notify_toast(crate::i18n::label("agentRegistryMissing"));
            return;
        };
        let mut file = self.agents.clone();
        let before = file.agents.len();
        file.agents.retain(|agent| agent.id != agent_id);
        if file.agents.len() == before {
            self.notify_toast(crate::i18n::label("agentRegistryMissing"));
            return;
        }
        if let Err(error) = duthub_core::agent_config::save_agents_atomic(&self.work_dir, &file) {
            self.notify_toast(format!("{} {error}", crate::i18n::label("saveFailed")));
            return;
        }
        self.agents = file;
        self.agents_error = None;
        // Registry record is gone; keyring values are best-effort cleanup.
        let pending_keys = match duthub_desktop::agent_ui::delete_agent_env_entries(&removed) {
            Ok(pending) => pending,
            Err(error) => {
                // Nothing was attempted: journal every key for the next startup.
                self.report_agent_env_cleanup(agent_id, &removed.env_keys, &error);
                removed.env_keys.clone()
            }
        };
        if !pending_keys.is_empty() {
            let reason = i18n::message(
                "keyringDeletePendingSummary",
                &[("count", &pending_keys.len().to_string())],
            );
            self.report_agent_env_cleanup(agent_id, &pending_keys, &reason);
            self.notify_toast(format!(
                "{} {}",
                crate::i18n::label("agentDeleteEnvCleanupFailed"),
                pending_keys.join(", ")
            ));
        }
        let mut bindings = self.agent_bindings.clone();
        let affected: Vec<String> = bindings
            .bindings
            .iter()
            .filter(|(_, selected)| selected.as_str() == agent_id)
            .map(|(dut_id, _)| dut_id.clone())
            .collect();
        for dut_id in affected {
            bindings.set_agent_id(&dut_id, duthub_core::agent_config::DEFAULT_AGENT_ID);
        }
        if let Err(error) =
            duthub_core::agent_config::save_agent_bindings_atomic(&self.work_dir, &bindings)
        {
            self.notify_toast(format!("{} {error}", crate::i18n::label("saveFailed")));
        } else {
            self.agent_bindings = bindings;
        }
        if self
            .agent_registry_edit
            .as_ref()
            .and_then(|edit| edit.id.as_deref())
            == Some(agent_id)
        {
            self.clear_agent_registry_drafts();
        }
        self.notify_toast(crate::i18n::label("agentRegistryDeleted"));
    }

    /// Journal + toast + shared `AlertCenter` entry for agent env cleanup that
    /// did not finish (W4b). The journal is retried at next startup.
    pub(super) fn report_agent_env_cleanup(
        &mut self,
        agent_id: &str,
        keys: &[String],
        reason: &str,
    ) {
        if let Err(error) = duthub_desktop::agent_ui::append_agent_env_pending_cleanup(
            &self.work_dir,
            agent_id,
            keys,
            reason,
        ) {
            self.notify_toast(i18n::message(
                "recordPendingCleanupFailed",
                &[("error", &error.clone())],
            ));
        }
        let (title, detail) = crate::alerts::control_action_alert_texts(
            agent_id,
            i18n::label("pendingCleanupAgentEnvTitle"),
            reason,
        );
        let _ = self.alerts.lock().map(|mut center| {
            center.report_for(agent_id, AlertSource::Agent, title, detail);
        });
    }

    /// Enable or disable an Agent entry. A disabled entry cannot be launched.
    /// Keep the default goose and at least one enabled entry.
    pub(crate) fn toggle_agent_registry(&mut self, agent_id: &str) {
        if let Err(error) = duthub_desktop::agent_ui::validate_agent_toggle(&self.agents, agent_id)
        {
            self.notify_toast(error);
            return;
        }
        let mut file = self.agents.clone();
        let Some(agent) = file.agent_mut(agent_id) else {
            self.notify_toast(crate::i18n::label("agentRegistryMissing"));
            return;
        };
        agent.enabled = !agent.enabled;
        match duthub_core::agent_config::save_agents_atomic(&self.work_dir, &file) {
            Ok(()) => {
                self.agents = file;
                self.agents_error = None;
            }
            Err(error) => {
                self.notify_toast(format!("{} {error}", crate::i18n::label("saveFailed")));
            }
        }
    }

    /// Get or lazily create the Agent host. Self-test leaves the host absent.
    pub(super) fn ensure_agent_host(&mut self) -> Result<&mut agent_host::HostManager, String> {
        if self.agent_self_test {
            return Err(crate::i18n::label("agentSessionSelfTest").to_owned());
        }
        if self.agent_host.is_none() {
            let access = self
                .agent_access
                .clone()
                .ok_or_else(|| crate::i18n::label("agentSessionUnavailable").to_owned())?;
            let host = agent_host::HostManager::new(
                self.work_dir.clone(),
                self.mcp_state.clone(),
                access,
                Some(Arc::new(AgentHostEvents {
                    alerts: self.alerts.clone(),
                    toasts: self.agent_host_toasts.clone(),
                    failed: Mutex::new(std::collections::HashSet::new()),
                })),
            );
            self.agent_host = Some(host);
        }
        Ok(self.agent_host.as_mut().expect("agent host initialized"))
    }

    /// Start a DUT Agent session. Initialization runs on the host worker.
    ///
    /// Use the host's common entry point for ACP and per-turn CLI profiles.
    /// Resolve the model binding from settings only for goose ACP.
    /// CLI and third-party ACP programs supply their own credentials.
    /// A binding resolution failure is reported as `HostError::Invalid`.
    pub fn start_agent_session(&mut self, dut_id: &str, cx: &mut Context<Self>) {
        if !self.agent_bindings.is_enabled(dut_id) {
            return;
        }
        let agent_id = self.agent_id_for_dut(dut_id);
        let Some(agent) = self.agent_definition(&agent_id) else {
            self.notify_toast(crate::i18n::label("agentRegistryMissing"));
            cx.notify();
            return;
        };
        // Only the goose ACP program consumes the duthub model binding.
        // CLI profiles and third-party ACP agents authenticate themselves;
        // start_entry routes CLI agents to the per-turn worker before any
        // binding is considered.
        let settings = self.settings.clone();
        let binding = if agent.is_goose() && agent.kind == duthub_core::agent_config::AgentKind::Acp
        {
            BindingSource::FromSettings(&settings)
        } else {
            // Non-goose product path never forwards a model binding.
            BindingSource::None
        };
        let started = match self.ensure_agent_host() {
            Ok(host) => host
                .start_entry(dut_id, binding, agent)
                .map_err(|error| error.to_string()),
            Err(error) => Err(error),
        };
        match started {
            Ok(()) => {
                self.agent_transcript_seen.remove(dut_id);
                self.notify_toast(crate::i18n::label("agentSessionStartingToast"));
            }
            Err(error) => self.notify_toast(format!(
                "{} {error}",
                crate::i18n::label("agentStartFailed")
            )),
        }
        cx.notify();
    }

    /// Request a stop without blocking. The host worker waits for and terminates the process tree.
    pub fn stop_agent_session(&mut self, dut_id: &str) {
        // Revoke the token and close stdin immediately. Waiting and tree termination stay on the worker.
        // The caller handles the UI notification. Cleanup can also reach this entry without a UI context.
        if let Some(host) = self.agent_host.as_mut() {
            host.stop(dut_id);
        }
    }

    /// Cancel the current turn while keeping the process available for the next one.
    pub fn cancel_agent_turn(&mut self, dut_id: &str, cx: &mut Context<Self>) {
        if let Some(host) = self.agent_host.as_mut() {
            host.cancel(dut_id);
        }
        cx.notify();
    }

    /// Submit the current DUT's draft when the session is ready, from Enter or the Send button.
    ///
    /// After enqueueing, mark the matching draft for clearing in the next render.
    /// On failure, keep the draft and record the attempted text in the transcript.
    pub fn send_agent_prompt(&mut self, dut_id: &str, cx: &mut Context<Self>) {
        if !self.agent_bindings.is_enabled(dut_id) {
            return;
        }
        let Some(editor) = self.agent_prompts.get(dut_id) else {
            return;
        };
        let text = editor.state.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        if self.agent_host.is_none() {
            self.notify_toast(crate::i18n::label("agentSessionUnavailable"));
            return;
        }
        let host = self.agent_host.as_mut().expect("agent host checked");
        let outcome = duthub_desktop::agent_ui::prompt_send_outcome(host.prompt(dut_id, &text));
        match outcome {
            duthub_desktop::agent_ui::PromptSendOutcome::Enqueued => {
                // Defer editing the input until a window is available.
                // Clear only when its current value still matches the submitted text.
                self.agent_prompt_token += 1;
                self.agent_prompt_clear
                    .insert(dut_id.to_owned(), (text, self.agent_prompt_token));
                self.agent_transcript_seen.remove(dut_id);
            }
            duthub_desktop::agent_ui::PromptSendOutcome::Rejected { reason } => {
                // Keep a draft whose submission failed and leave no pending clear request.
                // Record the attempted text in the transcript so it can be retried.
                if let Some(view) = host.view(dut_id) {
                    view.push_entry(duthub_desktop::agent_host::TranscriptEntry::error(
                        crate::i18n::message(
                            "agentPromptSendFailed",
                            &[("text", &text), ("error", &reason)],
                        ),
                    ));
                }
                self.notify_toast(crate::i18n::label("agentPromptSendFailedToast"));
            }
        }
        cx.notify();
    }

    /// Reap finished Agent workers and forward transcript and status changes on the 50 ms tick.
    pub fn agent_host_tick(&mut self) -> bool {
        let host_dirty = self
            .agent_host
            .as_mut()
            .is_some_and(duthub_desktop::agent_host::HostManager::tick);
        let toasts: Vec<String> = {
            let mut queue = self.agent_host_toasts.lock().unwrap();
            queue.drain(..).collect()
        };
        let forwarded = !toasts.is_empty();
        self.push_toasts(toasts);
        host_dirty | forwarded
    }

    /// Get or create the DUT's prompt input and bind Enter to `send_agent_prompt`.
    pub(crate) fn agent_prompt_state(
        &mut self,
        dut_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some(editor) = self.agent_prompts.get(dut_id) {
            return editor.state.clone();
        }
        let state = cx.new(|cx| InputState::new(window, cx));
        let owner = dut_id.to_owned();
        let subscription = cx.subscribe(&state, move |this, _state, event, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.send_agent_prompt(&owner, cx);
            }
        });
        self.agent_prompts.insert(
            dut_id.to_owned(),
            AgentPromptEditor {
                state: state.clone(),
                _subscription: subscription,
            },
        );
        state
    }

    /// Clear the submitted draft during rendering, when the input can be updated through a window.
    ///
    /// Clear only if the input still matches the submitted text.
    /// Discard a stale clear request and preserve text entered in the meantime.
    /// Requests are keyed by DUT so changing tabs leaves other drafts alone.
    pub(crate) fn clear_agent_prompt_if_requested(
        &mut self,
        dut_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((sent, _token)) = self.agent_prompt_clear.get(dut_id).cloned() else {
            return;
        };
        let current = self
            .agent_prompts
            .get(dut_id)
            .map_or_default(|editor| editor.state.read(cx).value().clone());
        // Value-matched clear (pinned by prompt_clear_only_if_value_matches).
        if duthub_desktop::agent_ui::should_clear_prompt(&current, &sent)
            && let Some(editor) = self.agent_prompts.get(dut_id)
        {
            let state = editor.state.clone();
            state.update(cx, |state, cx| state.set_value("", window, cx));
        }
        self.agent_prompt_clear.remove(dut_id);
    }

    /// Return the explicitly selected model if it is enabled and its protocol is supported.
    pub(super) fn active_agent_model(&self) -> Option<(String, String)> {
        let provider_id = self.settings.agent.active_provider_id.as_deref()?.trim();
        let model_id = self.settings.agent.active_model_id.as_deref()?.trim();
        if provider_id.is_empty() || model_id.is_empty() {
            return None;
        }
        let provider = self
            .settings
            .agent
            .providers
            .iter()
            .find(|provider| provider.id == provider_id && provider.enabled)?;
        if !agent_host::model_env::protocol_supported(provider.protocol) {
            return None;
        }
        provider
            .models
            .iter()
            .any(|model| model.id == model_id && model.enabled)
            .then(|| (provider_id.to_owned(), model_id.to_owned()))
    }

    /// Find the first available model for persisting a fallback when there is no explicit choice.
    pub(super) fn first_enabled_agent_model(&self) -> Option<(String, String)> {
        self.settings
            .agent
            .providers
            .iter()
            .filter(|provider| {
                provider.enabled && agent_host::model_env::protocol_supported(provider.protocol)
            })
            .find_map(|provider| {
                provider
                    .models
                    .iter()
                    .find(|model| model.enabled && !model.id.trim().is_empty())
                    .map(|model| (provider.id.clone(), model.id.clone()))
            })
    }

    /// Save the selected provider and model IDs and synchronize the legacy Agent fields.
    /// The next Agent launch uses the saved choice.
    pub(super) fn persist_agent_model_binding(
        &mut self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<(), String> {
        let setting = {
            let provider = self
                .settings
                .agent
                .providers
                .iter()
                .find(|provider| provider.id == provider_id)
                .ok_or_else(|| i18n::message("providerNotFound", &[("id", provider_id)]))?;
            if !provider.enabled {
                return Err(i18n::message(
                    "providerDisabled",
                    &[("name", &provider.name)],
                ));
            }
            let model = provider
                .models
                .iter()
                .find(|model| model.id == model_id)
                .ok_or_else(|| {
                    i18n::message(
                        "modelNotFoundInProvider",
                        &[("provider", &provider.name), ("model", model_id)],
                    )
                })?;
            if !model.enabled {
                return Err(i18n::message("modelDisabled", &[("model", model_id)]));
            }
            if !agent_host::model_env::protocol_supported(provider.protocol) {
                return Err(i18n::message(
                    "protocolNotEnabled",
                    &[("protocol", provider.protocol.label())],
                ));
            }
            provider_setting(provider.protocol)
        };
        let mut candidate = self.settings.clone();
        candidate.agent.active_provider_id = Some(provider_id.to_owned());
        candidate.agent.active_model_id = Some(model_id.to_owned());
        store::save_settings_secured(&self.data_dir, &candidate)
            .map_err(|error| error.to_string())?;
        self.settings = candidate;
        setting.clone_into(&mut self.agent_model_provider);
        model_id.clone_into(&mut self.agent_model_name);
        Ok(())
    }

    /// Save the exact model choice for the next session launch.
    pub(crate) fn select_agent_model(&mut self, provider_id: &str, model_id: &str) {
        let provider_name = self
            .settings
            .agent
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .map_or_else(|| provider_id.to_owned(), |provider| provider.name.clone());
        match self.persist_agent_model_binding(provider_id, model_id) {
            Ok(()) => self.notify_toast(crate::i18n::message(
                "agentModelSelected",
                &[("provider", &provider_name), ("model", model_id)],
            )),
            Err(error) => {
                self.notify_toast(format!("{} {error}", crate::i18n::label("saveFailed")));
            }
        }
    }

    /// Keep the explicit choice when present, or record the first enabled model.
    /// Goose resolves its binding from the same saved provider and model IDs.
    pub(crate) fn save_goose_model(&mut self) {
        let Some((provider_id, model_id)) = self
            .active_agent_model()
            .or_else(|| self.first_enabled_agent_model())
        else {
            self.notify_toast(crate::i18n::label("agentRuntimeNoModel"));
            return;
        };
        match self.persist_agent_model_binding(&provider_id, &model_id) {
            Ok(()) => self.notify_toast(crate::i18n::label("agentModelSaved")),
            Err(error) => {
                self.notify_toast(format!("{} {error}", crate::i18n::label("saveFailed")));
            }
        }
    }

    /// Validate and persist the external goose path and SHA-256 hash in `agents.json`.
    /// An empty path clears the stored program and hash.
    ///
    /// Validate the absolute path before scheduling the hash and version check in the background.
    /// Persist only after the current generation's check completes.
    pub(crate) fn save_goose_external(&mut self, cx: &mut Context<Self>) {
        let input = self.input_value(In::GoosePath).trim().to_owned();
        if input.is_empty() {
            // Clear the external record: pure config write, no probing.
            self.commit_goose_external(None, None);
            self.notify_toast(crate::i18n::label("goosePathCleared"));
            self.drafts.remove(&In::GoosePath);
            cx.notify();
            return;
        }
        // Cheap gate on the UI thread; the expensive SHA-256 + version probe
        // runs on the background executor.
        if let Err(error) = duthub_desktop::agent_ui::validate_external_program_path(&input) {
            self.notify_toast(format!(
                "{} {error}",
                crate::i18n::label("goosePathInvalid")
            ));
            cx.notify();
            return;
        }
        self.goose_probe_generation += 1;
        let generation = self.goose_probe_generation;
        self.pending_goose_save = Some(PendingGooseSave {
            path: input.clone(),
            generation,
        });
        self.goose_probe_pending = true;
        let probe = cx.background_executor().spawn(async move {
            agent_host::registry::probe_program(std::path::Path::new(&input), true)
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = probe.await;
            let _ = this.update(cx, |this, cx| {
                this.finish_goose_external_save(generation, result);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Background-probe completion for the external goose path save. Stale
    /// results (an older generation than the pending save) are dropped without
    /// consuming the slot, so a superseded probe can never clear or overwrite
    /// a newer request's pending state.
    pub(super) fn finish_goose_external_save(
        &mut self,
        generation: u64,
        result: Result<agent_host::registry::ProgramProbe, String>,
    ) {
        let Some(pending) = self.pending_goose_save.as_ref() else {
            return;
        };
        if pending.generation != generation {
            return;
        }
        let path = pending.path.clone();
        self.pending_goose_save = None;
        self.goose_probe_pending = false;
        match result {
            Ok(probe) => {
                self.commit_goose_external(Some(path), Some(probe.sha256.clone()));
                self.goose_probe = Some(probe.clone());
                self.drafts.remove(&In::GoosePath);
                let sha = probe.sha256.get(..12).unwrap_or(&probe.sha256);
                let version = probe.version.clone().unwrap_or_default();
                self.notify_toast(crate::i18n::message(
                    "goosePathVerified",
                    &[("sha", sha), ("version", &version)],
                ));
            }
            Err(error) => {
                self.notify_toast(format!(
                    "{} {error}",
                    crate::i18n::label("goosePathInvalid")
                ));
            }
        }
    }

    /// Write the goose registry entry (program + expected SHA-256) atomically;
    /// `None` fields mean "restore embedded". No probing happens here.
    pub(super) fn commit_goose_external(&mut self, path: Option<String>, sha256: Option<String>) {
        let mut goose = self
            .agents
            .goose()
            .cloned()
            .unwrap_or_else(duthub_core::agent_config::AgentDefinition::goose);
        goose.program = path;
        goose.expected_sha256 = sha256;
        let mut file = self.agents.clone();
        match file.goose_mut() {
            Some(existing) => *existing = goose,
            None => file.agents.insert(0, goose),
        }
        if let Err(error) = duthub_core::agent_config::save_agents_atomic(&self.work_dir, &file) {
            self.notify_toast(format!("{} {error}", crate::i18n::label("saveFailed")));
            return;
        }
        self.agents = file;
        self.agents_error = None;
    }

    /// Clear the external goose entry and invalidate any pending program checks.
    pub(crate) fn clear_goose_external(&mut self) {
        self.goose_probe_generation += 1;
        self.pending_goose_save = None;
        self.goose_probe_pending = false;
        let mut goose = self
            .agents
            .goose()
            .cloned()
            .unwrap_or_else(duthub_core::agent_config::AgentDefinition::goose);
        goose.program = None;
        goose.expected_sha256 = None;
        let mut file = self.agents.clone();
        match file.goose_mut() {
            Some(existing) => *existing = goose,
            None => file.agents.insert(0, goose),
        }
        if let Err(error) = duthub_core::agent_config::save_agents_atomic(&self.work_dir, &file) {
            self.notify_toast(format!("{} {error}", crate::i18n::label("saveFailed")));
            return;
        }
        self.agents = file;
        self.agents_error = None;
        self.goose_probe = None;
        self.drafts.remove(&In::GoosePath);
        self.notify_toast(crate::i18n::label("goosePathCleared"));
    }
}
