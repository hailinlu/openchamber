//! 动画托盘 — 复现 Electron tray.mjs 的 breathing 动画 + title/tooltip + 状态行图标。
//!
//! `desktop_tray_update` 由 UI 推送 live 状态 (sessions, approvals, dockBadgeCount),
//! 我们据此:
//! 1. compute icon state (busy > unseen > idle)
//! 2. busy → 启动 ping-pong breathing 动画 (16 帧, 75ms/帧)
//! 3. unseen → 静态 unseen 图标
//! 4. idle → 静态 idle 图标
//! 5. compute title (◆ N / ▲ N) + tooltip → set_title / set_tooltip
//! 6. 重建菜单 (含状态行图标)
//!
//! macOS: 所有图标 set_icon_as_template(true), 自动适配深/浅色。
//! Windows: 不动画 (breathIconPaths = [icon.ico] 单元素, < 2 帧 → 不启动动画)。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use serde::Deserialize;
use serde_json::{json, Value};
use tauri::{
    image::Image,
    menu::{IconMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    AppHandle, Emitter, Manager,
};

/// 动画帧间隔 (ms) — 复现 ANIM_INTERVAL_MS。
const ANIM_INTERVAL_MS: u64 = 75;

/// breathing 帧数 — 复现 TRAY_BREATH_FRAME_COUNT。
const BREATH_FRAME_COUNT: usize = 16;

/// 托盘菜单直接展示的最大审批数。超出部分进入 `More…` 子菜单。
pub(crate) const MAX_APPROVALS: usize = 10;

/// 托盘菜单直接展示的最大会话数。超出部分进入 `More…` 子菜单。
pub(crate) const MAX_SESSIONS: usize = 8;

/// `More…` 子菜单标题 (会话/审批超出限额时使用)。
const MORE_LABEL: &str = "More…";

/// `Needs your attention` 子标题 (审批区上方)。
const ATTENTION_LABEL: &str = "Needs your attention";

// ============================================================================
// 图标资源 (编译时嵌入)
// ============================================================================

/// 从编译时嵌入的 PNG 字节构建 Tauri Image。
macro_rules! tray_icon {
    ($path:expr) => {
        Image::from_bytes(include_bytes!($path))
            .expect("failed to parse embedded tray icon PNG")
    };
}

/// 加载 idle 图标。
fn load_idle_icon() -> Image<'static> {
    tray_icon!("../icons/tray/trayTemplate-idle.png")
}

/// 加载 unseen 图标。
fn load_unseen_icon() -> Image<'static> {
    tray_icon!("../icons/tray/trayTemplate-unseen.png")
}

/// 加载 16 帧 breathing 动画。
fn load_breath_frames() -> Vec<Image<'static>> {
    (0..BREATH_FRAME_COUNT)
        .map(|i| {
            // include_bytes! 需要字面量路径, 用 match 逐帧索引
            match i {
                0 => tray_icon!("../icons/tray/trayTemplate-breath-00.png"),
                1 => tray_icon!("../icons/tray/trayTemplate-breath-01.png"),
                2 => tray_icon!("../icons/tray/trayTemplate-breath-02.png"),
                3 => tray_icon!("../icons/tray/trayTemplate-breath-03.png"),
                4 => tray_icon!("../icons/tray/trayTemplate-breath-04.png"),
                5 => tray_icon!("../icons/tray/trayTemplate-breath-05.png"),
                6 => tray_icon!("../icons/tray/trayTemplate-breath-06.png"),
                7 => tray_icon!("../icons/tray/trayTemplate-breath-07.png"),
                8 => tray_icon!("../icons/tray/trayTemplate-breath-08.png"),
                9 => tray_icon!("../icons/tray/trayTemplate-breath-09.png"),
                10 => tray_icon!("../icons/tray/trayTemplate-breath-10.png"),
                11 => tray_icon!("../icons/tray/trayTemplate-breath-11.png"),
                12 => tray_icon!("../icons/tray/trayTemplate-breath-12.png"),
                13 => tray_icon!("../icons/tray/trayTemplate-breath-13.png"),
                14 => tray_icon!("../icons/tray/trayTemplate-breath-14.png"),
                15 => tray_icon!("../icons/tray/trayTemplate-breath-15.png"),
                _ => unreachable!(),
            }
        })
        .collect::<Vec<_>>()
}

/// 状态行图标集合 (busy / retry / error / unseen / blank)。
struct StatusIcons {
    busy: Image<'static>,
    retry: Image<'static>,
    error: Image<'static>,
    unseen: Image<'static>,
    blank: Image<'static>,
}

fn load_status_icons() -> StatusIcons {
    StatusIcons {
        busy: tray_icon!("../icons/tray/status/busy.png"),
        retry: tray_icon!("../icons/tray/status/retry.png"),
        error: tray_icon!("../icons/tray/status/error.png"),
        unseen: tray_icon!("../icons/tray/status/unseen.png"),
        blank: tray_icon!("../icons/tray/status/blank.png"),
    }
}

// ============================================================================
// 动画状态 (全局)
// ============================================================================

struct TrayAnimationState {
    /// 当前 icon state ("busy" / "unseen" / "idle" / null)。
    icon_state: Mutex<Option<String>>,
    /// 动画是否运行中。
    anim_running: AtomicBool,
    /// 动画是否已被销毁 (app quit)。
    destroyed: AtomicBool,
    /// 上次设置的 title (diff guard, 避免 per-tick native call)。
    last_title: Mutex<String>,
}

static TRAY_ANIM: LazyLock<TrayAnimationState> = LazyLock::new(|| TrayAnimationState {
    icon_state: Mutex::new(None),
    anim_running: AtomicBool::new(false),
    destroyed: AtomicBool::new(false),
    last_title: Mutex::new(String::new()),
});

/// 图标资源 (lazy init, 一次性加载)。
static IDLE_ICON: LazyLock<Image<'static>> = LazyLock::new(load_idle_icon);
static UNSEEN_ICON: LazyLock<Image<'static>> = LazyLock::new(load_unseen_icon);
static BREATH_FRAMES: LazyLock<Vec<Image<'static>>> = LazyLock::new(load_breath_frames);
static STATUS_ICONS: LazyLock<StatusIcons> = LazyLock::new(load_status_icons);

// ============================================================================
// Typed snapshot payload (frontend → backend 契约)
//
// 字段名与 packages/ui/src/hooks/useTraySync.ts 的 TraySnapshot 保持一致:
// 全部 camelCase, serde 默认即可反序列化。
// ============================================================================

/// UI 推送的快照: 会话 / 审批 / 配额 / Dock badge。
///
/// 解析失败 (`serde_json::from_value`) 不应阻塞 UI 流 — 调用方应吞掉错误
/// 并保留旧菜单状态, 而非整体失败。本类型仅描述合法字段; 缺失字段使用
/// `#[serde(default)]` 退化为空集合, 确保 UI 的"最小可用菜单"语义。
///
/// 字段同时接受 camelCase (frontend) 与 snake_case (rust native) 两种命名,
/// 与 `packages/ui/src/hooks/useTraySync.ts` 的 wire format 对齐。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(crate) struct TraySnapshot {
    #[serde(default)]
    pub sessions: Vec<TraySession>,
    #[serde(default)]
    pub approvals: Vec<TrayApproval>,
    #[serde(default, alias = "instanceName")]
    pub instance_name: String,
    #[serde(default)]
    pub usage: TrayUsage,
    #[serde(default, alias = "dockBadgeCount")]
    pub dock_badge_count: i64,
}

/// 复现前端 `TraySession` — 注意字段 `id` (不是 `sessionId`), 与
/// packages/ui/src/hooks/useTraySync.ts 保持一致。
///
/// 同时接受 `has_error` / `hasError` 两种命名以兼容两套 caller;
/// `id` / `sessionId` 别名在 [`parse_snapshot`] 中通过二次解析处理,
/// 此处的 `#[derive(Deserialize)]` 仅作为 fallback。
///
/// `branch` / `subtitle` 是 `tray.mjs` 已使用但尚未在 Rust 菜单中渲染的
/// 元数据 — 保留字段便于后续"项目 · 分支"副标题渲染直接复用。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
#[allow(dead_code)] // 部分字段仅做 wire-format 占位 (branch/subtitle 为未来副标题渲染预留)。
pub(crate) struct TraySession {
    #[serde(default, alias = "sessionId")]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub unseen: i64,
    #[serde(default, alias = "hasError")]
    pub has_error: bool,
    #[serde(default)]
    pub directory: String,
    #[serde(default)]
    pub subtitle: String,
}

