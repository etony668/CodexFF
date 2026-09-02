//! 官方订阅额度查询 (cc-switch 对齐)。
//!
//! 端点: GET https://chatgpt.com/backend-api/wham/usage
//! 关键: User-Agent 必须为 "codex-cli" (OpenAI 自家客户端 UA 过 Cloudflare,
//! 默认 reqwest UA 会被 CF 人机质询 403 拦截)。
//! 凭据: auth.json (auth_mode == "chatgpt" 的 OAuth tokens), 非 OAuth 模式不可查询。
//! 网络: macOS 系统代理 (chatgpt.com 国内不可直连)。

use serde::Serialize;

use crate::codex_config;

#[derive(Serialize)]
pub struct QuotaWindow {
    pub used_percent: f64,
    pub limit_window_seconds: i64,
    pub reset_after_seconds: Option<i64>,
    pub reset_at: Option<i64>,
}

#[derive(Serialize)]
pub struct OfficialQuota {
    pub plan_type: Option<String>,
    pub email: Option<String>,
    pub allowed: bool,
    pub limit_reached: bool,
    /// 周窗口 (604800s)
    pub primary_window: Option<QuotaWindow>,
    /// 5 小时窗口 (18000s) — Plus 计划可能为 null
    pub secondary_window: Option<QuotaWindow>,
    pub error: Option<String>,
}

impl OfficialQuota {
    fn fail(msg: impl Into<String>) -> OfficialQuota {
        OfficialQuota {
            plan_type: None,
            email: None,
            allowed: false,
            limit_reached: false,
            primary_window: None,
            secondary_window: None,
            error: Some(msg.into()),
        }
    }
}

/// macOS 系统代理 (scutil --proxy): HTTPS 优先, 无则 HTTP, 再则 SOCKS;
/// 显式代理全关时回退到 CFNetwork 解析 PAC。
fn explicit_system_proxy_url() -> Option<String> {
    let mut command = std::process::Command::new("scutil");
    crate::process_utils::hide_console_window(&mut command);
    let out = command.arg("--proxy").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut https_enabled = false;
    let mut https_host: Option<String> = None;
    let mut https_port: Option<String> = None;
    let mut http_enabled = false;
    let mut http_host: Option<String> = None;
    let mut http_port: Option<String> = None;
    let mut socks_enabled = false;
    let mut socks_host: Option<String> = None;
    let mut socks_port: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("HTTPSEnable : 1") {
            https_enabled = true;
        }
        if line.starts_with("HTTPSProxy : ") && https_host.is_none() {
            https_host = Some(line["HTTPSProxy : ".len()..].trim().to_string());
        }
        if line.starts_with("HTTPSPort : ") && https_port.is_none() {
            https_port = Some(line["HTTPSPort : ".len()..].trim().to_string());
        }
        if line.starts_with("HTTPEnable : 1") {
            http_enabled = true;
        }
        if line.starts_with("HTTPProxy : ") && http_host.is_none() {
            http_host = Some(line["HTTPProxy : ".len()..].trim().to_string());
        }
        if line.starts_with("HTTPPort : ") && http_port.is_none() {
            http_port = Some(line["HTTPPort : ".len()..].trim().to_string());
        }
        if line.starts_with("SOCKSEnable : 1") {
            socks_enabled = true;
        }
        if line.starts_with("SOCKSProxy : ") && socks_host.is_none() {
            socks_host = Some(line["SOCKSProxy : ".len()..].trim().to_string());
        }
        if line.starts_with("SOCKSPort : ") && socks_port.is_none() {
            socks_port = Some(line["SOCKSPort : ".len()..].trim().to_string());
        }
    }
    if https_enabled {
        if let (Some(h), Some(p)) = (https_host, https_port) {
            return Some(format!("http://{h}:{p}"));
        }
    }
    if http_enabled {
        if let (Some(h), Some(p)) = (http_host, http_port) {
            return Some(format!("http://{h}:{p}"));
        }
    }
    if socks_enabled {
        let h = socks_host?;
        let p = socks_port?;
        return Some(format!("socks5://{h}:{p}"));
    }
    None
}

pub(crate) fn system_proxy_url() -> Option<String> {
    system_proxy_url_for("https://1.1.1.1/dns-query")
}

