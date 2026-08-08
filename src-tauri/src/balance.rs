//! 余额查询 — usage script (cc-switch 对齐) + 厂商专用 API + 中转站通用探测。
//!
//! 优先级:
//! 1. usage script (导入链接携带 / 编辑表单填 / 从 cc-switch DB 按名回填) —
//!    中转站自带余额脚本, 与 cc-switch 完全同机制 (quickjs 执行:
//!    {{var}} 替换 → request 配置 → HTTP → extractor 提取余额)
//! 2. 已知厂商按域名匹配 (deepseek/stepfun/siliconflow/openrouter/novita)
//! 3. 其余 (中转站) 依次探测:
//!    - GET {base_url}/dashboard/billing/subscription + /usage  (OpenAI 兼容, new-api 支持)
//!    - GET {base_url}/user/balance                              (new-api 自有)
//!    - GET {base_url}/api/user/self                             (new-api 自带面板 API)
//!    - GET {base_url}/v1/usage                                  (部分中转站, 皮卡丘类)
//!
//! 错误语义: Err = 传输失败 (超时/断连); Ok(success:false) = 确定性失败 (鉴权/未知/非 JSON)

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct BalanceInfo {
    pub provider: String,
    pub success: bool,
    /// 剩余余额 (数值)
    pub balance: Option<f64>,
    pub currency: Option<String>,
    pub total: Option<f64>,
    pub used: Option<f64>,
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum BalanceError {
    #[error("网络错误: {0}")]
    Http(#[from] reqwest::Error),
    #[error("JSON 解析错误: {0}")]
    Json(#[from] serde_json::Error),
}

const TIMEOUT: Duration = Duration::from_secs(15);

/// usage script 配置 (cc-switch 对齐, deeplink v3.9+ / 编辑表单 / cc-switch DB 回填)
#[derive(Debug, Clone, Default)]
pub struct UsageScriptCfg {
    pub code: String,
    /// 用量查询专用 API key (通用模板用, {{apiKey}} 替换; 缺省用 profile key)
    pub api_key: Option<String>,
    /// 用量查询专用 base URL ({{baseUrl}} 替换; 缺省用 profile base_url)
    pub base_url: Option<String>,
    pub access_token: Option<String>,
    pub user_id: Option<String>,
    pub timeout_secs: Option<u64>,
}

/// 查询余额: usage script 优先, 其次厂商专用, 最后中转站通用探测。
/// `cc_switch_fallback`: profile 无脚本时从 cc-switch DB 按名称回填脚本
/// (用户之前用 cc-switch 导入过的中转站, 脚本在 cc-switch 的 providers.meta)。
pub async fn get_balance(
    base_url: &str,
    api_key: &str,
    usage: Option<&UsageScriptCfg>,
    profile_name: &str,
) -> Result<BalanceInfo, BalanceError> {
    // 1. usage script (profile 自带 → cc-switch DB 按名回填)
    let cfg = usage
        .cloned()
        .or_else(|| cc_switch_usage_script(profile_name));
    if let Some(cfg) = cfg {
        if !cfg.code.trim().is_empty() {
            return Ok(query_usage_script(profile_name, base_url, api_key, &cfg).await);
        }
    }

    // 2/3. 厂商专用 + 通用探测
    let client = reqwest::Client::builder().timeout(TIMEOUT).build()?;
    let provider = detect_provider(base_url);

    let info = match provider.as_deref() {
        Some("deepseek") => query_deepseek(&client, api_key).await?,
        Some("stepfun") => query_stepfun(&client, api_key).await?,
        Some("siliconflow") => query_siliconflow(&client, api_key).await?,
        Some("openrouter") => query_openrouter(&client, api_key).await?,
        Some("novita") => query_novita(&client, api_key).await?,
        _ => query_relay(&client, base_url, api_key).await?,
    };
    Ok(info)
}

// =============================================================================
// usage script 执行 (cc-switch 同机制, quickjs 沙箱)
// =============================================================================

/// 执行 usage script 查余额。
/// 脚本格式: `({request: {url, method, headers, body}, extractor: function(response){...}})`,
/// 变量 {{apiKey}} {{baseUrl}} {{accessToken}} {{userId}} 先替换。
/// quickjs 沙箱内无宿主 API — 脚本只能产出 request 配置; URL 由我们校验
/// (http(s) + 与 base_url 同源) 后发送; extractor 在响应 JSON 上求值。
pub async fn query_usage_script(
    profile_name: &str,
    default_base_url: &str,
    default_key: &str,
    cfg: &UsageScriptCfg,
) -> BalanceInfo {
    let api_key = cfg.api_key.as_deref().unwrap_or(default_key);
    let base_url = cfg.base_url.as_deref().unwrap_or(default_base_url);
    let mut code = cfg
        .code
        .replace("{{apiKey}}", api_key)
        .replace("{{baseUrl}}", base_url);
    if let Some(t) = &cfg.access_token {
        code = code.replace("{{accessToken}}", t);
    }
    if let Some(u) = &cfg.user_id {
        code = code.replace("{{userId}}", u);
    }

    // Phase A (quickjs): 求值 request 配置 (code 克隆, Phase B 还要用)
    let code_a = code.clone();
    let request = match tokio::task::spawn_blocking(move || -> Result<UsageRequest, String> {
        let runtime = rquickjs::Runtime::new().map_err(|e| format!("创建 JS 运行时失败: {e}"))?;
        let ctx =
            rquickjs::Context::full(&runtime).map_err(|e| format!("创建 JS 上下文失败: {e}"))?;
        ctx.with(|ctx| {
            let config: rquickjs::Object =
                ctx.eval(code_a).map_err(|e| format!("解析脚本失败: {e}"))?;
            let request: rquickjs::Object = config
                .get("request")
                .map_err(|_| "脚本缺少 request 配置".to_string())?;
            let url: String = request
                .get("url")
                .map_err(|_| "request.url 缺失".to_string())?;
            let method: String = request
                .get("method")
                .map_err(|_| "request.method 缺失".to_string())?;
            let headers: std::collections::HashMap<String, String> =
                request.get("headers").unwrap_or_default();
            let body: Option<String> = request.get("body").ok();
            Ok(UsageRequest {
                url,
                method,
                headers,
                body,
            })
        })
    })
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return fail(profile_name, format!("usage script: {e}")),
        Err(e) => return fail(profile_name, format!("usage script 执行中断: {e}")),
    };

    // URL 安全校验: http(s) + 与 base_url 同源 (脚本被中转站控制, 防 key 外发)
    let same_origin_ok = (|| -> Option<()> {
        let parsed = url::Url::parse(&request.url).ok()?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return None;
        }
        let base = url::Url::parse(base_url).ok()?;
        if parsed.scheme() != base.scheme()
            || parsed.host_str()? != base.host_str()?
            || parsed.port_or_known_default()? != base.port_or_known_default()?
        {
            return None;
        }
        Some(())
    })();
    if same_origin_ok.is_none() {
        return fail(
            profile_name,
            format!("usage script: 请求 URL 必须与 {base_url} 同源 (http/https)"),
        );
    }

    // HTTP 请求
    let timeout = cfg
        .timeout_secs
        .map(|s| Duration::from_secs(s.clamp(2, 30)))
        .unwrap_or(TIMEOUT);
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => return fail(profile_name, format!("HTTP 客户端构建失败: {e}")),
    };
    let method = match request.method.parse::<reqwest::Method>() {
        Ok(m) => m,
        Err(_) => {
            return fail(
                profile_name,
                format!("不支持的 HTTP 方法: {}", request.method),
            )
        }
    };
    let mut req = client.request(method, &request.url);
    for (k, v) in &request.headers {
        req = req.header(k, v);
    }
    if let Some(b) = &request.body {
        req = req.body(b.clone());
    }
    let response_text = match req.send().await {
        Ok(resp) if resp.status().is_success() => match resp.text().await {
            Ok(t) => t,
            Err(e) => return fail(profile_name, format!("读取响应失败: {e}")),
        },
        Ok(resp) => {
            return fail(
                profile_name,
                format!(
                    "usage script: HTTP {}: {}",
                    resp.status(),
                    resp.text()
                        .await
                        .unwrap_or_default()
                        .chars()
                        .take(200)
                        .collect::<String>()
                ),
            )
        }
        Err(e) => return fail(profile_name, format!("usage script 请求失败: {e}")),
    };

    // Phase B (quickjs): extractor(response) 提取余额
    let result: serde_json::Value =
        match tokio::task::spawn_blocking(move || -> Result<Value, String> {
            let runtime =
                rquickjs::Runtime::new().map_err(|e| format!("创建 JS 运行时失败: {e}"))?;
            let ctx = rquickjs::Context::full(&runtime)
                .map_err(|e| format!("创建 JS 上下文失败: {e}"))?;
            ctx.with(|ctx| {
                let config: rquickjs::Object =
                    ctx.eval(code).map_err(|e| format!("解析脚本失败: {e}"))?;
                let extractor: rquickjs::Function = config
                    .get("extractor")
                    .map_err(|_| "脚本缺少 extractor 函数".to_string())?;
                let response_js: rquickjs::Value = ctx
                    .json_parse(response_text.as_str())
                    .map_err(|e| format!("响应不是 JSON: {e}"))?;
                let result_js: rquickjs::Value = extractor
                    .call((response_js,))
                    .map_err(|e| format!("执行 extractor 失败: {e}"))?;
                let result_json: String = ctx
                    .json_stringify(result_js)
                    .map_err(|e| format!("序列化结果失败: {e}"))?
                    .ok_or_else(|| "序列化返回 None".to_string())?
                    .get()
                    .map_err(|e| format!("读取结果失败: {e}"))?;
                serde_json::from_str(&result_json).map_err(|e| format!("结果 JSON 解析失败: {e}"))
            })
        })
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return fail(profile_name, format!("usage script: {e}")),
            Err(e) => return fail(profile_name, format!("usage script 执行中断: {e}")),
        };

    // 结果: {isValid, remaining, unit, total?, used?}
    if result.get("isValid").and_then(Value::as_bool) == Some(false) {
        return fail(profile_name, "账号不可用 (isValid=false)".into());
    }
    let balance = result
        .get("remaining")
        .and_then(Value::as_f64)
        .or_else(|| result.get("balance").and_then(Value::as_f64));
    match balance {
        Some(b) => {
            let unit = result
                .get("unit")
                .and_then(Value::as_str)
                .unwrap_or("USD")
                .to_string();
            let total = result.get("total").and_then(Value::as_f64);
            let used = result.get("used").and_then(Value::as_f64);
            ok(profile_name, b, &unit, total, used)
        }
        None => fail(
            profile_name,
            format!("usage script: extractor 未返回余额字段 (remaining/balance): {result}"),
        ),
    }
}

