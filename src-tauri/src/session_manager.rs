//! 会话管理器 — 扫描 codex 本地会话 (sessions/ + archived_sessions/ + state DB)。
//!
//! 会话目录默认跨 profile 共享; 用户可标记“隔离”的会话, 在官方订阅激活时
//! 物理移入金库隔离区 (官方 CLI 扫不到 = 官方账号不可见), 切回中转时移回。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::codex_config::{self, CurrentProfile};
use crate::vault;

#[derive(Debug, Clone, Serialize)]
pub struct SessionMeta {
    pub id: String,
    /// session_meta 里的真实线程 ID (state DB 的键, 搜索/标题归属用)
    pub thread_id: String,
    pub provider: String,
    pub title: String,
    pub model: String,
    /// 最后活动时间 (unix ms)
    pub last_active_ms: i64,
    /// 会话文件相对路径 (sessions/xxx.jsonl)
    pub path: String,
    pub archived: bool,
    /// 已标记“官方订阅不可见”(文件当前在金库隔离区)
    pub isolated: bool,
    /// 首条用户消息摘要 (内容搜索/预览)
    pub preview: String,
    /// 该线程包含的 rollout 文件数 (续聊/子任务合并显示用)
    #[serde(default)]
    pub rollups: usize,
}

/// 隔离标记 (持久化在 vault/isolated-sessions.json)。
/// 按线程 ID 隔离: 该线程下所有 rollout 文件 (续聊/子任务) 一起移动。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsolatedItem {
    /// session_meta 里的真实线程 ID
    pub thread_id: String,
}

fn isolation_file_path() -> std::path::PathBuf {
    vault::vault_dir().join("isolated-sessions.json")
}

fn load_isolated() -> Vec<IsolatedItem> {
    let path = isolation_file_path();
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<IsolatedItem>>(&t).ok())
        .unwrap_or_default()
}

fn save_isolated(items: &[IsolatedItem]) -> Result<(), SessionError> {
    let bytes = serde_json::to_vec_pretty(items)?;
    vault::atomic_write_bytes(&isolation_file_path(), &bytes).map_err(|e| {
        SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("写入隔离标记失败: {e}"),
        ))
    })?;
    Ok(())
}

/// 隔离同步诊断日志 (排查"切换后官方仍能看到会话"问题时使用)
fn isolation_log(msg: &str) {
    let path = vault::vault_dir().join("isolation-sync.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let _ = writeln!(f, "[{ts}] {msg}");
    }
}

pub(crate) fn normal_root(archived: bool) -> std::path::PathBuf {
    let paths = codex_config::codex_sessions_paths();
    if archived {
        paths[1].clone()
    } else {
        paths[0].clone()
    }
}

pub(crate) fn quarantine_root(archived: bool) -> std::path::PathBuf {
    vault::vault_dir().join("session-quarantine").join(if archived {
        "archived_sessions"
    } else {
        "sessions"
    })
}

/// 移动会话文件 — 安全迁移:
/// 1. 同卷直接 rename (原子, 大文件瞬时完成, 不会损坏);
/// 2. 跨卷先复制到目标目录临时文件 + fsync, 再原子改名 (中断不会留下半个目标文件);
/// 3. 落盘后校验目标大小与源一致;
/// 4. 成功后清理空 rollup 目录。
fn move_file_safe(
    src: &std::path::Path,
    dst: &std::path::Path,
    progress: &dyn Fn(&str),
) -> Result<bool, SessionError> {
    if !src.exists() {
        return Ok(false);
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let src_len = std::fs::metadata(src)?.len();
    match std::fs::rename(src, dst) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            // 金库在其它磁盘/卷: 走临时文件复制, 避免大文件复制中断留半个目标
            copy_file_safe(src, dst, src_len, progress)?;
            std::fs::remove_file(src)?;
        }
        Err(e) => return Err(SessionError::Io(e)),
    }
    let dst_len = std::fs::metadata(dst)?.len();
    if dst_len != src_len {
        return Err(SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "会话文件迁移校验失败: {} ({dst_len}/{} bytes)",
                dst.display(),
                src_len
            ),
        )));
    }
    if let Some(parent) = src.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(true)
}

/// 跨卷复制: 写到目标目录内的临时文件, fsync 后原子改名; 失败清理临时文件。
fn copy_file_safe(
    src: &std::path::Path,
    dst: &std::path::Path,
    src_len: u64,
    progress: &dyn Fn(&str),
) -> Result<(), SessionError> {
    let parent = dst.parent().ok_or_else(|| {
        SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "目标路径缺少父目录",
        ))
    })?;
    std::fs::create_dir_all(parent)?;
    let name = dst
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "session".to_string());
    let tmp = parent.join(format!(
        ".{name}.codexff-tmp-{}",
        std::process::id()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut input = std::fs::File::open(src)?;
        let mut output = std::fs::File::create(&tmp)?;
        let mut buf = vec![0u8; 8 * 1024 * 1024];
        let mut copied: u64 = 0;
        let mut last_pct: u64 = 0;
        let total_mb = src_len as f64 / (1024.0 * 1024.0);
        progress(&format!(
            "复制会话文件 {name} (0 MB / {:.1} MB)…",
            total_mb
        ));
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            output.write_all(&buf[..n])?;
            copied += n as u64;
            if src_len > 0 {
                let pct = copied * 100 / src_len;
                if pct >= last_pct + 5 || copied >= src_len {
                    last_pct = pct;
                    progress(&format!(
                        "复制会话文件 {name} ({:.1} MB / {:.1} MB)…",
                        copied as f64 / (1024.0 * 1024.0),
                        total_mb
                    ));
                }
            }
        }
        output.sync_all()?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(SessionError::Io(e));
    }
    std::fs::rename(&tmp, dst)?;
    Ok(())
}