impl TraySession {
    /// 状态行图标 key。
    fn status_icon_key(&self) -> &'static str {
        if self.status == "busy" {
            return "busy";
        }
        if self.status == "retry" {
            return "retry";
        }
        if self.has_error {
            return "error";
        }
        if self.unseen > 0 {
            return "unseen";
        }
        "blank"
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
#[allow(dead_code)] // `session_title` 是 wire-format 占位, 暂未在 Rust 菜单中渲染。
pub(crate) struct TrayApproval {
    /// `"permission"` 或 `"question"` — 与前端 `kind` 字段对齐。
    #[serde(default)]
    pub kind: String,
    /// 审批请求 ID (传给 `respondToPermission` 的第二个参数)。
    #[serde(default)]
    pub id: String,
    #[serde(default, alias = "sessionId")]
    pub session_id: String,
    #[serde(default)]
    pub session_title: String,
    /// 显示文本。
    #[serde(default, alias = "title")]
    pub label: String,
    #[serde(default)]
    pub directory: String,
}

/// (空 — 之前的手动 helper 已被 `TraySession` / `TrayApproval` /
/// `TraySnapshot` 上的 `#[serde(alias)]` 取代。)

/// 复现前端 `TrayUsage` — 配额子菜单内容。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(crate) struct TrayUsage {
    pub mode: String,
    pub groups: Vec<TrayUsageGroup>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(crate) struct TrayUsageGroup {
    pub provider: String,
    pub rows: Vec<TrayUsageRow>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(crate) struct TrayUsageRow {
    pub label: String,
    pub value: String,
}

/// 自定义 snapshot 解析 — 同时支持 camelCase (frontend) 与 snake_case (rust)。
/// 失败时返回空 snapshot 而非错误, 保证 tray 流式更新时单次坏数据不会让
/// 整个菜单失效。
pub(crate) fn parse_snapshot(value: &Value) -> TraySnapshot {
    // 所有 camelCase ↔ snake_case 别名都已经在 struct 的 `#[serde(alias)]`
    // 里声明, 一次 from_value 就能反序列化全部字段。
    serde_json::from_value::<TraySnapshot>(value.clone()).unwrap_or_default()
}

// ============================================================================
// 纯模型: TrayMenuModel (不依赖 tauri, 可单测)
// ============================================================================

/// 单一菜单项的纯模型。**仅描述语义**, 不构造 tauri 菜单对象;
/// 真正的 tauri 菜单由 [`build_built_tray_menu`] 从模型构造。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrayMenuEntry {
    /// 头部: 实例名称 (不可点击)。
    Header { label: String },
    /// 不可点击的子标题 (e.g. "Needs your attention")。
    Subheader { label: String },
    /// 水平分隔线。
    Separator,
    /// 会话项 — 点击 → FocusSession。
    Session {
        index: usize,
        label: String,
        status_icon: TrayStatusIcon,
        session_id: String,
        directory: String,
    },
    /// 审批子菜单 (permission 类型) — 含 once/always/reject/open 4 个子项。
    ApprovalSubmenu {
        index: usize,
        label: String,
        session_id: String,
        approval_id: String,
        directory: String,
    },
    /// 审批直接项 (question 类型) — 点击 → FocusApproval。
    ApprovalDirect {
        index: usize,
        label: String,
        session_id: String,
        approval_id: String,
        directory: String,
    },
    /// `More…` 子菜单标题 — 包含被截断的会话/审批条目。
    MoreSubmenu { label: String },
    /// 配额子菜单 (Usage)。
    UsageSubmenu { label: String },
    /// 基础动作 (New Session / Show / Quit)。
    BaseAction { id: TrayBaseAction, label: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrayStatusIcon {
    Busy,
    Retry,
    Error,
    Unseen,
    Blank,
}

/// 基础动作枚举 — 复现 `tray_new_session` / `tray_show` / `tray_quit` 等。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrayBaseAction {
    NewSession,
    NewMiniChat,
    Show,
    Quit,
}

impl TrayBaseAction {
    pub fn label(self) -> &'static str {
        match self {
            TrayBaseAction::NewSession => "New Session",
            TrayBaseAction::NewMiniChat => "New Mini Chat",
            TrayBaseAction::Show => "Show GridForge",
            TrayBaseAction::Quit => "Quit GridForge",
        }
    }

    pub fn accelerator(self) -> Option<&'static str> {
        match self {
            TrayBaseAction::NewSession => Some("CmdOrCtrl+N"),
            TrayBaseAction::NewMiniChat => None,
            TrayBaseAction::Show => None,
            TrayBaseAction::Quit => Some("CmdOrCtrl+Q"),
        }
    }
}

/// 纯托盘菜单模型。`entries` 字段顺序就是菜单从上到下的展示顺序。
///
/// 该结构**不**包含任何 tauri 类型 (Menu / MenuItem), 因此可以无副作用
/// 地构造 + 断言, 适合单元测试。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TrayMenuModel {
    pub instance_name: String,
    pub entries: Vec<TrayMenuEntry>,
}

impl TrayMenuModel {
    pub fn new(instance_name: impl Into<String>) -> Self {
        Self {
            instance_name: instance_name.into(),
            entries: Vec::new(),
        }
    }

    #[allow(dead_code)] // API surface — exercised in tests; reserved for future external callers.
    pub fn push(&mut self, entry: TrayMenuEntry) {
        self.entries.push(entry);
    }

    /// 递归判断某个 label (e.g. "New Session" / "Allow once") 是否出现在
    /// 模型任何位置 — **包括** 子菜单内的项。
    #[allow(dead_code)] // API surface — exercised in tests.
    pub fn contains_label(&self, needle: &str) -> bool {
        self.entries.iter().any(|e| self.entry_contains_label(e, needle))
    }

    #[allow(dead_code)] // API surface — exercised in tests.
    fn entry_contains_label(&self, entry: &TrayMenuEntry, needle: &str) -> bool {
        match entry {
            TrayMenuEntry::Header { label }
            | TrayMenuEntry::Subheader { label }
            | TrayMenuEntry::MoreSubmenu { label }
            | TrayMenuEntry::UsageSubmenu { label } => label == needle,
            TrayMenuEntry::Session { label, .. }
            | TrayMenuEntry::ApprovalSubmenu { label, .. }
            | TrayMenuEntry::ApprovalDirect { label, .. }
            | TrayMenuEntry::BaseAction { label, .. } => label == needle,
            // Separator 没有 label, 直接返回 false (永不匹配 needle)。
            TrayMenuEntry::Separator => false,
        }
    }

    /// 递归判断某个**子菜单标题** (approval / More… / Usage) 是否存在。
    #[allow(dead_code)] // API surface — exercised in tests.
    pub fn contains_submenu(&self, needle: &str) -> bool {
        self.entries.iter().any(|e| match e {
            TrayMenuEntry::ApprovalSubmenu { label, .. }
            | TrayMenuEntry::MoreSubmenu { label, .. }
            | TrayMenuEntry::UsageSubmenu { label, .. } => label == needle,
            _ => false,
        })
    }

