//! 第三方用量统计 — 余额快照历史 + 本地路由请求日志。
//!
//! 纯本地存储 (vault 目录):
//! - usage-stats.json  余额快照 (按供应商按天去重, 保留最近 90 天)
//! - usage-log.jsonl   本地路由请求日志 (token/模型/供应商, 追加写)

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::vault;

const SNAPSHOT_FILE: &str = "usage-stats.json";
const REQUEST_LOG_FILE: &str = "usage-log.jsonl";
const SNAPSHOT_KEEP_DAYS: i64 = 90;
const LOG_KEEP_DAYS: i64 = 30;

static LOG_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
static TEST_DIR: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

/// 存储目录: 测试用独立临时目录, 生产走 vault
fn storage_dir() -> std::path::PathBuf {
    #[cfg(test)]
    {
        if let Some(d) = TEST_DIR
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return d;
        }
    }
    vault::vault_dir()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceSnapshot {
    pub ts_ms: i64,
    pub provider_id: String,
    pub provider_name: String,
    pub balance: Option<f64>,
    pub currency: Option<String>,
    pub total: Option<f64>,
    pub used: Option<f64>,
}

/// 本地路由请求日志 (每次 API 请求一条)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageLogEntry {
    pub ts_ms: i64,
    /// 实际处理请求的供应商 (故障转移后可能是备用)
    pub provider_id: String,
    pub provider_name: String,
    pub model: Option<String>,
    pub wire_api: Option<String>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    /// 上游报告的前缀缓存命中 token (DeepSeek Responses =
    /// input_tokens_details.cached_tokens; Chat Completions =
    /// prompt_cache_hit_tokens)
    pub cache_read_tokens: Option<u64>,
    /// 前缀缓存未命中 token (上游直接给出, 或由 input-cached 推算)
    pub cache_miss_tokens: Option<u64>,
    /// 成本估算 (元, 由模型单价表计算; 无单价为 None)
    pub cost: Option<f64>,
    pub status: u16,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DailyPoint {
    pub date: String,
    pub balance: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderUsage {
    pub provider_id: String,
    pub provider_name: String,
    pub latest: Option<BalanceSnapshot>,
    /// 最近 30 天逐日余额 (无当天数据则为 null)
    pub series: Vec<DailyPoint>,
    /// 最近 30 天前缀缓存命中/未命中 token (无缓存指标时为 0)
    pub cache_read_tokens: u64,
    pub cache_miss_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageOverview {
    pub providers: Vec<ProviderUsage>,
    /// 本地路由累计请求统计 (最近 30 天)
    pub requests: u64,
    pub total_tokens: u64,
    pub estimated_cost: f64,
    pub last_request_ms: Option<i64>,
    /// 前缀缓存命中/未命中 token (最近 30 天; 上游不支持缓存指标时为 0)
    pub cache_read_tokens: u64,
    pub cache_miss_tokens: u64,
    /// 会话扫描统计 (最近 30 天, Codex 会话文件)
    pub session_requests: u64,
    pub session_tokens: u64,
    pub session_cost: f64,
}

fn stats_path() -> std::path::PathBuf {
    storage_dir().join(SNAPSHOT_FILE)
}

fn log_path() -> std::path::PathBuf {
    storage_dir().join(REQUEST_LOG_FILE)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn read_snapshots() -> Vec<BalanceSnapshot> {
    std::fs::read_to_string(stats_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<BalanceSnapshot>>(&t).ok())
        .unwrap_or_default()
}

fn write_snapshots(items: &[BalanceSnapshot]) {
    if let Ok(bytes) = serde_json::to_vec_pretty(items) {
        let _ = vault::atomic_write_bytes(&stats_path(), &bytes);
    }
}

/// 记录一条余额快照: 同一供应商同一天只保留最新一条
pub fn record_balance(
    provider_id: &str,
    provider_name: &str,
    balance: Option<f64>,
    currency: Option<String>,
    total: Option<f64>,
    used: Option<f64>,
) {
    let ts = now_ms();
    let day = ts / 86_400_000;
    let mut items = read_snapshots();
    items.retain(|s| {
        !(s.provider_id == provider_id && s.ts_ms / 86_400_000 == day)
    });
    items.push(BalanceSnapshot {
        ts_ms: ts,
        provider_id: provider_id.to_string(),
        provider_name: provider_name.to_string(),
        balance,
        currency,
        total,
        used,
    });
    // 只保留最近 90 天
    let cutoff = (ts / 86_400_000 - SNAPSHOT_KEEP_DAYS) * 86_400_000;
    items.retain(|s| s.ts_ms >= cutoff);
    write_snapshots(&items);
}

/// 追加一条本地路由请求日志 (并清理过期行)
pub fn append_usage_log(entry: UsageLogEntry) {
    let _guard = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(parent) = log_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(line) = serde_json::to_string(&entry) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path())
        {
            use std::io::Write;
            let _ = writeln!(f, "{line}");
        }
    }
    prune_log();
}

fn prune_log() {
    let cutoff = (now_ms() / 86_400_000 - LOG_KEEP_DAYS) * 86_400_000;
    let Ok(text) = std::fs::read_to_string(log_path()) else {
        return;
    };
    let lines: Vec<&str> = text.lines().collect();
    let keep: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| {
            serde_json::from_str::<UsageLogEntry>(l)
                .map(|e| e.ts_ms >= cutoff)
                .unwrap_or(false)
        })
        .collect();
    if keep.len() != lines.len() {
        if let Ok(mut f) = std::fs::File::create(log_path()) {
            use std::io::Write;
            let _ = f.write_all(keep.join("\n").as_bytes());
            let _ = f.write_all(b"\n");
        }
    }
}