/// 轻量读取文件的线程 ID (session_meta 的 session_id/id)
fn file_thread_id(path: &std::path::Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    for _ in 0..20 {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        for key in ["session_id", "id"] {
            if let Some(s) = v
                .get("payload")
                .and_then(|p| p.get(key))
                .and_then(|t| t.as_str())
            {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

/// 找到线程 ID 对应的所有文件 (正常目录 + 金库隔离区), 返回 (源, 目标, 是否在正常目录)
fn thread_files(thread_id: &str) -> Vec<(std::path::PathBuf, std::path::PathBuf, bool)> {
    let mut out = Vec::new();
    for (normal, archived) in [
        (normal_root(false), false),
        (normal_root(true), true),
    ] {
        if !normal.exists() {
            continue;
        }
        let q = quarantine_root(archived);
        walk_jsonl(&normal, &normal, &q, thread_id, true, &mut out);
    }
    for (q, archived) in [
        (quarantine_root(false), false),
        (quarantine_root(true), true),
    ] {
        if !q.exists() {
            continue;
        }
        let normal = normal_root(archived);
        walk_jsonl(&q, &q, &normal, thread_id, false, &mut out);
    }
    out
}

fn walk_jsonl(
    root: &std::path::Path,
    dir: &std::path::Path,
    other: &std::path::Path,
    thread_id: &str,
    in_normal: bool,
    out: &mut Vec<(std::path::PathBuf, std::path::PathBuf, bool)>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // 跳过符号链接: 防目录环递归 / 防把链接指向的外部文件搬进金库
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            walk_jsonl(root, &path, other, thread_id, in_normal, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            if file_thread_id(&path).as_deref() == Some(thread_id) {
                let rel = path
                    .strip_prefix(root)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                out.push((path.clone(), other.join(rel), in_normal));
            }
        }
    }
}

/// Codex 状态库中隔离线程的备份表名 (隔离时把线程索引搬到这里, 恢复时搬回)
const THREADS_ISOLATED_TABLE: &str = "threads_codexff_isolated";
const SECTIONS_ISOLATED_TABLE: &str = "thread_sections_codexff_isolated";
const TOOLS_ISOLATED_TABLE: &str = "thread_dynamic_tools_codexff_isolated";

pub(crate) fn state_db_conn_rw() -> Result<Connection, SessionError> {
    let conn = Connection::open(codex_config::codex_state_db_path())?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

/// 确保隔离备份表存在 (同结构空表; 仅存隔离期间, 不复制触发器/索引)
fn ensure_db_backup_tables(conn: &Connection) -> Result<(), SessionError> {
    for (src, dst) in [
        ("threads", THREADS_ISOLATED_TABLE),
        ("thread_sections", SECTIONS_ISOLATED_TABLE),
        ("thread_dynamic_tools", TOOLS_ISOLATED_TABLE),
    ] {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [src],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !exists {
            continue;
        }
        let sql = format!("CREATE TABLE IF NOT EXISTS {dst} AS SELECT * FROM {src} WHERE 1=0");
        conn.execute(&sql, [])?;
    }
    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )
    .unwrap_or(false)
}

/// 把线程索引从主表搬入隔离备份表 (官方订阅激活时; Codex 侧边栏即不可见)
fn db_thread_to_backup(conn: &mut Connection, thread_id: &str) -> Result<(), SessionError> {
    ensure_db_backup_tables(conn)?;
    let tx = conn.transaction()?;
    let has_tools = table_exists(&tx, "thread_dynamic_tools");
    let has_sections = table_exists(&tx, "thread_sections");
    let in_main: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM threads WHERE id=?1)",
            [thread_id],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if in_main {
        let section_id: Option<String> = tx
            .query_row(
                "SELECT thread_section_id FROM threads WHERE id=?1",
                [thread_id],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        tx.execute(
            &format!(
                "INSERT OR IGNORE INTO {THREADS_ISOLATED_TABLE} SELECT * FROM threads WHERE id=?1"
            ),
            [thread_id],
        )?;
        // 先搬走动态工具再删线程 (FK 级联开启时, 删线程会连带清掉工具行)
        if has_tools {
            tx.execute(
                &format!(
                    "INSERT OR IGNORE INTO {TOOLS_ISOLATED_TABLE} SELECT * FROM thread_dynamic_tools WHERE thread_id=?1"
                ),
                [thread_id],
            )?;
            tx.execute(
                "DELETE FROM thread_dynamic_tools WHERE thread_id=?1",
                [thread_id],
            )?;
        }
        tx.execute("DELETE FROM threads WHERE id=?1", [thread_id])?;
        // 分区不再被任何可见线程引用时, 分区名也一起隐藏
        if has_sections {
            if let Some(sid) = section_id {
                let refs: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM threads WHERE thread_section_id=?1",
                        [&sid],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                if refs == 0 {
                    tx.execute(
                        &format!(
                            "INSERT OR IGNORE INTO {SECTIONS_ISOLATED_TABLE} SELECT * FROM thread_sections WHERE id=?1"
                        ),
                        [&sid],
                    )?;
                    tx.execute("DELETE FROM thread_sections WHERE id=?1", [&sid])?;
                }
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// 把线程索引从隔离备份表搬回主表 (切回第三方/取消隔离时)
fn db_thread_from_backup(conn: &mut Connection, thread_id: &str) -> Result<(), SessionError> {
    ensure_db_backup_tables(conn)?;
    let tx = conn.transaction()?;
    let has_tools = table_exists(&tx, "thread_dynamic_tools");
    let has_sections = table_exists(&tx, "thread_sections");
    let in_backup: bool = tx
        .query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {THREADS_ISOLATED_TABLE} WHERE id=?1)"),
            [thread_id],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if in_backup {
        let section_id: Option<String> = tx
            .query_row(
                &format!(
                    "SELECT thread_section_id FROM {THREADS_ISOLATED_TABLE} WHERE id=?1"
                ),
                [thread_id],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        if has_sections {
            if let Some(sid) = section_id {
                let in_main: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM thread_sections WHERE id=?1)",
                        [&sid],
                        |r| r.get(0),
                    )
                    .unwrap_or(false);
                if !in_main {
                    tx.execute(
                        &format!(
                            "INSERT OR IGNORE INTO thread_sections SELECT * FROM {SECTIONS_ISOLATED_TABLE} WHERE id=?1"
                        ),
                        [&sid],
                    )?;
                    tx.execute(
                        &format!("DELETE FROM {SECTIONS_ISOLATED_TABLE} WHERE id=?1"),
                        [&sid],
                    )?;
                }
            }
        }
        tx.execute(
            &format!(
                "INSERT OR IGNORE INTO threads SELECT * FROM {THREADS_ISOLATED_TABLE} WHERE id=?1"
            ),
            [thread_id],
        )?;
        tx.execute(
            &format!("DELETE FROM {THREADS_ISOLATED_TABLE} WHERE id=?1"),
            [thread_id],
        )?;
        if has_tools {
            tx.execute(
                &format!(
                    "INSERT OR IGNORE INTO thread_dynamic_tools SELECT * FROM {TOOLS_ISOLATED_TABLE} WHERE thread_id=?1"
                ),
                [thread_id],
            )?;
            tx.execute(
                &format!("DELETE FROM {TOOLS_ISOLATED_TABLE} WHERE thread_id=?1"),
                [thread_id],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// 按当前模式同步某线程的 Codex 索引: 官方→搬入备份表, 非官方→搬回主表
fn sync_db_thread(thread_id: &str, official: bool) -> Result<(), SessionError> {
    let db_path = codex_config::codex_state_db_path();
    if !db_path.exists() {
        return Ok(());
    }
    let mut conn = state_db_conn_rw()?;
    if official {
        db_thread_to_backup(&mut conn, thread_id)
    } else {
        db_thread_from_backup(&mut conn, thread_id)
    }
}

/// 读取线程的 cwd (从会话文件 session_meta 获取, 文件在正常目录或金库都能读到)
fn thread_cwd(thread_id: &str) -> Option<String> {
    for (src, _, _) in thread_files(thread_id) {
        let Ok(file) = std::fs::File::open(&src) else {
            continue;
        };
        let mut reader = std::io::BufReader::new(file);
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if let Some(cwd) = v
            .get("payload")
            .and_then(|p| p.get("cwd"))
            .and_then(|c| c.as_str())
        {
            return Some(cwd.to_string());
        }
    }
    None
}

fn ambient_suggestions_root() -> std::path::PathBuf {
    codex_config::codex_config_dir().join("ambient-suggestions")
}

fn ambient_quarantine_root() -> std::path::PathBuf {
    vault::vault_dir().join("ambient-suggestions-quarantine")
}

/// 把该 cwd 的 ambient-suggestions (桌面端"建议任务/会话片段") 移入金库,
/// 官方订阅下桌面端不会再显示这些片段; 切回第三方时原样恢复。
fn sync_ambient_suggestions(cwd: &str, official: bool) -> Result<(), SessionError> {
    let root = ambient_suggestions_root();
    if !root.exists() {
        return Ok(());
    }
    let q = ambient_quarantine_root();
    for entry in std::fs::read_dir(&root)?.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let meta_file = dir.join("ambient-suggestions.json");
        if !meta_file.exists() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&meta_file) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let matches = v
            .get("projectRoot")
            .and_then(|r| r.as_str())
            .map(|r| r == cwd)
            .unwrap_or(false);
        if !matches {
            continue;
        }
        let name = entry
            .file_name()
            .to_string_lossy()
            .to_string();
        let dst = q.join(&name);
        if official && dir.exists() && !dst.exists() {
            std::fs::create_dir_all(&q)?;
            std::fs::rename(&dir, &dst)?;
            isolation_log(&format!("ambient-suggestions moved: {name} ({cwd})"));
        } else if !official && dst.exists() && !dir.exists() {
            std::fs::create_dir_all(&root)?;
            std::fs::rename(&dst, &dir)?;
            isolation_log(&format!("ambient-suggestions restored: {name} ({cwd})"));
        }
    }
    Ok(())
}

fn session_index_path() -> std::path::PathBuf {
    codex_config::codex_config_dir().join("session_index.jsonl")
}

fn session_index_quarantine_root() -> std::path::PathBuf {
    vault::vault_dir().join("session-index-quarantine")
}

fn codex_global_state_path() -> std::path::PathBuf {
    codex_config::codex_config_dir().join(".codex-global-state.json")
}

fn global_state_quarantine_root() -> std::path::PathBuf {
    vault::vault_dir().join("global-state-quarantine")
}

/// 隔离时清理 Codex 桌面端全局状态里的项目/线程记录 (local-projects、
/// thread-project-assignments、prompt-history、thread-descriptions 等),
/// 否则侧边栏项目名/目录名仍会残留; 切回第三方时按备份恢复。
fn sync_global_state(thread_id: &str, official: bool) -> Result<(), SessionError> {
    let path = codex_global_state_path();
    let backup_root = global_state_quarantine_root();
    let backup = backup_root.join(format!("{thread_id}.json"));
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&path)?;
    let mut root: Value = serde_json::from_str(&text)?;
    let Some(obj) = root.as_object_mut() else {
        return Ok(());
    };

    if official {
        let mut removed = serde_json::Map::new();
        // 线程维度的嵌套索引 (key -> thread_id -> value)
        for key in [
            "thread-project-assignments",
            "thread-workspace-root-hints",
            "thread-writable-roots",
            "prompt-history",
            "heartbeat-thread-permissions-by-id",
            "thread-descriptions-v1",
        ] {
            if let Some(map) = obj.get_mut(key).and_then(|v| v.as_object_mut()) {
                if let Some(val) = map.remove(thread_id) {
                    let entry = removed
                        .entry(key.to_string())
                        .or_insert_with(|| Value::Object(serde_json::Map::new()));
                    if let Some(e) = entry.as_object_mut() {
                        e.insert(thread_id.to_string(), val);
                    }
                }
            }
        }
        // unread-thread-ids-by-host-v1: 从各 host 数组移除 (派生数据, 无需备份)
        if let Some(hosts) = obj
            .get_mut("unread-thread-ids-by-host-v1")
            .and_then(|v| v.as_object_mut())
        {
            for arr in hosts.values_mut() {
                if let Some(a) = arr.as_array_mut() {
                    a.retain(|x| x.as_str() != Some(thread_id));
                }
            }
        }
        // 顶层以线程 ID 为后缀的键
        for prefix in [
            format!("thread-client-id-v1:local%3A{thread_id}"),
            format!("thread-reference-capability:{thread_id}"),
            format!("thread-tab-routes-v1:{thread_id}"),
            format!("thread-browser-tabs-v1:{thread_id}"),
            format!("codex-writing-block-deleted-thread-v1:{thread_id}"),
        ] {
            if let Some(v) = obj.remove(&prefix) {
                removed.insert(prefix, v);
            }
        }
        // 项目注册表: 找到该线程 cwd 对应的 local-project, 若不再被任何
        // 线程引用则连项目节点/排序/展开态一起移除 (侧边栏项目名消失的关键)
        if let Some(cwd) = thread_cwd(thread_id) {
            let project_id: Option<String> = obj
                .get("local-projects")
                .and_then(|v| v.as_object())
                .and_then(|projects| {
                    projects.iter().find_map(|(pid, pv)| {
                        let has_root = pv
                            .get("rootPaths")
                            .and_then(|r| r.as_array())
                            .map(|roots| roots.iter().any(|r| r.as_str() == Some(cwd.as_str())))
                            .unwrap_or(false);
                        has_root.then(|| pid.clone())
                    })
                });
            if let Some(pid) = project_id {
                let still_used = obj
                    .get("thread-project-assignments")
                    .and_then(|v| v.as_object())
                    .map(|m| {
                        m.values().any(|v| {
                            v.get("projectId").and_then(|x| x.as_str()) == Some(pid.as_str())
                        })
                    })
                    .unwrap_or(false);
                if !still_used {
                    if let Some(projects) = obj.get_mut("local-projects").and_then(|v| v.as_object_mut())
                    {
                        if let Some(pv) = projects.remove(&pid) {
                            let entry = removed
                                .entry("local-projects".to_string())
                                .or_insert_with(|| Value::Object(serde_json::Map::new()));
                            if let Some(e) = entry.as_object_mut() {
                                e.insert(pid.clone(), pv);
                            }
                        }
                    }
                    if let Some(order) = obj.get_mut("project-order").and_then(|v| v.as_array_mut()) {
                        order.retain(|x| x.as_str() != Some(pid.as_str()));
                        removed
                            .entry("project-order".to_string())
                            .or_insert_with(|| Value::Array(Vec::new()))
                            .as_array_mut()
                            .map(|a| a.push(Value::String(pid.clone())));
                    }
                    let expanded_keys: Vec<String> = obj
                        .keys()
                        .filter(|k| {
                            k.starts_with("sidebar-project-expanded-v1-codex:") && k.contains(&pid)
                        })
                        .cloned()
                        .collect();
                    for k in expanded_keys {
                        if let Some(v) = obj.remove(&k) {
                            removed.insert(k, v);
                        }
                    }
                    if let Some(sel) = obj
                        .get("selected-project")
                        .and_then(|v| v.get("projectId"))
                        .and_then(|x| x.as_str())
                    {
                        if sel == pid {
                            if let Some(v) = obj.remove("selected-project") {
                                removed.insert("selected-project".to_string(), v);
                            }
                        }
                    }
                }
            }
        }
        if !removed.is_empty() {
            std::fs::create_dir_all(&backup_root)?;
            std::fs::write(
                &backup,
                serde_json::to_vec_pretty(&Value::Object(removed))?,
            )?;
            vault::atomic_write_bytes(
                &path,
                serde_json::to_vec_pretty(&root)?.as_slice(),
            )
            .map_err(|e| {
                SessionError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("写入 .codex-global-state.json 失败: {e}"),
                ))
            })?;
            isolation_log(&format!("global-state cleaned thread={thread_id}"));
        }
    } else if backup.exists() {
        let removed: Value = serde_json::from_str(&std::fs::read_to_string(&backup)?)?;
        if let Some(removed_obj) = removed.as_object() {
            for (k, v) in removed_obj {
                match k.as_str() {
                    "local-projects" => {
                        let dst = obj
                            .entry(k.clone())
                            .or_insert_with(|| Value::Object(serde_json::Map::new()));
                        if let (Some(d), Some(s)) = (dst.as_object_mut(), v.as_object()) {
                            for (pid, pv) in s {
                                d.insert(pid.clone(), pv.clone());
                            }
                        }
                    }
                    "project-order" => {
                        let dst = obj
                            .entry(k.clone())
                            .or_insert_with(|| Value::Array(Vec::new()));
                        if let (Some(d), Some(s)) = (dst.as_array_mut(), v.as_array()) {
                            for x in s {
                                if !d.contains(x) {
                                    d.push(x.clone());
                                }
                            }
                        }
                    }
                    "selected-project" => {
                        if !obj.contains_key(k) {
                            obj.insert(k.clone(), v.clone());
                        }
                    }
                    _ => {
                        if let Some(existing) = obj.get_mut(k).and_then(|x| x.as_object_mut()) {
                            if let Some(s) = v.as_object() {
                                for (kk, vv) in s {
                                    existing.insert(kk.clone(), vv.clone());
                                }
                            }
                        } else {
                            obj.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            vault::atomic_write_bytes(
                &path,
                serde_json::to_vec_pretty(&root)?.as_slice(),
            )
            .map_err(|e| {
                SessionError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("写入 .codex-global-state.json 失败: {e}"),
                ))
            })?;
        }
        std::fs::remove_file(&backup)?;
        isolation_log(&format!("global-state restored thread={thread_id}"));
    }
    Ok(())
}

/// 隔离时把线程从 session_index.jsonl (桌面端最近会话/索引) 移走,
/// 恢复时按原行加回, 避免官方订阅下仍能通过索引看到该会话。
fn sync_session_index(thread_id: &str, official: bool) -> Result<(), SessionError> {
    let path = session_index_path();
    let backup_root = session_index_quarantine_root();
    let backup = backup_root.join(format!("{thread_id}.jsonl"));
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&path)?;
    let mut keep = String::new();
    let mut removed = String::new();
    for line in text.lines() {
        let is_target = serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|v| {
                v.get("id")
                    .and_then(|i| i.as_str())
                    .map(|s| s.to_string())
            })
            .map(|s| s == thread_id)
            .unwrap_or(false);
        if is_target {
            removed.push_str(line);
            removed.push('\n');
        } else {
            keep.push_str(line);
            keep.push('\n');
        }
    }
    if official {
        if !removed.is_empty() {
            std::fs::create_dir_all(&backup_root)?;
            std::fs::write(&backup, removed.as_bytes())?;
            vault::atomic_write_bytes(&path, keep.as_bytes()).map_err(|e| {
                SessionError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("写入 session_index 失败: {e}"),
                ))
            })?;
            isolation_log(&format!("session_index removed thread={thread_id}"));
        }
    } else if backup.exists() {
        let restore_text = std::fs::read_to_string(&backup)?;
        let already = text.lines().any(|l| {
            serde_json::from_str::<Value>(l)
                .ok()
                .and_then(|v| {
                    v.get("id")
                        .and_then(|i| i.as_str())
                        .map(|s| s.to_string())
                })
                == Some(thread_id.to_string())
        });
        if !already {
            let mut merged = keep;
            merged.push_str(&restore_text);
            vault::atomic_write_bytes(&path, merged.as_bytes()).map_err(|e| {
                SessionError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("写入 session_index 失败: {e}"),
                ))
            })?;
        }
        std::fs::remove_file(&backup)?;
        isolation_log(&format!("session_index restored thread={thread_id}"));
    }
    Ok(())
}

