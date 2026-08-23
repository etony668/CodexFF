//! 统一会话历史 — 仅在用户显式开启时把官方与第三方会话迁入共享
//! `model_provider = "custom"` 桶；关闭时按归属账本恢复原始 provider。
//! 迁移前自动备份 jsonl、state DB 与项目索引到金库，统一期间持续增量
//! 快照，任何失败都优先回滚而不覆盖最新会话内容。

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{backup::Backup, params_from_iter, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::codex_config;
use crate::session_manager;
use crate::vault;

const OPENAI_BUCKET: &str = "openai";
const SHARED_BUCKET: &str = "custom";
/// 超过该长度且不含 session_meta 的行直接流式写走, 防止 GB 级图片行占内存
const MAX_BUFFER_LINE: usize = 64 * 1024 * 1024;
const META_MARKER: &[u8] = b"\"session_meta\"";

#[derive(Debug, Clone, Serialize)]
pub struct UnifySessionMeta {
    /// rollout 文件 id (session_meta.payload.id)
    pub id: String,
    /// 线程 ID (session_meta.payload.session_id)
    pub thread_id: String,
    pub title: String,
    /// 相对 ~/.codex 的路径
    pub path: String,
    pub archived: bool,
    pub size: u64,
    pub last_active_ms: i64,
    /// 该线程包含的 rollout 文件数
    pub rollups: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UnifyOutcome {
    pub migrated_files: usize,
    pub migrated_rows: usize,
    pub thread_ids: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct UnifyLedger {
    timestamp: String,
    codex_config_dir: String,
    thread_ids: Vec<String>,
    session_ids: Vec<String>,
    files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UnifiedState {
    pub enabled: bool,
    pub generation: Option<String>,
    pub last_checkpoint_ms: i64,
    pub backed_up_threads: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Provenance {
    thread_id: String,
    session_id: String,
    path: String,
    original_provider: String,
    original_account: String,
    #[serde(default)]
    original_sha256: String,
    #[serde(default)]
    last_checkpoint_sha256: String,
    #[serde(default)]
    last_checkpoint_size: u64,
    #[serde(default)]
    last_checkpoint_mtime_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
struct FileFingerprint {
    size: u64,
    mtime_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UnifiedLedger {
    timestamp: String,
    codex_config_dir: String,
    current_account: String,
    files: Vec<Provenance>,
    #[serde(default)]
    state_hashes: HashMap<String, String>,
    #[serde(default)]
    state_fingerprints: HashMap<String, FileFingerprint>,
}

fn unify_backup_root() -> PathBuf {
    vault::vault_dir().join("session-unify-backup")
}

fn unified_state_path() -> PathBuf {
    vault::vault_dir().join("session-unify-state.json")
}

fn project_visibility_backup_path(provider: &str) -> PathBuf {
    vault::vault_dir()
        .join("project-visibility-backup")
        .join(format!("{provider}.json"))
}

#[derive(Debug, Clone)]
struct ProjectBinding {
    id: String,
    name: String,
    roots: Vec<String>,
    position: i64,
}

fn load_project_bindings() -> Vec<ProjectBinding> {
    let path = codex_config::codex_config_dir().join(".codex-global-state.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(root) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let Some(projects) = root.get("local-projects").and_then(Value::as_object) else {
        return Vec::new();
    };
    let order = root
        .get("project-order")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    projects
        .iter()
        .enumerate()
        .filter_map(|(index, (id, project))| {
            let name = project.get("name").and_then(Value::as_str)?.to_string();
            let roots = project
                .get("rootPaths")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|root| root.trim_end_matches('/').to_string())
                        .filter(|root| !root.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if roots.is_empty() {
                return None;
            }
            let position = order
                .iter()
                .position(|item| item == id)
                .map(|value| value as i64)
                .unwrap_or(index as i64);
            Some(ProjectBinding {
                id: id.clone(),
                name,
                roots,
                position,
            })
        })
        .collect()
}

fn cwd_matches_root(cwd: &str, root: &str) -> bool {
    cwd == root || cwd.starts_with(&format!("{root}/"))
}

fn project_id_for_thread(
    thread_id: &str,
    cwd: &str,
    assignments: &HashMap<String, String>,
    projects: &[ProjectBinding],
) -> Option<String> {
    if let Some(project_id) = assignments.get(thread_id) {
        if projects.iter().any(|project| project.id == *project_id) {
            return Some(project_id.clone());
        }
    }
    projects
        .iter()
        .find(|project| project.roots.iter().any(|root| cwd_matches_root(cwd, root)))
        .map(|project| project.id.clone())
}

/// 把 Codex 新版 SQLite 的 project_id 补齐到 global-state 的项目索引。
///
/// 新版桌面端仍从 global-state 渲染项目名称，但会话列表改为使用
/// `threads.project_id` 过滤。旧代码只维护 thread-project-assignments，
/// 导致项目名称存在而每个项目显示“暂无聊天”。该同步是幂等的，只更新
/// 能从线程归属或 cwd 明确推导出的项目，不删除任何线程。
pub fn sync_sqlite_project_bindings() -> Result<(), session_manager::SessionError> {
    let db_path = codex_config::codex_state_db_path();
    if !db_path.exists() {
        return Ok(());
    }
    let projects = load_project_bindings();
    if projects.is_empty() {
        return Ok(());
    }
    let config_path = codex_config::codex_config_dir().join(".codex-global-state.json");
    let assignments = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|root| {
            root.get("thread-project-assignments")
                .and_then(Value::as_object)
                .cloned()
        })
        .map(|items| {
            items
                .into_iter()
                .filter_map(|(thread_id, value)| {
                    value
                        .get("projectId")
                        .and_then(Value::as_str)
                        .map(|project_id| (thread_id, project_id.to_string()))
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();

    let mut conn = Connection::open(&db_path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let has_threads = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='threads'",
            [],
            |_| Ok(1),
        )
        .is_ok();
    if !has_threads {
        return Ok(());
    }
    let has_project_id = conn
        .prepare("PRAGMA table_info(threads)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .flatten()
        .any(|name| name == "project_id");
    if !has_project_id {
        return Ok(());
    }

    let tx = conn.transaction()?;
    let has_projects = tx
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='projects'",
            [],
            |_| Ok(1),
        )
        .is_ok();
    let now = chrono::Utc::now().timestamp_millis();
    if has_projects {
        for project in &projects {
            tx.execute(
                "INSERT INTO projects (id, name, metadata, position, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, '{}', ?3, ?4, ?4)
                 ON CONFLICT(id) DO UPDATE SET
                   name = excluded.name,
                   position = excluded.position,
                   updated_at_ms = excluded.updated_at_ms",
                rusqlite::params![project.id, project.name, project.position, now],
            )?;
        }
    }

    let mut updates = Vec::new();
    {
        let mut stmt = tx.prepare("SELECT id, cwd, project_id FROM threads")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows.flatten() {
            let Some(project_id) = project_id_for_thread(&row.0, &row.1, &assignments, &projects)
            else {
                continue;
            };
            if row.2.as_deref() != Some(project_id.as_str()) {
                updates.push((row.0, project_id));
            }
        }
    }
    for (thread_id, project_id) in &updates {
        tx.execute(
            "UPDATE threads SET project_id = ?1 WHERE id = ?2",
            rusqlite::params![project_id, thread_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectVisibilityBackup {
    version: u32,
    provider: String,
    created_at: String,
    #[serde(default)]
    source_sha256: String,
    /// 只保存本次 provider 清理时移除的索引项，而不是整份 global-state。
    /// 这样恢复时不会覆盖用户在当前 provider 下后来做出的修改。
    removed: Value,
}

pub fn state() -> UnifiedState {
    std::fs::read_to_string(unified_state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn provider_cwds(provider: &str) -> Result<Vec<String>, session_manager::SessionError> {
    let db = codex_config::codex_state_db_path();
    if !db.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let _ = conn.busy_timeout(Duration::from_secs(2));
    let has_threads = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='threads'",
            [],
            |_| Ok(1),
        )
        .is_ok();
    if !has_threads {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT DISTINCT cwd FROM threads
         WHERE model_provider = ?1 AND archived = 0 AND cwd IS NOT NULL AND cwd <> ''",
    )?;
    let rows = stmt.query_map([provider], |row| row.get::<_, String>(0))?;
    Ok(rows.filter_map(Result::ok).collect())
}

fn provider_thread_ids(
    provider: &str,
) -> Result<Option<HashSet<String>>, session_manager::SessionError> {
    let db = codex_config::codex_state_db_path();
    if !db.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let _ = conn.busy_timeout(Duration::from_secs(2));
    let has_threads = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='threads'",
            [],
            |_| Ok(1),
        )
        .is_ok();
    if !has_threads {
        return Ok(None);
    }
    let mut stmt = conn.prepare(
        "SELECT id FROM threads
         WHERE model_provider = ?1 AND archived = 0 AND id IS NOT NULL AND id <> ''",
    )?;
    let rows = stmt.query_map([provider], |row| row.get::<_, String>(0))?;
    Ok(Some(rows.filter_map(Result::ok).collect()))
}

fn project_has_visible_assignment(
    project_id: &str,
    project: &Value,
    visible_threads: &HashSet<String>,
    assignments: &serde_json::Map<String, Value>,
    cwds: &[String],
) -> bool {
    let assigned: Vec<(&String, &Value)> = assignments
        .iter()
        .filter(|(_, value)| value.get("projectId").and_then(Value::as_str) == Some(project_id))
        .collect();
    if !assigned.is_empty() {
        // Assignment 是比 cwd 更精确的归属来源；混合 provider 项目不会因共享 cwd
        // 被错误地完整显示到另一个 provider。
        return assigned
            .iter()
            .any(|(thread_id, _)| visible_threads.contains(thread_id.as_str()));
    }
    // 新 schema 里没有 assignment 的项目不能安全删除，避免用户手动创建但
    // 尚未产生线程的项目被误删。只有整个索引没有 assignment 时，才回退到 cwd。
    if assignments.is_empty() {
        project_has_visible_cwd(project, cwds)
    } else {
        true
    }
}

fn read_project_visibility_backup(
    provider: &str,
) -> Result<Option<ProjectVisibilityBackup>, session_manager::SessionError> {
    let path = project_visibility_backup_path(provider);
    if !path.exists() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    if let Ok(backup) = serde_json::from_value::<ProjectVisibilityBackup>(value.clone()) {
        return Ok(Some(backup));
    }
    // 兼容此前写入的整份 global-state 备份：转换成恢复所需的“移除项”形态。
    Ok(Some(ProjectVisibilityBackup {
        version: 1,
        provider: provider.to_string(),
        created_at: chrono::Local::now().to_rfc3339(),
        source_sha256: String::new(),
        removed: value,
    }))
}

fn merge_removed_value(existing: &mut Value, incoming: &Value) {
    let (Some(dst), Some(src)) = (existing.as_object_mut(), incoming.as_object()) else {
        return;
    };
    for (key, value) in src {
        match key.as_str() {
            "local-projects" | "thread-project-assignments" => {
                let target = dst
                    .entry(key.clone())
                    .or_insert_with(|| Value::Object(serde_json::Map::new()));
                if let (Some(target), Some(source)) = (target.as_object_mut(), value.as_object()) {
                    for (id, item) in source {
                        target.entry(id.clone()).or_insert_with(|| item.clone());
                    }
                }
            }
            "project-order" => {
                let target = dst
                    .entry(key.clone())
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let (Some(target), Some(source)) = (target.as_array_mut(), value.as_array()) {
                    for item in source {
                        if !target.contains(item) {
                            target.push(item.clone());
                        }
                    }
                }
            }
            _ => {
                dst.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
    }
}

fn save_project_visibility_backup(
    provider: &str,
    removed: Value,
    source_sha256: String,
) -> Result<(), session_manager::SessionError> {
    let is_empty = removed
        .as_object()
        .map(|object| object.is_empty())
        .unwrap_or(true);
    if is_empty {
        return Ok(());
    }
    if let Some(parent) = project_visibility_backup_path(provider).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut backup = read_project_visibility_backup(provider)?.unwrap_or(ProjectVisibilityBackup {
        version: 2,
        provider: provider.to_string(),
        created_at: chrono::Local::now().to_rfc3339(),
        source_sha256,
        removed: Value::Object(serde_json::Map::new()),
    });
    merge_removed_value(&mut backup.removed, &removed);
    vault::atomic_write_bytes(
        &project_visibility_backup_path(provider),
        &serde_json::to_vec_pretty(&backup)?,
    )
    .map_err(|e| {
        session_manager::SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("备份项目索引失败: {e}"),
        ))
    })?;
    Ok(())
}

fn project_has_visible_cwd(project: &Value, cwds: &[String]) -> bool {
    let Some(roots) = project.get("rootPaths").and_then(Value::as_array) else {
        return false;
    };
    roots.iter().any(|root| {
        let Some(root) = root.as_str() else {
            return false;
        };
        cwds.iter()
            .any(|cwd| cwd == root || cwd.starts_with(&format!("{root}/")))
    })
}

fn restore_project_visibility(
    provider: &str,
    root: &mut Value,
) -> Result<(), session_manager::SessionError> {
    let backup = project_visibility_backup_path(provider);
    if !backup.exists() {
        return Ok(());
    }
    let saved: Value = match read_project_visibility_backup(provider)? {
        Some(backup) => backup.removed,
        None => return Ok(()),
    };
    let Some(saved_obj) = saved.as_object() else {
        return Ok(());
    };
    let Some(obj) = root.as_object_mut() else {
        return Ok(());
    };

    for key in ["local-projects", "thread-project-assignments"] {
        let Some(source) = saved_obj.get(key).and_then(Value::as_object) else {
            continue;
        };
        let target = obj
            .entry(key.to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        let Some(target) = target.as_object_mut() else {
            continue;
        };
        for (id, value) in source {
            target.entry(id.clone()).or_insert_with(|| value.clone());
        }
    }
    if let Some(source) = saved_obj.get("project-order").and_then(Value::as_array) {
        let target = obj
            .entry("project-order".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(target) = target.as_array_mut() {
            for value in source {
                if !target.contains(value) {
                    target.push(value.clone());
                }
            }
        }
    }
    for (key, value) in saved_obj {
        if key.starts_with("sidebar-project-expanded-v1-codex:") && !obj.contains_key(key) {
            obj.insert(key.clone(), value.clone());
        }
    }
    if let Some(value) = saved_obj.get("selected-project") {
        let should_restore = obj
            .get("selected-project")
            .and_then(|v| v.get("projectId"))
            .and_then(Value::as_str)
            .is_none();
        if should_restore {
            obj.insert("selected-project".into(), value.clone());
        }
    }
    Ok(())
}

pub fn restore_project_visibility_for_provider(
    provider: &str,
) -> Result<(), session_manager::SessionError> {
    if provider != OPENAI_BUCKET && provider != SHARED_BUCKET {
        return Ok(());
    }
    let path = codex_config::codex_config_dir().join(".codex-global-state.json");
    if !path.exists() {
        return Ok(());
    }
    let mut root: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let before = root.clone();
    restore_project_visibility(provider, &mut root)?;
    if root != before {
        vault::atomic_write_bytes(&path, &serde_json::to_vec_pretty(&root)?).map_err(|e| {
            session_manager::SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("恢复项目索引失败: {e}"),
            ))
        })?;
    }
    Ok(())
}

/// 按目标 provider 清理 Codex 侧边栏项目索引。
/// 只移除没有该 provider 活跃线程的项目节点，并把清理前的完整 global-state
/// 保存到 vault；切换回该 provider 时先合并恢复，避免项目名称和线程归属丢失。
pub fn sync_project_visibility(provider: &str) -> Result<(), session_manager::SessionError> {
    if provider != OPENAI_BUCKET && provider != SHARED_BUCKET {
        return Ok(());
    }
    let Some(visible_threads) = provider_thread_ids(provider)? else {
        // 没有可靠线程归属时不猜测、不删除任何项目。
        return Ok(());
    };
    let cwds = provider_cwds(provider)?;
    let path = codex_config::codex_config_dir().join(".codex-global-state.json");
    if !path.exists() {
        return Ok(());
    }
    let source_sha256 = sha256_file(&path)?;
    let mut root: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    restore_project_visibility(provider, &mut root)?;
    let Some(obj) = root.as_object_mut() else {
        return Ok(());
    };
    let Some(projects_snapshot) = obj
        .get("local-projects")
        .and_then(Value::as_object)
        .cloned()
    else {
        return Ok(());
    };
    let assignments = obj
        .get("thread-project-assignments")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut removed_snapshot = serde_json::Map::new();
    let removed: HashSet<String> = {
        projects_snapshot
            .iter()
            .filter_map(|(id, project)| {
                (!project_has_visible_assignment(
                    id,
                    project,
                    &visible_threads,
                    &assignments,
                    &cwds,
                ))
                .then(|| id.clone())
            })
            .collect()
    };
    if removed.is_empty() {
        return Ok(());
    }
    let mut removed_projects = serde_json::Map::new();
    let Some(projects) = obj.get_mut("local-projects").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    for id in &removed {
        if let Some(value) = projects.remove(id) {
            removed_projects.insert(id.clone(), value);
        }
    }
    removed_snapshot.insert("local-projects".into(), Value::Object(removed_projects));
    if let Some(order) = obj.get_mut("project-order").and_then(Value::as_array_mut) {
        let mut removed_order = Vec::new();
        order.retain(|value| {
            let remove = value
                .as_str()
                .map(|id| removed.contains(id))
                .unwrap_or(false);
            if remove {
                removed_order.push(value.clone());
            }
            !remove
        });
        if !removed_order.is_empty() {
            removed_snapshot.insert("project-order".into(), Value::Array(removed_order));
        }
    }
    let expanded: Vec<String> = obj
        .keys()
        .filter(|key| {
            key.starts_with("sidebar-project-expanded-v1-codex:")
                && removed.iter().any(|id| key.contains(id))
        })
        .cloned()
        .collect();
    for key in expanded {
        if let Some(value) = obj.remove(&key) {
            removed_snapshot.insert(key, value);
        }
    }
    if let Some(assignments) = obj
        .get_mut("thread-project-assignments")
        .and_then(Value::as_object_mut)
    {
        let mut removed_assignments = serde_json::Map::new();
        assignments.retain(|thread_id, value| {
            let remove = value
                .get("projectId")
                .and_then(Value::as_str)
                .map(|id| removed.contains(id))
                .unwrap_or(false);
            if remove {
                removed_assignments.insert(thread_id.clone(), value.clone());
            }
            !remove
        });
        if !removed_assignments.is_empty() {
            removed_snapshot.insert(
                "thread-project-assignments".into(),
                Value::Object(removed_assignments),
            );
        }
    }
    if obj
        .get("selected-project")
        .and_then(|v| v.get("projectId"))
        .and_then(Value::as_str)
        .map(|id| removed.contains(id))
        .unwrap_or(false)
    {
        if let Some(value) = obj.remove("selected-project") {
            removed_snapshot.insert("selected-project".into(), value);
        }
    }
    save_project_visibility_backup(provider, Value::Object(removed_snapshot), source_sha256)?;
    vault::atomic_write_bytes(&path, &serde_json::to_vec_pretty(&root)?).map_err(|e| {
        session_manager::SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("写入项目索引失败: {e}"),
        ))
    })?;
    Ok(())
}

fn save_state(next: &UnifiedState) -> Result<(), session_manager::SessionError> {
    vault::atomic_write_bytes(&unified_state_path(), &serde_json::to_vec_pretty(next)?).map_err(
        |e| {
            session_manager::SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("写入统一状态失败: {e}"),
            ))
        },
    )
}

fn account_marker() -> String {
    crate::profiles::active_account_marker()
}

fn sha256_file(path: &Path) -> Result<String, session_manager::SessionError> {
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn file_fingerprint(path: &Path) -> Result<FileFingerprint, session_manager::SessionError> {
    let metadata = std::fs::metadata(path)?;
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    Ok(FileFingerprint {
        size: metadata.len(),
        mtime_ms,
    })
}

fn copy_snapshot_file(src: &Path, dst: &Path) -> Result<(), session_manager::SessionError> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dst.with_extension(format!("tmp-{}", std::process::id()));
    #[cfg(target_os = "macos")]
    {
        let src_c = CString::new(src.as_os_str().as_encoded_bytes()).map_err(|e| {
            session_manager::SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("备份源路径无效: {e}"),
            ))
        })?;
        let tmp_c = CString::new(tmp.as_os_str().as_encoded_bytes()).map_err(|e| {
            session_manager::SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("备份目标路径无效: {e}"),
            ))
        })?;
        // APFS 上 clonefile 是 Copy-on-Write，首次快照几乎只写元数据；
        // 不支持或跨文件系统时回退到普通 copy，不能因此阻断会话统一。
        let cloned = unsafe { clonefile(src_c.as_ptr(), tmp_c.as_ptr(), 0) } == 0;
        if !cloned {
            std::fs::copy(src, &tmp)?;
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::copy(src, &tmp)?;
    }
    std::fs::rename(tmp, dst)?;
    Ok(())
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn clonefile(
        src: *const std::os::raw::c_char,
        dst: *const std::os::raw::c_char,
        flags: u32,
    ) -> i32;
}

fn all_session_files() -> Vec<(PathBuf, bool)> {
    let paths = codex_config::codex_sessions_paths();
    let mut out = Vec::new();
    for (root, archived) in [(paths[0].clone(), false), (paths[1].clone(), true)] {
        if !root.exists() {
            continue;
        }
        let mut files = Vec::new();
        walk_jsonl(&root, &mut files);
        out.extend(files.into_iter().map(|p| (p, archived)));
    }
    out
}

fn current_ledger_path(generation: &str) -> PathBuf {
    unify_backup_root().join(generation).join("ledger.json")
}

fn load_current_ledger(generation: &str) -> Result<UnifiedLedger, session_manager::SessionError> {
    let text = std::fs::read_to_string(current_ledger_path(generation))?;
    Ok(serde_json::from_str(&text)?)
}

fn write_ledger(
    generation: &str,
    ledger: &UnifiedLedger,
) -> Result<(), session_manager::SessionError> {
    vault::atomic_write_bytes(
        &current_ledger_path(generation),
        &serde_json::to_vec_pretty(ledger)?,
    )
    .map_err(|e| {
        session_manager::SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("写入统一归属账本失败: {e}"),
        ))
    })
}

fn codex_dir() -> PathBuf {
    let paths = codex_config::codex_sessions_paths();
    paths[0]
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| paths[0].clone())
}

fn canonical_dir_string(dir: &Path) -> String {
    std::fs::canonicalize(dir)
        .unwrap_or_else(|_| dir.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn walk_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            walk_jsonl(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

/// 读取文件首部 session_meta: (session_id, thread_id, model_provider)
fn file_meta(path: &Path) -> Option<(String, String, String)> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    for _ in 0..20 {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if !line.contains("\"session_meta\"") || !line.contains("\"model_provider\"") {
            continue;
        }
        let v: Value = serde_json::from_str(line.trim()).ok()?;
        if v.get("type")?.as_str()? != "session_meta" {
            continue;
        }
        let payload = v.get("payload")?;
        let provider = payload.get("model_provider")?.as_str()?;
        let session_id = payload.get("id")?.as_str()?.to_string();
        let thread_id = payload
            .get("session_id")
            .and_then(|x| x.as_str())
            .or_else(|| payload.get("parent_thread_id").and_then(|x| x.as_str()))
            .unwrap_or(&session_id)
            .to_string();
        return Some((session_id, thread_id, provider.to_string()));
    }
    None
}

/// 扫描仍停留在 "openai" 桶的旧官方会话, 按线程合并。
pub fn scan_openai_sessions() -> Result<Vec<UnifySessionMeta>, session_manager::SessionError> {
    let titles = session_manager::load_thread_titles();
    let paths = codex_config::codex_sessions_paths();
    let mut found: Vec<UnifySessionMeta> = Vec::new();
    for (root, archived) in [(paths[0].clone(), false), (paths[1].clone(), true)] {
        if !root.exists() {
            continue;
        }
        let mut files = Vec::new();
        walk_jsonl(&root, &mut files);
        for path in files {
            let Some((session_id, thread_id, provider)) = file_meta(&path) else {
                continue;
            };
            if provider != OPENAI_BUCKET {
                continue;
            }
            let meta = std::fs::metadata(&path)?;
            let last_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let rel = path
                .strip_prefix(&codex_dir())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let title = session_manager::normalize_title(
                &titles
                    .get(&thread_id)
                    .cloned()
                    .unwrap_or_else(|| session_id.clone()),
            );
            found.push(UnifySessionMeta {
                title,
                id: session_id,
                thread_id,
                path: rel,
                archived,
                size: meta.len(),
                last_active_ms: last_ms,
                rollups: 1,
            });
        }
    }
    let mut grouped: HashMap<String, Vec<UnifySessionMeta>> = HashMap::new();
    for s in found {
        grouped.entry(s.thread_id.clone()).or_default().push(s);
    }
    let mut merged: Vec<UnifySessionMeta> = grouped
        .into_values()
        .map(|mut v| {
            v.sort_by(|a, b| b.last_active_ms.cmp(&a.last_active_ms));
            let mut meta = v.remove(0);
            meta.rollups = v.len() + 1;
            meta.size = v.iter().fold(meta.size, |acc, m| acc + m.size);
            meta
        })
        .collect();
    merged.sort_by(|a, b| b.last_active_ms.cmp(&a.last_active_ms));
    Ok(merged)
}

fn validate_thread_ids(ids: &[String]) -> Result<(), session_manager::SessionError> {
    if ids.iter().any(|id| {
        id.is_empty()
            || id.contains('/')
            || id.contains('\\')
            || id.contains("..")
            || id.contains(" ")
    }) {
        return Err(session_manager::SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "非法线程 ID",
        )));
    }
    Ok(())
}

fn relative_session_path(path: &Path) -> String {
    path.strip_prefix(&codex_dir())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn rewrite_all_provider(
    from: &str,
    to: &str,
    progress: &dyn Fn(&str),
) -> Result<usize, session_manager::SessionError> {
    let mut changed = 0;
    for (path, _) in all_session_files() {
        if rewrite_provider_in_place(&path, from, to)? {
            changed += 1;
        }
    }
    if changed > 0 {
        progress(&format!("已统一 {} 个会话文件的渠道桶…", changed));
    }
    Ok(changed)
}

/// 更新 rollout 首部的 provider 元数据而不重写整个 JSONL。
///
/// Codex 的 `session_meta` 位于 rollout 文件头部，且当前统一涉及的
/// `openai`/`custom` 桶长度相同。原位替换可以避免为数百 MB/GB 的
/// rollout 创建临时副本；会话正文完全不触碰，失败时调用方仍可回滚
/// SQLite/配置快照。
fn rewrite_provider_in_place(
    path: &Path,
    from: &str,
    to: &str,
) -> Result<bool, session_manager::SessionError> {
    if from.len() != to.len() {
        return Err(session_manager::SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "会话 provider 原位替换要求新旧值长度一致",
        )));
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let mut head = vec![0u8; 1024 * 1024];
    let n = file.read(&mut head)?;
    head.truncate(n);
    let marker = format!("\"model_provider\":\"{from}\"").into_bytes();
    let spaced_marker = format!("\"model_provider\": \"{from}\"").into_bytes();
    let offset = if let Some(offset) = find_subslice(&head, &marker) {
        offset + marker.len() - from.len() - 1
    } else if let Some(offset) = find_subslice(&head, &spaced_marker) {
        offset + spaced_marker.len() - from.len() - 1
    } else {
        return Ok(false);
    };
    use std::io::Seek;
    file.seek(std::io::SeekFrom::Start(offset as u64))?;
    file.write_all(to.as_bytes())?;
    file.sync_data()?;
    Ok(true)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn snapshot_generation(
    generation: &str,
    progress: &dyn Fn(&str),
) -> Result<UnifiedLedger, session_manager::SessionError> {
    let root = unify_backup_root().join(generation);
    let mut files = Vec::new();
    let account = account_marker();
    let session_files = all_session_files();
    let total = session_files.len();
    for (index, (path, _)) in session_files.into_iter().enumerate() {
        let Some((session_id, thread_id, provider)) = file_meta(&path) else {
            continue;
        };
        let rel = relative_session_path(&path);
        let dst = root.join("original").join(&rel);
        copy_snapshot_file(&path, &dst)?;
        let fingerprint = file_fingerprint(&path)?;
        files.push(Provenance {
            thread_id,
            session_id,
            path: rel,
            original_provider: provider,
            original_account: account.clone(),
            // APFS clonefile 已生成可独立恢复的写时复制快照。同步计算
            // 数十 GB rollout 的 SHA-256 只会阻塞开启流程，不增加恢复能力；
            // 文件变化由 size + mtime 指纹检测。
            original_sha256: String::new(),
            last_checkpoint_sha256: String::new(),
            last_checkpoint_size: fingerprint.size,
            last_checkpoint_mtime_ms: fingerprint.mtime_ms,
        });
        if (index + 1) % 100 == 0 || index + 1 == total {
            progress(&format!(
                "正在创建会话快照 {}/{}（不会复制会话正文）…",
                index + 1,
                total
            ));
        }
    }
    let state_hashes = HashMap::new();
    let mut state_fingerprints = HashMap::new();
    let db = codex_config::codex_state_db_path();
    if db.exists() {
        backup_state_db(&db, &root.join("original-state").join("state_5.sqlite"))?;
        state_fingerprints.insert("state_5.sqlite".to_string(), file_fingerprint(&db)?);
    }
    for (name, path) in [
        (
            "global-state.json",
            codex_config::codex_config_dir().join(".codex-global-state.json"),
        ),
        (
            "session_index.jsonl",
            codex_config::codex_config_dir().join("session_index.jsonl"),
        ),
    ] {
        if path.exists() {
            copy_snapshot_file(&path, &root.join("original-state").join(name))?;
            state_fingerprints.insert(name.to_string(), file_fingerprint(&path)?);
        }
    }
    progress(&format!("已备份 {} 个会话文件和索引…", files.len()));
    let ledger = UnifiedLedger {
        timestamp: chrono::Local::now().to_rfc3339(),
        codex_config_dir: canonical_dir_string(&codex_dir()),
        current_account: account,
        files,
        state_hashes,
        state_fingerprints,
    };
    write_ledger(generation, &ledger)?;
    Ok(ledger)
}

fn checkpoint_generation(
    generation: &str,
    progress: &dyn Fn(&str),
) -> Result<UnifiedState, session_manager::SessionError> {
    let mut ledger = load_current_ledger(generation)?;
    let root = unify_backup_root().join(generation);
    let known: HashSet<String> = ledger.files.iter().map(|f| f.path.clone()).collect();
    let mut added = 0;
    let mut changed = 0;
    for (path, _) in all_session_files() {
        let rel = relative_session_path(&path);
        let Some((session_id, thread_id, provider)) = file_meta(&path) else {
            continue;
        };
        let fingerprint = file_fingerprint(&path)?;
        // 新增文件只进入增量安全副本；原始归属标记为当前统一状态下的 custom，
        // 关闭统一时不会被错误改回 openai。
        if !known.contains(&rel) {
            let dst = root.join("incremental").join(&rel);
            copy_snapshot_file(&path, &dst)?;
            ledger.files.push(Provenance {
                thread_id,
                session_id,
                path: rel,
                original_provider: provider,
                original_account: account_marker(),
                original_sha256: String::new(),
                last_checkpoint_sha256: String::new(),
                last_checkpoint_size: fingerprint.size,
                last_checkpoint_mtime_ms: fingerprint.mtime_ms,
            });
            added += 1;
        } else if let Some(item) = ledger.files.iter_mut().find(|f| f.path == rel) {
            // 兼容 1.2.227 以前没有文件指纹的账本：先把当前元数据种入账本，
            // 不重新读取几十 GB 正文；从下一次 checkpoint 起按指纹检测变化。
            if item.last_checkpoint_size == 0 && item.last_checkpoint_mtime_ms == 0 {
                item.last_checkpoint_size = fingerprint.size;
                item.last_checkpoint_mtime_ms = fingerprint.mtime_ms;
                continue;
            }
            // 只在内容变化时刷新增量副本，不用旧快照覆盖统一期间的新内容。
            if item.last_checkpoint_size == fingerprint.size
                && item.last_checkpoint_mtime_ms == fingerprint.mtime_ms
            {
                continue;
            }
            let dst = root.join("incremental").join(&rel);
            copy_snapshot_file(&path, &dst)?;
            item.last_checkpoint_sha256.clear();
            changed += 1;
            item.last_checkpoint_size = fingerprint.size;
            item.last_checkpoint_mtime_ms = fingerprint.mtime_ms;
        }
    }
    for (name, path) in [
        ("state_5.sqlite", codex_config::codex_state_db_path()),
        (
            "global-state.json",
            codex_config::codex_config_dir().join(".codex-global-state.json"),
        ),
        (
            "session_index.jsonl",
            codex_config::codex_config_dir().join("session_index.jsonl"),
        ),
    ] {
        if !path.exists() {
            continue;
        }
        let fingerprint = file_fingerprint(&path)?;
        if !ledger.state_fingerprints.contains_key(name) {
            ledger
                .state_fingerprints
                .insert(name.to_string(), fingerprint);
            continue;
        }
        if ledger.state_fingerprints.get(name) == Some(&fingerprint) {
            continue;
        }
        if name == "state_5.sqlite" {
            backup_state_db(&path, &root.join("incremental-state").join(name))?;
        } else {
            copy_snapshot_file(&path, &root.join("incremental-state").join(name))?;
        }
        ledger.state_hashes.remove(name);
        ledger
            .state_fingerprints
            .insert(name.to_string(), fingerprint);
    }
    write_ledger(generation, &ledger)?;
    let mut next = state();
    next.enabled = true;
    next.generation = Some(generation.to_string());
    next.last_checkpoint_ms = chrono::Utc::now().timestamp_millis();
    next.backed_up_threads = ledger
        .files
        .iter()
        .map(|f| f.thread_id.clone())
        .collect::<HashSet<_>>()
        .len();
    next.error = None;
    if added > 0 || changed > 0 {
        progress(&format!(
            "已增量备份 {} 个新会话文件、{} 个变更会话文件…",
            added, changed
        ));
    }
    save_state(&next)?;
    Ok(next)
}

pub fn checkpoint_if_enabled() -> Result<(), session_manager::SessionError> {
    let current = state();
    if !current.enabled {
        return Ok(());
    }
    // 项目归属修复是轻量的、幂等的，即使会话备份被节流也要先执行，
    // 否则新版 Codex 会出现“项目名存在但项目内暂无聊天”。
    sync_sqlite_project_bindings()?;
    let Some(generation) = current.generation else {
        return Ok(());
    };
    let now = chrono::Utc::now().timestamp_millis();
    // 列表刷新、供应商切换和前端轮询可能在短时间内重复触发 checkpoint。
    // 首次开启仍做完整快照，后续 8 秒内跳过重复 hash/copy，避免磁盘抖动。
    if current.last_checkpoint_ms > 0 && now - current.last_checkpoint_ms < 8_000 {
        return Ok(());
    }
    checkpoint_generation(&generation, &|_| {}).map(|_| ())
}

pub fn set_enabled(
    enabled: bool,
    progress: &dyn Fn(&str),
) -> Result<UnifiedState, session_manager::SessionError> {
    let current = state();
    if current.enabled == enabled {
        if enabled {
            let Some(generation) = current.generation.as_deref() else {
                return Err(session_manager::SessionError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "统一状态缺少安全快照 generation；请关闭并重新开启会话统一",
                )));
            };
            return checkpoint_generation(generation, progress);
        }
        return Ok(current);
    }
    if session_manager::codex_running() {
        return Err(blocked());
    }
    if enabled {
        let generation = format!("unified-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
        progress("创建会话统一安全快照…");
        let ledger = snapshot_generation(&generation, progress)?;
        let result = (|| -> Result<UnifiedState, session_manager::SessionError> {
            rewrite_all_provider(OPENAI_BUCKET, SHARED_BUCKET, progress)?;
            let db = codex_config::codex_state_db_path();
            if db.exists() {
                update_state_db(
                    &db,
                    &ledger
                        .files
                        .iter()
                        .map(|f| f.thread_id.clone())
                        .collect::<Vec<_>>(),
                    OPENAI_BUCKET,
                    SHARED_BUCKET,
                )?;
            }
            // 配置桶与文件/SQLite 迁移在同一事务边界内提交；失败由外层回滚。
            codex_config::set_session_unify_provider(true).map_err(|e| {
                session_manager::SessionError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("切换统一会话配置桶失败: {e}"),
                ))
            })?;
            sync_sqlite_project_bindings()?;
            let mut next = UnifiedState {
                enabled: true,
                generation: Some(generation.clone()),
                last_checkpoint_ms: chrono::Utc::now().timestamp_millis(),
                backed_up_threads: ledger
                    .files
                    .iter()
                    .map(|f| f.thread_id.clone())
                    .collect::<HashSet<_>>()
                    .len(),
                error: None,
            };
            save_state(&next)?;
            next = checkpoint_generation(&generation, progress)?;
            Ok(next)
        })();
        match result {
            Ok(next) => Ok(next),
            Err(error) => {
                progress("统一开启失败，正在恢复原始会话与索引…");
                let _ = codex_config::set_session_unify_provider(false);
                let rollback = rollback_generation(&generation, &ledger);
                let _ = save_state(&UnifiedState {
                    enabled: false,
                    error: Some(format!("{error}")),
                    ..UnifiedState::default()
                });
                if let Err(rollback_error) = rollback {
                    return Err(session_manager::SessionError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("{error}; 自动回滚失败: {rollback_error}"),
                    )));
                }
                Err(error)
            }
        }
    } else {
        let Some(generation) = current.generation.clone() else {
            return Ok(UnifiedState::default());
        };
        progress("备份并校验当前会话状态…");
        // 先补齐新版 Codex 的项目归属，避免关闭过程较慢时官方侧边栏
        // 继续显示项目名但项目内没有会话。
        sync_sqlite_project_bindings()?;
        let _ = checkpoint_generation(&generation, progress)?;
        let ledger = load_current_ledger(&generation)?;
        let result = (|| -> Result<UnifiedState, session_manager::SessionError> {
            for (path, _) in all_session_files() {
                let rel = relative_session_path(&path);
                let Some(provenance) = ledger.files.iter().find(|f| f.path == rel) else {
                    continue;
                };
                // 统一开启期间正文可能达到数百 MB/GB；关闭时只需恢复
                // 首部 session_meta 的 provider，禁止再创建整文件临时副本。
                if provenance.original_provider.len() == SHARED_BUCKET.len() {
                    let _ = rewrite_provider_in_place(
                        &path,
                        SHARED_BUCKET,
                        &provenance.original_provider,
                    )?;
                } else {
                    // 兼容未来长度不同的 provider；该路径只应出现在
                    // 非标准桶，标准 openai/custom 永远走原位更新。
                    let rewrite = |line: &[u8]| {
                        rewrite_meta_bucket(
                            line,
                            &HashSet::from([provenance.session_id.clone()]),
                            false,
                            SHARED_BUCKET,
                            &provenance.original_provider,
                        )
                    };
                    let _ = rewrite_jsonl_file(&path, &rewrite)?;
                }
            }
            let db = codex_config::codex_state_db_path();
            if db.exists() {
                let mut by_provider: HashMap<String, Vec<String>> = HashMap::new();
                for file in &ledger.files {
                    by_provider
                        .entry(file.original_provider.clone())
                        .or_default()
                        .push(file.thread_id.clone());
                }
                for (provider, ids) in by_provider {
                    update_state_db(&db, &ids, SHARED_BUCKET, &provider)?;
                }
            }
            codex_config::set_session_unify_provider(false).map_err(|e| {
                session_manager::SessionError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("恢复官方会话配置桶失败: {e}"),
                ))
            })?;
            let target_provider = match codex_config::current_profile_kind() {
                Ok(codex_config::CurrentProfile::Official) => OPENAI_BUCKET,
                Ok(codex_config::CurrentProfile::Relay) => SHARED_BUCKET,
                _ => "",
            };
            if !target_provider.is_empty() {
                sync_project_visibility(target_provider)?;
            }
            sync_sqlite_project_bindings()?;
            let next = UnifiedState::default();
            save_state(&next)?;
            progress("会话统一已关闭，归属已恢复；最新会话内容保留在原文件中。");
            Ok(next)
        })();
        match result {
            Ok(next) => Ok(next),
            Err(error) => {
                progress("统一关闭失败，正在恢复统一前的最新会话快照…");
                let _ = codex_config::set_session_unify_provider(true);
                let rollback = restore_latest_generation_snapshot(&generation, &ledger);
                let _ = save_state(&current);
                if let Err(rollback_error) = rollback {
                    return Err(session_manager::SessionError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("{error}; 自动回滚失败: {rollback_error}"),
                    )));
                }
                Err(error)
            }
        }
    }
}

