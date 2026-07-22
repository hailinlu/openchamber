//! Session-goal metadata — GoalMetadata 全套字段 + GOAL_STATUSES 枚举 +
//! session.metadata.gridforge.goal 的规范化与 merge。
//!
//! 对应 Node `session-goal/runtime.js` 的 `parseGoalMetadata` + `GOAL_STATUSES`。
//!
//! Wire format: 全 camelCase, 数字字段 (tokenBudget/tokensUsed/turnsUsed/...)
//! 都做 `Number.isFinite() > 0` 校验后 floor 存储。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Goal 状态枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    BudgetLimited,
    Complete,
}

impl GoalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::Blocked => "blocked",
            GoalStatus::BudgetLimited => "budgetLimited",
            GoalStatus::Complete => "complete",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "active" => Some(GoalStatus::Active),
            "paused" => Some(GoalStatus::Paused),
            "blocked" => Some(GoalStatus::Blocked),
            "budgetlimited" => Some(GoalStatus::BudgetLimited),
            "complete" => Some(GoalStatus::Complete),
            _ => None,
        }
    }
}

/// GoalStatus 字符串集合 — 校验来自 wire 的 status 字段。
pub const GOAL_STATUSES: &[&str] = &["active", "paused", "blocked", "budgetLimited", "complete"];

/// GoalMetadata — 对应 session.metadata.gridforge.goal。
///
/// UI 通过该对象读 goal 状态。注意: 所有数字字段 (tokenBudget 等) 都是 Option<u64> —
/// `tokenBudget` 缺省为 None; 其他累加字段缺省为 0。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GoalMetadata {
    /// Goal 唯一 id (UI 生成)。
    #[serde(default)]
    pub id: String,

    /// Objective 文本 (inline mode); 文件模式下为空 + objectiveFile=true。
    #[serde(default)]
    pub objective: String,

    /// 文件模式: objective 文本由 $DATA_DIR/goals/<session_id>.md 提供 (可被 UI 实时编辑)。
    #[serde(default)]
    pub objective_file: bool,

    /// Goal 状态。
    #[serde(default)]
    pub status: String,

    /// 用户设定的 token 上限 (None = 无上限)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,

    /// 累计已用 tokens。
    #[serde(default)]
    pub tokens_used: u64,

    /// Pre-goal 历史 token baseline (首次 tick 锁定)。
    #[serde(default)]
    pub tokens_baseline: u64,

    /// 已关闭段 (compaction) 的累计 token 数。
    #[serde(default)]
    pub tokens_committed: u64,

    /// 已使用的 auto-continuation 次数。
    #[serde(default)]
    pub turns_used: u32,

    /// 连续 audit `blocked` 次数。
    #[serde(default)]
    pub blocked_streak: u32,

    /// 连续 audit 失败次数。
    #[serde(default)]
    pub audit_fail_streak: u32,

    /// Audit note (≤ NOTE_CHAR_LIMIT chars)。
    #[serde(default)]
    pub note: String,

    /// 最近状态变更原因 (≤ REASON_CHAR_LIMIT chars)。
    #[serde(default)]
    pub status_reason: String,

    /// 最近一次纳入计费的 assistant message id (字符串比较大小)。
    #[serde(default)]
    pub last_accounted_message_id: String,

    /// Goal 创建时间 (millis)。
    #[serde(default)]
    pub created_at: i64,

    /// 最近更新时间 (millis)。
    #[serde(default)]
    pub updated_at: i64,
}

