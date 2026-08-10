//! Codex 配置读写 — 只做最小改动, 保留用户其他设置。
//!
//! 核心原则:
//! - config.toml 用 toml_edit 原地修改, 不动用户自定义字段
//! - auth.json 是凭证物理隔离的关键: 官方凭证只在官方 profile 激活时出现
//! - sessions/ 目录永不接触 — 会话跨 provider 共享

use std::path::PathBuf;

use serde_json::Value;
use toml_edit::{value, DocumentMut, Item, Table};

use crate::vault::{self, VaultError};

pub const CODEX_DIR_NAME: &str = ".codex";
pub const AUTH_FILENAME: &str = "auth.json";
pub const CONFIG_FILENAME: &str = "config.toml";
pub const SESSIONS_DIR_NAME: &str = "sessions";
pub const ARCHIVED_SESSIONS_DIR_NAME: &str = "archived_sessions";
pub const STATE_DB_FILENAME: &str = "state_5.sqlite";

/// CodexFF 维护的模型目录文件名 (DeepSeek 官方 models.json 拷贝)。
/// 只清理这个 sentinel 值, 不碰用户手写的 model_catalog_json (如官方脚本的 ~/.codex/models.json)。
pub const CODEXFF_MODEL_CATALOG_FILENAME: &str = "codexff-model-catalog.json";

/// 官方原生 provider 名 (codex 内置, 老配置/手动配置用)
pub const OFFICIAL_MODEL_PROVIDER: &str = "openai";
/// 共享 provider 桶 (统一会话历史): 官方与中转都以 "custom" 身份运行,
/// codex CLI 按 model_provider 分桶 → 互切时续聊列表互通 (cc-switch 同机制)。
/// 切换 = 覆写 [model_providers.custom] 表内容 (官方形态 ↔ 中转形态)。
pub const SHARED_MODEL_PROVIDER: &str = "custom";

/// 官方形态 custom 表: 认证走 auth.json ChatGPT 登录, 无 base_url → 直连官方后端
fn official_custom_table() -> Table {
    let mut table = Table::new();
    table["name"] = value("OpenAI");
    table["requires_openai_auth"] = value(true);
    table["supports_websockets"] = value(true);
    table["wire_api"] = value("responses");
    // 归属标记 — 切中转时识别"我们写的官方表"并替换 (无标记前官方表
    // 与用户手写表不可分, relay 切换保留旧官方表 → kind 永远判 Official)
    table["codexff_official"] = value(true);
    table
}

/// custom 表是否官方形态。匹配两种: 旧 4-key 注入产物 (升级前无标记) 与
/// 新 5-key (带 codexff_official 标记)。核心判据 = 4 个官方特征键 + 无
/// base_url + 无中转标记 (中转表也有 requires_openai_auth, 靠 base_url/
/// codexff_relay 区分)。
fn is_official_custom_table(table: &Table) -> bool {
    table.get("name").and_then(Item::as_str) == Some("OpenAI")
        && table.get("requires_openai_auth").and_then(Item::as_bool) == Some(true)
        && table.get("supports_websockets").and_then(Item::as_bool) == Some(true)
        && table.get("wire_api").and_then(Item::as_str) == Some("responses")
        && table.get("base_url").is_none()
        && table
            .get("codexff_relay")
            .and_then(Item::as_bool)
            .unwrap_or(false)
            != true
}

#[derive(Debug, thiserror::Error)]
pub enum CodexConfigError {
    #[error("codex 配置目录不存在: {0}")]
    MissingConfigDir(PathBuf),
    #[error("解析 config.toml 失败: {0}")]
    TomlParse(String),
    #[error("拒绝接管: {0}")]
    Refuse(String),
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error("vault 错误: {0}")]
    Vault(#[from] VaultError),
}

