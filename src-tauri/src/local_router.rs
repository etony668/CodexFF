//! 本地路由 — 为 Codex API 请求提供本地 HTTP 代理。
//!
//! - 转发: 把请求按原路径转发给当前激活供应商 (真实 base_url + key 由路由读取)
//! - 故障转移: 主供应商失败时按备用列表顺序尝试 (同 wire_api 的其它供应商)
//! - 熔断: 连续失败 3 次 → 冷却 30 秒, 冷却期跳过该供应商
//! - 用量日志: 从响应/SSE 流中提取 model 与 usage token, 写入 usage_stats

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::profiles::{self, ActiveSelection};
use crate::usage_stats::{self, UsageLogEntry};
use crate::vault;

pub const DEFAULT_PORT: u16 = 19331;
const BREAKER_THRESHOLD: u32 = 3;
const BREAKER_COOLDOWN_SECS: i64 = 30;
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(300);

static RUNTIME: Mutex<Option<RuntimeState>> = Mutex::new(None);
static BREAKER: LazyLock<Mutex<Breaker>> =
    LazyLock::new(|| Mutex::new(Breaker::default()));

struct RuntimeState {
    shutdown: Option<oneshot::Sender<()>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RouterState {
    pub enabled: bool,
    pub port: u16,
    /// 激活的供应商 base_url 是否已被改写为本地路由 (接入开关完成后启用)
    pub rewritten: bool,
    /// 改写前的真实 base_url (关闭/切换时还原)
    pub original_base_url: Option<String>,
    /// 被改写的供应商 id
    pub rewrote_profile: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RouterStatus {
    pub enabled: bool,
    pub port: u16,
    pub rewritten: bool,
    pub active_provider: Option<String>,
}

fn state_path() -> std::path::PathBuf {
    vault::vault_dir().join("router-state.json")
}

pub fn load_state() -> RouterState {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|t| serde_json::from_str::<RouterState>(&t).ok())
        .unwrap_or_default()
}

fn save_state(s: &RouterState) {
    if let Ok(bytes) = serde_json::to_vec_pretty(s) {
        let _ = vault::atomic_write_bytes(&state_path(), &bytes);
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 请求层清洗 + 模型归一化（直通官方 Responses API 的中转会严格校验）:
/// 1. reasoning 条目 encrypted_content 存在（即使 null）时 content 必须为空
///    数组，否则 400 array_above_max_length；
/// 2. 请求 model 不属于当前供应商 → 改写为供应商默认模型，旧会话无需
///    退出 Codex / 改写会话文件即可跨供应商接续。
/// 解析失败或非 responses 请求原样转发。
fn sanitize_responses_body(
    body: &Bytes,
    uri: &Uri,
    fallback_model: Option<&str>,
    supported: &[String],
) -> Bytes {
    let path = uri.path();
    if !path.ends_with("/responses") {
        return body.clone();
    }
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.clone();
    };
    let mut changed = false;
    if let Some(input) = v.get_mut("input").and_then(|i| i.as_array_mut()) {
        for item in input.iter_mut() {
            let Some(obj) = item.as_object_mut() else {
                continue;
            };
            let Some(typ) = obj.get("type").and_then(|t| t.as_str()) else {
                continue;
            };
            match typ {
                "reasoning" => {
                    if obj.contains_key("encrypted_content") {
                        let content_nonempty = obj
                            .get("content")
                            .and_then(|c| c.as_array())
                            .map(|a| !a.is_empty())
                            .unwrap_or(false);
                        if content_nonempty {
                            obj.insert("content".into(), serde_json::json!([]));
                            changed = true;
                        }
                    }
                }
                "function_call"
                | "function_call_output"
                | "custom_tool_call"
                | "custom_tool_call_output" => {
                    if obj.remove("content").is_some() {
                        changed = true;
                    }
                }
                _ => {}
            }
        }
    }
    // 模型归一化: 清单已知 → 不在清单即替换; 清单未知 → 仅官方模型名替换
    // (用户自定义的第三方模型名原样透传, 不误伤)。
    if let Some(cur) = v.get("model").and_then(|m| m.as_str()) {
        let needs_replace = if supported.is_empty() {
            is_official_model_name(cur)
        } else {
            !supported.iter().any(|s| s == cur)
        };
        if needs_replace {
            if let Some(fb) = fallback_model {
                if !fb.is_empty() && fb != cur {
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("model".into(), serde_json::json!(fb));
                        changed = true;
                    }
                }
            }
        }
    }
    if !changed {
        return body.clone();
    }
    serde_json::to_vec(&v)
        .map(Bytes::from)
        .unwrap_or_else(|_| body.clone())
}

/// 是否官方模型名（清单未知时只替换官方模型，避免误伤自定义模型）。
fn is_official_model_name(m: &str) -> bool {
    let lower = m.to_ascii_lowercase();
    crate::session_model::OFFICIAL_MODELS
        .iter()
        .any(|s| s.eq_ignore_ascii_case(m))
        || lower.starts_with("gpt-5")
        || (lower.starts_with('o')
            && lower[1..]
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false))
}

