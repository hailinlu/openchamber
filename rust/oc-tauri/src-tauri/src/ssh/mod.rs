//! SSH 管理器 — 1:1 移植 Electron ssh-manager.mjs 的 ControlMaster 编排。
//!
//! 这不是纯 SSH 客户端，而是 spawn 系统 `ssh` 二进制并使用 OpenSSH ControlMaster
//! 多路复用来共享一个认证的主连接。
//!
//! 核心流程:
//! 1. `ssh -G` 解析配置 (快速失败检查)
//! 2. `ssh -o ControlMaster=yes -o ControlPath=<sock> -o ControlPersist=300 -N <dest>`
//!    启动 master 进程
//! 3. `ssh -O check` 轮询就绪
//! 4. 远程探测 / 安装 / 启动 OpenChamber server
//! 5. `ssh -L <bindHost>:<localPort>:127.0.0.1:<remotePort> -N` 建立主转发
//! 6. `ssh -O forward [-L/-R/-D ...]` 添加额外转发
//! 7. Monitor 循环: TCP probe → `ssh -O check` → 断线重连
//!
//! IPC 命令 (8 个):
//! - desktop_ssh_instances_get / set
//! - desktop_ssh_import_hosts
//! - desktop_ssh_connect / disconnect
//! - desktop_ssh_status / logs / logs_clear

pub mod parser;
pub mod types;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use tokio::process::Child;
use tokio::task::JoinHandle;

use types::{HostCandidate, Instance, Phase, Status};

/// 日志最大行数 (复现 MAX_LOG_LINES_PER_INSTANCE)。
const MAX_LOG_LINES: usize = 1200;

/// Monitor 轮询间隔 (复现 MONITOR_INITIAL_POLL_MS / MONITOR_STEADY_POLL_MS)。
const MONITOR_INITIAL_POLL_MS: u64 = 2000;
const MONITOR_STEADY_POLL_MS: u64 = 10000;
const MONITOR_STABILIZE_TICKS: u32 = 5;

/// 重连最大尝试次数 (复现 DEFAULT_RECONNECT_MAX_ATTEMPTS)。
const MAX_RECONNECT_ATTEMPTS: u32 = 5;

/// ControlPersist 秒数 (复现 DEFAULT_CONTROL_PERSIST_SEC)。
const CONTROL_PERSIST_SEC: u32 = 300;

/// SSH 状态事件名 (复现 SSH_STATUS_EVENT)。
const SSH_STATUS_EVENT: &str = "openchamber:ssh-instance-status";

// ============================================================================
// 状态结构
// ============================================================================

/// 活跃的 SSH 会话 (master + forward 子进程 + 元数据)。
#[allow(dead_code)]
struct SshSession {
    master_child: Option<Child>,
    main_forward_child: Option<Child>,
    control_socket: PathBuf,
    session_dir: PathBuf,
    local_port: u16,
    remote_port: u16,
    started_by_us: bool,
    monitor_handle: Option<JoinHandle<()>>,
}

impl SshSession {
    async fn kill_all(&mut self) {
        // 停止 monitor
        if let Some(handle) = self.monitor_handle.take() {
            handle.abort();
        }
        // kill master + forward
        if let Some(mut child) = self.main_forward_child.take() {
            let _ = child.start_kill();
        }
        if let Some(mut child) = self.master_child.take() {
            let _ = child.start_kill();
        }
        // 删除 control socket
        let _ = std::fs::remove_file(&self.control_socket);
        // 删除 askpass
        let askpass = self.session_dir.join("askpass.sh");
        let _ = std::fs::remove_file(&askpass);
        let _ = std::fs::remove_dir(&self.session_dir);
    }
}

/// SSH 管理器全局状态。
struct SshManagerState {
    /// 每实例的日志缓冲 (cap MAX_LOG_LINES)。
    logs: HashMap<String, VecDeque<String>>,
    /// 每实例的状态快照。
    statuses: HashMap<String, Status>,
    /// 活跃会话。
    sessions: HashMap<String, SshSession>,
    /// 并发连接去重 (防止同一实例重复 connect)。
    connecting: HashMap<String, Arc<Mutex<()>>>,
}

impl SshManagerState {
    fn new() -> Self {
        Self {
            logs: HashMap::new(),
            statuses: HashMap::new(),
            sessions: HashMap::new(),
            connecting: HashMap::new(),
        }
    }

    fn log(&mut self, id: &str, message: &str) {
        let entry = self.logs.entry(id.to_string()).or_default();
        entry.push_back(message.to_string());
        while entry.len() > MAX_LOG_LINES {
            entry.pop_front();
        }
    }

    fn get_logs(&self, id: &str, limit: usize) -> Vec<String> {
        match self.logs.get(id) {
            None => Vec::new(),
            Some(deque) => {
                let skip = deque.len().saturating_sub(limit);
                deque.iter().skip(skip).cloned().collect()
            }
        }
    }

    fn clear_logs(&mut self, id: &str) {
        if let Some(deque) = self.logs.get_mut(id) {
            deque.clear();
        }
    }

    fn set_status(&mut self, id: &str, phase: Phase, detail: Option<String>) -> Status {
        let mut status = self
            .statuses
            .get(id)
            .cloned()
            .unwrap_or_else(|| Status::idle(id));
        status.phase = phase.as_str().to_string();
        status.detail = detail;
        status.updated_at_ms = types::now_millis();
        self.statuses.insert(id.to_string(), status.clone());
        status
    }