/// 清除 Codex 其它本地缓存里该线程的摘要/目录行 (官方订阅下不可见);
/// 这些是派生缓存, 切回第三方后 Codex 会从会话文件自动重建, 无需恢复。
fn purge_aux_db_rows(thread_id: &str) {
    for (db_path, table) in [
        (
            codex_config::codex_config_dir().join("sqlite/codex-dev.db"),
            "local_thread_catalog",
        ),
        (
            codex_config::codex_config_dir().join("memories_1.sqlite"),
            "stage1_outputs",
        ),
    ] {
        if !db_path.exists() {
            continue;
        }
        let Ok(conn) = Connection::open(&db_path) else {
            continue;
        };
        let _ = conn.busy_timeout(Duration::from_secs(2));
        let sql = format!("DELETE FROM {table} WHERE thread_id=?1");
        if let Ok(n) = conn.execute(&sql, [thread_id]) {
            if n > 0 {
                isolation_log(&format!("aux db purged {table} thread={thread_id} rows={n}"));
            }
        }
    }
}

/// 同步 Codex 本地派生索引: session_index + ambient-suggestions + 摘要缓存
fn sync_local_aux(thread_id: &str, official: bool) -> Result<(), SessionError> {
    if official {
        purge_aux_db_rows(thread_id);
    }
    sync_global_state(thread_id, official)?;
    sync_session_index(thread_id, official)?;
    if let Some(cwd) = thread_cwd(thread_id) {
        sync_ambient_suggestions(&cwd, official)?;
    }
    Ok(())
}

