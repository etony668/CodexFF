//! 集成测试: 官方 ↔ 中转 切换流程 + 凭证物理隔离。
//!
//! 环境隔离: CODEX_HOME + CODEXFF_VAULT_DIR 指到临时目录, 不碰真实 ~/.codex。
//! 注意: relay key 走系统 keyring (macOS Keychain), 首次跑可能弹授权, 测试结束会删除。
//!
//! 运行: cd src-tauri && cargo test

use std::fs;
use std::path::PathBuf;

use base64::engine::general_purpose;
use base64::Engine;
use codexff_lib::profiles;
use codexff_lib::vault;

fn temp_env(name: &str) -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    // panic 后 mutex 会被 poison, 测试并行会连环失败 — 容错
    let guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let dir = tempfile::tempdir().expect("tempdir");
    // codex_config 路径解析读 env, 直接设置
    std::env::set_var("CODEX_HOME", dir.path().join("codex"));
    std::env::set_var("CODEXFF_VAULT_DIR", dir.path().join("vault"));
    std::env::set_var("CODEXFF_DATA_DIR", dir.path().join("data"));
    fs::create_dir_all(dir.path().join("codex")).unwrap();
    let _ = name;
    (dir, guard)
}

/// 造一份模拟官方登录的 auth.json
fn seed_official_auth() {
    let auth = serde_json::json!({
        "ChatGPT": {
            "access_token": "test-access-token",
            "refresh_token": "test-refresh-token",
            "account_id": "test-account",
            "auth_mode": "chatgpt"
        }
    });
    fs::write(
        codexff_lib::codex_config::codex_auth_path(),
        auth.to_string(),
    )
    .unwrap();
}

/// 造一份官方 config.toml
fn seed_official_config() {
    fs::write(
        codexff_lib::codex_config::codex_config_path(),
        "model = \"gpt-5.2-codex\"\nmodel_provider = \"openai\"\n",
    )
    .unwrap();
}

fn auth_text() -> String {
    fs::read_to_string(codexff_lib::codex_config::codex_auth_path()).unwrap()
}

/// 简化构造供应商输入 (测试用)
fn relay_input(name: &str, base_url: &str, model: &str, key: &str) -> profiles::RelayProfileInput {
    profiles::RelayProfileInput {
        name: name.to_string(),
        base_url: base_url.to_string(),
        model: model.to_string(),
        wire_api: None,
        key: Some(key.to_string()),
        model_reasoning_effort: None,
        disable_response_storage: true,
        model_context_window: None,
        model_auto_compact_token_limit: None,
        notes: None,
        website_url: None,
        auth_json: None,
        config_toml: None,
        anthropic_auth_field: None,
        use_common_config: false,
        usage_script: None,
        usage_api_key: None,
        usage_base_url: None,
        usage_access_token: None,
        usage_user_id: None,
        usage_timeout_secs: None,
    }
}

#[test]
fn relay_activation_seals_official_credentials() {
    let (_dir, _guard) = temp_env("relay_seal");
    seed_official_auth();
    seed_official_config();

    // 添加中转 profile
    let profile = profiles::add_relay_profile(profiles::RelayProfileInput {
        name: "test-relay".to_string(),
        base_url: "https://relay.example.com/v1".to_string(),
        model: "gpt-5.2-codex".to_string(),
        wire_api: Some("responses".to_string()),
        key: Some("sk-relay-key-123".to_string()),
        model_reasoning_effort: None,
        disable_response_storage: true,
        model_context_window: None,
        model_auto_compact_token_limit: None,
        notes: None,
        website_url: None,
        auth_json: None,
        config_toml: None,
        anthropic_auth_field: None,
        use_common_config: false,
        usage_script: None,
        usage_api_key: None,
        usage_base_url: None,
        usage_access_token: None,
        usage_user_id: None,
        usage_timeout_secs: None,
    })
    .expect("add relay");

    // 激活中转
    profiles::activate_relay(&profile.id).expect("activate relay");

    // 1. auth.json 物理移除官方凭证
    let auth: serde_json::Value = serde_json::from_str(&auth_text()).unwrap();
    assert!(
        auth.get("ChatGPT").is_none(),
        "中转模式下 auth.json 不得有官方凭证: {auth}"
    );
    // 2. auth.json 是中转 key
    assert_eq!(
        auth.get("OPENAI_API_KEY").and_then(|v| v.as_str()),
        Some("sk-relay-key-123")
    );

    // 3. config.toml 指向共享 custom 桶 (中转形态), 官方/中转互切续聊互通
    let config = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
    assert!(
        config.contains("model_provider = \"custom\""),
        "config: {config}"
    );
    assert!(config.contains("codexff_relay"));
    assert!(config.contains("https://relay.example.com/v1"));

    // 4. 官方凭证已入 vault
    assert!(vault::restore_has_credentials());

    // 5. 会话目录不受影响 (切换不碰 sessions)
    assert!(!codexff_lib::codex_config::codex_config_path()
        .to_str()
        .unwrap()
        .contains("sessions"));

    // 清理 keyring
    vault::delete_relay_key(&profile.id).unwrap();
}

