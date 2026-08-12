//! 会话模型跟随 — 请求层默认适配当前供应商；用户也可以在会话管理中
//! 显式将单个旧会话重映射到当前模型，并保存原模型供切回时恢复。
//!
//! Codex 桌面端把每个会话的模型绑定在两个地方：
//! - `~/.codex/state_5.sqlite` 的 `threads.model` / `reasoning_effort`
//! - rollout JSONL 里的 `event_msg.payload.thread_settings_applied.thread_settings.model`
//!
//! 切换供应商时不再批量修改这些历史数据，避免大文件迁移、意外损坏和
//! 长时间等待。本模块的持久化改写仅供单会话显式操作。

use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session_manager::{self, SessionError};
use crate::vault;

const REMAP_FILENAME: &str = "session-model-remap.json";

/// 一次重映射的备份记录（原模型，切回时恢复）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemapRecord {
    pub thread_id: String,
    pub original_model: String,
    pub original_effort: Option<String>,
    pub remapped_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadModelInfo {
    pub thread_id: String,
    pub title: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub last_active_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelRemapPreview {
    pub threads: Vec<ThreadModelInfo>,
    pub target_model: String,
    pub target_effort: Option<String>,
    pub supported_models: Vec<String>,
    /// true = 拿不到目标供应商的模型清单，无法判断哪些会话不兼容
    pub models_unknown: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelRemapOutcome {
    pub remapped: usize,
    pub restored: usize,
    pub thread_ids: Vec<String>,
}

/// 迁移进度（前端展示 "迁移旧会话模型 (x/y) …"）。
#[derive(Debug, Clone, Serialize)]
pub struct RemapProgress {
    pub done: usize,
    pub total: usize,
    pub current: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelRemapError {
    #[error("{0}")]
    Blocked(String),
    #[error("{0}")]
    Other(String),
    #[error("会话错误: {0}")]
    Session(#[from] SessionError),
}

fn remap_file_path() -> PathBuf {
    vault::vault_dir().join(REMAP_FILENAME)
}

fn load_remaps() -> Vec<RemapRecord> {
    let path = remap_file_path();
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_remaps(records: &[RemapRecord]) -> Result<(), ModelRemapError> {
    let bytes =
        serde_json::to_vec_pretty(records).map_err(|e| ModelRemapError::Other(e.to_string()))?;
    vault::atomic_write_bytes(&remap_file_path(), &bytes)
        .map_err(|e| ModelRemapError::Other(format!("写入模型重映射备份失败: {e}")))
}

/// 官方模型清单（没有 CodexFF 目录时兜底用；不含自动评审线程）
pub const OFFICIAL_MODELS: [&str; 8] = [
    "gpt-5.6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.5",
    "gpt-5.2-codex",
    "gpt-5.2-codex-mini",
    "gpt-5.1-codex",
    "gpt-5-codex",
];

/// 读取 CodexFF 当前维护的模型目录 slug 列表（切换后 config 指向它）。
/// 目录缺失/损坏返回空。
pub fn list_catalog_slugs() -> Vec<String> {
    crate::workflow::list_catalog_models()
}

fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
    conn.prepare(&format!("PRAGMA table_info({table})"))
        .map(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(1))
                .map(|rows| rows.flatten().any(|c| c == column))
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

fn row_model_info(row: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadModelInfo> {
    Ok(ThreadModelInfo {
        thread_id: row.get(0)?,
        title: row.get(1)?,
        model: row.get(2)?,
        reasoning_effort: row.get(3)?,
        last_active_ms: row.get(4)?,
    })
}

/// 查询绑定模型不在 supported 列表里的线程（按最近活跃倒序）。
/// supported 为空 = 未知，返回空列表（无法判断）。
pub fn incompatible_threads(supported: &[String]) -> Result<Vec<ThreadModelInfo>, ModelRemapError> {
    if supported.is_empty() {
        return Ok(Vec::new());
    }
    let conn = session_manager::state_db_conn_rw()?;
    let has_effort = has_column(&conn, "threads", "reasoning_effort");
    let effort_sql = if has_effort {
        "reasoning_effort"
    } else {
        "NULL"
    };
    let placeholders = vec!["?"; supported.len()].join(",");
    let sql = format!(
        "SELECT id, title, model, {effort_sql}, COALESCE(updated_at_ms, updated_at * 1000) \
         FROM threads \
         WHERE model IS NOT NULL AND model != '' \
           AND model NOT IN ({placeholders}) \
           AND model != 'codex-auto-review' \
         ORDER BY COALESCE(updated_at_ms, updated_at * 1000) DESC"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| ModelRemapError::Other(format!("查询会话模型失败: {e}")))?;
    let params: Vec<&str> = supported.iter().map(|s| s.as_str()).collect();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), row_model_info)
        .map_err(|e| ModelRemapError::Other(format!("查询会话模型失败: {e}")))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| ModelRemapError::Other(format!("读取会话模型失败: {e}")))?);
    }
    Ok(out)
}

/// 快速判断是否存在目标供应商不支持的历史会话模型。
///
/// 自动兼容路由只需要布尔结果，不加载会话详情、不扫描 rollout。
/// 模型清单为空表示能力未知，为保证旧会话可续接按需要兼容处理。
pub fn has_incompatible_threads(supported: &[String]) -> Result<bool, ModelRemapError> {
    let conn = session_manager::state_db_conn_ro()?;
    if supported.is_empty() {
        return conn
            .query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM threads
                   WHERE model IS NOT NULL AND model != ''
                     AND model != 'codex-auto-review'
                   LIMIT 1
                 )",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|e| ModelRemapError::Other(format!("检查会话模型兼容性失败: {e}")));
    }
    let placeholders = vec!["?"; supported.len()].join(",");
    let sql = format!(
        "SELECT EXISTS(
           SELECT 1 FROM threads
           WHERE model IS NOT NULL AND model != ''
             AND model NOT IN ({placeholders})
             AND model != 'codex-auto-review'
           LIMIT 1
         )"
    );
    let params: Vec<&str> = supported.iter().map(|s| s.as_str()).collect();
    conn.query_row(&sql, rusqlite::params_from_iter(params), |row| {
        row.get::<_, bool>(0)
    })
    .map_err(|e| ModelRemapError::Other(format!("检查会话模型兼容性失败: {e}")))
}