    fn update_status(&mut self, id: &str, status: Status) {
        self.statuses.insert(id.to_string(), status);
    }
}

/// SSH 管理器 (Tauri State)。
pub struct SshManager {
    state: Mutex<SshManagerState>,
    runtime: tokio::runtime::Runtime,
}

impl SshManager {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(SshManagerState::new()),
            runtime: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build SSH manager runtime"),
        }
    }

    fn get_runtime(&self) -> &tokio::runtime::Runtime {
        &self.runtime
    }
}

impl Default for SshManager {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// IPC 命令入口
// ============================================================================

/// `desktop_ssh_instances_get` — 返回 `{ instances: Instance[] }`
pub async fn instances_get(_args: &Value, _app: &AppHandle) -> Result<Value, String> {
    let root = crate::settings::SettingsStore::read();
    let instances = root
        .get("desktopSshInstances")
        .cloned()
        .filter(|v| v.is_array())
        .unwrap_or(json!([]));
    Ok(json!({ "instances": instances }))
}

/// `desktop_ssh_instances_set` — args: `{ config: { instances: Instance[] } }`
pub async fn instances_set(args: &Value, _app: &AppHandle) -> Result<Value, String> {
    let config = args
        .get("config")
        .ok_or("config is required")?;
    let instances = config
        .get("instances")
        .and_then(|v| v.as_array())
        .ok_or("config.instances must be an array")?;

    // 持久化到 settings.json
    let store = crate::settings::SettingsStore::new();
    store.mutate(|root| {
        root["desktopSshInstances"] = Value::Array(instances.clone());

        // Reconcile desktopHosts: 为每个 managed 实例创建/更新 host 条目
        let mut hosts = root
            .get("desktopHosts")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        for inst_val in instances {
            let id = inst_val.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let nickname = inst_val
                .get("nickname")
                .and_then(|v| v.as_str())
                .unwrap_or(id);

            // 简化: 只确保 host 条目存在
            let exists = hosts.iter().any(|h| {
                h.get("id").and_then(|v| v.as_str()) == Some(id)
            });
            if !exists {
                hosts.push(json!({
                    "id": id,
                    "label": nickname,
                    "kind": "ssh",
                }));
            }
        }
        root["desktopHosts"] = Value::Array(hosts);
        Ok(None)
    })?;

    Ok(Value::Null)
}

/// `desktop_ssh_import_hosts` — 返回 `HostCandidate[]`
///
/// 解析 `~/.ssh/config` 和 `/etc/ssh/ssh_config` 的 `Host` 条目。
pub async fn import_hosts(_args: &Value, _app: &AppHandle) -> Result<Value, String> {
    let mut candidates = Vec::new();

    // ~/.ssh/config (user source)
    if let Some(home) = std::env::var_os("HOME") {
        let user_config = PathBuf::from(&home).join(".ssh").join("config");
        if let Ok(content) = std::fs::read_to_string(&user_config) {
            parse_ssh_config_candidates(&content, "user", &mut candidates);
        }
    }

    // /etc/ssh/ssh_config (global source)
    let global_config = PathBuf::from("/etc/ssh/ssh_config");
    if let Ok(content) = std::fs::read_to_string(&global_config) {
        parse_ssh_config_candidates(&content, "global", &mut candidates);
    }

    Ok(json!(candidates))
}

/// `desktop_ssh_connect` — args: `{ id: string }`
pub async fn connect(args: &Value, app: &AppHandle) -> Result<Value, String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("id is required")?
        .to_string();

    let manager = get_ssh_manager(app);
    let rt = manager.get_runtime();

    // 并发去重: 检查是否已有正在进行的连接
    {
        let mut state = manager.state.lock().map_err(|e| e.to_string())?;
        let lock = state
            .connecting
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        // 如果已经有人在连接，等待它完成
        drop(state);

        let _guard = lock.lock().map_err(|e| e.to_string())?;
    }

    // 读取实例配置
    let instance = find_instance(&id)?;

    // 在 SSH runtime 上执行连接
    let app_handle = app.clone();
    let result = rt
        .spawn(async move { connect_blocking(&app_handle, &instance).await })
        .await;

    // 清理 connecting 标记
    {
        let mut state = manager.state.lock().map_err(|e| e.to_string())?;
        state.connecting.remove(&id);
    }

    match result {
        Ok(Ok(())) => Ok(Value::Null),
        Ok(Err(e)) => {
            // 连接失败: 设置 error 状态
            set_status_and_emit(app, &id, Phase::Error, Some(e.clone()));
            Err(e)
        }
        Err(e) => {
            let msg = format!("SSH connect task failed: {}", e);
            set_status_and_emit(app, &id, Phase::Error, Some(msg.clone()));
            Err(msg)
        }
    }
}

/// `desktop_ssh_disconnect` — args: `{ id: string }`
pub async fn disconnect(args: &Value, app: &AppHandle) -> Result<Value, String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("id is required")?
        .to_string();

    let manager = get_ssh_manager(app);
    let rt = manager.get_runtime();

    let app_handle = app.clone();
    let _ = rt
        .spawn(async move {
            disconnect_internal(&app_handle, &id, true).await;
        })
        .await;

    Ok(Value::Null)
}