#[test]
fn official_activation_restores_credentials() {
    let (_dir, _guard) = temp_env("official_restore");
    seed_official_auth();
    seed_official_config();

    let profile = profiles::add_relay_profile(relay_input(
        "test-relay",
        "https://relay.example.com/v1",
        "",
        "sk-relay-key-123",
    ))
    .expect("add relay");

    profiles::activate_relay(&profile.id).expect("activate relay");
    // 中转模式: 官方凭证不在 auth.json
    assert!(!auth_text().contains("test-access-token"));

    // 切回官方
    profiles::activate_official().expect("activate official");

    // 1. 官方凭证恢复
    let auth: serde_json::Value = serde_json::from_str(&auth_text()).unwrap();
    assert_eq!(
        auth.get("ChatGPT")
            .and_then(|v| v.get("access_token"))
            .and_then(|v| v.as_str()),
        Some("test-access-token")
    );

    // 2. config 回到官方形态的共享 custom 桶 (统一会话历史)
    let config = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
    assert!(config.contains("model_provider = \"custom\""));
    assert!(config.contains("name = \"OpenAI\""));
    assert!(config.contains("requires_openai_auth"));
    assert!(config.contains("supports_websockets"));
    // 3. 中转痕迹清掉 (relay 表/base_url/标记), 用户自己的 model 字段保留
    assert!(!config.contains("codexff_relay"));
    assert!(!config.contains("relay.example.com"));
    assert!(config.contains("gpt-5.2-codex"));

    vault::delete_relay_key(&profile.id).unwrap();
}

#[test]
fn relay_activation_without_key_fails_cleanly() {
    let (_dir, _guard) = temp_env("no_key");
    seed_official_auth();
    seed_official_config();

    let profile = profiles::add_relay_profile(relay_input(
        "no-key-relay",
        "https://relay.example.com/v1",
        "",
        "sk-key",
    ))
    .expect("add relay");
    // 模拟 key 丢失
    vault::delete_relay_key(&profile.id).unwrap();

    let result = profiles::activate_relay(&profile.id);
    assert!(result.is_err(), "无 key 必须拒绝激活");

    // 官方凭证未丢失, 配置未变
    assert!(auth_text().contains("test-access-token"));
    let config = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
    assert!(config.contains("model_provider = \"openai\""));
}

#[test]
fn user_config_preserved_across_switches() {
    let (_dir, _guard) = temp_env("preserve");
    // 用户自定义字段
    fs::write(
        codexff_lib::codex_config::codex_config_path(),
        "model = \"gpt-5.2-codex\"\nmodel_provider = \"openai\"\n\n[experimental]\ntimeout = 60\n",
    )
    .unwrap();
    seed_official_auth();

    let profile = profiles::add_relay_profile(relay_input(
        "test-relay",
        "https://relay.example.com/v1",
        "",
        "sk-key",
    ))
    .unwrap();
    profiles::activate_relay(&profile.id).unwrap();

    let config = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
    assert!(config.contains("timeout = 60"), "用户字段被抹掉: {config}");

    profiles::activate_official().unwrap();
    let config = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
    assert!(config.contains("timeout = 60"), "用户字段被抹掉: {config}");

    vault::delete_relay_key(&profile.id).unwrap();
}

/// 防御: 极端场景 — auth.json 损坏时切换干净失败, 不 panic、不写任何东西
#[test]
fn corrupted_auth_does_not_panic() {
    let (_dir, _guard) = temp_env("corrupt");
    fs::write(
        codexff_lib::codex_config::codex_auth_path(),
        "{ invalid json !!!",
    )
    .unwrap();

    let profile = profiles::add_relay_profile(relay_input(
        "test-relay",
        "https://relay.example.com/v1",
        "",
        "sk-key",
    ))
    .unwrap();
    // 损坏 auth.json: seal 解析失败 → 拒绝切换, 不 panic, 不残留任何改动
    let result = profiles::activate_relay(&profile.id);
    assert!(result.is_err(), "损坏 auth 必须拒绝切换");
    let auth = fs::read_to_string(codexff_lib::codex_config::codex_auth_path()).unwrap();
    assert_eq!(auth, "{ invalid json !!!", "auth.json 不得被改动");
    // 测试没建 config.toml — 文件不存在即未被改写
    if codexff_lib::codex_config::codex_config_path().exists() {
        let config = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
        assert!(
            !config.contains("codexff_relay"),
            "config 不得被改写成中转形态: {config}"
        );
    }

    vault::delete_relay_key(&profile.id).unwrap();
}

