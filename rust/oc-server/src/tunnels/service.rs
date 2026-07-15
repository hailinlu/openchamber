//! TunnelService 编排。
//!
//! 移植自 `packages/web/server/lib/tunnels/index.js` (179 行)。
//!
//! 职责:
//!   - Mutex-locked start (防止并发 start 孤儿进程)
//!   - stop / check_availability / get_public_url / get_provider_metadata
//!   - resolve_active_mode / resolve_active_provider

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::tunnels::install_help::get_tunnel_dependency_install_info;
use crate::tunnels::providers::{self, StartContext, TunnelController};
use crate::tunnels::types::{
    normalize_tunnel_start_request, validate_tunnel_start_request, TunnelServiceError,
    NormalizedTunnelStartRequest, TUNNEL_MODE_QUICK, TUNNEL_PROVIDER_CLOUDFLARE,
};

/// 隧道启动结果。
pub struct StartResult {
    pub public_url: String,
    #[allow(dead_code)]
    pub request: NormalizedTunnelStartRequest,
    pub active_mode: String,
    pub provider: String,
    pub provider_metadata: Option<Value>,
}

/// 隧道运行时状态 (共享, Mutex 保护)。
pub struct TunnelRuntimeState {
    inner: Mutex<TunnelRuntimeInner>,
}

struct TunnelRuntimeInner {
    active_controller: Option<TunnelController>,
    runtime_managed_remote_hostname: String,
    runtime_managed_remote_token: String,
}

impl TunnelRuntimeState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(TunnelRuntimeInner {
                active_controller: None,
                runtime_managed_remote_hostname: String::new(),
                runtime_managed_remote_token: String::new(),
            }),
        }
    }

    pub async fn get_controller_mode(&self) -> Option<String> {
        let inner = self.inner.lock().await;
        inner
            .active_controller
            .as_ref()
            .map(|c| c.mode.clone())
    }

    pub async fn get_controller_provider(&self) -> Option<String> {
        let inner = self.inner.lock().await;
        inner
            .active_controller
            .as_ref()
            .and_then(|c| c.provider.clone())
    }

    pub async fn get_public_url(&self) -> Option<String> {
        let inner = self.inner.lock().await;
        inner
            .active_controller
            .as_ref()
            .and_then(|c| c.public_url.clone())
    }

    pub async fn get_provider_metadata(&self) -> Option<Value> {
        let inner = self.inner.lock().await;
        inner.active_controller.as_ref().and_then(|c| {
            let config_path = c.get_effective_config_path();
            let resolved_hostname = c.get_resolved_hostname();
            if config_path.is_none() && resolved_hostname.is_none() {
                None
            } else {
                Some(serde_json::json!({
                    "configPath": config_path,
                    "resolvedHostname": resolved_hostname,
                }))
            }
        })
    }

    pub async fn set_controller(&self, controller: Option<TunnelController>) {
        let mut inner = self.inner.lock().await;
        inner.active_controller = controller;
    }

    pub async fn stop_controller(&self) -> bool {
        let mut inner = self.inner.lock().await;
        if let Some(mut controller) = inner.active_controller.take() {
            controller.stop();
            true
        } else {
            false
        }
    }

    #[allow(dead_code)]
    pub async fn take_controller(&self) -> Option<TunnelController> {
        let mut inner = self.inner.lock().await;
        inner.active_controller.take()
    }

    pub async fn get_runtime_managed_remote_hostname(&self) -> String {
        self.inner.lock().await.runtime_managed_remote_hostname.clone()
    }

    pub async fn set_runtime_managed_remote_hostname(&self, hostname: &str) {
        self.inner.lock().await.runtime_managed_remote_hostname = hostname.to_string();
    }

    pub async fn get_runtime_managed_remote_token(&self) -> String {
        self.inner.lock().await.runtime_managed_remote_token.clone()
    }

    pub async fn set_runtime_managed_remote_token(&self, token: &str) {
        self.inner.lock().await.runtime_managed_remote_token = token.to_string();
    }

    /// 获取 controller 的 public_url (不解析 provider)。
    pub async fn controller_public_url(&self) -> Option<String> {
        let inner = self.inner.lock().await;
        inner
            .active_controller
            .as_ref()
            .and_then(|c| c.public_url.clone())
    }
}

impl Default for TunnelRuntimeState {
    fn default() -> Self {
        Self::new()
    }
}

