//! Web-push 发送 (VAPID + http-ece 加密)。
//!
//! 对应 Node `notifications/push-runtime.js` 的发送部分 (`sendPushToSubscription` +
//! `sendPushToAllUiSessions`)。用 `web-push` crate 发送, VAPID 密钥用 `p256` 生成。
//!
//! 可见性门控: web-push 订阅在 `requireNoSse` 模式下根据平台做可见性检查。
//! 移动端 PWA: 门控 `isAnyInteractiveClientVisible`; 其他: 门控 `isAnyUiVisible`。
//!
//! Windows 下本文件提供 no-op stub (webauthn-rs / web-push 在 Windows 上不可用,
//! 它们依赖 openssl-sys, 而 Windows MSVC 无系统 OpenSSL)。
//! 见 Cargo.toml `[target.'cfg(not(windows))'.dependencies]`。

// 注: use 语句在各分支的 mod inner 内各自声明, 此处不需要文件级 use。

// =========================================================================
// 分支 A: 非 Windows — 完整实现 (web-push 发送)
// =========================================================================
#[cfg(not(target_os = "windows"))]
mod inner {
    use std::sync::Arc;

    use serde_json::Value;
    use web_push::{
        ContentEncoding, HyperWebPushClient, SubscriptionInfo, VapidSignatureBuilder,
        WebPushClient, WebPushMessageBuilder,
    };

    use super::super::push_store::{PushStore, PushSubscription};
    use super::super::relay_key;
    use super::super::types::is_mobile_platform;

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
        fn resolve_vapid_subject(&self) -> String {
            if let Ok(subj) = std::env::var("OPENCHAMBER_VAPID_SUBJECT") {
                let trimmed = subj.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }

            if let Ok(origin) = std::env::var("OPENCHAMBER_PUBLIC_ORIGIN") {
                let trimmed = origin.trim();
                if !trimmed.is_empty() {
                    if is_loopback_http_origin(trimmed) {
                        return "mailto:openchamber@localhost".to_string();
                    }
                    return trimmed.to_string();
                }
            }

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
        async fn send_to_subscription(&self, sub: &PushSubscription, payload: &Value) {
            let body = match serde_json::to_string(payload) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "[Push] failed to serialize payload");
                    return;
                }
            };

            let (_public_key, private_key) = relay_key::get_or_create_vapid_keys();

            let partial = match VapidSignatureBuilder::from_base64_no_sub(&private_key) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "[Push] invalid VAPID private key");
                    return;
                }
            };

            let subscription_info = SubscriptionInfo::new(&sub.endpoint, &sub.p256dh, &sub.auth);

            let mut sig_builder = partial.add_sub_info(&subscription_info);
            sig_builder.add_claim("sub", self.resolve_vapid_subject());
            let vapid_sig = match sig_builder.build() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "[Push] failed to build VAPID signature");
                    return;
                }
            };

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
        pub async fn send_to_all_ui_sessions(&self, payload: &Value, options: &PushSendOptions) {
            let subscriptions = self.push_store.read_all_subscriptions();
            if subscriptions.is_empty() {
                return;
            }

            let require_no_sse = options.require_no_sse;

            for sub in subscriptions {
                if require_no_sse {
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

    fn is_loopback_http_origin(value: &str) -> bool {
        value.starts_with("http://localhost")
            || value.starts_with("http://127.0.0.1")
            || value.starts_with("http://[::1]")
    }
}

// =========================================================================
// 分支 B: Windows — no-op stub (web-push 在 Windows 被禁用)
// =========================================================================
#[cfg(target_os = "windows")]
mod inner {
    use std::sync::Arc;

    use serde_json::Value;

    use super::super::push_store::PushStore;

    /// No-op 版 PushSendRuntime (web-push 被禁用)。
    pub struct PushSendRuntime {
        _push_store: Arc<PushStore>,
    }

    /// 发送选项 (stub 下忽略)。
    #[derive(Debug, Clone, Default)]
    pub struct PushSendOptions {
        pub require_no_sse: bool,
    }

    impl PushSendRuntime {
        pub fn new(push_store: Arc<PushStore>, _http_client: reqwest::Client) -> Self {
            Self {
                _push_store: push_store,
            }
        }

        /// No-op: web-push 已禁用。
        pub async fn send_to_all_ui_sessions(
            &self,
            _payload: &Value,
            _options: &PushSendOptions,
        ) {
            // native push (web-push) is disabled via feature gate
        }
    }
}

pub use inner::{PushSendOptions, PushSendRuntime};