fn read_logs() -> Vec<UsageLogEntry> {
    let Ok(text) = std::fs::read_to_string(log_path()) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<UsageLogEntry>(l).ok())
        .collect()
}

/// 汇总: 各供应商余额序列 + 本地路由请求统计
pub fn overview() -> UsageOverview {
    let snaps = read_snapshots();
    let logs = read_logs();
    let session = crate::session_usage::scan();
    let cutoff = (now_ms() / 86_400_000 - LOG_KEEP_DAYS) * 86_400_000;
    let mut by_provider: Vec<ProviderUsage> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut cache_by_provider: HashMap<String, (u64, u64)> = HashMap::new();
    let mut name_by_provider: HashMap<String, String> = HashMap::new();

    for s in snaps.iter() {
        let pos = match index.get(&s.provider_id) {
            Some(p) => *p,
            None => {
                index.insert(s.provider_id.clone(), by_provider.len());
                by_provider.push(ProviderUsage {
                    provider_id: s.provider_id.clone(),
                    provider_name: s.provider_name.clone(),
                    latest: None,
                    series: Vec::new(),
                    cache_read_tokens: 0,
                    cache_miss_tokens: 0,
                });
                by_provider.len() - 1
            }
        };
        let p = &mut by_provider[pos];
        p.provider_name = s.provider_name.clone();
        if p
            .latest
            .as_ref()
            .map(|l: &BalanceSnapshot| s.ts_ms > l.ts_ms)
            .unwrap_or(true)
        {
            p.latest = Some(s.clone());
        }
        p.series.push(DailyPoint {
            date: day_str(s.ts_ms),
            balance: s.balance,
        });
    }

    for l in logs.iter().filter(|l| l.ts_ms >= cutoff) {
        name_by_provider
            .entry(l.provider_id.clone())
            .or_insert_with(|| l.provider_name.clone());
        if let (Some(read), Some(miss)) = (l.cache_read_tokens, l.cache_miss_tokens) {
            let entry = cache_by_provider.entry(l.provider_id.clone()).or_default();
            entry.0 += read;
            entry.1 += miss;
        }
    }
    for pid in cache_by_provider.keys() {
        if !index.contains_key(pid) {
            index.insert(pid.clone(), by_provider.len());
            by_provider.push(ProviderUsage {
                provider_id: pid.clone(),
                provider_name: name_by_provider.get(pid).cloned().unwrap_or_default(),
                latest: None,
                series: Vec::new(),
                cache_read_tokens: 0,
                cache_miss_tokens: 0,
            });
        }
    }

    for p in by_provider.iter_mut() {
        let (read, miss) = cache_by_provider
            .get(&p.provider_id)
            .copied()
            .unwrap_or((0, 0));
        p.cache_read_tokens = read;
        p.cache_miss_tokens = miss;
        p.series.sort_by(|a, b| a.date.cmp(&b.date));
        p.series.dedup_by(|a, b| a.date == b.date);
        // 补齐最近 30 天日期 (无数据为 null)
        let mut filled = Vec::new();
        let today = now_ms() / 86_400_000;
        for off in (0..30).rev() {
            let day = today - off;
            let date = day_str(day * 86_400_000);
            let val = p
                .series
                .iter()
                .find(|d| d.date == date)
                .map(|d| d.balance)
                .flatten();
            filled.push(DailyPoint { date, balance: val });
        }
        p.series = filled;
    }

    let recent: Vec<&UsageLogEntry> = logs.iter().filter(|l| l.ts_ms >= cutoff).collect();
    UsageOverview {
        providers: by_provider,
        requests: recent.len() as u64 + session.requests,
        total_tokens: recent.iter().filter_map(|l| l.total_tokens).sum::<u64>() + session.tokens,
        estimated_cost: recent.iter().filter_map(|l| l.cost).sum::<f64>() + session.cost,
        last_request_ms: recent.iter().map(|l| l.ts_ms).max(),
        cache_read_tokens: recent
            .iter()
            .filter_map(|l| l.cache_read_tokens)
            .sum::<u64>(),
        cache_miss_tokens: recent
            .iter()
            .filter_map(|l| l.cache_miss_tokens)
            .sum::<u64>(),
        session_requests: session.requests,
        session_tokens: session.tokens,
        session_cost: session.cost,
    }
}