/// relay→relay 切换不得覆盖官方快照 — 否则切回官方还原成 relay 的字段
#[test]
fn relay_to_relay_keeps_official_snapshot() {
    let (_dir, _guard) = temp_env("relay_snap");
    fs::write(
        codexff_lib::codex_config::codex_config_path(),
        "model = \"official-model\"\nmodel_provider = \"openai\"\n",
    )
    .unwrap();
    seed_official_auth();

    let a = profiles::add_relay_profile(profiles::RelayProfileInput {
        name: "relay-a".to_string(),
        base_url: "https://relay-a.example.com/v1".to_string(),
        model: "relay-a-model".to_string(),
        wire_api: None,
        key: Some("sk-a".to_string()),
        model_reasoning_effort: Some("high".to_string()),
        disable_response_storage: true,
        model_context_window: None,
        model_auto_compact_token_limit: None,
        notes: None,
        website_url: None,
        auth_json: None,
        config_toml: None,
        anthropic_auth_field: None,
        use_common_config: false,
        usage_script: None,
        usage_api_key: None,
        usage_base_url: None,
        usage_access_token: None,
        usage_user_id: None,
        usage_timeout_secs: None,
    })
    .unwrap();
    let b = profiles::add_relay_profile(profiles::RelayProfileInput {
        name: "relay-b".to_string(),
        base_url: "https://relay-b.example.com/v1".to_string(),
        model: "relay-b-model".to_string(),
        wire_api: None,
        key: Some("sk-b".to_string()),
        model_reasoning_effort: None,
        disable_response_storage: false,
        model_context_window: None,
        model_auto_compact_token_limit: None,
        notes: None,
        website_url: None,
        auth_json: None,
        config_toml: None,
        anthropic_auth_field: None,
        use_common_config: false,
        usage_script: None,
        usage_api_key: None,
        usage_base_url: None,
        usage_access_token: None,
        usage_user_id: None,
        usage_timeout_secs: None,
    })
    .unwrap();

    profiles::activate_relay(&a.id).unwrap();
    profiles::activate_relay(&b.id).unwrap(); // relay→relay: 快照必须还是官方的

    profiles::activate_official().unwrap();
    let config = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
    assert!(
        config.contains("official-model"),
        "应还原官方 model: {config}"
    );
    assert!(!config.contains("relay-a-model"));
    assert!(!config.contains("relay-b-model"));

    vault::delete_relay_key(&a.id).unwrap();
    vault::delete_relay_key(&b.id).unwrap();
}

/// 官方激活拒绝非 CodexFF 管理的 custom 表 (防官方流量误路由)
#[test]
fn official_activation_refuses_unknown_custom_table() {
    let (_dir, _guard) = temp_env("refuse");
    fs::write(
        codexff_lib::codex_config::codex_config_path(),
        "model_provider = \"custom\"\n\n[model_providers.custom]\nname = \"Manual\"\nbase_url = \"https://evil.example.com\"\n",
    )
    .unwrap();

    let result = profiles::activate_official();
    assert!(result.is_err(), "未知 custom 表必须拒绝接管");
    let config = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
    assert!(
        config.contains("evil.example.com"),
        "config 不得被改写: {config}"
    );
}

/// 编辑激活中的 profile 换 key → auth.json 同步换新 key (codex 不能拿旧 key)
#[test]
fn update_active_relay_key_refreshes_auth() {
    let (_dir, _guard) = temp_env("update_key");
    seed_official_auth();
    seed_official_config();

    let p = profiles::add_relay_profile(relay_input(
        "relay-uk",
        "https://uk.example.com/v1",
        "",
        "sk-old-key",
    ))
    .unwrap();
    profiles::activate_relay(&p.id).unwrap();
    assert!(auth_text().contains("sk-old-key"));

    profiles::update_relay_profile(
        &p.id,
        profiles::RelayProfileInput {
            name: "relay-uk".to_string(),
            base_url: "https://uk.example.com/v1".to_string(),
            model: "".to_string(),
            wire_api: None,
            key: Some("sk-new-key".to_string()),
            model_reasoning_effort: None,
            disable_response_storage: true,
            model_context_window: None,
            model_auto_compact_token_limit: None,
            notes: None,
            website_url: None,
            auth_json: None,
            config_toml: None,
            anthropic_auth_field: None,
            use_common_config: false,
            usage_script: None,
            usage_api_key: None,
            usage_base_url: None,
            usage_access_token: None,
            usage_user_id: None,
            usage_timeout_secs: None,
        },
    )
    .unwrap();

    let auth: serde_json::Value = serde_json::from_str(&auth_text()).unwrap();
    assert_eq!(
        auth.get("OPENAI_API_KEY").and_then(|v| v.as_str()),
        Some("sk-new-key"),
        "激活中换 key 必须同步写 auth.json"
    );

    vault::delete_relay_key(&p.id).unwrap();
}