/// quickjs 求值出的 request 配置
struct UsageRequest {
    url: String,
    method: String,
    headers: std::collections::HashMap<String, String>,
    body: Option<String>,
}

/// 从 cc-switch DB 按 profile 名称回填 usage script
/// (只读查询 providers.meta.usage_script, 不读 key/认证数据)
fn cc_switch_usage_script(profile_name: &str) -> Option<UsageScriptCfg> {
    let home = std::env::var("HOME").ok()?;
    let db_path = std::path::Path::new(&home).join(".cc-switch/cc-switch.db");
    if !db_path.exists() {
        return None;
    }
    let conn =
        rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    let meta: String = conn
        .query_row(
            "SELECT meta FROM providers WHERE app_type='codex' AND name=?1",
            [profile_name],
            |row| row.get(0),
        )
        .ok()?;
    let meta: Value = serde_json::from_str(&meta).ok()?;
    let script = meta.get("usage_script")?;
    let code = script.get("code").and_then(Value::as_str)?;
    Some(UsageScriptCfg {
        code: code.to_string(),
        api_key: script
            .get("apiKey")
            .and_then(Value::as_str)
            .map(str::to_string),
        base_url: script
            .get("baseUrl")
            .and_then(Value::as_str)
            .map(str::to_string),
        access_token: script
            .get("accessToken")
            .and_then(Value::as_str)
            .map(str::to_string),
        user_id: script
            .get("userId")
            .and_then(Value::as_str)
            .map(str::to_string),
        timeout_secs: script.get("timeout").and_then(Value::as_u64),
    })
}

