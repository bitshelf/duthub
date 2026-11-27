//! Evidence tool calls bridged to the W1 MCP evidence server over an
//! in-process duplex transport.
//!
//! Each skill call spins up a fresh `McpServerCore::for_evidence` server on
//! one end of a `tokio::io::duplex` pipe and a throwaway rmcp client on the
//! other, so the A2A executor reuses the exact W1 evidence surface
//! ([`EVIDENCE_SKILLS`]) with zero network I/O. Nothing here touches the
//! serial port directly; all reads go through the injected `McpState`.

use std::sync::Arc;
use std::time::Duration;

use a2a::A2AError;
use duthub_mcp::server::McpServerCore;
use duthub_mcp::state::McpState;
use rmcp::model::CallToolRequestParams;
use rmcp::service::ServiceExt;
use rmcp::transport::async_rw::AsyncRwTransport;

/// The only skills the A2A surface accepts — the W1 evidence tool list.
/// The executor fails any other skill (and any TextPart) with a policy
/// rejection before the MCP server is even started.
pub const EVIDENCE_SKILLS: [&str; 5] = duthub_mcp::server::EVIDENCE_TOOL_NAMES;

/// Per-call cap: evidence tools are quick local reads; a stuck call must not
/// pin the A2A task forever.
const EVIDENCE_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// In-process duplex buffer; evidence JSON responses are small.
const DUPLEX_BUF: usize = 64 * 1024;

/// An evidence call that did not produce a usable result. The executor turns
/// this into a `TASK_STATE_FAILED` protocol task (never a transport error).
#[derive(Debug, Clone)]
pub struct EvidenceFailure {
    pub message: String,
}