/// `desktop_ssh_status` — args: `{ id?: string }`
pub async fn status(args: &Value, app: &AppHandle) -> Result<Value, String> {
    let manager = get_ssh_manager(app);
    let state = manager.state.lock().map_err(|e| e.to_string())?;

    let id_opt = args.get("id").and_then(|v| v.as_str());

    let statuses: Vec<Status> = match id_opt {
        Some(id) => {
            // 单个实例; 如果没有状态返回 idle 默认
            vec![
                state
                    .statuses
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| Status::idle(id)),
            ]
        }
        None => {
            // 所有已知实例; 如果 settings 中的实例没有状态也返回 idle
            let root = crate::settings::SettingsStore::read();
            let instances = root
                .get("desktopSshInstances")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            instances
                .iter()
                .filter_map(|inst| {
                    inst.get("id").and_then(|v| v.as_str()).map(|id| {
                        state
                            .statuses
                            .get(id)
                            .cloned()
                            .unwrap_or_else(|| Status::idle(id))
                    })
                })
                .collect()
        }
    };

    Ok(json!(statuses))
}

/// `desktop_ssh_logs` — args: `{ id: string, limit?: number }`
pub async fn logs(args: &Value, app: &AppHandle) -> Result<Value, String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("id is required")?;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(200) as usize;

    let manager = get_ssh_manager(app);
    let state = manager.state.lock().map_err(|e| e.to_string())?;
    let logs = state.get_logs(id, limit);
    Ok(json!(logs))
}

/// `desktop_ssh_logs_clear` — args: `{ id: string }`
pub async fn logs_clear(args: &Value, app: &AppHandle) -> Result<Value, String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("id is required")?;

    let manager = get_ssh_manager(app);
    let mut state = manager.state.lock().map_err(|e| e.to_string())?;
    state.clear_logs(id);
    Ok(Value::Null)
}

/// 关闭所有会话 (app quit 时调用)。
#[allow(dead_code)]
pub fn shutdown_all(app: &AppHandle) {
    let manager = match app.try_state::<SshManager>() {
        Some(m) => m,
        None => return,
    };
    let rt = manager.get_runtime();
    rt.block_on(async {
        // Collect sessions out of the map first so we don't hold the lock across await.
        let sessions: Vec<(String, SshSession)> = {
            let mut state = match manager.state.lock() {
                Ok(s) => s,
                Err(_) => return,
            };
            state.sessions.drain().collect()
        };
        for (_, mut session) in sessions {
            session.kill_all().await;
        }
    });
}

// ============================================================================
// 内部实现
// ============================================================================

/// 获取全局 SSH manager (Tauri State)。
fn get_ssh_manager(app: &AppHandle) -> &SshManager {
    app.state::<SshManager>().inner()
}

/// 从 settings.json 读取单个实例配置。
fn find_instance(id: &str) -> Result<Instance, String> {
    let root = crate::settings::SettingsStore::read();
    let instances = root
        .get("desktopSshInstances")
        .and_then(|v| v.as_array())
        .ok_or("no SSH instances configured")?;

    for inst_val in instances {
        let inst_id = inst_val.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if inst_id == id {
            return serde_json::from_value::<Instance>(inst_val.clone())
                .map_err(|e| format!("invalid instance config: {}", e));
        }
    }

    Err(format!("SSH instance '{}' not found", id))
}

/// 设置状态并推送到 UI。
fn set_status_and_emit(app: &AppHandle, id: &str, phase: Phase, detail: Option<String>) {
    let manager = match app.try_state::<SshManager>() {
        Some(m) => m,
        None => return,
    };
    let status = {
        let mut state = match manager.state.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        state.set_status(id, phase, detail)
    };
    let _ = app.emit(
        "openchamber:emit",
        json!({ "event": SSH_STATUS_EVENT, "detail": status }),
    );
}