/// 按域名精确匹配厂商 — 子串匹配会把 relay.openrouter-proxy.com 这类
/// 中转域名误判成厂商, 导致 key 发给错误方。
fn detect_provider(base_url: &str) -> Option<String> {
    let host = url::Url::parse(base_url).ok()?.host_str()?.to_lowercase();
    let is_domain = |d: &str| host == d || host.ends_with(&format!(".{d}"));
    if is_domain("deepseek.com") {
        Some("deepseek".into())
    } else if is_domain("stepfun.ai") || is_domain("stepfun.com") {
        Some("stepfun".into())
    } else if is_domain("siliconflow.cn") || is_domain("siliconflow.com") {
        Some("siliconflow".into())
    } else if is_domain("openrouter.ai") {
        Some("openrouter".into())
    } else if is_domain("novita.ai") {
        Some("novita".into())
    } else {
        None
    }
}

fn ok(
    provider: &str,
    balance: f64,
    currency: &str,
    total: Option<f64>,
    used: Option<f64>,
) -> BalanceInfo {
    BalanceInfo {
        provider: provider.to_string(),
        success: true,
        balance: Some(balance),
        currency: Some(currency.to_string()),
        total,
        used,
        error: None,
    }
}

fn fail(provider: &str, msg: String) -> BalanceInfo {
    BalanceInfo {
        provider: provider.to_string(),
        success: false,
        balance: None,
        currency: None,
        total: None,
        used: None,
        error: Some(msg),
    }
}

