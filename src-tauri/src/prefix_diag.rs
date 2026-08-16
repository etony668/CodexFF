//! 前缀稳定性诊断 — 观测 Codex 发出的 Responses 请求，记录同一会话内
//! instructions / tools / 历史前缀的跨请求变化。
//!
//! 目的: 定位哪些变化会让 DeepSeek/中转站的前缀缓存从第一个变化 token
//! 起失效 (与 DeepSeek Harness 的 "prefix stability is corollary #1"
//! 同一观测口径)。只追加写变化事件到 vault/prefix-stability.log，
//! 不保存任何提示内容。

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write as _;
use std::sync::{LazyLock, Mutex};

use serde::Serialize;
use serde_json::Value;

use crate::vault;

const LOG_FILE: &str = "prefix-stability.log";
/// 历史前缀对比上限: 超过部分不再比较 (压缩通常发生在历史头部, 1 MiB
/// 足够捕获位置; 也避免 67MB 级长会话在每轮请求上做全量序列化)。
const MAX_PREFIX_BYTES: usize = 1 << 20;
const MAX_SESSIONS: usize = 32;

static FRAMES: LazyLock<Mutex<HashMap<SessionKey, Frame>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static LOG_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, PartialEq, Eq, Hash)]
struct SessionKey {
    provider_id: String,
    /// input[0] 的 hash: 同一会话第一条内容稳定, 用于区分不同会话。
    /// 不同会话恰有相同首条内容时可能串帧, 仅影响诊断日志, 无功能影响。
    first_item_hash: u64,
}

struct Frame {
    instructions_hash: u64,
    tools_hash: u64,
    history_prefix: Vec<u8>,
}

struct Snapshot {
    instructions_hash: u64,
    tools_hash: u64,
    history_prefix: Vec<u8>,
    input_len: usize,
}

#[derive(Serialize)]
struct Divergence {
    ts_ms: i64,
    provider_id: String,
    model: Option<String>,
    /// 变化部分: instructions / tools / history 的组合
    kind: String,
    /// history 变化时, 与上一请求历史前缀首个不同字节的偏移
    offset: Option<u64>,
    instructions_hash: u64,
    tools_hash: u64,
    input_len: usize,
    prefix_len: usize,
}

/// 观测一次 Responses 请求。仅当同一会话内 prefix 相对上一请求发生
/// 变化时写一条日志; 纯追加 (正常续聊) 不写。
pub fn observe(provider_id: &str, body: &[u8]) {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return;
    };
    let Some(obj) = value.as_object() else {
        return;
    };
    let Some(input) = obj.get("input").and_then(Value::as_array) else {
        return;
    };
    if input.is_empty() {
        return;
    }

    let instructions_hash = hash_str(
        obj.get("instructions")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let tools_hash = hash_value(obj.get("tools").unwrap_or(&Value::Null));
    let first_item_hash = hash_value(&input[0]);
    // 历史前缀 = input 去掉最后一条 (最后一条是当前用户消息)。
    let history_prefix = if input.len() > 1 {
        serialize_capped(&Value::Array(input[..input.len() - 1].to_vec()))
    } else {
        Vec::new()
    };
    let snapshot = Snapshot {
        instructions_hash,
        tools_hash,
        history_prefix,
        input_len: input.len(),
    };
    let key = SessionKey {
        provider_id: provider_id.to_string(),
        first_item_hash,
    };
    let model = obj.get("model").and_then(Value::as_str).map(str::to_string);

    let mut frames = FRAMES.lock().unwrap_or_else(|e| e.into_inner());
    match frames.get_mut(&key) {
        Some(frame) => {
            if let Some(d) = analyze(Some(&*frame), &snapshot, provider_id, model.as_deref()) {
                append(&d);
            }
            frame.instructions_hash = snapshot.instructions_hash;
            frame.tools_hash = snapshot.tools_hash;
            frame.history_prefix = snapshot.history_prefix;
        }
        None => {
            if frames.len() >= MAX_SESSIONS {
                let stale = frames.keys().next().cloned();
                if let Some(k) = stale {
                    frames.remove(&k);
                }
            }
            frames.insert(
                key,
                Frame {
                    instructions_hash: snapshot.instructions_hash,
                    tools_hash: snapshot.tools_hash,
                    history_prefix: snapshot.history_prefix,
                },
            );
        }
    }
}

