//! 凭证金库 — 官方凭证与中转 key 的物理隔离。
//!
//! 安全模型:
//! - 官方凭证只在官方 profile 激活时存在于 ~/.codex/auth.json
//! - 切到中转时, 官方凭证被 seal 进 vault 并从 auth.json 物理移除
//! - 所有凭证以 AES-256-GCM 加密后存 vault，主密钥只存系统安全凭据存储
//! - vault 目录权限 700
//!
//! 即使代理被绕过、config 被篡改, 中转模式下官方凭证也不在磁盘上。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
#[cfg(target_os = "windows")]
use std::time::{SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::codex_config;

pub const OFFICIAL_AUTH_FILENAME: &str = "official-auth.json";
pub const BACKUP_DIR_NAME: &str = "backups";
pub const RELAY_STATE_FILENAME: &str = "relay-state.json";
const OFFICIAL_ACCOUNTS_INDEX_SECRET: &str = "official-accounts-index";
const OFFICIAL_ACTIVE_ACCOUNT_SECRET: &str = "official-active-account";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfficialAccountInfo {
    pub id: String,
    pub account_id: Option<String>,
    pub label: String,
}

fn official_account_id(auth: &Value) -> String {
    let identity = auth
        .pointer("/tokens/account_id")
        .and_then(Value::as_str)
        .or_else(|| auth.pointer("/ChatGPT/account_id").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| serde_json::to_string(auth).unwrap_or_default());
    let mut digest = Sha256::new();
    digest.update(identity.as_bytes());
    let bytes = digest.finalize();
    bytes.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn official_account_label(auth: &Value, account_id: Option<&str>) -> String {
    let email = auth
        .pointer("/tokens/email")
        .and_then(Value::as_str)
        .or_else(|| auth.pointer("/ChatGPT/email").and_then(Value::as_str));
    if let Some(email) = email.filter(|v| !v.trim().is_empty()) {
        return mask_account_email(email);
    }
    account_id
        .map(|id| format!("官方账号 · {}", id.chars().take(8).collect::<String>()))
        .unwrap_or_else(|| "官方账号".to_string())
}

/// UI 只展示可识别但不可直接用于登录的账号标识，避免把 OAuth 邮箱完整暴露到前端。
fn mask_account_email(email: &str) -> String {
    let trimmed = email.trim();
    let Some((local, domain)) = trimmed.split_once('@') else {
        return trimmed.to_string();
    };
    if local.is_empty() || domain.is_empty() {
        return trimmed.to_string();
    }
    let mut chars = local.chars();
    let first = chars.next().unwrap_or('*');
    let second = chars.next();
    let prefix = match second {
        Some(value) => format!("{first}{value}"),
        None => first.to_string(),
    };
    format!("{prefix}***@{domain}")
}

fn load_official_account_index() -> Result<Vec<OfficialAccountInfo>, VaultError> {
    let Some(text) = get_secret(OFFICIAL_ACCOUNTS_INDEX_SECRET)? else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&text).map_err(VaultError::Json)
}

fn save_official_account_index(accounts: &[OfficialAccountInfo]) -> Result<(), VaultError> {
    set_secret(
        OFFICIAL_ACCOUNTS_INDEX_SECRET,
        &serde_json::to_string(accounts).map_err(VaultError::Json)?,
    )
}

fn official_account_secret(id: &str) -> String {
    format!("official-account:{id}")
}

/// 将现有单账号凭证迁移到多账号索引。凭证本体始终留在加密 vault。
pub fn migrate_official_accounts() -> Result<(), VaultError> {
    let mut accounts = load_official_account_index()?;
    if accounts.is_empty() {
        let Some(text) = get_secret("official-auth")? else {
            return Ok(());
        };
        let auth: Value = serde_json::from_str(&text)?;
        let account_id = auth
            .pointer("/tokens/account_id")
            .and_then(Value::as_str)
            .or_else(|| auth.pointer("/ChatGPT/account_id").and_then(Value::as_str))
            .map(str::to_string);
        let id = official_account_id(&auth);
        set_secret(&official_account_secret(&id), &text)?;
        accounts.push(OfficialAccountInfo {
            id: id.clone(),
            account_id,
            label: official_account_label(&auth, None),
        });
        save_official_account_index(&accounts)?;
        set_secret(OFFICIAL_ACTIVE_ACCOUNT_SECRET, &id)?;
    } else {
        // 旧版本索引可能保存了完整邮箱；启动时一次性归一化，之后前端只收到脱敏标签。
        let mut changed = false;
        for account in &mut accounts {
            let masked = account
                .label
                .split_once('@')
                .map(|_| mask_account_email(&account.label))
                .unwrap_or_else(|| account.label.clone());
            if masked != account.label {
                account.label = masked;
                changed = true;
            }
        }
        if changed {
            save_official_account_index(&accounts)?;
        }
    }
    Ok(())
}

pub fn list_official_accounts() -> Result<Vec<OfficialAccountInfo>, VaultError> {
    migrate_official_accounts()?;
    load_official_account_index()
}

pub fn active_official_account_id() -> Result<Option<String>, VaultError> {
    migrate_official_accounts()?;
    get_secret(OFFICIAL_ACTIVE_ACCOUNT_SECRET)
}

/// 将当前 auth.json 中的官方凭证保存为一个账号槽位并设为当前账号。
pub fn capture_official_account() -> Result<Option<OfficialAccountInfo>, VaultError> {
    let Some(auth) =
        codex_config::read_auth_json().map_err(|e| VaultError::Keyring(e.to_string()))?
    else {
        return Ok(None);
    };
    if !contains_official_credentials(&auth) {
        return Ok(None);
    }
    let auth = sanitize_official_auth(&auth)?;
    validate_official_auth_value(&auth)?;
    let account_id = auth
        .pointer("/tokens/account_id")
        .and_then(Value::as_str)
        .or_else(|| auth.pointer("/ChatGPT/account_id").and_then(Value::as_str))
        .map(str::to_string);
    let id = official_account_id(&auth);
    let info = OfficialAccountInfo {
        id: id.clone(),
        account_id: account_id.clone(),
        label: official_account_label(&auth, account_id.as_deref()),
    };
    set_secret(
        &official_account_secret(&id),
        &serde_json::to_string_pretty(&auth).map_err(VaultError::Json)?,
    )?;
    let mut accounts = load_official_account_index()?;
    accounts.retain(|item| item.id != id);
    accounts.push(info.clone());
    let index = serde_json::to_string(&accounts).map_err(VaultError::Json)?;
    let auth_text = serde_json::to_string_pretty(&auth).map_err(VaultError::Json)?;
    update_secrets(&[
        (&official_account_secret(&id), Some(auth_text.as_str())),
        (OFFICIAL_ACCOUNTS_INDEX_SECRET, Some(index.as_str())),
        (OFFICIAL_ACTIVE_ACCOUNT_SECRET, Some(id.as_str())),
        ("official-auth", Some(auth_text.as_str())),
    ])?;
    Ok(Some(info))
}

/// 只将指定账号的加密凭证恢复到 auth.json，调用方负责官方进程互斥与状态提交。
pub fn restore_official_account(id: &str) -> Result<OfficialAccountInfo, VaultError> {
    migrate_official_accounts()?;
    let accounts = load_official_account_index()?;
    let info = accounts
        .iter()
        .find(|item| item.id == id)
        .cloned()
        .ok_or_else(|| VaultError::Keyring("官方账号不存在".into()))?;
    let text = get_secret(&official_account_secret(id))?
        .ok_or_else(|| VaultError::Keyring("官方账号凭证不存在".into()))?;
    let auth: Value = serde_json::from_str(&text)?;
    validate_official_auth_value(&auth)?;
    let auth_path = codex_config::codex_auth_path();
    let previous_auth = if auth_path.exists() {
        Some(fs::read(&auth_path)?)
    } else {
        None
    };
    let rendered = serde_json::to_string_pretty(&auth).map_err(VaultError::Json)?;
    atomic_write_bytes(&auth_path, rendered.as_bytes())?;
    // 先确保 auth.json 可用，再原子地更新 vault 的“当前官方凭证 + 当前账号”。
    // 若 vault 更新失败，尽力还原之前 auth.json，避免 UI 显示 A 而磁盘实际留下 B。
    if let Err(error) = update_secrets(&[
        ("official-auth", Some(text.as_str())),
        (OFFICIAL_ACTIVE_ACCOUNT_SECRET, Some(id)),
    ]) {
        match previous_auth {
            Some(bytes) => {
                let _ = atomic_write_bytes(&auth_path, &bytes);
            }
            None => {
                let _ = fs::remove_file(&auth_path);
            }
        }
        return Err(error);
    }
    Ok(info)
}

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

/// vault 根目录 (~/Library/Application Support/codexff/vault 或 env CODEXFF_VAULT_DIR 覆盖)
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

/// 一次性启动迁移: 将旧的大写 `CodexFF` 用户数据目录合并到小写 `codexff`。
///
/// 默认 macOS APFS 不区分大小写, `codexff` 与 `CodexFF` 指向同一物理目录,
/// 此时无需迁移 (也绝不能尝试把目录移动到它自己身上)。仅在区分大小写的文件系统上,
/// 当旧的大写目录真实存在且与小写目录为不同物理路径时, 才把其下条目并入小写目录。
///
/// 幂等: 重复调用安全; 通过 env (CODEXFF_VAULT_DIR / CODEXFF_DATA_DIR) 覆盖路径时跳过。
pub fn migrate_legacy_data_dir_if_needed() {
    // env 覆盖场景下路径由测试/用户指定, 不做迁移
    if std::env::var("CODEXFF_VAULT_DIR").is_ok() || std::env::var("CODEXFF_DATA_DIR").is_ok() {
        return;
    }
    let data_dir = match dirs::data_dir() {
        Some(d) => d,
        None => {
            return;
        }
    };
    let legacy = data_dir.join("CodexFF");
    if !legacy.exists() {
        return;
    }
    let canonical = data_dir.join("codexff");
    // 区分大小写判定: 若两者 canonicalize 后相同, 物理为同一目录, 跳过移动自身
    if let (Ok(l), Ok(c)) = (legacy.canonicalize(), canonical.canonicalize()) {
        if l == c {
            return;
        }
    }
    if fs::create_dir_all(&canonical).is_err() {
        return;
    }
    if let Ok(entries) = fs::read_dir(&legacy) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let dst = canonical.join(&name);
            if dst.exists() {
                // 同名已存在则保留现有、不覆盖 (各子目录名本不冲突)
                continue;
            }
            let _ = fs::rename(entry.path(), &dst);
        }
    }
    // 旧目录已空则清理
    if let Ok(iter) = fs::read_dir(&legacy) {
        if iter.count() == 0 {
            let _ = fs::remove_dir(&legacy);
        }
    }
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
    get_secret("official-auth").ok().flatten().is_some() || official_auth_path().exists()
}