/// 解析 codex 配置目录 (支持 CODEX_HOME 环境变量覆盖, 同 codex CLI 行为)
pub fn codex_config_dir() -> PathBuf {
    if let Ok(home) = std::env::var("CODEX_HOME") {
        let trimmed = home.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(CODEX_DIR_NAME)
}

pub fn codex_auth_path() -> PathBuf {
    codex_config_dir().join(AUTH_FILENAME)
}

pub fn codex_config_path() -> PathBuf {
    codex_config_dir().join(CONFIG_FILENAME)
}

pub fn codex_sessions_paths() -> Vec<PathBuf> {
    let dir = codex_config_dir();
    vec![
        dir.join(SESSIONS_DIR_NAME),
        dir.join(ARCHIVED_SESSIONS_DIR_NAME),
    ]
}

pub fn codex_state_db_path() -> PathBuf {
    codex_config_dir().join(STATE_DB_FILENAME)
}

pub fn read_config_text() -> Result<String, CodexConfigError> {
    let path = codex_config_path();
    if !path.exists() {
        // 没有 config.toml 时返回默认配置, 让写入逻辑自己建
        return Ok(String::new());
    }
    std::fs::read_to_string(&path).map_err(CodexConfigError::Io)
}

pub fn read_auth_json() -> Result<Option<Value>, CodexConfigError> {
    let path = codex_auth_path();
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&text)?))
}

/// 官方 profile 激活: 写共享 custom 桶 + 官方形态表 (统一会话历史),
/// 官方/中转互切时 codex CLI 续聊列表互通; 认证仍走 auth.json 官方登录。
/// 清掉我们写过的中转表 (codexff_relay 标记) 和遗留中转形态 custom 表。
/// 保留用户自己的其他 provider 配置。
/// `restore` 来自切换前快照 (RelayState): 还原/清理顶层 model 等字段。
pub fn write_official_config(
    restore_model: Option<Option<String>>,
    restore_effort: Option<Option<String>>,
    restore_disable: Option<Option<bool>>,
) -> Result<(), CodexConfigError> {
    let mut doc = parse_or_default(&read_config_text()?)?;

    doc["model_provider"] = value(SHARED_MODEL_PROVIDER);
    // 顶层字段还原: Some(Some(v)) → 写回; Some(None) → 删除 (切换前没有)
    match restore_model {
        Some(Some(v)) => doc["model"] = value(v),
        Some(None) => {
            doc.as_table_mut().remove("model");
        }
        None => {}
    }
    match restore_effort {
        Some(Some(v)) => doc["model_reasoning_effort"] = value(v),
        Some(None) => {
            doc.as_table_mut().remove("model_reasoning_effort");
        }
        None => {}
    }
    match restore_disable {
        Some(Some(v)) => doc["disable_response_storage"] = value(v),
        Some(None) => {
            doc.as_table_mut().remove("disable_response_storage");
        }
        None => {}
    }

    // 安全闸: custom 表已存在、非官方形态、非我们标记的中转表, 且带 base_url
    // (会路由认证流量) → 拒绝接管, 防止官方凭证被发往未知后端
    if let Some(t) = doc
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|t| t.get(SHARED_MODEL_PROVIDER))
        .and_then(Item::as_table)
    {
        if !is_official_custom_table(t) {
            let is_ours = t
                .get("codexff_relay")
                .and_then(Item::as_bool)
                .unwrap_or(false);
            if !is_ours && t.get("base_url").is_some() {
                return Err(CodexConfigError::Refuse(
                    "检测到非 CodexFF 管理的 [model_providers.custom] 表 (含 base_url), \
                     拒绝接管以免官方流量误路由到未知后端。请先手动清理该表, \
                     或先用 CodexFF 添加并切换到某个中转 profile"
                        .into(),
                ));
            }
        }
    }

    // custom 桶: 已是官方形态则保留 (幂等); 否则清掉中转形态表后注入官方形态
    let providers = doc
        .entry("model_providers")
        .or_insert_with(|| Item::Table(Table::new()));
    let table = providers
        .as_table_mut()
        .ok_or_else(|| CodexConfigError::TomlParse("model_providers 不是表".into()))?;
    let already_official = table
        .get(SHARED_MODEL_PROVIDER)
        .and_then(Item::as_table)
        .is_some_and(is_official_custom_table);
    if !already_official {
        // 移除: 我们写的中转表 (codexff_relay 标记, 含旧 uuid 命名) + 中转形态 custom 表
        let ours: Vec<String> = table
            .iter()
            .filter(|(k, item)| {
                if *k == SHARED_MODEL_PROVIDER {
                    return true;
                }
                item.as_table()
                    .and_then(|t| t.get("codexff_relay"))
                    .and_then(Item::as_bool)
                    .unwrap_or(false)
            })
            .map(|(k, _)| k.to_string())
            .collect();
        for key in ours {
            table.remove(&key);
        }
        table.insert(SHARED_MODEL_PROVIDER, Item::Table(official_custom_table()));
    }

    // 切官方: 移除我们写过的模型目录字段 (DeepSeek catalog 只在 relay 态生效;
    // 只清 sentinel 值, 用户手写的 model_catalog_json 保留)
    set_codex_model_catalog_field(&mut doc, false);
    // 模型下拉目录换成官方模型 (供高效工作流使用)
    write_official_model_catalog()?;

    write_config_text(&doc.to_string())
}