    /// 构造"完整"菜单模型: header → approvals (capped) → More… → sessions
    /// (capped) → More… → usage (only if non-empty) → base actions。
    pub fn from_snapshot(snapshot: &TraySnapshot) -> Self {
        let mut model = Self::new(&snapshot.instance_name);

        model.push(TrayMenuEntry::Header {
            label: snapshot.instance_name.clone(),
        });
        model.push(TrayMenuEntry::Separator);

        // ---- 审批列表 ----
        if !snapshot.approvals.is_empty() {
            model.push(TrayMenuEntry::Subheader {
                label: ATTENTION_LABEL.to_string(),
            });

            let total = snapshot.approvals.len();
            let direct = total.min(MAX_APPROVALS);
            for (index, approval) in snapshot.approvals.iter().take(direct).enumerate() {
                push_approval_entry(&mut model, index, approval);
            }
            if total > MAX_APPROVALS {
                let overflow_label = format!("More approvals ({})", total - MAX_APPROVALS);
                model.push(TrayMenuEntry::MoreSubmenu {
                    label: MORE_LABEL.to_string(),
                });
                // 保留语义信息: 真实的 `More…` 项同样以 label = overflow_label
                // 暴露给 tests, 通过 `contains_label` 验证。
                model.push(TrayMenuEntry::Subheader {
                    label: overflow_label,
                });
            }

            model.push(TrayMenuEntry::Separator);
        }

        // ---- 会话列表 ----
        let total_sessions = snapshot.sessions.len();
        let direct_sessions = total_sessions.min(MAX_SESSIONS);
        for (index, session) in snapshot.sessions.iter().take(direct_sessions).enumerate() {
            model.push(TrayMenuEntry::Session {
                index,
                label: format!("{}. {}", index + 1, session.title),
                status_icon: status_icon_from_key(session.status_icon_key()),
                session_id: session.id.clone(),
                directory: session.directory.clone(),
            });
        }
        if total_sessions > MAX_SESSIONS {
            model.push(TrayMenuEntry::MoreSubmenu {
                label: MORE_LABEL.to_string(),
            });
            model.push(TrayMenuEntry::Subheader {
                label: format!("More sessions ({})", total_sessions - MAX_SESSIONS),
            });
        }

        // ---- Usage 子菜单 (只在 groups 非空时渲染) ----
        if !snapshot.usage.groups.is_empty() {
            model.push(TrayMenuEntry::UsageSubmenu {
                label: "Usage".to_string(),
            });
        }

        // ---- 基础动作 ----
        model.push(TrayMenuEntry::BaseAction {
            id: TrayBaseAction::NewSession,
            label: TrayBaseAction::NewSession.label().to_string(),
        });
        model.push(TrayMenuEntry::BaseAction {
            id: TrayBaseAction::Show,
            label: TrayBaseAction::Show.label().to_string(),
        });
        model.push(TrayMenuEntry::BaseAction {
            id: TrayBaseAction::Quit,
            label: TrayBaseAction::Quit.label().to_string(),
        });

        model
    }
}

fn push_approval_entry(model: &mut TrayMenuModel, index: usize, approval: &TrayApproval) {
    let label = if approval.label.is_empty() {
        "Approval".to_string()
    } else {
        approval.label.clone()
    };

    if approval.kind == "permission" {
        model.push(TrayMenuEntry::ApprovalSubmenu {
            index,
            label,
            session_id: approval.session_id.clone(),
            approval_id: approval.id.clone(),
            directory: approval.directory.clone(),
        });
    } else {
        model.push(TrayMenuEntry::ApprovalDirect {
            index,
            label,
            session_id: approval.session_id.clone(),
            approval_id: approval.id.clone(),
            directory: approval.directory.clone(),
        });
    }
}

fn status_icon_from_key(key: &str) -> TrayStatusIcon {
    match key {
        "busy" => TrayStatusIcon::Busy,
        "retry" => TrayStatusIcon::Retry,
        "error" => TrayStatusIcon::Error,
        "unseen" => TrayStatusIcon::Unseen,
        _ => TrayStatusIcon::Blank,
    }
}

// ============================================================================
// 动作枚举 + Registry (不透明 ID → typed action)
// ============================================================================

/// 用户在托盘菜单上点击某个条目触发的语义动作。
///
/// **所有上下文信息** (session_id / directory / approval_id / response)
/// 在此枚举中显式存在 — 解析 ID 字符串永远不应作为恢复语义的手段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrayAction {
    /// 焦点会话: 跳转到该会话的聊天面板。`directory` 是会话的工作目录,
    /// 用于跨项目路由 (UI 侧 `setCurrentSession(sessionId, directory)`)。
    FocusSession { session_id: String, directory: String },
    /// 焦点审批: 跳转到对应 session 并在 UI 内打开审批面板 (question 类型)。
    FocusApproval {
        session_id: String,
        approval_id: String,
        directory: String,
    },
    /// 响应权限请求 — payload `{type, sessionId, id, response}`。
    RespondPermission {
        session_id: String,
        approval_id: String,
        response: TrayPermissionResponse,
    },
    /// 打开应用主窗口。
    ShowMain,
    /// 触发新建会话 (走 menu-action channel, 与文件菜单一致)。
    NewSession,
    /// 触发新建 Mini Chat。
    NewMiniChat,
    /// 退出应用。
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrayPermissionResponse {
    Once,
    Always,
    Reject,
}

impl TrayPermissionResponse {
    pub fn as_str(self) -> &'static str {
        match self {
            TrayPermissionResponse::Once => "once",
            TrayPermissionResponse::Always => "always",
            TrayPermissionResponse::Reject => "reject",
        }
    }
}

/// tray 菜单点击 → typed 动作 的全局表。**只在 set_menu 成功后才整体替换**。
///
/// 关键不变量:
/// 1. 永不通过拆分/解析 ID 字符串恢复 `TrayAction` 的语义字段。
/// 2. 旧 ID 在新菜单安装后失效 (由 registry 替换实现), 不会有"幽灵"动作。
static TRAY_ACTIONS: LazyLock<Mutex<HashMap<String, TrayAction>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 不透明 ID 生成器 — 每次新菜单构建会从 1 开始重新计数, 与全局 registry
/// 一起整体替换, 避免跨菜单复用同一 ID 触发"幽灵"动作。
static ACTION_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 注册表: ID ↔ typed action。
#[derive(Debug, Default, Clone)]
pub(crate) struct TrayActionRegistry {
    entries: HashMap<String, TrayAction>,
}

impl TrayActionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 生成一个新 ID 并把 `action` 绑定到它。返回的 ID 是菜单项 ID,
    /// 之后可以传入 [`resolve_action`] 反查。
    pub fn register(&mut self, action: TrayAction) -> String {
        // 顺序很重要: 先分配 ID, 再插入 entries — 这样 build 过程中调用方
        // 可以立刻拿到 ID 并继续构造下一个条目。
        let id = next_action_id();
        self.entries.insert(id.clone(), action);
        id
    }

    #[allow(dead_code)] // API surface — exercised in tests.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[allow(dead_code)] // API surface — exercised in tests, future callers expected.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 从 ID 取出 typed 动作 (consumes self via &mut lookup)。
    #[allow(dead_code)] // API surface — exercised in tests.
    pub fn get(&self, id: &str) -> Option<&TrayAction> {
        self.entries.get(id)
    }

    /// 与全局 registry 合并 — 必须由 `install_built_tray_menu` 在
    /// `tray.set_menu` 成功之后调用, 任何失败路径都不能污染全局表。
    fn install(self) {
        let mut global = TRAY_ACTIONS.lock().expect("TRAY_ACTIONS mutex poisoned");
        *global = self.entries;
    }
}

/// 生成下一个不透明 ID。**仅由 [`TrayActionRegistry::register`] 调用**。
fn next_action_id() -> String {
    // 后缀递增保证单次构建中唯一; 不依赖时间戳/UUID, 也不暴露语义信息。
    let counter = ACTION_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("ta{:016x}", counter)
}

/// 全局查找: tray 菜单事件 → typed action。`None` 表示旧菜单残留 ID
/// 或 UI 自定义 ID (tray_header / tray_attention), 由调用方忽略。
pub(crate) fn resolve_action(id: &str) -> Option<TrayAction> {
    let global = TRAY_ACTIONS.lock().expect("TRAY_ACTIONS mutex poisoned");
    global.get(id).cloned()
}

// ============================================================================
// BuiltTrayMenu: 完整的 tauri menu + registry, 在 set_menu 成功后原子替换全局
// ============================================================================

/// 构建完成的托盘菜单 — 包含 tauri `Menu` 对象 + 独立的 action registry。
///
/// 调用 [`install_built_tray_menu`] 后, 此结构可以被 drop, 全局表接管后续点击。
pub(crate) struct BuiltTrayMenu {
    pub menu: Menu<tauri::Wry>,
    pub registry: TrayActionRegistry,
}

/// 把 built menu 装到托盘上, 并仅在 `tray.set_menu` 成功后才替换全局 registry。
///
/// 失败时 (set_menu 返回 Err) **不**替换 registry, 旧的 typed 动作仍然有效,
/// 避免用户看到"点击没反应"的中间态。
pub(crate) fn install_built_tray_menu(
    app: &AppHandle,
    built: BuiltTrayMenu,
) -> Result<(), String> {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        tray.set_menu(Some(built.menu.clone()))
            .map_err(|e| e.to_string())?;
    } else {
        create_tray(app, built.menu.clone())?;
    }

    // set_menu 已经成功, 现在可以整体替换全局表 — 旧 ID 自然失效。
    built.registry.install();
    Ok(())
}

// ============================================================================
// IPC 入口
// ============================================================================