fn blocked() -> session_manager::SessionError {
    session_manager::SessionError::Io(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "请先完全退出 Codex / ChatGPT 桌面端与命令行后再迁移会话历史",
    ))
}

/// 改写 jsonl: 逐行流式处理, 命中重写规则的行替换, 其余原样写回。
/// 超大且非 session_meta 的行直接流式通过, 不整体占内存。
fn rewrite_jsonl_file(
    path: &Path,
    rewrite: &dyn Fn(&[u8]) -> Option<Vec<u8>>,
) -> Result<bool, session_manager::SessionError> {
    let parent = path.parent().ok_or_else(|| {
        session_manager::SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "会话文件缺少父目录",
        ))
    })?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "session".to_string());
    let tmp = parent.join(format!(".{name}.codexff-unify-{}", std::process::id()));
    let src = std::fs::File::open(path)?;
    let mut reader = BufReader::with_capacity(256 * 1024, src);
    let mut writer = BufWriter::new(std::fs::File::create(&tmp)?);
    let mut changed = false;
    let mut line: Vec<u8> = Vec::new();
    let mut buf = [0u8; 256 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let mut start = 0;
        for (i, b) in buf[..n].iter().enumerate() {
            if *b == b'\n' {
                line.extend_from_slice(&buf[start..=i]);
                changed |= write_line(&mut line, &mut writer, rewrite)?;
                line.clear();
                start = i + 1;
            }
        }
        if start < n {
            line.extend_from_slice(&buf[start..n]);
        }
        if line.len() > MAX_BUFFER_LINE && !line.windows(META_MARKER.len()).any(|w| w == META_MARKER)
        {
            writer.write_all(&line)?;
            line.clear();
        }
    }
    if !line.is_empty() {
        changed |= write_line(&mut line, &mut writer, rewrite)?;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    std::fs::rename(&tmp, path)?;
    Ok(changed)
}