/// 是否存在可续接的历史会话。跨第三方供应商时，即使模型名称相同，
/// Responses reasoning/tool schema 也可能不同，因此有历史会话就需要请求兼容层。
pub fn has_historical_threads() -> Result<bool, ModelRemapError> {
    let conn = session_manager::state_db_conn_ro()?;
    conn.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM threads
           WHERE model IS NULL OR model != 'codex-auto-review'
           LIMIT 1
         )",
        [],
        |row| row.get::<_, bool>(0),
    )
    .map_err(|e| ModelRemapError::Other(format!("检查历史会话失败: {e}")))
}

struct ThreadState {
    model: String,
    reasoning_effort: Option<String>,
    rollout_path: Option<String>,
}

fn load_thread_states(
    conn: &Connection,
    thread_ids: &[String],
) -> Result<Vec<(String, ThreadState)>, ModelRemapError> {
    if thread_ids.is_empty() {
        return Ok(Vec::new());
    }
    let has_effort = has_column(conn, "threads", "reasoning_effort");
    let effort_sql = if has_effort {
        "reasoning_effort"
    } else {
        "NULL"
    };
    let placeholders = vec!["?"; thread_ids.len()].join(",");
    let sql = format!(
        "SELECT id, model, {effort_sql}, rollout_path FROM threads WHERE id IN ({placeholders})"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| ModelRemapError::Other(format!("读取会话状态失败: {e}")))?;
    let params: Vec<&str> = thread_ids.iter().map(|s| s.as_str()).collect();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |row| {
            Ok((
                row.get::<_, String>(0)?,
                ThreadState {
                    model: row.get(1)?,
                    reasoning_effort: row.get(2)?,
                    rollout_path: row.get(3)?,
                },
            ))
        })
        .map_err(|e| ModelRemapError::Other(format!("读取会话状态失败: {e}")))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| ModelRemapError::Other(format!("读取会话状态失败: {e}")))?);
    }
    Ok(out)
}

/// 选择迁移后的思考档位：目标供应商给了就优先，否则用 high（兼容面最广）。
fn choose_effort(target: Option<&str>) -> Option<String> {
    let t = target.map(str::trim).filter(|s| !s.is_empty());
    match t {
        Some(e) => Some(e.to_string()),
        None => Some("high".to_string()),
    }
}