async fn query_deepseek(client: &reqwest::Client, key: &str) -> Result<BalanceInfo, BalanceError> {
    let resp = client
        .get("https://api.deepseek.com/user/balance")
        .bearer_auth(key)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Ok(fail("deepseek", http_error(resp).await));
    }
    // 200 但非 JSON (HTML 拦截页等) → 确定性失败, 而非把整个查询报成 Err
    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return Ok(fail("deepseek", format!("响应非 JSON: {e}"))),
    };
    // {"is_available": true, "balance_infos": [{"currency":"CNY","total_balance":..., ...}]}
    // 注意: total_balance 是字符串 ("14.41"), 不是数字 — 数字/字符串都兼容
    if let Some(infos) = body.get("balance_infos").and_then(|v| v.as_array()) {
        if let Some(info) = infos.first() {
            let balance = info.get("total_balance").and_then(|v| {
                v.as_f64()
                    .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
            });
            let currency = info
                .get("currency")
                .and_then(|v| v.as_str())
                .unwrap_or("CNY");
            if let Some(b) = balance {
                return Ok(ok("deepseek", b, currency, None, None));
            }
        }
    }
    Ok(fail("deepseek", "响应格式不符".into()))
}

async fn query_stepfun(client: &reqwest::Client, key: &str) -> Result<BalanceInfo, BalanceError> {
    let resp = client
        .get("https://api.stepfun.com/v1/accounts")
        .bearer_auth(key)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Ok(fail("stepfun", http_error(resp).await));
    }
    // 200 但非 JSON (HTML 拦截页等) → 确定性失败, 而非把整个查询报成 Err
    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return Ok(fail("stepfun", format!("响应非 JSON: {e}"))),
    };
    // {"data": [{"id":"...","balance":...,"promo_balance":...}]}
    if let Some(data) = body.get("data").and_then(|v| v.as_array()) {
        if let Some(acc) = data.first() {
            if let Some(b) = acc.get("balance").and_then(|v| v.as_f64()) {
                let promo = acc
                    .get("promo_balance")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                return Ok(ok("stepfun", b + promo, "CNY", None, None));
            }
        }
    }
    Ok(fail("stepfun", "响应格式不符".into()))
}