/// 从 session 对象中规范化 `metadata.gridforge.goal` 字段。
///
/// 对应 Node `parseGoalMetadata(session)`。
/// 返回 None 表示 goal 不存在或字段无效。
pub fn parse_goal_metadata(session: &Value) -> Option<GoalMetadata> {
    let metadata = session.get("metadata")?;
    let namespace = metadata.get("gridforge")?;
    let goal = namespace.get("goal")?;
    let goal_obj = goal.as_object()?;

    let id = goal_obj.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let status_str = goal_obj.get("status").and_then(Value::as_str).unwrap_or("").to_string();
    let objective = goal_obj
        .get("objective")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let objective_file = goal_obj
        .get("objectiveFile")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if id.is_empty()
        || !GOAL_STATUSES.iter().any(|s| *s == status_str)
        || (objective.is_empty() && !objective_file)
    {
        return None;
    }

    // objective 截断到 GOAL_OBJECTIVE_CHAR_LIMIT
    use super::objectives::GOAL_OBJECTIVE_CHAR_LIMIT;
    let objective = objective.chars().take(GOAL_OBJECTIVE_CHAR_LIMIT).collect::<String>();

    // 数字字段: `Number.isFinite(x) && x > 0 ? floor(x) : default`
    let token_budget = goal_obj
        .get("tokenBudget")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n.floor() as u64);
    let tokens_used = goal_obj
        .get("tokensUsed")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n.floor() as u64)
        .unwrap_or(0);
    let tokens_baseline = goal_obj
        .get("tokensBaseline")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n.floor() as u64)
        .unwrap_or(0);
    let tokens_committed = goal_obj
        .get("tokensCommitted")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n.floor() as u64)
        .unwrap_or(0);
    let turns_used = goal_obj
        .get("turnsUsed")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n.floor() as u32)
        .unwrap_or(0);
    let blocked_streak = goal_obj
        .get("blockedStreak")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n.floor() as u32)
        .unwrap_or(0);
    let audit_fail_streak = goal_obj
        .get("auditFailStreak")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n.floor() as u32)
        .unwrap_or(0);

    const NOTE_CHAR_LIMIT: usize = 280;
    const REASON_CHAR_LIMIT: usize = 200;
    let note = goal_obj
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(NOTE_CHAR_LIMIT)
        .collect::<String>();
    let status_reason = goal_obj
        .get("statusReason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(REASON_CHAR_LIMIT)
        .collect::<String>();
    let last_accounted_message_id = goal_obj
        .get("lastAccountedMessageID")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let created_at = goal_obj
        .get("createdAt")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .map(|n| n as i64)
        .unwrap_or(0);
    let updated_at = goal_obj
        .get("updatedAt")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .map(|n| n as i64)
        .unwrap_or(0);

    Some(GoalMetadata {
        id,
        objective,
        objective_file,
        status: status_str,
        token_budget,
        tokens_used,
        tokens_baseline,
        tokens_committed,
        turns_used,
        blocked_streak,
        audit_fail_streak,
        note,
        status_reason,
        last_accounted_message_id,
        created_at,
        updated_at,
    })
}

/// 从 session metadata 中读取 `openchamber.goal` 字段 (Value 形式, 用于持久化前的 merge)。
pub fn read_goal_value_from_session(session: &Value) -> Option<Value> {
    let metadata = session.get("metadata")?;
    let namespace = metadata.get("gridforge")?;
    namespace.get("goal").cloned()
}

/// 在 session metadata 上合并写入 `openchamber.goal` — 保留其他 `openchamber.*` 子字段。
///
/// 对应 Node `writeGoal()` 中合并 metadata 的部分。
pub fn merge_goal_into_session_metadata(session: &Value, new_goal: &Value) -> Value {
    let metadata = session
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(Map::new);
    let mut metadata = metadata;

    let namespace = metadata
        .get("gridforge")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(Map::new);
    let mut namespace = namespace;
    namespace.insert("goal".to_string(), new_goal.clone());

    metadata.insert("gridforge".to_string(), Value::Object(namespace));
    let mut session_obj = session
        .as_object()
        .cloned()
        .unwrap_or_else(Map::new);
    session_obj.insert("metadata".to_string(), Value::Object(metadata));
    Value::Object(session_obj)
}

/// 把 GoalMetadata 序列化为 JSON (用于持久化)。
pub fn goal_to_value(goal: &GoalMetadata) -> Value {
    serde_json::to_value(goal).expect("GoalMetadata always serializable")
}

