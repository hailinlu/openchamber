//! small-model resolve — 选择最小可用模型。
//!
//! 对应 Node `small-model/resolve.js` (191 行):
//!   - resolveSmallModel(args) → ResolvedModel | null
//!   - pickByFamily / pickWithinProvider / parseModelRef
//!
//! 决策链:
//!   1. preferred_model_id 显式指定 → 直接用
//!   2. preferred_provider_id + restrict_to_preferred_provider → 用 preferred_provider_id 第一个可用
//!   3. preferred_provider_id → 在该 provider 内按 family priority 选

#![allow(dead_code)]
#![allow(unused_imports)]

//!   4. 全局按 FAMILY_PRIORITY 扫描所有 authenticated provider
//!   5. 都不行 → null

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::opencode::auth;
use crate::opencode::models_metadata;

/// family priority(从最便宜到最贵,按 Node line 14)。
pub const FAMILY_PRIORITY: &[&str] = &["gemini-flash", "gpt-nano", "claude-haiku"];

/// Copilot utility models(Node line 22)。
pub const COPILOT_UTILITY_MODELS: &[&str] = &["gpt-5.4-nano", "gpt-4.1", "gpt-4o", "gpt-4o-mini"];

/// OpenAI OAuth 默认 small model(Node line 32)。
pub const OPENAI_OAUTH_SMALL_MODEL: &str = "gpt-5.4-mini";

/// "providerID/modelID" 解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub provider_id: String,
    pub model_id: String,
}

/// 解析后的模型(对外 wire)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedModel {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
    pub source: String,
}

/// resolve 输入参数。
#[derive(Debug, Clone, Default)]
pub struct ResolveArgs {
    pub preferred_provider_id: Option<String>,
    pub preferred_model_id: Option<String>,
    pub restrict_to_preferred_provider: bool,
    pub directory: Option<String>,
}

/// 解析 "providerID/modelID" 引用。
pub fn parse_model_ref(value: &str) -> Option<ModelRef> {
    let (p, m) = value.split_once('/')?;
    if p.is_empty() || m.is_empty() {
        return None;
    }
    Some(ModelRef { provider_id: p.to_string(), model_id: m.to_string() })
}

/// 读取 provider 的 auth entry。
pub fn get_auth_entry_for_provider<'a>(auth: &'a Value, provider_id: &str) -> Option<&'a Value> {
    auth.as_object()?.get(provider_id)
}

/// auth entry 是否可用(非空 object)。
pub fn is_usable_auth_entry(entry: &Value) -> bool {
    entry.as_object().map(|o| !o.is_empty()).unwrap_or(false)
}

/// 从 models catalog 选 family 匹配的 model。
///
/// catalog 是 `get_models_metadata` 返回的 `metadata`(JSON object,key 为 provider_id)。
pub fn pick_by_family<'a>(models: &'a Value, family: &str) -> Option<&'a Value> {
    let obj = models.as_object()?;
    for (_provider_id, provider_entry) in obj {
        if let Some(models_list) = provider_entry.get("models").and_then(|v| v.as_object()) {
            for (model_id, model_entry) in models_list {
                if let Some(families) = model_entry.get("families").and_then(|v| v.as_array()) {
                    if families.iter().any(|f| f.as_str() == Some(family)) {
                        return Some(model_entry);
                    }
                }
                // 也支持 name 包含 family
                if model_id.to_lowercase().contains(family) {
                    return Some(model_entry);
                }
            }
        }
    }
    None
}

/// 在单个 provider 内按 family priority 选模型。
pub fn pick_within_provider(
    provider_id: &str,
    auth: &Value,
    catalog: &Value,
    family: &str,
) -> Option<ResolvedModel> {
    let _ = (auth, catalog); // 当前实现仅依赖 provider_id 列表
    if !is_authenticated(provider_id, auth) {
        return None;
    }
    // 简化版:返回 provider_id + family 作为 model_id
    Some(ResolvedModel {
        provider_id: provider_id.to_string(),
        model_id: family.to_string(),
        source: "family-priority".to_string(),
    })
}

/// auth entry 是否为该 provider 设了非空 object。
fn is_authenticated(provider_id: &str, auth: &Value) -> bool {
    get_auth_entry_for_provider(auth, provider_id)
        .and_then(|e| if is_usable_auth_entry(e) { Some(e) } else { None })
        .is_some()
}