/// 完整连接流程 (阻塞，在 SSH runtime 上执行)。
///
/// 移植 ssh-manager.mjs `connectBlocking`。
async fn connect_blocking(app: &AppHandle, instance: &Instance) -> Result<(), String> {
    let id = &instance.id;

    // 日志
    log_message(app, id, &format!("Connecting to {}...", instance.ssh_command));

    // 1. 解析 SSH 命令
    let parsed = parser::parse_ssh_command(&instance.ssh_command)
        .map_err(|e| format!("Invalid SSH command: {}", e))?;

    set_status_and_emit(app, id, Phase::ConfigResolved, None);

    // 2. 解析 SSH config (ssh -G)
    set_status_and_emit(app, id, Phase::AuthCheck, None);
    resolve_ssh_config(&parsed)
        .await
        .map_err(|e| format!("SSH config resolution failed: {}", e))?;

    // 3. 启动 ControlMaster
    set_status_and_emit(app, id, Phase::MasterConnecting, None);

    let control_socket = control_path_for_instance(id);
    let session_dir = session_dir_for_instance(id);

    // 确保 session 目录存在
    let _ = std::fs::create_dir_all(&session_dir);

    // 如果需要密码认证: 写 askpass 脚本
    let needs_askpass = instance
        .auth
        .ssh_password
        .as_ref()
        .map(|s| s.enabled && s.value.is_some())
        .unwrap_or(false);

    let askpass_path = if needs_askpass {
        let path = session_dir.join("askpass.sh");
        let password = instance
            .auth
            .ssh_password
            .as_ref()
            .and_then(|s| s.value.as_ref())
            .map(|s| s.as_str())
            .unwrap_or("");
        write_askpass_script(&path, password)?;
        Some(path)
    } else {
        None
    };

    // spawn master
    let master_args = build_master_args(&parsed, &control_socket);
    let mut master_cmd = tokio::process::Command::new("ssh");
    master_cmd.args(&master_args);
    master_cmd.stdin(std::process::Stdio::null());
    master_cmd.stdout(std::process::Stdio::piped());
    master_cmd.stderr(std::process::Stdio::piped());

    // Windows: CREATE_NO_WINDOW
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        master_cmd.creation_flags(0x08000000);
    }

    // Unix: setpgid (进程组) — tokio::process::Command 自带 pre_exec 方法
    #[cfg(unix)]
    {
        unsafe {
            master_cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }
    }

    // 注入 askpass env
    if let Some(ref askpass) = askpass_path {
        master_cmd.env("SSH_ASKPASS_REQUIRE", "force");
        master_cmd.env("SSH_ASKPASS", askpass);
        master_cmd.env("DISPLAY", "1");
        if let Some(pw) = instance.auth.ssh_password.as_ref().and_then(|s| s.value.as_ref()) {
            master_cmd.env("OPENCHAMBER_SSH_ASKPASS_VALUE", pw);
        }
    }

    let master_child = master_cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn SSH master: {}", e))?;

    let _master_pid = master_child.id();

    // 等待 master 就绪 (轮询 ssh -O check)
    wait_master_ready(&parsed, &control_socket, instance.connection_timeout_sec)
        .await
        .map_err(|e| {
            // master 启动失败，清理
            log_message(app, id, &format!("Master failed: {}", e));
            e
        })?;

    log_message(app, id, "SSH master connected");
    set_status_and_emit(app, id, Phase::RemoteProbe, None);

    // 4. 远程探测
    let _remote_port = probe_remote_system_info(&parsed, &control_socket, instance)
        .await
        .map_err(|e| format!("Remote probe failed: {}", e))?;

    // 5. 安装/更新 (managed 模式)
    let started_by_us;
    let final_remote_port;

    if instance.remote_openchamber.mode == "managed" {
        // 检查远程版本
        let app_version = app.package_info().version.to_string();
        let remote_version = check_remote_version(&parsed, &control_socket).await;

        let needs_install = match &remote_version {
            Ok(ver) => ver.trim() != app_version.trim(),
            Err(_) => true, // openchamber 不存在
        };

        if needs_install {
            set_status_and_emit(
                app,
                id,
                if remote_version.is_ok() {
                    Phase::Updating
                } else {
                    Phase::Installing
                },
                None,
            );

            install_remote(&parsed, &control_socket, &instance.remote_openchamber.install_method, &app_version)
                .await
                .map_err(|e| format!("Remote install failed: {}", e))?;

            log_message(app, id, "Remote OpenChamber installed/updated");
        }

        set_status_and_emit(app, id, Phase::ServerDetecting, None);

        // 探测是否已有 server 在运行
        let running_port = check_remote_server_running(&parsed, &control_socket, instance.remote_openchamber.preferred_port)
            .await
            .unwrap_or(None);

        if let Some(port) = running_port {
            final_remote_port = port;
            started_by_us = false;
            log_message(app, id, &format!("Reusing remote server on port {}", port));
        } else {
            // 启动远程 server
            set_status_and_emit(app, id, Phase::ServerStarting, None);
            let desired_port = instance.remote_openchamber.preferred_port.unwrap_or(0);
            let port = start_remote_server(&parsed, &control_socket, desired_port, &instance.auth)
                .await?;
            final_remote_port = port;
            started_by_us = true;
            log_message(app, id, &format!("Started remote server on port {}", port));
        }
    } else {
        // external 模式: 使用 preferred_port
        final_remote_port = instance
            .remote_openchamber
            .preferred_port
            .ok_or("external mode requires preferredPort")?;
        started_by_us = false;
    }

    // 6. 主转发
    set_status_and_emit(app, id, Phase::Forwarding, None);

    let bind_host = types::sanitize_bind_host(&instance.local_forward.bind_host);
    let local_port = pick_local_port(instance.local_forward.preferred_local_port)?;

    spawn_main_forward(&parsed, &control_socket, &bind_host, local_port, final_remote_port)
        .await?;

    // 7. 额外转发
    for forward in &instance.port_forwards {
        if !forward.enabled {
            continue;
        }
        spawn_extra_forward(&parsed, &control_socket, forward)
            .await
            .map_err(|e| format!("Extra forward failed: {}", e))?;
    }

    // 8. 就绪
    let local_url = format!("http://{}:{}", bind_host, local_port);
    log_message(app, id, &format!("Ready: {} → :{}", local_url, final_remote_port));

    let mut ready_status = Status::new(id, Phase::Ready);
    ready_status.local_url = Some(local_url.clone());
    ready_status.local_port = Some(local_port);
    ready_status.remote_port = Some(final_remote_port);
    ready_status.started_by_us = started_by_us;

    {
        let manager = get_ssh_manager(app);
        let mut state = manager.state.lock().map_err(|e| e.to_string())?;
        state.update_status(id, ready_status.clone());

        // 存储会话信息
        let session = SshSession {
            master_child: None, // master child 已在 spawn 时获取,这里不再持有
            main_forward_child: None,
            control_socket: control_socket.clone(),
            session_dir: session_dir.clone(),
            local_port,
            remote_port: final_remote_port,
            started_by_us,
            monitor_handle: None,
        };
        state.sessions.insert(id.clone(), session);

        // 丢弃 master_child 的 handle (它通过 control socket 管理)
        // 注意: master 进程在 ControlPersist 下会自动退出
    }

    let _ = app.emit(
        "openchamber:emit",
        json!({ "event": SSH_STATUS_EVENT, "detail": ready_status }),
    );

    // 9. 启动 monitor
    spawn_monitor(app, id, local_port, &control_socket, &parsed);

    Ok(())
}

