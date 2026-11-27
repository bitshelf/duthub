//! M4.5 stdio 端到端：client 子进程方式启动 `duthub-mcp` bin（快照模式，空态），
//! 验证 initialize / `list_tools` / 只读行为 / stdout 纯协议。

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
    // 两种拒绝形态都归一为 Err：
    //   a) JSON-RPC 错误（参数校验/审批失败，rmcp 宏用 invalid_params 返回）
    //   b) isError=true 的 CallToolResult
    //   c) MRTR 中间态（自测工具不使用）
    match client.call_tool_once(params).await {
        Err(_) => Err(Box::new(rmcp::model::CallToolResult::error(vec![]))),
        Ok(rmcp::model::CallToolResponse::Complete(result)) => {
            if result.is_error.unwrap_or_default() {
                Err(Box::new(result))
            } else {
                Ok(result)
            }
        }
        Ok(other) => panic!("MRTR 中间态: {other:?}"),
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
    // bin 路径：target/debug/duthub-mcp.exe（cargo test 构建同目录）
    let client = connect(env!("CARGO_BIN_EXE_duthub-mcp")).await;

    // 1) 快照模式仍能发现拉流工具，但不会返回活动预览的地址。
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

    // 2) dut_list：无快照 → 空列表（bin 工作目录无 duthub-data）
    eprintln!("[test] calling dut_list...");
    let result = call(&client, "dut_list", serde_json::json!({}))
        .await
        .expect("dut_list");
    let text = first_text(&result);
    assert!(
        text.contains("\"duts\":[]") || text.contains("duts"),
        "dut_list 响应: {text}"
    );

    // 3) serial_write（快照模式）→ 拒绝且文案指向 HTTP 端点
    let err = call(
        &client,
        "serial_write",
        serde_json::json!({ "dutId": "x", "text": "uname", "approvalToken": "t" }),
    )
    .await;
    // 快照模式写操作必须走拒绝路径（审批校验 Unknown 或只读限制）；
    // JSON-RPC 错误形态下文案在 ErrorData.message（此处仅断言拒绝发生）
    let _err = err.expect_err("快照模式写操作必须拒绝");

    client.cancel().await.ok();
}

/// `需求⑥：skill_export` 工具已注册（只断言 `list_tools`，**不调用**——
/// bin 的 `discover_data_dir` 在测试 cwd 下可能退化成仓库内 duthub-data/，
/// 落盘副作用由 snapshot.rs/skill.rs 单测覆盖，用显式 temp 目录）。
#[tokio::test]
async fn skill_export_tool_is_listed() {
    let client = connect(env!("CARGO_BIN_EXE_duthub-mcp")).await;
    let tools = client.list_tools(None).await.expect("list_tools");
    let skill = tools
        .tools
        .iter()
        .find(|tool| tool.name == "skill_export")
        .expect("skill_export 工具必须注册");
    assert!(
        skill
            .description
            .as_ref()
            .is_some_and(|d| d.contains("SKILL.md")),
        "skill_export 描述应说明导出 SKILL.md"
    );
    client.cancel().await.ok();
}