/// 是否已标记了隔离会话 (切换前守卫: 有标记 + Codex 在运行 → 拒绝切换)
pub fn has_isolated_sessions() -> bool {
    !load_isolated().is_empty()
}

/// 当前是否官方订阅激活 (隔离生效条件)
fn official_active() -> bool {
    matches!(
        codex_config::current_profile_kind(),
        Ok(CurrentProfile::Official)
    )
}

/// Codex 桌面/CLI 是否在运行 (隔离前必须完全退出, 防止移动正在写入的文件)
pub fn codex_running() -> bool {
    let checks: [(&str, &str); 7] = [
        ("-x", "codex"),       // CLI 进程
        ("-x", "Codex"),       // 桌面 app 进程名
        ("-x", "ChatGPT"),     // ChatGPT Mac 桌面端 (Codex 内嵌其中)
        ("-f", "/Codex.app/"), // 任意 Codex.app 路径下的进程
        ("-f", "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT"),
        // Codex 引擎辅助进程 (真正持有会话/存储的进程; 不含 crashpad 等无害残留)
        ("-f", "Codex \\(Service\\)\\.app"),
        ("-f", "Codex \\(Renderer\\)\\.app"),
    ];
    for (flag, pat) in checks {
        if let Ok(out) = std::process::Command::new("pgrep").arg(flag).arg(pat).output() {
            if out.status.success() {
                return true;
            }
        }
    }
    false
}