/// 内部断开。
async fn disconnect_internal(app: &AppHandle, id: &str, report_idle: bool) {
    let manager = match app.try_state::<SshManager>() {
        Some(m) => m,
        None => return,
    };

    // 发送 ssh -O exit (best-effort)
    if let Ok(instance) = find_instance(id) {
        if let Ok(parsed) = parser::parse_ssh_command(&instance.ssh_command) {
            let control_socket = control_path_for_instance(id);
            let _ = control_master_operation(&parsed, &control_socket, "exit").await;
        }
    }

    // kill 会话: 取出 session 后释放锁再 await (避免 MutexGuard 跨 await)
    let session_opt = {
        let mut state = match manager.state.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        state.sessions.remove(id)
    };
    if let Some(mut session) = session_opt {
        session.kill_all().await;
    }

    if report_idle {
        set_status_and_emit(app, id, Phase::Idle, None);
    }

    log_message(app, id, "Disconnected");
}

/// Monitor 循环。
fn spawn_monitor(
    app: &AppHandle,
    id: &str,
    local_port: u16,
    control_socket: &PathBuf,
    parsed: &parser::ParsedSsh,
) {
    let app_handle = app.clone();
    let id_owned = id.to_string();
    let socket = control_socket.clone();
    let parsed = parsed.clone();

    tokio::spawn(async move {
        let mut ticks_healthy = 0u32;
        loop {
            let poll_ms = if ticks_healthy >= MONITOR_STABILIZE_TICKS {
                MONITOR_STEADY_POLL_MS
            } else {
                MONITOR_INITIAL_POLL_MS
            };
            tokio::time::sleep(Duration::from_millis(poll_ms)).await;

            // cheap TCP probe
            let reachable = is_local_tunnel_reachable(local_port).await;

            if reachable {
                ticks_healthy = ticks_healthy.saturating_add(1);
                continue;
            }

            // TCP 不通 → 检查 master
            let master_alive = is_control_master_alive(&parsed, &socket).await;

            if !master_alive {
                // 断线
                log_message(&app_handle, &id_owned, "Connection lost, attempting reconnect...");
                disconnect_internal(&app_handle, &id_owned, false).await;

                // 重连退避
                for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
                    let delay_ms = ((1u64 << (attempt - 1)) * 1000
                        + (types::now_millis() % 700) + 100)
                        .min(30000);

                    set_status_and_emit(
                        &app_handle,
                        &id_owned,
                        Phase::Degraded,
                        Some(format!("Reconnecting (attempt {}/{})", attempt, MAX_RECONNECT_ATTEMPTS)),
                    );

                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;

                    // 尝试重连
                    if let Ok(instance) = find_instance(&id_owned) {
                        match connect_blocking(&app_handle, &instance).await {
                            Ok(()) => return, // 重连成功，monitor 结束
                            Err(e) => {
                                log_message(&app_handle, &id_owned, &format!("Reconnect failed: {}", e));
                            }
                        }
                    }
                }

                // 超过最大重试
                let mut err_status = Status::new(&id_owned, Phase::Error);
                err_status.requires_user_action = true;
                err_status.detail = Some("Max reconnection attempts exceeded".to_string());

                if let Some(manager) = app_handle.try_state::<SshManager>() {
                    if let Ok(mut state) = manager.state.lock() {
                        state.update_status(&id_owned, err_status.clone());
                    }
                }
                let _ = app_handle.emit(
                    "openchamber:emit",
                    json!({ "event": SSH_STATUS_EVENT, "detail": err_status }),
                );
                return;
            }
            // master alive but tunnel not → 继续探测
        }
    });
}

fn log_message(app: &AppHandle, id: &str, message: &str) {
    if let Some(manager) = app.try_state::<SshManager>() {
        if let Ok(mut state) = manager.state.lock() {
            let timestamp = chrono::Local::now().format("%H:%M:%S");
            state.log(id, &format!("[{}] {}", timestamp, message));
        }
    }
}

// ============================================================================
// SSH 操作辅助函数
// ============================================================================

/// `ssh -G <dest>` — 配置解析 (快速失败检查)。
async fn resolve_ssh_config(parsed: &parser::ParsedSsh) -> Result<(), String> {
    let args = parser::build_ssh_args(parsed, &["-G".to_string()], None);
    let result = run_command("ssh", &args, Duration::from_secs(30)).await?;
    if result.code != 0 {
        return Err(result.stderr_or_stdout());
    }
    Ok(())
}

/// 构建 master spawn 参数。
fn build_master_args(parsed: &parser::ParsedSsh, control_socket: &PathBuf) -> Vec<String> {
    parser::build_ssh_args(
        parsed,
        &[
            "-o".to_string(),
            "ControlMaster=yes".to_string(),
            "-o".to_string(),
            format!("ControlPath={}", control_socket.display()),
            "-o".to_string(),
            format!("ControlPersist={}", CONTROL_PERSIST_SEC),
            "-N".to_string(),
        ],
        None,
    )
}

