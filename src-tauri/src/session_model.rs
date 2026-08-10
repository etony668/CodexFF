//! 会话模型跟随 — 切换供应商时把旧会话绑定的模型重映射到目标供应商
//! 支持的模型，并保存原模型；切回支持原模型的供应商时自动恢复。
//!
//! Codex 桌面端把每个会话的模型绑定在两个地方：
//! - `~/.codex/state_5.sqlite` 的 `threads.model` / `reasoning_effort`
//! - rollout JSONL 里的 `event_msg.payload.thread_settings_applied.thread_settings.model`
//!
//! 切换供应商只改 config.toml 时，旧会话仍绑定旧模型 → 新供应商不支持就
//! 无法续聊。本模块负责把这两个绑定一起迁移，并持久化原模型用于切回恢复。

use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
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

/// 快速字节预扫描：文件里是否出现 `"model":"<old>"`（serde_json 紧凑格式）。
/// GB 级 JSONL 大部分其实不包含目标模型名，先做一次廉价扫描，命中才整份重写，
/// 避免每次切换都把所有会话文件读+写一遍（几十 GB 变成几秒）。
fn file_contains_model_binding(path: &Path, old_model: &str) -> std::io::Result<bool> {
    if old_model.is_empty() {
        return Ok(true);
    }
    let needle = format!("\"model\":\"{old_model}\"");
    let needle = needle.as_bytes();
    let mut file = std::fs::File::open(path)?;
    let mut buf = [0u8; 256 * 1024];
    let mut carry: Vec<u8> = Vec::with_capacity(needle.len() + 64);
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        carry.extend_from_slice(&buf[..n]);
        if carry.len() >= needle.len()
            && carry.windows(needle.len()).any(|w| w == needle)
        {
            return Ok(true);
        }
        // 保留末尾 needle.len()-1 字节，覆盖跨块边界
        let keep = needle.len().saturating_sub(1);
        if carry.len() > keep {
            carry.drain(..carry.len() - keep);
        }
    }
    Ok(false)
}

/// 把 rollout 里所有 `thread_settings_applied` 的模型绑定从 old 改为 new。
/// 流式改写（GB 级文件不会整体载入内存），失败时删除临时文件、原文件不动。
fn rewrite_rollout_models(
    path: &Path,
    old_model: &str,
    new_model: &str,
    new_effort: Option<&str>,
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
                        .and_then(|s| s.as_object_mut());
                    if let Some(s) = settings {
                        let m = s.get("model").and_then(|x| x.as_str()).unwrap_or("");
                        if m == old_model {
                            s.insert("model".to_string(), Value::String(new_model.to_string()));
                            if let Some(e) = new_effort {
                                s.insert(
                                    "reasoning_effort".to_string(),
                                    Value::String(e.to_string()),
                                );
                            } else if s.contains_key("reasoning_effort") {
                                s.remove("reasoning_effort");
                            }
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

/// 执行模型重映射（Codex 必须完全退出）。
///
/// `thread_ids`:
/// - `None` = 所有绑定模型不被目标供应商支持的线程
/// - `Some(list)` = 只处理指定线程
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
    let ids: Vec<String> = match thread_ids {
        Some(list) => list.to_vec(),
        None => incompatible_threads(supported)?
            .into_iter()
            .map(|t| t.thread_id)
            .collect(),
    };
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
                        file_jobs.push((
                            f,
                            st.model.clone(),
                            new_model.clone(),
                            new_effort.clone(),
                        ));
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
                    file_jobs.push((f, st.model.clone(), new_model.clone(), new_effort.clone()));
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
    // 去重后并发处理：预扫描跳过未命中的大文件，命中才整份流式重写。
    let mut seen: HashSet<(PathBuf, String)> = HashSet::new();
    file_jobs.retain(|(p, old, _, _)| seen.insert((p.clone(), old.clone())));
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
                    let Some((path, old_model, new_model, new_effort)) = job else {
                        break;
                    };
                    // 预扫描：模型绑定字符串不存在就直接跳过（大文件无谓读写的主因）
                    let matched = file_contains_model_binding(&path, &old_model)
                        .unwrap_or(true); // 读失败保守按命中走，交给原逻辑告警
                    if matched {
                        if let Err(e) =
                            rewrite_rollout_models(&path, &old_model, &new_model, new_effort.as_deref())
                        {
                            log::warn!("改写会话模型绑定失败 {}: {e}", path.display());
                        }
                    }
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    progress(RemapProgress {
                        done: d,
                        total,
                        current: path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned()),
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

/// 会话管理页“用当前模型续聊”：把单个线程重映射到当前配置默认模型。
pub fn remap_single_thread(
    thread_id: &str,
    target_model: &str,
    target_effort: Option<&str>,
    supported: &[String],
    progress: &(dyn Fn(RemapProgress) + Sync),
) -> Result<ModelRemapOutcome, ModelRemapError> {
    let id = thread_id.to_string();
    apply_remap(
        Some(std::slice::from_ref(&id)),
        target_model,
        target_effort,
        supported,
        progress,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
        rewrite_rollout_models(&p, "gpt-5.6-luna", "deepseek-v4-flash", Some("high")).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("\"model\":\"deepseek-v4-flash\""));
        assert!(text.contains("\"reasoning_effort\":\"high\""));
        assert!(text.contains("\"model\":\"gpt-5.6-sol\""));
        assert!(!text.contains("gpt-5.6-luna"));
    }

    #[test]
    fn effort_fallback_is_high() {
        assert_eq!(choose_effort(None).as_deref(), Some("high"));
        assert_eq!(choose_effort(Some("max")).as_deref(), Some("max"));
        assert_eq!(choose_effort(Some("  ")).as_deref(), Some("high"));
    }

    #[test]
    fn contains_model_binding_detects_hits_and_skips_misses() {
        let dir = tempfile::tempdir().unwrap();
        let hit = dir.path().join("hit.jsonl");
        let miss = dir.path().join("miss.jsonl");
        std::fs::write(&hit, "{\"payload\":{\"model\":\"gpt-5.6-luna\"}}\n").unwrap();
        std::fs::write(&miss, "{\"payload\":{\"model\":\"gpt-5.6-sol\"}}\n").unwrap();
        assert!(file_contains_model_binding(&hit, "gpt-5.6-luna").unwrap());
        assert!(!file_contains_model_binding(&miss, "gpt-5.6-luna").unwrap());
        assert!(file_contains_model_binding(&dir.path().join("nope.jsonl"), "x").is_err());

        // 跨块边界：needle 起点在 256KB 缓冲末尾，横跨下一个块
        let big = dir.path().join("big.jsonl");
        let prefix = "a".repeat((256 * 1024) - 10);
        std::fs::write(&big, format!("{prefix}\"model\":\"gpt-5.6-luna\"")).unwrap();
        assert!(file_contains_model_binding(&big, "gpt-5.6-luna").unwrap());
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
