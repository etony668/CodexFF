//! 配置一键导入 — 中转站生成的 deeplink URL 或 JSON。
//!
//! deeplink 格式 (对齐 cc-switch):
//! `codexff://v1/import?resource=provider&app=codex&name=xxx&endpoint=xxx&apiKey=xxx&model=xxx&wire_api=xxx`
//!
//! 可选参数 (cc-switch v3.8+): homepage / notes / config (Base64 编码的配置
//! JSON, 对齐 cc-switch: `{"auth": {...}, "config": "<config.toml 文本>"}`) /
//! configFormat (json | toml) / configUrl
//!
//! JSON 格式:
//!   {"name": "...", "endpoint": "...", "apiKey": "...", "model": "...", "wire_api": "..."}

use base64::Engine;
use serde::Deserialize;
use std::net::{IpAddr, ToSocketAddrs};
use url::Url;

use crate::codex_config;

#[derive(Debug, Clone, Deserialize)]
pub struct ImportRequest {
    pub name: Option<String>,
    pub endpoint: Option<String>,
    #[serde(rename = "apiKey", alias = "api_key")]
    pub api_key: Option<String>,
    pub model: Option<String>,
    #[serde(rename = "wire_api", alias = "wireApi")]
    pub wire_api: Option<String>,
    // ---- cc-switch v3.8+ 全量字段 ----
    pub homepage: Option<String>,
    pub notes: Option<String>,
    /// Base64 编码的配置内容 (codex: {"auth": {...}, "config": "<toml>"})
    #[serde(rename = "config")]
    pub config: Option<String>,
    #[serde(rename = "configFormat", alias = "config_format")]
    pub config_format: Option<String>,
    /// 远程配置 URL (cc-switch 也暂不支持, 解析即报错)
    #[serde(rename = "configUrl", alias = "config_url")]
    pub config_url: Option<String>,
    // ---- 余额查询脚本 (cc-switch v3.9+, usage script) ----
    #[serde(rename = "usageScript", alias = "usage_script")]
    pub usage_script: Option<String>,
    #[serde(rename = "usageApiKey", alias = "usage_api_key")]
    pub usage_api_key: Option<String>,
    #[serde(rename = "usageBaseUrl", alias = "usage_base_url")]
    pub usage_base_url: Option<String>,
    #[serde(rename = "usageAccessToken", alias = "usage_access_token")]
    pub usage_access_token: Option<String>,
    #[serde(rename = "usageUserId", alias = "usage_user_id")]
    pub usage_user_id: Option<String>,
    #[serde(rename = "usageAutoInterval", alias = "usage_auto_interval")]
    pub usage_auto_interval: Option<u64>,
}

/// 导入解析出的完整配置: auth.json + config.toml 均已物化 (同 cc-switch —
/// 导入后编辑表单展示真实内容, 而非留空等切换时自动生成)。
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub auth_json: Option<String>,
    pub config_toml: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("无法解析导入内容: {0}")]
    Parse(String),
    #[error("缺少必填字段 endpoint (Base URL)")]
    MissingEndpoint,
    #[error("缺少必填字段 name (名称)")]
    MissingName,
    #[error("缺少必填字段 apiKey (API Key)")]
    MissingApiKey,
    #[error("endpoint 必须是 https 公网地址")]
    BadEndpoint,
}

/// 解析 deeplink URL 或 JSON 文本 → ImportRequest
pub fn parse_import_text(text: &str) -> Result<ImportRequest, ImportError> {
    let text = text.trim();
    if text.starts_with("codexff://") || text.starts_with("ccswitch://") {
        parse_deeplink(text)
    } else if text.starts_with('{') {
        serde_json::from_str::<ImportRequest>(text).map_err(|e| ImportError::Parse(e.to_string()))
    } else {
        Err(ImportError::Parse(
            "既不是 deeplink URL 也不是 JSON".to_string(),
        ))
    }
}