/// 中转 profile 激活: 覆写共享 custom 桶为当前中转形态 (统一会话历史),
/// 格式对齐 cc-switch:
///
/// ```toml
/// model_provider = "custom"
/// model = "<默认模型>"                      # 顶层, codex 启动默认请求的模型
/// model_reasoning_effort = "high"           # 可选
/// disable_response_storage = true           # 可选 (中转站通常要求)
///
/// [model_providers.custom]
/// name = "<显示名>"
/// base_url = "<中转地址>"
/// wire_api = "responses"                    # chat | responses | anthropic
/// requires_openai_auth = true               # 认证走 auth.json 的 OPENAI_API_KEY
/// model_context_window = 400000             # 可选 — 上下文窗口 (默认 128k, 官方模型 400k)
/// model_auto_compact_token_limit = 360000   # 可选 — 超限自动压缩阈值
/// codexff_relay = true                      # 归属标记, 切换回官方时清理
/// ```
///
/// `custom_config` (用户自定义 config.toml 底稿, cc-switch 对齐):
/// 以底稿为底, 只强制 model_provider="custom" (统一会话桶必需) +
/// 维护 [model_providers.custom] 中转形态表; 其余字段 (任意用户配置)
/// 用户全控 — reasoning effort 等高级配置由用户在 TOML 编辑器里自行管理。
/// 通用段 (notify/marketplaces/plugins/features/mcp_servers/desktop/
/// shell_environment_policy) 例外: 始终以磁盘实时状态为准, 防止底稿里的
/// 旧快照把 Codex 桌面端运行时写入的设置回退 (如 [desktop] 的
/// selected-avatar-id 宠物选择、插件与市场更新)。
///
/// 顶层字段规则 (表单字段是 config.toml 顶层的视图, 同 cc-switch 单文档语义):
/// - model: 表单非空 → 覆盖 (含底稿); 空 → 程序化路径清残留, 底稿不动
/// - model_reasoning_effort: 表单非空 → 覆盖; 空 → 保留底稿 (用户常在 TOML
///   自行管理, 表单通常不填); 程序化路径 → 清残留
/// - disable_response_storage: 表单 checkbox 权威, 但只在底稿缺失或与表单
///   不一致时写入 — 不误删底稿里中转站要求的 disable_response_storage
pub fn build_relay_config(
    display_name: &str,
    base_url: &str,
    model: &str,
    wire_api: Option<&str>,
    model_reasoning_effort: Option<&str>,
    disable_response_storage: bool,
    model_context_window: Option<u64>,
    model_auto_compact_token_limit: Option<u64>,
    custom_config: Option<&str>,
) -> Result<DocumentMut, CodexConfigError> {
    let mut doc = match custom_config {
        Some(text) if !text.trim().is_empty() => {
            // 底稿 = provider 视图: 通用段始终以磁盘实时状态为准 (白名单)。
            // 底稿里的通用段只是添加供应商时的旧快照, Codex 桌面端会在
            // 运行中写入 [desktop] 的 selected-avatar-id (宠物选择) 并更新
            // 插件/市场/MCP, 以旧快照为准会把运行时设置回退 (宠物变默认)。
            // 磁盘缺失时才保留底稿里的通用段 (全新目录/用户手写补充)。
            let mut merged = parse_or_default(text)?;
            let disk = parse_or_default(&read_config_text()?)?;
            const COMMON: &[&str] = &[
                "notify",
                "marketplaces",
                "plugins",
                "features",
                "mcp_servers",
                "desktop",
                "shell_environment_policy",
            ];
            for key in COMMON {
                if let Some(item) = disk.get(key) {
                    merged[key] = item.clone();
                }
            }
            // model_providers: 保留磁盘上的非 custom 表 (用户自定义 provider)
            if let Some(disk_providers) = disk.get("model_providers").and_then(Item::as_table) {
                let tbl = merged
                    .entry("model_providers")
                    .or_insert(Item::Table(Table::new()));
                let tbl = tbl
                    .as_table_like_mut()
                    .ok_or_else(|| CodexConfigError::TomlParse("model_providers 不是表".into()))?;
                for (k, v) in disk_providers.iter() {
                    if k != SHARED_MODEL_PROVIDER && !tbl.contains_key(k) {
                        tbl.insert(k, v.clone());
                    }
                }
            }
            merged
        }
        _ => parse_or_default(&read_config_text()?)?,
    };
    // 程序化路径 (无底稿): 切换 relay 时清掉上一个 relay 的顶层残留
    let clear_empty = custom_config.is_none();
    apply_relay_fields(
        &mut doc,
        display_name,
        base_url,
        model,
        wire_api,
        model_reasoning_effort,
        disable_response_storage,
        model_context_window,
        model_auto_compact_token_limit,
        clear_empty,
    )?;
    Ok(doc)
}

