//! 统一会话历史 — 把仍停留在 "openai" 桶的旧官方会话迁入共享 "custom" 桶。
//!
//! 官方与第三方共用 `model_provider = "custom"` 后历史列表互通; 旧官方会话
//! 的 session_meta / state DB 仍记录 "openai", 需要改写桶字段才出现在共享
//! 历史里。迁移前自动备份 jsonl + state DB 到金库, 支持按备份账本精确还原。

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{backup::Backup, params_from_iter, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

fn unify_backup_root() -> PathBuf {
    vault::vault_dir().join("session-unify-backup")
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
}