/// 按当前激活模式同步隔离: 官方 → 标记会话移入金库; 中转/未接管 → 移回 codex。
/// 幂等, 可自愈 (崩溃/切换中途残留的错误位置会被纠正)。
pub fn sync_session_isolation() -> Result<(), SessionError> {
    sync_session_isolation_with_progress(&|_| {})
}

/// 带进度回调的隔离同步; 每个线程内先移动, 任一步失败立即回滚该线程已移动文件,
/// 避免“一半在金库一半在原位”的会话分裂。
pub fn sync_session_isolation_with_progress(
    progress: &dyn Fn(&str),
) -> Result<(), SessionError> {
    let items = load_isolated();
    let official = official_active();
    isolation_log(&format!(
        "sync start official={official} isolated_items={}",
        items.len()
    ));
    for it in &items {
        if it.thread_id.is_empty() || it.thread_id.contains('/') || it.thread_id.contains("..") {
            continue;
        }
        let files = thread_files(&it.thread_id);
        // 防御: 即使上层漏检, 只要有文件需要迁移且 Codex/ChatGPT 仍在运行,
        // 也拒绝迁移 — 防止从运行中的桌面端下方搬走会话 (它会继续显示/写回)。
        let needs_move = files
            .iter()
            .any(|(_, _, in_normal)| (official && *in_normal) || (!official && !*in_normal));
        if needs_move && codex_running() {
            isolation_log(&format!(
                "sync blocked thread={} needs_move=true codex_running=true",
                it.thread_id
            ));
            return Err(SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Codex / ChatGPT 桌面端正在运行，请先完全退出后再切换供应商",
            )));
        }
        isolation_log(&format!(
            "sync thread={} files={} needs_move={needs_move} codex_running={}",
            it.thread_id,
            files.len(),
            codex_running()
        ));
        let total = files.len();
        let mut moved: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
        for (i, (src, dst, in_normal)) in files.iter().enumerate() {
            let should_move = (official && *in_normal) || (!official && !*in_normal);
            if !should_move {
                continue;
            }
            progress(&format!("迁移会话文件 {}/{}…", i + 1, total));
            match move_file_safe(src, dst, progress) {
                Ok(false) => {}
                Ok(true) => moved.push((src.clone(), dst.clone())),
                Err(e) => {
                    // 回滚本线程已移动的文件, 保持迁移前状态
                    for (s, d) in moved.iter().rev() {
                        let _ = move_file_safe(d, s, &|_| {});
                    }
                    isolation_log(&format!(
                        "sync thread={} file move error: {e}",
                        it.thread_id
                    ));
                    return Err(e);
                }
            }
        }
        isolation_log(&format!(
            "sync thread={} moved_files={}",
            it.thread_id,
            moved.len()
        ));
        // 文件移动完成后, 同步 Codex 线程索引 (侧边栏的项目/目录/会话名)
        if let Err(e) = sync_db_thread(&it.thread_id, official) {
            for (s, d) in moved.iter().rev() {
                let _ = move_file_safe(d, s, &|_| {});
            }
            isolation_log(&format!(
                "sync thread={} db error: {e}",
                it.thread_id
            ));
            return Err(e);
        }
        isolation_log(&format!("sync thread={} db ok", it.thread_id));
        // 同步本地派生索引 (ambient-suggestions / session_index / 摘要缓存)
        if let Err(e) = sync_local_aux(&it.thread_id, official) {
            for (s, d) in moved.iter().rev() {
                let _ = move_file_safe(d, s, &|_| {});
            }
            let _ = sync_db_thread(&it.thread_id, !official);
            let _ = sync_local_aux(&it.thread_id, !official);
            isolation_log(&format!("sync thread={} aux error: {e}", it.thread_id));
            return Err(e);
        }
        isolation_log(&format!("sync thread={} aux ok", it.thread_id));
    }
    isolation_log("sync done");
    Ok(())
}