/// 我们写的中转 auth.json 带归属标记; seal 认识标记, 不再靠形状猜测
#[test]
fn relay_auth_marker_roundtrip() {
    let (_dir, _guard) = temp_env("marker");
    seed_official_auth();
    seed_official_config();

    let p = profiles::add_relay_profile(relay_input(
        "relay-mk",
        "https://mk.example.com/v1",
        "",
        "sk-marker",
    ))
    .unwrap();
    profiles::activate_relay(&p.id).unwrap();

    let auth: serde_json::Value = serde_json::from_str(&auth_text()).unwrap();
    assert_eq!(
        auth.get("codexff_relay_key").and_then(|v| v.as_bool()),
        Some(true),
        "中转 auth.json 必须带归属标记"
    );

    // relay→relay: seal 识别标记 → 不备份 → 官方备份不被中转 key 冲掉
    let p2 = profiles::add_relay_profile(relay_input(
        "relay-mk2",
        "https://mk2.example.com/v1",
        "",
        "sk-marker2",
    ))
    .unwrap();
    profiles::activate_relay(&p2.id).unwrap();

    // 官方凭证备份仍在 vault, 内容还是官方凭证 (ChatGPT 形态)
    assert!(vault::restore_has_credentials());

    profiles::activate_official().unwrap();
    let auth: serde_json::Value = serde_json::from_str(&auth_text()).unwrap();
    assert_eq!(
        auth.get("ChatGPT")
            .and_then(|v| v.get("access_token"))
            .and_then(|v| v.as_str()),
        Some("test-access-token"),
        "官方凭证必须完好"
    );

    vault::delete_relay_key(&p.id).unwrap();
    vault::delete_relay_key(&p2.id).unwrap();
}

/// 会话详情: 路径穿越拒绝 + archived 会话按归档根解析
#[test]
fn session_detail_path_safety_and_archived() {
    let (_dir, _guard) = temp_env("session_path");
    let codex = _dir.path().join("codex");
    fs::create_dir_all(codex.join("sessions")).unwrap();
    fs::create_dir_all(codex.join("archived_sessions")).unwrap();
    fs::write(
        codex.join("sessions/abc.jsonl"),
        r#"{"type":"session_meta","payload":{"title":"t1","model":"m1"}}"#,
    )
    .unwrap();
    fs::write(
        codex.join("archived_sessions/old.jsonl"),
        r#"{"type":"session_meta","payload":{"title":"t2","model":"m2"}}"#,
    )
    .unwrap();
    // 造一个 sessions 目录外的文件, 供穿越测试
    fs::write(codex.join("secret.json"), "{}").unwrap();

    let d = codexff_lib::session_manager::session_detail("abc.jsonl", 10).unwrap();
    assert_eq!(d.len(), 1, "sessions 根下正常读取");
    let d = codexff_lib::session_manager::session_detail("old.jsonl", 10).unwrap();
    assert_eq!(d.len(), 1, "archived_sessions 根下正常读取");
    assert!(codexff_lib::session_manager::session_detail("../config.toml", 10).is_err());
    assert!(codexff_lib::session_manager::session_detail("a/../secret.json", 10).is_err());
    assert!(codexff_lib::session_manager::session_detail("/etc/hosts", 10).is_err());
}

/// 用户自定义 auth.json + config.toml: 切换时整份写入 (cc-switch 对齐),
/// 只强制 custom 桶 + 注入缺失 relay 表; 其余用户全控
#[test]
fn custom_auth_and_config_respected() {
    let (_dir, _guard) = temp_env("custom_cfg");
    seed_official_auth();
    seed_official_config();

    let mut input = relay_input(
        "custom-relay",
        "https://custom.example.com/v1",
        "gpt-5.2-codex",
        "sk-custom",
    );
    input.auth_json = Some(
        r#"{"OPENAI_API_KEY":"sk-custom","BASE_URL":"https://custom.example.com","extra":1}"#
            .to_string(),
    );
    input.config_toml = Some(
        "model = \"gpt-5.6-codex\"\nmodel_reasoning_effort = \"max\"\nenable_goal_mode = true\n\n[experimental]\ntimeout = 42\n"
            .to_string(),
    );
    let p = profiles::add_relay_profile(input).unwrap();
    profiles::activate_relay(&p.id).unwrap();

    // auth.json: 用户自定义内容整份写入 + 归属标记注入
    let auth: serde_json::Value = serde_json::from_str(&auth_text()).unwrap();
    assert_eq!(
        auth.get("OPENAI_API_KEY").and_then(|v| v.as_str()),
        Some("sk-custom")
    );
    assert_eq!(
        auth.get("BASE_URL").and_then(|v| v.as_str()),
        Some("https://custom.example.com")
    );
    assert_eq!(auth.get("extra"), Some(&serde_json::json!(1)));
    assert_eq!(
        auth.get("codexff_relay_key").and_then(|v| v.as_bool()),
        Some(true),
        "必须注入归属标记"
    );

    // config.toml: 用户底稿保留全部字段 + 强制 custom 桶 + 注入 relay 表
    let config = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
    assert!(
        config.contains("model_provider = \"custom\""),
        "config: {config}"
    );
    assert!(
        config.contains("model_reasoning_effort = \"max\""),
        "用户 effort 被抹掉: {config}"
    );
    assert!(
        config.contains("enable_goal_mode = true"),
        "用户字段被抹掉: {config}"
    );
    assert!(config.contains("timeout = 42"), "用户字段被抹掉: {config}");
    assert!(
        config.contains("https://custom.example.com/v1"),
        "relay 表必须注入: {config}"
    );
    assert!(config.contains("codexff_relay"), "config: {config}");

    vault::delete_relay_key(&p.id).unwrap();
}