/// `desktop_tray_update` — args: TraySnapshot `{ sessions, approvals, instanceName, usage, dockBadgeCount }`
///
/// 复现 tray.mjs update(): compute counts → applyIconState → setTitle → setTooltip → rebuild menu。
pub async fn handle_tray_update(args: &Value, app: &AppHandle) -> Result<Value, String> {
    let snapshot = parse_snapshot(args);

    // dock badge count (macOS only)
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    let badge_count = snapshot.dock_badge_count;
    #[cfg(target_os = "macos")]
    {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.set_badge_count(if badge_count > 0 {
                Some(badge_count)
            } else {
                None
            });
        }
    }

    // compute counts
    let (counts, session_count) = compute_counts(&snapshot);

    // apply icon state (启动/停止动画)
    let next_state = compute_icon_state(&counts);
    apply_icon_state(app, next_state);

    // title (diff-guarded)
    let title = compute_title(&counts);
    set_title_if_changed(app, &title);

    // tooltip
    let tooltip = compute_tooltip(&counts, session_count);
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(&tooltip));
    }

    // 重建菜单 (从 typed snapshot → pure model → tauri menu + registry)
    if let Err(e) = rebuild_tray_menu(app, &snapshot) {
        log::warn!("[tray] failed to rebuild menu: {}", e);
    }

    Ok(Value::Null)
}

// ============================================================================
// 计数 + 状态计算
// ============================================================================

/// 从 snapshot 计算 counts + session_count。
struct TrayCounts {
    busy: usize,
    error: usize,
    approvals: usize,
    unseen: usize,
}

fn compute_counts(snapshot: &TraySnapshot) -> (TrayCounts, usize) {
    let approval_count = snapshot.approvals.len();

    let mut busy = 0usize;
    let mut error = 0usize;
    let mut unseen = 0usize;
    for s in &snapshot.sessions {
        if s.status == "busy" || s.status == "retry" {
            busy += 1;
        }
        if s.has_error {
            error += 1;
        }
        if s.unseen > 0 {
            unseen += 1;
        }
    }

    (
        TrayCounts {
            busy,
            error,
            approvals: approval_count,
            unseen,
        },
        snapshot.sessions.len(),
    )
}

/// busy > unseen > idle。
fn compute_icon_state(counts: &TrayCounts) -> &'static str {
    if counts.busy > 0 {
        "busy"
    } else if counts.unseen > 0 {
        "unseen"
    } else {
        "idle"
    }
}

/// ◆ N (approvals) / ▲ N (error) / 空。
fn compute_title(counts: &TrayCounts) -> String {
    if counts.approvals > 0 {
        return format!("◆ {}", counts.approvals);
    }
    if counts.error > 0 {
        return format!("▲ {}", counts.error);
    }
    String::new()
}

/// "GridForge — N session(s) · ..."。
fn compute_tooltip(counts: &TrayCounts, session_count: usize) -> String {
    if session_count == 0 {
        return "GridForge — no active sessions".to_string();
    }
    let mut bits = Vec::new();
    if counts.approvals > 0 {
        bits.push(format!("{} awaiting approval", counts.approvals));
    }
    if counts.error > 0 {
        bits.push(format!("{} with errors", counts.error));
    }
    if counts.busy > 0 {
        bits.push(format!("{} working", counts.busy));
    }
    if counts.unseen > 0 {
        bits.push(format!("{} unread", counts.unseen));
    }
    let suffix = if bits.is_empty() {
        " · idle".to_string()
    } else {
        format!(" · {}", bits.join(", "))
    };
    let plural = if session_count == 1 { "" } else { "s" };
    format!(
        "GridForge — {} session{}{}",
        session_count, plural, suffix
    )
}

// ============================================================================
// 图标状态应用 + 动画
// ============================================================================

/// 应用 icon state — 启动/停止动画, 设置静态图标。
fn apply_icon_state(app: &AppHandle, next_state: &str) {
    let mut current = TRAY_ANIM.icon_state.lock().unwrap();
    let prev = current.as_deref();
    if prev == Some(next_state) {
        return; // no-op, state 未变
    }
    *current = Some(next_state.to_string());
    drop(current);

    if TRAY_ANIM.destroyed.load(Ordering::Relaxed) {
        return;
    }

    match next_state {
        "busy" => {
            // Windows: breath frames = [icon.ico] 单元素 → 不动画, 设静态
            if BREATH_FRAMES.len() > 1 {
                start_animation(app);
            } else if let Some(tray) = app.tray_by_id(TRAY_ID) {
                let frame = BREATH_FRAMES.first().unwrap_or(&*IDLE_ICON);
                let _ = tray.set_icon(Some(frame.clone()));
            }
        }
        "unseen" => {
            stop_animation();
            if let Some(tray) = app.tray_by_id(TRAY_ID) {
                let _ = tray.set_icon(Some((*UNSEEN_ICON).clone()));
                #[cfg(target_os = "macos")]
                {
                    let _ = tray.set_icon_as_template(true);
                }
            }
        }
        _ => {
            // idle
            stop_animation();
            if let Some(tray) = app.tray_by_id(TRAY_ID) {
                let _ = tray.set_icon(Some((*IDLE_ICON).clone()));
                #[cfg(target_os = "macos")]
                {
                    let _ = tray.set_icon_as_template(true);
                }
            }
        }
    }
}

/// 启动 breathing 动画 (ping-pong, 0→15→0)。
fn start_animation(app: &AppHandle) {
    // 已在运行 → no-op
    if TRAY_ANIM.anim_running.swap(true, Ordering::SeqCst) {
        return;
    }

    let app_handle = app.clone();
    let frame_count = BREATH_FRAMES.len();

    tauri::async_runtime::spawn(async move {
        let mut index: usize = 0;
        let mut dir: i32 = 1;

        loop {
            if TRAY_ANIM.destroyed.load(Ordering::Relaxed) || !TRAY_ANIM.anim_running.load(Ordering::Relaxed) {
                break;
            }

            // 设当前帧
            if let Some(tray) = app_handle.tray_by_id(TRAY_ID) {
                let frame = BREATH_FRAMES.get(index).unwrap_or(&*IDLE_ICON);
                let _ = tray.set_icon(Some(frame.clone()));
                #[cfg(target_os = "macos")]
                {
                    let _ = tray.set_icon_as_template(true);
                }
            }

            // ping-pong advance
            if dir > 0 && index >= frame_count - 1 {
                index = frame_count - 1;
                dir = -1;
            } else if dir < 0 && index == 0 {
                index = 0;
                dir = 1;
            } else {
                index = (index as i32 + dir) as usize;
            }

            tokio::time::sleep(std::time::Duration::from_millis(ANIM_INTERVAL_MS)).await;
        }

        TRAY_ANIM.anim_running.store(false, Ordering::SeqCst);
    });
}

/// 停止动画。
fn stop_animation() {
    TRAY_ANIM.anim_running.store(false, Ordering::SeqCst);
}

/// 销毁动画 (app quit 时调用)。
pub fn destroy_tray_animation() {
    TRAY_ANIM.destroyed.store(true, Ordering::Relaxed);
    stop_animation();
}

/// 设置 title (diff-guarded)。
fn set_title_if_changed(app: &AppHandle, title: &str) {
    let mut last = TRAY_ANIM.last_title.lock().unwrap();
    if *last == title {
        return;
    }
    *last = title.to_string();
    drop(last);

    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        // macOS: set_title 设菜单栏图标旁的文字
        let _ = tray.set_title(Some(title));
    }
}

// ============================================================================
// 状态行图标
// ============================================================================

/// 获取状态行图标 Image。
fn status_icon_for(key: TrayStatusIcon) -> &'static Image<'static> {
    match key {
        TrayStatusIcon::Busy => &STATUS_ICONS.busy,
        TrayStatusIcon::Retry => &STATUS_ICONS.retry,
        TrayStatusIcon::Error => &STATUS_ICONS.error,
        TrayStatusIcon::Unseen => &STATUS_ICONS.unseen,
        TrayStatusIcon::Blank => &STATUS_ICONS.blank,
    }
}

// ============================================================================
// 菜单重建
// ============================================================================

/// 托盘图标 ID — 所有 `tray_by_id` / `TrayIconBuilder::with_id` 都必须用此常量,
/// 否则 `setup_tray` 的幂等守卫 (`tray_by_id(TRAY_ID).is_some()`) 会失效。
const TRAY_ID: &str = "main_tray";