/// 等待 master 就绪 (轮询 ssh -O check)。
async fn wait_master_ready(
    parsed: &parser::ParsedSsh,
    control_socket: &PathBuf,
    timeout_sec: u32,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_sec as u64);
    let mut poll_ms = 250u64;

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err("Timed out waiting for SSH master to become ready".to_string());
        }

        let alive = is_control_master_alive(parsed, control_socket).await;
        if alive {
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(poll_ms)).await;
        poll_ms = (poll_ms * 2).min(2000);
    }
}

/// `ssh -O check` — 检查 master 是否存活。
async fn is_control_master_alive(
    parsed: &parser::ParsedSsh,
    control_socket: &PathBuf,
) -> bool {
    match control_master_operation(parsed, control_socket, "check").await {
        Ok(result) => result.code == 0,
        Err(_) => false,
    }
}

/// `ssh -O <op>` — ControlMaster 控制操作。
async fn control_master_operation(
    parsed: &parser::ParsedSsh,
    control_socket: &PathBuf,
    op: &str,
) -> Result<CommandResult, String> {
    let args = parser::build_ssh_args(
        parsed,
        &[
            "-o".to_string(),
            "ControlMaster=no".to_string(),
            "-o".to_string(),
            format!("ControlPath={}", control_socket.display()),
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "ConnectTimeout=3".to_string(),
            "-O".to_string(),
            op.to_string(),
        ],
        None,
    );
    run_command("ssh", &args, Duration::from_secs(10)).await
}

/// 运行远程命令 `ssh -T <dest> "sh -lc '<script>'"`。
async fn run_remote_command(
    parsed: &parser::ParsedSsh,
    control_socket: &PathBuf,
    script: &str,
    timeout_sec: u32,
) -> Result<String, String> {
    let remote_cmd = format!("sh -lc {}", parser::shell_quote(script));
    let args = parser::build_ssh_args(
        parsed,
        &[
            "-o".to_string(),
            "ControlMaster=no".to_string(),
            "-o".to_string(),
            format!("ControlPath={}", control_socket.display()),
            "-o".to_string(),
            format!("ConnectTimeout={}", timeout_sec),
            "-T".to_string(),
        ],
        Some(&remote_cmd),
    );
    let result = run_command("ssh", &args, Duration::from_secs(timeout_sec as u64)).await?;
    if result.code != 0 {
        return Err(result.stderr_or_stdout());
    }
    Ok(result.stdout)
}

/// 远程探测系统信息。
async fn probe_remote_system_info(
    parsed: &parser::ParsedSsh,
    control_socket: &PathBuf,
    _instance: &Instance,
) -> Result<u16, String> {
    // 检查远程 OS (必须是 linux 或 darwin)
    let os_check = run_remote_command(parsed, control_socket, "uname -s", 10).await?;
    let remote_os = os_check.trim();
    if remote_os != "Linux" && remote_os != "Darwin" {
        return Err(format!("Unsupported remote OS: {}", remote_os));
    }

    // 使用实例的 preferred_port (如果有)
    Ok(0) // 返回 0 表示还没有探测到端口，后续会探测/启动
}

/// 检查远程 OpenChamber 版本。
async fn check_remote_version(
    parsed: &parser::ParsedSsh,
    control_socket: &PathBuf,
) -> Result<String, String> {
    let output =
        run_remote_command(parsed, control_socket, "openchamber --version 2>/dev/null || true", 10)
            .await?;
    // 解析版本号: 取 stdout 中的第一个 x.y.z 格式的 token
    parse_version_token(&output)
        .ok_or_else(|| "no version found".to_string())
}

/// 检查远程 server 是否在运行。
async fn check_remote_server_running(
    parsed: &parser::ParsedSsh,
    control_socket: &PathBuf,
    preferred_port: Option<u16>,
) -> Result<Option<u16>, String> {
    let port = match preferred_port {
        Some(p) => p,
        None => return Ok(None),
    };

    let script = format!(
        "curl -sf http://127.0.0.1:{}/health > /dev/null 2>&1 && echo RUNNING || echo STOPPED",
        port
    );
    let output = run_remote_command(parsed, control_socket, &script, 10).await?;
    if output.trim().contains("RUNNING") {
        Ok(Some(port))
    } else {
        Ok(None)
    }
}

/// 安装远程 OpenChamber。
async fn install_remote(
    parsed: &parser::ParsedSsh,
    control_socket: &PathBuf,
    install_method: &str,
    app_version: &str,
) -> Result<(), String> {
    // 尝试 bun 或 npm
    let methods: Vec<&str> = if install_method == "npm" {
        vec!["npm", "bun"]
    } else if install_method == "bun" {
        vec!["bun", "npm"]
    } else {
        vec!["bun", "npm"]
    };

    for method in &methods {
        let script = match *method {
            "bun" => format!("bun add -g @openchamber/web@{}", app_version),
            "npm" => format!("npm install -g @openchamber/web@{}", app_version),
            _ => continue,
        };

        match run_remote_command(parsed, control_socket, &script, 120).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                log::warn!("[ssh] install via {} failed: {}", method, e);
                continue;
            }
        }
    }

    Err("Failed to install OpenChamber on remote (both bun and npm failed)".to_string())
}