fn write_line(
    line: &mut Vec<u8>,
    writer: &mut impl Write,
    rewrite: &dyn Fn(&[u8]) -> Option<Vec<u8>>,
) -> Result<bool, session_manager::SessionError> {
    if let Some(next) = rewrite(line) {
        writer.write_all(&next)?;
        Ok(true)
    } else {
        writer.write_all(line)?;
        Ok(false)
    }
}

fn rewrite_meta_bucket(
    line: &[u8],
    selected: &HashSet<String>,
    by_thread: bool,
    from: &str,
    to: &str,
) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(line).ok()?;
    if !text.contains("\"session_meta\"") || !text.contains("\"model_provider\"") {
        return None;
    }
    let trimmed = text.trim_end_matches(['\n', '\r']);
    let mut v: Value = serde_json::from_str(trimmed).ok()?;
    if v.get("type")?.as_str()? != "session_meta" {
        return None;
    }
    let payload = v.get_mut("payload")?.as_object_mut()?;
    let key = if by_thread {
        payload
            .get("session_id")
            .and_then(|x| x.as_str())
            .or_else(|| payload.get("parent_thread_id").and_then(|x| x.as_str()))
            .or_else(|| payload.get("id").and_then(|x| x.as_str()))
    } else {
        payload.get("id").and_then(|x| x.as_str())
    }?;
    if !selected.contains(key) {
        return None;
    }
    if payload.get("model_provider")?.as_str()? != from {
        return None;
    }
    payload.insert(
        "model_provider".to_string(),
        Value::String(to.to_string()),
    );
    let mut out = serde_json::to_string(&v).ok()?;
    out.push('\n');
    Some(out.into_bytes())
}