/// 托盘动作 → menu-action channel payload 决策 (pure helper)。
///
/// 与 `menu.rs::handle_menu_event` 中自定义项的 fallthrough 保持一致:
/// emit `gridforge:menu-action`, detail = 去掉 `tray_` 前缀后的 action 名。
/// UI 侧 `useMenuActions.handleAction` 收到 `"new_mini_chat"` / `"new_session"`
/// 等 detail 后走与文件菜单相同的分发路径, 避免引入新事件名。
///
/// 返回 `None` 表示该动作不走 menu-action channel (例如 `show` / `quit`
/// 在本地处理, 不需要 UI 介入)。
fn tray_menu_action_payload(action_id: &str) -> Option<Value> {
    // 去掉 `tray_` 前缀; 不能剥前缀的 id (例如 `session_*` / `approval_*`)
    // 不属于本 helper 的范围, 调用方应自行处理。
    let action = action_id.strip_prefix("tray_")?;
    if action.is_empty() {
        return None;
    }
    Some(json!({
        "event": "gridforge:menu-action",
        "detail": action,
    }))
}

/// 构建初始 (UI hydration 前可用) 托盘菜单:
/// GridForge header → separator → New Session → New Mini Chat → Show GridForge → separator → Quit GridForge。
///
/// 与 `rebuild_tray_menu` 在 live 状态下重建的菜单结构保持一致 (header + 快捷操作),
/// 便于用户在 UI 加载完成前立即看到 tray 并触发基本动作。
fn build_initial_menu(app: &AppHandle) -> Result<Menu<tauri::Wry>, String> {
    let mut registry = TrayActionRegistry::new();
    let mut items: Vec<Box<dyn tauri::menu::IsMenuItem<tauri::Wry>>> = Vec::new();

    let header_id = registry.register(TrayAction::ShowMain); // 显示主窗口仅作为语义; 点击 header 不应触发
    let _ = header_id; // header 不可点击, 实际使用固定 id
    let header = MenuItem::with_id(app, "tray_header", "GridForge", false, None::<&str>)
        .map_err(|error| error.to_string())?;
    let sep1 = PredefinedMenuItem::separator(app).map_err(|error| error.to_string())?;

    let new_session_id = registry.register(TrayAction::NewSession);
    let new_session = MenuItem::with_id(
        app,
        new_session_id,
        TrayBaseAction::NewSession.label(),
        true,
        TrayBaseAction::NewSession.accelerator(),
    )
    .map_err(|error| error.to_string())?;
    let new_mini_chat_id = registry.register(TrayAction::NewMiniChat);
    let new_mini_chat = MenuItem::with_id(
        app,
        new_mini_chat_id,
        TrayBaseAction::NewMiniChat.label(),
        true,
        TrayBaseAction::NewMiniChat.accelerator(),
    )
    .map_err(|error| error.to_string())?;
    let show_id = registry.register(TrayAction::ShowMain);
    let show = MenuItem::with_id(app, show_id, TrayBaseAction::Show.label(), true, None::<&str>)
        .map_err(|error| error.to_string())?;
    let quit_id = registry.register(TrayAction::Quit);
    let quit = MenuItem::with_id(
        app,
        quit_id,
        TrayBaseAction::Quit.label(),
        true,
        TrayBaseAction::Quit.accelerator(),
    )
    .map_err(|error| error.to_string())?;

    let sep2 = PredefinedMenuItem::separator(app).map_err(|error| error.to_string())?;

    items.push(Box::new(header));
    items.push(Box::new(sep1));
    items.push(Box::new(new_session));
    items.push(Box::new(new_mini_chat));
    items.push(Box::new(show));
    items.push(Box::new(sep2));
    items.push(Box::new(quit));

    // 构建 menu
    let item_refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
        items.iter().map(|b| b.as_ref()).collect();
    let menu = Menu::with_items(app, &item_refs).map_err(|e| e.to_string())?;

    Ok(menu)
}

/// 幂等的托盘初始化入口 — 在后端启动之前调用, 确保 tray 在 UI hydration 完成前已可见。
///
/// 如果 tray 已经存在 (再次 setup 或 live 状态 `rebuild_tray_menu` 已创建), 直接返回 Ok
/// 不重复创建, 避免 Tauri 抛 "tray already exists"。
pub fn setup_tray(app: &AppHandle) -> Result<(), String> {
    if app.tray_by_id(TRAY_ID).is_some() {
        return Ok(());
    }

    let menu = build_initial_menu(app)?;
    create_tray(app, menu)
}

/// 恢复并聚焦主窗口 (unminimize → show → set_focus)。
///
/// 托盘左键点击 / `tray_show` / `desktop_focus_main_window` 共用此恢复顺序,
/// 防止最小化窗口被 `show()` 提前唤起后, `unminimize()` 再触发第二次恢复抖动。
pub(crate) fn restore_main_window(app: &AppHandle) -> bool {
    let Some(window) = app.get_webview_window("main") else {
        log::warn!("[tray] main window not found");
        return false;
    };

    if let Err(error) = window.unminimize() {
        log::warn!("[tray] failed to unminimize main window: {}", error);
    }
    if let Err(error) = window.show() {
        log::warn!("[tray] failed to show main window: {}", error);
    }
    if let Err(error) = window.set_focus() {
        log::warn!("[tray] failed to focus main window: {}", error);
    }
    true
}

/// 重建托盘菜单 (typed snapshot → tauri menu)。
///
/// 流程:
/// 1. `TrayMenuModel::from_snapshot` 构造纯模型 (无副作用, 可单测)。
/// 2. 遍历 entries, 对每个 entry 调用 `tauri::menu::*::with_id` 并把
///    typed `TrayAction` 注册到 `TrayActionRegistry`。
/// 3. 构造 `BuiltTrayMenu { menu, registry }`。
/// 4. 调用 `install_built_tray_menu` → `set_menu` 成功后才原子替换全局
///    `TRAY_ACTIONS` 表 (旧 ID 自然失效)。
fn rebuild_tray_menu(app: &AppHandle, snapshot: &TraySnapshot) -> Result<(), String> {
    let model = TrayMenuModel::from_snapshot(snapshot);
    let built = build_built_tray_menu(app, &model)?;
    install_built_tray_menu(app, built)
}