async fn query_siliconflow(
    client: &reqwest::Client,
    key: &str,
) -> Result<BalanceInfo, BalanceError> {
    let resp = client
        .get("https://api.siliconflow.cn/v1/user/info")
        .bearer_auth(key)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Ok(fail("siliconflow", http_error(resp).await));
    }
    // 200 但非 JSON (HTML 拦截页等) → 确定性失败, 而非把整个查询报成 Err
    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return Ok(fail("siliconflow", format!("响应非 JSON: {e}"))),
    };
    // {"data": {"balance":..., "total_recharge":...}}
    if let Some(data) = body.get("data") {
        if let Some(b) = data.get("balance").and_then(|v| v.as_f64()) {
            let total = data.get("total_recharge").and_then(|v| v.as_f64());
            return Ok(ok("siliconflow", b, "CNY", total, None));
        }
    }
    Ok(fail("siliconflow", "响应格式不符".into()))
}

async fn query_openrouter(
    client: &reqwest::Client,
    key: &str,
) -> Result<BalanceInfo, BalanceError> {
    let resp = client
        .get("https://openrouter.ai/api/v1/credits")
        .bearer_auth(key)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Ok(fail("openrouter", http_error(resp).await));
    }
    // 200 但非 JSON (HTML 拦截页等) → 确定性失败, 而非把整个查询报成 Err
    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return Ok(fail("openrouter", format!("响应非 JSON: {e}"))),
    };
    // {"credits_remaining": 12.34, "total_credits": 50.0, "total_usage": 37.66}
    if let Some(b) = body.get("credits_remaining").and_then(|v| v.as_f64()) {
        let total = body.get("total_credits").and_then(|v| v.as_f64());
        let used = body.get("total_usage").and_then(|v| v.as_f64());
        return Ok(ok("openrouter", b, "USD", total, used));
    }
    Ok(fail("openrouter", "响应格式不符".into()))
}

async fn query_novita(client: &reqwest::Client, key: &str) -> Result<BalanceInfo, BalanceError> {
    let resp = client
        .get("https://api.novita.ai/v3/user/balance")
        .bearer_auth(key)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Ok(fail("novita", http_error(resp).await));
    }
    // 200 但非 JSON (HTML 拦截页等) → 确定性失败, 而非把整个查询报成 Err
    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return Ok(fail("novita", format!("响应非 JSON: {e}"))),
    };
    // {"data": {"balance": 12.34, ...}}
    if let Some(b) = body
        .get("data")
        .and_then(|d| d.get("balance"))
        .and_then(|v| v.as_f64())
    {
        return Ok(ok("novita", b, "USD", None, None));
    }
    Ok(fail("novita", "响应格式不符".into()))
}

/// codex 语义: base_url 无 /v1 后缀时客户端自动补 /v1 (cc-switch 无 v1 配置
/// 能工作即因此)。探测/测试拼接同源对齐 — 否则 {base}/models 等路径可能被
/// 站点前端路由吞掉返回 SPA 首页 (200 HTML), 误报"响应不是 JSON"。
/// 返回候选前缀: base 显式含 /v1 时只有 [base]; 否则 [base, base/v1]。
pub fn candidate_bases(base_url: &str) -> Vec<String> {
    let base = base_url.trim_end_matches('/').to_string();
    if base.ends_with("/v1") || base.contains("/v1/") {
        vec![base]
    } else {
        vec![base.clone(), format!("{base}/v1")]
    }
}

