//! 终端输出重放缓冲 — 为晚订阅的客户端保留最近启动输出 (例如 shell prompt)。
//!
//! 对应 Node `output-replay-buffer.js`。
//!
//! 纯内存、小容量 (默认 64KB), 仅为覆盖启动竞态而非持久 scrollback。

use std::sync::Mutex;

/// 单条重放块。
#[derive(Debug, Clone)]
pub struct ReplayChunk {
    pub id: u64,
    pub data: String,
    pub bytes: usize,
}

/// 重放缓冲状态 (线程安全 via `Mutex`)。
pub struct ReplayBuffer {
    inner: Mutex<ReplayBufferInner>,
}

struct ReplayBufferInner {
    chunks: Vec<ReplayChunk>,
    total_bytes: usize,
    next_id: u64,
}

impl Default for ReplayBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplayBuffer {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ReplayBufferInner {
                chunks: Vec::new(),
                total_bytes: 0,
                next_id: 1,
            }),
        }
    }

    /// 追加一块输出, 裁剪旧数据保持在 `max_bytes` 内。
    ///
    /// 返回追加的块 (含 `id`); 若 `data` 为空或 trim 后为空则返回 `None`。
    /// 单块超过 `max_bytes` 时, 仅保留尾部 `max_bytes` (按 UTF-8 字符边界)。
    pub fn append(&self, data: &str, max_bytes: usize) -> Option<ReplayChunk> {
        if data.is_empty() {
            return None;
        }
        let trimmed = trim_chunk_to_max_bytes(data, max_bytes);
        if trimmed.is_empty() {
            return None;
        }
        let bytes = trimmed.len();
        let mut inner = self.inner.lock().unwrap();
        let chunk = ReplayChunk {
            id: inner.next_id,
            data: trimmed,
            bytes,
        };
        inner.next_id += 1;
        inner.chunks.push(chunk.clone());
        inner.total_bytes += bytes;

        while inner.total_bytes > max_bytes && inner.chunks.len() > 1 {
            let removed = inner.chunks.remove(0);
            inner.total_bytes = inner.total_bytes.saturating_sub(removed.bytes);
        }

        Some(chunk)
    }

    /// 返回 `last_seen_id` 之后的所有块。
    pub fn list_since(&self, last_seen_id: u64) -> Vec<ReplayChunk> {
        let inner = self.inner.lock().unwrap();
        inner
            .chunks
            .iter()
            .filter(|c| c.id > last_seen_id)
            .cloned()
            .collect()
    }

    /// 返回最新块 id, 空缓冲返回 0。
    #[allow(dead_code)]
    pub fn latest_id(&self) -> u64 {
        let inner = self.inner.lock().unwrap();
        inner.chunks.last().map(|c| c.id).unwrap_or(0)
    }
}

/// 将单块 trim 到 `max_bytes` (从尾部保留)。
///
/// 对齐 Node `trimTerminalOutputChunkToMaxBytes`: 从最后一个字符向前累计,
/// 超过 `max_bytes` 时停止。按字符 (char) 而非字节迭代, 避免拆开多字节 UTF-8。
fn trim_chunk_to_max_bytes(data: &str, max_bytes: usize) -> String {
    if data.len() <= max_bytes {
        return data.to_string();
    }
    // 从尾部逐字符累计, 保留尾部 max_bytes。
    let chars: Vec<char> = data.chars().collect();
    let mut kept: Vec<char> = Vec::new();
    let mut trimmed_bytes = 0usize;
    for &ch in chars.iter().rev() {
        let ch_bytes = ch.len_utf8();
        if trimmed_bytes + ch_bytes > max_bytes {
            break;
        }
        kept.push(ch);
        trimmed_bytes += ch_bytes;
    }
    kept.reverse();
    kept.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_returns_increasing_ids() {
        let buf = ReplayBuffer::new();
        let c1 = buf.append("hello", 1024).unwrap();
        let c2 = buf.append("world", 1024).unwrap();
        assert_eq!(c1.id, 1);
        assert_eq!(c2.id, 2);
        assert_eq!(c1.data, "hello");
        assert_eq!(c2.data, "world");
        assert_eq!(buf.latest_id(), 2);
    }

    #[test]
    fn append_empty_returns_none() {
        let buf = ReplayBuffer::new();
        assert!(buf.append("", 1024).is_none());
        assert_eq!(buf.latest_id(), 0);
    }

    #[test]
    fn list_since_filters_by_id() {
        let buf = ReplayBuffer::new();
        buf.append("a", 1024);
        buf.append("b", 1024);
        buf.append("c", 1024);
        let since = buf.list_since(1);
        assert_eq!(since.len(), 2);
        assert_eq!(since[0].data, "b");
        assert_eq!(since[1].data, "c");
    }

    #[test]
    fn list_since_zero_returns_all() {
        let buf = ReplayBuffer::new();
        buf.append("a", 1024);
        buf.append("b", 1024);
        let all = buf.list_since(0);
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn trims_old_chunks_when_over_budget() {
        let buf = ReplayBuffer::new();
        // 每块 5 字节, 总预算 12 → 第三块 (15) 时裁掉第一块 (保留 10)。
        buf.append("aaaaa", 12); // id=1, total=5
        buf.append("bbbbb", 12); // id=2, total=10
        buf.append("ccccc", 12); // id=3, total=15 → trim id1 → total=10
        let all = buf.list_since(0);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, 2);
        assert_eq!(all[1].id, 3);
    }

    #[test]
    fn large_chunk_trims_to_max_bytes_keeping_tail() {
        let buf = ReplayBuffer::new();
        let big = "a".repeat(100);
        let chunk = buf.append(&big, 20).unwrap();
        assert_eq!(chunk.bytes, 20);
        assert_eq!(chunk.data.len(), 20);
        assert_eq!(chunk.data, "a".repeat(20));
    }

    #[test]
    fn trim_preserves_multibyte_boundary() {
        // 每个 'é' 是 2 字节。max_bytes=5 → 保留 2 个 'é' (4 字节), 第 3 个 (6 字节) 超限。
        let data = "ééé"; // 6 字节
        let trimmed = trim_chunk_to_max_bytes(data, 5);
        assert_eq!(trimmed, "éé");
        assert_eq!(trimmed.len(), 4);
    }

    #[test]
    fn latest_id_empty_returns_zero() {
        let buf = ReplayBuffer::new();
        assert_eq!(buf.latest_id(), 0);
    }

    #[test]
    fn single_chunk_over_budget_keeps_trimmed_version() {
        // 单块超过预算时, while 循环条件 chunks.len() > 1 不成立, 不裁剪自身。
        let buf = ReplayBuffer::new();
        let big = "x".repeat(50);
        let chunk = buf.append(&big, 20).unwrap();
        // 单块 trim 到 20 (在 append 内部完成), 总 20 ≤ 20, 不触发 while。
        assert_eq!(chunk.data.len(), 20);
        assert_eq!(buf.list_since(0).len(), 1);
    }
}
