//! 会话扫描用量统计 — 从 Codex 会话 JSONL 提取 token 消耗。
//!
//! 数据源: ~/.codex/sessions/**/*.jsonl (含 archived_sessions)。
//! 结构: event_msg / payload.type=token_count / payload.info.last_token_usage
//! 增量: 按文件 mtime+size 缓存, 只解析新增/变更文件。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::codex_config;
use crate::local_router::estimate_cost;
use crate::vault;

const CACHE_FILE: &str = "session-usage-cache.json";
const KEEP_DAYS: i64 = 30;

#[cfg(test)]
pub(crate) static TEST_DIR: std::sync::Mutex<Option<std::path::PathBuf>> =
    std::sync::Mutex::new(None);

/// 缓存目录: 测试用独立临时目录, 生产走 vault
fn storage_dir() -> std::path::PathBuf {
    #[cfg(test)]
    {
        if let Some(d) = TEST_DIR.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return d;
        }
    }
    vault::vault_dir()
}

/// 会话根目录: 测试用临时目录, 生产读 ~/.codex
fn session_roots() -> Vec<PathBuf> {
    #[cfg(test)]
    {
        if let Some(d) = TEST_DIR.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return vec![d.join("sessions"), d.join("archived_sessions")];
        }
    }
    codex_config::codex_sessions_paths()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct FileStat {
    mtime_ms: i64,
    size: u64,
    requests: u64,
    tokens: u64,
    cost: f64,
    last_ts_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Cache {
    files: HashMap<String, FileStat>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionUsageSummary {
    pub files: u64,
    pub requests: u64,
    pub tokens: u64,
    pub cost: f64,
    pub last_ts_ms: Option<i64>,
}

fn cache_path() -> PathBuf {
    storage_dir().join(CACHE_FILE)
}

fn read_cache() -> Cache {
    std::fs::read_to_string(cache_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Cache>(&t).ok())
        .unwrap_or_default()
}

fn write_cache(c: &Cache) {
    if let Ok(bytes) = serde_json::to_vec_pretty(c) {
        let _ = vault::atomic_write_bytes(&cache_path(), &bytes);
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 解析一行, 返回 (model, 本次增量 token 统计) — 仅 token_count 行产生统计
fn parse_line(line: &str, model: &mut Option<String>) -> Option<(Option<String>, u64, u64)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    // 跟踪模型: response_item / session_meta 行带 payload.model
    if let Some(m) = v
        .pointer("/payload/model")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty() && m.len() < 128)
    {
        *model = Some(m.to_string());
    }
    if v.get("type").and_then(|t| t.as_str()) != Some("event_msg") {
        return None;
    }
    if v.pointer("/payload/type").and_then(|t| t.as_str()) != Some("token_count") {
        return None;
    }
    let usage = v.pointer("/payload/info/last_token_usage")?;
    let input = usage
        .get("input_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    if input == 0 && output == 0 {
        return None;
    }
    Some((model.clone(), input, output))
}

fn parse_file(path: &Path) -> (u64, u64, f64, i64) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (0, 0, 0.0, 0);
    };
    let mut model: Option<String> = None;
    let mut requests = 0u64;
    let mut tokens = 0u64;
    let mut cost = 0.0f64;
    let mut last_ts = 0i64;
    for line in text.lines() {
        if let Some((m, input, output)) = parse_line(line, &mut model) {
            requests += 1;
            tokens += input + output;
            if let Some(c) = estimate_cost(m.as_deref(), Some(input), Some(output)) {
                cost += c;
            }
        }
        // 记录该文件最新时间戳 (任意行的 timestamp)
        if let Some(ts) = line_timestamp_ms(line) {
            if ts > last_ts {
                last_ts = ts;
            }
        }
    }
    (requests, tokens, cost, last_ts)
}

fn line_timestamp_ms(line: &str) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let ts = v.get("timestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.timestamp_millis())
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            out.push(p);
        }
    }
}

/// 增量扫描全部会话文件, 返回最近 30 天汇总
pub fn scan() -> SessionUsageSummary {
    let mut cache = read_cache();
    let mut files = Vec::new();
    for root in session_roots() {
        walk(&root, &mut files);
    }
    for path in files.iter() {
        let key = path.display().to_string();
        let meta = std::fs::metadata(path);
        let Ok(meta) = meta else {
            cache.files.remove(&key);
            continue;
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let size = meta.len();
        let cached = cache.files.get(&key);
        if cached
            .map(|c| c.mtime_ms == mtime && c.size == size)
            .unwrap_or(false)
        {
            continue;
        }
        let (requests, tokens, cost, last_ts) = parse_file(path);
        cache.files.insert(
            key,
            FileStat {
                mtime_ms: mtime,
                size,
                requests,
                tokens,
                cost,
                last_ts_ms: last_ts,
            },
        );
    }
    // 清理已删除文件
    let existing: std::collections::HashSet<String> =
        files.iter().map(|p| p.display().to_string()).collect();
    cache.files.retain(|k, _| existing.contains(k));
    write_cache(&cache);

    let cutoff = (now_ms() / 86_400_000 - KEEP_DAYS) * 86_400_000;
    let mut summary = SessionUsageSummary::default();
    for stat in cache.files.values() {
        if stat.last_ts_ms > 0 && stat.last_ts_ms < cutoff {
            continue;
        }
        summary.files += 1;
        summary.requests += stat.requests;
        summary.tokens += stat.tokens;
        summary.cost += stat.cost;
        summary.last_ts_ms = Some(
            summary
                .last_ts_ms
                .map(|t| t.max(stat.last_ts_ms))
                .unwrap_or(stat.last_ts_ms),
        );
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_token_count_line() {
        let line = r#"{"timestamp":"2026-08-06T07:32:35.123Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":40,"total_tokens":150},"total_token_usage":{}}}}"#;
        let mut model = None;
        let r = parse_line(line, &mut model).unwrap();
        assert_eq!(r.1, 100);
        assert_eq!(r.2, 50);

        let meta = r#"{"timestamp":"2026-08-06T07:32:30Z","type":"session_meta","payload":{"model":"gpt-5.5"}}"#;
        assert!(parse_line(meta, &mut model).is_none());
        assert_eq!(model.as_deref(), Some("gpt-5.5"));
    }
}