fn resolve_rollout_files(rollout_path: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (root, _archived) in [
        (session_manager::normal_root(false), false),
        (session_manager::normal_root(true), true),
        (session_manager::quarantine_root(false), false),
        (session_manager::quarantine_root(true), true),
    ] {
        let p = root.join(rollout_path);
        if p.is_file() {
            out.push(p);
        }
    }
    out
}

fn is_special_model(model: &str) -> bool {
    model == "codex-auto-review"
}

/// 递归修正 thread_settings 内的模型绑定。Codex 新版会把当前协作代理模型
/// 放在 collaboration_mode.settings 中，不能只改最外层 model。
fn rewrite_settings_models(
    value: &mut Value,
    new_model: &str,
    new_effort: Option<&str>,
    supported: &[String],
) -> bool {
    let mut changed = false;
    match value {
        Value::Object(obj) => {
            let replace_here = obj
                .get("model")
                .and_then(Value::as_str)
                .map(|model| {
                    !model.is_empty()
                        && !is_special_model(model)
                        && !supported.iter().any(|m| m == model)
                })
                .unwrap_or(false);
            if replace_here {
                obj.insert("model".into(), Value::String(new_model.to_string()));
                if let Some(effort) = new_effort {
                    obj.insert("reasoning_effort".into(), Value::String(effort.to_string()));
                } else {
                    obj.remove("reasoning_effort");
                }
                changed = true;
            }
            for child in obj.values_mut() {
                changed |= rewrite_settings_models(child, new_model, new_effort, supported);
            }
        }
        Value::Array(items) => {
            for child in items {
                changed |= rewrite_settings_models(child, new_model, new_effort, supported);
            }
        }
        _ => {}
    }
    changed
}