/// 纯物化 (导入用): 以表单字段构建完整中转文档, 不读磁盘当前 config —
/// 导入的新 profile 不应继承当前激活 relay 的配置。
pub fn materialize_relay_config(
    display_name: &str,
    base_url: &str,
    model: &str,
    wire_api: Option<&str>,
    model_reasoning_effort: Option<&str>,
    disable_response_storage: bool,
    model_context_window: Option<u64>,
    model_auto_compact_token_limit: Option<u64>,
) -> Result<String, CodexConfigError> {
    let mut doc = DocumentMut::new();
    apply_relay_fields(
        &mut doc,
        display_name,
        base_url,
        model,
        wire_api,
        model_reasoning_effort,
        disable_response_storage,
        model_context_window,
        model_auto_compact_token_limit,
        false,
    )?;
    Ok(doc.to_string())
}

/// 顶层字段 + custom 桶维护的共享逻辑 (见 build_relay_config 文档注释)。
/// `clear_empty`: 程序化路径 (无底稿) 时空表单字段要清残留; 底稿/新文档不动。
/// 参数多但都是写入文档的字段, 与 build_relay_config 公开签名一一对应。
#[allow(clippy::too_many_arguments)]
fn apply_relay_fields(
    doc: &mut DocumentMut,
    display_name: &str,
    base_url: &str,
    model: &str,
    wire_api: Option<&str>,
    model_reasoning_effort: Option<&str>,
    disable_response_storage: bool,
    model_context_window: Option<u64>,
    model_auto_compact_token_limit: Option<u64>,
    clear_empty: bool,
) -> Result<(), CodexConfigError> {
    // 统一会话桶 — 强制, 不可被用户内容覆盖 (续聊列表分桶依赖)
    doc["model_provider"] = value(SHARED_MODEL_PROVIDER);
    // 顶层字段: 见文档注释的规则
    if !model.is_empty() {
        doc["model"] = value(model);
    } else if clear_empty {
        doc.as_table_mut().remove("model");
    }
    match model_reasoning_effort {
        Some(e) if !e.is_empty() => doc["model_reasoning_effort"] = value(e),
        _ if clear_empty => {
            doc.as_table_mut().remove("model_reasoning_effort");
        }
        _ => {}
    }
    match doc.get("disable_response_storage").and_then(Item::as_bool) {
        None if disable_response_storage => {
            doc["disable_response_storage"] = value(true);
        }
        Some(d) if d != disable_response_storage => {
            doc["disable_response_storage"] = value(disable_response_storage);
        }
        _ => {}
    }

    // custom 桶: 缺 → 注入中转形态表; 是我们的 (codexff_relay 标记) → 刷新
    // (切换 relay / 编辑 profile 时旧表换新, 否则 base_url/名称残留上一个);
    // 非我们标记的用户表 → 保留 (用户全控)
    let providers = doc
        .entry("model_providers")
        .or_insert(Item::Table(Table::new()));
    let table = providers
        .as_table_mut()
        .ok_or_else(|| CodexConfigError::TomlParse("model_providers 不是表".into()))?;
    let existing = table.get(SHARED_MODEL_PROVIDER).and_then(Item::as_table);
    let is_ours = existing
        .map(|t| {
            t.get("codexff_relay")
                .and_then(Item::as_bool)
                .unwrap_or(false)
        })
        .unwrap_or(false);
    // 官方形态表 (含旧版无标记的) 也要替换 — 否则 official→relay 切换后
    // 表残留官方形态, current_profile_kind 判 Official, UI 永远显示官方
    let is_app_official = existing.map(is_official_custom_table).unwrap_or(false);
    // 带 base_url 的表 (中转形态, 含底稿路径无标记表) 也替换 — 补上
    // codexff_relay 标记。否则切官方时安全闸 (write_official_config) 把
    // 无标记 + base_url 的表判为"未知后端"拒绝接管。
    let has_base_url = existing
        .map(|t| t.get("base_url").is_some())
        .unwrap_or(false);
    if table.get(SHARED_MODEL_PROVIDER).is_none() || is_ours || is_app_official || has_base_url {
        table.insert(
            SHARED_MODEL_PROVIDER,
            Item::Table(relay_table(
                display_name,
                base_url,
                wire_api,
                model_context_window,
                model_auto_compact_token_limit,
            )),
        );
    }

    // DeepSeek 官方网关 → 注入模型目录字段 (桌面端模型选择器显示「自定义」,
    // 而不是回退内置 gpt-5.6); 其它网关 → 清我们写过的 sentinel 残留
    set_codex_model_catalog_field(doc, is_deepseek_official_gateway(base_url, wire_api));

    Ok(())
}

