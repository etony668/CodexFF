//! 凭证金库 — 官方凭证与中转 key 的物理隔离。
//!
//! 安全模型:
//! - 官方凭证只在官方 profile 激活时存在于 ~/.codex/auth.json
//! - 切到中转时, 官方凭证被 seal 进 vault 并从 auth.json 物理移除
//! - 中转 key 存 vault 文件 (0600), 默认不访问系统 keyring;
//!   CODEXFF_KEYRING=1 时才切回 keyring (兼容旧版本, 会触发钥匙串授权)
//! - vault 目录权限 700
//!
//! 即使代理被绕过、config 被篡改, 中转模式下官方凭证也不在磁盘上。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::codex_config;

pub const OFFICIAL_AUTH_FILENAME: &str = "official-auth.json";
pub const BACKUP_DIR_NAME: &str = "backups";
pub const RELAY_STATE_FILENAME: &str = "relay-state.json";

/// 切到中转前的官方配置顶层字段 — 切回官方时还原
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RelayState {
    pub prev_model: Option<String>,
    pub prev_effort: Option<String>,
    pub prev_disable_storage: Option<bool>,
}

fn relay_state_path() -> PathBuf {
    vault_dir().join(RELAY_STATE_FILENAME)
}

pub fn save_relay_state(state: &RelayState) -> Result<(), VaultError> {
    ensure_vault_dir()?;
    atomic_write_bytes(
        &relay_state_path(),
        serde_json::to_string_pretty(state)?.as_bytes(),
    )
}

pub fn load_relay_state() -> RelayState {
    let path = relay_state_path();
    if !path.exists() {
        return RelayState::default();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn clear_relay_state() {
    let _ = std::fs::remove_file(relay_state_path());
}

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("vault 目录不可用: {0}")]
    Dir(String),
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error("keyring 错误: {0}")]
    Keyring(String),
}

/// vault 根目录 (~/.codexff/vault 或 env CODEXFF_VAULT_DIR 覆盖)
pub fn vault_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CODEXFF_VAULT_DIR") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    dirs::data_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
        .join("codexff")
        .join("vault")
}

fn official_auth_path() -> PathBuf {
    vault_dir().join(OFFICIAL_AUTH_FILENAME)
}

fn backup_dir() -> PathBuf {
    vault_dir().join(BACKUP_DIR_NAME)
}

fn ensure_vault_dir() -> Result<(), VaultError> {
    let dir = vault_dir();
    fs::create_dir_all(&dir).map_err(|e| VaultError::Dir(e.to_string()))?;
    // 仅本用户可访问
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// tmp 文件唯一后缀计数器
static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
use std::sync::atomic::Ordering;

/// 原子写文件: 临时文件 + rename, 避免半截文件。
/// 安全: tmp 权限 0600 (凭证可能明文), 写入后 fsync 再 rename (防崩溃丢数据),
/// rename 失败清理 tmp。
pub fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<(), VaultError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // 唯一 tmp 后缀: 同进程并发写同一目标不得共用同一 tmp 路径
    // (否则线程 A 写一半被 B 截断, A 再 rename 发布交错内容)
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{}-{n}", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        fs::write(&tmp, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        // 数据落盘再 rename — 否则断电后 rename 已生效但内容未写全。
        // 必须用写句柄 fsync: 只读句柄在 macOS 上 EINVAL, 错误被吞等于没落盘。
        let f = fs::OpenOptions::new().write(true).open(&tmp)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(VaultError::Io)
}

/// vault 里是否有官方凭证 (决定官方激活时能否自动恢复登录)
pub fn restore_has_credentials() -> bool {
    official_auth_path().exists()
}

