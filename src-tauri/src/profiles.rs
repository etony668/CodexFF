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

/// 激活/切换串行化: config.toml + auth.json + profiles.json 多文件非原子,
/// Tauri 命令多线程并发执行时快速双击/多窗口切换会交错撕裂。
/// 只保护需要写这几个文件的入口, 不嵌套 (delete_relay_profile 内部调
/// activate_official 会拿锁, 自身不再拿锁)。
static ACTIVATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        }
    }
    save_profiles(&profiles)?;
    Ok(())
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    Ok(serde_json::from_str(&text)?)
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
    vault::atomic_write_bytes(&path, serde_json::to_string_pretty(profiles)?.as_bytes())?;
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
        .map(str::to_string);

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

    // key 先落 keyring, 失败则 profile 不创建
    vault::set_relay_key(&profile.id, key.trim())?;

    profiles.relays.push(profile.clone());
    save_profiles(&profiles)?;
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
    let was_active = matches!(
        profiles.active,
        Some(ActiveSelection::Relay { ref profile_id }) if profile_id == id
    );
    let (name, base_url, wire_api, auth_json, config_toml) =
        normalize_input(&input, profiles.codex_common_config.as_deref())?;

    let stored_auth = profiles.relays[index].auth_json.clone();
    let mut key_updated = false;
    // update: key=Some("") 或 None = 不修改
    if let Some(key) = input.key.as_deref() {
        if !key.trim().is_empty() {
            vault::set_relay_key(id, key.trim())?;
            profiles.relays[index].has_key = true;
            key_updated = true;
        }
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
    let profile = profiles.relays[index].clone();
    save_profiles(&profiles)?;
    // 编辑的是激活中的 profile → 立即重写 config.toml, 否则 codex 继续用旧配置
    if was_active {
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
        // 改了 key 或 auth_json → auth.json 同步换新, 否则 codex 拿旧 key 请求
        if key_updated || input.auth_json.is_some() {
            vault::write_relay_auth(id, profile.auth_json.as_deref())?;
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
    // 删的是当前激活 profile → 先切回官方; 切换失败则中止删除,
    // 保证 config/auth/profile 三者状态一致 (不会出现 profile 已删、config 仍中转)
    if was_active {
        activate_official()?;
    }
    profiles.relays.retain(|p| p.id != id);
    vault::delete_relay_key(id)?;
    save_profiles(&profiles)?;
    Ok(())
}

/// 切换历史 (防频繁切换告警): vault/switch-history.json, [rfc3339 时间戳, ...]
fn switch_history_path() -> PathBuf {
    vault::vault_dir().join("switch-history.json")
}

fn load_switch_history() -> Vec<String> {
    let path = switch_history_path();
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn record_switch() {
    let mut hist = load_switch_history();
    let now = chrono::Local::now().to_rfc3339();
    // 插入头部, 只保留最近 50 条
    hist.insert(0, now);
    hist.truncate(50);
    if let Some(parent) = switch_history_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = vault::atomic_write_bytes(
        &switch_history_path(),
        serde_json::to_string(&hist).unwrap_or_default().as_bytes(),
    );
}

/// 最近 30 分钟切换次数 (前端告警: 频繁切换 = 出口抖动信号)
pub fn recent_switch_count(minutes: i64) -> usize {
    let cutoff = chrono::Local::now() - chrono::Duration::minutes(minutes);
    load_switch_history()
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
    activate_official_with_progress(&|_| {})
}

/// 带进度回调的官方激活 (前端切换进度条): progress(步骤文案)
pub fn activate_official_with_progress(
    progress: &dyn Fn(&str),
) -> Result<ActiveSelection, ProfilesError> {
    let _guard = ACTIVATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut profiles = load_profiles()?;

    // 0. 有标记隔离的会话需要迁移时, Codex 必须完全退出 —
    //    移动正在写入的 GB 级会话文件会造成会话分裂/损坏。
    if crate::session_manager::has_isolated_sessions() && crate::session_manager::codex_running() {
        return Err(ProfilesError::Blocked(
            "有标记隔离的会话需要迁移，请先完全退出 Codex / ChatGPT 桌面端与命令行再切换官方订阅"
                .into(),
        ));
    }

    // 1. 备份 config.toml (回滚用) + 读切换前快照
    progress("备份当前配置…");
    let backup = vault::backup_config()?;
    let state = vault::load_relay_state();
    let restore = (
        Some(state.prev_model),
        Some(state.prev_effort),
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
        Ok(true) => {}
        Ok(false) => {
            let _ = std::fs::remove_file(codex_config::codex_auth_path());
        }
        Err(e) => {
            restore_config_or_remove(&backup);
            return Err(ProfilesError::RolledBack(e.to_string()));
        }
    }

    vault::clear_relay_state();
    profiles.active = Some(ActiveSelection::Official);
    save_profiles(&profiles)?;
    // 3.5 清洗 reasoning 条目 (官方 Responses schema 要求 encrypted_content
    // 存在时 content 为空数组)。Codex 正在运行会自动跳过, 本地路由请求层兜底。
    progress("清洗会话推理数据…");
    if let Err(e) = crate::session_model::sanitize_reasoning_content(None, &|p| {
        progress(&format!(
            "清洗会话推理数据 ({}/{})…",
            p.done, p.total
        ));
    }) {
        log::warn!("官方激活后清洗会话推理数据失败: {e}");
    }
    // 4. 会话隔离: 标记的会话移入金库隔离区, 官方 CLI 扫不到
    progress("隔离标记会话…");
    if let Err(e) = crate::session_manager::sync_session_isolation_with_progress(progress) {
        log::warn!("官方激活后会话隔离同步失败: {e}");
        progress(&format!("会话迁移失败: {e}"));
    }
    record_switch();
    Ok(ActiveSelection::Official)
}

/// 激活中转 profile: 官方凭证 seal 离场, 中转 key 入场
pub fn activate_relay(profile_id: &str) -> Result<ActiveSelection, ProfilesError> {
    activate_relay_with_progress(profile_id, &|_| {})
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
    // 0. 有标记隔离的会话需要从金库移回时, Codex 必须完全退出 (防止写坏会话文件)
    if crate::session_manager::has_isolated_sessions() && crate::session_manager::codex_running() {
        return Err(ProfilesError::Blocked(
            "有标记隔离的会话需要恢复，请先完全退出 Codex / ChatGPT 桌面端与命令行再切换第三方"
                .into(),
        ));
    }
    // 切换前状态 — 回滚时按它恢复 auth.json (relay→relay 失败要重写旧中转 key)
    let prev_active = profiles.active.clone();
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

    profiles.active = Some(ActiveSelection::Relay {
        profile_id: profile_id.to_string(),
    });
    save_profiles(&profiles)?;
    // 3.5 清洗 reasoning 条目 — 保证直通官方 Responses API 的第三方中转
    // (皮卡丘等) 不会被 array_above_max_length 拒掉。Codex 正在运行会自动跳过,
    // 由本地路由在请求层兜底清洗。
    progress("清洗会话推理数据…");
    if let Err(e) = crate::session_model::sanitize_reasoning_content(None, &|p| {
        progress(&format!(
            "清洗会话推理数据 ({}/{})…",
            p.done, p.total
        ));
    }) {
        log::warn!("中转激活后清洗会话推理数据失败: {e}");
    }
    // 4. 会话恢复: 标记的会话从金库隔离区移回 codex 目录
    progress("恢复标记会话…");
    if let Err(e) = crate::session_manager::sync_session_isolation_with_progress(progress) {
        log::warn!("中转激活后会话恢复失败: {e}");
        progress(&format!("会话恢复失败: {e}"));
    }
    record_switch();
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