/// 官方模式下补捕获: codex login 后 vault 尚无官方凭证副本时, 从当前
/// auth.json 拷一份入 vault。原设计只在切中转 (seal) 时捕获 — 登录后
/// 一直停留官方模式则永不落库, Settings 永远显示"未保存"。
///
/// 幂等: vault 已有副本直接跳过; 仅捕获官方凭证形态 (旧 ChatGPT 对象 /
/// 新 tokens.auth_mode=chatgpt / 非空 OPENAI_API_KEY), 我们写的中转 key
/// 文件 (带归属标记) 不捕获 — 防止把中转 key 当官方凭证备份。
pub fn capture_official_if_missing() -> Result<(), VaultError> {
    migrate_legacy_official_auth()?;
    if get_secret("official-auth")?.is_some() {
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
        let official = sanitize_official_auth(&parsed)?;
        validate_official_auth_value(&official)?;
        set_secret("official-auth", &serde_json::to_string_pretty(&official)?)?;
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

fn is_relay_marked(auth: &Value) -> bool {
    auth.as_object()
        .and_then(|o| o.get(RELAY_AUTH_MARKER))
        .and_then(Value::as_bool)
        == Some(true)
}

/// 官方 OAuth 与中转字段曾被第三方工具写进同一个 auth.json 时，只保存
/// 官方认证所需字段。这样切回官方不会把 relay key/marker 一起恢复。
///
/// 没有 OAuth、也没有 CodexFF relay 标记的单独 OPENAI_API_KEY 仍按用户
/// 手动的官方 API key 保留；一旦存在 OAuth，则 API key 必须移除，避免
/// 无法判定归属的 key 与官方订阅同时在线。
fn sanitize_official_auth(auth: &Value) -> Result<Value, VaultError> {
    let Some(obj) = auth.as_object() else {
        return Err(VaultError::Keyring("auth.json 必须是 JSON 对象".into()));
    };
    if is_relay_marked(auth) && !contains_official_credentials(auth) {
        return Err(VaultError::Keyring("中转凭证不能保存为官方凭证".into()));
    }

    if contains_official_credentials(auth) {
        let mut official = serde_json::Map::new();
        for key in ["ChatGPT", "auth_mode", "tokens", "last_refresh"] {
            if let Some(value) = obj.get(key) {
                official.insert(key.to_string(), value.clone());
            }
        }
        let value = Value::Object(official);
        if !contains_official_credentials(&value) {
            return Err(VaultError::Keyring(
                "官方凭证清洗后缺少 ChatGPT 登录信息".into(),
            ));
        }
        Ok(value)
    } else {
        let mut value = auth.clone();
        if let Some(clean) = value.as_object_mut() {
            clean.remove(RELAY_AUTH_MARKER);
        }
        Ok(value)
    }
}

pub(crate) fn validate_official_auth_value(auth: &Value) -> Result<(), VaultError> {
    if is_relay_marked(auth) {
        return Err(VaultError::Keyring(
            "官方 auth.json 中仍有中转归属标记".into(),
        ));
    }
    if contains_official_credentials(auth) {
        let has_relay_key = auth
            .as_object()
            .map(|o| {
                [
                    "OPENAI_API_KEY",
                    "ANTHROPIC_API_KEY",
                    "ANTHROPIC_AUTH_TOKEN",
                ]
                .iter()
                .any(|key| {
                    o.get(*key)
                        .and_then(Value::as_str)
                        .is_some_and(|v| !v.is_empty())
                })
            })
            .unwrap_or(false);
        if has_relay_key {
            return Err(VaultError::Keyring(
                "官方 OAuth 与第三方 API key 不能同时存在".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_relay_auth_value(auth: &Value) -> Result<(), VaultError> {
    if contains_official_credentials(auth) {
        return Err(VaultError::Keyring(
            "中转 auth.json 中包含官方 ChatGPT 凭证".into(),
        ));
    }
    if !is_relay_marked(auth) {
        return Err(VaultError::Keyring(
            "中转 auth.json 缺少 CodexFF 归属标记".into(),
        ));
    }
    Ok(())
}

pub fn validate_official_auth_file() -> Result<(), VaultError> {
    let Some(auth) =
        codex_config::read_auth_json().map_err(|e| VaultError::Keyring(e.to_string()))?
    else {
        return Ok(());
    };
    validate_official_auth_value(&auth)
}

pub fn validate_relay_auth_file() -> Result<(), VaultError> {
    let auth = codex_config::read_auth_json()
        .map_err(|e| VaultError::Keyring(e.to_string()))?
        .ok_or_else(|| VaultError::Keyring("中转 auth.json 不存在".into()))?;
    validate_relay_auth_value(&auth)
}

/// seal: 官方凭证从 ~/.codex/auth.json 移入 vault (副本), 然后删除 auth.json。
/// 之后立刻用中转 key 重写 auth.json 的操作由调用方完成, 此处只保证官方凭证离场。
///
/// 备份规则: 官方登录 (ChatGPT) 或用户手动凭证 (非我们写的中转 key 形态) 一律
/// 整文件备份; 仅含 OPENAI_API_KEY 的 auth.json (我们写的中转 key) 不备份也不
/// 覆盖已存官方备份 — 防止 relay→relay 切换把官方凭证备份冲掉。
pub fn seal_official_auth() -> Result<bool, VaultError> {
    ensure_vault_dir()?;
    migrate_legacy_official_auth()?;
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
        if has_official || get_secret("official-auth")?.is_none() {
            let official = sanitize_official_auth(&parsed)?;
            validate_official_auth_value(&official)?;
            set_secret("official-auth", &serde_json::to_string_pretty(&official)?)?;
        }
    }
    // 物理移除 — 中转模式下官方凭证不存在于 auth.json
    fs::remove_file(&auth_path)?;
    Ok(has_official)
}

/// restore: vault 里的官方凭证写回 ~/.codex/auth.json
pub fn restore_official_auth() -> Result<bool, VaultError> {
    migrate_legacy_official_auth()?;
    let Some(text) = get_secret("official-auth")? else {
        return Ok(false);
    };
    let parsed: Value = serde_json::from_str(&text)?;
    let official = sanitize_official_auth(&parsed)?;
    validate_official_auth_value(&official)?;
    atomic_write_bytes(
        &codex_config::codex_auth_path(),
        serde_json::to_string_pretty(&official)?.as_bytes(),
    )?;
    validate_official_auth_file()?;
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

/// 会话/代理配置等大文件的快照备份: 复制到 vault/backups/{kind}-{ts}-{name}。
/// 备份失败返回 None（调用方按告警处理，不阻断主流程）。
pub fn backup_snapshot(kind: &str, path: &Path) -> Option<PathBuf> {
    ensure_vault_dir().ok()?;
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session.jsonl".into());
    let dest = backup_dir().join(format!("{kind}-{ts}-{name}"));
    fs::copy(path, &dest).ok()?;
    Some(dest)
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

// ---- 加密机密存储: AES-256-GCM 文件 + 钥匙串主密钥 ----
// 只访问一个钥匙串条目，避免每个供应商各弹一次授权。磁盘上的
// secrets.v1.json 永远只有密文；旧版明文 relay-keys.json / official-auth.json
// 首次读取后会迁移并删除。
// 首次访问或 App 被替换后，系统安全凭据存储可能显示授权窗口。3 秒会在用户
// 尚未来得及点击时误报 vault 读取失败；保留上限是为了避免系统服务永久挂起。
const KEYRING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const MASTER_SERVICE: &str = "com.codexff.vault.v2";
const MASTER_ACCOUNT: &str = "encryption-master-key";
static MASTER_KEY_CACHE: LazyLock<Mutex<Option<[u8; 32]>>> = LazyLock::new(|| Mutex::new(None));
// 单飞锁：启动阶段状态、许可证、供应商可能同时访问 vault。只允许一次
// Keychain 读取/授权，其余调用等待缓存，避免重复弹窗和竞态生成不同主密钥。
static MASTER_KEY_INIT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static TEST_SECRETS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EncryptedValue {
    nonce: String,
    ciphertext: String,
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

fn use_test_secret_store() -> bool {
    cfg!(debug_assertions)
        && std::env::var("CODEXFF_VAULT_DIR")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
}

fn secrets_path() -> PathBuf {
    vault_dir().join("secrets.v1.json")
}

/// Windows 早期构建没有启用原生凭据管理器，主密钥只存在于进程内 mock
/// 存储；重启后遗留的旧密文不可能被解密，但也绝不能静默覆盖。
///
/// 仅在 Windows 明确得到 `NoEntry` 后调用：将旧文件原样移到恢复目录，让当前
/// Windows 用户可以创建新的主密钥并继续导入热链。macOS 保持原有的严格阻断，
/// 避免钥匙串临时不可用时错误轮换密钥。
#[cfg(target_os = "windows")]
fn archive_orphaned_windows_secrets() -> Result<(), VaultError> {
    let source = secrets_path();
    if !source.exists() {
        return Ok(());
    }
    let recovery = vault_dir()
        .join("recovery-backups")
        .join("windows-orphaned-vault");
    fs::create_dir_all(&recovery)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let destination = recovery.join(format!("secrets.v1-{timestamp}.json"));
    fs::rename(&source, &destination).map_err(VaultError::Io)
}

fn master_key() -> Result<[u8; 32], VaultError> {
    if let Some(key) = *MASTER_KEY_CACHE.lock().unwrap_or_else(|e| e.into_inner()) {
        return Ok(key);
    }
    let _init = MASTER_KEY_INIT_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // 等锁期间另一个调用可能已完成初始化。
    if let Some(key) = *MASTER_KEY_CACHE.lock().unwrap_or_else(|e| e.into_inner()) {
        return Ok(key);
    }
    let existing = keyring_call(|| {
        let entry =
            keyring::Entry::new(MASTER_SERVICE, MASTER_ACCOUNT).map_err(|e| e.to_string())?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    })?
    .ok_or_else(|| {
        VaultError::Keyring("等待系统安全凭据存储授权超时，请允许 CodexFF 访问后重试".into())
    })?;
    let key = if let Some(encoded) = existing {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| VaultError::Keyring(format!("钥匙串主密钥损坏: {e}")))?;
        <[u8; 32]>::try_from(bytes.as_slice())
            .map_err(|_| VaultError::Keyring("钥匙串主密钥长度无效".into()))?
    } else {
        // 已有密文却没有原主密钥时绝不能生成新密钥，否则旧数据必然无法解密，
        // 且错误会被误认为“vault 损坏”。让用户先恢复钥匙串条目。
        if secrets_path().exists() {
            #[cfg(target_os = "windows")]
            {
                archive_orphaned_windows_secrets()?;
            }
            #[cfg(not(target_os = "windows"))]
            {
                return Err(VaultError::Keyring(
                    "系统安全凭据存储中缺少 CodexFF 主密钥，现有 vault 密文未被覆盖；请恢复原系统凭据或原 App 授权后重试"
                        .into(),
                ));
            }
        }
        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        let encoded = base64::engine::general_purpose::STANDARD.encode(key);
        let saved = keyring_call(move || {
            let entry =
                keyring::Entry::new(MASTER_SERVICE, MASTER_ACCOUNT).map_err(|e| e.to_string())?;
            entry.set_password(&encoded).map_err(|e| e.to_string())
        })?;
        if saved.is_none() {
            return Err(VaultError::Keyring(
                "等待系统安全凭据存储写入授权超时，请允许后重试".into(),
            ));
        }
        key
    };
    *MASTER_KEY_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some(key);
    Ok(key)
}

fn read_encrypted_values() -> Result<HashMap<String, EncryptedValue>, VaultError> {
    let path = secrets_path();
    if !path.exists() {
        return Ok(HashMap::new());
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn write_encrypted_values(values: &HashMap<String, EncryptedValue>) -> Result<(), VaultError> {
    ensure_vault_dir()?;
    if values.is_empty() {
        let _ = fs::remove_file(secrets_path());
        return Ok(());
    }
    atomic_write_bytes(
        &secrets_path(),
        serde_json::to_string_pretty(values)?.as_bytes(),
    )
}

fn encrypt_value(key: &[u8; 32], value: &str) -> Result<EncryptedValue, VaultError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| VaultError::Keyring(e.to_string()))?;
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), value.as_bytes())
        .map_err(|e| VaultError::Keyring(format!("机密加密失败: {e}")))?;
    Ok(EncryptedValue {
        nonce: base64::engine::general_purpose::STANDARD.encode(nonce),
        ciphertext: base64::engine::general_purpose::STANDARD.encode(ciphertext),
    })
}

/// 一次性更新一组机密。生产环境只写一次加密文件，避免多字段保存到一半
/// 失败后出现“API 报失败但金库已有部分新值”的撕裂状态。
fn update_secrets(updates: &[(&str, Option<&str>)]) -> Result<(), VaultError> {
    if use_test_secret_store() {
        let mut secrets = TEST_SECRETS.lock().unwrap_or_else(|e| e.into_inner());
        for (account, value) in updates {
            match value.map(str::trim).filter(|v| !v.is_empty()) {
                Some(value) => {
                    secrets.insert((*account).to_string(), value.to_string());
                }
                None => {
                    secrets.remove(*account);
                }
            }
        }
        return Ok(());
    }
    let key = master_key()?;
    let mut values = read_encrypted_values()?;
    for (account, value) in updates {
        match value.map(str::trim).filter(|v| !v.is_empty()) {
            Some(value) => {
                values.insert((*account).to_string(), encrypt_value(&key, value)?);
            }
            None => {
                values.remove(*account);
            }
        }
    }
    write_encrypted_values(&values)
}

pub fn set_secret(account: &str, value: &str) -> Result<(), VaultError> {
    update_secrets(&[(account, Some(value))])
}

pub fn get_secret(account: &str) -> Result<Option<String>, VaultError> {
    if use_test_secret_store() {
        return Ok(TEST_SECRETS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(account)
            .cloned());
    }
    let values = read_encrypted_values()?;
    let Some(value) = values.get(account) else {
        return Ok(None);
    };
    let nonce = base64::engine::general_purpose::STANDARD
        .decode(&value.nonce)
        .map_err(|e| VaultError::Keyring(format!("机密 nonce 损坏: {e}")))?;
    let nonce = <[u8; 12]>::try_from(nonce.as_slice())
        .map_err(|_| VaultError::Keyring("机密 nonce 长度无效".into()))?;
    let ciphertext = base64::engine::general_purpose::STANDARD
        .decode(&value.ciphertext)
        .map_err(|e| VaultError::Keyring(format!("机密密文损坏: {e}")))?;
    let key = master_key()?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| VaultError::Keyring(e.to_string()))?;
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|_| VaultError::Keyring("机密解密失败，钥匙串主密钥可能已变化".into()))?;
    String::from_utf8(plaintext)
        .map(Some)
        .map_err(|e| VaultError::Keyring(format!("机密文本编码无效: {e}")))
}

pub fn delete_secret(account: &str) -> Result<(), VaultError> {
    update_secrets(&[(account, None)])
}

pub fn set_profile_secret(
    profile_id: &str,
    kind: &str,
    value: Option<&str>,
) -> Result<(), VaultError> {
    let account = format!("relay:{profile_id}:{kind}");
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        Some(value) => set_secret(&account, value),
        None => delete_secret(&account),
    }
}

pub fn get_profile_secret(profile_id: &str, kind: &str) -> Result<Option<String>, VaultError> {
    get_secret(&format!("relay:{profile_id}:{kind}"))
}

pub fn set_profile_secrets(
    profile_id: &str,
    updates: &[(&str, Option<&str>)],
) -> Result<(), VaultError> {
    let accounts = updates
        .iter()
        .map(|(kind, value)| (format!("relay:{profile_id}:{kind}"), *value))
        .collect::<Vec<_>>();
    let refs = accounts
        .iter()
        .map(|(account, value)| (account.as_str(), *value))
        .collect::<Vec<_>>();
    update_secrets(&refs)?;
    if updates.iter().any(|(kind, _)| *kind == "key") {
        file_del_key(profile_id)?;
    }
    Ok(())
}

pub fn delete_profile_secrets(profile_id: &str) -> Result<(), VaultError> {
    let kinds = [
        "key",
        "auth-json",
        "usage-api-key",
        "usage-access-token",
        "config-toml",
        "usage-script",
    ];
    let accounts = kinds
        .iter()
        .map(|kind| (format!("relay:{profile_id}:{kind}"), None))
        .collect::<Vec<_>>();
    let refs = accounts
        .iter()
        .map(|(account, value)| (account.as_str(), *value))
        .collect::<Vec<_>>();
    update_secrets(&refs)?;
    Ok(())
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

pub fn set_relay_key(profile_id: &str, key: &str) -> Result<(), VaultError> {
    ensure_vault_dir()?;
    set_profile_secrets(profile_id, &[("key", Some(key))])
}

pub fn get_relay_key(profile_id: &str) -> Result<Option<String>, VaultError> {
    if let Some(key) = get_profile_secret(profile_id, "key")? {
        return Ok(Some(key));
    }
    let legacy = file_relay_keys().get(profile_id).cloned();
    if let Some(key) = legacy {
        set_profile_secret(profile_id, "key", Some(&key))?;
        file_del_key(profile_id)?;
        return Ok(Some(key));
    }
    Ok(None)
}

pub fn delete_relay_key(profile_id: &str) -> Result<(), VaultError> {
    delete_profile_secrets(profile_id)?;
    file_del_key(profile_id)
}

fn migrate_legacy_official_auth() -> Result<(), VaultError> {
    let path = official_auth_path();
    if !path.exists() {
        return Ok(());
    }
    if get_secret("official-auth")?.is_none() {
        let bytes = fs::read(&path)?;
        let parsed: Value = serde_json::from_slice(&bytes)?;
        let official = sanitize_official_auth(&parsed)?;
        validate_official_auth_value(&official)?;
        set_secret("official-auth", &serde_json::to_string_pretty(&official)?)?;
    }
    fs::remove_file(path)?;
    Ok(())
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
    let parsed: Value = serde_json::from_str(&text)?;
    validate_relay_auth_value(&parsed)?;
    atomic_write_bytes(&codex_config::codex_auth_path(), text.as_bytes())?;
    validate_relay_auth_file()
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    #[test]
    fn mixed_oauth_and_relay_fields_are_split_before_official_storage() {
        let mixed = serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": { "access_token": "oauth-token" },
            "OPENAI_API_KEY": "relay-key",
            RELAY_AUTH_MARKER: true,
            "unrelated": "drop-me"
        });
        let official = sanitize_official_auth(&mixed).expect("sanitize official auth");
        assert!(contains_official_credentials(&official));
        assert!(official.get("OPENAI_API_KEY").is_none());
        assert!(official.get(RELAY_AUTH_MARKER).is_none());
        assert!(official.get("unrelated").is_none());
        validate_official_auth_value(&official).expect("official credentials are isolated");
    }

    #[test]
    fn relay_validation_rejects_official_credentials_and_missing_marker() {
        let mixed = serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": { "access_token": "oauth-token" },
            "OPENAI_API_KEY": "relay-key",
            RELAY_AUTH_MARKER: true
        });
        assert!(validate_relay_auth_value(&mixed).is_err());
        assert!(validate_relay_auth_value(&serde_json::json!({
            "OPENAI_API_KEY": "relay-key"
        }))
        .is_err());
    }

    #[test]
    fn unmarked_api_key_only_auth_remains_valid_official_api_key() {
        let api_key = serde_json::json!({ "OPENAI_API_KEY": "user-official-key" });
        let official = sanitize_official_auth(&api_key).expect("sanitize api key auth");
        assert_eq!(official, api_key);
        validate_official_auth_value(&official).expect("standalone official api key accepted");
    }

    #[test]
    fn official_account_email_is_masked_without_losing_domain() {
        assert_eq!(mask_account_email("alice@example.com"), "al***@example.com");
        assert_eq!(mask_account_email("a@example.com"), "a***@example.com");
        assert_eq!(mask_account_email("not-an-email"), "not-an-email");
    }
}