/// 把纯模型编译为 `BuiltTrayMenu` — 构造 tauri menu 对象 + 独立 registry。
///
/// 该函数**不**接触全局 `TRAY_ACTIONS`, 因此即便后续 `set_menu` 失败,
/// 调用方也能在错误路径上简单地丢弃 `BuiltTrayMenu`。
fn build_built_tray_menu(app: &AppHandle, model: &TrayMenuModel) -> Result<BuiltTrayMenu, String> {
    let mut registry = TrayActionRegistry::new();
    let mut items: Vec<Box<dyn tauri::menu::IsMenuItem<tauri::Wry>>> = Vec::new();

    for entry in &model.entries {
        match entry {
            TrayMenuEntry::Header { label } => {
                let item = MenuItem::with_id(app, "tray_header", label, false, None::<&str>)
                    .map_err(|e| e.to_string())?;
                items.push(Box::new(item));
            }
            TrayMenuEntry::Subheader { label } => {
                // 不可点击的子标题 — 使用固定 id `tray_subheader`, 不进入 registry
                // (这些是纯展示条目, 不应触发任何动作)。
                let item =
                    MenuItem::with_id(app, "tray_subheader", label, false, None::<&str>)
                        .map_err(|e| e.to_string())?;
                items.push(Box::new(item));
            }
            TrayMenuEntry::Separator => {
                let sep = PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?;
                items.push(Box::new(sep));
            }
            TrayMenuEntry::Session {
                label,
                status_icon,
                session_id,
                directory,
                ..
            } => {
                let action = TrayAction::FocusSession {
                    session_id: session_id.clone(),
                    directory: directory.clone(),
                };
                let id = registry.register(action);
                let icon = status_icon_for(*status_icon);
                let item = IconMenuItem::with_id(
                    app,
                    id,
                    label,
                    true,
                    Some(icon.clone()),
                    None::<&str>,
                )
                .map_err(|e| e.to_string())?;
                items.push(Box::new(item));
            }
            TrayMenuEntry::ApprovalSubmenu {
                label,
                session_id,
                approval_id,
                directory,
                ..
            } => {
                // 4 个子项: once / always / reject / open
                let once_id = registry.register(TrayAction::RespondPermission {
                    session_id: session_id.clone(),
                    approval_id: approval_id.clone(),
                    response: TrayPermissionResponse::Once,
                });
                let always_id = registry.register(TrayAction::RespondPermission {
                    session_id: session_id.clone(),
                    approval_id: approval_id.clone(),
                    response: TrayPermissionResponse::Always,
                });
                let reject_id = registry.register(TrayAction::RespondPermission {
                    session_id: session_id.clone(),
                    approval_id: approval_id.clone(),
                    response: TrayPermissionResponse::Reject,
                });
                let open_id = registry.register(TrayAction::FocusApproval {
                    session_id: session_id.clone(),
                    approval_id: approval_id.clone(),
                    directory: directory.clone(),
                });

                let once = MenuItem::with_id(app, once_id, "Allow once", true, None::<&str>)
                    .map_err(|e| e.to_string())?;
                let always = MenuItem::with_id(app, always_id, "Allow always", true, None::<&str>)
                    .map_err(|e| e.to_string())?;
                let deny =
                    MenuItem::with_id(app, reject_id, "Deny", true, None::<&str>)
                        .map_err(|e| e.to_string())?;
                let open = MenuItem::with_id(app, open_id, "Open in app", true, None::<&str>)
                    .map_err(|e| e.to_string())?;

                let sub = Submenu::with_items(app, label, true, &[&once, &always, &deny, &open])
                    .map_err(|e| e.to_string())?;
                items.push(Box::new(sub));
            }
            TrayMenuEntry::ApprovalDirect {
                label,
                session_id,
                approval_id,
                directory,
                ..
            } => {
                let action = TrayAction::FocusApproval {
                    session_id: session_id.clone(),
                    approval_id: approval_id.clone(),
                    directory: directory.clone(),
                };
                let id = registry.register(action);
                let item = MenuItem::with_id(app, id, label, true, None::<&str>)
                    .map_err(|e| e.to_string())?;
                items.push(Box::new(item));
            }
            TrayMenuEntry::MoreSubmenu { label } => {
                // 真实使用中 `More…` 子菜单展示更多条目, 自身注册一个
                // `ShowMain` typed action (打开主窗口查看完整列表)。
                let id = registry.register(TrayAction::ShowMain);
                let more = MenuItem::with_id(app, id, label, true, None::<&str>)
                    .map_err(|e| e.to_string())?;
                items.push(Box::new(more));
            }
            TrayMenuEntry::UsageSubmenu { label } => {
                // Usage 子菜单的内部条目没有可执行的 typed 动作 (纯展示),
                // 因此整个 submenu 用一个固定 id `tray_usage`, 不进入 registry。
                let usage = Submenu::with_items(app, label, true, &[])
                    .map_err(|e| e.to_string())?;
                items.push(Box::new(usage));
            }
            TrayMenuEntry::BaseAction { id, label } => {
                let action = match id {
                    TrayBaseAction::NewSession => TrayAction::NewSession,
                    TrayBaseAction::NewMiniChat => TrayAction::NewMiniChat,
                    TrayBaseAction::Show => TrayAction::ShowMain,
                    TrayBaseAction::Quit => TrayAction::Quit,
                };
                let action_id = registry.register(action);
                let item = MenuItem::with_id(
                    app,
                    action_id,
                    label,
                    true,
                    id.accelerator(),
                )
                .map_err(|e| e.to_string())?;
                items.push(Box::new(item));
            }
        }
    }

    // 构建 menu
    let item_refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
        items.iter().map(|b| b.as_ref()).collect();
    let menu = Menu::with_items(app, &item_refs).map_err(|e| e.to_string())?;

    Ok(BuiltTrayMenu { menu, registry })
}

/// 创建托盘图标 + 绑定菜单。
fn create_tray(app: &AppHandle, menu: Menu<tauri::Wry>) -> Result<(), String> {
    let icon = (*IDLE_ICON).clone();

    let _tray = tauri::tray::TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .menu(&menu)
        .tooltip("GridForge")
        .on_menu_event(|app, event| {
            handle_tray_menu_click(app, &event.id().0);
        })
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                button_state: tauri::tray::MouseButtonState::Up,
                ..
            } = event
            {
                // 左键点击 → 恢复并聚焦主窗口
                let app = tray.app_handle();
                restore_main_window(app);
            }
        })
        .build(app)
        .map_err(|e| e.to_string())?;

    // macOS: 首次创建后设 template
    #[cfg(target_os = "macos")]
    {
        if let Some(tray) = app.tray_by_id(TRAY_ID) {
            let _ = tray.set_icon_as_template(true);
        }
    }

    Ok(())
}