/// mpsc 接收端包装成 axum 可用的 Stream（响应逐块回传，不整包缓冲）。
struct RxStream {
    rx: mpsc::Receiver<Result<Bytes, std::io::Error>>,
}

impl Stream for RxStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

/// 熔断器: 连续失败计数 + 冷却期
#[derive(Debug, Default)]
struct Breaker {
    failures: HashMap<String, u32>,
    opened_until: HashMap<String, i64>,
}

impl Breaker {
    fn is_open(&self, id: &str) -> bool {
        self.opened_until
            .get(id)
            .map(|t| now_ms() < *t)
            .unwrap_or(false)
    }
    fn record_failure(&mut self, id: &str) {
        let n = self.failures.entry(id.to_string()).or_insert(0);
        *n += 1;
        if *n >= BREAKER_THRESHOLD {
            self.opened_until
                .insert(id.to_string(), now_ms() + BREAKER_COOLDOWN_SECS * 1000);
            *n = 0;
        }
    }
    fn record_success(&mut self, id: &str) {
        self.failures.remove(id);
        self.opened_until.remove(id);
    }
}

/// 备用供应商: 同 wire_api 且非当前激活, 保持 profile 列表顺序
fn fallback_chain<'a>(
    active: &'a ActiveSelection,
    relays: &'a [profiles::RelayProfile],
) -> Vec<&'a profiles::RelayProfile> {
    let (active_id, wire) = match active {
        ActiveSelection::Relay { profile_id } => {
            let Some(p) = relays.iter().find(|r| r.id == *profile_id) else {
                return Vec::new();
            };
            (profile_id.as_str(), p.wire_api.as_deref().unwrap_or("openai_chat"))
        }
        ActiveSelection::Official => return Vec::new(),
    };
    relays
        .iter()
        .filter(|r| r.id != active_id && r.wire_api.as_deref().unwrap_or("openai_chat") == wire)
        .collect()
}

/// 已知模型单价 (元 / 百万 token), 前缀匹配; 未收录返回 None
pub(crate) fn model_price(model: &str) -> Option<(f64, f64)> {
    let m = model.to_ascii_lowercase();
    let table: &[(&str, f64, f64)] = &[
        ("deepseek-chat", 2.0, 8.0),
        ("deepseek-reasoner", 4.0, 16.0),
        ("gpt-4o-mini", 1.05, 4.2),
        ("gpt-4o", 17.5, 70.0),
        ("gpt-4.1", 15.4, 61.6),
        ("claude-3-5-sonnet", 22.4, 112.0),
        ("claude-3-7-sonnet", 22.4, 112.0),
        ("claude-3-haiku", 1.75, 8.75),
        ("moonshot", 8.0, 32.0),
        ("kimi", 8.0, 32.0),
        ("qwen-max", 16.0, 64.0),
        ("qwen-plus", 2.8, 11.2),
        ("qwen-turbo", 0.7, 2.8),
        ("glm-4", 0.7, 2.8),
        ("glm-4v", 9.1, 36.4),
        ("ernie", 0.35, 2.8),
        ("gemini-1.5", 3.5, 10.5),
        ("gemini-2.0", 8.4, 26.6),
        ("gemini-2.5", 8.4, 26.6),
    ];
    for (prefix, pin, pout) in table {
        if m.starts_with(prefix) {
            return Some((*pin, *pout));
        }
    }
    None
}

pub(crate) fn estimate_cost(
    model: Option<&str>,
    prompt: Option<u64>,
    completion: Option<u64>,
) -> Option<f64> {
    let model = model?;
    let (pin, pout) = model_price(model)?;
    let mut cost = 0.0;
    if let Some(p) = prompt {
        cost += p as f64 / 1_000_000.0 * pin;
    }
    if let Some(c) = completion {
        cost += c as f64 / 1_000_000.0 * pout;
    }
    Some(cost)
}