fn parse_deeplink(url: &str) -> Result<ImportRequest, ImportError> {
    let parsed = Url::parse(url).map_err(|e| ImportError::Parse(e.to_string()))?;
    // 兼容 ccswitch:// 格式 (中转站可能生成旧版链接)
    if parsed.scheme() != "codexff" && parsed.scheme() != "ccswitch" {
        return Err(ImportError::Parse(format!(
            "不支持的 scheme: {}",
            parsed.scheme()
        )));
    }
    let resource = parsed
        .query_pairs()
        .find(|(k, _)| k == "resource")
        .map(|(_, v)| v.to_string());
    if resource.as_deref() != Some("provider") {
        return Err(ImportError::Parse("resource 必须是 provider".to_string()));
    }
    let app = parsed
        .query_pairs()
        .find(|(k, _)| k == "app")
        .map(|(_, v)| v.to_string());
    if app.as_deref().map(|a| a.to_ascii_lowercase()).as_deref() != Some("codex") {
        return Err(ImportError::Parse("app 必须是 codex".to_string()));
    }

    let get = |key: &str| -> Option<String> {
        parsed
            .query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.to_string())
            .filter(|v| !v.is_empty())
    };

    Ok(ImportRequest {
        name: get("name"),
        endpoint: get("endpoint").or_else(|| get("base_url")),
        api_key: get("apiKey").or_else(|| get("api_key")),
        model: get("model"),
        wire_api: get("wire_api").or_else(|| get("wireApi")),
        homepage: get("homepage"),
        notes: get("notes"),
        config: get("config"),
        config_format: get("configFormat").or_else(|| get("config_format")),
        config_url: get("configUrl").or_else(|| get("config_url")),
        usage_script: get("usageScript").or_else(|| get("usage_script")),
        usage_api_key: get("usageApiKey").or_else(|| get("usage_api_key")),
        usage_base_url: get("usageBaseUrl").or_else(|| get("usage_base_url")),
        usage_access_token: get("usageAccessToken").or_else(|| get("usage_access_token")),
        usage_user_id: get("usageUserId").or_else(|| get("usage_user_id")),
        usage_auto_interval: get("usageAutoInterval")
            .or_else(|| get("usage_auto_interval"))
            .and_then(|v| v.parse::<u64>().ok()),
    })
}

/// 校验必填字段
pub fn validate(req: &ImportRequest) -> Result<(), ImportError> {
    if req.name.as_deref().map(str::trim).unwrap_or("").is_empty() {
        return Err(ImportError::MissingName);
    }
    let endpoint = req.endpoint.as_deref().map(str::trim).unwrap_or("");
    if endpoint.is_empty() {
        return Err(ImportError::MissingEndpoint);
    }
    fn is_public_ip(ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => !(v4.is_loopback() || v4.is_private() || v4.is_link_local()),
            IpAddr::V6(v6) => {
                !(v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local())
            }
        }
    }

    // 只接受 https + 公网可解析 host — 拒绝 file://、内网、明文 http。
    // 域名在导入时实际解析一次, 防止恶意 deeplink 用域名指向回环/私网。
    if let Ok(parsed) = Url::parse(endpoint) {
        let host_ok = match parsed.host_str() {
            Some(h) => match h.parse::<IpAddr>() {
                Ok(ip) => is_public_ip(ip),
                Err(_) => {
                    // 域名: 解析后必须全部是公网 IP; 解析失败也拒绝
                    (h, 443)
                        .to_socket_addrs()
                        .ok()
                        .map(|addrs| addrs.map(|a| a.ip()).all(is_public_ip))
                        .unwrap_or(false)
                }
            },
            None => false,
        };
        if parsed.scheme() == "https" && host_ok {
            // 通过
        } else {
            return Err(ImportError::BadEndpoint);
        }
    } else {
        return Err(ImportError::BadEndpoint);
    }
    if req
        .api_key
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        return Err(ImportError::MissingApiKey);
    }
    Ok(())
}