fn analyze(
    prev: Option<&Frame>,
    snap: &Snapshot,
    provider_id: &str,
    model: Option<&str>,
) -> Option<Divergence> {
    let prev = prev?;
    let mut kinds = Vec::new();
    let mut offset = None;
    if prev.instructions_hash != snap.instructions_hash {
        kinds.push("instructions");
    }
    if prev.tools_hash != snap.tools_hash {
        kinds.push("tools");
    }
    if !starts_with(&prev.history_prefix, &snap.history_prefix) {
        kinds.push("history");
        offset = Some(first_divergence(&prev.history_prefix, &snap.history_prefix) as u64);
    }
    if kinds.is_empty() {
        return None;
    }
    Some(Divergence {
        ts_ms: now_ms(),
        provider_id: provider_id.to_string(),
        model: model.map(str::to_string),
        kind: kinds.join(","),
        offset,
        instructions_hash: snap.instructions_hash,
        tools_hash: snap.tools_hash,
        input_len: snap.input_len,
        prefix_len: snap.history_prefix.len(),
    })
}

fn append(d: &Divergence) {
    let _guard = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = vault::vault_dir().join(LOG_FILE);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(line) = serde_json::to_string(d) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn hash_value(value: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn hash_str(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

fn starts_with(prefix: &[u8], longer: &[u8]) -> bool {
    longer.len() >= prefix.len() && prefix == &longer[..prefix.len()]
}

fn first_divergence(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    for i in 0..n {
        if a[i] != b[i] {
            return i;
        }
    }
    n
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 把 value 序列化到最多 cap 字节 (超出的部分丢弃但序列化继续, 避免
/// 为大历史分配整块内存)。
fn serialize_capped(value: &Value) -> Vec<u8> {
    struct CappedWriter {
        buf: Vec<u8>,
        cap: usize,
    }
    impl std::io::Write for CappedWriter {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            let remaining = self.cap.saturating_sub(self.buf.len());
            if remaining > 0 {
                let take = data.len().min(remaining);
                self.buf.extend_from_slice(&data[..take]);
            }
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = CappedWriter {
        buf: Vec::with_capacity(4096),
        cap: MAX_PREFIX_BYTES,
    };
    let _ = serde_json::to_writer(&mut writer, value);
    writer.buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(history: &[u8]) -> Frame {
        Frame {
            instructions_hash: 1,
            tools_hash: 2,
            history_prefix: history.to_vec(),
        }
    }

    fn snap(history: &[u8], instructions_hash: u64, tools_hash: u64) -> Snapshot {
        Snapshot {
            instructions_hash,
            tools_hash,
            history_prefix: history.to_vec(),
            input_len: 0,
        }
    }

    #[test]
    fn first_request_has_no_divergence() {
        let s = snap(b"abc", 1, 2);
        assert!(analyze(None, &s, "p", None).is_none());
    }

    #[test]
    fn append_only_history_is_stable() {
        let prev = frame(b"abc");
        let s = snap(b"abcdef", 1, 2);
        assert!(analyze(Some(&prev), &s, "p", None).is_none());
    }

    #[test]
    fn rewritten_history_reports_offset() {
        let prev = frame(b"abcdef");
        let s = snap(b"abx-def", 1, 2);
        let d = analyze(Some(&prev), &s, "p", None).expect("divergence");
        assert_eq!(d.kind, "history");
        assert_eq!(d.offset, Some(2));
    }

    #[test]
    fn instructions_and_tools_changes_are_reported() {
        let prev = frame(b"abc");
        let s = snap(b"abcdef", 9, 8);
        let d = analyze(Some(&prev), &s, "p", Some("deepseek-v4-pro")).expect("divergence");
        assert!(d.kind.contains("instructions"));
        assert!(d.kind.contains("tools"));
        assert_eq!(d.offset, None);
    }
}