/// 官方模式下补捕获: codex login 后 vault 尚无官方凭证副本时, 从当前
/// auth.json 拷一份入 vault。原设计只在切中转 (seal) 时捕获 — 登录后
/// 一直停留官方模式则永不落库, Settings 永远显示"未保存"。
///
/// 幂等: vault 已有副本直接跳过; 仅捕获官方凭证形态 (旧 ChatGPT 对象 /
/// 新 tokens.auth_mode=chatgpt / 非空 OPENAI_API_KEY), 我们写的中转 key
/// 文件 (带归属标记) 不捕获 — 防止把中转 key 当官方凭证备份。
pub fn capture_official_if_missing() -> Result<(), VaultError> {
    if official_auth_path().exists() {
        return Ok(());
    }
    let auth_path = codex_config::codex_auth_path();
    if !auth_path.exists() {
        return Ok(());
    }
    let bytes = fs::read(&auth_path)?;
    let parsed: Value = serde_json::from_slice(&bytes)?;
    let obj = parsed.as_object();
    let has_chatgpt = matches!(extract_official_credentials(&parsed), Value::Object(_));
    // codex tokens 重构后: 顶层 auth_mode=chatgpt + tokens 对象
    let has_new_format = obj
        .map(|o| {
            o.get("auth_mode").and_then(Value::as_str) == Some("chatgpt")
                && o.get("tokens").map(|t| t.is_object()).unwrap_or(false)
        })
        .unwrap_or(false);
    let has_api_key = obj
        .map(|o| {
            o.get("OPENAI_API_KEY")
                .and_then(Value::as_str)
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        })
        .unwrap_or(false);
    let is_our_relay_key = obj
        .map(|o| {
            o.get(RELAY_AUTH_MARKER).and_then(Value::as_bool) == Some(true)
                || (o.len() == 1 && o.contains_key("OPENAI_API_KEY"))
        })
        .unwrap_or(false);
    if (has_chatgpt || has_new_format || has_api_key) && !is_our_relay_key {
        atomic_write_bytes(&official_auth_path(), &bytes)?;
    }
    Ok(())
}

/// 从 ~/.codex/auth.json 提取官方登录凭证 (ChatGPT OAuth 部分)
fn extract_official_credentials(auth: &Value) -> Value {
    auth.get("ChatGPT").cloned().unwrap_or(Value::Null)
}

/// 是否包含官方登录凭证: 旧 ChatGPT 对象, 或新版 auth_mode=chatgpt + tokens。
pub(crate) fn contains_official_credentials(auth: &Value) -> bool {
    auth.get("ChatGPT").is_some()
        || auth
            .as_object()
            .map(|o| {
                o.get("auth_mode").and_then(Value::as_str) == Some("chatgpt")
                    && o.get("tokens").map(|t| t.is_object()).unwrap_or(false)
            })
            .unwrap_or(false)
}

/// auth.json 归属标记: 我们写的中转 key 文件带此字段 (识别而非形状猜测)
const RELAY_AUTH_MARKER: &str = "codexff_relay_key";

/// seal: 官方凭证从 ~/.codex/auth.json 移入 vault (副本), 然后删除 auth.json。
/// 之后立刻用中转 key 重写 auth.json 的操作由调用方完成, 此处只保证官方凭证离场。
///
/// 备份规则: 官方登录 (ChatGPT) 或用户手动凭证 (非我们写的中转 key 形态) 一律
/// 整文件备份; 仅含 OPENAI_API_KEY 的 auth.json (我们写的中转 key) 不备份也不
/// 覆盖已存官方备份 — 防止 relay→relay 切换把官方凭证备份冲掉。
pub fn seal_official_auth() -> Result<bool, VaultError> {
    ensure_vault_dir()?;
    let auth_path = codex_config::codex_auth_path();
    if !auth_path.exists() {
        return Ok(false);
    }
    let bytes = fs::read(&auth_path)?;
    let parsed: Value = serde_json::from_slice(&bytes)?;
    let has_official = contains_official_credentials(&parsed);
    // 只有带归属标记的文件才确定是我们写的中转 key。无标记文件 (包括
    // 旧版单 key) 一律视为“可能重要”, 若 vault 还没有官方备份则整文件备份,
    // 避免把用户手动配置的官方 API Key 当成中转 key 静默删掉。
    let is_our_relay_key = parsed
        .as_object()
        .map(|o| o.get(RELAY_AUTH_MARKER).and_then(Value::as_bool) == Some(true))
        .unwrap_or(false);

    if has_official || !is_our_relay_key {
        if has_official || !official_auth_path().exists() {
            // 整文件备份 — 官方凭证 + 用户可能放的其他 key 一并保全
            atomic_write_bytes(&official_auth_path(), &bytes)?;
        }
    }
    // 物理移除 — 中转模式下官方凭证不存在于 auth.json
    fs::remove_file(&auth_path)?;
    Ok(has_official)
}