fn backup_state_db(
    db_path: &Path,
    backup_path: &Path,
) -> Result<(), session_manager::SessionError> {
    if let Some(parent) = backup_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut dst = Connection::open(backup_path)?;
    let src = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let backup = Backup::new(&src, &mut dst)?;
    backup.run_to_completion(5, Duration::from_millis(25), None)?;
    Ok(())
}

fn restore_state_db(
    backup_path: &Path,
    db_path: &Path,
) -> Result<(), session_manager::SessionError> {
    if !backup_path.exists() {
        return Ok(());
    }
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let src = Connection::open_with_flags(
        backup_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut dst = Connection::open(db_path)?;
    dst.busy_timeout(Duration::from_secs(5))?;
    let backup = Backup::new(&src, &mut dst)?;
    backup.run_to_completion(5, Duration::from_millis(25), None)?;
    Ok(())
}

/// 开启统一的任一步骤失败时恢复启用前的完整文件/索引快照。
/// Codex 在 set_enabled 中已被强制退出，因此用原始快照覆盖当前文件不会
/// 与正在写入的 rollout 竞争，也不会丢失用户在开启前的内容。
fn rollback_generation(
    generation: &str,
    ledger: &UnifiedLedger,
) -> Result<(), session_manager::SessionError> {
    let root = unify_backup_root().join(generation);
    for file in &ledger.files {
        let src = root.join("original").join(&file.path);
        if src.exists() {
            copy_snapshot_file(&src, &codex_dir().join(&file.path))?;
        }
    }
    restore_state_db(
        &root.join("original-state").join("state_5.sqlite"),
        &codex_config::codex_state_db_path(),
    )?;
    for (name, path) in [
        (
            "global-state.json",
            codex_config::codex_config_dir().join(".codex-global-state.json"),
        ),
        (
            "session_index.jsonl",
            codex_config::codex_config_dir().join("session_index.jsonl"),
        ),
    ] {
        let src = root.join("original-state").join(name);
        if src.exists() {
            copy_snapshot_file(&src, &path)?;
        }
    }
    let _ = std::fs::remove_file(unified_state_path());
    Ok(())
}

fn restore_latest_generation_snapshot(
    generation: &str,
    ledger: &UnifiedLedger,
) -> Result<(), session_manager::SessionError> {
    let root = unify_backup_root().join(generation);
    for file in &ledger.files {
        let incremental = root.join("incremental").join(&file.path);
        let original = root.join("original").join(&file.path);
        let source = if incremental.exists() {
            incremental
        } else {
            original
        };
        if source.exists() {
            copy_snapshot_file(&source, &codex_dir().join(&file.path))?;
        }
    }
    for (name, path) in [
        ("state_5.sqlite", codex_config::codex_state_db_path()),
        (
            "global-state.json",
            codex_config::codex_config_dir().join(".codex-global-state.json"),
        ),
        (
            "session_index.jsonl",
            codex_config::codex_config_dir().join("session_index.jsonl"),
        ),
    ] {
        let incremental = root.join("incremental-state").join(name);
        let original = root.join("original-state").join(name);
        let source = if incremental.exists() {
            incremental
        } else {
            original
        };
        if !source.exists() {
            continue;
        }
        if name == "state_5.sqlite" {
            restore_state_db(&source, &path)?;
        } else {
            copy_snapshot_file(&source, &path)?;
        }
    }
    Ok(())
}

fn update_state_db(
    db_path: &Path,
    ids: &[String],
    from: &str,
    to: &str,
) -> Result<usize, session_manager::SessionError> {
    if !db_path.exists() || ids.is_empty() {
        return Ok(0);
    }
    let mut conn = Connection::open(db_path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let has_threads = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='threads'",
            [],
            |_| Ok(1),
        )
        .is_ok();
    if !has_threads {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let mut changed = 0;
    for chunk in ids.chunks(500) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "UPDATE threads SET model_provider = ?1 WHERE model_provider = ?2 AND id IN ({placeholders})"
        );
        let mut values: Vec<String> = Vec::with_capacity(chunk.len() + 2);
        values.push(to.to_string());
        values.push(from.to_string());
        values.extend(chunk.iter().cloned());
        changed += tx.execute(&sql, params_from_iter(values.iter()))?;
    }
    tx.commit()?;
    Ok(changed)
}