/// 过滤旧版 DNS 守护劫持遗留的本地前置代理 (127.0.0.1:19090):
/// 它只为自己服务, 全系统流量经它会 502; 旧状态文件/旧守护进程残留时
/// 不能把它当成出口代理, 否则出口检测/DoH 上游全被拖死。
fn ignore_legacy_local_front_proxy(url: Option<String>) -> Option<String> {
    url.and_then(|u| {
        let parsed = url::Url::parse(&u).ok()?;
        let host = parsed.host_str()?;
        let port = parsed.port_or_known_default()?;
        let is_loopback = host.eq_ignore_ascii_case("localhost")
            || host == "127.0.0.1"
            || host == "::1"
            || host == "[::1]";
        if is_loopback && port == 19090 {
            None
        } else {
            Some(u)
        }
    })
}

/// 针对具体目标 URL 解析系统代理; 显式 HTTP/HTTPS/SOCKS 优先, 都没有时
/// 交给 CFNetwork 解析 PAC。
pub(crate) fn system_proxy_url_for(target: &str) -> Option<String> {
    ignore_legacy_local_front_proxy(explicit_system_proxy_url())
        .or_else(|| ignore_legacy_local_front_proxy(system_pac_proxy_url(target)))
}

/// 通过进程名判断 PID 是否为常见代理客户端 (避免把普通本地服务当代理)。
fn is_known_proxy_process(pid: u32) -> bool {
    const KEYWORDS: &[&str] = &[
        "flclash",
        "clash",
        "mihomo",
        "sing-box",
        "singbox",
        "verge",
        "surge",
        "v2ray",
        "xray",
        "shadowsocks",
        "ss-local",
        "naive",
        "hysteria",
        "libcyber",
        "quantumult",
        "qv2ray",
        "trojan",
        "hiddify",
        "nekoray",
        "stash",
        "outline",
    ];
    // 进程名 (core-darwin-arm64 这类不带产品名的进程也要能识别)
    let mut ps = std::process::Command::new("/bin/ps");
    crate::process_utils::hide_console_window(&mut ps);
    if let Ok(out) = ps.args(["-p", &pid.to_string(), "-o", "comm="]).output() {
        let name = String::from_utf8_lossy(&out.stdout)
            .trim()
            .to_ascii_lowercase();
        if KEYWORDS.iter().any(|k| name.contains(k)) {
            return true;
        }
    }
    // 进程名不含关键字时, 看可执行路径 (如 /Applications/LibCyber Desktop.app/...)
    let mut lsof = std::process::Command::new("/usr/sbin/lsof");
    crate::process_utils::hide_console_window(&mut lsof);
    if let Ok(out) = lsof
        .args(["-nP", "-a", "-p", &pid.to_string(), "-d", "txt", "-Fn"])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
        if KEYWORDS.iter().any(|k| text.contains(k)) {
            return true;
        }
    }
    false
}