impl EvidenceFailure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Run `skill` with `args` against the evidence server bound to `dut_id`.
/// Returns the tool's structured JSON result on success.
pub async fn call_evidence_tool(
    state: &Arc<dyn McpState>,
    dut_id: &str,
    skill: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, EvidenceFailure> {
    let (client_io, server_io) = tokio::io::duplex(DUPLEX_BUF);
    let (client_read, client_write) = tokio::io::split(client_io);
    let (server_read, server_write) = tokio::io::split(server_io);

    // Evidence server side: no auth context (in-process peer), one DUT,
    // restricted to EVIDENCE_TOOL_NAMES (W1 surface).
    let server = McpServerCore::for_evidence(state.clone(), dut_id);
    let server_task = tokio::spawn(async move {
        match server
            .serve(AsyncRwTransport::new_server(server_read, server_write))
            .await
        {
            Ok(running) => {
                let _ = running.waiting().await;
            }
            Err(error) => {
                tracing::warn!(%error, "A2A evidence server failed to initialize");
            }
        }
    });

    let call = async {
        let client = ().serve(AsyncRwTransport::new_client(client_read, client_write)).await;
        let client = client.map_err(|error| {
            EvidenceFailure::new(format!("evidence server init failed: {error}"))
        })?;
        let params = CallToolRequestParams::new(skill.to_owned())
            .with_arguments(args.as_object().cloned().unwrap_or_default());
        let result = client
            .call_tool(params)
            .await
            .map_err(|error| EvidenceFailure::new(format!("evidence tool call failed: {error}")))?;
        let value = tool_result_to_value(&result)?;
        // Shut the client down so the server side sees EOF and finishes.
        let _ = client.cancel().await;
        Ok(value)
    };

    let outcome = match tokio::time::timeout(EVIDENCE_CALL_TIMEOUT, call).await {
        Ok(outcome) => outcome,
        Err(_) => Err(EvidenceFailure::new("evidence tool call timed out")),
    };
    let _ = server_task.await;
    outcome
}

fn tool_result_to_value(
    result: &rmcp::model::CallToolResult,
) -> Result<serde_json::Value, EvidenceFailure> {
    if result.is_error == Some(true) {
        return Err(EvidenceFailure::new(format!(
            "evidence tool reported an error: {}",
            first_text(result).unwrap_or_else(|| "unknown tool error".to_owned())
        )));
    }
    if let Some(structured) = &result.structured_content {
        return Ok(structured.clone());
    }
    Ok(serde_json::json!({ "text": first_text(result).unwrap_or_default() }))
}

fn first_text(result: &rmcp::model::CallToolResult) -> Option<String> {
    result.content.iter().find_map(|block| match block {
        rmcp::model::ContentBlock::Text(text) => Some(text.text.clone()),
        _ => None,
    })
}

/// Map a client skill request onto the evidence tool call:
/// - skill must be in [`EVIDENCE_SKILLS`] (policy failure otherwise);
/// - an explicit `dutId` must match the bound DUT (cross-DUT is rejected);
/// - `args.dutId` is forced to the bound DUT so a crafted `args` payload
///   cannot smuggle a different DUT past the policy layer.
pub fn prepare_tool_args(
    skill: &str,
    dut_id: &str,
    requested_dut_id: Option<&str>,
    args: serde_json::Value,
) -> Result<serde_json::Value, A2AError> {
    if !EVIDENCE_SKILLS.contains(&skill) {
        return Err(A2AError::invalid_params(format!(
            "POLICY: skill {skill:?} is not on the evidence allowlist"
        )));
    }
    if let Some(requested) = requested_dut_id
        && requested != dut_id
    {
        return Err(A2AError::invalid_params(format!(
            "POLICY: this endpoint serves DUT {dut_id} only; dutId {requested:?} was rejected"
        )));
    }
    let mut object = match args {
        serde_json::Value::Null => serde_json::Map::new(),
        serde_json::Value::Object(map) => map,
        other => {
            return Err(A2AError::invalid_params(format!(
                "POLICY: args must be a JSON object, got {other}"
            )));
        }
    };
    // Evidence tools address the DUT via camelCase `dutId` (server arg
    // structs use #[serde(rename_all = "camelCase")]).
    if object.keys().any(|key| key == "dut_id") {
        return Err(A2AError::invalid_params(
            "POLICY: args must use camelCase keys (dutId, not dut_id)",
        ));
    }
    // A dutId smuggled inside args that disagrees with the bound DUT is a
    // policy rejection, not a silent overwrite.
    if let Some(requested) = object.get("dutId").and_then(serde_json::Value::as_str)
        && requested != dut_id
    {
        return Err(A2AError::invalid_params(format!(
            "POLICY: this endpoint serves DUT {dut_id} only; args.dutId {requested:?} was rejected"
        )));
    }
    object.insert("dutId".to_owned(), serde_json::json!(dut_id));
    Ok(serde_json::Value::Object(object))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_skills_match_w1_surface() {
        assert_eq!(EVIDENCE_SKILLS.len(), 5);
        assert!(EVIDENCE_SKILLS.contains(&"dut_list"));
        assert!(EVIDENCE_SKILLS.contains(&"dut_status"));
        assert!(!EVIDENCE_SKILLS.contains(&"dut_files"));
        assert!(!EVIDENCE_SKILLS.contains(&"media_streams"));
    }

    #[test]
    fn prepare_args_forces_bound_dut() {
        let args = prepare_tool_args("dut_status", "dut-a", None, serde_json::json!({})).unwrap();
        assert_eq!(args["dutId"], serde_json::json!("dut-a"));
    }

    #[test]
    fn prepare_args_rejects_cross_dut_and_snake_case() {
        let error = prepare_tool_args(
            "dut_status",
            "dut-a",
            Some("dut-b"),
            serde_json::json!({"dutId": "dut-a"}),
        )
        .unwrap_err();
        assert_eq!(error.code, a2a::error_code::INVALID_PARAMS);

        let error = prepare_tool_args(
            "dut_status",
            "dut-a",
            None,
            serde_json::json!({"dut_id": "dut-a"}),
        )
        .unwrap_err();
        assert_eq!(error.code, a2a::error_code::INVALID_PARAMS);

        let error =
            prepare_tool_args("flash_now", "dut-a", None, serde_json::json!({})).unwrap_err();
        assert_eq!(error.code, a2a::error_code::INVALID_PARAMS);

        let error =
            prepare_tool_args("serial_read", "dut-a", None, serde_json::json!([1])).unwrap_err();
        assert_eq!(error.code, a2a::error_code::INVALID_PARAMS);
    }
}