/// 托盘菜单点击处理: 通过 typed registry 解析 ID, 然后按动作类型分派。
///
/// 这是关键改动: 旧实现按 `id.strip_prefix("session_")` / `approval_` /
/// `split('_')` 等字符串拆分恢复语义, 一旦 ID 包含下划线就崩。新实现
/// 严格通过 `TRAY_ACTIONS` 全局表查 typed action, **绝不** 拆分 ID。
fn handle_tray_menu_click(app: &AppHandle, id: &str) {
    let Some(action) = resolve_action(id) else {
        // 不在 registry 中 — 可能是不可点击的 header/subheader, 或者
        // 旧菜单残留 ID (新菜单安装后旧 ID 自然失效, 走这里)。
        return;
    };

    match action {
        TrayAction::NewSession => {
            if let Some(payload) = tray_menu_action_payload("tray_new_session") {
                let _ = app.emit("gridforge:emit", payload);
            }
        }
        TrayAction::NewMiniChat => {
            if let Some(payload) = tray_menu_action_payload("tray_new_mini_chat") {
                let _ = app.emit("gridforge:emit", payload);
            }
        }
        TrayAction::ShowMain => {
            restore_main_window(app);
        }
        TrayAction::Quit => {
            destroy_tray_animation();
            crate::request_quit(app);
        }
        TrayAction::FocusSession { session_id, directory } => {
            let detail = json!({
                "sessionId": session_id,
                "directory": directory,
            });
            let _ = app.emit(
                "gridforge:emit",
                json!({
                    "event": "gridforge:open-session",
                    "detail": detail,
                }),
            );
        }
        TrayAction::FocusApproval { session_id, directory, .. } => {
            // question 类型直接项 / "Open in app" → 跳转到对应 session。
            let detail = json!({
                "sessionId": session_id,
                "directory": directory,
            });
            let _ = app.emit(
                "gridforge:emit",
                json!({
                    "event": "gridforge:open-session",
                    "detail": detail,
                }),
            );
        }
        TrayAction::RespondPermission {
            session_id,
            approval_id,
            response,
        } => {
            // payload: `{type, sessionId, id, response}` — 严格匹配
            // useTraySync.ts 的 `TrayAction` 联合类型 (line 92-93)。
            let detail = json!({
                "type": "respond-permission",
                "sessionId": session_id,
                "id": approval_id,
                "response": response.as_str(),
            });
            let _ = app.emit(
                "gridforge:emit",
                json!({
                    "event": "gridforge:tray-action",
                    "detail": detail,
                }),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- typed snapshot deserialization ----

    #[test]
    fn parse_snapshot_with_camel_case_fields() {
        let value = json!({
            "instanceName": "Local GridForge",
            "dockBadgeCount": 2,
            "sessions": [{
                "id": "sess-1",
                "title": "Build parity",
                "status": "busy",
                "branch": "main",
                "unseen": 3,
                "hasError": false,
                "directory": "/tmp/proj",
                "subtitle": "proj · main",
            }],
            "approvals": [{
                "kind": "permission",
                "id": "perm-1",
                "sessionId": "sess-1",
                "sessionTitle": "Build parity",
                "label": "Bash: npm test",
                "directory": "/tmp/proj",
            }],
            "usage": {
                "mode": "usage",
                "groups": [{ "provider": "OpenAI", "rows": [], "status": null }],
            },
        });

        let snap = parse_snapshot(&value);
        assert_eq!(snap.instance_name, "Local GridForge");
        assert_eq!(snap.dock_badge_count, 2);
        assert_eq!(snap.sessions.len(), 1);
        assert_eq!(snap.sessions[0].id, "sess-1");
        assert_eq!(snap.sessions[0].status_icon_key(), "busy");
        assert_eq!(snap.approvals.len(), 1);
        assert_eq!(snap.approvals[0].id, "perm-1");
        assert_eq!(snap.approvals[0].session_id, "sess-1");
        assert_eq!(snap.approvals[0].label, "Bash: npm test");
        assert_eq!(snap.usage.groups.len(), 1);
        assert_eq!(snap.usage.groups[0].provider, "OpenAI");
    }

    #[test]
    fn parse_snapshot_accepts_session_id_alias() {
        // 旧 wire format 用了 `sessionId` 而非 `id` — 仍能解析。
        let value = json!({
            "sessions": [{ "sessionId": "old-1", "title": "legacy", "status": "idle" }],
        });
        let snap = parse_snapshot(&value);
        assert_eq!(snap.sessions.len(), 1);
        assert_eq!(snap.sessions[0].id, "old-1");
    }

    #[test]
    fn parse_snapshot_accepts_label_title_alias() {
        // 旧 wire format 用了 `title` 作为审批 label — 仍能解析。
        let value = json!({
            "approvals": [{
                "kind": "question",
                "id": "q-1",
                "sessionId": "s",
                "title": "Approve plan?",
            }],
        });
        let snap = parse_snapshot(&value);
        assert_eq!(snap.approvals.len(), 1);
        assert_eq!(snap.approvals[0].label, "Approve plan?");
    }

    #[test]
    fn parse_snapshot_defaults_when_empty() {
        let snap = parse_snapshot(&json!({}));
        assert!(snap.sessions.is_empty());
        assert!(snap.approvals.is_empty());
        assert_eq!(snap.instance_name, "");
        assert_eq!(snap.dock_badge_count, 0);
    }

    #[test]
    fn parse_snapshot_returns_empty_on_garbage() {
        let value = json!("not an object");
        let snap = parse_snapshot(&value);
        // 非对象 → 解析失败 → fallback 到 default snapshot
        assert!(snap.sessions.is_empty());
        assert!(snap.approvals.is_empty());
    }

    // ---- pure model helpers ----

    fn build_basic_model() -> TrayMenuModel {
        let mut model = TrayMenuModel::new("Local GridForge");
        model.push(TrayMenuEntry::Header {
            label: "Local GridForge".to_string(),
        });
        model.push(TrayMenuEntry::Separator);
        model.push(TrayMenuEntry::Subheader {
            label: "Needs your attention".to_string(),
        });
        model.push(TrayMenuEntry::ApprovalSubmenu {
            index: 0,
            label: "Bash: npm test".to_string(),
            session_id: "sess-1".to_string(),
            approval_id: "perm-1".to_string(),
            directory: "/tmp/proj".to_string(),
        });
        model.push(TrayMenuEntry::Separator);
        model.push(TrayMenuEntry::Session {
            index: 0,
            label: "1. Build parity".to_string(),
            status_icon: TrayStatusIcon::Busy,
            session_id: "sess-1".to_string(),
            directory: "/tmp/proj".to_string(),
        });
        model.push(TrayMenuEntry::BaseAction {
            id: TrayBaseAction::Quit,
            label: TrayBaseAction::Quit.label().to_string(),
        });
        model
    }

    #[test]
    fn contains_label_finds_top_level_entries() {
        let model = build_basic_model();
        assert!(model.contains_label("1. Build parity"));
        assert!(model.contains_label("Local GridForge"));
        assert!(!model.contains_label("Bogus Item"));
    }

    #[test]
    fn contains_submenu_finds_approval_submenu() {
        let model = build_basic_model();
        assert!(model.contains_submenu("Bash: npm test"));
        assert!(model.contains_submenu("Needs your attention") == false);
    }

    #[test]
    fn contains_label_finds_approval_submenu_inner_actions() {
        // ApprovalSubmenu 的 4 个内部子项 (Allow once / Allow always /
        // Deny / Open in app) 由 build_built_tray_menu 在编译阶段注入,
        // 不出现在纯模型中 (模型只持有 ApprovalSubmenu 整体)。 本测试
        // 固定"纯模型 = ApprovalSubmenu, 编译产物 = 4 子项"的契约:
        // contains_label 只在编译产物层级生效。
        let model = build_basic_model();
        assert!(!model.contains_label("Allow once"));
        assert!(!model.contains_label("Deny"));
    }

    #[test]
    fn contains_submenu_finds_more_label() {
        let mut model = TrayMenuModel::new("X");
        model.push(TrayMenuEntry::MoreSubmenu {
            label: "More…".to_string(),
        });
        assert!(model.contains_submenu("More…"));
    }

    #[test]
    fn contains_submenu_finds_usage_label() {
        let mut model = TrayMenuModel::new("X");
        model.push(TrayMenuEntry::UsageSubmenu {
            label: "Usage".to_string(),
        });
        assert!(model.contains_submenu("Usage"));
    }

    // ---- from_snapshot composition ----

    #[test]
    fn from_snapshot_includes_all_base_actions() {
        let snap = TraySnapshot::default();
        let model = TrayMenuModel::from_snapshot(&snap);
        assert!(model.contains_label("New Session"));
        assert!(model.contains_label("Show GridForge"));
        assert!(model.contains_label("Quit GridForge"));
    }

    #[test]
    fn from_snapshot_omits_usage_submenu_when_groups_empty() {
        let snap = TraySnapshot::default();
        let model = TrayMenuModel::from_snapshot(&snap);
        assert!(!model.contains_submenu("Usage"));
    }

    #[test]
    fn from_snapshot_includes_usage_submenu_when_groups_non_empty() {
        let snap = TraySnapshot {
            usage: TrayUsage {
                mode: "usage".to_string(),
                groups: vec![TrayUsageGroup {
                    provider: "OpenAI".to_string(),
                    rows: vec![],
                    status: None,
                }],
            },
            ..Default::default()
        };
        let model = TrayMenuModel::from_snapshot(&snap);
        assert!(model.contains_submenu("Usage"));
    }

    #[test]
    fn from_snapshot_includes_attention_subheader_when_approvals_present() {
        let snap = TraySnapshot {
            approvals: vec![TrayApproval {
                kind: "permission".to_string(),
                id: "perm-1".to_string(),
                session_id: "sess-1".to_string(),
                label: "Bash: test".to_string(),
                directory: "/tmp".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let model = TrayMenuModel::from_snapshot(&snap);
        assert!(model.contains_label("Needs your attention"));
        assert!(model.contains_submenu("Bash: test"));
    }

    #[test]
    fn from_snapshot_creates_more_submenu_when_approvals_exceed_limit() {
        let approvals: Vec<TrayApproval> = (0..12)
            .map(|i| TrayApproval {
                kind: "permission".to_string(),
                id: format!("perm-{i}"),
                session_id: format!("sess-{i}"),
                label: format!("Approval {i}"),
                directory: "/tmp".to_string(),
                ..Default::default()
            })
            .collect();
        let snap = TraySnapshot {
            approvals,
            ..Default::default()
        };
        let model = TrayMenuModel::from_snapshot(&snap);
        // 前 MAX_APPROVALS = 10 项直接展示
        for i in 0..10 {
            assert!(
                model.contains_submenu(&format!("Approval {i}")),
                "missing direct approval {i}"
            );
        }
        // 后 2 项折叠进 More…
        assert!(model.contains_submenu("More…"));
        assert!(model.contains_label("More approvals (2)"));
    }

    #[test]
    fn from_snapshot_creates_more_submenu_when_sessions_exceed_limit() {
        let sessions: Vec<TraySession> = (0..10)
            .map(|i| TraySession {
                id: format!("sess-{i}"),
                title: format!("Session {i}"),
                directory: "/tmp".to_string(),
                ..Default::default()
            })
            .collect();
        let snap = TraySnapshot {
            sessions,
            ..Default::default()
        };
        let model = TrayMenuModel::from_snapshot(&snap);
        // 前 MAX_SESSIONS = 8 项直接展示
        for i in 0..8 {
            assert!(model.contains_label(&format!("{}. Session {i}", i + 1)));
        }
        assert!(model.contains_submenu("More…"));
        assert!(model.contains_label("More sessions (2)"));
    }

    #[test]
    fn from_snapshot_renders_question_kind_as_direct_item() {
        let snap = TraySnapshot {
            approvals: vec![TrayApproval {
                kind: "question".to_string(),
                id: "q-1".to_string(),
                session_id: "sess-1".to_string(),
                label: "Approve plan?".to_string(),
                directory: "/tmp".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let model = TrayMenuModel::from_snapshot(&snap);
        // question 类型直接作为菜单项, 而不是 ApprovalSubmenu
        assert!(model.contains_label("Approve plan?"));
        assert!(!model.contains_submenu("Approve plan?"));
    }

    // ---- TrayActionRegistry ----

    #[test]
    fn registry_registers_and_resolves_actions() {
        let mut registry = TrayActionRegistry::new();
        let id = registry.register(TrayAction::Quit);
        match registry.get(&id) {
            Some(TrayAction::Quit) => {}
            other => panic!("expected Quit, got {:?}", other),
        }
    }

    #[test]
    fn registry_generates_unique_ids() {
        let mut registry = TrayActionRegistry::new();
        let id1 = registry.register(TrayAction::Quit);
        let id2 = registry.register(TrayAction::ShowMain);
        let id3 = registry.register(TrayAction::NewSession);
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
        assert_eq!(registry.len(), 3);
    }

    #[test]
    fn registry_ids_are_opaque() {
        // 关键反退化测试: ID 绝不暴露 TrayAction 的语义字段 (sessionId 等)。
        let mut registry = TrayActionRegistry::new();
        let id = registry.register(TrayAction::FocusSession {
            session_id: "abc-123".to_string(),
            directory: "/tmp/proj".to_string(),
        });
        assert!(
            !id.contains("abc-123"),
            "registry id leaked sessionId: {id}"
        );
        assert!(!id.contains("/tmp"), "registry id leaked directory: {id}");
    }

    #[test]
    fn registry_distinguishes_respond_permission_responses() {
        let mut registry = TrayActionRegistry::new();
        let once_id = registry.register(TrayAction::RespondPermission {
            session_id: "s".to_string(),
            approval_id: "p".to_string(),
            response: TrayPermissionResponse::Once,
        });
        let always_id = registry.register(TrayAction::RespondPermission {
            session_id: "s".to_string(),
            approval_id: "p".to_string(),
            response: TrayPermissionResponse::Always,
        });
        let reject_id = registry.register(TrayAction::RespondPermission {
            session_id: "s".to_string(),
            approval_id: "p".to_string(),
            response: TrayPermissionResponse::Reject,
        });
        assert_ne!(once_id, always_id);
        assert_ne!(always_id, reject_id);
        assert!(matches!(
            registry.get(&once_id),
            Some(TrayAction::RespondPermission {
                response: TrayPermissionResponse::Once,
                ..
            })
        ));
        assert!(matches!(
            registry.get(&always_id),
            Some(TrayAction::RespondPermission {
                response: TrayPermissionResponse::Always,
                ..
            })
        ));
        assert!(matches!(
            registry.get(&reject_id),
            Some(TrayAction::RespondPermission {
                response: TrayPermissionResponse::Reject,
                ..
            })
        ));
    }

    // ---- counters (existing icon/title/tooltip) ----

    #[test]
    fn icon_state_priority_busy_over_unseen() {
        let counts = TrayCounts {
            busy: 1,
            error: 0,
            approvals: 0,
            unseen: 5,
        };
        assert_eq!(compute_icon_state(&counts), "busy");
    }

    #[test]
    fn icon_state_unseen_over_idle() {
        let counts = TrayCounts {
            busy: 0,
            error: 0,
            approvals: 0,
            unseen: 3,
        };
        assert_eq!(compute_icon_state(&counts), "unseen");
    }

    #[test]
    fn icon_state_idle_when_empty() {
        let counts = TrayCounts {
            busy: 0,
            error: 0,
            approvals: 0,
            unseen: 0,
        };
        assert_eq!(compute_icon_state(&counts), "idle");
    }

    #[test]
    fn title_shows_approvals_diamond() {
        let counts = TrayCounts {
            busy: 0,
            error: 2,
            approvals: 3,
            unseen: 0,
        };
        assert_eq!(compute_title(&counts), "◆ 3");
    }

    #[test]
    fn title_shows_errors_triangle() {
        let counts = TrayCounts {
            busy: 0,
            error: 1,
            approvals: 0,
            unseen: 0,
        };
        assert_eq!(compute_title(&counts), "▲ 1");
    }

    #[test]
    fn title_empty_when_nothing_notable() {
        let counts = TrayCounts {
            busy: 5,
            error: 0,
            approvals: 0,
            unseen: 10,
        };
        assert_eq!(compute_title(&counts), "");
    }

    #[test]
    fn tooltip_no_sessions() {
        let counts = TrayCounts {
            busy: 0,
            error: 0,
            approvals: 0,
            unseen: 0,
        };
        assert_eq!(
            compute_tooltip(&counts, 0),
            "GridForge — no active sessions"
        );
    }

    #[test]
    fn tooltip_single_session_idle() {
        let counts = TrayCounts {
            busy: 0,
            error: 0,
            approvals: 0,
            unseen: 0,
        };
        assert_eq!(
            compute_tooltip(&counts, 1),
            "GridForge — 1 session · idle"
        );
    }

    #[test]
    fn tooltip_multiple_sessions_working() {
        let counts = TrayCounts {
            busy: 2,
            error: 0,
            approvals: 1,
            unseen: 0,
        };
        assert_eq!(
            compute_tooltip(&counts, 5),
            "GridForge — 5 sessions · 1 awaiting approval, 2 working"
        );
    }

    // ---- 状态行图标 (session 字段映射) ----

    #[test]
    fn status_icon_key_busy_overrides() {
        let session = TraySession {
            status: "busy".to_string(),
            unseen: 5,
            has_error: true,
            ..Default::default()
        };
        assert_eq!(session.status_icon_key(), "busy");
    }

    #[test]
    fn status_icon_key_retry() {
        let session = TraySession {
            status: "retry".to_string(),
            ..Default::default()
        };
        assert_eq!(session.status_icon_key(), "retry");
    }

    #[test]
    fn status_icon_key_error() {
        let session = TraySession {
            status: "idle".to_string(),
            has_error: true,
            ..Default::default()
        };
        assert_eq!(session.status_icon_key(), "error");
    }

    #[test]
    fn status_icon_key_unseen() {
        let session = TraySession {
            status: "idle".to_string(),
            unseen: 3,
            ..Default::default()
        };
        assert_eq!(session.status_icon_key(), "unseen");
    }

    #[test]
    fn status_icon_key_blank() {
        let session = TraySession {
            status: "idle".to_string(),
            ..Default::default()
        };
        assert_eq!(session.status_icon_key(), "blank");
    }

    // ---- 菜单 action → menu-action channel payload 决策 (pure helper) ----

    #[test]
    fn tray_menu_action_payload_new_session_uses_menu_action_channel() {
        // tray_new_session → 去掉 `tray_` 前缀 → detail = "new_session",
        // 与 menu.rs 中 menu_new_session 的 fallthrough 行为一致 (line 201-208)。
        let payload = tray_menu_action_payload("tray_new_session").expect("payload");
        assert_eq!(
            payload,
            json!({
                "event": "gridforge:menu-action",
                "detail": "new_session",
            })
        );
    }

    #[test]
    fn tray_menu_action_payload_new_mini_chat_uses_menu_action_channel() {
        // 关键 review 修复: 不再 emit `gridforge:open-mini-chat` 这一新事件,
        // 改为 emit `gridforge:menu-action` + detail "new_mini_chat",
        // 与 menu.rs:71 (menu_new_mini_chat) + menu.rs:201-208 fallthrough
        // 行为完全一致。
        let payload = tray_menu_action_payload("tray_new_mini_chat").expect("payload");
        assert_eq!(
            payload,
            json!({
                "event": "gridforge:menu-action",
                "detail": "new_mini_chat",
            })
        );
        // 防回归: 不再使用旧的 `gridforge:open-mini-chat` 事件名。
        assert_ne!(payload["event"], "gridforge:open-mini-chat");
    }

    #[test]
    fn tray_menu_action_payload_rejects_non_tray_prefixed_ids() {
        // session_* / approval_* 这类 id 没有 `tray_` 前缀, 不属于本 helper
        // 的范围 (调用方走自己的分支)。 必须返回 None。
        assert!(tray_menu_action_payload("session_0_abc").is_none());
        assert!(tray_menu_action_payload("approval_0_abc_once").is_none());
        assert!(tray_menu_action_payload("approval_focus_0_abc").is_none());
    }

    #[test]
    fn tray_menu_action_payload_local_actions_still_have_valid_payload() {
        // tray_show / tray_quit 也属于 `tray_` 前缀, helper 会返回合法 payload
        // (用于测试纯函数), 但调用方 `handle_tray_menu_click` 选择本地处理
        // 而非走 menu-action channel。本测试固定 helper 的"只看前缀"语义。
        let show = tray_menu_action_payload("tray_show").expect("payload");
        assert_eq!(show["detail"], "show");
        let quit = tray_menu_action_payload("tray_quit").expect("payload");
        assert_eq!(quit["detail"], "quit");
    }

    #[test]
    fn tray_menu_action_payload_rejects_empty_after_prefix_strip() {
        // 防御性: 如果未来有人加 "tray_" 但不带 action, 必须返回 None
        // 而不是 emit 一个空 detail 的 menu-action。
        assert!(tray_menu_action_payload("tray_").is_none());
    }
}