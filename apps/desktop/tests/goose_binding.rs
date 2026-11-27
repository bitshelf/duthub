//! F7/F11/F19: model binding selection and session resume fallback regressions.
//!
//! - Dual providers: explicitly selecting B must use B's `base_url`/model/key (not enablement
//!   order);
//! - Host trims and validates bindings before launch (blank fields are rejected before spawning);
//! - When `session-id` exists and the agent supports `loadSession`, resume it; stale/failed resumes
//!   fall back to a new session with a degraded-session note.

#![cfg(windows)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use duthub_core::agent_access::{AgentAccessRuntime, AgentAccessSettings};
use duthub_core::agent_config::AgentDefinition;
use duthub_core::lease::ControlLeaseManager;
use duthub_core::model::{ModelProvider, ProviderModel};
use duthub_core::{AuditSink, Provider, model_provider_credential_id};
use duthub_desktop::agent_host::host::{HostError, HostManager};
use duthub_desktop::agent_host::model_env::{self, GooseBinding};
use duthub_desktop::agent_host::registry;
use duthub_desktop::agent_host::sandbox;
use duthub_desktop::agent_host::session::SessionStatus;
use duthub_mcp::auth_http::AccessContext;
use duthub_mcp::snapshot::SnapshotState;

const PROVIDER_A: &str = "https://provider-a.example.com/v1";
const PROVIDER_B: &str = "https://provider-b.example.com/v1";

fn access_context() -> Arc<AccessContext> {
    let auth = AgentAccessRuntime::from_settings(AgentAccessSettings::default());
    let lease = Arc::new(ControlLeaseManager::new(None));
    Arc::new(AccessContext::new(auth, lease, AuditSink::memory()))
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("duthub-goose-bind-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn provider(id: &str, name: &str, base_url: &str, model: &str) -> ModelProvider {
    ModelProvider {
        id: id.into(),
        name: name.into(),
        base_url: base_url.into(),
        protocol: Provider::Openai,
        enabled: true,
        models: vec![ProviderModel::new(model, true)],
    }
}

fn binding() -> GooseBinding {
    GooseBinding {
        provider: Provider::Openai,
        base_url: "http://127.0.0.1:1/v1".into(),
        model: "fake-model".into(),
        api_key: "sk-binding-secret".into(),
    }
}

// ---------------------------------------------------------------------------
// Test goose fixture (goose is downloaded, not embedded)
// ---------------------------------------------------------------------------

/// `DUTHUB_TEST_GOOSE` (absolute path to a goose executable) wins, otherwise the
/// product resolver decides (downloaded install > PATH). `None` = not installed.
fn test_goose_program() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("DUTHUB_TEST_GOOSE") {
        let path = PathBuf::from(raw);
        assert!(
            path.is_absolute() && path.is_file(),
            "DUTHUB_TEST_GOOSE must be an absolute path to an existing goose executable: {}",
            path.display()
        );
        return Some(path);
    }
    registry::goose_install_state()
        .program()
        .map(std::path::Path::to_path_buf)
}

/// External-program definition that runs the test goose through the same
/// fail-closed path as production (absolute path + pinned SHA-256).
fn test_goose_agent() -> Option<AgentDefinition> {
    let program = test_goose_program()?;
    let sha256 = duthub_core::store::sha256_file(&program).ok()?;
    let mut agent = AgentDefinition::goose();
    agent.program = Some(program.display().to_string());
    agent.expected_sha256 = Some(sha256);
    Some(agent)
}

/// Loud skip: goose is no longer embedded, so ACP tests need an installed one.
fn skip_without_goose() {
    eprintln!(
        "SKIP: goose is not installed (goose is no longer embedded) — \
         download goose from Tool Downloads or set DUTHUB_TEST_GOOSE to the goose executable"
    );
}

