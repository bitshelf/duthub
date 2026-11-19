//! Agent 接入模型集成测试（方案 N5）：
//! 旧 settings.json 兼容（无 agentAccess 段 → 默认 auth 关）、camelCase 往返、
//! 默认值段整段跳过序列化（坑 #19/#24）、认证/授权矩阵（deny by default）。

use duthub_core::agent_access::{AgentAccessSettings, DutAction, DutGrant, generate_token};
use duthub_core::{AgentAccessRuntime, Principal, SettingsFile};

fn settings_with(agents: Vec<duthub_core::AgentCredential>, enabled: bool) -> SettingsFile {
    let mut settings = duthub_core::store::default_settings();
    settings.agent_access = AgentAccessSettings {
        enabled,
        agents,
        ..Default::default()
    };
    settings
}

/// 旧 settings.json（无 agentAccess 段）加载 → 默认段（auth 关、无凭据），
/// 序列化时整段省略（不改变旧文件形状）。
#[test]
fn legacy_settings_file_defaults_to_auth_off_and_omits_section() {
    let legacy = serde_json::json!({
        "version": 1,
        "appearance": { "theme": "dark", "language": "zh-CN", "platform": "windows" }
    });
    let parsed: SettingsFile = serde_json::from_value(legacy).unwrap();
    assert_eq!(
        parsed.agent_access,
        AgentAccessSettings::default(),
        "旧文件缺段必须反序列化为默认"
    );
    assert!(!parsed.agent_access.enabled, "auth 默认关");
    assert_eq!(parsed.agent_access.http_port, 8797);
    assert_eq!(parsed.agent_access.lease_ttl_secs, 120);
    // 默认段序列化省略（roundtrip 无损的关键）
    let back = serde_json::to_value(&parsed).unwrap();
    assert!(
        back.get("agentAccess").is_none(),
        "默认 agentAccess 段必须整段跳过序列化: {back}"
    );
}

/// 配置了凭据的 settings.json：camelCase 键往返 + 非默认段保留。
#[test]
fn agent_access_section_roundtrips_camel_case() {
    let token = generate_token();
    let settings = settings_with(
        vec![duthub_core::AgentCredential {
            listener_id: None,
            id: "cred-claude".into(),
            agent_name: "Claude".into(),
            token: token.clone(),
            enabled: true,
            grants: vec![
                DutGrant {
                    dut_id: "dut-1".into(),
                    actions: vec![DutAction::Read, DutAction::SerialWrite],
                },
                DutGrant {
                    dut_id: "dut-2".into(),
                    actions: vec![DutAction::Read, DutAction::Relay],
                },
            ],
        }],
        true,
    );
    let value = serde_json::to_value(&settings).unwrap();
    let section = value.get("agentAccess").expect("非默认段必须序列化");
    assert_eq!(section["enabled"], serde_json::json!(true));
    assert_eq!(section["httpPort"], serde_json::json!(8797));
    assert_eq!(section["leaseTtlSecs"], serde_json::json!(120));
    assert_eq!(
        section["agents"][0]["agentName"],
        serde_json::json!("Claude")
    );
    assert_eq!(
        section["agents"][0]["grants"][0]["dutId"],
        serde_json::json!("dut-1")
    );
    assert_eq!(
        section["agents"][0]["grants"][0]["actions"],
        serde_json::json!(["read", "serialWrite"])
    );
    // 类型化 → 再反序列化语义相等（roundtrip.rs 同款判据）
    let re: SettingsFile = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(re, settings);
    assert_eq!(re.agent_access.agents[0].token, token, "原文存储");
}

/// 带凭据段写入文件再读回（store 级别）：默认段不落盘、凭据段无损。
#[test]
fn settings_file_store_roundtrip_with_agent_access() {
    let dir = std::env::temp_dir().join(format!(
        "duthub-auth-it-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let token = generate_token();
    let settings = settings_with(
        vec![duthub_core::AgentCredential {
            listener_id: None,
            id: "cred-x".into(),
            agent_name: "X".into(),
            token,
            enabled: false,
            grants: Vec::new(),
        }],
        false,
    );
    duthub_core::store::save_settings_atomic(&dir, &settings).unwrap();
    let loaded = duthub_core::store::load_settings(&dir).unwrap();
    assert_eq!(loaded, settings);
    std::fs::remove_dir_all(&dir).ok();
}

/// 认证/授权矩阵端到端（Runtime）：开关切换语义 + deny by default。
#[test]
fn runtime_auth_matrix() {
    let token = generate_token();
    let rt = AgentAccessRuntime::from_settings(AgentAccessSettings {
        enabled: false,
        agents: vec![duthub_core::AgentCredential {
            listener_id: None,
            id: "cred-a".into(),
            agent_name: "A".into(),
            token: token.clone(),
            enabled: true,
            grants: vec![DutGrant {
                dut_id: "dut-1".into(),
                actions: vec![DutAction::Read],
            }],
        }],
        ..Default::default()
    });
    // 关：任何 Bearer（含错误 token）都视为本地管理员
    assert_eq!(rt.authenticate(None).unwrap(), Principal::LocalAdmin);
    assert_eq!(
        rt.authenticate(Some("junk")).unwrap(),
        Principal::LocalAdmin
    );
    // 开：正确 token → Agent；错误 → Unauthorized
    rt.update(AgentAccessSettings {
        enabled: true,
        agents: rt.settings().agents.clone(),
        ..Default::default()
    });
    let p = rt.authenticate(Some(&token)).unwrap();
    assert!(p.audit_identity().is_some());
    assert!(rt.authenticate(Some(&generate_token())).is_err());
    // 授权：只读授予 → read 过、serialWrite 拒、其他 DUT 拒
    rt.authorize(&p, "dut-1", DutAction::Read).unwrap();
    assert!(rt.authorize(&p, "dut-1", DutAction::SerialWrite).is_err());
    assert!(rt.authorize(&p, "dut-2", DutAction::Read).is_err());
    // LocalAdmin 全通过
    rt.authorize(&Principal::LocalAdmin, "dut-9", DutAction::Flash)
        .unwrap();
    // 热更新增授权 → 立即生效（AuthSnapshot 替换）
    let mut next = rt.settings();
    next.agents[0].grants.push(DutGrant {
        dut_id: "dut-2".into(),
        actions: vec![DutAction::Read],
    });
    rt.update(next);
    rt.authorize(&p, "dut-2", DutAction::Read).unwrap();
}
