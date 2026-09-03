//! IP 指纹守护 — 官方 profile 的网络出口一致性检测。
//!
//! 封号主因之一是官方账号活跃 IP 频繁变化 (出口节点乱跳/共享 IP)。
//! 策略: 记录每次官方激活时的出口 IP, 下次激活时比对, 变了就警告。
//!
//! DNS 泄露守护 — 对齐 ip.net.coffee/dns/ 方法论: 用唯一子域名查询多个
//! DoH 解析器, 取解析器出口 IP (Google /resolve 的 resolver_ip 字段),
//! 与当前出口 IP 比对 — 不一致 = DNS 解析没走代理/VPN, 真实位置对
//! OpenAI 可见 (封号风险)。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::vault::{self, VaultError};

// IPv4 优先 (IPv6 与解析器出口 IPv4 比对会误报泄露)
const IP_SERVICES: &[&str] = &[
    "https://ipinfo.io/ip",
    "https://ifconfig.me/ip",
    "https://api.ipify.org",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpHistory {
    /// 上次官方 profile 激活时的出口 IP
    pub last_official_ip: Option<String>,
    pub last_official_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IpCheckResult {
    pub current_ip: Option<String>,
    pub last_official_ip: Option<String>,
    /// IP 变了 → 警告 (封号风险升高)
    pub changed: bool,
    pub unknown: bool,
}

/// 出口 IP 类型检测 (ipinfo org → 数据中心判定)。
/// 共享出口 + 数据中心 IP 是风控高危信号 (批量账号池特征)。
#[derive(Debug, Clone, Serialize)]
pub struct IpTypeResult {
    pub ip: Option<String>,
    /// 归属组织 (如 "AS16509 Amazon.com, Inc.")
    pub org: Option<String>,
    /// true = 数据中心/云厂商 IP (高风险); None = 无法判定
    pub hosting: Option<bool>,
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum IpGuardError {
    #[error("vault 错误: {0}")]
    Vault(#[from] VaultError),
    #[error("HTTP 错误: {0}")]
    Http(#[from] reqwest::Error),
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
}

// ---- DNS 泄露检测 (对齐 ip.net.coffee/dns/ 方法论, 权威服务器法) ----

/// 权威触发域: 系统解析器查 `<token>-<i>.d.ip.net.coffee` → 对方权威 DNS
/// 记录解析器出口 IP (国内可达, 无需 DoH 可达性依赖)
const LEAK_AUTH_DOMAIN: &str = "d.ip.net.coffee";
const LEAK_RESULT_API: &str = "https://ip.net.coffee/api/dns/result";
const LEAK_ROUNDS: usize = 3;

#[derive(Debug, Clone, Serialize)]
pub struct DnsLeakResult {
    /// 本次检测 token (唯一子域名, 防缓存)
    pub token: String,
    /// 解析器出口 IP 集合 (对方权威 DNS 观测到的)
    pub resolver_ips: Vec<String>,
    /// 当前出口 IP
    pub current_ip: Option<String>,
    /// true = 泄露 / DoH 降级 (DNS 未走出口代理); false = 无泄露; None = 无法判定
    pub leaking: Option<bool>,
    /// 成功触发的轮数
    pub rounds: usize,
    pub error: Option<String>,
    /// DoH 保护开启且存活 — 查询唯一路径 = 本地 stub → DoH 上游 (国内),
    /// 解析器出口必然 ≠ 代理出口, 但无本地泄露路径, 判定规则不同
    pub doh_protected: bool,
    /// DoH 保护中, stub 最近是否通过系统代理/TUN 隧道出口:
    /// Some(true)=走代理或 TUN; Some(false)=国内兜底/未走代理; None=未知
    pub dns_via_proxy: Option<bool>,
}

/// DNS 泄露检测 (权威服务器法, 同 ip.net.coffee/dns/):
/// 1. 生成随机 token, HTTP 触发 `<token>-<i>.d.ip.net.coffee` 解析 —
///    走系统解析器 → 对方权威 DNS 记录解析器出口 IP
/// 2. 轮询 /api/dns/result/{token} 取解析器 IP 集合
/// 3. 与当前出口 IP 比对: 任一不一致 = 泄露 (DNS 没走代理/VPN)
pub async fn check_dns_leak() -> DnsLeakResult {
    // token 需与页面生成格式一致 (小写字母数字, 服务器按 token-N 匹配)
    let token = format!("{}", uuid::Uuid::new_v4().simple());
    // 免费版已剥离 DNS 守护: 泄露检测按纯系统解析路径判定
    let doh_protected = false;
    let dns_via_proxy = None;
    // 注意: 这里不能用系统代理, 触发请求必须走本机系统解析器,
    // 否则代理会替我们解析域名, 测不到本地 DNS 路径。
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return DnsLeakResult {
                token,
                resolver_ips: vec![],
                current_ip: None,
                leaking: None,
                rounds: 0,
                error: Some(format!("HTTP 客户端构建失败: {e}")),
                doh_protected,
                dns_via_proxy,
            }
        }
    };

    // 1. 触发: 并行请求 3 个唯一子域名 (走系统解析器)。CN 解析器可能劫持
    // 随机子域 (NXDOMAIN 广告页) 导致 TLS 失败 — 失败时重试一整波。
    let mut rounds = 0usize;
    for wave in 0..2 {
        if rounds > 0 {
            break;
        }
        let mut handles = Vec::new();
        for i in 1..=LEAK_ROUNDS {
            let client = client.clone();
            let url = format!("https://{token}-{i}.{LEAK_AUTH_DOMAIN}/pixel.gif");
            handles.push(tokio::spawn(async move {
                client
                    .get(&url)
                    .send()
                    .await
                    .ok()
                    .map(|r| r.status().is_success())
                    .unwrap_or(false)
            }));
        }
        for h in handles {
            if h.await.unwrap_or(false) {
                rounds += 1;
            }
        }
        if rounds == 0 && wave == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        }
    }

    // 2. 轮询结果 (权威 DNS 记录落库需要 1-3s)
    let mut resolver_ips: Vec<String> = Vec::new();
    for _ in 0..6 {
        if !resolver_ips.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        if let Ok(resp) = client
            .get(format!("{LEAK_RESULT_API}/{token}"))
            .send()
            .await
        {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                if let Some(list) = json.get("dns_servers").and_then(|v| v.as_array()) {
                    resolver_ips = list
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_string)
                        .collect();
                }
            }
        }
    }

    // 3. 比对 (同族 IP 才可比: 解析器 IPv4 vs 出口 IPv6 不能判泄露)
    let current_ip = current_public_ip().await;
    if doh_protected {
        // DoH 场景下不能用“解析器出口 == 当前出口”判断: 上游是 1.1.1.1 /
        // 223.5.5.5 这类公共 DoH, 权威侧看到的永远是 DoH 服务器 IP, 而不是
        // 本机/出口。是否真的过了出口代理, 由 stub 运行时状态回答。
        let leaking = match dns_via_proxy {
            Some(true) => Some(false),
            Some(false) => Some(true),
            None => None,
        };
        let error = if leaking.is_none() {
            Some("DoH 保护生效, 但暂无法确认 stub 是否走代理".to_string())
        } else {
            None
        };
        return DnsLeakResult {
            token,
            resolver_ips,
            current_ip,
            leaking,
            rounds,
            error,
            doh_protected,
            dns_via_proxy,
        };
    }
    let leaking = match (&resolver_ips, &current_ip) {
        (ips, Some(cur)) if !ips.is_empty() => {
            let cur_parse = cur.parse::<std::net::IpAddr>().ok();
            let same_family = |r: &String| {
                cur_parse
                    .as_ref()
                    .map(|c| {
                        r.parse::<std::net::IpAddr>()
                            .map(|r| r.is_ipv4() == c.is_ipv4())
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
            };
            let comparable: Vec<&String> = ips.iter().filter(|r| same_family(r)).collect();
            if comparable.is_empty() {
                None
            } else {
                Some(comparable.iter().any(|r| r.as_str() != cur.as_str()))
            }
        }
        _ => None,
    };

    let error = if resolver_ips.is_empty() {
        Some(if rounds == 0 {
            "权威触发全部失败 (网络/系统解析器异常)".to_string()
        } else {
            "权威 DNS 未返回解析器记录, 无法判定".to_string()
        })
    } else if leaking.is_none() {
        Some("解析器与出口 IP 族不一致, 无法判定".to_string())
    } else {
        None
    };
    DnsLeakResult {
        token,
        resolver_ips,
        current_ip,
        leaking,
        rounds,
        error,
        doh_protected,
        dns_via_proxy,
    }
}

fn history_path() -> PathBuf {
    vault::vault_dir().join("ip-history.json")
}

fn load_history() -> IpHistory {
    let path = history_path();
    if !path.exists() {
        return IpHistory {
            last_official_ip: None,
            last_official_at: None,
        };
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(IpHistory {
            last_official_ip: None,
            last_official_at: None,
        })
}

/// 上次官方激活基线 IP
pub fn last_official_ip() -> Option<String> {
    load_history().last_official_ip
}

/// 云厂商/数据中心关键词 (org 字段匹配) — 判定出口为机房 IP
const CLOUD_KEYWORDS: &[&str] = &[
    "amazon",
    "aws",
    "google",
    "microsoft",
    "azure",
    "digitalocean",
    "linode",
    "vultr",
    "alibaba",
    "tencent",
    "oracle",
    "hetzner",
    "ovh",
    "akamai",
    "cloudflare",
    "huawei",
    "leaseweb",
    "contabo",
    "serverfarms",
    "hosting",
    "datacamp",
    "amazon.com",
];

/// 检测出口 IP 类型: ipinfo.io/json (免费, org 字段)。走系统代理 (同官方额度)。
pub async fn check_ip_type() -> IpTypeResult {
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("codexff/0.1");
    if let Some(p) = crate::official_quota::system_proxy_url_for("https://ipinfo.io/json") {
        if let Ok(proxy) = reqwest::Proxy::all(p) {
            builder = builder.proxy(proxy);
        }
    }
    let Ok(client) = builder.build() else {
        return IpTypeResult {
            ip: None,
            org: None,
            hosting: None,
            error: Some("客户端构建失败".into()),
        };
    };
    let resp = match client.get("https://ipinfo.io/json").send().await {
        Ok(r) => r,
        Err(e) => {
            return IpTypeResult {
                ip: None,
                org: None,
                hosting: None,
                error: Some(format!("查询失败: {e}")),
            }
        }
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return IpTypeResult {
                ip: None,
                org: None,
                hosting: None,
                error: Some(format!("响应非 JSON: {e}")),
            }
        }
    };
    let ip = body.get("ip").and_then(|v| v.as_str()).map(str::to_string);
    let org = body.get("org").and_then(|v| v.as_str()).map(str::to_string);
    let hosting = org.as_deref().map(|o| {
        let l = o.to_lowercase();
        CLOUD_KEYWORDS.iter().any(|k| l.contains(k))
    });
    IpTypeResult {
        ip,
        org,
        hosting,
        error: None,
    }
}

/// 官方激活时记录当前出口 IP
pub fn record_official_activation(ip: Option<String>) -> Result<(), IpGuardError> {
    let mut history = load_history();
    history.last_official_ip = ip;
    history.last_official_at = Some(chrono::Local::now().to_rfc3339());
    let path = history_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    vault::atomic_write_bytes(&path, serde_json::to_string_pretty(&history)?.as_bytes())?;
    Ok(())
}

/// 查当前出口 IP (多服务 fallback)
pub async fn current_public_ip() -> Option<String> {
    // 走系统代理查出口, 否则在“仅 HTTP 代理、没有 TUN”的出口模式下
    // 会拿到本地宽带 IP 而不是出口 IP。
    for service in IP_SERVICES {
        let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(6));
        if let Some(p) = crate::official_quota::system_proxy_url_for(service) {
            if let Ok(proxy) = reqwest::Proxy::all(p) {
                builder = builder.proxy(proxy);
            }
        }
        let client = builder.build().ok()?;
        if let Ok(resp) = client.get(*service).send().await {
            if let Ok(text) = resp.text().await {
                let ip = text.trim().to_string();
                // 必须能解析成 IP — 代理拦截页/HTML 错误页会被拒, 防止脏基线
                if ip.parse::<std::net::IpAddr>().is_ok() {
                    return Some(ip);
                }
            }
        }
    }
    None
}

/// IP 检测缓存: get_status 每次刷新都调 check_ip, 30s 内不重复出网
const IP_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);
static IP_CACHE: std::sync::Mutex<Option<(String, std::time::Instant)>> =
    std::sync::Mutex::new(None);