/// 把 rollout 里所有 `thread_settings_applied` 的不兼容模型绑定改为目标模型。
/// 流式改写（GB 级文件不会整体载入内存），失败时删除临时文件、原文件不动。
fn rewrite_rollout_models(
    path: &Path,
    new_model: &str,
    new_effort: Option<&str>,
    supported: &[String],
) -> Result<(), ModelRemapError> {
    if !path.is_file() {
        return Ok(());
    }
    let tmp = path.with_extension("jsonl.remap-tmp");
    let result = (|| -> Result<(), ModelRemapError> {
        let input = std::fs::File::open(path).map_err(SessionError::Io)?;
        let mut reader = BufReader::new(input);
        let output = std::fs::File::create(&tmp).map_err(SessionError::Io)?;
        let mut writer = BufWriter::new(output);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).map_err(SessionError::Io)?;
            if n == 0 {
                break;
            }
            let mut changed = line.clone();
            if let Ok(mut v) = serde_json::from_str::<Value>(line.trim()) {
                let is_settings = v.get("type").and_then(|t| t.as_str()) == Some("event_msg")
                    && v.get("payload")
                        .and_then(|p| p.get("type"))
                        .and_then(|t| t.as_str())
                        == Some("thread_settings_applied");
                if is_settings {
                    let settings = v
                        .get_mut("payload")
                        .and_then(|p| p.get_mut("thread_settings"))
                        .filter(|s| s.is_object());
                    if let Some(s) = settings {
                        if rewrite_settings_models(s, new_model, new_effort, supported) {
                            changed = serde_json::to_string(&v)
                                .map_err(|e| ModelRemapError::Other(e.to_string()))?;
                            changed.push('\n');
                        }
                    }
                }
            }
            writer
                .write_all(changed.as_bytes())
                .map_err(SessionError::Io)?;
        }
        writer.flush().map_err(SessionError::Io)?;
        writer
            .into_inner()
            .map_err(|e| SessionError::Io(e.into_error()))?
            .sync_all()
            .map_err(SessionError::Io)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            std::fs::rename(&tmp, path).map_err(SessionError::Io)?;
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 执行显式模型重映射（Codex 必须完全退出）。
///
/// `thread_ids`:
/// - `None` = 拒绝执行，禁止切换流程批量改写历史会话
/// - `Some(list)` = 只处理用户明确指定的线程
///
/// 每个线程：
/// - 若已有备份且原模型被目标供应商支持 → 恢复原模型（并删除备份）；
/// - 否则当前模型不支持时 → 改成 target_model，并备份原模型。
pub fn apply_remap(
    thread_ids: Option<&[String]>,
    target_model: &str,
    target_effort: Option<&str>,
    supported: &[String],
    progress: &(dyn Fn(RemapProgress) + Sync),
) -> Result<ModelRemapOutcome, ModelRemapError> {
    if target_model.trim().is_empty() {
        return Err(ModelRemapError::Blocked(
            "当前配置没有默认模型，无法迁移旧会话。请先在供应商里设置默认模型。".into(),
        ));
    }
    if session_manager::codex_running() {
        return Err(ModelRemapError::Blocked(
            "迁移旧会话模型需要修改 Codex 会话数据，请先完全退出 Codex / ChatGPT 桌面端与命令行。"
                .into(),
        ));
    }

    let mut conn = session_manager::state_db_conn_rw()?;
    let ids: Vec<String> = thread_ids
        .ok_or_else(|| {
            ModelRemapError::Blocked(
                "为保护历史会话，显式模型迁移入口已停用；请使用会话兼容路由接续。".into(),
            )
        })?
        .to_vec();
    if ids.is_empty() {
        return Ok(ModelRemapOutcome {
            remapped: 0,
            restored: 0,
            thread_ids: Vec::new(),
        });
    }

    let mut remaps = load_remaps();
    let mut remapped = 0usize;
    let mut restored = 0usize;
    let mut touched = Vec::new();
    let effort = choose_effort(target_effort);
    let mut file_jobs = Vec::new();

    {
        let tx = conn.transaction().map_err(SessionError::Sqlite)?;
        let has_effort = has_column(&tx, "threads", "reasoning_effort");
        let states = load_thread_states(&tx, &ids)?;
        for (thread_id, st) in states {
            let record_pos = remaps.iter().position(|r| r.thread_id == thread_id);
            let original_supported = record_pos
                .and_then(|i| remaps.get(i))
                .map(|r| supported.iter().any(|m| m == &r.original_model))
                .unwrap_or(false);

            if original_supported {
                // 切回支持原模型的供应商 → 恢复原模型
                let rec = remaps.remove(record_pos.unwrap());
                let new_model = rec.original_model.clone();
                let new_effort = rec.original_effort.clone();
                if has_effort {
                    tx.execute(
                        "UPDATE threads SET model=?1, reasoning_effort=?2 WHERE id=?3",
                        rusqlite::params![new_model, new_effort, thread_id],
                    )
                    .map_err(SessionError::Sqlite)?;
                } else {
                    tx.execute(
                        "UPDATE threads SET model=?1 WHERE id=?2",
                        rusqlite::params![new_model, thread_id],
                    )
                    .map_err(SessionError::Sqlite)?;
                }
                if let Some(rel) = &st.rollout_path {
                    for f in resolve_rollout_files(rel) {
                        file_jobs.push((f, new_model.clone(), new_effort.clone()));
                    }
                }
                restored += 1;
                touched.push(thread_id);
                continue;
            }

            if supported.iter().any(|m| m == &st.model) {
                // 已支持当前模型，无需迁移
                continue;
            }

            if record_pos.is_none() {
                remaps.push(RemapRecord {
                    thread_id: thread_id.clone(),
                    original_model: st.model.clone(),
                    original_effort: st.reasoning_effort.clone(),
                    remapped_at: Utc::now().to_rfc3339(),
                });
            }
            let new_model = target_model.to_string();
            let new_effort = effort.clone();
            if has_effort {
                tx.execute(
                    "UPDATE threads SET model=?1, reasoning_effort=?2 WHERE id=?3",
                    rusqlite::params![new_model, new_effort, thread_id],
                )
                .map_err(SessionError::Sqlite)?;
            } else {
                tx.execute(
                    "UPDATE threads SET model=?1 WHERE id=?2",
                    rusqlite::params![new_model, thread_id],
                )
                .map_err(SessionError::Sqlite)?;
            }
            if let Some(rel) = &st.rollout_path {
                for f in resolve_rollout_files(rel) {
                    file_jobs.push((f, new_model.clone(), new_effort.clone()));
                }
            }
            remapped += 1;
            touched.push(thread_id);
        }
        tx.commit().map_err(SessionError::Sqlite)?;
    }

    save_remaps(&remaps)?;
    // rollout 里的 thread_settings 是次要绑定（桌面端续聊以 state DB 为准）。
    // 改写失败只告警不阻断，避免单个文件异常导致整个切换回滚。
    // 去重后并发、单次流式处理。旧实现先预扫描再重写，命中文件会完整读取两遍。
    let mut seen: HashSet<PathBuf> = HashSet::new();
    file_jobs.retain(|(p, _, _)| seen.insert(p.clone()));
    let total = file_jobs.len();
    if total > 0 {
        let done = Arc::new(AtomicUsize::new(0));
        let queue = Arc::new(Mutex::new(VecDeque::from(file_jobs)));
        let workers = 4.min(total);
        thread::scope(|s| {
            for _ in 0..workers {
                let queue = Arc::clone(&queue);
                let done = Arc::clone(&done);
                s.spawn(move || loop {
                    let job = {
                        let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                        q.pop_front()
                    };
                    let Some((path, new_model, new_effort)) = job else {
                        break;
                    };
                    if let Err(e) =
                        rewrite_rollout_models(&path, &new_model, new_effort.as_deref(), supported)
                    {
                        log::warn!("改写会话模型绑定失败 {}: {e}", path.display());
                    }
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    progress(RemapProgress {
                        done: d,
                        total,
                        current: path.file_name().map(|n| n.to_string_lossy().into_owned()),
                    });
                });
            }
        });
    }
    Ok(ModelRemapOutcome {
        remapped,
        restored,
        thread_ids: touched,
    })
}

/// 清洗进度（前端展示 "清洗会话推理数据 (x/y) …"）。
#[derive(Debug, Clone, Serialize)]
pub struct SanitizeProgress {
    pub done: usize,
    pub total: usize,
    pub current: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SanitizeOutcome {
    pub sanitized_files: usize,
    pub sanitized_items: usize,
    /// Codex 正在运行时跳过（会话文件写入中，不能安全改写）
    pub skipped_running: bool,
    pub backups: Vec<String>,
}

/// reasoning 条目是否需要清洗: 官方 Responses schema 要求 reasoning 条目
/// 只要带 encrypted_content 字段（即使为 null）content 就必须是空数组。
/// Codex 写第三方供应商会话时会产生 content+encrypted_content:null 的组合，
/// 直通官方 Responses API 的中转（皮卡丘等）会 400（array_above_max_length）。
fn reasoning_content_needs_sanitize(v: &Value) -> bool {
    let Some(pl) = v.get("payload").and_then(|p| p.as_object()) else {
        return false;
    };
    if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
        return false;
    }
    if pl.get("type").and_then(|t| t.as_str()) != Some("reasoning") {
        return false;
    }
    if !pl.contains_key("encrypted_content") {
        return false;
    }
    pl.get("content")
        .and_then(|c| c.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false)
}

fn count_offending_reasoning(path: &Path) -> Result<usize, ModelRemapError> {
    let input = std::fs::File::open(path).map_err(SessionError::Io)?;
    let reader = BufReader::new(input);
    let mut n = 0usize;
    for line in reader.lines() {
        let line = line.map_err(SessionError::Io)?;
        if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
            if reasoning_content_needs_sanitize(&v) {
                n += 1;
            }
        }
    }
    Ok(n)
}