/// 标记/取消标记会话隔离。
/// 官方激活时标记 → 立即移入金库; 取消标记 → 立即移回。
pub fn set_session_isolated(thread_id: &str, isolated: bool) -> Result<(), SessionError> {
    set_session_isolated_with_progress(thread_id, isolated, &|_| {})
}

/// 带进度回调的隔离/取消隔离 (前端显示“移动会话文件 i/n”)
pub fn set_session_isolated_with_progress(
    thread_id: &str,
    isolated: bool,
    progress: &dyn Fn(&str),
) -> Result<(), SessionError> {
    isolation_log(&format!("set isolate={isolated} thread={thread_id}"));
    if thread_id.is_empty()
        || thread_id.contains('/')
        || thread_id.contains('\\')
        || thread_id.contains("..")
    {
        return Err(SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "非法线程 ID",
        )));
    }
    // 保护: 正在写入的活跃线程不允许隔离 — 会话文件被移动会导致
    // 续聊失败或 Codex 原子重写时线程分裂。
    if isolated {
        progress("检查会话状态…");
        for (src, _, _) in thread_files(thread_id) {
            if let Ok(meta) = std::fs::metadata(&src) {
                if let Ok(modified) = meta.modified() {
                    let active = modified
                        .elapsed()
                        .map(|d| d.as_secs() < 120)
                        .unwrap_or(false);
                    if active {
                        return Err(SessionError::Io(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "当前正在使用的会话不能隔离（最近 2 分钟内有写入），请会话结束后再操作",
                        )));
                    }
                }
            }
        }
    }
    let previous_items = load_isolated();
    let mut items = previous_items.clone();
    items.retain(|i| i.thread_id != thread_id);
    if isolated {
        items.push(IsolatedItem {
            thread_id: thread_id.to_string(),
        });
    }
    save_isolated(&items)?;

    let official = official_active();
    isolation_log(&format!(
        "set thread={thread_id} official={official} saved_marker={isolated}"
    ));
    progress("扫描会话文件…");
    let files = thread_files(thread_id);
    let total = files.len();
    progress(&format!("找到 {total} 个会话文件"));
    let mut moved: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    for (i, (src, dst, in_normal)) in files.iter().enumerate() {
        progress(&format!("移动会话文件 {}/{}…", i + 1, total));
        let should_move = (isolated && official && *in_normal) || (!isolated && !*in_normal);
        if !should_move {
            continue;
        }
        match move_file_safe(src, dst, progress) {
            Ok(false) => {}
            Ok(true) => moved.push((src.clone(), dst.clone())),
            Err(e) => {
                for (s, d) in moved.iter().rev() {
                    let _ = move_file_safe(d, s, &|_| {});
                }
                let _ = save_isolated(&previous_items);
                isolation_log(&format!(
                    "set thread={thread_id} file move error: {e}"
                ));
                return Err(e);
            }
        }
    }
    isolation_log(&format!(
        "set thread={thread_id} moved_files={}",
        moved.len()
    ));
    // 隔离且官方 → 线程索引移入备份表; 取消隔离 → 索引移回主表
    let db_result = if isolated && official {
        sync_db_thread(thread_id, true)
    } else if !isolated {
        sync_db_thread(thread_id, false)
    } else {
        Ok(())
    };
    if let Err(e) = db_result {
        for (s, d) in moved.iter().rev() {
            let _ = move_file_safe(d, s, &|_| {});
        }
        let _ = save_isolated(&previous_items);
        isolation_log(&format!("set thread={thread_id} db error: {e}"));
        return Err(e);
    }
    // 同步本地派生索引 (ambient-suggestions / session_index / 摘要缓存)
    let aux_result = if isolated && official {
        sync_local_aux(thread_id, true)
    } else if !isolated {
        sync_local_aux(thread_id, false)
    } else {
        Ok(())
    };
    if let Err(e) = aux_result {
        for (s, d) in moved.iter().rev() {
            let _ = move_file_safe(d, s, &|_| {});
        }
        let _ = sync_db_thread(thread_id, !official);
        let _ = sync_local_aux(thread_id, !official);
        let _ = save_isolated(&previous_items);
        isolation_log(&format!("set thread={thread_id} aux error: {e}"));
        return Err(e);
    }
    isolation_log(&format!("set thread={thread_id} done"));
    progress("完成");
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error("sqlite 错误: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// 扫描所有会话, 按最后活动时间倒序
pub fn scan_sessions() -> Result<Vec<SessionMeta>, SessionError> {
    // 自愈: 按当前激活模式把标记会话放到正确位置 (官方 ↔ 金库隔离区)
    let _ = sync_session_isolation();
    let titles = load_thread_titles();
    let models = load_thread_models();
    let mut sessions = Vec::new();

    let roots: [(std::path::PathBuf, bool, bool); 4] = [
        (codex_config::codex_sessions_paths()[0].clone(), false, false),
        (codex_config::codex_sessions_paths()[1].clone(), true, false),
        (quarantine_root(false), false, true),
        (quarantine_root(true), true, true),
    ];
    for (root, archived, isolated) in roots {
        if !root.exists() {
            continue;
        }
        collect_jsonl(
            &root,
            &root,
            &titles,
            &models,
            archived,
            isolated,
            &mut sessions,
        )?;
    }

    // 按线程合并: 同一 thread_id 的多个 rollout (续聊/子任务) 只保留最新一条,
    // 计数写入 rollups — 避免“同标题多条、只是时间不同”的重复列表。
    let mut grouped: std::collections::HashMap<String, Vec<SessionMeta>> =
        std::collections::HashMap::new();
    for s in sessions {
        grouped.entry(s.thread_id.clone()).or_default().push(s);
    }
    // 隔离状态 = 文件位置隔离 OR 隔离标记存在。
    // 非官方模式下文件不移动, 但标记已持久化, checkbox 必须仍显示已隔离。
    let isolated_markers: std::collections::HashSet<String> = load_isolated()
        .into_iter()
        .map(|i| i.thread_id)
        .collect();
    let mut merged: Vec<SessionMeta> = grouped
        .into_values()
        .map(|mut v| {
            v.sort_by(|a, b| b.last_active_ms.cmp(&a.last_active_ms));
            let mut meta = v.remove(0);
            meta.rollups = v.len() + 1;
            if isolated_markers.contains(&meta.thread_id) {
                meta.isolated = true;
            }
            meta
        })
        .collect();
    merged.sort_by(|a, b| b.last_active_ms.cmp(&a.last_active_ms));
    Ok(merged)
}

fn collect_jsonl(
    root: &Path,
    dir: &Path,
    titles: &HashMap<String, String>,
    models: &HashMap<String, String>,
    archived: bool,
    isolated: bool,
    out: &mut Vec<SessionMeta>,
) -> Result<(), SessionError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        // 跳过符号链接: 指回自身的环会无限递归栈溢出, 指向外部会被越界读取
        if entry.file_type()?.is_symlink() {
            continue;
        }
        let path = entry.path();
        let meta = entry.metadata()?;
        if meta.is_dir() {
            collect_jsonl(root, &path, titles, models, archived, isolated, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            if let Some(session) =
                parse_session(&path, root, titles, models, archived, isolated)?
            {
                out.push(session);
            }
        }
    }
    Ok(())
}