/// 网络状态签名: 默认路由接口 + 带 IPv4 地址的 utun 接口。
/// 机场切节点/开关 TUN/换 Wi-Fi 时这些会变化, 签名变化即强制重新检测,
/// 让出口 IP 能跟随网络变化实时更新 (不受 30s 缓存影响)。
fn network_signature() -> String {
    let mut sig = String::new();
    let mut netstat = std::process::Command::new("netstat");
    crate::process_utils::hide_console_window(&mut netstat);
    if let Ok(out) = netstat.args(["-rn", "-f", "inet"]).output() {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            if parts.next() == Some("default") {
                if let Some(netif) = parts.last() {
                    sig.push_str("route:");
                    sig.push_str(netif);
                    sig.push(';');
                }
            }
        }
    }
    let mut ifconfig = std::process::Command::new("ifconfig");
    crate::process_utils::hide_console_window(&mut ifconfig);
    if let Ok(out) = ifconfig.output() {
        let text = String::from_utf8_lossy(&out.stdout);
        let mut in_utun = false;
        for line in text.lines() {
            let t = line.trim_start();
            if t.starts_with("utun") {
                in_utun = true;
                sig.push_str("utun:");
                if let Some(name) = t.split_whitespace().next() {
                    sig.push_str(name);
                }
                sig.push(';');
                continue;
            }
            if in_utun {
                if !t.starts_with('\t') && !t.starts_with(' ') {
                    in_utun = false;
                } else if let Some(ip) = t.strip_prefix("inet ") {
                    if let Some(ip) = ip.split_whitespace().next() {
                        sig.push_str(ip);
                        sig.push(';');
                    }
                }
            }
        }
    }
    sig
}