/// 中转站通用探测: OpenAI 兼容 billing → new-api /user/balance → /api/user/self
async fn query_relay(
    client: &reqwest::Client,
    base_url: &str,
    key: &str,
) -> Result<BalanceInfo, BalanceError> {
    // 1. OpenAI 兼容: /dashboard/billing/subscription (总额度, USD 硬上限)
    for base in candidate_bases(base_url) {
        let sub_url = format!("{base}/dashboard/billing/subscription");
        if let Ok(resp) = client.get(&sub_url).bearer_auth(key).send().await {
            if resp.status().is_success() {
                if let Ok(body) = resp.json::<Value>().await {
                    if let Some(limit) = body.get("hard_limit_usd").and_then(|v| v.as_f64()) {
                        // 2. 已用量 (cents)
                        let usage_url = format!("{base}/dashboard/billing/usage");
                        let mut used: Option<f64> = None;
                        if let Ok(resp) = client.get(&usage_url).bearer_auth(key).send().await {
                            if let Ok(body) = resp.json::<Value>().await {
                                used = body
                                    .get("total_usage")
                                    .and_then(|v| v.as_f64())
                                    .map(|c| c / 100.0);
                            }
                        }
                        let remaining = (limit - used.unwrap_or(0.0)).max(0.0);
                        return Ok(ok("relay", remaining, "USD", Some(limit), used));
                    }
                }
            }
        }
    }

    // 2. new-api 自有: /user/balance
    for base in candidate_bases(base_url) {
        let ub_url = format!("{base}/user/balance");
        if let Ok(resp) = client.get(&ub_url).bearer_auth(key).send().await {
            if resp.status().is_success() {
                if let Ok(body) = resp.json::<Value>().await {
                    if let Some(b) = body.get("balance").and_then(|v| v.as_f64()) {
                        let currency = body
                            .get("currency")
                            .and_then(|v| v.as_str())
                            .unwrap_or("USD");
                        return Ok(ok("relay", b, currency, None, None));
                    }
                }
            }
        }
    }

    // 3. new-api 面板: /api/user/self → data.quota (token 单位, 大数)
    for base in candidate_bases(base_url) {
        let self_url = format!("{base}/api/user/self");
        if let Ok(resp) = client.get(&self_url).bearer_auth(key).send().await {
            if resp.status().is_success() {
                if let Ok(body) = resp.json::<Value>().await {
                    if let Some(quota) = body
                        .get("data")
                        .and_then(|d| d.get("quota"))
                        .and_then(|v| v.as_f64())
                    {
                        // new-api quota 单位是 token 数 (1 quota = $0.002)
                        let usd = quota * 0.002;
                        return Ok(ok("relay", usd, "USD", None, None));
                    }
                }
            }
        }
    }

    // 4. /v1/usage (部分中转站, 皮卡丘类): remaining/quota.remaining/balance + unit。
    // 用候选第一项: 无 v1 后缀的 base 即裸 base → /v1/usage; 显式含 v1 的 base
    // 拼出 /v1/v1/usage 本来就不存在, 但这类站不走此路径, 保持原行为不回归。
    let v1_url = format!("{}/v1/usage", candidate_bases(base_url)[0]);
    if let Ok(resp) = client.get(&v1_url).bearer_auth(key).send().await {
        if resp.status().is_success() {
            if let Ok(body) = resp.json::<Value>().await {
                let b = body
                    .get("remaining")
                    .and_then(Value::as_f64)
                    .or_else(|| body.get("balance").and_then(Value::as_f64))
                    .or_else(|| {
                        body.get("quota")
                            .and_then(|q| q.get("remaining"))
                            .and_then(Value::as_f64)
                    });
                if let Some(b) = b {
                    let unit = body.get("unit").and_then(Value::as_str).unwrap_or("USD");
                    return Ok(ok("relay", b, unit, None, None));
                }
            }
        }
    }

    Ok(fail("relay", "该中转站没有可用的余额接口".into()))
}

async fn http_error(resp: reqwest::Response) -> String {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    format!(
        "HTTP {status}: {}",
        body.chars().take(200).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_known_providers() {
        assert_eq!(
            detect_provider("https://api.deepseek.com/v1"),
            Some("deepseek".into())
        );
        assert_eq!(
            detect_provider("https://openrouter.ai/api/v1"),
            Some("openrouter".into())
        );
        assert_eq!(
            detect_provider("https://api.siliconflow.cn/v1"),
            Some("siliconflow".into())
        );
        assert_eq!(
            detect_provider("https://api.novita.ai/v3"),
            Some("novita".into())
        );
        assert_eq!(
            detect_provider("https://api.stepfun.com/v1"),
            Some("stepfun".into())
        );
        assert_eq!(detect_provider("https://relay.example.com/v1"), None);
    }
}