fn collect_candidate_files(
    selected_threads: &HashSet<String>,
    provider: &str,
) -> Vec<(PathBuf, String, String)> {
    let paths = codex_config::codex_sessions_paths();
    let mut out = Vec::new();
    for root in [paths[0].clone(), paths[1].clone()] {
        if !root.exists() {
            continue;
        }
        let mut files = Vec::new();
        walk_jsonl(&root, &mut files);
        for path in files {
            let Some((session_id, thread_id, p)) = file_meta(&path) else {
                continue;
            };
            if p != provider || !selected_threads.contains(&thread_id) {
                continue;
            }
            let rel = path
                .strip_prefix(&codex_dir())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            out.push((path, rel, session_id));
        }
    }
    out
}

/// 迁移选中线程: "openai" → "custom", 迁移前备份 jsonl + state DB。
pub fn migrate_selected(
    thread_ids: &[String],
    progress: &dyn Fn(&str),
) -> Result<UnifyOutcome, session_manager::SessionError> {
    if thread_ids.is_empty() {
        return Ok(UnifyOutcome::default());
    }
    validate_thread_ids(thread_ids)?;
    if session_manager::codex_running() {
        return Err(blocked());
    }
    let selected: HashSet<String> = thread_ids.iter().cloned().collect();
    let files = collect_candidate_files(&selected, OPENAI_BUCKET);
    if files.is_empty() {
        return Ok(UnifyOutcome::default());
    }

    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let generation = unify_backup_root().join(format!("unify-{ts}"));
    let codex_dir_key = canonical_dir_string(&codex_dir());
    let mut session_ids = Vec::new();

    progress(&format!("准备迁移 {} 个会话文件…", files.len()));
    for (i, (path, rel, session_id)) in files.iter().enumerate() {
        progress(&format!("备份并迁移会话文件 {}/{}…", i + 1, files.len()));
        let backup_path = generation.join("jsonl").join(rel);
        if let Some(parent) = backup_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(path, &backup_path)?;
        let selected = selected.clone();
        let rewrite = |line: &[u8]| {
            rewrite_meta_bucket(line, &selected, true, OPENAI_BUCKET, SHARED_BUCKET)
        };
        rewrite_jsonl_file(path, &rewrite)?;
        session_ids.push(session_id.clone());
    }

    let db_path = codex_config::codex_state_db_path();
    let migrated_rows = if db_path.exists() {
        progress("备份并迁移会话索引…");
        backup_state_db(&db_path, &generation.join("state").join("state_5.sqlite"))?;
        update_state_db(&db_path, thread_ids, OPENAI_BUCKET, SHARED_BUCKET)?
    } else {
        0
    };

    let ledger = UnifyLedger {
        timestamp: ts,
        codex_config_dir: codex_dir_key,
        thread_ids: thread_ids.to_vec(),
        session_ids,
        files: files.iter().map(|(_, rel, _)| rel.clone()).collect(),
    };
    vault::atomic_write_bytes(
        &generation.join("ledger.json"),
        &serde_json::to_vec_pretty(&ledger)?,
    )
    .map_err(|e| {
        session_manager::SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("写入迁移账本失败: {e}"),
        ))
    })?;

    Ok(UnifyOutcome {
        migrated_files: files.len(),
        migrated_rows,
        thread_ids: thread_ids.to_vec(),
    })
}