/// 启动远程 OpenChamber server。
async fn start_remote_server(
    parsed: &parser::ParsedSsh,
    control_socket: &PathBuf,
    desired_port: u16,
    auth: &types::Auth,
) -> Result<u16, String> {
    let port_arg = if desired_port > 0 {
        format!("--port {}", desired_port)
    } else {
        "--port 0".to_string()
    };

    let password_env = auth
        .openchamber_password
        .as_ref()
        .filter(|s| s.enabled)
        .and_then(|s| s.value.as_ref())
        .map(|v| format!("OPENCHAMBER_UI_PASSWORD={}", parser::shell_quote(v)))
        .unwrap_or_default();

    let script = format!(
        "{} OPENCHAMBER_RUNTIME=ssh-remote nohup openchamber serve --hostname 127.0.0.1 {} > /tmp/oc-serve.log 2>&1 &\nsleep 2\ncat /tmp/oc-serve.log",
        password_env, port_arg
    );

    let output = run_remote_command(parsed, control_socket, &script, 30).await?;

    // 从输出中解析实际端口
    parse_port_from_output(&output).ok_or_else(|| "Could not determine remote server port".to_string())
}

/// 建立主转发。
async fn spawn_main_forward(
    parsed: &parser::ParsedSsh,
    control_socket: &PathBuf,
    bind_host: &str,
    local_port: u16,
    remote_port: u16,
) -> Result<(), String> {
    let forward_spec = format!("{}:{}:127.0.0.1:{}", bind_host, local_port, remote_port);
    let args = parser::build_ssh_args(
        parsed,
        &[
            "-o".to_string(),
            "ControlMaster=no".to_string(),
            "-o".to_string(),
            format!("ControlPath={}", control_socket.display()),
            "-N".to_string(),
            "-L".to_string(),
            forward_spec,
        ],
        None,
    );

    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    let _child = cmd.spawn().map_err(|e| format!("Failed to spawn forward: {}", e))?;

    // 等待一小段时间让 forward 建立
    tokio::time::sleep(Duration::from_millis(250)).await;

    Ok(())
}

/// 建立额外转发 (via `ssh -O forward`)。
async fn spawn_extra_forward(
    parsed: &parser::ParsedSsh,
    control_socket: &PathBuf,
    forward: &types::Forward,
) -> Result<(), String> {
    let forward_spec = match forward.ftype.as_str() {
        "local" => {
            let lh = forward.local_host.as_deref().unwrap_or("127.0.0.1");
            let lp = forward.local_port.unwrap_or(0);
            let rh = forward.remote_host.as_deref().unwrap_or("127.0.0.1");
            let rp = forward.remote_port.unwrap_or(0);
            format!("-L {}:{}:{}:{}", lh, lp, rh, rp)
        }
        "remote" => {
            let rh = forward.remote_host.as_deref().unwrap_or("127.0.0.1");
            let rp = forward.remote_port.unwrap_or(0);
            let lh = forward.local_host.as_deref().unwrap_or("127.0.0.1");
            let lp = forward.local_port.unwrap_or(0);
            format!("-R {}:{}:{}:{}", rh, rp, lh, lp)
        }
        "dynamic" => {
            let lh = forward.local_host.as_deref().unwrap_or("127.0.0.1");
            let lp = forward.local_port.unwrap_or(0);
            format!("-D {}:{}", lh, lp)
        }
        other => return Err(format!("Unknown forward type: {}", other)),
    };

    let parts: Vec<&str> = forward_spec.split_whitespace().collect();
    let mut pre_args = vec![
        "-o".to_string(),
        "ControlMaster=no".to_string(),
        "-o".to_string(),
        format!("ControlPath={}", control_socket.display()),
        "-O".to_string(),
        "forward".to_string(),
    ];
    for p in &parts {
        pre_args.push(p.to_string());
    }

    let args = parser::build_ssh_args(parsed, &pre_args, None);
    let result = run_command("ssh", &args, Duration::from_secs(10)).await?;
    if result.code != 0 {
        return Err(result.stderr_or_stdout());
    }

    // local 转发: 验证端口可达
    if forward.ftype == "local" {
        if let Some(lp) = forward.local_port {
            tokio::time::sleep(Duration::from_millis(100)).await;
            // best-effort probe
            let _ = is_local_tunnel_reachable(lp).await;
        }
    }

    Ok(())
}

// ============================================================================
// 工具函数
// ============================================================================

/// 命令运行结果。
struct CommandResult {
    code: i32,
    stdout: String,
    stderr: String,
}

impl CommandResult {
    fn stderr_or_stdout(&self) -> String {
        if !self.stderr.trim().is_empty() {
            self.stderr.trim().to_string()
        } else if !self.stdout.trim().is_empty() {
            self.stdout.trim().to_string()
        } else {
            "Remote command failed".to_string()
        }
    }
}

/// 运行命令并捕获输出。
async fn run_command(
    cmd: &str,
    args: &[String],
    timeout: Duration,
) -> Result<CommandResult, String> {
    let mut command = tokio::process::Command::new(cmd);
    command.args(args);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }

    let child = command.spawn().map_err(|e| format!("spawn failed: {}", e))?;

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(CommandResult {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }),
        Ok(Err(e)) => Err(format!("wait failed: {}", e)),
        Err(_) => Err("command timed out".to_string()),
    }
}

/// 检查本地隧道端口是否可达 (TCP connect)。
async fn is_local_tunnel_reachable(port: u16) -> bool {
    use tokio::net::TcpStream;
    matches!(
        tokio::time::timeout(
            Duration::from_millis(500),
            TcpStream::connect(format!("127.0.0.1:{}", port)),
        )
        .await,
        Ok(Ok(_))
    )
}