/// GoalMetadata 是否处于 active 状态 — guard helper。
pub fn is_active(goal: &GoalMetadata) -> bool {
    goal.status == "active"
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn goal_status_round_trip() {
        for s in GOAL_STATUSES.iter() {
            let parsed = GoalStatus::parse(s).unwrap();
            assert_eq!(parsed.as_str(), *s);
        }
        assert!(GoalStatus::parse("unknown").is_none());
    }

    #[test]
    fn parse_goal_metadata_minimal() {
        let session = json!({
            "id": "sess_1",
            "metadata": {
                "gridforge": {
                    "goal": {
                        "id": "goal_1",
                        "objective": "build widget",
                        "status": "active"
                    }
                }
            }
        });
        let g = parse_goal_metadata(&session).unwrap();
        assert_eq!(g.id, "goal_1");
        assert_eq!(g.objective, "build widget");
        assert_eq!(g.status, "active");
        assert!(!g.objective_file);
        assert_eq!(g.turns_used, 0);
    }

    #[test]
    fn parse_goal_metadata_file_mode() {
        let session = json!({
            "metadata": {
                "gridforge": {
                    "goal": {
                        "id": "g",
                        "objectiveFile": true,
                        "status": "paused"
                    }
                }
            }
        });
        let g = parse_goal_metadata(&session).unwrap();
        assert!(g.objective_file);
        assert_eq!(g.status, "paused");
    }

    #[test]
    fn parse_goal_metadata_rejects_invalid_status() {
        let session = json!({
            "metadata": {
                "gridforge": {
                    "goal": {
                        "id": "g",
                        "objective": "x",
                        "status": "deleted"
                    }
                }
            }
        });
        assert!(parse_goal_metadata(&session).is_none());
    }

    #[test]
    fn parse_goal_metadata_rejects_missing_id() {
        let session = json!({
            "metadata": {
                "gridforge": {
                    "goal": {
                        "objective": "x",
                        "status": "active"
                    }
                }
            }
        });
        assert!(parse_goal_metadata(&session).is_none());
    }

    #[test]
    fn parse_goal_metadata_rejects_empty_without_file_flag() {
        let session = json!({
            "metadata": {
                "gridforge": {
                    "goal": {
                        "id": "g",
                        "objective": "",
                        "objectiveFile": false,
                        "status": "active"
                    }
                }
            }
        });
        assert!(parse_goal_metadata(&session).is_none());
    }

    #[test]
    fn parse_goal_metadata_normalizes_numbers() {
        let session = json!({
            "metadata": {
                "gridforge": {
                    "goal": {
                        "id": "g",
                        "objective": "x",
                        "status": "active",
                        "tokenBudget": 5000.7,
                        "tokensUsed": 1234.9,
                        "turnsUsed": 3.2,
                        "blockedStreak": 0.0
                    }
                }
            }
        });
        let g = parse_goal_metadata(&session).unwrap();
        assert_eq!(g.token_budget, Some(5000));
        assert_eq!(g.tokens_used, 1234);
        assert_eq!(g.turns_used, 3);
        assert_eq!(g.blocked_streak, 0);
    }

    #[test]
    fn parse_goal_metadata_rejects_negative_token_budget() {
        let session = json!({
            "metadata": {
                "gridforge": {
                    "goal": {
                        "id": "g",
                        "objective": "x",
                        "status": "active",
                        "tokenBudget": -1
                    }
                }
            }
        });
        let g = parse_goal_metadata(&session).unwrap();
        assert_eq!(g.token_budget, None); // 不接受 ≤0
    }

    #[test]
    fn parse_goal_metadata_truncates_long_objective() {
        use crate::session_goal::objectives::GOAL_OBJECTIVE_CHAR_LIMIT;
        let long = "x".repeat(GOAL_OBJECTIVE_CHAR_LIMIT + 100);
        let session = json!({
            "metadata": {
                "gridforge": {
                    "goal": {
                        "id": "g",
                        "objective": long,
                        "status": "active"
                    }
                }
            }
        });
        let g = parse_goal_metadata(&session).unwrap();
        assert_eq!(g.objective.len(), GOAL_OBJECTIVE_CHAR_LIMIT);
    }

    #[test]
    fn merge_preserves_other_gridforge_fields() {
        let session = json!({
            "id": "sess_1",
            "metadata": {
                "gridforge": {
                    "assist": {"recap": "x"},
                    "goal": {"id": "old"}
                },
                "other_namespace": "value"
            }
        });
        let new_goal = json!({"id": "new", "objective": "y", "status": "active"});
        let merged = merge_goal_into_session_metadata(&session, &new_goal);
        assert_eq!(merged["metadata"]["gridforge"]["goal"], new_goal);
        assert_eq!(merged["metadata"]["gridforge"]["assist"]["recap"], "x");
        assert_eq!(merged["metadata"]["other_namespace"], "value");
        assert_eq!(merged["id"], "sess_1");
    }

    #[test]
    fn merge_creates_namespaces_if_missing() {
        let session = json!({"id": "sess_2"});
        let new_goal = json!({"id": "g", "status": "active"});
        let merged = merge_goal_into_session_metadata(&session, &new_goal);
        assert_eq!(merged["metadata"]["gridforge"]["goal"], new_goal);
    }

    #[test]
    fn goal_to_value_round_trip() {
        let g = GoalMetadata {
            id: "g1".into(),
            objective: "build".into(),
            status: "active".into(),
            token_budget: Some(1000),
            tokens_used: 100,
            turns_used: 1,
            created_at: 12345,
            updated_at: 67890,
            ..Default::default()
        };
        let v = goal_to_value(&g);
        assert_eq!(v["id"], "g1");
        assert_eq!(v["tokenBudget"], 1000);
        assert_eq!(v["tokensUsed"], 100);
        assert_eq!(v["turnsUsed"], 1);
        assert_eq!(v["createdAt"], 12345);
        // skipped 字段: tokenBudget=None 不应序列化
        let g_no_budget = GoalMetadata {
            id: "g2".into(),
            objective: "x".into(),
            status: "active".into(),
            ..Default::default()
        };
        let v_no_budget = goal_to_value(&g_no_budget);
        assert!(v_no_budget.get("tokenBudget").is_none());
    }

    #[test]
    fn is_active_helper() {
        let mut g = GoalMetadata { status: "active".into(), ..Default::default() };
        assert!(is_active(&g));
        g.status = "complete".into();
        assert!(!is_active(&g));
    }
}