/// 从响应文本中提取 usage/model 字段 (兼容 SSE 与 JSON 响应)
fn extract_usage_from_text(text: &str) -> (Option<String>, Option<u64>, Option<u64>, Option<u64>) {
    let mut model = None;
    let mut usage = None;
    for line in text.split('\n') {
        if model.is_none() {
            if let Some(idx) = line.find("\"model\"") {
                if let Some(colon) = line[idx..].find(':') {
                    let rest = &line[idx + colon + 1..];
                    let rest = rest.trim();
                    if let Some(stripped) = rest.strip_prefix('"') {
                        if let Some(end) = stripped.find('"') {
                            let s = &stripped[..end];
                            if !s.is_empty() && s.len() < 128 {
                                model = Some(s.to_string());
                            }
                        }
                    }
                }
            }
        }
        if usage.is_none() {
            if let Some(idx) = line.find("\"usage\"") {
                let rest = &line[idx..];
                if let Some(start) = rest.find('{') {
                    let mut depth = 0i32;
                    for (i, ch) in rest[start..].char_indices() {
                        match ch {
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    let end = start + i + ch.len_utf8();
                                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(
                                        &rest[start..end],
                                    ) {
                                        usage = Some(v);
                                    }
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        if usage.is_some() && model.is_some() {
            break;
        }
    }
    let (prompt, completion, total) = usage.map(|u| {
        (
            u.get("prompt_tokens").and_then(|v| v.as_u64()),
            u.get("completion_tokens").and_then(|v| v.as_u64()),
            u.get("total_tokens").and_then(|v| v.as_u64()),
        )
    }).unwrap_or((None, None, None));
    (model, prompt, completion, total)
}

fn active_relay() -> Option<(profiles::RelayProfile, String)> {
    let relays = profiles::list_relay_profiles().ok()?;
    let active = profiles::current_active().ok()?;
    let ActiveSelection::Relay { profile_id } = active else {
        return None;
    };
    let profile = relays.iter().find(|r| r.id == profile_id)?.clone();
    let key = vault::get_relay_key(&profile_id).ok()??;
    Some((profile, key))
}

fn client() -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .timeout(UPSTREAM_TIMEOUT)
        .user_agent("codexff-router");
    // 与 DNS 守护同一套系统代理发现: 机场设置了 HTTP/SOCKS 代理时
    // 上游请求也走代理 (TUN 模式则无需代理, 直连即进隧道)
    if let Some(url) = crate::official_quota::system_proxy_url() {
        if let Ok(mut proxy) = reqwest::Proxy::all(&url) {
            proxy =
                proxy.no_proxy(reqwest::NoProxy::from_string("localhost,127.0.0.1,::1"));
            builder = builder.proxy(proxy);
        }
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

/// 读取当前 config.toml 中 custom 供应商的 base_url
fn current_config_base_url() -> Option<String> {
    let text = crate::codex_config::read_config_text().ok()?;
    let doc: toml_edit::DocumentMut = text.parse().ok()?;
    doc.get("model_providers")?
        .get("custom")?
        .get("base_url")?
        .as_str()
        .map(|s| s.to_string())
}

/// 改写/还原 config.toml 的 custom.base_url
fn write_config_base_url(base: Option<&str>) -> Result<(), String> {
    let text = crate::codex_config::read_config_text().map_err(|e| e.to_string())?;
    let mut doc: toml_edit::DocumentMut =
        text.parse::<toml_edit::DocumentMut>().map_err(|e| e.to_string())?;
    let providers = doc
        .entry("model_providers")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let custom = providers
        .as_table_mut()
        .ok_or("model_providers 不是表")?
        .entry("custom")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let tbl = custom.as_table_mut().ok_or("custom 不是表")?;
    match base {
        Some(b) => {
            tbl.insert("base_url", toml_edit::value(b));
        }
        None => {
            tbl.remove("base_url");
        }
    }
    crate::codex_config::write_config_text(&doc.to_string()).map_err(|e| e.to_string())
}

/// 按状态还原真实 base_url
fn restore_config(s: &mut RouterState) {
    if !s.rewritten {
        return;
    }
    if let Some(orig) = s.original_base_url.clone() {
        let _ = write_config_base_url(Some(&orig));
    } else {
        let _ = write_config_base_url(None);
    }
    s.rewritten = false;
    s.original_base_url = None;
    s.rewrote_profile = None;
    save_state(s);
}

/// 路由开启时, 把当前激活的中转供应商 base_url 改写为本地代理地址。
/// 激活/切换供应商后调用, 保证 Codex 请求始终走本地路由。
pub fn sync_active() {
    let mut s = load_state();
    if !s.enabled {
        return;
    }
    // 先还原旧的改写 (若激活供应商已变化)
    restore_config(&mut s);
    if let Some((profile, _key)) = active_relay() {
        let port = if s.port == 0 { DEFAULT_PORT } else { s.port };
        let proxy_url = format!("http://127.0.0.1:{port}/v1");
        let orig = current_config_base_url().unwrap_or_else(|| profile.base_url.clone());
        if orig != proxy_url {
            if write_config_base_url(Some(&proxy_url)).is_ok() {
                s.rewritten = true;
                s.original_base_url = Some(orig);
                s.rewrote_profile = Some(profile.id);
                save_state(&s);
            }
        }
    }
}

async fn forward(
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if active_relay().is_none() {
        return Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::from("no active relay provider"))
            .unwrap();
    }
    let chain: Vec<profiles::RelayProfile> = {
        let relays = profiles::list_relay_profiles().unwrap_or_default();
        let active = profiles::current_active().ok();
        let mut chain = Vec::new();
        if let Some(ActiveSelection::Relay { profile_id }) = &active {
            if let Some(p) = relays.iter().find(|r| r.id == *profile_id) {
                chain.push(p.clone());
            }
        }
        for p in fallback_chain(
            &profiles::current_active().unwrap_or(ActiveSelection::Official),
            &relays,
        ) {
            chain.push(p.clone());
        }
        chain
    };
    let mut last_error: Option<String> = None;
    let mut attempted = Vec::new();
    for p in chain.iter() {
        let breaker_open = BREAKER
            .lock()
            .map(|b| b.is_open(&p.id))
            .unwrap_or(false);
        if breaker_open {
            continue;
        }
        let Some(pkey) = vault::get_relay_key(&p.id).ok().flatten() else {
            continue;
        };
        attempted.push(p.id.clone());
        // 每个候选供应商用自己的默认模型/模型清单做请求层清洗+归一化
        // (fallback chain 里各供应商模型不同, 不能复用同一个 body)。
        let body =
            sanitize_responses_body(&body, &uri, Some(p.model.as_str()), &p.supported_models);
        // 本地路由 base_url 统一写成 http://127.0.0.1:PORT/v1, Codex 会请求
        // /v1/responses; 转发时必须剥掉 /v1 前缀, 还原成直连形态
        // (base_url 无 /v1 的 DeepSeek/皮卡丘 = /responses, 带 /v1 的 =
        // /v1/responses), 否则根路径直达的中转会 404。
        let raw_path = uri
            .path_and_query()
            .map(|q| q.as_str())
            .unwrap_or("");
        let forward_path = raw_path.strip_prefix("/v1").unwrap_or(raw_path);
        let url = format!("{}{}", p.base_url.trim_end_matches('/'), forward_path);
    let http = client();
    let mut req = http
            .request(method.clone(), &url)
            .header("authorization", format!("Bearer {pkey}"))
            .header("content-type", headers.get("content-type").map(|v| v.to_str().unwrap_or("application/json")).unwrap_or("application/json"))
            .header("accept", headers.get("accept").map(|v| v.to_str().unwrap_or("application/json")).unwrap_or("application/json"))
            .body(body.clone());
        if let Some(ua) = headers.get("user-agent") {
            if let Ok(s) = ua.to_str() {
                req = req.header("user-agent", s);
            }
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status >= 500 || status == 429 {
                    let _ = BREAKER.lock().map(|mut b| b.record_failure(&p.id));
                    last_error = Some(format!("upstream {} {}", p.id, status));
                    continue;
                }
                let _ = BREAKER.lock().map(|mut b| b.record_success(&p.id));
                let ct = resp
                    .headers()
                    .get("content-type")
                    .map(|v| v.to_str().unwrap_or("application/json").to_string())
                    .unwrap_or_else(|| "application/json".to_string());
                // 流式转发 (SSE tee): 逐块回给客户端, 同时采样 usage/model。
                let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);
                let sampler: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
                let sampler_task = Arc::clone(&sampler);
                let stream = resp.bytes_stream();
                let pid = p.id.clone();
                let pname = p.name.clone();
                let wire = p.wire_api.clone();
                tokio::spawn(async move {
                    let mut stream = stream;
                    const SAMPLE_CAP: usize = 8 * 1024 * 1024;
                    while let Some(item) = stream.next().await {
                        match item {
                            Ok(b) => {
                                {
                                    let mut buf = sampler_task
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner());
                                    if buf.len() < SAMPLE_CAP {
                                        buf.extend_from_slice(&b);
                                    }
                                }
                                if tx.send(Ok(b)).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                let _ = tx
                                    .send(Err(std::io::Error::other(e.to_string())))
                                    .await;
                                break;
                            }
                        }
                    }
                    let buf = sampler_task.lock().map(|g| g.clone()).unwrap_or_default();
                    let text = String::from_utf8_lossy(&buf);
                    let (model, prompt, completion, total) = extract_usage_from_text(&text);
                    let cost = estimate_cost(model.as_deref(), prompt, completion);
                    usage_stats::append_usage_log(UsageLogEntry {
                        ts_ms: now_ms(),
                        provider_id: pid,
                        provider_name: pname,
                        model,
                        wire_api: wire,
                        prompt_tokens: prompt,
                        completion_tokens: completion,
                        total_tokens: total,
                        cost,
                        status,
                        error: None,
                    });
                });
                return Response::builder()
                    .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
                    .header("content-type", ct)
                    .body(Body::from_stream(RxStream { rx }))
                    .unwrap();
            }
            Err(e) => {
                let _ = BREAKER.lock().map(|mut b| b.record_failure(&p.id));
                last_error = Some(e.to_string());
            }
        }
    }
    // 全部失败 → 记录日志 (失败请求)
    usage_stats::append_usage_log(UsageLogEntry {
        ts_ms: now_ms(),
        provider_id: attempted.first().cloned().unwrap_or_default(),
        provider_name: attempted.first().cloned().unwrap_or_default(),
        model: None,
        wire_api: None,
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: None,
        cost: None,
        status: 0,
        error: last_error,
    });
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .body(Body::from("all upstream providers failed"))
        .unwrap()
}

pub fn status() -> RouterStatus {
    let state = load_state();
    RouterStatus {
        enabled: state.enabled && RUNTIME.lock().map(|r| r.is_some()).unwrap_or(false),
        port: state.port,
        rewritten: state.rewritten,
        active_provider: profiles::current_active().ok().map(|a| match a {
            ActiveSelection::Relay { profile_id } => profile_id,
            ActiveSelection::Official => "official".to_string(),
        }),
    }
}

/// App 退出时同步收尾: 还原 base_url + 停止代理 + 标记关闭
pub fn shutdown() {
    if let Some(rt) = RUNTIME.lock().unwrap_or_else(|e| e.into_inner()).take() {
        if let Some(tx) = rt.shutdown {
            let _ = tx.send(());
        }
    }
    let mut s = load_state();
    restore_config(&mut s);
    s.enabled = false;
    save_state(&s);
}

/// 启动/停止本地路由 (接入开关完成后, 还会改写激活供应商 base_url)
pub async fn set_enabled(enabled: bool) -> Result<RouterStatus, String> {
    if enabled {
        if RUNTIME.lock().map(|r| r.is_some()).unwrap_or(false) {
            return Ok(status());
        }
        let port = {
            let mut s = load_state();
            s.enabled = true;
            s.port = if s.port == 0 { DEFAULT_PORT } else { s.port };
            save_state(&s);
            s.port
        };
        let (tx, rx) = oneshot::channel::<()>();
        let app = axum::Router::new().fallback(handler);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| e.to_string())?;
        let server = axum::serve(listener, app).with_graceful_shutdown(async {
            let _ = rx.await;
        });
        let _handle = tokio::spawn(async move {
            let _ = server.await;
        });
        *RUNTIME.lock().unwrap_or_else(|e| e.into_inner()) = Some(RuntimeState {
            shutdown: Some(tx),
        });
        sync_active();
    } else {
        let mut s = load_state();
        restore_config(&mut s);
        s.enabled = false;
        save_state(&s);
        if let Some(rt) = RUNTIME.lock().unwrap_or_else(|e| e.into_inner()).take() {
            if let Some(tx) = rt.shutdown {
                let _ = tx.send(());
            }
        }
    }
    Ok(status())
}

async fn handler(
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let bytes = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("body too large"))
                .unwrap()
        }
    };
    forward(method, uri, headers, bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rx_stream_delivers_chunks_in_order() {
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(4);
        tx.send(Ok(Bytes::from_static(b"data: {\"a\":1}\n\n")))
            .await
            .unwrap();
        tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n")))
            .await
            .unwrap();
        drop(tx);
        let mut s = RxStream { rx };
        let mut got = Vec::new();
        while let Some(chunk) = s.next().await {
            got.push(chunk.unwrap().to_vec());
        }
        assert_eq!(got.len(), 2);
        assert_eq!(got[1], b"data: [DONE]\n\n");
    }

    #[test]
    fn responses_body_reasoning_content_emptied() {
        let body = Bytes::from(
            r#"{"model":"gpt-5.5","input":[
                {"type":"reasoning","id":"r1","summary":[],"content":[{"type":"reasoning_text","text":"x"}],"encrypted_content":null},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"function_call","id":"f1","name":"x","arguments":"{}","content":[]}
            ]}"#,
        );
        let uri: Uri = "/v1/responses".parse().unwrap();
        let out = sanitize_responses_body(&body, &uri, None, &[]);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let input = v.get("input").unwrap().as_array().unwrap();
        assert_eq!(
            input[0].get("content").unwrap().as_array().unwrap().len(),
            0
        );
        assert!(input[2].get("content").is_none());
        // 非 responses 路径原样
        let uri2: Uri = "/v1/chat/completions".parse().unwrap();
        assert_eq!(sanitize_responses_body(&body, &uri2, None, &[]), body);
    }

    #[test]
    fn responses_model_rewritten_per_supported_list() {
        let body = Bytes::from(
            r#"{"model":"gpt-5.6-sol","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
        );
        let uri: Uri = "/v1/responses".parse().unwrap();
        // 清单已知且不含当前模型 → 替换为供应商默认
        let out = sanitize_responses_body(
            &body,
            &uri,
            Some("deepseek-v4-flash"),
            &["deepseek-v4-flash".into(), "deepseek-v4-pro".into()],
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v.get("model").unwrap(), "deepseek-v4-flash");
        // 清单未知 + 官方模型名 → 替换
        let out2 = sanitize_responses_body(&body, &uri, Some("gpt-5.5"), &[]);
        let v2: serde_json::Value = serde_json::from_slice(&out2).unwrap();
        assert_eq!(v2.get("model").unwrap(), "gpt-5.5");
        // 清单未知 + 自定义模型 → 原样透传
        let body3 = Bytes::from(
            r#"{"model":"my-custom-llm","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
        );
        let out3 = sanitize_responses_body(&body3, &uri, Some("gpt-5.5"), &[]);
        assert_eq!(out3, body3);
    }

    #[test]
    fn breaker_opens_after_threshold() {
        let mut b = Breaker::default();
        assert!(!b.is_open("p"));
        b.record_failure("p");
        b.record_failure("p");
        b.record_failure("p");
        assert!(b.is_open("p"));
        b.record_success("p");
        assert!(!b.is_open("p"));
    }

    #[test]
    fn fallback_same_wire() {
        let active = ActiveSelection::Relay {
            profile_id: "a".to_string(),
        };
        let relays = vec![
            profiles::RelayProfile {
                id: "a".into(),
                wire_api: Some("openai_chat".into()),
                ..Default::default()
            },
            profiles::RelayProfile {
                id: "b".into(),
                wire_api: Some("openai_chat".into()),
                ..Default::default()
            },
            profiles::RelayProfile {
                id: "c".into(),
                wire_api: Some("anthropic".into()),
                ..Default::default()
            },
        ];
        let chain = fallback_chain(&active, &relays);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].id, "b");
    }

    #[test]
    fn usage_extraction_from_sse() {
        let sse = r#"data: {"id":"x","model":"deepseek-chat","choices":[]}

data: {"id":"x","usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150}}

data: [DONE]
"#;
        let (m, p, c, t) = extract_usage_from_text(sse);
        assert_eq!(m.as_deref(), Some("deepseek-chat"));
        assert_eq!(p, Some(100));
        assert_eq!(c, Some(50));
        assert_eq!(t, Some(150));
    }

    #[test]
    fn cost_from_known_model() {
        let c = estimate_cost(Some("deepseek-chat"), Some(1_000_000), Some(1_000_000));
        assert_eq!(c, Some(10.0));
    }
}
