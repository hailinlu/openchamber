//! Models.dev catalog 缓存 + inflight dedup + stale fallback。
//!
//! 对应 Node `opencode/models-metadata.js` (61 行):
//!   - getModelsMetadata({ url, ttlMs, timeoutMs }) → { metadata, fromCache, stale? }
//!
//! 行为契约:
//!   1. 缓存命中(within TTL) → 返回 { metadata, fromCache: true }
//!   2. 缓存 miss → 触发 fetch,inflight dedup(并发 5 个调用只发 1 个请求)
//!   3. fetch 失败 + 有 cache → 返回 stale fallback { metadata, fromCache: true, stale: true }
//!   4. fetch 失败 + 无 cache → 传播错误

#![allow(dead_code)]
#![allow(unused_imports)]

//!   5. 完全 fresh fetch → { metadata, fromCache: false }
//!
//! 测试用 `tokio::net::TcpListener` 手写 mock HTTP server,不引入 mock crate。
//!
//! Inflight dedup: 用 `Arc<OnceCell<Result<Value, Error>>>` + `tokio::sync::Mutex`
//! 保护 spawn handle,所有并发调用 await 同一 handle。

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use once_cell::sync::OnceCell as StaticOnceCell;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio::time::timeout;

use oc_core::Error;

/// models.dev catalog 默认 URL。
pub const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";

/// 缓存 TTL: 10 分钟(对齐 Node `DEFAULT_TTL_MS`)。
pub const DEFAULT_TTL_MS: u64 = 10 * 60 * 1000;

/// 单次 fetch 超时: 8 秒(对齐 Node `DEFAULT_TIMEOUT_MS`)。
pub const DEFAULT_TIMEOUT_MS: u64 = 8000;

/// `get_models_metadata` 返回值。
#[derive(Debug, Clone)]
pub struct ModelsMetadata {
    /// catalog 内容(models.dev 的 JSON)。
    pub metadata: Value,
    /// 是否来自缓存(true = 缓存内,false = 本次 fresh fetch)。
    pub from_cache: bool,
    /// 是否 stale(只在 fetch 失败 + 返回旧 cache 时为 true)。
    pub stale: bool,
}

/// 进程内缓存状态。
struct MetadataCache {
    metadata: Value,
    cached_at: u64,
}

/// 全局状态: cache + inflight single-flight handle(测试可重置)。
struct GlobalState {
    cache: Mutex<Option<MetadataCache>>,
    inflight: Mutex<Option<Arc<InflightHandle>>>,
}

/// Inflight 共享句柄:可被多个 awaiter 拿到的同一份 Result。
struct InflightHandle {
    result: tokio::sync::OnceCell<Result<Value, Error>>,
}

impl InflightHandle {
    fn new() -> Self {
        Self {
            result: tokio::sync::OnceCell::new(),
        }
    }

    /// 触发 fetch 并把结果设到共享 cell。
    async fn drive(self: Arc<Self>, url: String, timeout_ms: u64) {
        let outcome = fetch_catalog(&url, timeout_ms).await;
        let _ = self.result.set(outcome);
    }

    /// 等待已触发的 fetch 完成。
    async fn wait_result(&self) -> Option<&Result<Value, Error>> {
        self.result.get()
    }
}

impl GlobalState {
    fn new() -> Self {
        Self {
            cache: Mutex::new(None),
            inflight: Mutex::new(None),
        }
    }
}

static STATE: StaticOnceCell<Arc<GlobalState>> = StaticOnceCell::new();

fn state() -> &'static Arc<GlobalState> {
    STATE.get_or_init(|| Arc::new(GlobalState::new()))
}

/// 单元测试用: 重置全局缓存和 inflight。
#[cfg(test)]
pub async fn reset_for_tests() {
    if let Some(s) = STATE.get() {
        let mut cache = s.cache.lock().await;
        *cache = None;
        let mut inflight = s.inflight.lock().await;
        *inflight = None;
    }
}

/// 当前时间(毫秒,UNIX epoch)。
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 实际执行 fetch(无 inflight dedup),由 `get_models_metadata` 调用。
async fn fetch_catalog(url: &str, timeout_ms: u64) -> Result<Value, Error> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .map_err(|e| Error::Internal(format!("build http client: {}", e)))?;

    let response = timeout(Duration::from_millis(timeout_ms), client.get(url).send())
        .await
        .map_err(|_| Error::Internal(format!("models.dev fetch timeout after {}ms", timeout_ms)))?
        .map_err(|e| Error::Internal(format!("models.dev fetch: {}", e)))?;

    if !response.status().is_success() {
        return Err(Error::Internal(format!(
            "models.dev responded with status {}",
            response.status()
        )));
    }

    let metadata = response
        .json::<Value>()
        .await
        .map_err(|e| Error::Internal(format!("models.dev parse: {}", e)))?;

    if !metadata.is_object() {
        return Err(Error::Internal("models.dev returned an unexpected payload".to_string()));
    }

    Ok(metadata)
}

/// 读取缓存,若在 TTL 内则克隆 metadata 返回。
async fn try_cache(ttl_ms: u64) -> Option<Value> {
    let cell = state().cache.lock().await;
    let cache = cell.as_ref()?;
    if now_ms().saturating_sub(cache.cached_at) < ttl_ms {
        Some(cache.metadata.clone())
    } else {
        None
    }
}