fn wait_ready(host: &mut HostManager, dut: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        host.tick();
        if host
            .view(dut)
            .is_some_and(|view| matches!(view.status(), SessionStatus::Ready))
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_terminal(host: &mut HostManager, dut: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        host.tick();
        if host
            .view(dut)
            .is_some_and(|view| view.status().is_terminal())
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn active_ids_serialize_as_camel_case_and_default_to_none() {
    let raw = r#"{
        "version": 1,
        "appearance": {"theme": "dark", "language": "zh-CN", "platform": "windows"},
        "agent": {
            "activeProviderId": "b",
            "activeModelId": "model-b",
            "provider": "openai",
            "slow": "model-b"
        }
    }"#;
    let settings: duthub_core::SettingsFile = serde_json::from_str(raw).unwrap();
    assert_eq!(settings.agent.active_provider_id.as_deref(), Some("b"));
    assert_eq!(settings.agent.active_model_id.as_deref(), Some("model-b"));
    let value = serde_json::to_value(&settings).unwrap();
    assert_eq!(value["agent"]["activeProviderId"], "b");
    assert_eq!(value["agent"]["activeModelId"], "model-b");

    // None must be omitted from serialization (preserves round-trip equivalence with examples).
    let defaults = duthub_core::store::default_settings();
    let value = serde_json::to_value(&defaults).unwrap();
    assert!(value["agent"].get("activeProviderId").is_none());
    assert!(value["agent"].get("activeModelId").is_none());
}

#[test]
fn selected_provider_b_wins_with_its_own_endpoint_model_and_key() {
    let mut settings = duthub_core::store::default_settings();
    // A appears first and is enabled; only explicit selection ensures B wins.
    settings.agent.providers = vec![
        provider("a", "Provider A", PROVIDER_A, "model-a"),
        provider("b", "Provider B", PROVIDER_B, "model-b"),
    ];
    settings.agent.active_provider_id = Some("b".into());
    settings.agent.active_model_id = Some("model-b".into());

    let key_a = model_provider_credential_id("a", Provider::Openai, PROVIDER_A);
    let key_b = model_provider_credential_id("b", Provider::Openai, PROVIDER_B);
    let binding = model_env::resolve_binding_with(&settings, |id| {
        if id == key_a {
            Ok(Some("sk-a".into()))
        } else if id == key_b {
            Ok(Some("sk-b".into()))
        } else {
            Ok(None)
        }
    })
    .unwrap();

    assert_eq!(binding.provider, Provider::Openai);
    assert_eq!(binding.base_url, PROVIDER_B);
    assert_eq!(binding.model, "model-b");
    assert_eq!(binding.api_key, "sk-b");

    // Environment variables must also come from B, not A.
    let root = temp_dir("env").join("goose");
    std::fs::create_dir_all(&root).unwrap();
    let env = model_env::goose_env(&binding, &root).unwrap();
    assert!(env.contains(&("OPENAI_API_KEY".into(), "sk-b".into())));
    assert!(env.contains(&("OPENAI_BASE_URL".into(), PROVIDER_B.into())));
    assert!(env.contains(&("GOOSE_MODEL".into(), "model-b".into())));
    let _ = std::fs::remove_dir_all(root.parent().unwrap());
}

#[test]
fn host_start_normalizes_binding_and_rejects_blank_fields() {
    let normalized = model_env::normalize_binding(&GooseBinding {
        provider: Provider::Openai,
        base_url: "  https://api.example.com/v1/  ".into(),
        model: "  model-a  ".into(),
        api_key: "  sk-trim  ".into(),
    })
    .unwrap();
    assert_eq!(normalized.base_url, "https://api.example.com/v1");
    assert_eq!(normalized.model, "model-a");
    assert_eq!(normalized.api_key, "sk-trim");

    let work = temp_dir("blank");
    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    let error = host
        .start(
            "dut-blank",
            GooseBinding {
                provider: Provider::Openai,
                base_url: "https://api.example.com/v1".into(),
                model: "   ".into(),
                api_key: "sk".into(),
            },
        )
        .unwrap_err();
    assert!(matches!(error, HostError::Invalid(_)), "{error}");
    assert!(
        host.view("dut-blank").is_none(),
        "validation failure must not create a session entry or spawn goose"
    );
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn stale_session_id_falls_back_to_new_session_with_degraded_note() {
    let Some(agent) = test_goose_agent() else {
        skip_without_goose();
        return;
    };
    let work = temp_dir("stale");
    let paths = sandbox::prepare(&work, "dut-stale").unwrap();
    std::fs::write(&paths.session_id, "stale-session-id").unwrap();

    let mut host = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    host.start_with_agent("dut-stale", Some(binding()), agent)
        .unwrap();
    assert!(
        wait_ready(&mut host, "dut-stale"),
        "fallback new session did not become ready: {:?} stderr={}",
        host.view("dut-stale").map(|view| view.status()),
        host.view("dut-stale")
            .map_or_default(|view| view.stderr_tail())
    );

    let view = host.view("dut-stale").unwrap();
    let session_id = view.session_id().unwrap();
    assert_ne!(
        session_id, "stale-session-id",
        "stale session id must not be treated as valid"
    );
    let entries = view.transcript_snapshot();
    assert!(
        entries
            .iter()
            .any(|entry| entry.text.contains("已降级为新会话")),
        "degraded note must appear in transcript: {entries:?}"
    );
    let stored = std::fs::read_to_string(&paths.session_id).unwrap();
    assert_eq!(
        stored.trim(),
        session_id,
        "new session id must be persisted to disk"
    );

    host.stop("dut-stale");
    assert!(wait_terminal(&mut host, "dut-stale"));
    drop(host);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn stored_session_id_is_resumed_when_agent_supports_load() {
    let Some(agent) = test_goose_agent() else {
        skip_without_goose();
        return;
    };
    let work = temp_dir("resume");
    let paths = sandbox::prepare(&work, "dut-resume").unwrap();

    // First launch creates a session, then explicitly stops (which clears resume markers).
    let mut first = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    first
        .start_with_agent("dut-resume", Some(binding()), agent.clone())
        .unwrap();
    assert!(
        wait_ready(&mut first, "dut-resume"),
        "initial session did not become ready: {:?}",
        first.view("dut-resume").map(|view| view.status())
    );
    let original = first.view("dut-resume").unwrap().session_id().unwrap();
    first.stop("dut-resume");
    assert!(wait_terminal(&mut first, "dut-resume"));
    drop(first);

    // Simulate an unclean exit: restore the valid session-id so the next launch resumes it.
    std::fs::write(&paths.session_id, &original).unwrap();
    let mut second = HostManager::new(
        work.clone(),
        Arc::new(SnapshotState::empty()),
        access_context(),
        None,
    );
    second
        .start_with_agent("dut-resume", Some(binding()), agent.clone())
        .unwrap();
    assert!(
        wait_ready(&mut second, "dut-resume"),
        "resumed session did not become ready: {:?} stderr={}",
        second.view("dut-resume").map(|view| view.status()),
        second
            .view("dut-resume")
            .map_or_default(|view| view.stderr_tail())
    );
    let view = second.view("dut-resume").unwrap();
    assert_eq!(
        view.session_id().as_deref(),
        Some(original.as_str()),
        "should resume the same session; transcript={:?}",
        view.transcript_snapshot()
    );
    assert!(
        view.transcript_snapshot()
            .iter()
            .any(|entry| entry.text.contains("已恢复上次会话")),
        "resumed note must appear in transcript: {:?}",
        view.transcript_snapshot()
    );

    second.stop("dut-resume");
    assert!(wait_terminal(&mut second, "dut-resume"));
    drop(second);
    let _ = std::fs::remove_dir_all(&work);
}