/// TunnelService 编排。
pub struct TunnelService {
    runtime: Arc<TunnelRuntimeState>,
    start_lock: Mutex<()>,
    get_active_port: Arc<dyn Fn() -> Option<u16> + Send + Sync>,
    on_quick_tunnel_warning: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl TunnelService {
    pub fn new(runtime: Arc<TunnelRuntimeState>) -> Self {
        Self {
            runtime,
            start_lock: Mutex::new(()),
            get_active_port: Arc::new(|| None),
            on_quick_tunnel_warning: None,
        }
    }

    #[allow(dead_code)]
    pub fn with_active_port<F>(mut self, f: F) -> Self
    where
        F: Fn() -> Option<u16> + Send + Sync + 'static,
    {
        self.get_active_port = Arc::new(f);
        self
    }

    #[allow(dead_code)]
    pub fn with_quick_tunnel_warning<F>(mut self, f: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.on_quick_tunnel_warning = Some(Arc::new(f));
        self
    }

    /// 获取 runtime state 引用。
    #[allow(dead_code)]
    pub fn runtime(&self) -> &Arc<TunnelRuntimeState> {
        &self.runtime
    }

    /// 解析活动 tunnel mode。
    pub async fn resolve_active_mode(&self) -> Option<String> {
        self.runtime.get_controller_mode().await
    }

    /// 解析活动 tunnel provider。
    pub async fn resolve_active_provider(&self) -> Option<String> {
        self.runtime.get_controller_provider().await
    }

    /// 获取 public URL。
    pub async fn get_public_url(&self) -> Option<String> {
        self.runtime.get_public_url().await
    }

    /// 获取 provider metadata。
    pub async fn get_provider_metadata(&self) -> Option<Value> {
        self.runtime.get_provider_metadata().await
    }

    /// 检查 provider 可用性。
    pub async fn check_availability(&self, provider_id: &str) -> Result<Value, TunnelServiceError> {
        providers::check_availability(provider_id).await
    }

    /// 停止活动隧道。
    #[allow(dead_code)]
    pub async fn stop(&self) -> bool {
        self.runtime.stop_controller().await
    }

    /// 启动隧道 (Mutex-locked)。
    pub async fn start(
        &self,
        raw_request: &Value,
    ) -> Result<StartResult, TunnelServiceError> {
        let _guard = self.start_lock.lock().await;

        let request = normalize_tunnel_start_request(raw_request);

        // 验证 capabilities
        let capabilities = match request.provider.as_str() {
            "ngrok" => providers::ngrok::capabilities(),
            _ => providers::cloudflare::capabilities(),
        };
        validate_tunnel_start_request(&request, &capabilities)?;

        // 检查现有 controller
        let mut public_url = self.runtime.controller_public_url().await;
        let active_mode = self.resolve_active_mode().await;
        let active_provider = self.resolve_active_provider().await;

        if public_url.is_some()
            && (active_mode.as_deref() != Some(request.mode.as_str())
                || active_provider.as_deref() != Some(request.provider.as_str()))
        {
            self.runtime.stop_controller().await;
            public_url = None;
        }

        if public_url.is_none() {
            // 可用性检查
            let availability = providers::check_availability(&request.provider).await?;
            let available = availability
                .get("available")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if !available {
                let message = availability
                    .get("message")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| {
                        if request.provider == TUNNEL_PROVIDER_CLOUDFLARE {
                            get_tunnel_dependency_install_info(TUNNEL_PROVIDER_CLOUDFLARE).message
                        } else {
                            format!(
                                "Required dependency for provider '{}' is missing",
                                request.provider
                            )
                        }
                    });
                return Err(TunnelServiceError::missing_dependency(message));
            }

            let active_port = (self.get_active_port)();
            let origin_url = active_port.map(|p| format!("http://127.0.0.1:{}", p));

            let context = StartContext {
                active_port,
                origin_url,
            };

            let mut controller = providers::start(&request.provider, &request, &context).await?;
            controller.provider = Some(request.provider.clone());

            let controller_url = controller.public_url.clone();
            self.runtime.set_controller(Some(controller)).await;

            public_url = controller_url;
            if public_url.is_none() {
                self.runtime.stop_controller().await;
                return Err(TunnelServiceError::startup_failed(
                    "Tunnel started but no public URL was assigned",
                ));
            }

            if request.mode == TUNNEL_MODE_QUICK {
                if let Some(ref warning_fn) = self.on_quick_tunnel_warning {
                    warning_fn();
                }
            }
        }

        let provider_metadata = self.runtime.get_provider_metadata().await;

        Ok(StartResult {
            public_url: public_url.unwrap(),
            request,
            active_mode: self.resolve_active_mode().await.unwrap_or_default(),
            provider: self.resolve_active_provider().await.unwrap_or_default(),
            provider_metadata,
        })
    }
}