fn load_ledgers() -> (Vec<String>, Vec<String>) {
    let mut thread_ids = Vec::new();
    let mut session_ids = Vec::new();
    let dir_key = canonical_dir_string(&codex_dir());
    let Ok(entries) = std::fs::read_dir(unify_backup_root()) else {
        return (thread_ids, session_ids);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let ledger_path = path.join("ledger.json");
        let Ok(text) = std::fs::read_to_string(&ledger_path) else {
            continue;
        };
        let Ok(ledger) = serde_json::from_str::<UnifyLedger>(&text) else {
            continue;
        };
        if ledger.codex_config_dir != dir_key {
            continue;
        }
        thread_ids.extend(ledger.thread_ids);
        session_ids.extend(ledger.session_ids);
    }
    thread_ids.sort();
    thread_ids.dedup();
    session_ids.sort();
    session_ids.dedup();
    (thread_ids, session_ids)
}

/// 是否存在可用于还原的迁移备份
pub fn has_backup() -> bool {
    let dir_key = canonical_dir_string(&codex_dir());
    let Ok(entries) = std::fs::read_dir(unify_backup_root()) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if !path.is_dir() {
            return false;
        }
        std::fs::read_to_string(path.join("ledger.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<UnifyLedger>(&text).ok())
            .map(|l| l.codex_config_dir == dir_key)
            .unwrap_or(false)
    })
}