/// DeepSeek 官方网关判定: 自家 base_url + 原生 Responses。只有这种网关
/// 才注入官方 models.json — 聚合站 (aihubmix 等) 只是托管同名模型, 不承诺
/// 官方 catalog 声明的能力 (freeform apply_patch 等), 保持中性模板即可。
fn is_deepseek_official_gateway(base_url: &str, wire_api: Option<&str>) -> bool {
    base_url.to_ascii_lowercase().contains("deepseek.com")
        && wire_api.map(|w| w == "responses").unwrap_or(false)
}

/// 顶层 model_catalog_json 字段维护 (仅动自己的 sentinel 值):
/// Some → 注入 CodexFF 文件名; None → 只移除 sentinel 值, 用户手写的路径保留。
fn set_codex_model_catalog_field(doc: &mut DocumentMut, enable: bool) {
    match enable {
        true => doc["model_catalog_json"] = value(CODEXFF_MODEL_CATALOG_FILENAME),
        false => {
            let is_ours = doc.get("model_catalog_json").and_then(Item::as_str)
                == Some(CODEXFF_MODEL_CATALOG_FILENAME);
            if is_ours {
                doc.as_table_mut().remove("model_catalog_json");
            }
        }
    }
}

/// 落盘 DeepSeek 官方模型目录 (models.json 拷贝, 与官方一键脚本同源)。
/// 没有它 Codex 不认识 deepseek-v4-flash, 桌面端模型选择器回退显示内置 gpt-5.6。
fn write_deepseek_model_catalog() -> Result<(), CodexConfigError> {
    let path = codex_config_dir().join(CODEXFF_MODEL_CATALOG_FILENAME);
    let content = include_str!("resources/codex_deepseek_catalog_template.json");
    vault::atomic_write_bytes(&path, content.as_bytes()).map_err(CodexConfigError::Vault)
}

/// 官方订阅模型目录 (供高效工作流模型下拉使用; 不注入 config 的
/// model_catalog_json 字段, 官方 Codex 用内置模型列表, 我们只维护自己的下拉)
fn write_official_model_catalog() -> Result<(), CodexConfigError> {
    let path = codex_config_dir().join(CODEXFF_MODEL_CATALOG_FILENAME);
    let content = r#"{"models":[{"slug":"gpt-5.6-luna"},{"slug":"gpt-5.6-sol"},{"slug":"gpt-5.6-terra"},{"slug":"gpt-5.2-codex"},{"slug":"gpt-5.2-codex-mini"},{"slug":"gpt-5.1-codex"},{"slug":"gpt-5-codex"}]}"#;
    vault::atomic_write_bytes(&path, content.as_bytes()).map_err(CodexConfigError::Vault)
}