/// 解析 config 参数并物化完整配置 (auth.json + config.toml)。
/// config 参数 (cc-switch v3.8+): Base64 编码, 默认 JSON 格式
/// `{"auth": {...}, "config": "<config.toml 文本>"}`; configFormat=toml 时
/// 内容本身是 config.toml; 兼容纯 auth.json 直传 (未 base64) 的站点。
/// config 参数缺失时按表单字段物化 (同 cc-switch 导入后表单展示真实内容)。
pub fn resolve_config(req: &ImportRequest) -> Result<ResolvedConfig, ImportError> {
    // 0. configUrl 远程配置暂不支持 (cc-switch 同) — 独立于 config 参数检查
    if let Some(url) = req.config_url.as_deref().filter(|s| !s.is_empty()) {
        return Err(ImportError::Parse(format!(
            "configUrl 远程配置暂不支持 (cc-switch 同), 请用 config 参数内联配置: {url}"
        )));
    }

    // 1. config 参数 → auth / config 文本
    let mut auth_json: Option<String> = None;
    let mut config_text: Option<String> = None;
    if let Some(raw) = req.config.as_deref().filter(|s| !s.is_empty()) {
        let decoded = decode_config(raw)?;
        match req.config_format.as_deref().unwrap_or("json") {
            "toml" => {
                validate_toml(&decoded)?;
                config_text = Some(decoded);
            }
            _ => {
                let v: serde_json::Value = serde_json::from_str(&decoded)
                    .map_err(|e| ImportError::Parse(format!("config 内容不是合法 JSON: {e}")))?;
                if let Some(auth) = v.get("auth") {
                    auth_json = Some(serde_json::to_string_pretty(auth).map_err(|e| {
                        ImportError::Parse(format!("config 的 auth 无法序列化: {e}"))
                    })?);
                }
                if let Some(cfg) = v.get("config").and_then(serde_json::Value::as_str) {
                    validate_toml(cfg)?;
                    config_text = Some(cfg.to_string());
                }
                if auth_json.is_none() && config_text.is_none() {
                    // 无 auth/config 键 → 整体视为 auth.json 内容
                    auth_json =
                        Some(serde_json::to_string_pretty(&v).map_err(|e| {
                            ImportError::Parse(format!("config 内容无法序列化: {e}"))
                        })?);
                }
            }
        }
    }

    // 2. 补齐缺失的半边 (物化): 不读磁盘当前 config, 以表单字段构建
    let config_toml = match config_text {
        Some(t) => t,
        None => codex_config::materialize_relay_config(
            req.name.as_deref().unwrap_or(""),
            req.endpoint.as_deref().unwrap_or(""),
            req.model.as_deref().unwrap_or(""),
            req.wire_api.as_deref(),
            None, // effort 不物化 — 用户通常在 TOML 自行管理
            true, // 中转站通常要求 disable_response_storage
            None, // 上下文窗口由用户表单设置, 导入不推断
            None,
        )
        .map_err(|e| ImportError::Parse(format!("生成 config.toml 失败: {e}")))?,
    };
    let auth_json = match auth_json {
        Some(a) => Some(a),
        None => {
            let key = req.api_key.as_deref().map(str::trim).unwrap_or("");
            if key.is_empty() {
                None
            } else {
                Some(
                    serde_json::to_string_pretty(&serde_json::json!({ "OPENAI_API_KEY": key }))
                        .map_err(|e| ImportError::Parse(format!("生成 auth.json 失败: {e}")))?,
                )
            }
        }
    };

    Ok(ResolvedConfig {
        auth_json,
        config_toml,
    })
}

/// 从物化后的 auth.json 提取 API key (apiKey 参数缺失时兜底, 同 cc-switch:
/// key 可以只在 config 里)。
pub fn derive_api_key(auth_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(auth_json).ok()?;
    [
        "OPENAI_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
    ]
    .iter()
    .find_map(|k| {
        v.get(*k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    })
}

/// 解码 config 参数: 兼容 base64 (标准/URL-safe, 容忍空白、+→空格、缺 padding)
/// 与纯 JSON 直传 (未 base64 的站点)。
fn decode_config(raw: &str) -> Result<String, ImportError> {
    // 剥离全部空白 (base64 输出可能 76 字符换行 / 手动拷贝带入换行)
    // url crate 不解码 query 里的 +, 无需 cc-switch 的 空格→+ 还原
    let cleaned: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.starts_with('{') {
        return Ok(cleaned);
    }
    let mut padded = cleaned;
    while !padded.len().is_multiple_of(4) {
        padded.push('=');
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&padded)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&padded))
        .map_err(|e| ImportError::Parse(format!("config 参数不是合法 Base64: {e}")))?;
    String::from_utf8(bytes).map_err(|e| ImportError::Parse(format!("config 内容不是 UTF-8: {e}")))
}