/// 收集线程对应的 rollout 文件（去重，普通 + 隔离区 + 归档一起）。
fn collect_rollout_files(
    conn: &Connection,
    thread_ids: Option<&[String]>,
) -> Result<Vec<PathBuf>, ModelRemapError> {
    let ids: Vec<String> = match thread_ids {
        Some(list) => list.to_vec(),
        None => {
            let mut stmt = conn
                .prepare("SELECT id FROM threads")
                .map_err(|e| ModelRemapError::Other(format!("读取会话列表失败: {e}")))?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|e| ModelRemapError::Other(format!("读取会话列表失败: {e}")))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| ModelRemapError::Other(format!("读取会话列表失败: {e}")))?);
            }
            out
        }
    };
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let states = load_thread_states(conn, &ids)?;
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut files: Vec<PathBuf> = Vec::new();
    for (_thread_id, st) in states {
        if let Some(rel) = &st.rollout_path {
            for f in resolve_rollout_files(rel) {
                if seen.insert(f.clone()) {
                    files.push(f);
                }
            }
        }
    }
    // 按最近修改倒序 — 活跃会话排最前, “是否还需要清洗”的判断能在
    // 毫秒级命中, 不会因为先扫几百个旧会话拖慢切换/自愈。
    files.sort_by(|a, b| {
        let ma = std::fs::metadata(a).and_then(|m| m.modified()).ok();
        let mb = std::fs::metadata(b).and_then(|m| m.modified()).ok();
        mb.cmp(&ma)
    });
    Ok(files)
}