/// 移除我们维护的模型目录文件 (切到非 DeepSeek 中转时, 避免模型下拉
/// 残留上一个供应商的模型)
fn remove_our_model_catalog() -> Result<(), CodexConfigError> {
    let path = codex_config_dir().join(CODEXFF_MODEL_CATALOG_FILENAME);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CodexConfigError::Io(e)),
    }
}

/// 为任意中转写模型目录（桌面端模型选择器只显示该中转真实支持的模型）。
/// 以 DeepSeek 官方目录的模型条目为模板，覆盖 slug / 名称 / 上下文窗口 /
/// 思考档位，避免桌面端因为缺字段而解析失败。
pub fn write_relay_model_catalog(
    models: &[String],
    context_window: Option<u64>,
) -> Result<(), CodexConfigError> {
    let template_text = include_str!("resources/codex_deepseek_catalog_template.json");
    let mut root: Value = serde_json::from_str(template_text)?;
    let Some(template) = root
        .get_mut("models")
        .and_then(|m| m.as_array_mut())
        .and_then(|arr| arr.first().cloned())
    else {
        return Err(CodexConfigError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "模型目录模板为空",
        )));
    };
    let ctx = context_window.unwrap_or(128_000);
    let mut entries = Vec::new();
    for (idx, slug) in models.iter().enumerate() {
        if slug.trim().is_empty() {
            continue;
        }
        let mut m = template.clone();
        let obj = m.as_object_mut().ok_or_else(|| {
            CodexConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "模型模板不是对象",
            ))
        })?;
        obj.insert("slug".to_string(), Value::String(slug.trim().to_string()));
        obj.insert(
            "display_name".to_string(),
            Value::String(slug.trim().to_string()),
        );
        obj.insert(
            "description".to_string(),
            Value::String(format!("{slug}（第三方网关）")),
        );
        obj.insert("context_window".to_string(), Value::from(ctx));
        obj.insert("max_context_window".to_string(), Value::from(ctx));
        obj.insert(
            "auto_compact_token_limit".to_string(),
            Value::from(ctx.saturating_mul(90) / 100),
        );
        obj.insert(
            "supported_reasoning_levels".to_string(),
            serde_json::json!([
                {"effort": "low", "description": "Fast responses with lighter reasoning"},
                {"effort": "medium", "description": "Balanced reasoning depth"},
                {"effort": "high", "description": "Extra high reasoning depth for complex problems"}
            ]),
        );
        obj.insert(
            "default_reasoning_level".to_string(),
            Value::String("high".into()),
        );
        obj.insert("priority".to_string(), Value::from(idx + 1));
        obj.insert("visibility".to_string(), Value::String("list".into()));
        entries.push(m);
    }
    root["models"] = Value::Array(entries);
    let content = serde_json::to_string_pretty(&root)?;
    let path = codex_config_dir().join(CODEXFF_MODEL_CATALOG_FILENAME);
    vault::atomic_write_bytes(&path, content.as_bytes()).map_err(CodexConfigError::Vault)
}

/// 中转形态 custom 表 (含归属标记, 切换回官方时按标记清理)。
/// 上下文窗口字段写在表内 (codex 读 provider 级属性, 与 cc-switch
/// 手写类预设同位置); None = 不写, codex 用默认 128k。
fn relay_table(
    display_name: &str,
    base_url: &str,
    wire_api: Option<&str>,
    model_context_window: Option<u64>,
    model_auto_compact_token_limit: Option<u64>,
) -> Table {
    let mut relay_table = Table::new();
    relay_table["name"] = value(display_name);
    relay_table["base_url"] = value(base_url);
    relay_table["requires_openai_auth"] = value(true);
    relay_table["codexff_relay"] = value(true);
    if let Some(wire) = wire_api {
        if !wire.is_empty() {
            relay_table["wire_api"] = value(wire);
        }
    }
    if let Some(w) = model_context_window {
        relay_table["model_context_window"] =
            toml_edit::Item::Value(toml_edit::Value::from(w as i64));
    }
    if let Some(l) = model_auto_compact_token_limit {
        relay_table["model_auto_compact_token_limit"] =
            toml_edit::Item::Value(toml_edit::Value::from(l as i64));
    }
    relay_table
}

