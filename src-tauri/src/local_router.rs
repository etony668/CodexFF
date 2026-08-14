//! 本地路由 — 为 Codex API 请求提供本地 HTTP 代理。
//!
//! - 转发: 把请求按原路径转发给当前激活供应商 (真实 base_url + key 由路由读取)
//! - 故障转移: 主供应商失败时按备用列表顺序尝试 (同 wire_api 的其它供应商)
//! - 熔断: 连续失败 3 次 → 冷却 30 秒, 冷却期跳过该供应商
//! - 用量日志: 从响应/SSE 流中提取 model 与 usage token, 写入 usage_stats

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{LazyLock, Mutex};
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
const UPSTREAM_ERROR_BODY_LIMIT: usize = 8 * 1024;
const UPSTREAM_ERROR_TEXT_LIMIT: usize = 1200;

static RUNTIME: Mutex<Option<RuntimeState>> = Mutex::new(None);
static BREAKER: LazyLock<Mutex<Breaker>> = LazyLock::new(|| Mutex::new(Breaker::default()));
/// 各中转对 Responses reasoning 条目的实际校验策略。
/// true = OpenAI 严格形态（encrypted_content 存在时 content=[]）；
/// false = thinking 兼容形态（保留 reasoning_text，已空的旧条目直接丢弃）。
/// 同一聚合站可能随上游模型切换规则，因此根据明确 400 响应动态学习。
static REASONING_POLICY: LazyLock<Mutex<HashMap<String, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct RuntimeState {
    shutdown: Option<oneshot::Sender<()>>,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RouterState {
    pub enabled: bool,
    pub port: u16,
    /// 因旧会话模型不兼容而自动开启；用户手动开启时为 false。
    #[serde(default)]
    pub automatic: bool,
    /// 激活的供应商 base_url 是否已被改写为本地路由 (接入开关完成后启用)
    pub rewritten: bool,
    /// 改写前的真实 base_url (关闭/切换时还原)
    pub original_base_url: Option<String>,
    /// 被改写的供应商 id
    pub rewrote_profile: Option<String>,
    /// 上次恢复/接管失败。此状态必须保留到完成验证后的恢复，不能静默清理。
    #[serde(default)]
    pub degraded: bool,
    #[serde(default)]
    pub recovery_message: Option<String>,
    /// App 被外部终止时 Codex 仍可能缓存 localhost。下次启动必须先恢复
    /// 监听与接管，不能仅依赖已经还原为真实地址的磁盘配置。
    #[serde(default)]
    pub resume_after_restart: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RouterStatus {
    pub enabled: bool,
    pub port: u16,
    pub rewritten: bool,
    pub automatic: bool,
    pub degraded: bool,
    pub recovery_message: Option<String>,
    pub active_provider: Option<String>,
    /// 最近一次实际转发到备用供应商 (故障转移): (provider_id, ts_ms)
    /// 前端据此提示用户当前走的是备用中转。
    #[serde(default)]
    pub last_fallback: Option<(String, i64)>,
}

static LAST_FALLBACK: Mutex<Option<(String, i64)>> = Mutex::new(None);

fn record_fallback(provider_id: &str) {
    *LAST_FALLBACK.lock().unwrap_or_else(|e| e.into_inner()) =
        Some((provider_id.to_string(), now_ms()));
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

fn save_state(s: &RouterState) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?;
    vault::atomic_write_bytes(&state_path(), &bytes).map_err(|e| e.to_string())
}

/// Persist a degraded state without changing listener/config ownership.
/// Used when an outer provider transaction cannot prove its snapshot was
/// completely restored; keeping the listener alive avoids manufacturing 502s
/// while making the unsafe state visible to status/tray callers.
pub fn mark_degraded(reason: impl Into<String>) -> Result<RouterStatus, String> {
    let mut state = load_state();
    state.enabled = state.enabled || listener_running();
    state.degraded = true;
    state.recovery_message = Some(reason.into());
    save_state(&state)?;
    Ok(status())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 请求层清洗 + 模型归一化（直通官方 Responses API 的中转会严格校验）:
/// 1. GPT 类中转 (empty_reasoning_content=true): reasoning 条目
///    encrypted_content 存在（即使 null）时 content 必须为空数组，
///    否则 400 array_above_max_length；
///    DeepSeek (empty_reasoning_content=false): 官方 API 与 OpenAI schema
///    相反，要求 thinking 模式的 reasoning_text 原样回传；上一轮清洗
///    已把 content 清空的条目无法重建，直接丢弃避免 400；
/// 2. 请求 model 不属于当前供应商 → 改写为供应商默认模型，旧会话无需
///    退出 Codex / 改写会话文件即可跨供应商接续。
/// 解析失败或非 responses 请求原样转发。
fn sanitize_responses_body(
    body: &Bytes,
    uri: &Uri,
    fallback_model: Option<&str>,
    fallback_effort: Option<&str>,
    supported: &[String],
    empty_reasoning_content: bool,
) -> Bytes {
    let path = uri.path();
    if !is_responses_path(path) {
        return body.clone();
    }
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.clone();
    };
    let mut changed = false;
    if let Some(input) = v.get_mut("input").and_then(|i| i.as_array_mut()) {
        let mut drop_idx = Vec::new();
        for (idx, item) in input.iter_mut().enumerate() {
            let Some(obj) = item.as_object_mut() else {
                continue;
            };
            let Some(typ) = obj.get("type").and_then(|t| t.as_str()) else {
                continue;
            };
            match typ {
                "reasoning" => {
                    let content_empty = obj
                        .get("content")
                        .and_then(|c| c.as_array())
                        .map(|a| a.is_empty())
                        .unwrap_or(true);
                    if empty_reasoning_content {
                        if obj.contains_key("encrypted_content") && !content_empty {
                            obj.insert("content".into(), serde_json::json!([]));
                            changed = true;
                        }
                    } else if content_empty {
                        // DeepSeek 要求 thinking 的 reasoning_text 回传；
                        // 内容已被上一轮清洗清空时无法重建，丢弃该条目。
                        drop_idx.push(idx);
                        changed = true;
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
                // Responses API 要求 web_search_call 的 id 以 "ws_" 开头;
                // Codex 落盘的 web_search_call 用的是 call_ 前缀, 直通官方
                // 校验会被拒 (invalid_id_prefix)。改写为 ws_ + 原 id 后缀。
                "web_search_call" => {
                    if let Some(id) = obj.get("id").and_then(|v| v.as_str()) {
                        if let Some(suffix) = id.strip_prefix("call_") {
                            obj.insert("id".into(), serde_json::json!(format!("ws_{suffix}")));
                            changed = true;
                        }
                    }
                }
                _ => {}
            }
        }
        for &i in drop_idx.iter().rev() {
            input.remove(i);
        }
    }
    // 模型归一化: 清单已知 → 不在清单即替换；清单未知 → 统一使用目标
    // 供应商明确配置的默认模型。兼容路由的职责就是保证跨供应商续接，
    // 不能把上一个供应商的自定义模型名原样发给能力未知的新供应商。
    if let Some(cur) = v.get("model").and_then(|m| m.as_str()) {
        let needs_replace = if supported.is_empty() {
            fallback_model
                .map(|fallback| !fallback.is_empty() && fallback != cur)
                .unwrap_or(false)
        } else {
            !supported.iter().any(|s| s == cur)
        };
        if needs_replace {
            if let Some(fb) = fallback_model {
                if !fb.is_empty() && fb != cur {
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("model".into(), serde_json::json!(fb));
                        match obj.get_mut("reasoning").and_then(|r| r.as_object_mut()) {
                            Some(reasoning) => {
                                if let Some(effort) =
                                    fallback_effort.filter(|e| !e.trim().is_empty())
                                {
                                    reasoning.insert("effort".into(), serde_json::json!(effort));
                                } else {
                                    reasoning.remove("effort");
                                }
                            }
                            None => {
                                if let Some(effort) =
                                    fallback_effort.filter(|e| !e.trim().is_empty())
                                {
                                    obj.insert(
                                        "reasoning".into(),
                                        serde_json::json!({ "effort": effort }),
                                    );
                                }
                            }
                        }
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

/// DeepSeek 官方 Responses API 与 OpenAI schema 相反: thinking 模式要求
/// reasoning_text 原样回传，清空会 400。模型名带 deepseek 或 base_url
/// 指向 deepseek（含聚合站托管 deepseek 模型）→ 按 DeepSeek 规则透传。
pub(crate) fn relay_is_deepseek(p: &profiles::RelayProfile) -> bool {
    let model = p.model.to_ascii_lowercase();
    let url = p.base_url.to_ascii_lowercase();
    model.starts_with("deepseek") || url.contains("deepseek")
}

fn reasoning_policy_for(p: &profiles::RelayProfile) -> bool {
    REASONING_POLICY
        .lock()
        .ok()
        .and_then(|m| m.get(&p.id).copied())
        .unwrap_or_else(|| !relay_is_deepseek(p))
}

fn learn_reasoning_policy(provider_id: &str, empty_content: bool) {
    if let Ok(mut policies) = REASONING_POLICY.lock() {
        policies.insert(provider_id.to_string(), empty_content);
    }
}

/// 只识别两类明确、互斥的 reasoning schema 错误。返回下一次重试策略；
/// 其它 400 不重试，避免重复请求或掩盖真实错误。
fn reasoning_error_policy(body: &[u8], current: bool) -> Option<bool> {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    if text.contains("reasoning_text")
        && (text.contains("must be passed back") || text.contains("thinking mode"))
    {
        return current.then_some(false);
    }
    if text.contains("array_above_max_length")
        || (text.contains("maximum length 0") && text.contains("content"))
    {
        return (!current).then_some(true);
    }
    None
}

fn reasoning_retry_policy(status: u16, body: &[u8], current: bool) -> Option<bool> {
    (status == 400)
        .then(|| reasoning_error_policy(body, current))
        .flatten()
}

/// SSE 中只要出现正文、工具调用或完成事件，就认为上游已经开始产生有效结果。
/// 此后即便流尾返回 schema 错误也不得自动重发，避免重复生成与重复计费。
fn sse_has_valid_output(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    [
        "response.output_text.delta",
        "response.output_item.added",
        "response.content_part.added",
        "response.function_call_arguments.delta",
        "response.completed",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

struct BufferedStream {
    prefix: Bytes,
    stream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
}

enum InitialStream {
    Forward(BufferedStream),
    Retry(bool),
}

/// 中转有时以 HTTP 200 建立 SSE，随后才通过 event:error/response.failed
/// 返回 reasoning schema 错误。先缓存一个很小的起始窗口；仅在尚无有效
/// 输出时允许重试一次。缓存上限防止异常服务一直不发标准事件而占用内存。
async fn inspect_initial_sse(resp: reqwest::Response, current_policy: bool) -> InitialStream {
    const PREFIX_CAP: usize = 256 * 1024;
    let mut stream = Box::pin(resp.bytes_stream());
    let mut prefix = Vec::new();
    while prefix.len() < PREFIX_CAP {
        match stream.next().await {
            Some(Ok(chunk)) => {
                prefix.extend_from_slice(&chunk);
                if sse_has_valid_output(&prefix) {
                    break;
                }
                if let Some(next) = reasoning_error_policy(&prefix, current_policy) {
                    return InitialStream::Retry(next);
                }
                // 一个完整 SSE 事件已到达但只是 created/in_progress，继续等
                // 后续首个有效输出或明确错误。
            }
            Some(Err(e)) => {
                let msg = e.to_string();
                prefix.extend_from_slice(msg.as_bytes());
                break;
            }
            None => break,
        }
    }
    InitialStream::Forward(BufferedStream {
        prefix: Bytes::from(prefix),
        stream,
    })
}

fn response_from_buffered_stream(
    status: u16,
    content_type: String,
    buffered: BufferedStream,
    provider: profiles::RelayProfile,
) -> Response {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    tokio::spawn(async move {
        const SAMPLE_CAP: usize = 8 * 1024 * 1024;
        let mut sample = Vec::new();
        if !buffered.prefix.is_empty() {
            sample.extend_from_slice(&buffered.prefix);
            if tx.send(Ok(buffered.prefix)).await.is_err() {
                return;
            }
        }
        let mut stream = buffered.stream;
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => {
                    if sample.len() < SAMPLE_CAP {
                        let remaining = SAMPLE_CAP - sample.len();
                        sample.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
                    }
                    if tx.send(Ok(bytes)).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
                    break;
                }
            }
        }
        let text = String::from_utf8_lossy(&sample);
        let (model, prompt, completion, total) = extract_usage_from_text(&text);
        let cost = estimate_cost(model.as_deref(), prompt, completion);
        usage_stats::append_usage_log(UsageLogEntry {
            ts_ms: now_ms(),
            provider_id: provider.id,
            provider_name: provider.name,
            model,
            wire_api: provider.wire_api,
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: total,
            cost,
            status,
            error: None,
        });
    });
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
        .header("content-type", content_type)
        .body(Body::from_stream(RxStream { rx }))
        .unwrap()
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

fn model_family(p: &profiles::RelayProfile) -> &'static str {
    let model = p.model.to_ascii_lowercase();
    if relay_is_deepseek(p) {
        "deepseek"
    } else if model.starts_with("gpt-")
        || model.starts_with("codex")
        || model
            .strip_prefix('o')
            .and_then(|rest| rest.chars().next())
            .map(|ch| ch.is_ascii_digit())
            .unwrap_or(false)
    {
        "openai"
    } else if model.starts_with("claude") {
        "anthropic"
    } else if model.starts_with("gemini") {
        "gemini"
    } else if model.starts_with("qwen") {
        "qwen"
    } else {
        "other"
    }
}

/// 备用供应商: 必须 wire_api 与模型家族都一致，且非当前激活。
///
/// 仅同为 Responses 并不代表会话协议兼容：GPT 与 DeepSeek 对 reasoning
/// 条目的要求相反，跨家族故障转移会把主站故障伪装成 reasoning_text 错误。
fn fallback_chain<'a>(
    active: &'a ActiveSelection,
    relays: &'a [profiles::RelayProfile],
) -> Vec<&'a profiles::RelayProfile> {
    let (active_id, wire, family) = match active {
        ActiveSelection::Relay { profile_id } => {
            let Some(p) = relays.iter().find(|r| r.id == *profile_id) else {
                return Vec::new();
            };
            (
                profile_id.as_str(),
                p.wire_api.as_deref().unwrap_or("openai_chat"),
                model_family(p),
            )
        }
        ActiveSelection::Official => return Vec::new(),
    };
    relays
        .iter()
        .filter(|r| {
            r.id != active_id
                && r.wire_api.as_deref().unwrap_or("openai_chat") == wire
                && model_family(r) == family
        })
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
                                    if let Ok(v) =
                                        serde_json::from_str::<serde_json::Value>(&rest[start..end])
                                    {
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
    let (prompt, completion, total) = usage
        .map(|u| {
            (
                u.get("prompt_tokens").and_then(|v| v.as_u64()),
                u.get("completion_tokens").and_then(|v| v.as_u64()),
                u.get("total_tokens").and_then(|v| v.as_u64()),
            )
        })
        .unwrap_or((None, None, None));
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
            proxy = proxy.no_proxy(reqwest::NoProxy::from_string("localhost,127.0.0.1,::1"));
            builder = builder.proxy(proxy);
        }
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

async fn send_upstream_request(
    http: &reqwest::Client,
    method: &Method,
    url: &str,
    headers: &HeaderMap,
    key: &str,
    body: Bytes,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut req = http
        .request(method.clone(), url)
        .header("authorization", format!("Bearer {key}"))
        .header(
            "content-type",
            headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json"),
        )
        .header(
            "accept",
            headers
                .get("accept")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json"),
        )
        .body(body);
    if let Some(ua) = headers.get("user-agent") {
        if let Ok(s) = ua.to_str() {
            req = req.header("user-agent", s);
        }
    }
    req.send().await
}

async fn read_upstream_error_body(resp: reqwest::Response) -> Bytes {
    let mut stream = resp.bytes_stream();
    let mut body = Vec::new();
    while body.len() < UPSTREAM_ERROR_BODY_LIMIT {
        let Some(chunk) = stream.next().await else {
            break;
        };
        let Ok(chunk) = chunk else {
            break;
        };
        let remaining = UPSTREAM_ERROR_BODY_LIMIT - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    Bytes::from(body)
}

fn upstream_error_snippet(body: &[u8], api_key: &str) -> String {
    let parsed = serde_json::from_slice::<serde_json::Value>(body).ok();
    let text = parsed
        .as_ref()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                .or_else(|| value.get("message").and_then(serde_json::Value::as_str))
                .or_else(|| value.get("error").and_then(serde_json::Value::as_str))
        })
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    let redacted = if api_key.is_empty() {
        text
    } else {
        text.replace(api_key, "[REDACTED]")
    };
    let compact = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return "empty response body".to_string();
    }
    let mut chars = compact.chars();
    let limited: String = chars.by_ref().take(UPSTREAM_ERROR_TEXT_LIMIT).collect();
    if chars.next().is_some() {
        format!("{limited}…")
    } else {
        limited
    }
}

fn upstream_http_failure(
    profile: &profiles::RelayProfile,
    status: u16,
    body: &[u8],
    api_key: &str,
) -> String {
    format!(
        "upstream {} ({}) returned {}: {}",
        profile.name,
        profile.id,
        status,
        upstream_error_snippet(body, api_key)
    )
}

fn router_error_response(status: StatusCode, message: impl Into<String>) -> Response {
    let message = message.into();
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": "codexff_router_error",
            "param": null,
            "code": status.as_u16().to_string()
        }
    });
    Response::builder()
        .status(status)
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(body.to_string()))
        .unwrap()
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
    let mut doc: toml_edit::DocumentMut = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| e.to_string())?;
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

/// 当前激活中转的 id 与真实 base_url。
///
/// 路由状态可能来自上一次供应商（例如路由开启时直接从 DeepSeek 切到 GPT
/// 中转）。恢复配置时不能盲信旧的 original_base_url，否则会把新供应商
/// 的配置恢复成上一个供应商。
fn active_relay_identity() -> Option<(String, String)> {
    let ActiveSelection::Relay { profile_id } = profiles::current_active().ok()? else {
        return None;
    };
    let profile = profiles::list_relay_profiles()
        .ok()?
        .into_iter()
        .find(|p| p.id == profile_id)?;
    Some((profile.id, profile.base_url))
}

fn restore_base_url(active: Option<&(String, String)>, saved: Option<&str>) -> Option<String> {
    // profiles.json 是供应商真实地址的权威来源。状态文件可能因旧版本的
    // 切换顺序错误而出现 “rewrote_profile 已是新供应商，但
    // original_base_url 仍是旧供应商” 的组合，不能继续信任 saved。
    active.map(|(_, base)| base.clone()).or_else(|| {
        saved
            .filter(|url| !url.contains("127.0.0.1") && !url.contains("localhost"))
            .map(ToOwned::to_owned)
    })
}

fn is_local_router_url(url: &str) -> bool {
    url.contains("127.0.0.1") || url.contains("localhost")
}

fn local_proxy_url(port: u16) -> String {
    format!(
        "http://127.0.0.1:{}/v1",
        if port == 0 { DEFAULT_PORT } else { port }
    )
}

fn is_responses_path(path: &str) -> bool {
    matches!(path, "/responses" | "/v1/responses")
}

fn listener_running() -> bool {
    RUNTIME
        .lock()
        .map(|runtime| {
            runtime
                .as_ref()
                .map(|state| state.alive.load(std::sync::atomic::Ordering::Acquire))
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

fn config_points_at_expected_router(state: &RouterState) -> bool {
    current_config_base_url().as_deref() == Some(local_proxy_url(state.port).as_str())
}

pub fn profile_supports_lossless_compatibility(profile: &profiles::RelayProfile) -> bool {
    matches!(
        profile.wire_api.as_deref(),
        Some("responses" | "openai_responses")
    )
}

fn config_needs_restore(state: &RouterState, current_base_url: Option<&str>) -> bool {
    state.rewritten || current_base_url.map(is_local_router_url).unwrap_or(false)
}

fn should_resume_on_startup(
    active_relay_known: bool,
    has_router_evidence: bool,
    resume_after_restart: bool,
    codex_running: bool,
) -> bool {
    active_relay_known
        && (has_router_evidence || (resume_after_restart && codex_running))
}

/// 按状态还原真实 base_url
fn restore_config(s: &mut RouterState) -> Result<(), String> {
    if !config_needs_restore(s, current_config_base_url().as_deref()) {
        return Ok(());
    }

    // 当前激活供应商档案是权威来源；状态文件只在档案不可用时兜底。
    let active = active_relay_identity();
    let restore_base = restore_base_url(active.as_ref(), s.original_base_url.as_deref());

    let expected = if let Some(orig) = restore_base {
        write_config_base_url(Some(&orig))?;
        Some(orig)
    } else {
        write_config_base_url(None)?;
        None
    };
    let actual = current_config_base_url();
    let verified = match expected.as_deref() {
        Some(url) => actual.as_deref() == Some(url),
        None => actual
            .as_deref()
            .is_none_or(|url| !is_local_router_url(url)),
    };
    if !verified {
        return Err(format!(
            "恢复真实供应商地址后校验失败（期望 {:?}，实际 {:?}）",
            expected, actual
        ));
    }
    s.rewritten = false;
    s.original_base_url = None;
    s.rewrote_profile = None;
    s.degraded = false;
    s.recovery_message = None;
    save_state(s)?;
    Ok(())
}

/// 切换供应商前解除旧供应商的 base_url 接管，但保留路由服务。
///
/// 必须在 profiles::activate_relay_with_progress 写入新配置之前调用；否则
/// sync_active 会先恢复旧地址，从而覆盖刚写入的新供应商地址。
pub fn prepare_provider_switch() -> Result<(), String> {
    let mut s = load_state();
    restore_config(&mut s)
}

/// App 启动时清理异常退出留下的本地路由接管状态。
///
/// 路由不作为供应商切换的必需组件，也不默认常驻。上一实例不存在时，
/// `enabled=true` 只代表旧状态文件，不能证明 19331 仍有服务；必须先把
/// Codex 配置恢复到当前供应商真实地址，避免冷启动后的首个请求报 502。
pub fn recover_stale_startup_state() -> Result<RouterStatus, String> {
    let mut s = load_state();
    if let Err(e) = restore_config(&mut s) {
        s.degraded = true;
        s.recovery_message = Some(format!("兼容路由恢复失败: {e}"));
        return match save_state(&s) {
            Ok(()) => Err(e),
            Err(save_error) => Err(format!("{e}; 降级状态保存失败: {save_error}")),
        };
    }
    s.enabled = false;
    s.automatic = false;
    s.rewritten = false;
    s.original_base_url = None;
    s.rewrote_profile = None;
    s.degraded = false;
    s.recovery_message = None;
    s.resume_after_restart = false;
    save_state(&s)?;
    Ok(status())
}

/// 路由开启时, 把当前激活的中转供应商 base_url 改写为本地代理地址。
/// 激活/切换供应商后调用, 保证 Codex 请求始终走本地路由。
pub fn sync_active() -> Result<RouterStatus, String> {
    let mut s = load_state();
    let previous_state = s.clone();
    let previous_base_url = current_config_base_url();
    let apply = (|| -> Result<RouterStatus, String> {
        if !s.enabled {
            return Err("兼容路由监听尚未启用".into());
        }
        if !RUNTIME.lock().map(|r| r.is_some()).unwrap_or(false) {
            return Err("兼容路由监听未运行".into());
        }
        // 先还原旧的改写 (若激活供应商已变化)
        restore_config(&mut s)?;
        let (profile, _key) =
            active_relay().ok_or_else(|| "找不到激活的第三方供应商或其密钥".to_string())?;
        if !profile_supports_lossless_compatibility(&profile) {
            return Err(format!(
                "供应商 {} 使用 {:?} 协议，本地会话兼容路由仅支持 Responses",
                profile.name, profile.wire_api
            ));
        }
        let proxy_url = local_proxy_url(s.port);
        let orig = current_config_base_url().unwrap_or_else(|| profile.base_url.clone());
        if orig != proxy_url {
            write_config_base_url(Some(&proxy_url))?;
        }
        let actual = current_config_base_url();
        if actual.as_deref() != Some(proxy_url.as_str()) {
            return Err(format!(
                "兼容路由接管后校验失败（期望 {proxy_url}，实际 {:?}）",
                actual
            ));
        }
        s.rewritten = true;
        s.original_base_url = Some(orig);
        s.rewrote_profile = Some(profile.id);
        s.degraded = false;
        s.recovery_message = None;
        s.resume_after_restart = false;
        save_state(&s)?;
        let current = status();
        if !current.enabled || !current.rewritten || current.degraded {
            return Err("兼容路由未达到可用状态".into());
        }
        Ok(current)
    })();
    if let Err(error) = apply {
        let config_rollback = write_config_base_url(previous_base_url.as_deref());
        let state_rollback = save_state(&previous_state);
        return Err(format!(
            "{error}; config 回滚: {config_rollback:?}; 状态回滚: {state_rollback:?}"
        ));
    }
    apply
}

async fn forward(method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let Some((active_profile, _)) = active_relay() else {
        return router_error_response(StatusCode::BAD_GATEWAY, "no active relay provider");
    };
    if !profile_supports_lossless_compatibility(&active_profile) {
        return router_error_response(
            StatusCode::NOT_IMPLEMENTED,
            "session compatibility router only supports Responses providers",
        );
    }
    if !is_responses_path(uri.path()) {
        return router_error_response(
            StatusCode::NOT_IMPLEMENTED,
            "session compatibility router only accepts Responses requests",
        );
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
    let mut last_status: Option<u16> = None;
    let mut attempted = Vec::new();
    let primary_id = chain
        .first()
        .map(|c| c.id.as_str())
        .unwrap_or_default()
        .to_string();
    for p in chain.iter() {
        let breaker_open = BREAKER.lock().map(|b| b.is_open(&p.id)).unwrap_or(false);
        if breaker_open {
            continue;
        }
        let Some(pkey) = vault::get_relay_key(&p.id).ok().flatten() else {
            continue;
        };
        attempted.push((p.id.clone(), p.name.clone()));
        // 每个候选供应商用自己的默认模型/模型清单做请求层清洗+归一化
        // (fallback chain 里各供应商模型不同, 不能复用同一个 body)。
        let empty_reasoning_content = reasoning_policy_for(p);
        let request_body = sanitize_responses_body(
            &body,
            &uri,
            Some(p.model.as_str()),
            p.model_reasoning_effort.as_deref(),
            &p.supported_models,
            empty_reasoning_content,
        );
        // 本地路由 base_url 统一写成 http://127.0.0.1:PORT/v1, Codex 会请求
        // /v1/responses; 转发时必须剥掉 /v1 前缀, 还原成直连形态
        // (base_url 无 /v1 的 DeepSeek/皮卡丘 = /responses, 带 /v1 的 =
        // /v1/responses), 否则根路径直达的中转会 404。
        let raw_path = uri.path_and_query().map(|q| q.as_str()).unwrap_or("");
        let forward_path = raw_path.strip_prefix("/v1").unwrap_or(raw_path);
        let url = format!("{}{}", p.base_url.trim_end_matches('/'), forward_path);
        let http = client();
        match send_upstream_request(&http, &method, &url, &headers, &pkey, request_body).await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status == 400 {
                    let content_type = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("application/json")
                        .to_string();
                    let bytes = resp.bytes().await.unwrap_or_default();
                    if let Some(next_policy) =
                        reasoning_retry_policy(status, &bytes, empty_reasoning_content)
                    {
                        // 必须从原始 Codex 请求重新生成；首次清空后的 body 已丢失
                        // reasoning_text，无法用于“保留内容”方向的自愈重试。
                        let retry_body = sanitize_responses_body(
                            &body,
                            &uri,
                            Some(p.model.as_str()),
                            p.model_reasoning_effort.as_deref(),
                            &p.supported_models,
                            next_policy,
                        );
                        if let Ok(retry_resp) =
                            send_upstream_request(&http, &method, &url, &headers, &pkey, retry_body)
                                .await
                        {
                            let retry_status = retry_resp.status().as_u16();
                            if retry_status < 500 && retry_status != 429 {
                                learn_reasoning_policy(&p.id, next_policy);
                                let _ = BREAKER.lock().map(|mut b| b.record_success(&p.id));
                                let ct = retry_resp
                                    .headers()
                                    .get("content-type")
                                    .map(|v| v.to_str().unwrap_or("application/json").to_string())
                                    .unwrap_or_else(|| "application/json".to_string());
                                return Response::builder()
                                    .status(
                                        StatusCode::from_u16(retry_status)
                                            .unwrap_or(StatusCode::OK),
                                    )
                                    .header("content-type", ct)
                                    .body(Body::from_stream(RxStream {
                                        rx: {
                                            let (tx, rx) =
                                                mpsc::channel::<Result<Bytes, std::io::Error>>(32);
                                            let mut stream = retry_resp.bytes_stream();
                                            tokio::spawn(async move {
                                                while let Some(item) = stream.next().await {
                                                    if tx
                                                        .send(item.map_err(|e| {
                                                            std::io::Error::other(e.to_string())
                                                        }))
                                                        .await
                                                        .is_err()
                                                    {
                                                        break;
                                                    }
                                                }
                                            });
                                            rx
                                        },
                                    }))
                                    .unwrap();
                            }
                        }
                    }
                    // 不是已知 reasoning schema 错误，或自愈重试未成功：
                    // 把原始 400 原样交给 Codex，不错误切换到备用供应商。
                    return Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .header("content-type", content_type)
                        .body(Body::from(bytes))
                        .unwrap();
                }
                if status >= 500 || status == 429 {
                    let _ = BREAKER.lock().map(|mut b| b.record_failure(&p.id));
                    let error_body = read_upstream_error_body(resp).await;
                    last_status = Some(status);
                    last_error = Some(upstream_http_failure(p, status, &error_body, &pkey));
                    continue;
                }
                let ct = resp
                    .headers()
                    .get("content-type")
                    .map(|v| v.to_str().unwrap_or("application/json").to_string())
                    .unwrap_or_else(|| "application/json".to_string());
                let is_sse = ct.to_ascii_lowercase().contains("text/event-stream");
                let buffered = if is_sse {
                    match inspect_initial_sse(resp, empty_reasoning_content).await {
                        InitialStream::Forward(stream) => stream,
                        InitialStream::Retry(next_policy) => {
                            let retry_body = sanitize_responses_body(
                                &body,
                                &uri,
                                Some(p.model.as_str()),
                                p.model_reasoning_effort.as_deref(),
                                &p.supported_models,
                                next_policy,
                            );
                            match send_upstream_request(
                                &http, &method, &url, &headers, &pkey, retry_body,
                            )
                            .await
                            {
                                Ok(retry)
                                    if retry.status().as_u16() < 500
                                        && retry.status().as_u16() != 429 =>
                                {
                                    let retry_status = retry.status().as_u16();
                                    let retry_ct = retry
                                        .headers()
                                        .get("content-type")
                                        .and_then(|v| v.to_str().ok())
                                        .unwrap_or("text/event-stream")
                                        .to_string();
                                    learn_reasoning_policy(&p.id, next_policy);
                                    match inspect_initial_sse(retry, next_policy).await {
                                        InitialStream::Forward(stream) => {
                                            let _ =
                                                BREAKER.lock().map(|mut b| b.record_success(&p.id));
                                            if p.id != primary_id {
                                                record_fallback(&p.id);
                                            }
                                            return response_from_buffered_stream(
                                                retry_status,
                                                retry_ct,
                                                stream,
                                                p.clone(),
                                            );
                                        }
                                        InitialStream::Retry(_) => {
                                            return router_error_response(
                                                StatusCode::BAD_REQUEST,
                                                "upstream rejected both reasoning schemas",
                                            );
                                        }
                                    }
                                }
                                Ok(retry) => {
                                    let status = retry.status();
                                    let bytes = read_upstream_error_body(retry).await;
                                    return router_error_response(
                                        status,
                                        upstream_http_failure(
                                            p,
                                            status.as_u16(),
                                            &bytes,
                                            &pkey,
                                        ),
                                    );
                                }
                                Err(e) => {
                                    return router_error_response(
                                        StatusCode::BAD_GATEWAY,
                                        format!(
                                            "upstream {} ({}) transport error: {}",
                                            p.name, p.id, e
                                        ),
                                    );
                                }
                            }
                        }
                    }
                } else {
                    BufferedStream {
                        prefix: Bytes::new(),
                        stream: Box::pin(resp.bytes_stream()),
                    }
                };
                let _ = BREAKER.lock().map(|mut b| b.record_success(&p.id));
                if p.id != primary_id {
                    record_fallback(&p.id);
                }
                return response_from_buffered_stream(status, ct, buffered, p.clone());
            }
            Err(e) => {
                let _ = BREAKER.lock().map(|mut b| b.record_failure(&p.id));
                last_status = None;
                last_error = Some(format!(
                    "upstream {} ({}) transport error: {}",
                    p.name, p.id, e
                ));
            }
        }
    }
    let diagnostic = last_error.unwrap_or_else(|| {
        if attempted.is_empty() {
            "no usable upstream provider (missing key, incompatible provider, or breaker open)"
                .to_string()
        } else {
            "all attempted upstream providers failed without a response".to_string()
        }
    });
    // 全部失败 → 记录日志 (失败请求)
    usage_stats::append_usage_log(UsageLogEntry {
        ts_ms: now_ms(),
        provider_id: attempted
            .first()
            .map(|(id, _)| id.clone())
            .unwrap_or_default(),
        provider_name: attempted
            .first()
            .map(|(_, name)| name.clone())
            .unwrap_or_default(),
        model: None,
        wire_api: None,
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: None,
        cost: None,
        status: last_status.unwrap_or(0),
        error: Some(diagnostic.clone()),
    });
    router_error_response(StatusCode::BAD_GATEWAY, diagnostic)
}

pub fn status() -> RouterStatus {
    let state = load_state();
    let runtime_live = listener_running();
    let active_identity = profiles::current_active().ok();
    let active_relay_id = match &active_identity {
        Some(ActiveSelection::Relay { profile_id }) => Some(profile_id.as_str()),
        _ => None,
    };
    let takeover_verified = state.rewritten
        && config_points_at_expected_router(&state)
        && state.rewrote_profile.as_deref() == active_relay_id
        && state
            .original_base_url
            .as_deref()
            .is_some_and(|url| !is_local_router_url(url))
        && active_relay().is_some();
    let runtime_died = state.enabled && !runtime_live;
    let last_fallback = LAST_FALLBACK.lock().map(|g| g.clone()).unwrap_or_default();
    RouterStatus {
        enabled: state.enabled && runtime_live,
        port: state.port,
        rewritten: takeover_verified,
        automatic: state.automatic,
        degraded: state.degraded || runtime_died,
        recovery_message: state
            .recovery_message
            .or_else(|| runtime_died.then(|| "会话兼容路由监听已停止，正在等待恢复".to_string())),
        active_provider: active_identity.map(|a| match a {
            ActiveSelection::Relay { profile_id } => profile_id,
            ActiveSelection::Official => "official".to_string(),
        }),
        last_fallback,
    }
}

/// Codex 是否仍指向本地路由 (config 的 base_url 被改写且未还原)。
///
/// 只要 Codex 还在用本地地址, 无论路由服务当前是否存活都不能退出 App —
/// Codex 不重读 config, 直接退出后下一请求会打到已关闭的本地端口 → 502。
pub fn codex_points_at_router() -> bool {
    if let Some(url) = current_config_base_url() {
        return is_local_router_url(&url);
    }
    false
}

/// 除磁盘仍指向 localhost 外，路由状态记录为已接管且 Codex 仍在运行时，
/// 进程也可能缓存了本地地址。此谓词用于退出/停服保护，不能用于判定热切换
/// 已经安全接管（后者必须用 status().rewritten 的磁盘回读结果）。
pub fn codex_may_depend_on_router() -> bool {
    codex_points_at_router() || (load_state().rewritten && crate::session_manager::codex_running())
}

/// App 退出时同步收尾: 先还原 config 里的 base_url (Codex 新请求立即回到
/// 真实中转地址), 再停止代理 — 顺序不能反, 否则 Codex 请求会打到正在关闭
/// 的本地端口导致会话连接失败。即使 `enabled=false`，也会检查 config，修复
/// 崩溃/旧版本遗留的 localhost 改写。
pub fn shutdown() -> Result<(), String> {
    let mut s = load_state();
    let resume_after_restart =
        crate::session_manager::codex_running() && codex_may_depend_on_router();
    let previous_automatic = s.automatic;
    // 先落盘重启意图。即使随后还原配置或进程退出中断，下次启动仍知道
    // 正在运行的 Codex 可能缓存了 localhost。
    s.resume_after_restart = resume_after_restart;
    save_state(&s)?;
    // 1. 先还原 base_url, 让 Codex 后续请求走真实中转 (不再依赖本地路由)
    restore_config(&mut s)?;
    // 2. 再停止本地代理服务
    if let Some(rt) = RUNTIME.lock().unwrap_or_else(|e| e.into_inner()).take() {
        if let Some(tx) = rt.shutdown {
            let _ = tx.send(());
        }
    }
    s.enabled = false;
    s.automatic = if resume_after_restart {
        previous_automatic
    } else {
        false
    };
    s.resume_after_restart = resume_after_restart;
    save_state(&s)?;
    Ok(())
}

/// 官方模式下彻底关闭本地路由: 官方 config 已写入, 不能再把改写前的
/// 中转 base_url 还原回去覆盖官方配置 — 先清空改写记录, 再停止服务。
pub async fn disable_for_official() -> Result<RouterStatus, String> {
    {
        let mut s = load_state();
        s.rewritten = false;
        s.original_base_url = None;
        s.rewrote_profile = None;
        s.resume_after_restart = false;
        save_state(&s)?;
    }
    set_enabled(false).await
}

async fn start_runtime(automatic: bool, takeover: bool) -> Result<RouterStatus, String> {
    if listener_running() {
        let mut s = load_state();
        s.enabled = true;
        // 手动开启优先，后续自动检查不能把它变成自动模式。
        if !automatic {
            s.automatic = false;
        }
        save_state(&s)?;
        if takeover {
            sync_active()?;
        }
        let current = status();
        if takeover && (!current.enabled || !current.rewritten || current.degraded) {
            return Err("兼容路由接管未完成".into());
        }
        return Ok(current);
    }
    let port = {
        let mut s = load_state();
        s.port = if s.port == 0 { DEFAULT_PORT } else { s.port };
        if takeover {
            s.enabled = true;
            s.automatic = automatic;
            save_state(&s)?;
        }
        s.port
    };
    let (tx, rx) = oneshot::channel::<()>();
    let app = axum::Router::new().fallback(handler);
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(e) => {
            let mut s = load_state();
            s.enabled = false;
            s.automatic = false;
            if s.rewritten || codex_points_at_router() {
                s.degraded = true;
                s.recovery_message = Some(format!(
                    "本地端口 {port} 无法启动，但 Codex 可能仍指向兼容路由: {e}"
                ));
            }
            save_state(&s)?;
            return Err(e.to_string());
        }
    };
    let server = axum::serve(listener, app).with_graceful_shutdown(async {
        let _ = rx.await;
    });
    let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let server_alive = alive.clone();
    let _handle = tokio::spawn(async move {
        let _ = server.await;
        server_alive.store(false, std::sync::atomic::Ordering::Release);
    });
    *RUNTIME.lock().unwrap_or_else(|e| e.into_inner()) = Some(RuntimeState {
        shutdown: Some(tx),
        alive,
    });
    if takeover {
        if let Err(e) = sync_active() {
            let mut s = load_state();
            let restore = restore_config(&mut s);
            s.degraded = true;
            s.recovery_message = Some(match &restore {
                Ok(()) => format!("兼容路由接管失败，已恢复真实供应商地址: {e}"),
                Err(restore_error) => {
                    format!("兼容路由接管失败且真实供应商地址恢复失败: {e}; {restore_error}")
                }
            });
            if restore.is_ok() {
                s.enabled = false;
                s.automatic = false;
                save_state(&s)?;
                if let Some(rt) = RUNTIME.lock().unwrap_or_else(|err| err.into_inner()).take() {
                    if let Some(tx) = rt.shutdown {
                        let _ = tx.send(());
                    }
                }
            } else {
                // 配置仍可能指向 localhost，必须保留监听器，避免直接制造 502。
                s.enabled = listener_running();
                save_state(&s)?;
            }
            return Err(match restore {
                Ok(()) => e,
                Err(restore_error) => format!("{e}; 恢复真实供应商地址失败: {restore_error}"),
            });
        }
    }
    if !takeover {
        // 预绑定只属于内存事务，不写 enabled=true；进程在 profile 提交前崩溃
        // 时，下次启动不会把 listener-only 阶段误升级为真正的路由接管。
        return Ok(status());
    }
    let current = status();
    if takeover && (!current.enabled || !current.rewritten || current.degraded) {
        return Err("兼容路由接管未完成".into());
    }
    Ok(current)
}

async fn enable_with_mode(automatic: bool) -> Result<RouterStatus, String> {
    let (profile, _key) =
        active_relay().ok_or_else(|| "没有可接管的第三方供应商或密钥".to_string())?;
    if !profile_supports_lossless_compatibility(&profile) {
        return Err(format!(
            "供应商 {} 使用 {:?} 协议，本地会话兼容路由仅支持 Responses",
            profile.name, profile.wire_api
        ));
    }
    start_runtime(automatic, true).await
}

/// 在供应商切换事务提交前先启动监听端口，但暂不改写 config。
/// 返回 true 表示本次新启动，切换失败时调用方应撤销。
pub async fn prepare_session_compatibility() -> Result<bool, String> {
    if listener_running() {
        return Ok(false);
    }
    start_runtime(true, false).await?;
    Ok(true)
}

/// 撤销尚未接管 config 的预启动兼容层。
pub fn cancel_prepared_compatibility() -> Result<(), String> {
    let mut s = load_state();
    if s.rewritten
        || current_config_base_url()
            .as_deref()
            .map(is_local_router_url)
            == Some(true)
    {
        return Ok(());
    }
    if !s.rewritten {
        s.enabled = false;
        s.automatic = false;
        // Persist the cancellation before stopping the listener. If this
        // write fails, keep the harmless listener-only runtime alive so the
        // caller can retry instead of returning an error after having already
        // made the cancellation irreversible in memory.
        save_state(&s)?;
    }
    if let Some(rt) = RUNTIME.lock().unwrap_or_else(|e| e.into_inner()).take() {
        if let Some(tx) = rt.shutdown {
            let _ = tx.send(());
        }
    }
    Ok(())
}

/// 旧会话跨供应商续接所需的按需兼容层。
/// 只允许第三方模式调用；官方切换会先关闭路由并恢复官方凭证。
pub async fn ensure_session_compatibility(required: bool) -> Result<RouterStatus, String> {
    let (profile, _key) =
        active_relay().ok_or_else(|| "会话兼容路由只允许在有效第三方供应商模式运行".to_string())?;
    if !profile_supports_lossless_compatibility(&profile) {
        return Err(format!(
            "供应商 {} 使用 {:?} 协议，历史会话无损接续目前仅支持 Responses",
            profile.name, profile.wire_api
        ));
    }
    if required {
        enable_with_mode(true).await
    } else {
        Ok(status())
    }
}

/// App 冷启动时恢复上一实例正在使用的路由。
///
/// Codex 进程会缓存 provider/base_url。若 App 异常退出后 config 仍指向
/// localhost，只恢复配置但不恢复监听端口，正在运行的 Codex 仍会报 502。
/// 因此第三方模式下优先恢复路由服务；其它情况才清理陈旧状态。
pub async fn resume_or_recover_startup() -> Result<RouterStatus, String> {
    let state = load_state();
    let takeover_evidence = state.rewritten
        && config_points_at_expected_router(&state)
        && state
            .original_base_url
            .as_deref()
            .is_some_and(|url| !is_local_router_url(url));
    let has_router_evidence = takeover_evidence || codex_points_at_router();
    let codex_running = crate::session_manager::codex_running();
    let restart_requested = state.resume_after_restart && codex_running;
    let active_profile = active_relay().map(|(profile, _)| profile);
    let active_relay_known = active_profile.is_some();
    if (has_router_evidence || restart_requested) && !active_relay_known && codex_running {
        let mut failed = state.clone();
        failed.degraded = true;
        failed.recovery_message = Some(
            "检测到 Codex 可能仍缓存本地兼容路由，但当前第三方供应商身份不完整。请完全退出 Codex / ChatGPT 后修复供应商配置。"
                .into(),
        );
        let message = failed.recovery_message.clone().unwrap();
        save_state(&failed).map_err(|e| format!("{message}; 降级状态保存失败: {e}"))?;
        return Err(message);
    }
    let should_resume = should_resume_on_startup(
        active_relay_known,
        has_router_evidence,
        state.resume_after_restart,
        codex_running,
    );
    if should_resume {
        if !active_profile
            .as_ref()
            .is_some_and(profile_supports_lossless_compatibility)
        {
            if codex_running && (codex_may_depend_on_router() || restart_requested) {
                let mut failed = state.clone();
                failed.degraded = true;
                failed.recovery_message = Some(
                    "当前供应商不是 Responses 协议，不能恢复历史会话兼容路由。请完全退出 Codex / ChatGPT 后关闭本地路由。"
                        .into(),
                );
                let message = failed.recovery_message.clone().unwrap();
                save_state(&failed).map_err(|e| format!("{message}; 降级状态保存失败: {e}"))?;
                return Err(message);
            }
            return recover_stale_startup_state();
        }
        match enable_with_mode(state.automatic).await {
            Ok(status) => Ok(status),
            Err(e) if codex_running => {
                let mut failed = load_state();
                failed.degraded = true;
                failed.recovery_message = Some(format!(
                    "兼容路由恢复失败，请完全退出 Codex / ChatGPT 后重试: {e}"
                ));
                let message = failed.recovery_message.clone().unwrap_or(e);
                save_state(&failed)
                    .map_err(|save_error| format!("{message}; 降级状态保存失败: {save_error}"))?;
                Err(message)
            }
            Err(e) => recover_stale_startup_state().map_err(|restore| {
                format!("兼容路由启动失败: {e}; 恢复真实供应商地址也失败: {restore}")
            }),
        }
    } else {
        recover_stale_startup_state()
    }
}

/// 启动/停止本地路由 (接入开关完成后, 还会改写激活供应商 base_url)
pub async fn set_enabled(enabled: bool) -> Result<RouterStatus, String> {
    if enabled {
        return enable_with_mode(false).await;
    } else {
        // Codex 会缓存启动时的 provider/base_url，不会实时重读 config.toml。
        // 此时停止 19331 即使已经还原配置，当前会话仍会继续访问本地端口并
        // 报 502。保持现状并明确要求先退出 Codex，避免把正在进行的会话断掉。
        if crate::session_manager::codex_running() && codex_may_depend_on_router() {
            return Err(
                "Codex / ChatGPT 当前仍在运行，不能直接关闭本地路由，否则当前会话会报 502。请先完全退出 Codex / ChatGPT，再关闭本地路由。"
                    .to_string(),
            );
        }
        let mut s = load_state();
        restore_config(&mut s)?;
        s.enabled = false;
        s.automatic = false;
        s.resume_after_restart = false;
        save_state(&s)?;
        if let Some(rt) = RUNTIME.lock().unwrap_or_else(|e| e.into_inner()).take() {
            if let Some(tx) = rt.shutdown {
                let _ = tx.send(());
            }
        }
    }
    Ok(status())
}

async fn handler(method: Method, uri: Uri, headers: HeaderMap, body: Body) -> Response {
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
        let out = sanitize_responses_body(&body, &uri, None, None, &[], true);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let input = v.get("input").unwrap().as_array().unwrap();
        assert_eq!(
            input[0].get("content").unwrap().as_array().unwrap().len(),
            0
        );
        assert!(input[2].get("content").is_none());
        // 非 responses 路径原样
        let uri2: Uri = "/v1/chat/completions".parse().unwrap();
        assert_eq!(
            sanitize_responses_body(&body, &uri2, None, None, &[], true),
            body
        );
    }

    #[test]
    fn responses_reasoning_passthrough_for_deepseek() {
        let body = Bytes::from(
            r#"{"model":"deepseek-v4-flash","input":[
                {"type":"reasoning","id":"r1","summary":[],"content":[{"type":"reasoning_text","text":"think"}],"encrypted_content":null},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
            ]}"#,
        );
        let uri: Uri = "/v1/responses".parse().unwrap();
        // DeepSeek: 非空 reasoning content 原样透传，不触碰
        assert_eq!(
            sanitize_responses_body(&body, &uri, None, None, &[], false),
            body
        );
    }

    #[test]
    fn responses_deepseek_drops_emptied_reasoning() {
        let body = Bytes::from(
            r#"{"model":"deepseek-v4-flash","input":[
                {"type":"reasoning","id":"r1","summary":[],"content":[],"encrypted_content":null},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
            ]}"#,
        );
        let uri: Uri = "/v1/responses".parse().unwrap();
        let out = sanitize_responses_body(&body, &uri, None, None, &[], false);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let input = v.get("input").unwrap().as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0].get("type").unwrap(), "message");
    }

    #[test]
    fn reasoning_retry_policy_switches_side_on_explicit_errors() {
        // GPT 当前策略是清空 (true)；上游要求回传 reasoning_text → 改为保留 (false) 重试
        assert_eq!(
            reasoning_retry_policy(
                400,
                br#"{"error":{"message":"The reasoning_text in the thinking mode must be passed back to the API."}}"#,
                true,
            ),
            Some(false)
        );
        // 保留策略 (false) 收到 array_above_max_length → 改为清空 (true) 重试
        assert_eq!(
            reasoning_retry_policy(
                400,
                br#"{"error":{"message":"Invalid 'input[523].content': array too long. Expected an array with maximum length 0, but got an array with length 1 instead."}}"#,
                false,
            ),
            Some(true)
        );
        // 与当前策略同方向的错误不重试 (保留策略又收到要求回传类错误)
        assert_eq!(
            reasoning_retry_policy(
                400,
                br#"{"error":{"message":"The reasoning_text in the thinking mode must be passed back to the API."}}"#,
                false,
            ),
            None
        );
        // 清空策略又收到 array 超长类错误 → 不重试
        assert_eq!(
            reasoning_retry_policy(
                400,
                br#"{"error":{"message":"array_above_max_length"}}"#,
                true,
            ),
            None
        );
        // 其它 400 / 非 400 不触发
        assert_eq!(
            reasoning_retry_policy(400, br#"{"error":"nope"}"#, true),
            None
        );
        assert_eq!(reasoning_retry_policy(500, br#"oops"#, true), None);
    }

    #[test]
    fn detects_reasoning_error_inside_successful_sse() {
        let sse = br#"event: error
data: {"type":"error","message":"The reasoning_text in the thinking mode must be passed back to the API."}

"#;
        assert_eq!(reasoning_error_policy(sse, true), Some(false));
        assert!(!sse_has_valid_output(sse));
    }

    #[test]
    fn valid_sse_output_disables_safe_retry() {
        let sse = br#"event: response.output_text.delta
data: {"type":"response.output_text.delta","delta":"hello"}

"#;
        assert!(sse_has_valid_output(sse));
    }

    #[test]
    fn restore_prefers_active_provider_over_stale_saved_url() {
        let active = Some((
            "gpt-relay".to_string(),
            "https://relay.example/v1".to_string(),
        ));
        assert_eq!(
            restore_base_url(active.as_ref(), Some("https://api.deepseek.com")),
            Some("https://relay.example/v1".to_string())
        );
    }

    #[test]
    fn restore_ignores_local_saved_url() {
        let active = None;
        assert_eq!(
            restore_base_url(active.as_ref(), Some("http://127.0.0.1:19331/v1")),
            None
        );
        assert_eq!(
            restore_base_url(active.as_ref(), Some("http://localhost:19331/v1")),
            None
        );
    }

    #[test]
    fn stale_local_config_is_restored_even_when_router_state_is_disabled() {
        let state = RouterState {
            enabled: false,
            rewritten: false,
            ..RouterState::default()
        };
        assert!(config_needs_restore(
            &state,
            Some("http://127.0.0.1:19331/v1")
        ));
        assert!(config_needs_restore(
            &state,
            Some("http://localhost:19331/v1")
        ));
        assert!(!config_needs_restore(
            &state,
            Some("https://relay.example/v1")
        ));
    }

    #[test]
    fn restore_uses_saved_url_only_without_active_provider() {
        let active = None;
        assert_eq!(
            restore_base_url(active.as_ref(), Some("https://relay.example/v1")),
            Some("https://relay.example/v1".to_string())
        );
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
            Some("high"),
            &["deepseek-v4-flash".into(), "deepseek-v4-pro".into()],
            true,
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v.get("model").unwrap(), "deepseek-v4-flash");
        assert_eq!(v["reasoning"]["effort"], "high");
        // 清单未知 + 官方模型名 → 替换
        let out2 = sanitize_responses_body(&body, &uri, Some("gpt-5.5"), None, &[], true);
        let v2: serde_json::Value = serde_json::from_slice(&out2).unwrap();
        assert_eq!(v2.get("model").unwrap(), "gpt-5.5");
        // 清单未知 + 自定义模型 → 也替换为目标供应商默认，避免跨站模型泄漏
        let body3 = Bytes::from(
            r#"{"model":"my-custom-llm","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
        );
        let out3 = sanitize_responses_body(&body3, &uri, Some("gpt-5.5"), None, &[], true);
        let v3: serde_json::Value = serde_json::from_slice(&out3).unwrap();
        assert_eq!(v3.get("model").unwrap(), "gpt-5.5");
    }

    #[test]
    fn lossless_compatibility_only_accepts_responses_wire() {
        fn profile(wire: Option<&str>) -> profiles::RelayProfile {
            profiles::RelayProfile {
                wire_api: wire.map(str::to_string),
                ..profiles::RelayProfile::default()
            }
        }
        assert!(profile_supports_lossless_compatibility(&profile(Some(
            "responses"
        ))));
        assert!(profile_supports_lossless_compatibility(&profile(Some(
            "openai_responses"
        ))));
        assert!(!profile_supports_lossless_compatibility(&profile(Some(
            "chat"
        ))));
        assert!(!profile_supports_lossless_compatibility(&profile(Some(
            "anthropic"
        ))));
        assert!(!profile_supports_lossless_compatibility(&profile(None)));
    }

    #[test]
    fn web_search_call_id_prefix_rewritten() {
        let body = Bytes::from(
            r#"{"model":"gpt-5.6-sol","input":[
                {"type":"web_search_call","id":"call_00_abc123","status":"completed","action":{"type":"search","queries":["q"]}},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
            ]}"#,
        );
        let uri: Uri = "/v1/responses".parse().unwrap();
        let out = sanitize_responses_body(&body, &uri, None, None, &[], true);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let input = v.get("input").unwrap().as_array().unwrap();
        assert_eq!(input[0].get("id").unwrap(), "ws_00_abc123");
        // 非 call_ 前缀原样保留
        let body2 = Bytes::from(
            r#"{"model":"gpt-5.6-sol","input":[{"type":"web_search_call","id":"ws_xyz","status":"completed","action":{"type":"search","queries":["q"]}}]}"#,
        );
        let out2 = sanitize_responses_body(&body2, &uri, None, None, &[], true);
        let v2: serde_json::Value = serde_json::from_slice(&out2).unwrap();
        assert_eq!(v2.get("input").unwrap()[0].get("id").unwrap(), "ws_xyz");
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
    fn fallback_requires_same_wire_and_model_family() {
        let active = ActiveSelection::Relay {
            profile_id: "a".to_string(),
        };
        let relays = vec![
            profiles::RelayProfile {
                id: "a".into(),
                model: "gpt-5.6-sol".into(),
                wire_api: Some("responses".into()),
                ..Default::default()
            },
            profiles::RelayProfile {
                id: "b".into(),
                model: "gpt-5.5".into(),
                wire_api: Some("responses".into()),
                ..Default::default()
            },
            profiles::RelayProfile {
                id: "c".into(),
                model: "deepseek-v4-flash".into(),
                wire_api: Some("responses".into()),
                ..Default::default()
            },
            profiles::RelayProfile {
                id: "d".into(),
                model: "gpt-5.5".into(),
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

    #[test]
    fn upstream_error_diagnostic_extracts_message_and_redacts_key() {
        let profile = profiles::RelayProfile {
            id: "relay-1".into(),
            name: "测试中转".into(),
            ..Default::default()
        };
        let key = "sk-secret-value";
        let body = br#"{"error":{"message":"gateway rejected sk-secret-value temporarily"}}"#;
        let diagnostic = upstream_http_failure(&profile, 502, body, key);
        assert_eq!(
            diagnostic,
            "upstream 测试中转 (relay-1) returned 502: gateway rejected [REDACTED] temporarily"
        );
        assert!(!diagnostic.contains(key));
    }

    #[test]
    fn upstream_error_diagnostic_is_bounded_and_flattens_html() {
        let body = format!("<html>\n  <body>{}</body>\n</html>", "x".repeat(2000));
        let snippet = upstream_error_snippet(body.as_bytes(), "");
        assert!(!snippet.contains('\n'));
        assert!(snippet.chars().count() <= UPSTREAM_ERROR_TEXT_LIMIT + 1);
        assert!(snippet.ends_with('…'));
    }

    #[tokio::test]
    async fn router_errors_use_codex_readable_json_shape() {
        let response = router_error_response(StatusCode::BAD_GATEWAY, "upstream failed");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/json; charset=utf-8"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["message"], "upstream failed");
        assert_eq!(value["error"]["type"], "codexff_router_error");
        assert_eq!(value["error"]["code"], "502");
    }

    #[test]
    fn restart_marker_only_resumes_for_a_running_codex_and_known_relay() {
        assert!(should_resume_on_startup(true, false, true, true));
        assert!(!should_resume_on_startup(true, false, true, false));
        assert!(!should_resume_on_startup(false, false, true, true));
        assert!(should_resume_on_startup(true, true, false, false));
    }
}