/// 系统代理为空时, 扫描常见代理客户端的本地监听端口 (TUN/增强模式
/// 不写系统代理, 但客户端通常仍在本机开 7890 等混合端口)。
pub(crate) fn fallback_local_proxy_url() -> Option<String> {
    const CANDIDATES: &[(u16, &str)] = &[
        (7890, "http"),
        (7891, "http"),
        (7892, "socks5"),
        (1087, "http"),
        (1080, "socks5"),
        (8888, "http"),
        (8889, "socks5"),
        (6152, "http"),
        (8890, "http"),
        (8891, "socks5"),
        (10809, "http"),
        (10808, "socks5"),
    ];
    for (port, scheme) in CANDIDATES {
        let mut lsof = std::process::Command::new("/usr/sbin/lsof");
        crate::process_utils::hide_console_window(&mut lsof);
        let Ok(out) = lsof
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-F", "p"])
            .output()
        else {
            continue;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let Some(pid_line) = text.lines().find_map(|l| l.strip_prefix('p')) else {
            continue;
        };
        let Ok(pid) = pid_line.trim().parse::<u32>() else {
            continue;
        };
        if is_known_proxy_process(pid) {
            return Some(format!("{scheme}://127.0.0.1:{port}"));
        }
    }
    None
}

/// 有效代理: 系统代理优先 (含 PAC), 无则扫描常见本地代理端口兜底。
pub(crate) fn effective_proxy_url() -> Option<String> {
    system_proxy_url().or_else(fallback_local_proxy_url)
}

/// 通过 CFNetwork 让 macOS 自己解析 PAC, 返回目标 URL 应该使用的代理。
/// 仅用于“显式 HTTP/HTTPS/SOCKS 都没有配置、但开了 PAC”的场景。
#[cfg(target_os = "macos")]
pub(crate) fn system_pac_proxy_url(target: &str) -> Option<String> {
    use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
    use core_foundation_sys::base::{kCFAllocatorDefault, CFRelease, CFTypeRef};
    use core_foundation_sys::dictionary::{CFDictionaryGetValue, CFDictionaryRef};
    use core_foundation_sys::number::{kCFNumberIntType, CFNumberGetValue, CFNumberRef};
    use core_foundation_sys::string::{
        kCFStringEncodingUTF8, CFStringCreateWithCString, CFStringGetCString, CFStringGetLength,
        CFStringRef,
    };
    use core_foundation_sys::url::{CFURLCreateWithString, CFURLRef};
    use std::ffi::{CStr, CString};
    use std::os::raw::c_char;
    use std::os::raw::c_void;

    #[link(name = "CFNetwork", kind = "framework")]
    extern "C" {
        fn CFNetworkCopySystemProxySettings() -> CFDictionaryRef;
        fn CFNetworkCopyProxiesForURL(url: CFURLRef, settings: CFDictionaryRef) -> CFArrayRef;
    }

    unsafe fn cfstring(value: &str) -> Option<CFStringRef> {
        let c = CString::new(value).ok()?;
        Some(CFStringCreateWithCString(
            kCFAllocatorDefault,
            c.as_ptr(),
            kCFStringEncodingUTF8,
        ))
    }

    unsafe fn cfstring_value(s: CFStringRef) -> Option<String> {
        if s.is_null() {
            return None;
        }
        let len = CFStringGetLength(s);
        let mut buf = vec![0u8; len as usize * 4 + 1];
        let ok = CFStringGetCString(
            s,
            buf.as_mut_ptr() as *mut c_char,
            buf.len() as _,
            kCFStringEncodingUTF8,
        );
        if ok == 0 {
            return None;
        }
        Some(
            CStr::from_ptr(buf.as_ptr() as *const c_char)
                .to_string_lossy()
                .into_owned(),
        )
    }

    unsafe fn dict_string(dict: CFDictionaryRef, key: &str) -> Option<String> {
        let key_ref = cfstring(key)?;
        let val = CFDictionaryGetValue(dict, key_ref as *const c_void);
        CFRelease(key_ref as CFTypeRef);
        if val.is_null() {
            None
        } else {
            cfstring_value(val as CFStringRef)
        }
    }

    unsafe fn dict_port(dict: CFDictionaryRef, key: &str) -> Option<u16> {
        let key_ref = cfstring(key)?;
        let val = CFDictionaryGetValue(dict, key_ref as *const c_void);
        CFRelease(key_ref as CFTypeRef);
        if val.is_null() {
            return None;
        }
        let mut port: i32 = 0;
        if !CFNumberGetValue(
            val as CFNumberRef,
            kCFNumberIntType,
            &mut port as *mut i32 as *mut c_void,
        ) {
            return None;
        }
        u16::try_from(port).ok()
    }

    unsafe {
        let url_str = cfstring(target)?;
        let url = CFURLCreateWithString(kCFAllocatorDefault, url_str, std::ptr::null());
        CFRelease(url_str as CFTypeRef);
        if url.is_null() {
            return None;
        }
        let settings = CFNetworkCopySystemProxySettings();
        let proxies = CFNetworkCopyProxiesForURL(url, settings);
        if !settings.is_null() {
            CFRelease(settings as CFTypeRef);
        }
        CFRelease(url as CFTypeRef);
        if proxies.is_null() {
            return None;
        }

        let mut result = None;
        if CFArrayGetCount(proxies) > 0 {
            let proxy = CFArrayGetValueAtIndex(proxies, 0) as CFDictionaryRef;
            if !proxy.is_null() {
                let type_key = cfstring("kCFProxyTypeKey")?;
                let type_val = CFDictionaryGetValue(proxy, type_key as *const c_void);
                CFRelease(type_key as CFTypeRef);
                if !type_val.is_null() {
                    let proxy_type = cfstring_value(type_val as CFStringRef);
                    match proxy_type.as_deref() {
                        Some("kCFProxyTypeHTTP") | Some("kCFProxyTypeHTTPS") => {
                            let host = dict_string(proxy, "kCFProxyHostNameKey");
                            let port = dict_port(proxy, "kCFProxyPortNumberKey");
                            if let (Some(h), Some(p)) = (host, port) {
                                result = Some(format!("http://{h}:{p}"));
                            }
                        }
                        Some(t) if t.starts_with("kCFProxyTypeSOCKS") => {
                            let host = dict_string(proxy, "kCFProxyHostNameKey");
                            let port = dict_port(proxy, "kCFProxyPortNumberKey");
                            if let (Some(h), Some(p)) = (host, port) {
                                result = Some(format!("socks5://{h}:{p}"));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        CFRelease(proxies as CFTypeRef);
        result
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn system_pac_proxy_url(_target: &str) -> Option<String> {
    None
}

#[derive(serde::Deserialize)]
struct WindowJson {
    used_percent: f64,
    limit_window_seconds: i64,
    #[serde(default)]
    reset_after_seconds: Option<i64>,
    #[serde(default)]
    reset_at: Option<i64>,
}

#[derive(serde::Deserialize)]
struct RateLimitJson {
    allowed: bool,
    limit_reached: bool,
    #[serde(default)]
    primary_window: Option<WindowJson>,
    #[serde(default)]
    secondary_window: Option<WindowJson>,
}

#[derive(serde::Deserialize)]
struct UsageJson {
    plan_type: Option<String>,
    email: Option<String>,
    rate_limit: Option<RateLimitJson>,
}

pub async fn query_official_quota() -> Result<OfficialQuota, String> {
    // 凭据: 仅 chatgpt OAuth 形态可查询
    let auth_text = std::fs::read_to_string(codex_config::codex_auth_path())
        .map_err(|e| format!("读取 auth.json 失败: {e}"))?;
    let auth: serde_json::Value =
        serde_json::from_str(&auth_text).map_err(|e| format!("auth.json 解析失败: {e}"))?;
    if auth.get("auth_mode").and_then(|v| v.as_str()) != Some("chatgpt") {
        return Ok(OfficialQuota::fail(
            "当前非 ChatGPT OAuth 登录形态, 无法查询官方额度。切到官方并重新 codex login。",
        ));
    }
    let token = auth
        .pointer("/tokens/access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "auth.json 缺少 OAuth access_token".to_string())?;
    let account_id = auth.pointer("/tokens/account_id").and_then(|v| v.as_str());

    // 客户端: 系统代理 (chatgpt.com 需代理可达)
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("codex-cli");
    if let Some(p) = system_proxy_url_for("https://chatgpt.com/backend-api/wham/usage") {
        if let Ok(proxy) = reqwest::Proxy::all(p) {
            builder = builder.proxy(proxy);
        }
    }
    let client = builder.build().map_err(|e| e.to_string())?;

    let mut req = client
        .get("https://chatgpt.com/backend-api/wham/usage")
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json");
    if let Some(id) = account_id {
        req = req.header("ChatGPT-Account-Id", id);
    }
    let resp = req.send().await.map_err(|e| format!("网络错误: {e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Ok(OfficialQuota::fail(format!(
            "官方登录已过期 (HTTP {status}) — 请切到官方并重新 codex login"
        )));
    }
    if !status.is_success() {
        return Ok(OfficialQuota::fail(format!("官方额度接口 HTTP {status}")));
    }
    let body: UsageJson = resp
        .json()
        .await
        .map_err(|e| format!("响应解析失败: {e}"))?;
    let rl = body.rate_limit;
    Ok(OfficialQuota {
        plan_type: body.plan_type,
        email: body.email,
        allowed: rl.as_ref().map(|r| r.allowed).unwrap_or(false),
        limit_reached: rl.as_ref().map(|r| r.limit_reached).unwrap_or(false),
        primary_window: rl
            .as_ref()
            .and_then(|r| r.primary_window.as_ref())
            .map(wrap),
        secondary_window: rl
            .as_ref()
            .and_then(|r| r.secondary_window.as_ref())
            .map(wrap),
        error: None,
    })
}

fn wrap(w: &WindowJson) -> QuotaWindow {
    QuotaWindow {
        used_percent: w.used_percent,
        limit_window_seconds: w.limit_window_seconds,
        reset_after_seconds: w.reset_after_seconds,
        reset_at: w.reset_at,
    }
}