/// 中转 auth.json 不得含官方 ChatGPT 凭证 (隔离承诺) — 保存即拒绝
#[test]
fn relay_auth_rejects_official_credentials() {
    let (_dir, _guard) = temp_env("reject_oauth");
    seed_official_auth();
    seed_official_config();

    let mut input = relay_input("bad-relay", "https://bad.example.com/v1", "", "sk-bad");
    input.auth_json =
        Some(r#"{"OPENAI_API_KEY":"sk-bad","ChatGPT":{"access_token":"stolen"}}"#.to_string());
    let result = profiles::add_relay_profile(input);
    assert!(result.is_err(), "含 ChatGPT 凭证必须拒绝保存");
}

/// 公共配置片段: 保存 profile 时合并; 更新片段后启用 profile 自动跟随
#[test]
fn common_config_snippet_merges() {
    let (_dir, _guard) = temp_env("common_cfg");
    seed_official_auth();
    seed_official_config();

    // 先设公共片段
    profiles::set_common_config("model_reasoning_effort = \"high\"\nenable_goal_mode = true\n")
        .unwrap();

    // 启用公共片段的 profile: config_toml 自动合并
    let mut input = relay_input(
        "common-relay",
        "https://common.example.com/v1",
        "gpt-5.2-codex",
        "sk-common",
    );
    input.use_common_config = true;
    input.config_toml = Some("model = \"gpt-5.6-codex\"\n".to_string());
    let p = profiles::add_relay_profile(input).unwrap();
    let stored = profiles::load_profiles().unwrap();
    let saved = stored.relays.iter().find(|r| r.id == p.id).unwrap();
    let cfg = saved.config_toml.as_deref().unwrap();
    assert!(
        cfg.contains("model_reasoning_effort = \"high\""),
        "公共片段没合并: {cfg}"
    );
    assert!(
        cfg.contains("enable_goal_mode = true"),
        "公共片段没合并: {cfg}"
    );
    assert!(
        cfg.contains("model = \"gpt-5.6-codex\""),
        "用户字段丢失: {cfg}"
    );

    // 切换: 合并后的 config 生效
    profiles::activate_relay(&p.id).unwrap();
    let live = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
    assert!(
        live.contains("model_reasoning_effort = \"high\""),
        "live: {live}"
    );
    assert!(live.contains("enable_goal_mode = true"), "live: {live}");

    // 更新公共片段 → 启用 profile 自动重合并
    profiles::set_common_config("model_reasoning_effort = \"max\"\n").unwrap();
    let stored = profiles::load_profiles().unwrap();
    let saved = stored.relays.iter().find(|r| r.id == p.id).unwrap();
    let cfg = saved.config_toml.as_deref().unwrap();
    assert!(
        cfg.contains("model_reasoning_effort = \"max\""),
        "片段更新未跟随: {cfg}"
    );
    assert!(
        cfg.contains("enable_goal_mode = true"),
        "旧片段字段应保留 (合并非覆盖): {cfg}"
    );

    vault::delete_relay_key(&p.id).unwrap();
}

/// 一键导入 (cc-switch v3.8+ 带 config 参数): auth.json/config.toml 从
/// config 参数物化, 切换后原样写入
#[test]
fn import_with_config_param_materializes() {
    let (_dir, _guard) = temp_env("import_cfg");
    seed_official_auth();
    seed_official_config();

    let config_json = r#"{"auth":{"OPENAI_API_KEY":"sk-import"},"config":"model = \"gpt-5.6-codex\"\nenable_goal_mode = true\n"}"#;
    let b64 = general_purpose::STANDARD.encode(config_json);
    let url = format!(
        "ccswitch://v1/import?resource=provider&app=codex&name=ImportFull&endpoint=https%3A%2F%2Fimport.example.com%2Fv1&apiKey=sk-import&model=gpt-5.2-codex&config={b64}"
    );

    let p = profiles::import_from_text(&url).unwrap();
    let saved = profiles::load_profiles()
        .unwrap()
        .relays
        .into_iter()
        .find(|r| r.id == p.id)
        .unwrap();
    // 编辑表单展示物化内容 (用户投诉点: 导入后 auth.json/config.toml 有数据)
    let auth_json = saved.auth_json.as_deref().unwrap();
    assert!(auth_json.contains("sk-import"), "auth: {auth_json}");
    let cfg = saved.config_toml.as_deref().unwrap();
    assert!(
        cfg.contains("enable_goal_mode = true"),
        "用户 config 丢失: {cfg}"
    );
    assert!(
        cfg.contains("model = \"gpt-5.6-codex\""),
        "config 原样保留: {cfg}"
    );

    // 激活: 物化内容 + 强制约束 (custom 桶 + 归属标记) 写盘
    profiles::activate_relay(&p.id).unwrap();
    let auth: serde_json::Value = serde_json::from_str(&auth_text()).unwrap();
    assert_eq!(
        auth.get("OPENAI_API_KEY").and_then(|v| v.as_str()),
        Some("sk-import")
    );
    assert_eq!(
        auth.get("codexff_relay_key").and_then(|v| v.as_bool()),
        Some(true),
        "必须注入归属标记"
    );
    let live = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
    assert!(live.contains("model_provider = \"custom\""), "live: {live}");
    assert!(live.contains("enable_goal_mode = true"), "live: {live}");
    assert!(live.contains("import.example.com"), "relay 表注入: {live}");

    vault::delete_relay_key(&p.id).unwrap();
}

/// 一键导入 (无 config 参数): auth.json/config.toml 按表单字段物化
/// (同 cc-switch 导入后表单展示真实内容), 而非留空
#[test]
fn import_without_config_materializes_defaults() {
    let (_dir, _guard) = temp_env("import_plain");
    seed_official_auth();
    seed_official_config();

    let url = "ccswitch://v1/import?resource=provider&app=codex&name=ImportPlain&endpoint=https%3A%2F%2Fplain.example.com%2Fv1&apiKey=sk-plain&model=gpt-5.2-codex";
    let p = profiles::import_from_text(url).unwrap();

    let saved = profiles::load_profiles()
        .unwrap()
        .relays
        .into_iter()
        .find(|r| r.id == p.id)
        .unwrap();
    // auth.json 物化
    let auth_json = saved.auth_json.as_deref().unwrap();
    assert!(
        auth_json.contains("sk-plain"),
        "auth 必须物化 key: {auth_json}"
    );
    // config.toml 物化: 完整中转文档
    let cfg = saved.config_toml.as_deref().unwrap();
    assert!(cfg.contains("model = \"gpt-5.2-codex\""), "cfg: {cfg}");
    assert!(cfg.contains("plain.example.com"), "cfg: {cfg}");
    assert!(cfg.contains("codexff_relay = true"), "cfg: {cfg}");

    profiles::activate_relay(&p.id).unwrap();
    let live = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
    assert!(live.contains("plain.example.com"), "live: {live}");
    assert!(live.contains("model_provider = \"custom\""), "live: {live}");

    vault::delete_relay_key(&p.id).unwrap();
}

/// 导入的 config 里藏官方 ChatGPT 凭证 → 保存即拒绝 (隔离承诺)
#[test]
fn import_rejects_official_creds_in_config() {
    let (_dir, _guard) = temp_env("import_creds");
    seed_official_auth();
    seed_official_config();

    let config_json = r#"{"auth":{"OPENAI_API_KEY":"sk-bad","ChatGPT":{"access_token":"stolen"}}}"#;
    let b64 = general_purpose::STANDARD.encode(config_json);
    let url = format!(
        "ccswitch://v1/import?resource=provider&app=codex&name=BadImport&endpoint=https%3A%2F%2Fbad.example.com%2Fv1&apiKey=sk-bad&config={b64}"
    );
    assert!(
        profiles::import_from_text(&url).is_err(),
        "config 含 ChatGPT 凭证必须拒绝导入"
    );
}

/// relay → relay 切换: [model_providers.custom] 表必须刷新为当前 profile,
/// 残留上一个 relay 的 base_url 会导致流量路由到旧中转
#[test]
fn relay_to_relay_refreshes_table() {
    let (_dir, _guard) = temp_env("relay_relay");
    seed_official_auth();
    seed_official_config();

    let a = relay_input(
        "relay-a",
        "https://a.example.com/v1",
        "gpt-5.2-codex",
        "sk-a",
    );
    let pa = profiles::add_relay_profile(a.clone()).unwrap();
    profiles::activate_relay(&pa.id).unwrap();

    let mut b = relay_input(
        "relay-b",
        "https://b.example.com/v1",
        "gpt-5.6-codex",
        "sk-b",
    );
    b.config_toml = Some("model = \"gpt-5.6-codex\"\n".to_string());
    let pb = profiles::add_relay_profile(b.clone()).unwrap();
    profiles::activate_relay(&pb.id).unwrap();

    let live = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
    assert!(live.contains("b.example.com"), "表必须刷新为 B: {live}");
    assert!(
        !live.contains("a.example.com"),
        "A 的 base_url 残留: {live}"
    );
    assert!(live.contains("codexff_relay"), "live: {live}");
    // 表单 model 覆盖底稿顶层 (cc-switch 单文档语义)
    assert!(live.contains("model = \"gpt-5.6-codex\""), "live: {live}");

    vault::delete_relay_key(&pa.id).unwrap();
    vault::delete_relay_key(&pb.id).unwrap();
}

/// key 轮换同步: 改了 key 但没动 auth.json textarea → 物化内容用新 key 重建,
/// 避免旧 key 被写回 auth.json
#[test]
fn key_rotation_rebuilds_materialized_auth() {
    let (_dir, _guard) = temp_env("key_rotate");
    seed_official_auth();
    seed_official_config();

    let url = "ccswitch://v1/import?resource=provider&app=codex&name=Rotate&endpoint=https%3A%2F%2Frot.example.com%2Fv1&apiKey=sk-old";
    let p = profiles::import_from_text(url).unwrap();

    // 只改 key, auth.json textarea 内容不动 (前端总会回传当前 textarea)
    let mut input = relay_input("Rotate", "https://rot.example.com/v1", "", "sk-new");
    input.auth_json = Some(
        profiles::load_profiles().unwrap().relays[0]
            .auth_json
            .clone()
            .unwrap(),
    );
    input.key = Some("sk-new".to_string());

    let updated = profiles::update_relay_profile(&p.id, input).unwrap();
    assert!(
        updated
            .auth_json
            .as_deref()
            .unwrap_or("")
            .contains("sk-new"),
        "物化 auth 必须跟随新 key"
    );

    profiles::activate_relay(&p.id).unwrap();
    let auth: serde_json::Value = serde_json::from_str(&auth_text()).unwrap();
    assert_eq!(
        auth.get("OPENAI_API_KEY").and_then(|v| v.as_str()),
        Some("sk-new"),
        "写盘必须用新 key, 不能回退旧 key"
    );

    vault::delete_relay_key(&p.id).unwrap();
}

fn _unused(_p: PathBuf) {}

/// keyring 卡死降级: 超时设 0ms → 强制走 vault 文件存储, 存取删全通
#[test]
fn relay_key_file_fallback_when_keyring_hangs() {
    let (_dir, _guard) = temp_env("key_fallback");
    std::env::set_var("CODEXFF_KEYRING_TIMEOUT_MS", "0");

    vault::set_relay_key("f1", "sk-file-key").unwrap();
    assert_eq!(
        vault::get_relay_key("f1").unwrap().as_deref(),
        Some("sk-file-key")
    );
    vault::delete_relay_key("f1").unwrap();
    assert_eq!(vault::get_relay_key("f1").unwrap(), None);

    std::env::remove_var("CODEXFF_KEYRING_TIMEOUT_MS");
}

/// usage script 执行: 脚本产出 request 配置 + extractor 提取余额
#[test]
fn usage_script_extracts_balance() {
    let (_dir, _guard) = temp_env("usage_script_test");
    std::env::set_var("CODEXFF_KEYRING_TIMEOUT_MS", "0");

    // 起一个本地 HTTP 服务模拟中转站余额 API
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        for stream in server.incoming().take(1) {
            let Ok(mut s) = stream else { continue };
            use std::io::{Read, Write};
            s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .ok();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let body = r#"{"remaining": 12.5, "unit": "CNY", "is_active": true}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let base = format!("http://127.0.0.1:{port}");
        let cfg = codexff_lib::balance::UsageScriptCfg {
            code: r#"({request: {url: "{{baseUrl}}/v1/usage", method: "GET", headers: {"Authorization": "Bearer {{apiKey}}"}}, extractor: function(response) {return {isValid: response?.is_active ?? true, remaining: response?.remaining, unit: response?.unit};}})"#.to_string(),
            api_key: None,
            base_url: None,
            access_token: None,
            user_id: None,
            timeout_secs: Some(5),
        };
        let info = codexff_lib::balance::query_usage_script("test", &base, "sk-test", &cfg).await;
        assert!(info.success, "余额查询失败: {:?}", info.error);
        assert_eq!(info.balance, Some(12.5));
        assert_eq!(info.currency.as_deref(), Some("CNY"));
    });
    handle.join().unwrap();
    std::env::remove_var("CODEXFF_KEYRING_TIMEOUT_MS");
}