fn parse_session(
    path: &Path,
    root: &Path,
    titles: &HashMap<String, String>,
    models: &HashMap<String, String>,
    archived: bool,
    isolated: bool,
) -> Result<Option<SessionMeta>, SessionError> {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();

    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut title = None;
    let mut thread_id = String::new();
    let mut model = String::new();
    let mut preview = String::new();
    let mut found = false;

    // 读前 200 行: 提取真实线程 ID (state DB 键)、标题、模型、首条用户消息
    for _ in 0..200 {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        found = true;
        if let Some(t) = v.get("payload").and_then(|p| p.get("session_id")) {
            if let Some(s) = t.as_str() {
                if thread_id.is_empty() {
                    thread_id = s.to_string();
                }
            }
        }
        if thread_id.is_empty() {
            if let Some(t) = v.get("payload").and_then(|p| p.get("id")) {
                if let Some(s) = t.as_str() {
                    thread_id = s.to_string();
                }
            }
        }
        if let Some(t) = v.get("payload").and_then(|p| p.get("title")) {
            if let Some(s) = t.as_str() {
                if !s.is_empty() {
                    title = Some(s.to_string());
                }
            }
        }
        if let Some(t) = v.get("payload").and_then(|p| p.get("model")) {
            if let Some(s) = t.as_str() {
                model = s.to_string();
            }
        }
        if preview.is_empty()
            && v.get("type").and_then(|t| t.as_str()) == Some("user")
        {
            if let Some(t) = v.get("payload").and_then(|p| p.get("text")) {
                if let Some(s) = t.as_str() {
                    preview = s.chars().take(300).collect();
                }
            }
        }
        // 现代会话文件内没有 title 字段 (标题在 state DB), 收集到
        // 线程 ID + 模型 + 首条用户消息即可提前结束
        if !thread_id.is_empty() && !model.is_empty() && !preview.is_empty() {
            break;
        }
    }
    if !found {
        return Ok(None);
    }

    let meta = std::fs::metadata(path)?;
    let last_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // 标题优先级: 文件内 title → state DB (按真实线程 ID) → 首条消息 → 文件名
    let title = title
        .or_else(|| {
            titles
                .get(&thread_id)
                .cloned()
                .or_else(|| titles.get(&id).cloned())
        })
        .or_else(|| {
            if preview.is_empty() {
                None
            } else {
                Some(preview.chars().take(80).collect())
            }
        })
        .unwrap_or_else(|| id.clone());

    let rel = path
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let thread_id = if thread_id.is_empty() {
        id.clone()
    } else {
        thread_id
    };
    // 模型以 state DB 的 threads.model 为准（rollout 里的 payload.model 可能
    // 残留早期/瞬时设置，不代表当前线程绑定）。
    let model = models
        .get(&thread_id)
        .cloned()
        .unwrap_or(model);

    Ok(Some(SessionMeta {
        id,
        thread_id,
        provider: "codex".to_string(),
        title,
        model,
        last_active_ms: last_ms,
        path: rel,
        archived,
        isolated,
        preview,
        rollups: 1,
    }))
}

/// 从 state_5.sqlite 读 thread 标题 (codex 运行时占用 DB, 只读 + busy timeout)
pub(crate) fn load_thread_titles() -> HashMap<String, String> {
    let mut titles = HashMap::new();
    let db_path = codex_config::codex_state_db_path();
    if !db_path.exists() {
        return titles;
    }

    let Ok(conn) = Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return titles;
    };
    let _ = conn.busy_timeout(Duration::from_secs(2));

    let Ok(mut stmt) = conn.prepare(
        "SELECT id, title FROM threads \
         WHERE title <> ''",
    ) else {
        return titles;
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) else {
        return titles;
    };
    for row in rows.flatten() {
        titles.insert(row.0, row.1);
    }
    titles
}

/// 从 state_5.sqlite 读线程当前绑定模型（会话列表“用当前模型续聊”判断用）。
pub(crate) fn load_thread_models() -> HashMap<String, String> {
    let mut models = HashMap::new();
    let db_path = codex_config::codex_state_db_path();
    if !db_path.exists() {
        return models;
    }

    let Ok(conn) = Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return models;
    };
    let _ = conn.busy_timeout(Duration::from_secs(2));

    let Ok(mut stmt) = conn.prepare(
        "SELECT id, model FROM threads \
         WHERE model IS NOT NULL AND model <> ''",
    ) else {
        return models;
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) else {
        return models;
    };
    for row in rows.flatten() {
        models.insert(row.0, row.1);
    }
    models
}

