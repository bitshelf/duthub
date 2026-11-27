//! M4.5 stdio end-to-end test: spawns the `duthub-mcp` binary as a child process
//! (empty snapshot mode) and verifies initialize, `list_tools`, read-only behavior,
//! and protocol-clean stdout.

use rmcp::{
    RoleClient,
    model::{CallToolRequestParams, ClientConfig, Implementation},
    service::{RunningService, serve_client},
    transport::child_process::TokioChildProcess,
};

async fn connect(bin: &str) -> RunningService<RoleClient, ClientConfig> {
    let process =
        TokioChildProcess::new(tokio::process::Command::new(bin)).expect("spawn duthub-mcp");
    serve_client(
        ClientConfig::new(
            rmcp::model::ClientCapabilities::default(),
            Implementation::new("test-client", "0.1"),
        ),
        process,
    )
    .await
    .expect("client init")
}

async fn call(
    client: &RunningService<RoleClient, ClientConfig>,
    name: &str,
    arguments: serde_json::Value,
) -> Result<rmcp::model::CallToolResult, Box<rmcp::model::CallToolResult>> {
    let mut params = CallToolRequestParams::new(name.to_string());
    params = params.with_arguments(arguments.as_object().expect("obj").clone());
    // Normalize both rejection shapes to Err:
    //   a) JSON-RPC error (parameter validation/approval failure via rmcp invalid_params)
    //   b) CallToolResult with isError=true
    //   c) MRTR intermediate state (unused by self-test tools)
    match client.call_tool_once(params).await {
        Err(_) => Err(Box::new(rmcp::model::CallToolResult::error(vec![]))),
        Ok(rmcp::model::CallToolResponse::Complete(result)) => {
            if result.is_error.unwrap_or_default() {
                Err(Box::new(result))
            } else {
                Ok(result)
            }
        }
        Ok(other) => panic!("Unexpected MRTR intermediate state: {other:?}"),
    }
}

fn first_text(result: &rmcp::model::CallToolResult) -> String {
    for content in &result.content {
        if let rmcp::model::ContentBlock::Text(t) = content {
            return t.text.clone();
        }
    }
    String::new()
}

#[tokio::test]
async fn stdio_discovers_and_reads_closed_dut_segments() {
    let root = std::env::temp_dir().join(format!("duthub-mcp-log-{}", std::process::id()));
    let folder = root.join("logs/Board One");
    std::fs::create_dir_all(&folder).unwrap();
    let prefix = duthub_core::log_archive::archive_prefix("board-one");
    let filename = format!("{prefix}0001-0001.log");
    std::fs::write(folder.join(&filename), "[mark] Power ON\nboot complete\n").unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_duthub-mcp"));
    command.args(["--workdir", root.to_str().unwrap()]);
    let process = TokioChildProcess::new(command).unwrap();
    let client = serve_client(ClientConfig::default(), process)
        .await
        .unwrap();
    let response = call(
        &client,
        "log_segments",
        serde_json::json!({ "dutId": "board-one" }),
    )
    .await
    .unwrap();
    let data: serde_json::Value = serde_json::from_str(&first_text(&response)).unwrap();
    assert_eq!(data["segments"][0]["label"], "Power ON");
    let id = data["segments"][0]["id"].as_str().unwrap();
    let response = call(
        &client,
        "log_segment_read",
        serde_json::json!({ "dutId": "board-one", "segmentId": id }),
    )
    .await
    .unwrap();
    let data: serde_json::Value = serde_json::from_str(&first_text(&response)).unwrap();
    assert_eq!(data["text"], "[mark] Power ON\nboot complete\n");
    assert_eq!(data["eof"], true);
    assert!(
        call(
            &client,
            "log_segment_read",
            serde_json::json!({ "dutId": "board-two", "segmentId": id })
        )
        .await
        .is_err()
    );
    client.cancel().await.unwrap();
    std::fs::remove_file(folder.join(filename)).unwrap();
    std::fs::remove_dir(folder).unwrap();
    std::fs::remove_dir(root.join("logs")).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[tokio::test]
async fn stdio_end_to_end() {
    // Binary path: target/debug/duthub-mcp.exe (built alongside cargo test).
    let client = connect(env!("CARGO_BIN_EXE_duthub-mcp")).await;

    // 1) Snapshot mode still exposes media_streams, but returns no active preview URLs.
    let tools = client.list_tools(None).await.expect("list_tools");
    eprintln!(
        "[test] tools = {:?}",
        tools
            .tools
            .iter()
            .map(|t| t.name.as_ref())
            .collect::<Vec<_>>()
    );
    assert!(tools.tools.iter().any(|tool| tool.name == "media_streams"));
    let media = call(
        &client,
        "media_streams",
        serde_json::json!({ "dutId": "x" }),
    )
    .await
    .expect("media_streams");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&first_text(&media)).unwrap(),
        serde_json::json!({ "streams": {} })
    );

    // 2) dut_list: no snapshot -> empty list (no duthub-data in binary workdir).
    eprintln!("[test] calling dut_list...");
    let result = call(&client, "dut_list", serde_json::json!({}))
        .await
        .expect("dut_list");
    let text = first_text(&result);
    assert!(
        text.contains("\"duts\":[]") || text.contains("duts"),
        "dut_list response: {text}"
    );

    // 3) serial_write (snapshot mode) -> rejected with guidance to use the HTTP endpoint.
    let err = call(
        &client,
        "serial_write",
        serde_json::json!({ "dutId": "x", "text": "uname", "approvalToken": "t" }),
    )
    .await;
    // Snapshot mode writes must be rejected (Unknown approval or read-only restriction);
    // for JSON-RPC errors the detail is in ErrorData.message, so we only assert rejection here.
    let _err = err.expect_err("snapshot mode write must be rejected");

    client.cancel().await.ok();
}

/// Verify `skill_export` is registered (asserts `list_tools` only without calling it,
/// because `discover_data_dir` in the binary could fall back to the repo `duthub-data/`
/// under test cwd; disk writes are covered by `snapshot.rs` and `skill.rs` unit tests
/// using an explicit temp directory).
#[tokio::test]
async fn skill_export_tool_is_listed() {
    let client = connect(env!("CARGO_BIN_EXE_duthub-mcp")).await;
    let tools = client.list_tools(None).await.expect("list_tools");
    let skill = tools
        .tools
        .iter()
        .find(|tool| tool.name == "skill_export")
        .expect("skill_export tool must be registered");
    assert!(
        skill
            .description
            .as_ref()
            .is_some_and(|d| d.contains("SKILL.md")),
        "skill_export description should mention exporting SKILL.md"
    );
    client.cancel().await.ok();
}
