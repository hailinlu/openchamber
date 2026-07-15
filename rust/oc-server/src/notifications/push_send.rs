//! Web-push 发送 (VAPID + http-ece 加密)。
//!
//! 对应 Node `notifications/push-runtime.js` 的发送部分 (`sendPushToSubscription` +
//! `sendPushToAllUiSessions`)。用 `web-push` crate 发送, VAPID 密钥用 `p256` 生成。
//!
//! 可见性门控: web-push 订阅在 `requireNoSse` 模式下根据平台做可见性检查。
//! 移动端 PWA: 门控 `isAnyInteractiveClientVisible`; 其他: 门控 `isAnyUiVisible`。

use std::sync::Arc;

use serde_json::Value;
use web_push::{
    ContentEncoding, HyperWebPushClient, SubscriptionInfo, VapidSignatureBuilder,
    WebPushClient, WebPushMessageBuilder,
};

use super::push_store::{PushStore, PushSubscription};
use super::relay_key;
use super::types::is_mobile_platform;

/// Web-push 发送运行时。
pub struct PushSendRuntime {
    push_store: Arc<PushStore>,
    #[allow(dead_code)]
    http_client: reqwest::Client,
}

/// 发送选项。
#[derive(Debug, Clone, Default)]
pub struct PushSendOptions {
    /// `requireNoSse=true`: 根据可见性门控是否发送。
    pub require_no_sse: bool,
}

impl PushSendRuntime {
    pub fn new(push_store: Arc<PushStore>, http_client: reqwest::Client) -> Self {
        Self {
            push_store,
            http_client,
        }
    }

    /// 解析 VAPID subject (mailto: 或 https:// origin)。
    ///
    /// 对应 Node `resolveVapidSubject`。
    fn resolve_vapid_subject(&self) -> String {
        // 1. OPENCHAMBER_VAPID_SUBJECT env
        if let Ok(subj) = std::env::var("OPENCHAMBER_VAPID_SUBJECT") {
            let trimmed = subj.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }

        // 2. OPENCHAMBER_PUBLIC_ORIGIN env (loopback → mailto:)
        if let Ok(origin) = std::env::var("OPENCHAMBER_PUBLIC_ORIGIN") {
            let trimmed = origin.trim();
            if !trimmed.is_empty() {
                if is_loopback_http_origin(trimmed) {
                    return "mailto:openchamber@localhost".to_string();
                }
                return trimmed.to_string();
            }
        }

        // 3. settings.publicOrigin
        let settings = crate::github::settings::read_settings();
        if let Some(stored) = settings.get("publicOrigin").and_then(|v| v.as_str()) {
            let trimmed = stored.trim();
            if !trimmed.is_empty() {
                if is_loopback_http_origin(trimmed) {
                    return "mailto:openchamber@localhost".to_string();
                }
                return trimmed.to_string();
            }
        }

        "mailto:openchamber@localhost".to_string()
    }

    /// 发送 push 到单个订阅。
    ///
    /// 对应 Node `sendPushToSubscription`。410/404 → 删除死订阅。
    async fn send_to_subscription(&self, sub: &PushSubscription, payload: &Value) {
        let body = match serde_json::to_string(payload) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "[Push] failed to serialize payload");
                return;
            }
        };

        // 获取 VAPID 密钥
        let (_public_key, private_key) = relay_key::get_or_create_vapid_keys();

        // 构建 VAPID 签名 builder (from_base64_no_sub 接受 32-byte scalar base64url)
        let partial = match VapidSignatureBuilder::from_base64_no_sub(&private_key) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "[Push] invalid VAPID private key");
                return;
            }
        };

        // 构建订阅信息
        let subscription_info = SubscriptionInfo::new(&sub.endpoint, &sub.p256dh, &sub.auth);

        // 添加 sub_info + subject claim + build
        let mut sig_builder = partial.add_sub_info(&subscription_info);
        sig_builder.add_claim("sub", self.resolve_vapid_subject());
        let vapid_sig = match sig_builder.build() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "[Push] failed to build VAPID signature");
                return;
            }
        };

        // 构建 message
        let mut builder = WebPushMessageBuilder::new(&subscription_info);
        builder.set_payload(ContentEncoding::Aes128Gcm, body.as_bytes());
        builder.set_vapid_signature(vapid_sig);

        let message = match builder.build() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "[Push] failed to build message");
                return;
            }
        };

        // 发送
        let client = HyperWebPushClient::new();

        match client.send(message).await {
            Ok(_) => {}
            Err(e) => {
                match e {
                    web_push::WebPushError::EndpointNotValid(_) | web_push::WebPushError::EndpointNotFound(_) => {
                        tracing::debug!(endpoint = %sub.endpoint, "[Push] removing dead subscription");
                        self.push_store.remove_from_all_sessions(&sub.endpoint);
                    }
                    _ => {
                        tracing::warn!(error = %e, "[Push] failed to send notification");
                    }
                }
            }
        }
    }

    /// 发送 push 到所有 UI session。
    ///
    /// 对应 Node `sendPushToAllUiSessions`。可见性门控。
    pub async fn send_to_all_ui_sessions(&self, payload: &Value, options: &PushSendOptions) {
        let subscriptions = self.push_store.read_all_subscriptions();
        if subscriptions.is_empty() {
            return;
        }

        let require_no_sse = options.require_no_sse;

        for sub in subscriptions {
            if require_no_sse {
                // 移动端 PWA: 门控 interactive client; 其他: 门控 any UI
                let suppressed = if is_mobile_platform(sub.platform.as_deref()) {
                    self.push_store.is_any_interactive_client_visible()
                } else {
                    self.push_store.is_any_ui_visible()
                };
                if suppressed {
                    continue;
                }
            }
            self.send_to_subscription(&sub, payload).await;
        }
    }
}

/// 判断是否为 loopback HTTP origin (localhost/127.0.0.1/::1)。
fn is_loopback_http_origin(value: &str) -> bool {
    value.starts_with("http://localhost")
        || value.starts_with("http://127.0.0.1")
        || value.starts_with("http://[::1]")
}