/// 会话详情: 返回原始 JSONL 行 (前端渲染)。限流防止大文件炸内存。
///
/// 路径安全: 仅允许 sessions/ 或 archived_sessions/ 内的相对路径 —
/// canonicalize 后校验前缀, 拒绝 `..`/绝对路径/符号链接逃逸。
/// 同时修复 archived 会话 (相对路径按归档根解析) 找不到文件的问题。
pub fn session_detail(path: &str, max_lines: usize) -> Result<Vec<Value>, SessionError> {
    if path.contains("..") {
        return Err(SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "非法路径",
        )));
    }
    let denied = || {
        SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "路径越权",
        ))
    };
    let mut opened: Option<std::path::PathBuf> = None;
    let mut roots = codex_config::codex_sessions_paths();
    roots.push(quarantine_root(false));
    roots.push(quarantine_root(true));
    for root in roots {
        if !root.exists() {
            continue;
        }
        let canon_root = root.canonicalize().map_err(SessionError::Io)?;
        let full = match root.join(path).canonicalize() {
            Ok(f) => f,
            // 文件不存在/损坏 → 试下一个根 (sessions vs archived_sessions)
            Err(_) => continue,
        };
        if !full.starts_with(&canon_root) {
            return Err(denied());
        }
        opened = Some(full);
        break;
    }
    let full = opened.ok_or_else(|| {
        SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "会话文件不存在",
        ))
    })?;
    let file = std::fs::File::open(&full)?;
    // 单行可能 GB 级 (超大 tool result) — 限总字节, 防整行读入内存
    const MAX_BYTES: u64 = 16 * 1024 * 1024;
    let reader = BufReader::new(file.take(MAX_BYTES));

    let mut out = Vec::new();
    for line in reader.lines().take(max_lines) {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            out.push(v);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_isolation_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.sqlite");
        let setup = Connection::open(&db_path).unwrap();
        setup
            .execute_batch(
                r#"
                CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    source TEXT NOT NULL,
                    model_provider TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    title TEXT NOT NULL,
                    sandbox_policy TEXT NOT NULL,
                    approval_mode TEXT NOT NULL,
                    tokens_used INTEGER NOT NULL DEFAULT 0,
                    has_user_event INTEGER NOT NULL DEFAULT 0,
                    archived INTEGER NOT NULL DEFAULT 0,
                    archived_at INTEGER,
                    git_sha TEXT,
                    git_branch TEXT,
                    git_origin_url TEXT,
                    cli_version TEXT NOT NULL DEFAULT '',
                    first_user_message TEXT NOT NULL DEFAULT '',
                    agent_nickname TEXT,
                    agent_role TEXT,
                    memory_mode TEXT NOT NULL DEFAULT 'enabled',
                    model TEXT,
                    reasoning_effort TEXT,
                    agent_path TEXT,
                    created_at_ms INTEGER,
                    updated_at_ms INTEGER,
                    thread_source TEXT,
                    preview TEXT NOT NULL DEFAULT '',
                    recency_at INTEGER NOT NULL DEFAULT 0,
                    recency_at_ms INTEGER NOT NULL DEFAULT 0,
                    history_mode TEXT NOT NULL DEFAULT 'legacy',
                    name TEXT,
                    is_pinned INTEGER NOT NULL DEFAULT 0,
                    thread_section_id TEXT REFERENCES thread_sections(id) ON DELETE SET NULL,
                    section_position INTEGER,
                    section_entered_at_ms INTEGER
                );
                CREATE TABLE thread_sections (id TEXT PRIMARY KEY, name TEXT NOT NULL);
                CREATE TABLE thread_dynamic_tools (
                    thread_id TEXT NOT NULL,
                    position INTEGER NOT NULL,
                    name TEXT NOT NULL,
                    description TEXT NOT NULL,
                    input_schema TEXT NOT NULL,
                    defer_loading INTEGER NOT NULL DEFAULT 0,
                    namespace TEXT,
                    PRIMARY KEY(thread_id, position),
                    FOREIGN KEY(thread_id) REFERENCES threads(id) ON DELETE CASCADE
                );
                INSERT INTO thread_sections VALUES ('sec1', 'Pinned');
                INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode, thread_section_id)
                VALUES ('t1', '/x/r.jsonl', 1, 1, 'user', 'custom', '/tmp', 'Hi', 'read-only', 'on-failure', 'sec1');
                INSERT INTO thread_dynamic_tools VALUES ('t1', 0, 'tool', 'desc', '{}', 0, NULL);
                "#,
            )
            .unwrap();

        let mut conn = Connection::open(&db_path).unwrap();
        db_thread_to_backup(&mut conn, "t1").unwrap();
        let counts: (i64, i64, i64, i64, i64, i64) = conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM threads WHERE id='t1'),
                    (SELECT COUNT(*) FROM threads_codexff_isolated WHERE id='t1'),
                    (SELECT COUNT(*) FROM thread_sections WHERE id='sec1'),
                    (SELECT COUNT(*) FROM thread_sections_codexff_isolated WHERE id='sec1'),
                    (SELECT COUNT(*) FROM thread_dynamic_tools WHERE thread_id='t1'),
                    (SELECT COUNT(*) FROM thread_dynamic_tools_codexff_isolated WHERE thread_id='t1')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!(counts, (0, 1, 0, 1, 0, 1));

        db_thread_from_backup(&mut conn, "t1").unwrap();
        let counts: (i64, i64, i64, i64, i64, i64) = conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM threads WHERE id='t1'),
                    (SELECT COUNT(*) FROM threads_codexff_isolated WHERE id='t1'),
                    (SELECT COUNT(*) FROM thread_sections WHERE id='sec1'),
                    (SELECT COUNT(*) FROM thread_sections_codexff_isolated WHERE id='sec1'),
                    (SELECT COUNT(*) FROM thread_dynamic_tools WHERE thread_id='t1'),
                    (SELECT COUNT(*) FROM thread_dynamic_tools_codexff_isolated WHERE thread_id='t1')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!(counts, (1, 0, 1, 0, 1, 0));
    }

    #[test]
    fn db_isolation_without_optional_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.sqlite");
        let setup = Connection::open(&db_path).unwrap();
        setup
            .execute_batch(
                r#"
                CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    source TEXT NOT NULL,
                    model_provider TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    title TEXT NOT NULL,
                    sandbox_policy TEXT NOT NULL,
                    approval_mode TEXT NOT NULL
                );
                INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode)
                VALUES ('t1', '/x/r.jsonl', 1, 1, 'user', 'custom', '/tmp', 'Hi', 'read-only', 'on-failure');
                "#,
            )
            .unwrap();

        let mut conn = Connection::open(&db_path).unwrap();
        db_thread_to_backup(&mut conn, "t1").unwrap();
        let main: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads WHERE id='t1'", [], |r| r.get(0))
            .unwrap();
        let backup: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM threads_codexff_isolated WHERE id='t1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!((main, backup), (0, 1));

        db_thread_from_backup(&mut conn, "t1").unwrap();
        let main: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads WHERE id='t1'", [], |r| r.get(0))
            .unwrap();
        let backup: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM threads_codexff_isolated WHERE id='t1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!((main, backup), (1, 0));
    }
}