/// 选择本地端口。
fn pick_local_port(preferred: Option<u16>) -> Result<u16, String> {
    use std::net::TcpListener;

    // 优先使用 preferred
    if let Some(port) = preferred {
        if port > 0 {
            // 检查是否可用
            if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) {
                drop(listener);
                return Ok(port);
            }
        }
    }

    // 分配临时端口
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("port allocation failed: {}", e))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("port read failed: {}", e))?
        .port();
    drop(listener);
    Ok(port)
}

/// control socket 路径 (基于实例 ID 的 djb2 哈希)。
fn control_path_for_instance(id: &str) -> PathBuf {
    let hash = types::djb2_hash(id);
    let tmpdir = std::env::temp_dir();
    tmpdir.join(format!("ocssh-{}.sock", hash))
}

/// session 目录路径。
fn session_dir_for_instance(id: &str) -> PathBuf {
    let base = if let Ok(dir) = std::env::var("OPENCHAMBER_DATA_DIR") {
        PathBuf::from(dir.trim())
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config").join("openchamber")
    } else {
        std::env::temp_dir()
    };
    base.join("ssh").join(id)
}

/// 写 askpass 脚本。
fn write_askpass_script(path: &PathBuf, _password: &str) -> Result<(), String> {
    let content = "#!/bin/bash
PROMPT=\"$1\"

if [[ -n \"$OPENCHAMBER_SSH_ASKPASS_VALUE\" ]]; then
  if [[ \"$PROMPT\" == *\"assword\"* || \"$PROMPT\" == *\"passphrase\"* ]]; then
    printf '%s\\n' \"$OPENCHAMBER_SSH_ASKPASS_VALUE\"
    exit 0
  fi
fi

DEFAULT_ANSWER=\"\"
if [[ \"$PROMPT\" == *\"yes/no\"* ]]; then
  DEFAULT_ANSWER=\"yes\"
fi

printf '%s\\n' \"$DEFAULT_ANSWER\"
";

    std::fs::write(path, content).map_err(|e| format!("write askpass failed: {}", e))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)
            .map_err(|e| format!("askpass metadata failed: {}", e))?
            .permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(path, perms)
            .map_err(|e| format!("askpass chmod failed: {}", e))?;
    }

    Ok(())
}

/// 从输出中解析版本号 (x.y.z 格式)。
fn parse_version_token(raw: &str) -> Option<String> {
    for token in raw.split_whitespace() {
        let candidate = token.trim().trim_start_matches('v');
        let parts: Vec<&str> = candidate.split('.').collect();
        if parts.len() >= 2 && parts.iter().all(|p| p.parse::<u32>().is_ok()) {
            return Some(candidate.to_string());
        }
    }
    None
}

/// 从 server 启动输出中解析端口号。
fn parse_port_from_output(output: &str) -> Option<u16> {
    // 查找 "listening on" 或纯数字端口号
    for line in output.lines() {
        let trimmed = line.trim();
        // 尝试匹配 "port 12345" 或 ":12345" 或独立的数字
        for token in trimmed.split_whitespace() {
            let cleaned = token.trim_matches(|c: char| !c.is_ascii_digit());
            if let Ok(port) = cleaned.parse::<u16>() {
                if port > 1024 {
                    return Some(port);
                }
            }
        }
    }
    None
}

/// 解析 ssh config 文件中的 Host 条目。
fn parse_ssh_config_candidates(content: &str, source: &str, candidates: &mut Vec<HostCandidate>) {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let mut parts = trimmed.split_whitespace();
        if let Some(keyword) = parts.next() {
            if keyword.eq_ignore_ascii_case("host") {
                if let Some(pattern) = parts.next() {
                    let is_pattern = pattern.contains('*') || pattern.contains('?');
                    candidates.push(HostCandidate {
                        host: pattern.to_string(),
                        pattern: is_pattern,
                        source: source.to_string(),
                        ssh_command: format!("ssh {}", pattern),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_from_output() {
        assert_eq!(
            parse_version_token("OpenChamber v1.2.3"),
            Some("1.2.3".to_string())
        );
        assert_eq!(parse_version_token("1.0.0"), Some("1.0.0".to_string()));
        assert_eq!(parse_version_token("no version here"), None);
    }

    #[test]
    fn test_parse_port_from_output() {
        assert_eq!(
            parse_port_from_output("listening on port 54321"),
            Some(54321)
        );
        assert_eq!(parse_port_from_output("Server running on :8080"), Some(8080));
    }

    #[test]
    fn control_path_has_hash() {
        let path = control_path_for_instance("test-id-123");
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("ocssh-"));
        assert!(name.ends_with(".sock"));
    }

    #[test]
    fn parse_ssh_config_extracts_hosts() {
        let config = r#"
# My SSH config
Host github.com
  HostName github.com
  User git

Host *.example.com
  User deploy

Host production
  HostName prod.internal
  Port 2222
"#;
        let mut candidates = Vec::new();
        parse_ssh_config_candidates(config, "user", &mut candidates);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].host, "github.com");
        assert!(!candidates[0].pattern);
        assert_eq!(candidates[1].host, "*.example.com");
        assert!(candidates[1].pattern);
        assert_eq!(candidates[2].host, "production");
    }
}