fn validate_toml(text: &str) -> Result<(), ImportError> {
    text.parse::<toml_edit::DocumentMut>()
        .map_err(|e| ImportError::Parse(format!("config 里的 TOML 不合法: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn b64(text: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(text)
    }

    #[test]
    fn parse_codexff_deeplink() {
        let url = "codexff://v1/import?resource=provider&app=codex&name=Relay%20A&endpoint=https%3A%2F%2F8.8.8.8%2Fv1&apiKey=sk-abc&model=gpt-5.2-codex&wire_api=responses";
        let req = parse_import_text(url).unwrap();
        assert_eq!(req.name.as_deref(), Some("Relay A"));
        assert_eq!(req.endpoint.as_deref(), Some("https://8.8.8.8/v1"));
        assert_eq!(req.api_key.as_deref(), Some("sk-abc"));
        assert_eq!(req.model.as_deref(), Some("gpt-5.2-codex"));
        assert_eq!(req.wire_api.as_deref(), Some("responses"));
        validate(&req).unwrap();
    }

    #[test]
    fn parse_ccswitch_deeplink() {
        let url = "ccswitch://v1/import?resource=provider&app=codex&name=Legacy&endpoint=https%3A%2F%2F1.1.1.1&apiKey=sk-old";
        let req = parse_import_text(url).unwrap();
        assert_eq!(req.name.as_deref(), Some("Legacy"));
        assert_eq!(req.endpoint.as_deref(), Some("https://1.1.1.1"));
        validate(&req).unwrap();
    }

    #[test]
    fn parse_json() {
        let req = parse_import_text(
            r#"{"name":"J","endpoint":"https://api.j.com/v1","apiKey":"sk-j","model":"m1","wire_api":"chat"}"#,
        )
        .unwrap();
        assert_eq!(req.name.as_deref(), Some("J"));
        assert_eq!(req.wire_api.as_deref(), Some("chat"));
    }

    #[test]
    fn reject_garbage() {
        assert!(parse_import_text("hello world").is_err());
        assert!(parse_import_text("https://evil.com/x").is_err());
    }

    #[test]
    fn missing_required() {
        let req = parse_import_text(
            "codexff://v1/import?resource=provider&app=codex&name=NoKey&endpoint=https%3A%2F%2Fx.com",
        )
        .unwrap();
        assert!(validate(&req).is_err());
    }

    #[test]
    fn resolve_with_config_param() {
        let config_json = r#"{"auth":{"OPENAI_API_KEY":"sk-import"},"config":"model = \"gpt-5.6-codex\"\nenable_goal_mode = true\n"}"#;
        let url = format!(
            "ccswitch://v1/import?resource=provider&app=codex&name=Full&endpoint=https%3A%2F%2Fapi.full.com%2Fv1&apiKey=sk-import&config={}",
            b64(config_json)
        );
        let req = parse_import_text(&url).unwrap();
        let resolved = resolve_config(&req).unwrap();
        // auth.json: config 里的 auth 原样物化
        let auth = resolved.auth_json.unwrap();
        assert!(auth.contains("sk-import"), "auth: {auth}");
        // config.toml: config 里的 TOML 原样保留
        assert!(
            resolved.config_toml.contains("enable_goal_mode = true"),
            "toml: {}",
            resolved.config_toml
        );
        assert!(
            resolved.config_toml.contains("model = \"gpt-5.6-codex\""),
            "toml: {}",
            resolved.config_toml
        );
    }

    #[test]
    fn resolve_tolerates_wrapped_base64() {
        // base64 输出带 76 字符换行 / 手动拷贝带入换行 — 解码前剥离空白
        let config_json = r#"{"auth":{"OPENAI_API_KEY":"sk-wrap"}}"#;
        let b64 = b64(config_json);
        let wrapped: String = b64
            .chars()
            .enumerate()
            .flat_map(|(i, c)| if i % 20 == 19 { vec![c, '\n'] } else { vec![c] })
            .collect();
        let url = format!(
            "ccswitch://v1/import?resource=provider&app=codex&name=Wrap&endpoint=https%3A%2F%2Fapi.wrap.com&apiKey=sk-wrap&config={wrapped}"
        );
        let req = parse_import_text(&url).unwrap();
        let resolved = resolve_config(&req).unwrap();
        assert!(resolved.auth_json.unwrap().contains("sk-wrap"));
    }

    #[test]
    fn resolve_with_plain_json_config() {
        // 未 base64 的纯 JSON 直传
        let config_json = r#"{"auth":{"OPENAI_API_KEY":"sk-plain"}}"#;
        let url = format!(
            "ccswitch://v1/import?resource=provider&app=codex&name=Plain&endpoint=https%3A%2F%2Fapi.plain.com&apiKey=sk-plain&config={}",
            urlencoding(config_json)
        );
        let req = parse_import_text(&url).unwrap();
        let resolved = resolve_config(&req).unwrap();
        assert!(resolved.auth_json.unwrap().contains("sk-plain"));
        assert!(
            resolved.config_toml.contains("api.plain.com"),
            "物化必须含 relay 表"
        );
    }

    fn urlencoding(s: &str) -> String {
        // 测试里用 percent-encoding 简单替代
        s.replace('{', "%7B")
            .replace('}', "%7D")
            .replace('"', "%22")
            .replace(':', "%3A")
            .replace(',', "%2C")
            .replace(' ', "%20")
    }

    #[test]
    fn resolve_toml_format() {
        let toml_text = "model = \"m1\"\n[model_providers.custom]\nbase_url = \"https://x.com\"\n";
        let url = format!(
            "ccswitch://v1/import?resource=provider&app=codex&name=TomlF&endpoint=https%3A%2F%2Fx.com&apiKey=sk-t&configFormat=toml&config={}",
            b64(toml_text)
        );
        let req = parse_import_text(&url).unwrap();
        let resolved = resolve_config(&req).unwrap();
        assert!(
            resolved.config_toml.contains("[model_providers.custom]"),
            "toml: {}",
            resolved.config_toml
        );
        assert!(
            resolved.auth_json.unwrap().contains("sk-t"),
            "auth 从 apiKey 物化"
        );
    }

    #[test]
    fn resolve_materializes_when_no_config_param() {
        let url = "ccswitch://v1/import?resource=provider&app=codex&name=Mat&endpoint=https%3A%2F%2Fapi.mat.com%2Fv1&apiKey=sk-mat&model=gpt-5.2-codex&wire_api=responses";
        let req = parse_import_text(url).unwrap();
        let resolved = resolve_config(&req).unwrap();
        // auth.json 物化: OPENAI_API_KEY = key
        let auth_text = resolved.auth_json.unwrap();
        let auth: serde_json::Value = serde_json::from_str(&auth_text).unwrap();
        assert_eq!(
            auth.get("OPENAI_API_KEY").and_then(|v| v.as_str()),
            Some("sk-mat")
        );
        // config.toml 物化: 完整中转文档 (模型 + relay 表 + 归属标记)
        let toml = resolved.config_toml.as_str();
        assert!(toml.contains("model_provider = \"custom\""), "toml: {toml}");
        assert!(toml.contains("model = \"gpt-5.2-codex\""), "toml: {toml}");
        assert!(
            toml.contains("base_url = \"https://api.mat.com/v1\""),
            "toml: {toml}"
        );
        assert!(toml.contains("codexff_relay = true"), "toml: {toml}");
        assert!(toml.contains("wire_api = \"responses\""), "toml: {toml}");
        assert!(
            toml.contains("disable_response_storage = true"),
            "toml: {toml}"
        );
    }

    #[test]
    fn resolve_rejects_bad_base64_and_config_url() {
        let req = parse_import_text(
            "ccswitch://v1/import?resource=provider&app=codex&name=X&endpoint=https%3A%2F%2Fx.com&apiKey=sk-x&config=%21%21%21",
        )
        .unwrap();
        assert!(resolve_config(&req).is_err(), "坏 base64 必须报错");

        let req = parse_import_text(
            "ccswitch://v1/import?resource=provider&app=codex&name=X&endpoint=https%3A%2F%2Fx.com&apiKey=sk-x&configUrl=https%3A%2F%2Fremote.com%2Fc.json",
        )
        .unwrap();
        assert!(resolve_config(&req).is_err(), "configUrl 必须报错");
    }

    #[test]
    fn derive_key_from_auth() {
        assert_eq!(
            derive_api_key(r#"{"OPENAI_API_KEY":"sk-1"}"#),
            Some("sk-1".to_string())
        );
        assert_eq!(
            derive_api_key(r#"{"ANTHROPIC_AUTH_TOKEN":"t-1"}"#),
            Some("t-1".to_string())
        );
        assert_eq!(derive_api_key(r#"{}"#), None);
        assert_eq!(derive_api_key("not json"), None);
    }
}
