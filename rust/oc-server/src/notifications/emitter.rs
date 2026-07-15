//! SSE 客户端池 + broadcast + desktop notify。
//!
//! 对应 Node `notifications/emitter-runtime.js` (`createNotificationEmitterRuntime`)。
//!
//! SSE 客户端用 `broadcast::Sender<Bytes>` 扇出, 每个连接 subscribe 一个 receiver。
//! desktop notify 通过可选回调 (未来 Tauri 注入)。

use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use serde_json::{json, Value};
use tokio::sync::broadcast;

/// desktop notify 回调类型 (对应 Electron `onDesktopNotification`)。
pub type DesktopNotifyCallback = Arc<dyn Fn(Value) + Send + Sync>;

/// SSE 通知 emitter — 广播通知事件到所有 SSE 客户端 + desktop shell。
pub struct NotificationEmitter {
    /// SSE 广播 channel: 每个 SSE 连接 subscribe 一个 receiver。
    clients_tx: broadcast::Sender<Bytes>,
    /// desktop notify 回调 (可选, Tauri/Electron 注入)。
    desktop_notify: Mutex<Option<DesktopNotifyCallback>>,
}

impl NotificationEmitter {
    pub fn new() -> Self {
        let (clients_tx, _) = broadcast::channel(256);
        Self {
            clients_tx,
            desktop_notify: Mutex::new(None),
        }
    }

    /// 设置 desktop notify 回调。
    ///
    /// 对应 Node `setOnDesktopNotification`。
    pub fn set_desktop_notify_callback(&self, callback: Option<DesktopNotifyCallback>) {
        let mut guard = self.desktop_notify.lock().unwrap();
        *guard = callback;
    }

    /// 订阅 SSE 流 (返回 broadcast receiver)。
    pub fn subscribe_sse(&self) -> broadcast::Receiver<Bytes> {
        self.clients_tx.subscribe()
    }

    /// 写 SSE 事件到所有连接的客户端。
    ///
    /// 对应 Node `writeSseEvent(res, payload)`。
    fn write_sse_event(&self, payload: &Value) {
        let data = format!("data: {}\n\n", payload);
        let bytes = Bytes::from(data);
        // send 失败 = 无接收者, 忽略
        let _ = self.clients_tx.send(bytes);
    }

    /// 发送 desktop 通知。
    ///
    /// 对应 Node `emitDesktopNotification`。有回调则调回调, 无则 stdout fallback。
    /// 返回 true 如果已投递。
    pub fn emit_desktop_notification(&self, payload: &Value) -> bool {
        let guard = self.desktop_notify.lock().unwrap();
        if let Some(ref callback) = *guard {
            // 回调可能 panic — 我们不希望 panic 跨边界传播
            // 但 callback 本身是闭包, 实现里不应 panic
            callback(payload.clone());
            return true;
        }
        drop(guard);
        // stdout fallback (对应 Node 的 process.stdout.write)
        // 当前 Rust 端不解析 stdout, 这里不做任何事
        false
    }

    /// 广播 UI 通知。
    ///
    /// 对应 Node `broadcastUiNotification`。包装为 `{type:'openchamber:notification', properties:{...}}`
    /// + `desktopNotificationDelivered` + `desktopStdoutActive` 标记, 然后 SSE 广播。
    pub fn broadcast_ui_notification(&self, payload: &Value, desktop_delivered: bool) {
        let mut properties = match payload.as_object() {
            Some(obj) => obj.clone(),
            None => return,
        };
        properties.insert(
            "desktopNotificationDelivered".to_string(),
            Value::Bool(desktop_delivered),
        );
        properties.insert("desktopStdoutActive".to_string(), Value::Bool(false));

        let synthetic = json!({
            "type": "openchamber:notification",
            "properties": properties,
        });

        self.write_sse_event(&synthetic);
    }
}

impl Default for NotificationEmitter {
    fn default() -> Self {
        Self::new()
    }
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_writes_to_subscribers() {
        let emitter = NotificationEmitter::new();
        let mut rx = emitter.subscribe_sse();

        let payload = json!({ "type": "test", "properties": { "msg": "hello" } });
        emitter.write_sse_event(&payload);

        let received = rx.try_recv().unwrap();
        let text = String::from_utf8(received.to_vec()).unwrap();
        assert!(text.starts_with("data: "));
        assert!(text.contains("hello"));
        assert!(text.ends_with("\n\n"));
    }

    #[test]
    fn broadcast_ui_notification_wraps_payload() {
        let emitter = NotificationEmitter::new();
        let mut rx = emitter.subscribe_sse();

        let payload = json!({ "title": "Test", "body": "Hello" });
        emitter.broadcast_ui_notification(&payload, true);

        let received = rx.try_recv().unwrap();
        let text = String::from_utf8(received.to_vec()).unwrap();
        assert!(text.contains("openchamber:notification"));
        assert!(text.contains("desktopNotificationDelivered"));
        assert!(text.contains("Test"));
    }

    #[test]
    fn emit_desktop_no_callback_returns_false() {
        let emitter = NotificationEmitter::new();
        assert!(!emitter.emit_desktop_notification(&json!({})));
    }

    #[test]
    fn emit_desktop_with_callback() {
        let emitter = NotificationEmitter::new();
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_clone = called.clone();
        emitter.set_desktop_notify_callback(Some(Arc::new(move |_payload| {
            called_clone.store(true, std::sync::atomic::Ordering::Relaxed);
        })));
        assert!(emitter.emit_desktop_notification(&json!({ "title": "X" })));
        assert!(called.load(std::sync::atomic::Ordering::Relaxed));
    }
}