/// 按备份账本精确还原: 只把账本内且当前为 "custom" 的会话翻回 "openai"。
/// 开启统一后新建的会话不在账本内, 永不触碰。
pub fn restore_from_backup(
    progress: &dyn Fn(&str),
) -> Result<UnifyOutcome, session_manager::SessionError> {
    if session_manager::codex_running() {
        return Err(blocked());
    }
    let (thread_ids, session_ids) = load_ledgers();
    if thread_ids.is_empty() && session_ids.is_empty() {
        return Ok(UnifyOutcome::default());
    }
    let selected_sessions: HashSet<String> = session_ids.iter().cloned().collect();
    let selected_threads: HashSet<String> = thread_ids.iter().cloned().collect();
    let files = collect_candidate_files(&selected_threads, SHARED_BUCKET);
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let restore_dir = unify_backup_root().join(format!("restore-{ts}"));
    let mut restored_files = 0;

    progress(&format!("准备还原 {} 个会话文件…", files.len()));
    for (i, (path, rel, _)) in files.iter().enumerate() {
        progress(&format!("备份并还原会话文件 {}/{}…", i + 1, files.len()));
        let backup_path = restore_dir.join("jsonl").join(rel);
        if let Some(parent) = backup_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(path, &backup_path)?;
        let selected = selected_sessions.clone();
        let rewrite = |line: &[u8]| {
            rewrite_meta_bucket(line, &selected, false, SHARED_BUCKET, OPENAI_BUCKET)
        };
        if rewrite_jsonl_file(path, &rewrite)? {
            restored_files += 1;
        }
    }

    let db_path = codex_config::codex_state_db_path();
    let restored_rows = if db_path.exists() {
        progress("备份并还原会话索引…");
        backup_state_db(&db_path, &restore_dir.join("state").join("state_5.sqlite"))?;
        update_state_db(&db_path, &thread_ids, SHARED_BUCKET, OPENAI_BUCKET)?
    } else {
        0
    };

    Ok(UnifyOutcome {
        migrated_files: restored_files,
        migrated_rows: restored_rows,
        thread_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_meta_line_changes_bucket() {
        let line = br#"{"timestamp":"t","type":"session_meta","payload":{"id":"s1","session_id":"t1","model_provider":"openai"}}
"#;
        let selected = HashSet::from(["t1".to_string()]);
        let out = rewrite_meta_bucket(&line[..], &selected, true, OPENAI_BUCKET, SHARED_BUCKET)
            .expect("should rewrite");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("\"model_provider\":\"custom\""));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn rewrite_meta_line_skips_non_selected_thread() {
        let line = br#"{"timestamp":"t","type":"session_meta","payload":{"id":"s1","session_id":"t1","model_provider":"openai"}}
"#;
        let selected = HashSet::from(["other".to_string()]);
        assert!(rewrite_meta_bucket(&line[..], &selected, true, OPENAI_BUCKET, SHARED_BUCKET).is_none());
    }

    #[test]
    fn rewrite_jsonl_streams_huge_lines() {
        let dir = std::env::temp_dir().join(format!(
            "codexff-unify-rewrite-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("rollout.jsonl");
        let meta =
            "{\"timestamp\":\"t\",\"type\":\"session_meta\",\"payload\":{\"id\":\"s1\",\"session_id\":\"t1\",\"model_provider\":\"openai\"}}\n";
        let huge = "x".repeat(70 * 1024 * 1024) + "\n";
        std::fs::write(&path, format!("{meta}{huge}")).expect("write file");

        let selected = HashSet::from(["t1".to_string()]);
        let rewrite = |line: &[u8]| {
            rewrite_meta_bucket(line, &selected, true, OPENAI_BUCKET, SHARED_BUCKET)
        };
        let changed = rewrite_jsonl_file(&path, &rewrite).expect("rewrite");
        assert!(changed);

        let text = std::fs::read_to_string(&path).expect("read back");
        let first_line = text.lines().next().expect("first line");
        let v: Value = serde_json::from_str(first_line).expect("valid json");
        assert_eq!(
            v["payload"]["model_provider"].as_str(),
            Some("custom")
        );
        assert_eq!(v["payload"]["session_id"].as_str(), Some("t1"));
        assert!(text.len() > 70 * 1024 * 1024);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn provider_rewrite_updates_rollout_header_in_place() {
        let dir =
            std::env::temp_dir().join(format!("codexff-provider-in-place-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("rollout.jsonl");
        let body = br#"{"type":"session_meta","payload":{"model_provider":"openai","id":"s1"}}
{"type":"response_item","payload":{"text":"body"}}
"#;
        std::fs::write(&path, body).expect("write file");

        assert!(rewrite_provider_in_place(&path, OPENAI_BUCKET, SHARED_BUCKET).unwrap());
        let updated = std::fs::read(&path).expect("read back");
        assert!(updated.starts_with(
            br#"{"type":"session_meta","payload":{"model_provider":"custom","id":"s1"}}"#
        ));
        assert!(updated.ends_with(
            br#"{"type":"response_item","payload":{"text":"body"}}
"#
        ));
        assert!(!rewrite_provider_in_place(&path, OPENAI_BUCKET, SHARED_BUCKET).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn state_db_bucket_update() {
        let path = std::env::temp_dir().join(format!(
            "codexff-unify-db-{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).expect("open db");
            conn.execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, model_provider TEXT); \
                 INSERT INTO threads VALUES ('t1','openai'),('t2','openai'),('t3','custom');",
            )
            .expect("seed db");
        }
        let n = update_state_db(
            &path,
            &["t1".to_string(), "t2".to_string()],
            OPENAI_BUCKET,
            SHARED_BUCKET,
        )
        .expect("update");
        assert_eq!(n, 2);
        let conn = Connection::open(&path).expect("reopen db");
        let custom: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE model_provider = 'custom'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(custom, 3);
        drop(conn);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn project_visibility_uses_path_boundaries() {
        let project = serde_json::json!({
            "rootPaths": ["/Users/test/DeepSeek Harness Glass"]
        });
        assert!(project_has_visible_cwd(
            &project,
            &["/Users/test/DeepSeek Harness Glass/src".to_string()]
        ));
        assert!(!project_has_visible_cwd(
            &project,
            &["/Users/test/DeepSeek Harness Glass-old".to_string()]
        ));
    }

    #[test]
    fn project_visibility_uses_thread_assignments_for_mixed_projects() {
        let project = serde_json::json!({
            "rootPaths": ["/Users/test/shared"]
        });
        let assignments = serde_json::json!({
            "thread-official": {"projectId": "p1"},
            "thread-relay": {"projectId": "p1"}
        });
        let assignments = assignments.as_object().expect("object");
        let official = HashSet::from(["thread-official".to_string()]);
        let relay = HashSet::from(["thread-relay".to_string()]);
        assert!(project_has_visible_assignment(
            "p1",
            &project,
            &official,
            assignments,
            &["/Users/test/shared".to_string()]
        ));
        assert!(project_has_visible_assignment(
            "p1",
            &project,
            &relay,
            assignments,
            &["/Users/test/shared".to_string()]
        ));
        assert!(!project_has_visible_assignment(
            "p1",
            &project,
            &HashSet::new(),
            assignments,
            &["/Users/test/shared".to_string()]
        ));
    }

    #[test]
    fn unassigned_project_is_preserved_when_assignments_are_available() {
        let project = serde_json::json!({"rootPaths": ["/Users/test/unknown"]});
        let assignments = serde_json::json!({
            "thread-1": {"projectId": "other"}
        });
        assert!(project_has_visible_assignment(
            "unassigned",
            &project,
            &HashSet::new(),
            assignments.as_object().expect("object"),
            &[]
        ));
    }

    #[test]
    fn project_visibility_backup_merge_keeps_first_snapshot() {
        let mut existing = serde_json::json!({
            "local-projects": {"p1": {"name": "original"}},
            "project-order": ["p1"]
        });
        let incoming = serde_json::json!({
            "local-projects": {
                "p1": {"name": "changed"},
                "p2": {"name": "new"}
            },
            "project-order": ["p2"]
        });
        merge_removed_value(&mut existing, &incoming);
        assert_eq!(existing["local-projects"]["p1"]["name"], "original");
        assert_eq!(existing["local-projects"]["p2"]["name"], "new");
        assert_eq!(existing["project-order"], serde_json::json!(["p1", "p2"]));
    }
}