/// 添加供应商的默认 config.toml 底稿: 磁盘 config 的通用段
/// (notify/marketplaces/plugins/features/mcp_servers/desktop 等, codex app
/// 与用户手写维护) + 预设的 provider 段 (顶层 model/model_provider/
/// model_reasoning_effort/disable_response_storage + [model_providers.custom] 表)。
///
/// 通用段必须保留 — 切中转后 codex 的 MCP 服务、marketplace 插件等靠它们工作;
/// 否则只剩 provider 段, 用户 mcp_servers/notify 配置全丢。磁盘不存在时只用预设。
/// 返回完整 TOML 文本, 作为表单 config.toml 的默认底稿 (保存后切换时强制注入)。
pub fn merge_preset_config(preset_config: Option<&str>) -> Result<String, CodexConfigError> {
    let mut doc = parse_or_default(&read_config_text()?)?;
    // 清 provider 专属残留 (上一个 relay/official 形态的 custom 表 + 顶层字段),
    // 只留通用段 — 否则旧 base_url/名称会混进新供应商底稿
    if let Some(providers) = doc
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
    {
        providers.remove(SHARED_MODEL_PROVIDER);
    }
    let top = doc.as_table_mut();
    for key in [
        "model_provider",
        "model",
        "model_reasoning_effort",
        "disable_response_storage",
    ] {
        top.remove(key);
    }
    // 预设 provider 段注入
    if let Some(text) = preset_config.filter(|t| !t.trim().is_empty()) {
        let preset = parse_or_default(text)?;
        for key in [
            "model_provider",
            "model",
            "model_reasoning_effort",
            "disable_response_storage",
        ] {
            if let Some(item) = preset.get(key) {
                doc[key] = item.clone();
            }
        }
        if let Some(providers) = preset.get("model_providers").and_then(Item::as_table) {
            if let Some(custom) = providers.get(SHARED_MODEL_PROVIDER) {
                let providers_tbl = doc
                    .entry("model_providers")
                    .or_insert(Item::Table(Table::new()));
                providers_tbl
                    .as_table_like_mut()
                    .ok_or_else(|| CodexConfigError::TomlParse("model_providers 不是表".into()))?
                    .insert(SHARED_MODEL_PROVIDER, custom.clone());
            }
        }
    }
    Ok(doc.to_string())
}

/// 构建 + 落盘中转 config.toml
pub fn write_relay_config(
    display_name: &str,
    base_url: &str,
    model: &str,
    wire_api: Option<&str>,
    model_reasoning_effort: Option<&str>,
    disable_response_storage: bool,
    model_context_window: Option<u64>,
    model_auto_compact_token_limit: Option<u64>,
    custom_config: Option<&str>,
    supported_models: Option<&[String]>,
) -> Result<(), CodexConfigError> {
    let mut doc = build_relay_config(
        display_name,
        base_url,
        model,
        wire_api,
        model_reasoning_effort,
        disable_response_storage,
        model_context_window,
        model_auto_compact_token_limit,
        custom_config,
    )?;
    let has_supported = supported_models.map(|m| !m.is_empty()).unwrap_or(false);
    // DeepSeek 官方网关: 落盘模型目录文件 (字段已由 apply_relay_fields 注入)
    if is_deepseek_official_gateway(base_url, wire_api) {
        write_deepseek_model_catalog()?;
        set_codex_model_catalog_field(&mut doc, true);
    } else if has_supported {
        // 任意中转: 只显示它真实支持的模型, 避免选了不支持的模型提交才报错
        write_relay_model_catalog(supported_models.unwrap_or_default(), model_context_window)?;
        set_codex_model_catalog_field(&mut doc, true);
    } else {
        // 未知模型清单: 移除我们的目录文件, 避免下拉残留上一个供应商的模型
        remove_our_model_catalog()?;
        set_codex_model_catalog_field(&mut doc, false);
    }
    write_config_text(&doc.to_string())
}

/// 当前 config.toml 顶层字段 (model, model_reasoning_effort, disable_response_storage)
pub fn top_level_fields() -> Result<(Option<String>, Option<String>, Option<bool>), CodexConfigError>
{
    let doc = parse_or_default(&read_config_text()?)?;
    Ok((
        doc.get("model").and_then(Item::as_str).map(str::to_string),
        doc.get("model_reasoning_effort")
            .and_then(Item::as_str)
            .map(str::to_string),
        doc.get("disable_response_storage").and_then(Item::as_bool),
    ))
}