/// DeepSeek 官方网关激活: 注入 model_catalog_json 字段 + 落盘官方 models.json
/// (没有它 Codex 不认识 deepseek-v4-flash, 桌面端模型选择器回退内置 gpt-5.6)
#[test]
fn deepseek_relay_injects_model_catalog() {
    let (_dir, _guard) = temp_env("deepseek_catalog");
    seed_official_auth();
    seed_official_config();

    let mut input = relay_input(
        "DeepSeek",
        "https://api.deepseek.com",
        "deepseek-v4-flash",
        "sk-deepseek-key-1",
    );
    input.wire_api = Some("responses".to_string());
    let profile = profiles::add_relay_profile(input).expect("add relay");

    profiles::activate_relay(&profile.id).expect("activate deepseek relay");

    let config = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
    assert!(
        config.contains("model_catalog_json = \"codexff-model-catalog.json\""),
        "DeepSeek relay 必须注入 model_catalog_json: {config}"
    );

    // models.json 落盘在 ~/.codex/ 下且与官方模板同源 (含 deepseek-v4-flash 元数据)
    let catalog_path =
        codexff_lib::codex_config::codex_config_dir().join("codexff-model-catalog.json");
    let catalog: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&catalog_path).unwrap()).unwrap();
    let models = catalog.get("models").and_then(|m| m.as_array()).unwrap();
    assert!(
        models
            .iter()
            .any(|m| m.get("slug").and_then(|s| s.as_str()) == Some("deepseek-v4-flash")),
        "catalog 必须声明 deepseek-v4-flash"
    );

    vault::delete_relay_key(&profile.id).unwrap();
}