fn day_str(ts_ms: i64) -> String {
    let secs = (ts_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    // 简单的 UTC 日期 (展示用; 本地时区偏移不敏感)
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// 天数 → (年,月,日) 公历换算 (Howard Hinnant 算法)
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_dedupe_and_fill() {
        let _env = crate::test_util::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("codexff-usage-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        *TEST_DIR.lock().unwrap() = Some(tmp.clone());
        *crate::session_usage::TEST_DIR.lock().unwrap() = Some(tmp.clone());
        record_balance("p1", "甲", Some(10.0), Some("CNY".into()), Some(20.0), Some(10.0));
        record_balance("p1", "甲", Some(9.5), Some("CNY".into()), Some(20.0), Some(10.5));
        let snaps = read_snapshots();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].balance, Some(9.5));
        let ov = overview();
        assert_eq!(ov.providers.len(), 1);
        assert_eq!(ov.providers[0].series.len(), 30);
        assert!(ov.providers[0].series.iter().any(|d| d.balance.is_some()));

        append_usage_log(UsageLogEntry {
            ts_ms: now_ms(),
            provider_id: "p1".into(),
            provider_name: "甲".into(),
            model: Some("deepseek-chat".into()),
            wire_api: Some("openai_chat".into()),
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            total_tokens: Some(150),
            cache_read_tokens: Some(40),
            cache_miss_tokens: Some(60),
            cost: Some(0.01),
            status: 200,
            error: None,
        });
        let ov = overview();
        assert_eq!(ov.requests, 1);
        assert_eq!(ov.total_tokens, 150);
        assert_eq!(ov.cache_read_tokens, 40);
        assert_eq!(ov.cache_miss_tokens, 60);
        assert_eq!(ov.providers[0].cache_read_tokens, 40);
        assert_eq!(ov.providers[0].cache_miss_tokens, 60);
        assert!((ov.estimated_cost - 0.01).abs() < 1e-9);
        *TEST_DIR.lock().unwrap() = None;
        *crate::session_usage::TEST_DIR.lock().unwrap() = None;
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