static LAST_NET_SIG: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 检测当前出口 IP 与上次官方激活时是否一致
pub async fn check_ip() -> IpCheckResult {
    let history = load_history();
    let now = std::time::Instant::now();
    // 网络状态变化 → 强制重新出网检测, 否则复用 30s 缓存
    let net_sig = network_signature();
    let net_changed = {
        let mut last = LAST_NET_SIG.lock().unwrap_or_else(|e| e.into_inner());
        let changed = last.as_ref() != Some(&net_sig);
        *last = Some(net_sig);
        changed
    };
    let cached = IP_CACHE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let current = match cached {
        Some((ip, at)) if !net_changed && now.duration_since(at) < IP_CACHE_TTL => Some(ip),
        _ => {
            let ip = current_public_ip().await;
            if let Some(ip) = &ip {
                *IP_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((ip.clone(), now));
            }
            ip
        }
    };
    match (current.clone(), history.last_official_ip.clone()) {
        (Some(cur), Some(last)) => IpCheckResult {
            current_ip: Some(cur.clone()),
            last_official_ip: Some(last.clone()),
            changed: cur != last,
            unknown: false,
        },
        (Some(cur), None) => IpCheckResult {
            current_ip: Some(cur),
            last_official_ip: None,
            changed: false,
            unknown: true, // 首次, 无基线
        },
        (None, last) => IpCheckResult {
            current_ip: None,
            last_official_ip: last,
            changed: false,
            unknown: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实网络检测 (默认跳过; cargo test --lib -- --ignored 运行)
    #[tokio::test]
    #[ignore]
    async fn dns_leak_check_network() {
        let r = check_dns_leak().await;
        eprintln!("dns leak result: {r:?}");
        assert!(
            !r.resolver_ips.is_empty() || r.error.is_some(),
            "既无记录也无错误"
        );
        if !r.resolver_ips.is_empty() {
            assert!(r.leaking.is_some(), "无法判定: {:?}", r.error);
        }
    }
}