fn parse_or_default(text: &str) -> Result<DocumentMut, CodexConfigError> {
    text.parse::<DocumentMut>()
        .map_err(|e| CodexConfigError::TomlParse(e.to_string()))
}

pub(crate) fn write_config_text(text: &str) -> Result<(), CodexConfigError> {
    let path = codex_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    vault::atomic_write_bytes(&path, text.as_bytes()).map_err(CodexConfigError::Vault)
}

/// 当前激活的 profile 类型 (从 config.toml 推断)。
/// 官方与中转共用 custom 桶, 按表形态区分: 官方形态 (无 base_url) → Official;
/// codexff_relay 标记或带 base_url (cc-switch 遗留) → Relay。
pub fn current_profile_kind() -> Result<CurrentProfile, CodexConfigError> {
    let text = read_config_text()?;
    let doc = parse_or_default(&text)?;
    let Some(provider) = doc
        .get("model_provider")
        .and_then(Item::as_str)
        .map(str::to_string)
    else {
        return Ok(CurrentProfile::None);
    };
    if provider == OFFICIAL_MODEL_PROVIDER {
        // 老配置: 原生 openai 桶
        return Ok(CurrentProfile::Official);
    }
    if provider != SHARED_MODEL_PROVIDER {
        return Ok(CurrentProfile::None);
    }
    match doc
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|t| t.get(SHARED_MODEL_PROVIDER))
        .and_then(Item::as_table)
    {
        Some(t) if is_official_custom_table(t) => Ok(CurrentProfile::Official),
        Some(t)
            if t.get("codexff_relay")
                .and_then(Item::as_bool)
                .unwrap_or(false)
                || t.get("base_url").is_some() =>
        {
            Ok(CurrentProfile::Relay)
        }
        _ => Ok(CurrentProfile::None),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CurrentProfile {
    /// 未接管 (用户手动配置或未配置)
    None,
    /// 官方 (原生 openai 桶或共享 custom 桶官方形态)
    Official,
    /// 中转 (共享 custom 桶中转形态; 具体 profile 由 profiles.json 解析)
    Relay,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_switch_preserves_live_common_sections() {
        let _guard = crate::test_util::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("codexff-config-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create home");
        std::env::set_var("CODEX_HOME", &home);

        // 磁盘实时状态: 用户选了自定义宠物 + 安装了新插件
        let disk = r#"
model_provider = "custom"
model = "deepseek-v4-flash"

[model_providers.custom]
name = "DeepSeek"
base_url = "https://api.deepseek.com"
requires_openai_auth = true
codexff_relay = true

[desktop]
conversationDetailMode = "STEPS_COMMANDS"
selected-avatar-id = "custom:susuta--xiangzi529"

[plugins."browser@openai-bundled"]
enabled = true

[plugins."visualize@openai-bundled"]
enabled = true
"#;
        std::fs::write(home.join("config.toml"), disk).expect("write disk config");

        // 底稿是添加供应商时的旧快照: desktop 没有宠物字段, 插件也不全
        let stale_draft = r#"
model_provider = "custom"
model = "deepseek-v4-flash"

[model_providers.custom]
name = "DeepSeek"
base_url = "https://api.deepseek.com"

[desktop]
conversationDetailMode = "STEPS_COMMANDS"

[plugins."browser@openai-bundled"]
enabled = true
"#;

        let doc = build_relay_config(
            "DeepSeek",
            "https://api.deepseek.com",
            "deepseek-v4-flash",
            Some("responses"),
            Some("high"),
            true,
            None,
            None,
            Some(stale_draft),
        )
        .expect("build relay config");

        let text = doc.to_string();
        assert!(
            text.contains("selected-avatar-id = \"custom:susuta--xiangzi529\""),
            "宠物选择应来自磁盘实时状态: {text}"
        );
        assert!(
            text.contains("[plugins.\"visualize@openai-bundled\"]"),
            "新插件应来自磁盘实时状态: {text}"
        );

        std::env::remove_var("CODEX_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}