/// 切回官方: 移除 CodexFF 的 model_catalog_json 字段 (sentinel), 但保留
/// 用户手写的 model_catalog_json (如 DeepSeek 官方一键脚本的 ~/.codex/models.json)
#[test]
fn official_switch_removes_only_our_catalog_field() {
    let (_dir, _guard) = temp_env("official_catalog_cleanup");
    seed_official_auth();
    seed_official_config();

    let mut input = relay_input(
        "DeepSeek",
        "https://api.deepseek.com",
        "deepseek-v4-flash",
        "sk-deepseek-key-2",
    );
    input.wire_api = Some("responses".to_string());
    let profile = profiles::add_relay_profile(input).expect("add relay");
    profiles::activate_relay(&profile.id).expect("activate deepseek relay");

    let config = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
    assert!(config.contains("model_catalog_json = \"codexff-model-catalog.json\""));

    // 用户手写一个 model_catalog_json (模拟官方脚本), 验证切官方后不被误删
    let config = config.replace(
        "model_catalog_json = \"codexff-model-catalog.json\"",
        "model_catalog_json = \"~/.codex/models.json\"",
    );
    fs::write(codexff_lib::codex_config::codex_config_path(), config).unwrap();

    profiles::activate_official().expect("activate official");

    let config = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
    assert!(
        config.contains("model_catalog_json = \"~/.codex/models.json\""),
        "用户手写的 model_catalog_json 不得被删: {config}"
    );
    assert!(!config.contains("codexff-model-catalog.json"));

    vault::delete_relay_key(&profile.id).unwrap();
}