/// 获取 models.dev catalog,带 TTL 缓存 + inflight dedup + stale fallback。
pub async fn get_models_metadata(
    url: Option<&str>,
    ttl_ms: Option<u64>,
    timeout_ms: Option<u64>,
) -> Result<ModelsMetadata, Error> {
    let url = url.unwrap_or(MODELS_DEV_API_URL);
    let ttl = ttl_ms.unwrap_or(DEFAULT_TTL_MS);
    let fetch_timeout = timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);

    // 1. 缓存命中?
    if let Some(metadata) = try_cache(ttl).await {
        return Ok(ModelsMetadata { metadata, from_cache: true, stale: false });
    }

    // 2. inflight dedup: 同一时刻只触发一次 fetch
    let handle = {
        let mut guard = state().inflight.lock().await;
        if let Some(existing) = guard.as_ref() {
            existing.clone()
        } else {
            let handle = Arc::new(InflightHandle::new());
            let drive_handle = handle.clone();
            let url_owned = url.to_string();
            tokio::spawn(async move {
                drive_handle.drive(url_owned, fetch_timeout).await;
            });
            *guard = Some(handle.clone());
            handle
        }
    };

    // 等待 inflight 完成(spawn 后结果会被 set 进 OnceCell;
    // 调用方循环检查直到拿到结果,这里用 busy-poll 1ms)。
    let fetch_result: Result<Value, Error> = loop {
        if let Some(r) = handle.wait_result().await {
            break match r {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(clone_error(e)),
            };
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    };

    match fetch_result {
        Ok(metadata) => {
            // 写入缓存
            let mut cache = state().cache.lock().await;
            *cache = Some(MetadataCache {
                metadata: metadata.clone(),
                cached_at: now_ms(),
            });
            // 清理 inflight,下次 fetch 可重新触发
            let mut inflight = state().inflight.lock().await;
            *inflight = None;
            Ok(ModelsMetadata { metadata, from_cache: false, stale: false })
        }
        Err(e) => {
            // fetch 失败: stale fallback?
            let cache = state().cache.lock().await;
            if let Some(c) = cache.as_ref() {
                Ok(ModelsMetadata {
                    metadata: c.metadata.clone(),
                    from_cache: true,
                    stale: true,
                })
            } else {
                Err(e)
            }
        }
    }
}

/// 克隆一个错误以便同时满足 `?` 和 stale fallback 分支(保留错误信息)。
fn clone_error(e: &Error) -> Error {
    match e {
        Error::Internal(m) => Error::Internal(m.clone()),
        other => Error::Internal(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU16, Ordering};

    /// 序列化所有 models_metadata 测试,因为它们共享全局 STATE static。
    /// 用 tokio::sync::Mutex 让 guard 可以跨 .await 持有 (避免 clippy::await_holding_lock)。
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// 在 127.0.0.1:0 启动 mock HTTP server,返回固定 JSON。返回 (port, request_count, handle)。
    async fn start_mock_server(
        body: Value,
        status: u16,
    ) -> (u16, Arc<AtomicU16>, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let request_count = Arc::new(AtomicU16::new(0));
        let count_clone = request_count.clone();
        let body_str = body.to_string();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let count = count_clone.clone();
                let body = body_str.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    count.fetch_add(1, Ordering::SeqCst);
                    let response = format!(
                        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        status,
                        if status == 200 { "OK" } else { "ERROR" },
                        body.len(),
                        body,
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });

        (port, request_count, handle)
    }

    fn url_for_port(port: u16) -> String {
        format!("http://127.0.0.1:{}/api.json", port)
    }

    #[tokio::test]
    async fn fresh_fetch_succeeds_and_caches() {
        let _guard = SERIAL.lock().await;
        reset_for_tests().await;
        let (port, count, h) = start_mock_server(json!({"openai": {"gpt-4o": {}}}), 200).await;
        let url = url_for_port(port);

        let result = get_models_metadata(Some(&url), Some(60_000), Some(2000)).await.unwrap();
        assert!(!result.from_cache);
        assert!(!result.stale);
        assert_eq!(result.metadata["openai"]["gpt-4o"], json!({}));
        assert_eq!(count.load(Ordering::SeqCst), 1);

        h.abort();
    }

    #[tokio::test]
    async fn cache_hit_within_ttl_skips_fetch() {
        let _guard = SERIAL.lock().await;
        reset_for_tests().await;
        let (port, count, h) = start_mock_server(json!({"x": 1}), 200).await;
        let url = url_for_port(port);

        // 第一次: miss
        let r1 = get_models_metadata(Some(&url), Some(60_000), Some(2000)).await.unwrap();
        assert!(!r1.from_cache);

        // 第二次: hit
        let r2 = get_models_metadata(Some(&url), Some(60_000), Some(2000)).await.unwrap();
        assert!(r2.from_cache);
        assert!(!r2.stale);
        assert_eq!(count.load(Ordering::SeqCst), 1, "second call must not fetch");

        h.abort();
    }

    #[tokio::test]
    async fn stale_fallback_when_fetch_fails_after_successful_first() {
        let _guard = SERIAL.lock().await;
        reset_for_tests().await;
        let (port1, count1, h1) = start_mock_server(json!({"v": 1}), 200).await;
        let url1 = url_for_port(port1);

        // 第一次: 成功 + 写入缓存
        let r1 = get_models_metadata(Some(&url1), Some(60_000), Some(2000)).await.unwrap();
        assert!(!r1.from_cache);
        assert_eq!(count1.load(Ordering::SeqCst), 1);

        // 模拟 TTL 过期(用很短的 TTL 跑一次后立即用 0 TTL 再来)
        let (port2, _count2, h2) = start_mock_server(json!({}), 500).await;
        let url2 = url_for_port(port2);
        let r2 = get_models_metadata(Some(&url2), Some(0), Some(2000)).await.unwrap();
        assert!(r2.from_cache);
        assert!(r2.stale);
        assert_eq!(r2.metadata, json!({"v": 1}));

        h1.abort();
        h2.abort();
    }
}