/// restore: vault 里的官方凭证写回 ~/.codex/auth.json
pub fn restore_official_auth() -> Result<bool, VaultError> {
    let path = official_auth_path();
    if !path.exists() {
        return Ok(false);
    }
    let bytes = fs::read(&path)?;
    atomic_write_bytes(&codex_config::codex_auth_path(), &bytes)?;
    Ok(true)
}

/// 切换前备份 ~/.codex/config.toml, 返回备份路径
pub fn backup_config() -> Result<Option<PathBuf>, VaultError> {
    let path = codex_config::codex_config_path();
    if !path.exists() {
        return Ok(None);
    }
    ensure_vault_dir()?;
    let backup = backup_dir().join("config.toml");
    atomic_write_bytes(&backup, &fs::read(&path)?)?;
    Ok(Some(backup))
}

/// 回滚 config.toml 到备份
pub fn restore_config_backup() -> Result<(), VaultError> {
    let backup = backup_dir().join("config.toml");
    if backup.exists() {
        let bytes = fs::read(&backup)?;
        atomic_write_bytes(&codex_config::codex_config_path(), &bytes)?;
    }
    Ok(())
}

// ---- 中转 key 存取: 默认 vault 文件 (0600), keyring 可选 ----
// GUI 环境下 SecItem 访问每次弹钥匙串授权框 (未签名 app macOS 不记住授权),
// 且弹窗在部分签名/系统组合下卡死 (采样确认: SecKeychainAddGenericPassword →
// CSSM_EncryptDataFinal, 即使不在主线程) — 自动流程 (余额查询) 也会触发,
// 体验灾难。因此默认全走 vault 文件: 与官方凭证同目录同权限 (0600/0700),
// 威胁模型一致 (官方凭证本就明文存 vault; cc-switch 更是明文 sqlite)。
// 设置 CODEXFF_KEYRING=1 可切回 keyring (优先, 超时/失败降级文件)。
const KEYRING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

fn keyring_enabled() -> bool {
    std::env::var("CODEXFF_KEYRING")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn keyring_timeout() -> std::time::Duration {
    // 测试用: 0ms = 强制走降级路径
    std::env::var("CODEXFF_KEYRING_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(KEYRING_TIMEOUT)
}

/// keyring 调用带超时: 返回 Ok(Some(v)) = keyring 成功;
/// Ok(None) = 超时 (调用方降级文件); Err = keyring 明确报错
fn keyring_call<T: Send + 'static>(
    op: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<Option<T>, VaultError> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(op());
    });
    match rx.recv_timeout(keyring_timeout()) {
        Ok(Ok(v)) => Ok(Some(v)),
        Ok(Err(e)) => Err(VaultError::Keyring(e)),
        Err(_) => Ok(None),
    }
}

fn relay_keys_path() -> PathBuf {
    vault_dir().join("relay-keys.json")
}

