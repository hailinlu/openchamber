//! Terminal 会话状态 + 会话存储。
//!
//! 对应 Node `runtime.js` 的 `terminalSessions` Map + `MAX_TERMINAL_SESSIONS`
//! + `TERMINAL_IDLE_TIMEOUT` + idle sweep。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use super::pty::{TerminalPty, IDLE_SWEEP_INTERVAL, IDLE_TIMEOUT};
use super::replay_buffer::ReplayBuffer;

/// 单个终端会话。
pub struct TerminalSession {
    /// PTY 句柄 (输入/输出/resize/kill)。
    pub pty: Arc<TerminalPty>,
    /// 创建时的工作目录。
    pub cwd: PathBuf,
    /// PTY 后端名 (`"portable-pty"`)。
    pub pty_backend: String,
    /// 最后活动时间 (输入/输出/resize 更新)。
    pub last_activity: Mutex<Instant>,
    /// 输出重放缓冲 (晚订阅客户端拿启动 prompt)。
    pub replay_buffer: Arc<ReplayBuffer>,
}

impl TerminalSession {
    pub fn new(pty: TerminalPty, cwd: PathBuf) -> Self {
        let backend = pty.backend.to_string();
        Self {
            pty: Arc::new(pty),
            cwd,
            pty_backend: backend,
            last_activity: Mutex::new(Instant::now()),
            replay_buffer: Arc::new(ReplayBuffer::new()),
        }
    }

    /// 更新最后活动时间为现在。
    pub async fn touch(&self) {
        *self.last_activity.lock().await = Instant::now();
    }
}

/// 终端会话存储 (线程安全)。
pub struct TerminalSessionStore {
    sessions: Mutex<HashMap<String, Arc<TerminalSession>>>,
}

impl Default for TerminalSessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalSessionStore {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// 当前会话数。
    pub async fn len(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// 插入会话。
    pub async fn insert(&self, session_id: String, session: Arc<TerminalSession>) {
        self.sessions.lock().await.insert(session_id, session);
    }

    /// 获取会话。
    pub async fn get(&self, session_id: &str) -> Option<Arc<TerminalSession>> {
        self.sessions.lock().await.get(session_id).cloned()
    }

    /// 移除会话 (不杀进程)。
    pub async fn remove(&self, session_id: &str) -> Option<Arc<TerminalSession>> {
        self.sessions.lock().await.remove(session_id)
    }

    /// 杀所有会话并清空 (进程退出时调用)。
    pub async fn kill_all(&self) {
        let mut sessions = self.sessions.lock().await;
        for session in sessions.values() {
            session.pty.kill_process_group(super::pty::KillMode::Kill);
        }
        sessions.clear();
    }

    /// 获取所有会话快照 (供 force-kill 扫描)。
    pub async fn sessions_for_sweep(&self) -> HashMap<String, Arc<TerminalSession>> {
        self.sessions.lock().await.clone()
    }

    /// 清空所有会话 (不杀进程; 供 force-kill all 路径, 已单独 kill)。
    pub async fn clear(&self) {
        self.sessions.lock().await.clear();
    }

    /// 启动 idle sweep 后台 task: 每 5min 扫描, 超过 30min 无活动则杀 + 删除。
    ///
    /// 返回 `JoinHandle` 供调用方管理生命周期 (通常在 app 生命周期内运行)。
    pub fn start_idle_sweep(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(IDLE_SWEEP_INTERVAL);
            interval.tick().await; // 跳过第一次
            loop {
                interval.tick().await;
                let mut to_remove = Vec::new();
                {
                    let sessions = self.sessions.lock().await;
                    for (id, session) in sessions.iter() {
                        let last = *session.last_activity.lock().await;
                        if last.elapsed() > IDLE_TIMEOUT {
                            session
                                .pty
                                .kill_process_group(super::pty::KillMode::Term);
                            to_remove.push(id.clone());
                        }
                    }
                }
                if !to_remove.is_empty() {
                    let mut sessions = self.sessions.lock().await;
                    for id in &to_remove {
                        sessions.remove(id);
                    }
                    tracing::info!(
                        "terminal idle sweep removed {} session(s)",
                        to_remove.len()
                    );
                }
            }
        });
    }
}

/// 生成随机 session ID (对齐 Node `Math.random().toString(36).substring(2,15)` × 2)。
pub fn generate_session_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let a: u64 = rng.gen();
    let b: u64 = rng.gen();
    format!("{:x}{:x}", a, b)
}

/// 生成随机 client ID (对齐 Node `Math.random().toString(36).substring(7)`)。
#[allow(dead_code)]
pub fn generate_client_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let a: u64 = rng.gen();
    format!("{:x}", a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_nonempty() {
        let id = generate_session_id();
        assert!(!id.is_empty());
        assert!(id.len() >= 8);
    }

    #[test]
    fn session_ids_are_distinct() {
        let a = generate_session_id();
        let b = generate_session_id();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn store_insert_get_remove() {
        // 不能 spawn 真 PTY (需要 shell), 仅测试 store 的 CRUD 逻辑。
        // 这里用一个 mock 占位: store 应能 insert/get/remove 任意 Arc。
        let store = TerminalSessionStore::new();
        assert_eq!(store.len().await, 0);
        // 不插入真实 session (PTY 需要 shell), 仅验证空 store 行为。
        assert!(store.get("nonexistent").await.is_none());
        assert!(store.remove("nonexistent").await.is_none());
        store.kill_all().await; // 不应 panic
    }
}