/// 是否存在需要清洗的 reasoning 条目（切中转前快速判断）。
///
/// fail-open: 数据库被 Codex 占用/读取异常时按“需要清洗”处理, 让本地路由
/// 兜底清洗, 而不是静默跳过导致直连中转报错。
pub fn reasoning_sanitize_needed() -> bool {
    let Ok(conn) = session_manager::state_db_conn_ro() else {
        return true;
    };
    let Ok(files) = collect_rollout_files(&conn, None) else {
        return true;
    };
    files
        .iter()
        .any(|f| count_offending_reasoning(f).unwrap_or(1) > 0)
}

/// 流式改写 reasoning 条目的 content → []。失败时删除临时文件、原文件不动。
fn rewrite_reasoning_content(path: &Path) -> Result<(), ModelRemapError> {
    let tmp = path.with_extension("jsonl.reasoning-tmp");
    let result = (|| -> Result<(), ModelRemapError> {
        let input = std::fs::File::open(path).map_err(SessionError::Io)?;
        let mut reader = BufReader::new(input);
        let output = std::fs::File::create(&tmp).map_err(SessionError::Io)?;
        let mut writer = BufWriter::new(output);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).map_err(SessionError::Io)?;
            if n == 0 {
                break;
            }
            let mut changed = line.clone();
            if let Ok(mut v) = serde_json::from_str::<Value>(line.trim()) {
                if reasoning_content_needs_sanitize(&v) {
                    if let Some(pl) = v.get_mut("payload").and_then(|p| p.as_object_mut()) {
                        pl.insert("content".into(), serde_json::json!([]));
                        changed = serde_json::to_string(&v)
                            .map_err(|e| ModelRemapError::Other(e.to_string()))?;
                        changed.push('\n');
                    }
                }
            }
            writer
                .write_all(changed.as_bytes())
                .map_err(SessionError::Io)?;
        }
        writer.flush().map_err(SessionError::Io)?;
        writer
            .into_inner()
            .map_err(|e| SessionError::Io(e.into_error()))?
            .sync_all()
            .map_err(SessionError::Io)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            std::fs::rename(&tmp, path).map_err(SessionError::Io)?;
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 清洗所有（或指定）线程 rollout 里不合规的 reasoning 条目。
/// Codex 正在运行时跳过（返回 skipped_running=true），由本地路由在请求层兜底。
pub fn sanitize_reasoning_content(
    thread_ids: Option<&[String]>,
    progress: &dyn Fn(SanitizeProgress),
) -> Result<SanitizeOutcome, ModelRemapError> {
    let skipped_running = session_manager::codex_running();
    if skipped_running {
        log::warn!("清洗 reasoning 条目需修改 Codex 会话数据，Codex 正在运行，跳过");
        return Ok(SanitizeOutcome {
            sanitized_files: 0,
            sanitized_items: 0,
            skipped_running: true,
            backups: Vec::new(),
        });
    }
    let conn = session_manager::state_db_conn_rw()?;
    let files = collect_rollout_files(&conn, thread_ids)?;
    let total = files.len();
    let mut outcome = SanitizeOutcome {
        sanitized_files: 0,
        sanitized_items: 0,
        skipped_running: false,
        backups: Vec::new(),
    };
    for (i, f) in files.iter().enumerate() {
        progress(SanitizeProgress {
            done: i + 1,
            total,
            current: f.file_name().map(|n| n.to_string_lossy().into_owned()),
        });
        let count = match count_offending_reasoning(f) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("扫描会话推理数据失败 {}: {e}", f.display());
                continue;
            }
        };
        if count == 0 {
            continue;
        }
        let Some(backup) = crate::vault::backup_snapshot("session-reasoning", f) else {
            log::warn!("备份会话文件失败，跳过清洗 {}", f.display());
            continue;
        };
        if let Err(e) = rewrite_reasoning_content(f) {
            log::warn!("清洗会话推理数据失败 {}: {e}", f.display());
            continue;
        }
        outcome.sanitized_files += 1;
        outcome.sanitized_items += count;
        outcome.backups.push(backup.to_string_lossy().into_owned());
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_needs_sanitize_only_when_ec_present_with_content() {
        let ok_empty: Value = serde_json::from_str(
            r#"{"type":"response_item","payload":{"type":"reasoning","id":"r1","summary":[],"content":[],"encrypted_content":null}}"#,
        )
        .unwrap();
        assert!(!reasoning_content_needs_sanitize(&ok_empty));

        let bad: Value = serde_json::from_str(
            r#"{"type":"response_item","payload":{"type":"reasoning","id":"r1","summary":[],"content":[{"type":"reasoning_text","text":"x"}],"encrypted_content":null}}"#,
        )
        .unwrap();
        assert!(reasoning_content_needs_sanitize(&bad));

        let no_ec: Value = serde_json::from_str(
            r#"{"type":"response_item","payload":{"type":"reasoning","id":"r1","summary":[],"content":[{"type":"reasoning_text","text":"x"}]}}"#,
        )
        .unwrap();
        assert!(!reasoning_content_needs_sanitize(&no_ec));
    }

    #[test]
    fn rewrite_reasoning_empties_content_and_preserves_other_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("reasoning.jsonl");
        std::fs::write(
            &p,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s1\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\",\"id\":\"r1\",\"summary\":[],\"content\":[{\"type\":\"reasoning_text\",\"text\":\"x\"}],\"encrypted_content\":null}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]}}\n",
            ),
        )
        .unwrap();
        rewrite_reasoning_content(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("\"content\":[]"));
        assert!(text.contains("\"type\":\"reasoning\""));
        assert!(text.contains("\"type\":\"message\""));
        assert!(!text.contains("reasoning_text"));
    }

    #[test]
    fn rollout_settings_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rollout.jsonl");
        std::fs::write(
            &p,
            concat!(
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_settings_applied\",\"thread_settings\":{\"model\":\"gpt-5.6-luna\",\"reasoning_effort\":\"xhigh\"}}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_settings_applied\",\"thread_settings\":{\"model\":\"gpt-5.6-sol\"}}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hi\"}}\n",
            ),
        )
        .unwrap();
        rewrite_rollout_models(
            &p,
            "deepseek-v4-flash",
            Some("high"),
            &["deepseek-v4-flash".into(), "deepseek-v4-pro".into()],
        )
        .unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("\"model\":\"deepseek-v4-flash\""));
        assert!(text.contains("\"reasoning_effort\":\"high\""));
        assert!(!text.contains("gpt-5.6-luna"));
        assert!(!text.contains("gpt-5.6-sol"));
    }

    #[test]
    fn effort_fallback_is_high() {
        assert_eq!(choose_effort(None).as_deref(), Some("high"));
        assert_eq!(choose_effort(Some("max")).as_deref(), Some("max"));
        assert_eq!(choose_effort(Some("  ")).as_deref(), Some("high"));
    }

    #[test]
    fn rollout_nested_collaboration_model_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nested.jsonl");
        std::fs::write(
            &p,
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_settings_applied\",\"thread_settings\":{\"model\":\"gpt-5.5\",\"reasoning_effort\":\"high\",\"collaboration_mode\":{\"settings\":{\"model\":\"deepseek-v4-flash\",\"reasoning_effort\":\"low\"}},\"review\":{\"model\":\"codex-auto-review\"}}}}\n",
        )
        .unwrap();
        rewrite_rollout_models(
            &p,
            "gpt-5.6-luna",
            Some("max"),
            &["gpt-5.5".into(), "gpt-5.6-luna".into()],
        )
        .unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("\"model\":\"gpt-5.5\""));
        assert!(text.contains("\"model\":\"gpt-5.6-luna\""));
        assert!(text.contains("\"reasoning_effort\":\"max\""));
        assert!(text.contains("\"model\":\"codex-auto-review\""));
        assert!(!text.contains("deepseek-v4-flash"));
    }

    #[test]
    fn remap_file_roundtrip() {
        let rec = RemapRecord {
            thread_id: "t1".into(),
            original_model: "gpt-5.6-luna".into(),
            original_effort: Some("xhigh".into()),
            remapped_at: "now".into(),
        };
        // 直接测序列化/反序列化，不依赖真实 vault 目录
        let bytes = serde_json::to_vec(&vec![rec.clone()]).unwrap();
        let back: Vec<RemapRecord> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back[0].thread_id, rec.thread_id);
        assert_eq!(back[0].original_model, rec.original_model);
    }
}