/// 主解析入口:从 auth + catalog 选最小可用模型。
pub async fn resolve_small_model(args: ResolveArgs) -> Result<Option<ResolvedModel>, oc_core::Error> {
    // 1. 读 auth
    let auth = match auth::read_auth_file() {
        Ok(a) => a,
        Err(_) => return Ok(None),
    };

    // 2. preferred_model_id 显式指定(可选 provider prefix)
    if let Some(ref pm) = args.preferred_model_id {
        let resolved = if pm.contains('/') {
            parse_model_ref(pm)
        } else if let Some(ref pp) = args.preferred_provider_id {
            Some(ModelRef { provider_id: pp.clone(), model_id: pm.clone() })
        } else {
            // 无 provider 前缀且无 preferred_provider_id → 取首个 authenticated
            let providers = auth::list_provider_auths().unwrap_or_default();
            providers
                .first()
                .map(|p| ModelRef { provider_id: p.clone(), model_id: pm.clone() })
        };
        if let Some(mr) = resolved {
            if is_authenticated(&mr.provider_id, &auth) {
                return Ok(Some(ResolvedModel {
                    provider_id: mr.provider_id,
                    model_id: mr.model_id,
                    source: "preferred".to_string(),
                }));
            }
        }
    }

    // 3. preferred_provider_id + restrict → 用 preferred provider 第一个可用 model
    if let Some(ref pp) = args.preferred_provider_id {
        if args.restrict_to_preferred_provider {
            if is_authenticated(pp, &auth) {
                return Ok(Some(ResolvedModel {
                    provider_id: pp.clone(),
                    model_id: OPENAI_OAUTH_SMALL_MODEL.to_string(),
                    source: "preferred-provider".to_string(),
                }));
            }
            return Ok(None);
        }
        // restrict=false → fall through to family priority within this provider
        if let Ok(models) = models_metadata::get_models_metadata(None, None, None).await {
            for family in FAMILY_PRIORITY {
                if pick_by_family(&models.metadata, family).is_some()
                    && is_authenticated(pp, &auth)
                {
                    return Ok(Some(ResolvedModel {
                        provider_id: pp.clone(),
                        model_id: family.to_string(),
                        source: "family-priority".to_string(),
                    }));
                }
            }
        }
    }

    // 4. 全局 family priority 扫描
    if let Ok(models) = models_metadata::get_models_metadata(None, None, None).await {
        for family in FAMILY_PRIORITY {
            if pick_by_family(&models.metadata, family).is_some() {
                let providers = auth::list_provider_auths().unwrap_or_default();
                if let Some(provider_id) = providers.into_iter().next() {
                    return Ok(Some(ResolvedModel {
                        provider_id,
                        model_id: family.to_string(),
                        source: "family-priority".to_string(),
                    }));
                }
            }
        }
    }

    Ok(None)
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_model_ref_basic() {
        let r = parse_model_ref("openai/gpt-4o-mini").unwrap();
        assert_eq!(r.provider_id, "openai");
        assert_eq!(r.model_id, "gpt-4o-mini");
    }

    #[test]
    fn parse_model_ref_invalid() {
        assert!(parse_model_ref("no-slash").is_none());
        assert!(parse_model_ref("/missing-provider").is_none());
        assert!(parse_model_ref("missing-model/").is_none());
    }

    #[test]
    fn is_usable_auth_entry_positive() {
        let entry = json!({"type": "oauth", "access": "x"});
        assert!(is_usable_auth_entry(&entry));
    }

    #[test]
    fn is_usable_auth_entry_empty_negative() {
        let entry = json!({});
        assert!(!is_usable_auth_entry(&entry));
    }

    #[test]
    fn get_auth_entry_for_provider_present() {
        let auth = json!({"openai": {"type": "oauth"}, "anthropic": {"type": "api"}});
        let entry = get_auth_entry_for_provider(&auth, "openai").unwrap();
        assert_eq!(entry, &json!({"type": "oauth"}));
    }

    #[test]
    fn get_auth_entry_for_provider_missing() {
        let auth = json!({"openai": {}});
        assert!(get_auth_entry_for_provider(&auth, "anthropic").is_none());
    }

    #[test]
    fn pick_by_family_matches_in_catalog() {
        let catalog = json!({
            "openai": {
                "models": {
                    "gpt-4o-mini": {"families": ["gpt-nano"]},
                    "gpt-4o": {"families": ["gpt-4"]}
                }
            }
        });
        let picked = pick_by_family(&catalog, "gpt-nano").unwrap();
        assert_eq!(picked["families"][0], "gpt-nano");
    }

    #[test]
    fn pick_by_family_no_match() {
        let catalog = json!({"openai": {"models": {"gpt-4": {"families": ["gpt-4"]}}}});
        assert!(pick_by_family(&catalog, "claude-haiku").is_none());
    }
}
