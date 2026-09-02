//! Profile 管理与切换。
//!
//! 切换 = 配置写入 + 凭证双向搬运:
//! - 官方: config.toml → openai; vault 官方凭证 → ~/.codex/auth.json
//! - 中转: 官方凭证 seal 离场; config.toml → relay provider; keyring key → auth.json
//!
//! 顺序保证: 官方凭证先在 vault 落盘才删除源文件 (seal), 失败可回滚。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::codex_config::{self, CodexConfigError, CurrentProfile};
use crate::vault::{self, VaultError};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RelayProfile {
    pub id: String,
    pub name: String,
    pub base_url: String,
    /// 默认模型 — 写入 config.toml 顶层 model (cc-switch 同格式)
    pub model: String,
    /// wire_api: "chat" | "responses" | None (留空 = 让 codex 自动)
    pub wire_api: Option<String>,
    /// model_reasoning_effort: "low"|"medium"|"high" (可选)
    #[serde(default)]
    pub model_reasoning_effort: Option<String>,
    /// disable_response_storage: 中转站通常要求 (不把会话存官方服务器)
    #[serde(default = "default_true")]
    pub disable_response_storage: bool,
    /// 上下文窗口 (token, 写 [model_providers.custom].model_context_window;
    /// None = 不写, codex 默认 128k; 官方模型默认 400k)
    #[serde(default)]
    pub model_context_window: Option<u64>,
    /// 超限自动压缩阈值 (model_auto_compact_token_limit, 通常 90% 窗口)
    #[serde(default)]
    pub model_auto_compact_token_limit: Option<u64>,
    /// 是否已保存中转 key (key 本体在 keyring)
    #[serde(default)]
    pub has_key: bool,
    // ---- cc-switch 对齐字段 (供应商表单全量) ----
    /// 备注 (cc-switch provider.notes)
    #[serde(default)]
    pub notes: Option<String>,
    /// 官网链接 (cc-switch provider.websiteUrl)
    #[serde(default)]
    pub website_url: Option<String>,
    /// 用户自定义完整 auth.json 内容 (None = 自动生成 {OPENAI_API_KEY})
    /// 隔离守卫: 内容不得含官方 ChatGPT 凭证, 保存/切换时校验
    #[serde(default)]
    pub auth_json: Option<String>,
    /// 用户自定义 config.toml 底稿 (None = 程序化生成)。
    /// 切换时以它为底, 强制 model_provider="custom" + 注入缺失的
    /// [model_providers.custom] 表; 其余字段用户全控 (reasoning effort 等在此体现)
    #[serde(default)]
    pub config_toml: Option<String>,
    /// anthropic 格式的认证字段名 (ANTHROPIC_AUTH_TOKEN | ANTHROPIC_API_KEY)
    #[serde(default)]
    pub anthropic_auth_field: Option<String>,
    /// 保存时合并全局公共配置片段 (cc-switch writeCommonConfig)
    #[serde(default)]
    pub use_common_config: bool,
    // ---- 余额查询脚本 (cc-switch usage script, deeplink v3.9+ 导入携带) ----
    /// JS 脚本: ({request: {...}, extractor: function(response){...}})
    #[serde(default)]
    pub usage_script: Option<String>,
    /// 用量查询专用 API key (通用模板用, {{apiKey}} 替换)
    #[serde(default)]
    pub usage_api_key: Option<String>,
    /// 用量查询专用 base URL ({{baseUrl}} 替换)
    #[serde(default)]
    pub usage_base_url: Option<String>,
    /// 访问令牌 (new-api 模板用, {{accessToken}} 替换)
    #[serde(default)]
    pub usage_access_token: Option<String>,
    /// 用户 ID (new-api 模板用, {{userId}} 替换)
    #[serde(default)]
    pub usage_user_id: Option<String>,
    /// 脚本请求超时秒数
    #[serde(default)]
    pub usage_timeout_secs: Option<u64>,
    /// 该中转 /models 返回的真实模型列表（写入桌面端模型目录用）。
    /// 空 = 未获取/未知，切换时尝试在线拉取。
    #[serde(default)]
    pub supported_models: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProfilesError {
    #[error("vault 错误: {0}")]
    Vault(#[from] VaultError),
    #[error("codex 配置错误: {0}")]
    Codex(#[from] CodexConfigError),
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error("profile 不存在: {0}")]
    NotFound(String),
    #[error("切换失败, 已回滚: {0}")]
    RolledBack(String),
    #[error("操作被阻止: {0}")]
    Blocked(String),
    #[error("导入失败: {0}")]
    Import(#[from] crate::import_config::ImportError),
}

fn default_true() -> bool {
    true
}

/// 同步文件事务串行化。Tauri 层另有覆盖完整 async 切换流程的全局锁；
/// 此锁继续保护内部/测试/非 Tauri 调用，避免 config/auth/profiles 多文件
/// 写入交错。
static ACTIVATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Tauri 层在启动/接管会话兼容路由前保存的精确切换快照。
/// profile 自身的切换事务只能覆盖配置、凭证和会话迁移阶段；若随后路由
/// postcondition 失败，需要把已经提交的 profile 一并退回原状态。
#[derive(Debug, Clone)]
pub struct SwitchSnapshot {
    config: Option<Vec<u8>>,
    auth: Option<Vec<u8>>,
    model_catalog: Option<Vec<u8>>,
    active: Option<ActiveSelection>,
    relay_state: Option<Vec<u8>>,
}

pub fn capture_switch_snapshot() -> Result<SwitchSnapshot, ProfilesError> {
    let config_path = codex_config::codex_config_path();
    let auth_path = codex_config::codex_auth_path();
    Ok(SwitchSnapshot {
        config: config_path
            .exists()
            .then(|| std::fs::read(&config_path))
            .transpose()?,
        auth: auth_path
            .exists()
            .then(|| std::fs::read(&auth_path))
            .transpose()?,
        model_catalog: {
            let path =
                codex_config::codex_config_dir().join(codex_config::CODEXFF_MODEL_CATALOG_FILENAME);
            path.exists().then(|| std::fs::read(path)).transpose()?
        },
        active: load_profiles()?.active,
        relay_state: {
            let path = vault::vault_dir().join(vault::RELAY_STATE_FILENAME);
            path.exists().then(|| std::fs::read(path)).transpose()?
        },
    })
}

fn restore_optional_file(
    path: &std::path::Path,
    bytes: &Option<Vec<u8>>,
) -> Result<(), ProfilesError> {
    match bytes {
        Some(bytes) => vault::atomic_write_bytes(path, bytes)?,
        None if path.exists() => std::fs::remove_file(path)?,
        None => {}
    }
    Ok(())
}

fn verify_profile_secrets(
    profile_id: &str,
    expected: &[(&str, Option<&str>)],
) -> Result<(), String> {
    let mut errors = Vec::new();
    for (kind, expected) in expected {
        let expected = expected.map(str::trim).filter(|value| !value.is_empty());
        match vault::get_profile_secret(profile_id, kind) {
            Ok(actual) if actual.as_deref() == expected => {}
            Ok(actual) => errors.push(format!(
                "{kind}: 回滚后内容不一致（期望存在={}，实际存在={}）",
                expected.is_some(),
                actual.is_some()
            )),
            Err(e) => errors.push(format!("{kind}: 回滚后校验失败: {e}")),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// 兼容路由接管失败后的外层事务回滚。先恢复磁盘的 config/auth/active，
/// 再按恢复后的 provider 形态纠正会话隔离位置。
pub fn restore_switch_snapshot(snapshot: &SwitchSnapshot) -> Result<(), ProfilesError> {
    let _guard = ACTIVATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut errors = Vec::new();
    if let Err(e) = restore_optional_file(&codex_config::codex_config_path(), &snapshot.config) {
        errors.push(format!("config: {e}"));
    }
    if let Err(e) = restore_optional_file(&codex_config::codex_auth_path(), &snapshot.auth) {
        errors.push(format!("auth: {e}"));
    }
    if let Err(e) = restore_optional_file(
        &codex_config::codex_config_dir().join(codex_config::CODEXFF_MODEL_CATALOG_FILENAME),
        &snapshot.model_catalog,
    ) {
        errors.push(format!("model catalog: {e}"));
    }
    match load_profiles() {
        Ok(mut profiles) => {
            profiles.active = snapshot.active.clone();
            if let Err(e) = save_profiles(&profiles) {
                errors.push(format!("active selection: {e}"));
            }
        }
        Err(e) => errors.push(format!("load profiles: {e}")),
    }
    if let Err(e) = restore_optional_file(
        &vault::vault_dir().join(vault::RELAY_STATE_FILENAME),
        &snapshot.relay_state,
    ) {
        errors.push(format!("relay state: {e}"));
    }
    let auth_validation = match &snapshot.active {
        Some(ActiveSelection::Relay { .. }) => vault::validate_relay_auth_file(),
        Some(ActiveSelection::Official) => vault::validate_official_auth_file(),
        None => Ok(()),
    };
    if let Err(e) = auth_validation {
        errors.push(format!("auth validation: {e}"));
    }
    let verify_file =
        |label: &str, path: &std::path::Path, expected: &Option<Vec<u8>>| -> Option<String> {
            match path.exists().then(|| std::fs::read(path)).transpose() {
                Ok(actual) if &actual == expected => None,
                Ok(_) => Some(format!("{label}: 回滚后内容与快照不一致")),
                Err(e) => Some(format!("{label}: 回滚后校验失败: {e}")),
            }
        };
    if let Some(e) = verify_file(
        "config",
        &codex_config::codex_config_path(),
        &snapshot.config,
    ) {
        errors.push(e);
    }
    if let Some(e) = verify_file("auth", &codex_config::codex_auth_path(), &snapshot.auth) {
        errors.push(e);
    }
    if let Some(e) = verify_file(
        "model catalog",
        &codex_config::codex_config_dir().join(codex_config::CODEXFF_MODEL_CATALOG_FILENAME),
        &snapshot.model_catalog,
    ) {
        errors.push(e);
    }
    if let Some(e) = verify_file(
        "relay state",
        &vault::vault_dir().join(vault::RELAY_STATE_FILENAME),
        &snapshot.relay_state,
    ) {
        errors.push(e);
    }
    match load_profiles() {
        Ok(profiles) if profiles.active == snapshot.active => {}
        Ok(_) => errors.push("active selection: 回滚后与快照不一致".into()),
        Err(e) => errors.push(format!("active selection: 回滚后校验失败: {e}")),
    }
    if !errors.is_empty() {
        return Err(ProfilesError::RolledBack(errors.join("; ")));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilesFile {
    pub relays: Vec<RelayProfile>,
    #[serde(default)]
    pub active: Option<ActiveSelection>,
    /// 全局 Codex 公共配置片段 (cc-switch common config snippet):
    /// 启用 use_common_config 的 profile 保存时合并进自身 config_toml
    #[serde(default)]
    pub codex_common_config: Option<String>,
}

pub fn get_common_config() -> Result<Option<String>, ProfilesError> {
    Ok(load_profiles()?.codex_common_config)
}

/// 更新公共片段 + 重新合并所有启用它的 profile (cc-switch 同行为:
/// 片段变更后勾选 provider 的 config 立即跟随)
pub fn set_common_config(snippet: &str) -> Result<(), ProfilesError> {
    let mut profiles = load_profiles()?;
    let previous_profiles = profiles.clone();
    let previous_common = vault::get_secret("codex-common-config")?;
    let previous_configs = profiles
        .relays
        .iter()
        .filter(|profile| profile.use_common_config)
        .map(|profile| {
            Ok((
                profile.id.clone(),
                vault::get_profile_secret(&profile.id, "config-toml")?,
            ))
        })
        .collect::<Result<Vec<_>, ProfilesError>>()?;
    let snippet = snippet.trim().to_string();
    profiles.codex_common_config = if snippet.is_empty() {
        None
    } else {
        // 提前校验 TOML 合法性
        snippet
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| ProfilesError::Codex(CodexConfigError::TomlParse(e.to_string())))?;
        Some(snippet.clone())
    };
    // 重新合并所有启用公共片段的 profile
    for p in &mut profiles.relays {
        if p.use_common_config {
            if let Some(cfg) = p.config_toml.take() {
                p.config_toml = Some(merge_common_snippet(&cfg, &snippet));
            } else {
                p.config_toml = Some(snippet.clone());
            }
            if let Err(e) =
                vault::set_profile_secret(&p.id, "config-toml", p.config_toml.as_deref())
            {
                rollback_common_config(&previous_profiles, &previous_common, &previous_configs);
                return Err(ProfilesError::RolledBack(format!(
                    "更新公共配置机密失败: {e}"
                )));
            }
        }
    }
    let common_result = match profiles.codex_common_config.as_deref() {
        Some(value) => vault::set_secret("codex-common-config", value),
        None => vault::delete_secret("codex-common-config"),
    };
    if let Err(e) = common_result {
        rollback_common_config(&previous_profiles, &previous_common, &previous_configs);
        return Err(ProfilesError::RolledBack(format!(
            "更新公共配置机密失败: {e}"
        )));
    }
    if let Err(e) = save_profiles(&profiles) {
        rollback_common_config(&previous_profiles, &previous_common, &previous_configs);
        return Err(ProfilesError::RolledBack(format!("保存公共配置失败: {e}")));
    }
    Ok(())
}

fn rollback_common_config(
    previous_profiles: &ProfilesFile,
    previous_common: &Option<String>,
    previous_configs: &[(String, Option<String>)],
) {
    for (id, value) in previous_configs {
        let _ = vault::set_profile_secret(id, "config-toml", value.as_deref());
    }
    match previous_common {
        Some(value) => {
            let _ = vault::set_secret("codex-common-config", value);
        }
        None => {
            let _ = vault::delete_secret("codex-common-config");
        }
    }
    let _ = save_profiles(previous_profiles);
}

/// 公共片段合并进 config.toml: 顶层标量覆盖, 表递归合并 (cc-switch merge_toml_table_like)
fn merge_common_snippet(config_toml: &str, snippet: &str) -> String {
    let mut target = config_toml
        .parse::<toml_edit::DocumentMut>()
        .unwrap_or_default();
    let Ok(source) = snippet.parse::<toml_edit::DocumentMut>() else {
        return config_toml.to_string();
    };
    merge_table(target.as_table_mut(), source.as_table());
    target.to_string()
}

fn merge_table(target: &mut toml_edit::Table, source: &toml_edit::Table) {
    for (key, item) in source.iter() {
        let item = item.clone();
        match item
            .as_table()
            .zip(target.get_mut(key).and_then(|t| t.as_table_mut()))
        {
            // 两边都是表 → 递归合并 (保留用户目标表里片段没写的键)
            Some((src_t, tgt_t)) => merge_table(tgt_t, src_t),
            _ => {
                target.insert(key, item);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActiveSelection {
    Official,
    Relay { profile_id: String },
}

fn profiles_path() -> PathBuf {
    if let Ok(dir) = std::env::var("CODEXFF_DATA_DIR") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join("profiles.json");
        }
    }
    dirs::data_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
        .join("codexff")
        .join("profiles.json")
}

pub fn load_profiles() -> Result<ProfilesFile, ProfilesError> {
    let path = profiles_path();
    if !path.exists() {
        return Ok(ProfilesFile {
            relays: Vec::new(),
            active: None,
            codex_common_config: None,
        });
    }
    let text = std::fs::read_to_string(&path)?;
    let mut profiles: ProfilesFile = serde_json::from_str(&text)?;
    let mut migrated = false;
    if let Some(value) = profiles.codex_common_config.take() {
        vault::set_secret("codex-common-config", &value)?;
        migrated = true;
    }
    for profile in &mut profiles.relays {
        // 皮卡丘已停止提供 Luna；旧版本缓存的 /models 清单不能继续
        // 把它声明为可用，否则旧会话会被原样转发并返回 502/503。
        if profile.base_url.contains("sub.pikaqiu.shop") {
            let before = profile.supported_models.len();
            profile
                .supported_models
                .retain(|model| model != "gpt-5.6-luna");
            migrated |= before != profile.supported_models.len();
        }
        let canonical_wire = match profile.wire_api.as_deref() {
            Some("openai_responses") => Some("responses".to_string()),
            Some("openai_chat") => Some("chat".to_string()),
            other => other.map(str::to_string),
        };
        if canonical_wire != profile.wire_api {
            profile.wire_api = canonical_wire;
            migrated = true;
        }
        if let Some(value) = profile.auth_json.take() {
            vault::set_profile_secret(&profile.id, "auth-json", Some(&value))?;
            migrated = true;
        }
        if let Some(value) = profile.usage_api_key.take() {
            vault::set_profile_secret(&profile.id, "usage-api-key", Some(&value))?;
            migrated = true;
        }
        if let Some(value) = profile.usage_access_token.take() {
            vault::set_profile_secret(&profile.id, "usage-access-token", Some(&value))?;
            migrated = true;
        }
        if let Some(value) = profile.config_toml.take() {
            vault::set_profile_secret(&profile.id, "config-toml", Some(&value))?;
            migrated = true;
        }
        if let Some(value) = profile.usage_script.take() {
            vault::set_profile_secret(&profile.id, "usage-script", Some(&value))?;
            migrated = true;
        }
    }
    if migrated {
        save_profiles(&profiles)?;
    }
    for profile in &mut profiles.relays {
        profile.auth_json = vault::get_profile_secret(&profile.id, "auth-json")?;
        profile.usage_api_key = vault::get_profile_secret(&profile.id, "usage-api-key")?;
        profile.usage_access_token = vault::get_profile_secret(&profile.id, "usage-access-token")?;
        profile.config_toml = vault::get_profile_secret(&profile.id, "config-toml")?;
        profile.usage_script = vault::get_profile_secret(&profile.id, "usage-script")?;
    }
    profiles.codex_common_config = vault::get_secret("codex-common-config")?;
    Ok(profiles)
}

/// 会话归属账本使用的稳定账号标识：官方固定为 official，中转使用 profile_id。
/// 只记录本地 profile 标识，不记录 API key 或任何凭证内容。
pub fn active_account_marker() -> String {
    match load_profiles().ok().and_then(|p| p.active) {
        Some(ActiveSelection::Relay { profile_id }) => format!("relay:{profile_id}"),
        Some(ActiveSelection::Official) => "official".to_string(),
        None => "unknown".to_string(),
    }
}

fn save_profiles(profiles: &ProfilesFile) -> Result<(), ProfilesError> {
    let path = profiles_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let mut disk = profiles.clone();
    disk.codex_common_config = None;
    for profile in &mut disk.relays {
        profile.auth_json = None;
        profile.config_toml = None;
        profile.usage_script = None;
        profile.usage_api_key = None;
        profile.usage_access_token = None;
    }
    vault::atomic_write_bytes(&path, serde_json::to_string_pretty(&disk)?.as_bytes())?;
    Ok(())
}

pub fn list_relay_profiles() -> Result<Vec<RelayProfile>, ProfilesError> {
    Ok(load_profiles()?.relays)
}

/// 回填某个中转的模型清单（切换/预览时在线拉取成功后保存，
/// 下次切换不用再拉）。
pub fn update_relay_supported_models(
    profile_id: &str,
    models: Vec<String>,
) -> Result<(), ProfilesError> {
    let mut profiles = load_profiles()?;
    let Some(profile) = profiles.relays.iter_mut().find(|p| p.id == profile_id) else {
        return Err(ProfilesError::NotFound(profile_id.to_string()));
    };
    profile.supported_models = models
        .into_iter()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .filter(|m| !(profile.base_url.contains("sub.pikaqiu.shop") && m == "gpt-5.6-luna"))
        .collect();
    save_profiles(&profiles)
}

/// 当前激活状态 (按磁盘实际状态推断, 不信任 profiles.json)
pub fn current_active() -> Result<ActiveSelection, ProfilesError> {
    let saved = load_profiles()?.active;
    match codex_config::current_profile_kind()? {
        CurrentProfile::Official => Ok(ActiveSelection::Official),
        // 共享桶无法从 config 区分具体 relay, 用记录的 active 解析;
        // 记录丢失时不能假装是官方 (可能 auth.json 仍是中转 key), 返回未知。
        CurrentProfile::Relay => match saved {
            Some(ActiveSelection::Relay { profile_id }) => {
                Ok(ActiveSelection::Relay { profile_id })
            }
            _ => Err(ProfilesError::NotFound(
                "config 是中转形态但缺少 active profile 记录".into(),
            )),
        },
        CurrentProfile::None => {
            // 回退到记录的 active (可能用户手动改过 config)
            Ok(saved.unwrap_or(ActiveSelection::Official))
        }
    }
}

/// 供应商表单全量字段 (cc-switch 对齐), add/update 共用入参
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RelayProfileInput {
    pub name: String,
    pub base_url: String,
    /// 默认模型 (顶层 model)
    pub model: String,
    /// wire_api: "openai_chat" | "openai_responses" | "anthropic" (cc-switch 命名)
    pub wire_api: Option<String>,
    /// add 必填; update 传 Some("") = 不修改
    pub key: Option<String>,
    pub model_reasoning_effort: Option<String>,
    pub disable_response_storage: bool,
    /// add 可空; update None = 不修改
    pub model_context_window: Option<u64>,
    pub model_auto_compact_token_limit: Option<u64>,
    pub notes: Option<String>,
    pub website_url: Option<String>,
    /// 自定义完整 auth.json (None = 自动生成); 校验: JSON 可解析 + 不含官方 ChatGPT 凭证
    pub auth_json: Option<String>,
    /// 自定义 config.toml 底稿 (None = 程序化生成); 校验: TOML 可解析
    pub config_toml: Option<String>,
    /// anthropic 格式认证字段名
    pub anthropic_auth_field: Option<String>,
    /// 保存时合并全局公共片段
    pub use_common_config: bool,
    // ---- 余额查询脚本 (cc-switch usage script) ----
    pub usage_script: Option<String>,
    pub usage_api_key: Option<String>,
    pub usage_base_url: Option<String>,
    pub usage_access_token: Option<String>,
    pub usage_user_id: Option<String>,
    pub usage_timeout_secs: Option<u64>,
    /// add 保存测试到的模型列表; update None = 不修改
    pub supported_models: Option<Vec<String>>,
}

/// 归一化 + 校验供应商输入 (add/update 共用):
/// auth.json 必须可解析且不得含官方 ChatGPT 凭证 (中转模式隔离承诺);
/// config.toml 必须 TOML 可解析 (保存即暴露错误, 而非切换时才炸);
/// use_common_config → config_toml 合并公共片段
fn normalize_input(
    input: &RelayProfileInput,
    common: Option<&str>,
) -> Result<
    (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    ),
    ProfilesError,
> {
    let name = input.name.trim().to_string();
    let base_url = input.base_url.trim().trim_end_matches('/').to_string();
    let wire_api = input
        .wire_api
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|wire| match wire {
            "responses" | "openai_responses" => Ok("responses".to_string()),
            "chat" | "openai_chat" => Ok("chat".to_string()),
            "anthropic" => Ok("anthropic".to_string()),
            other => Err(ProfilesError::Codex(CodexConfigError::Refuse(format!(
                "不支持的 wire_api: {other}"
            )))),
        })
        .transpose()?;

    // auth.json 校验 (自定义时)
    let auth_json = input
        .auth_json
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(text) = &auth_json {
        let parsed: serde_json::Value = serde_json::from_str(text).map_err(|e| {
            ProfilesError::Codex(CodexConfigError::TomlParse(format!(
                "auth.json 不是合法 JSON: {e}"
            )))
        })?;
        // 隔离守卫: 中转 auth.json 不得含官方登录凭证 (旧 ChatGPT / 新版 tokens)
        if vault::contains_official_credentials(&parsed) {
            return Err(ProfilesError::Codex(CodexConfigError::Refuse(
                "中转 auth.json 不得包含官方登录凭证 (中转模式隔离承诺)。\
                 官方凭证请留在 CodexFF 金库, 切官方时自动恢复"
                    .into(),
            )));
        }
    }

    // config.toml 校验 + 公共片段合并
    let config_toml = input
        .config_toml
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let merged = if input.use_common_config {
        let common = common.unwrap_or("").trim();
        match &config_toml {
            Some(cfg) => Some(merge_common_snippet(cfg, common)),
            None => Some(common.to_string()),
        }
    } else {
        config_toml.clone()
    };
    let final_config = match merged {
        Some(m) if !m.trim().is_empty() => {
            m.parse::<toml_edit::DocumentMut>()
                .map_err(|e| ProfilesError::Codex(CodexConfigError::TomlParse(e.to_string())))?;
            Some(m)
        }
        _ => None,
    };

    Ok((name, base_url, wire_api, auth_json, final_config))
}

/// 一键导入: deeplink/JSON 文本 → 解析 config 参数 (cc-switch v3.8+) →
/// 物化 auth.json + config.toml (导入后编辑表单展示真实内容) → 建 profile。
/// apiKey 参数缺失时从 config 的 auth 里提取 (cc-switch 同: key 可只在 config)。
pub fn import_from_text(text: &str) -> Result<RelayProfile, ProfilesError> {
    use crate::import_config;

    let mut req = import_config::parse_import_text(text)?;
    let resolved = import_config::resolve_config(&req)?;
    // apiKey 参数缺失 → 从物化的 auth 提取 (中转站可能把 key 只放 config)
    if req
        .api_key
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        if let Some(key) = resolved
            .auth_json
            .as_deref()
            .and_then(import_config::derive_api_key)
        {
            req.api_key = Some(key);
        }
    }
    import_config::validate(&req)?;

    add_relay_profile(RelayProfileInput {
        name: req.name.unwrap_or_default(),
        base_url: req.endpoint.unwrap_or_default(),
        model: req.model.unwrap_or_default(),
        wire_api: req.wire_api,
        key: Some(req.api_key.unwrap_or_default()),
        model_reasoning_effort: None,
        disable_response_storage: true,
        model_context_window: None,
        model_auto_compact_token_limit: None,
        notes: req.notes,
        website_url: req.homepage,
        auth_json: resolved.auth_json,
        config_toml: Some(resolved.config_toml),
        anthropic_auth_field: None,
        use_common_config: false,
        // 余额查询脚本 (cc-switch usage script) — 导入链接携带时原样保存
        usage_script: req.usage_script,
        usage_api_key: req.usage_api_key,
        usage_base_url: req.usage_base_url,
        usage_access_token: req.usage_access_token,
        usage_user_id: req.usage_user_id,
        usage_timeout_secs: req.usage_auto_interval,
        supported_models: None,
    })
}

pub fn add_relay_profile(input: RelayProfileInput) -> Result<RelayProfile, ProfilesError> {
    let mut profiles = load_profiles()?;
    let (name, base_url, wire_api, auth_json, config_toml) =
        normalize_input(&input, profiles.codex_common_config.as_deref())?;
    let key = input.key.unwrap_or_default();
    if key.trim().is_empty() {
        return Err(ProfilesError::Codex(CodexConfigError::Refuse(
            "中转 key 不能为空".into(),
        )));
    }
    // 名称查重
    if profiles
        .relays
        .iter()
        .any(|p| p.name.eq_ignore_ascii_case(&name))
    {
        return Err(ProfilesError::NotFound(format!(
            "同名 profile 已存在: {name}"
        )));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let profile = RelayProfile {
        id,
        name,
        base_url,
        model: input.model.trim().to_string(),
        wire_api,
        model_reasoning_effort: input
            .model_reasoning_effort
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        disable_response_storage: input.disable_response_storage,
        has_key: true,
        notes: input
            .notes
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        website_url: input
            .website_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        auth_json,
        config_toml,
        anthropic_auth_field: input
            .anthropic_auth_field
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        use_common_config: input.use_common_config,
        model_context_window: input.model_context_window.filter(|w| *w > 0),
        model_auto_compact_token_limit: input.model_auto_compact_token_limit.filter(|l| *l > 0),
        usage_script: input
            .usage_script
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        usage_api_key: input
            .usage_api_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        usage_base_url: input
            .usage_base_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        usage_access_token: input
            .usage_access_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        usage_user_id: input
            .usage_user_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        usage_timeout_secs: input.usage_timeout_secs,
        supported_models: input
            .supported_models
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .collect(),
    };

    // 整组机密一次加密落盘，任一字段失败都不留下半份 profile。
    vault::set_profile_secrets(
        &profile.id,
        &[
            ("key", Some(key.trim())),
            ("auth-json", profile.auth_json.as_deref()),
            ("usage-api-key", profile.usage_api_key.as_deref()),
            ("usage-access-token", profile.usage_access_token.as_deref()),
            ("config-toml", profile.config_toml.as_deref()),
            ("usage-script", profile.usage_script.as_deref()),
        ],
    )?;

    profiles.relays.push(profile.clone());
    if let Err(e) = save_profiles(&profiles) {
        let _ = vault::delete_profile_secrets(&profile.id);
        return Err(e);
    }
    Ok(profile)
}

pub fn update_relay_profile(
    id: &str,
    input: RelayProfileInput,
) -> Result<RelayProfile, ProfilesError> {
    // 激活流程串行化: activate_*/update 的 was_active 重写会并发写
    // config.toml + auth.json (Tauri 命令可多线程并发), 无锁会交错撕裂
    let _guard = ACTIVATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut profiles = load_profiles()?;
    let Some(index) = profiles.relays.iter().position(|p| p.id == id) else {
        return Err(ProfilesError::NotFound(id.to_string()));
    };
    let previous_profile = profiles.relays[index].clone();
    let previous_key = vault::get_relay_key(id)?;
    let previous_config = codex_config::codex_config_path()
        .exists()
        .then(|| std::fs::read(codex_config::codex_config_path()))
        .transpose()?;
    let previous_auth = codex_config::codex_auth_path()
        .exists()
        .then(|| std::fs::read(codex_config::codex_auth_path()))
        .transpose()?;
    let was_active = matches!(
        profiles.active,
        Some(ActiveSelection::Relay { ref profile_id }) if profile_id == id
    );
    let (name, base_url, wire_api, auth_json, config_toml) =
        normalize_input(&input, profiles.codex_common_config.as_deref())?;

    let stored_auth = profiles.relays[index].auth_json.clone();
    // update: key=Some("") 或 None = 不修改
    let new_key = input
        .key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty());
    let key_updated = new_key.is_some();
    if key_updated {
        profiles.relays[index].has_key = true;
    }
    // key 轮换同步: key 变了且用户没动 auth.json textarea (内容与存储一致)
    // → 用新 key 重建物化内容, 否则写盘会把旧 key 写回 auth.json
    let mut auth_json = auth_json;
    if key_updated && auth_json.as_deref() == stored_auth.as_deref() {
        auth_json = stored_auth
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|mut v| {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert(
                        "OPENAI_API_KEY".to_string(),
                        serde_json::json!(input.key.as_deref().map(str::trim).unwrap_or("")),
                    );
                }
                serde_json::to_string_pretty(&v).ok()
            });
    }
    profiles.relays[index].name = name;
    profiles.relays[index].base_url = base_url;
    profiles.relays[index].model = input.model.trim().to_string();
    profiles.relays[index].wire_api = wire_api;
    profiles.relays[index].model_reasoning_effort = input
        .model_reasoning_effort
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    profiles.relays[index].disable_response_storage = input.disable_response_storage;
    // 上下文窗口: None = 不修改; Some(0) = 清空回默认 (前端留空发送 0)
    if let Some(w) = input.model_context_window {
        profiles.relays[index].model_context_window = (w > 0).then_some(w);
    }
    if let Some(l) = input.model_auto_compact_token_limit {
        profiles.relays[index].model_auto_compact_token_limit = (l > 0).then_some(l);
    }
    profiles.relays[index].notes = input
        .notes
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    profiles.relays[index].website_url = input
        .website_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    profiles.relays[index].auth_json = auth_json;
    profiles.relays[index].config_toml = config_toml;
    profiles.relays[index].anthropic_auth_field = input
        .anthropic_auth_field
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    profiles.relays[index].usage_script = input
        .usage_script
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    profiles.relays[index].usage_api_key = input
        .usage_api_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    profiles.relays[index].usage_base_url = input
        .usage_base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    profiles.relays[index].usage_access_token = input
        .usage_access_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    profiles.relays[index].usage_user_id = input
        .usage_user_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    profiles.relays[index].usage_timeout_secs = input.usage_timeout_secs;
    if let Some(models) = input.supported_models {
        profiles.relays[index].supported_models = models
            .into_iter()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .collect();
    }
    profiles.relays[index].use_common_config = input.use_common_config;
    let mut secret_updates = vec![
        ("auth-json", profiles.relays[index].auth_json.as_deref()),
        (
            "usage-api-key",
            profiles.relays[index].usage_api_key.as_deref(),
        ),
        (
            "usage-access-token",
            profiles.relays[index].usage_access_token.as_deref(),
        ),
        ("config-toml", profiles.relays[index].config_toml.as_deref()),
        (
            "usage-script",
            profiles.relays[index].usage_script.as_deref(),
        ),
    ];
    if let Some(key) = new_key {
        secret_updates.push(("key", Some(key)));
    }
    vault::set_profile_secrets(id, &secret_updates)?;
    let profile = profiles.relays[index].clone();
    if let Err(e) = save_profiles(&profiles) {
        let rollback = [
            ("key", previous_key.as_deref()),
            ("auth-json", previous_profile.auth_json.as_deref()),
            ("usage-api-key", previous_profile.usage_api_key.as_deref()),
            (
                "usage-access-token",
                previous_profile.usage_access_token.as_deref(),
            ),
            ("config-toml", previous_profile.config_toml.as_deref()),
            ("usage-script", previous_profile.usage_script.as_deref()),
        ];
        let rollback_result = vault::set_profile_secrets(id, &rollback);
        let rollback_verify = verify_profile_secrets(id, &rollback);
        return Err(ProfilesError::RolledBack(format!(
            "保存供应商资料失败: {e}; 机密回滚: {}; 回滚校验: {}",
            rollback_result
                .map(|_| "完成".to_string())
                .unwrap_or_else(|rollback| format!("失败: {rollback}")),
            rollback_verify
                .map(|_| "通过".to_string())
                .unwrap_or_else(|verify| format!("失败: {verify}"))
        )));
    }
    // 编辑的是激活中的 profile → 立即重写 config.toml, 否则 codex 继续用旧配置
    if was_active {
        let live_update = (|| -> Result<(), ProfilesError> {
            codex_config::write_relay_config(
                &profile.name,
                &profile.base_url,
                &profile.model,
                profile.wire_api.as_deref(),
                profile.model_reasoning_effort.as_deref(),
                profile.disable_response_storage,
                profile.model_context_window,
                profile.model_auto_compact_token_limit,
                profile.config_toml.as_deref(),
                Some(profile.supported_models.as_slice()),
            )?;
            if key_updated || input.auth_json.is_some() {
                vault::write_relay_auth(id, profile.auth_json.as_deref())?;
            }
            Ok(())
        })();
        if let Err(e) = live_update {
            let mut rollback_errors = Vec::new();
            profiles.relays[index] = previous_profile.clone();
            if let Err(rollback) = save_profiles(&profiles) {
                rollback_errors.push(format!("profile: {rollback}"));
            }
            let secret_rollback = [
                ("key", previous_key.as_deref()),
                ("auth-json", previous_profile.auth_json.as_deref()),
                ("usage-api-key", previous_profile.usage_api_key.as_deref()),
                (
                    "usage-access-token",
                    previous_profile.usage_access_token.as_deref(),
                ),
                ("config-toml", previous_profile.config_toml.as_deref()),
                ("usage-script", previous_profile.usage_script.as_deref()),
            ];
            if let Err(rollback) = vault::set_profile_secrets(id, &secret_rollback) {
                rollback_errors.push(format!("secrets: {rollback}"));
            }
            if let Err(verify) = verify_profile_secrets(id, &secret_rollback) {
                rollback_errors.push(format!("secrets verification: {verify}"));
            }
            if let Err(rollback) =
                restore_optional_file(&codex_config::codex_config_path(), &previous_config)
            {
                rollback_errors.push(format!("config: {rollback}"));
            }
            if let Err(rollback) =
                restore_optional_file(&codex_config::codex_auth_path(), &previous_auth)
            {
                rollback_errors.push(format!("auth: {rollback}"));
            }
            return Err(ProfilesError::RolledBack(format!(
                "更新当前供应商实时配置失败: {e}; 回滚结果: {}",
                if rollback_errors.is_empty() {
                    "完成".to_string()
                } else {
                    rollback_errors.join("; ")
                }
            )));
        }
    }
    Ok(profile)
}

pub fn delete_relay_profile(id: &str) -> Result<(), ProfilesError> {
    let mut profiles = load_profiles()?;
    if !profiles.relays.iter().any(|p| p.id == id) {
        return Err(ProfilesError::NotFound(id.to_string()));
    }
    let was_active = matches!(
        profiles.active,
        Some(ActiveSelection::Relay { ref profile_id }) if profile_id == id
    );
    // 当前供应商必须走 Tauri 层完整的安全切换事务，不能在同步删除入口里
    // 绕过 Codex 运行守卫、路由关闭和外层快照。
    if was_active {
        return Err(ProfilesError::Blocked(
            "不能直接删除当前供应商，请先使用「切换到官方」完成安全切换".into(),
        ));
    }
    let profile = profiles
        .relays
        .iter()
        .find(|profile| profile.id == id)
        .cloned()
        .ok_or_else(|| ProfilesError::NotFound(id.to_string()))?;
    let key = vault::get_relay_key(id)?;
    // 先清理机密再提交档案删除；若档案提交失败，恢复完整机密快照。
    vault::delete_relay_key(id)?;
    profiles.relays.retain(|p| p.id != id);
    if let Err(e) = save_profiles(&profiles) {
        let restore = [
            ("key", key.as_deref()),
            ("auth-json", profile.auth_json.as_deref()),
            ("usage-api-key", profile.usage_api_key.as_deref()),
            ("usage-access-token", profile.usage_access_token.as_deref()),
            ("config-toml", profile.config_toml.as_deref()),
            ("usage-script", profile.usage_script.as_deref()),
        ];
        let secret_restore = vault::set_profile_secrets(id, &restore);
        return Err(ProfilesError::RolledBack(format!(
            "删除供应商失败: {e}; 凭证恢复: {secret_restore:?}"
        )));
    }
    Ok(())
}

/// 切换历史 (防频繁切换告警): vault/switch-history.json, [rfc3339 时间戳, ...]
fn switch_history_path() -> PathBuf {
    vault::vault_dir().join("switch-history.json")
}

fn load_switch_history() -> Result<Vec<String>, ProfilesError> {
    let path = switch_history_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

pub fn record_switch() -> Result<(), ProfilesError> {
    let mut hist = load_switch_history()?;
    let now = chrono::Local::now().to_rfc3339();
    // 插入头部, 只保留最近 50 条
    hist.insert(0, now);
    hist.truncate(50);
    if let Some(parent) = switch_history_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    vault::atomic_write_bytes(
        &switch_history_path(),
        serde_json::to_string(&hist)?.as_bytes(),
    )?;
    Ok(())
}

/// 最近 30 分钟切换次数 (前端告警: 频繁切换 = 出口抖动信号)
pub fn recent_switch_count(minutes: i64) -> usize {
    let cutoff = chrono::Local::now() - chrono::Duration::minutes(minutes);
    load_switch_history()
        .unwrap_or_else(|e| {
            log::warn!("读取供应商切换历史失败: {e}");
            Vec::new()
        })
        .iter()
        .filter(|ts| {
            chrono::DateTime::parse_from_rfc3339(ts)
                .map(|t| t > cutoff)
                .unwrap_or(false)
        })
        .count()
}

/// 激活官方 profile: 中转凭证离场, 官方凭证归位, 顶层字段还原
pub fn activate_official() -> Result<ActiveSelection, ProfilesError> {
    let result = activate_official_with_progress(&|_| {})?;
    if let Err(e) = record_switch() {
        log::warn!("记录官方切换历史失败: {e}");
    }
    Ok(result)
}

/// 带进度回调的官方激活 (前端切换进度条): progress(步骤文案)
pub fn activate_official_with_progress(
    progress: &dyn Fn(&str),
) -> Result<ActiveSelection, ProfilesError> {
    let _guard = ACTIVATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut profiles = load_profiles()?;
    let prev_active = profiles.active.clone();

    // 1. 备份 config.toml (回滚用) + 读切换前快照
    progress("备份当前配置…");
    let backup = vault::backup_config()?;
    let state = vault::load_relay_state();
    let restore = (
        Some(state.prev_model.clone()),
        Some(state.prev_effort.clone()),
        Some(state.prev_disable_storage),
    );

    // 2. 写官方 config
    progress("写入官方配置与凭证…");
    if let Err(e) = codex_config::write_official_config(restore.0, restore.1, restore.2) {
        let _ = backup.map(std::fs::remove_file);
        return Err(ProfilesError::RolledBack(e.to_string()));
    }

    // 3. 官方凭证从 vault 恢复。若 vault 无凭证 (从未登录/登录已清),
    //    移除 auth.json 里的中转 key 残留 — 官方模式不得残留任何中转明文凭证。
    match vault::restore_official_auth() {
        Ok(true) => {
            if let Err(e) = vault::validate_official_auth_file() {
                restore_config_or_remove(&backup);
                let auth_ok = rollback_auth(&prev_active);
                return Err(ProfilesError::RolledBack(format!(
                    "官方凭证互斥校验失败: {e}; 凭证回滚={auth_ok}"
                )));
            }
        }
        Ok(false) => {
            let path = codex_config::codex_auth_path();
            if path.exists() {
                if let Err(e) = std::fs::remove_file(&path) {
                    restore_config_or_remove(&backup);
                    let auth_ok = rollback_auth(&prev_active);
                    return Err(ProfilesError::RolledBack(format!(
                        "移除第三方凭证失败: {e}; 凭证回滚={auth_ok}"
                    )));
                }
            }
        }
        Err(e) => {
            restore_config_or_remove(&backup);
            return Err(ProfilesError::RolledBack(e.to_string()));
        }
    }
    if let Err(e) = vault::validate_official_auth_file() {
        restore_config_or_remove(&backup);
        let auth_ok = rollback_auth(&prev_active);
        return Err(ProfilesError::RolledBack(format!(
            "官方凭证互斥校验失败: {e}; 凭证回滚={auth_ok}"
        )));
    }

    let project_scope_changed = !matches!(prev_active, Some(ActiveSelection::Official));
    if project_scope_changed {
        progress("同步官方项目索引…");
        if let Err(e) = crate::session_unify::sync_project_visibility(
            crate::codex_config::OFFICIAL_MODEL_PROVIDER,
        ) {
            restore_config_or_remove(&backup);
            let auth_ok = rollback_auth(&prev_active);
            return Err(ProfilesError::RolledBack(format!(
                "官方项目索引同步失败: {e}; 凭证回滚={auth_ok}"
            )));
        }
    }

    vault::clear_relay_state();
    profiles.active = Some(ActiveSelection::Official);
    if let Err(e) = save_profiles(&profiles) {
        // 最终提交 active 状态失败同样不能把官方凭证/配置留在外面；
        // 否则 UI 仍显示旧中转、实际已切官方，既不一致也可能暴露未隔离会话。
        restore_config_or_remove(&backup);
        let auth_ok = rollback_auth(&prev_active);
        if matches!(prev_active, Some(ActiveSelection::Relay { .. })) {
            let _ = vault::save_relay_state(&state);
        } else {
            vault::clear_relay_state();
        }
        let previous_provider = match &prev_active {
            Some(ActiveSelection::Official) => crate::codex_config::OFFICIAL_MODEL_PROVIDER,
            Some(ActiveSelection::Relay { .. }) => crate::codex_config::SHARED_MODEL_PROVIDER,
            None => "",
        };
        if project_scope_changed && !previous_provider.is_empty() {
            let _ = crate::session_unify::sync_project_visibility(previous_provider);
        }
        return Err(ProfilesError::RolledBack(format!(
            "保存官方切换状态失败: {e}; 凭证回滚={auth_ok}"
        )));
    }
    Ok(ActiveSelection::Official)
}

/// 激活中转 profile: 官方凭证 seal 离场, 中转 key 入场
pub fn activate_relay(profile_id: &str) -> Result<ActiveSelection, ProfilesError> {
    let result = activate_relay_with_progress(profile_id, &|_| {})?;
    if let Err(e) = record_switch() {
        log::warn!("记录第三方切换历史失败: {e}");
    }
    Ok(result)
}

/// 带进度回调的中转激活 (前端切换进度条): progress(步骤文案)
pub fn activate_relay_with_progress(
    profile_id: &str,
    progress: &dyn Fn(&str),
) -> Result<ActiveSelection, ProfilesError> {
    let _guard = ACTIVATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut profiles = load_profiles()?;
    let Some(profile) = profiles.relays.iter().find(|p| p.id == profile_id) else {
        return Err(ProfilesError::NotFound(profile_id.to_string()));
    };
    if !profile.has_key {
        return Err(ProfilesError::NotFound(
            "该 profile 未保存中转 key".to_string(),
        ));
    }
    // 切换前状态 — 回滚时按它恢复 auth.json (relay→relay 失败要重写旧中转 key)
    let prev_active = profiles.active.clone();
    let relay_state_before = vault::load_relay_state();
    let profile = profile.clone();

    progress("备份当前配置…");
    let backup = vault::backup_config()?;

    // 1. 官方凭证 seal 离场 (auth.json 删除)
    //    必须先于写中转 key — 避免两个凭证同盘
    if let Err(e) = vault::seal_official_auth() {
        return Err(ProfilesError::RolledBack(e.to_string()));
    }

    // 1.5 记录切换前顶层字段 (切回官方时还原)。
    //     仅当当前不是 relay 形态才保存 — relay→relay 或重复激活不得覆盖
    //     官方快照 (否则切回官方会还原成上一个 relay 的 model/effort)
    let is_relay_kind = matches!(
        codex_config::current_profile_kind(),
        Ok(CurrentProfile::Relay)
    );
    if !is_relay_kind {
        let (prev_model, prev_effort, prev_disable) =
            codex_config::top_level_fields().unwrap_or((None, None, None));
        if let Err(e) = vault::save_relay_state(&vault::RelayState {
            prev_model,
            prev_effort,
            prev_disable_storage: prev_disable,
        }) {
            restore_config_or_remove(&backup);
            rollback_auth(&prev_active);
            return Err(ProfilesError::RolledBack(e.to_string()));
        }
    }

    // 2. 写中转 config (共享 custom 桶)
    progress("写入中转配置与凭证…");
    if let Err(e) = codex_config::write_relay_config(
        &profile.name,
        &profile.base_url,
        &profile.model,
        profile.wire_api.as_deref(),
        profile.model_reasoning_effort.as_deref(),
        profile.disable_response_storage,
        profile.model_context_window,
        profile.model_auto_compact_token_limit,
        profile.config_toml.as_deref(),
        Some(profile.supported_models.as_slice()),
    ) {
        // 回滚: 恢复 config + auth.json 按切换前状态归位 (官方凭证或旧中转 key)
        restore_config_or_remove(&backup);
        let auth_ok = rollback_auth(&prev_active);
        return Err(ProfilesError::RolledBack(format!(
            "{e}{}",
            if auth_ok {
                ""
            } else {
                "; 且凭证恢复失败, 请检查 vault"
            }
        )));
    }

    // 3. 中转 key 写 auth.json (用户自定义 auth_json 优先, 否则自动生成)
    if let Err(e) = vault::write_relay_auth(profile_id, profile.auth_json.as_deref()) {
        restore_config_or_remove(&backup);
        let auth_ok = rollback_auth(&prev_active);
        return Err(ProfilesError::RolledBack(format!(
            "{e}{}",
            if auth_ok {
                ""
            } else {
                "; 且凭证恢复失败, 请检查 vault"
            }
        )));
    }
    if let Err(e) = vault::validate_relay_auth_file() {
        restore_config_or_remove(&backup);
        let auth_ok = rollback_auth(&prev_active);
        return Err(ProfilesError::RolledBack(format!(
            "中转凭证互斥校验失败: {e}; 凭证回滚={auth_ok}"
        )));
    }

    let project_scope_changed = !matches!(prev_active, Some(ActiveSelection::Relay { .. }));
    if project_scope_changed {
        progress("同步第三方项目索引…");
        if let Err(e) = crate::session_unify::sync_project_visibility(
            crate::codex_config::SHARED_MODEL_PROVIDER,
        ) {
            restore_config_or_remove(&backup);
            let auth_ok = rollback_auth(&prev_active);
            let _ = vault::save_relay_state(&relay_state_before);
            return Err(ProfilesError::RolledBack(format!(
                "第三方项目索引同步失败: {e}; 凭证回滚={auth_ok}"
            )));
        }
    }

    profiles.active = Some(ActiveSelection::Relay {
        profile_id: profile_id.to_string(),
    });
    if let Err(e) = save_profiles(&profiles) {
        restore_config_or_remove(&backup);
        let auth_ok = rollback_auth(&prev_active);
        let _ = vault::save_relay_state(&relay_state_before);
        let previous_provider = match &prev_active {
            Some(ActiveSelection::Official) => crate::codex_config::OFFICIAL_MODEL_PROVIDER,
            Some(ActiveSelection::Relay { .. }) => crate::codex_config::SHARED_MODEL_PROVIDER,
            None => "",
        };
        if project_scope_changed && !previous_provider.is_empty() {
            let _ = crate::session_unify::sync_project_visibility(previous_provider);
        }
        return Err(ProfilesError::RolledBack(format!(
            "保存第三方切换状态失败: {e}; 凭证回滚={auth_ok}"
        )));
    }
    Ok(ActiveSelection::Relay {
        profile_id: profile_id.to_string(),
    })
}

pub fn activate(selection: ActiveSelection) -> Result<ActiveSelection, ProfilesError> {
    match selection {
        ActiveSelection::Official => activate_official(),
        ActiveSelection::Relay { profile_id } => activate_relay(&profile_id),
    }
}

fn restore_config_or_remove(backup: &Option<PathBuf>) {
    if backup.is_some() {
        let _ = vault::restore_config_backup();
    } else {
        // 原本没有 config.toml, 回滚 = 删除我们写的
        let _ = std::fs::remove_file(codex_config::codex_config_path());
    }
}

/// 切换失败回滚时恢复 auth.json: 按切换前状态 —
/// 之前是 relay → 从 keyring 重写旧中转 key (seal 已删掉它),
/// 旧 profile 有自定义 auth_json 则按其原样恢复;
/// 之前是官方/未接管 → 从 vault 恢复官方凭证 (若已 seal 走)。
fn rollback_auth(prev_active: &Option<ActiveSelection>) -> bool {
    match prev_active {
        Some(ActiveSelection::Relay { profile_id }) => {
            let custom = load_profiles()
                .ok()
                .and_then(|p| p.relays.into_iter().find(|r| r.id == *profile_id))
                .and_then(|r| r.auth_json);
            vault::write_relay_auth(profile_id, custom.as_deref()).is_ok()
        }
        _ => vault::restore_official_auth().is_ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_profile_fields_are_not_written_to_profiles_json() {
        let _env = crate::test_util::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().expect("create temp root");
        let data_dir = root.path().join("data");
        let vault_dir = root.path().join("vault");
        std::env::set_var("CODEXFF_DATA_DIR", &data_dir);
        std::env::set_var("CODEXFF_VAULT_DIR", &vault_dir);

        let profile_id = format!("secret-test-{}", uuid::Uuid::new_v4());
        let auth_json = r#"{"OPENAI_API_KEY":"auth-secret-value"}"#;
        let config_toml = "model_provider = \"secret-provider-value\"";
        let usage_script = "return 'usage-script-secret-value'";
        let usage_api_key = "usage-api-secret-value";
        let usage_access_token = "usage-access-secret-value";
        let common_config = "model_reasoning_effort = \"secret-common-value\"";

        vault::set_profile_secret(&profile_id, "auth-json", Some(auth_json))
            .expect("store auth json");
        vault::set_profile_secret(&profile_id, "config-toml", Some(config_toml))
            .expect("store config");
        vault::set_profile_secret(&profile_id, "usage-script", Some(usage_script))
            .expect("store usage script");
        vault::set_profile_secret(&profile_id, "usage-api-key", Some(usage_api_key))
            .expect("store usage api key");
        vault::set_profile_secret(&profile_id, "usage-access-token", Some(usage_access_token))
            .expect("store usage access token");
        vault::set_secret("codex-common-config", common_config).expect("store common config");

        let profile = RelayProfile {
            id: profile_id.clone(),
            name: "Secret Test".into(),
            base_url: "https://example.invalid/v1".into(),
            model: "test-model".into(),
            auth_json: Some(auth_json.into()),
            config_toml: Some(config_toml.into()),
            usage_script: Some(usage_script.into()),
            usage_api_key: Some(usage_api_key.into()),
            usage_access_token: Some(usage_access_token.into()),
            ..RelayProfile::default()
        };
        save_profiles(&ProfilesFile {
            relays: vec![profile],
            active: None,
            codex_common_config: Some(common_config.into()),
        })
        .expect("save profiles");

        let disk =
            std::fs::read_to_string(data_dir.join("profiles.json")).expect("read profiles json");
        for plaintext in [
            auth_json,
            config_toml,
            usage_script,
            usage_api_key,
            usage_access_token,
            common_config,
        ] {
            assert!(
                !disk.contains(plaintext),
                "profiles.json leaked sensitive plaintext: {plaintext}"
            );
        }

        let loaded = load_profiles().expect("load profiles");
        let loaded_profile = loaded.relays.first().expect("saved profile");
        assert_eq!(loaded_profile.auth_json.as_deref(), Some(auth_json));
        assert_eq!(loaded_profile.config_toml.as_deref(), Some(config_toml));
        assert_eq!(loaded_profile.usage_script.as_deref(), Some(usage_script));
        assert_eq!(loaded_profile.usage_api_key.as_deref(), Some(usage_api_key));
        assert_eq!(
            loaded_profile.usage_access_token.as_deref(),
            Some(usage_access_token)
        );
        assert_eq!(loaded.codex_common_config.as_deref(), Some(common_config));

        vault::delete_profile_secrets(&profile_id).expect("clear profile secrets");
        vault::delete_secret("codex-common-config").expect("clear common config");
        std::env::remove_var("CODEXFF_DATA_DIR");
        std::env::remove_var("CODEXFF_VAULT_DIR");
    }

    #[test]
    fn load_profiles_migrates_legacy_wire_aliases() {
        let _env = crate::test_util::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().expect("create temp root");
        let data_dir = root.path().join("data");
        let vault_dir = root.path().join("vault");
        std::env::set_var("CODEXFF_DATA_DIR", &data_dir);
        std::env::set_var("CODEXFF_VAULT_DIR", &vault_dir);
        std::fs::create_dir_all(&data_dir).expect("create data dir");
        std::fs::write(
            data_dir.join("profiles.json"),
            r#"{
              "relays": [
                {"id":"responses","name":"R","base_url":"https://r.invalid","model":"m","wire_api":"openai_responses"},
                {"id":"chat","name":"C","base_url":"https://c.invalid","model":"m","wire_api":"openai_chat"}
              ],
              "active": null
            }"#,
        )
        .expect("write profiles");

        let loaded = load_profiles().expect("load profiles");
        assert_eq!(loaded.relays[0].wire_api.as_deref(), Some("responses"));
        assert_eq!(loaded.relays[1].wire_api.as_deref(), Some("chat"));
        let disk = std::fs::read_to_string(data_dir.join("profiles.json")).expect("read disk");
        assert!(!disk.contains("openai_responses"));
        assert!(!disk.contains("openai_chat"));

        std::env::remove_var("CODEXFF_DATA_DIR");
        std::env::remove_var("CODEXFF_VAULT_DIR");
    }
}