/// DeepSeek → 普通中转 (OpenAI 兼容, 模型与官方一致): 清掉 DeepSeek catalog,
/// 模型选择器恢复官方内置模型列表
#[test]
fn switching_to_openai_compat_relay_restores_official_models() {
    let (_dir, _guard) = temp_env("relay_catalog_cleanup");
    seed_official_auth();
    seed_official_config();

    let mut ds = relay_input(
        "DeepSeek",
        "https://api.deepseek.com",
        "deepseek-v4-flash",
        "sk-deepseek-key-3",
    );
    ds.wire_api = Some("responses".to_string());
    let ds_profile = profiles::add_relay_profile(ds).expect("add deepseek");
    profiles::activate_relay(&ds_profile.id).expect("activate deepseek");

    let config = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
    assert!(config.contains("model_catalog_json = \"codexff-model-catalog.json\""));

    // 切到 OpenAI 兼容中转 (模型与官方一致) — 必须清掉 DeepSeek catalog 字段
    let mut openai_relay = relay_input(
        "OpenAI 中转",
        "https://relay.example.com/v1",
        "gpt-5.6",
        "sk-relay-key-456",
    );
    openai_relay.wire_api = Some("responses".to_string());
    let relay_profile = profiles::add_relay_profile(openai_relay).expect("add openai relay");
    profiles::activate_relay(&relay_profile.id).expect("activate openai relay");

    let config = fs::read_to_string(codexff_lib::codex_config::codex_config_path()).unwrap();
    assert!(
        !config.contains("codexff-model-catalog.json"),
        "OpenAI 兼容中转不得残留 DeepSeek catalog: {config}"
    );
    // 顶层 model 用中转的官方模型名
    assert!(config.contains("model = \"gpt-5.6\""), "config: {config}");

    vault::delete_relay_key(&ds_profile.id).unwrap();
    vault::delete_relay_key(&relay_profile.id).unwrap();
}