fn file_relay_keys() -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(relay_keys_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn file_set_key(profile_id: &str, key: &str) -> Result<(), VaultError> {
    ensure_vault_dir()?;
    let mut m = file_relay_keys();
    m.insert(profile_id.to_string(), key.to_string());
    atomic_write_bytes(
        &relay_keys_path(),
        serde_json::to_string_pretty(&m)?.as_bytes(),
    )
}

fn file_del_key(profile_id: &str) -> Result<(), VaultError> {
    let mut m = file_relay_keys();
    if m.remove(profile_id).is_some() {
        if m.is_empty() {
            let _ = std::fs::remove_file(relay_keys_path());
        } else {
            atomic_write_bytes(
                &relay_keys_path(),
                serde_json::to_string_pretty(&m)?.as_bytes(),
            )?;
        }
    }
    Ok(())
}

/// 中转 key 存取入口。默认 vault 文件 (无钥匙串弹窗);
/// CODEXFF_KEYRING=1 时 keyring 优先, 超时/报错降级文件。
pub fn set_relay_key(profile_id: &str, key: &str) -> Result<(), VaultError> {
    ensure_vault_dir()?;
    let pid = profile_id.to_string();
    if !keyring_enabled() {
        return file_set_key(&pid, key);
    }
    let key = key.to_string();
    let pid_kr = pid.clone();
    let key_kr = key.clone();
    match keyring_call(move || {
        let entry = keyring::Entry::new("com.codexff.vault", &format!("relay:{pid_kr}"))
            .map_err(|e| e.to_string())?;
        entry.set_password(&key_kr).map_err(|e| e.to_string())
    }) {
        Ok(Some(())) => Ok(()),
        _ => file_set_key(&pid, &key),
    }
}

pub fn get_relay_key(profile_id: &str) -> Result<Option<String>, VaultError> {
    let pid = profile_id.to_string();
    // 文件优先 — 无钥匙串弹窗 (默认完全不访问 keyring)
    if let Some(k) = file_relay_keys().get(&pid) {
        return Ok(Some(k.clone()));
    }
    // 仅显式开启 CODEXFF_KEYRING=1 时才读 keyring (兼容旧版本存储)
    if keyring_enabled() {
        let pid_kr = pid.clone();
        match keyring_call(move || {
            let entry = keyring::Entry::new("com.codexff.vault", &format!("relay:{pid_kr}"))
                .map_err(|e| e.to_string())?;
            match entry.get_password() {
                Ok(k) => Ok(Some(k)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(e.to_string()),
            }
        }) {
            Ok(Some(Some(k))) => {
                let _ = file_set_key(&pid, &k);
                Ok(Some(k))
            }
            _ => Ok(None),
        }
    } else {
        Ok(None)
    }
}

pub fn delete_relay_key(profile_id: &str) -> Result<(), VaultError> {
    let pid = profile_id.to_string();
    // 默认只删文件; 仅显式开启 CODEXFF_KEYRING=1 时同步删 keyring 残留
    if keyring_enabled() {
        let pid_kr = pid.clone();
        let _ = keyring_call(move || {
            let entry = keyring::Entry::new("com.codexff.vault", &format!("relay:{pid_kr}"))
                .map_err(|e| e.to_string())?;
            match entry.delete_credential() {
                Ok(()) => Ok(()),
                Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        });
    }
    file_del_key(&pid)
}

/// 写中转 auth.json。前置条件: 官方凭证已被 seal 移除。
///
/// 带归属标记 codexff_relay_key — seal 时凭标记识别"我们写的",
/// 不靠形状猜测 (形状猜测会把用户手动凭证当我们的删掉,
/// 或把带多余字段的中转文件备份去覆盖 vault 里的官方凭证)。
///
/// `custom_auth`: 用户自定义完整 auth.json 内容 (cc-switch 对齐)。
/// 内容在保存 profile 时已校验 (JSON 合法 + 不含官方 ChatGPT 凭证),
/// 此处只注入归属标记 (用户内容无标记时)。
pub fn write_relay_auth(profile_id: &str, custom_auth: Option<&str>) -> Result<(), VaultError> {
    let text = match custom_auth {
        Some(text) if !text.trim().is_empty() => {
            let mut v: serde_json::Value =
                serde_json::from_str(text).map_err(|e| VaultError::Json(e))?;
            // 隔离守卫: 中转 auth.json 不得包含官方凭证 (旧 ChatGPT 或新版 tokens)
            if contains_official_credentials(&v) {
                return Err(VaultError::Keyring(
                    "中转 auth.json 不得包含官方登录凭证".into(),
                ));
            }
            // 注入归属标记 (seal 识别用), 用户没写就补
            if let Some(obj) = v.as_object_mut() {
                obj.entry(RELAY_AUTH_MARKER)
                    .or_insert(serde_json::Value::Bool(true));
            }
            serde_json::to_string_pretty(&v).map_err(VaultError::Json)?
        }
        _ => {
            let Some(key) = get_relay_key(profile_id)? else {
                return Err(VaultError::Keyring(format!(
                    "profile {profile_id} 没有保存中转 key"
                )));
            };
            serde_json::json!({ "OPENAI_API_KEY": key, RELAY_AUTH_MARKER: true }).to_string()
        }
    };
    atomic_write_bytes(&codex_config::codex_auth_path(), text.as_bytes())
}
