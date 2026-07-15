//! Google provider - auth。

use serde_json::Value;

use crate::opencode::auth::read_auth_file;
use crate::quota::providers::google::transforms::{parse_google_refresh_token, ParsedRefreshToken};
use crate::quota::utils::auth::{
    antigravity_accounts_paths, get_auth_entry, normalize_auth_entry, read_json_file,
};
use crate::quota::utils::transformers::{as_non_empty_string, as_object, to_timestamp};

const ANTIGRAVITY_GOOGLE_CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const ANTIGRAVITY_GOOGLE_CLIENT_SECRET: &str = "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";
const GEMINI_GOOGLE_CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";
const GEMINI_GOOGLE_CLIENT_SECRET: &str = "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";
pub const DEFAULT_PROJECT_ID: &str = "rising-fact-p41fc";

pub struct OAuthClient {
    pub client_id: String,
    pub client_secret: String,
}

pub struct GoogleAuthSource {
    pub source_id: &'static str,
    pub source_label: &'static str,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub project_id: Option<String>,
    pub expires: Option<i64>,
    pub email: Option<String>,
}

pub fn resolve_google_oauth_client(source_id: &str) -> OAuthClient {
    if source_id == "gemini" {
        OAuthClient {
            client_id: GEMINI_GOOGLE_CLIENT_ID.to_string(),
            client_secret: GEMINI_GOOGLE_CLIENT_SECRET.to_string(),
        }
    } else {
        OAuthClient {
            client_id: ANTIGRAVITY_GOOGLE_CLIENT_ID.to_string(),
            client_secret: ANTIGRAVITY_GOOGLE_CLIENT_SECRET.to_string(),
        }
    }
}

fn resolve_gemini_cli_auth(auth: &Value) -> Option<GoogleAuthSource> {
    let entry = normalize_auth_entry(get_auth_entry(auth, &["google", "google.oauth"]))?;
    let entry_obj = as_object(&entry)?;
    // oauth sub-field 优先; 若不存在则用 entry 自身 (同 Node `asObject(entryObject.oauth) ?? entryObject`)
    let oauth_entry = entry_obj.get("oauth").unwrap_or(&entry);
    let oauth_obj = as_object(oauth_entry)?;
    let access = oauth_obj
        .get("access")
        .and_then(|v| as_non_empty_string(v))
        .or_else(|| oauth_obj.get("token").and_then(|v| as_non_empty_string(v)));
    let refresh_parts: ParsedRefreshToken = oauth_obj
        .get("refresh")
        .and_then(parse_google_refresh_token)
        .unwrap_or(ParsedRefreshToken {
            refresh_token: None,
            project_id: None,
            managed_project_id: None,
        });

    if access.is_none() && refresh_parts.refresh_token.is_none() {
        return None;
    }

    let project_id = refresh_parts
        .project_id
        .clone()
        .or(refresh_parts.managed_project_id.clone());

    Some(GoogleAuthSource {
        source_id: "gemini",
        source_label: "Gemini",
        access_token: access,
        refresh_token: refresh_parts.refresh_token,
        project_id,
        expires: oauth_obj.get("expires").and_then(|v| to_timestamp(v)),
        email: None,
    })
}

fn resolve_antigravity_auth() -> Option<GoogleAuthSource> {
    for path in antigravity_accounts_paths() {
        let data = read_json_file(&path);
        let accounts = data
            .as_ref()
            .and_then(|d| d.get("accounts"))
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();
        if !accounts.is_empty() {
            let active_index = data
                .as_ref()
                .and_then(|d| d.get("activeIndex"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let account = accounts.get(active_index).cloned().unwrap_or_else(|| accounts[0].clone());
            if let Some(rt) = account.get("refreshToken").and_then(|v| v.as_str()) {
                let rt_str = rt.to_string();
                let parsed = parse_google_refresh_token(&Value::String(rt_str.clone()));
                let parsed = parsed.unwrap_or(ParsedRefreshToken {
                    refresh_token: Some(rt_str.clone()),
                    project_id: None,
                    managed_project_id: None,
                });
                let project_id = as_non_empty_string(account.get("projectId").unwrap_or(&Value::Null))
                    .or_else(|| as_non_empty_string(account.get("managedProjectId").unwrap_or(&Value::Null)))
                    .or_else(|| parsed.project_id.clone())
                    .or_else(|| parsed.managed_project_id.clone());
                return Some(GoogleAuthSource {
                    source_id: "antigravity",
                    source_label: "Antigravity",
                    access_token: None,
                    refresh_token: parsed.refresh_token.or(Some(rt_str)),
                    project_id,
                    expires: None,
                    email: account.get("email").and_then(|v| v.as_str()).map(|s| s.to_string()),
                });
            }
        }
    }
    None
}

pub fn resolve_google_auth_sources() -> Vec<GoogleAuthSource> {
    let auth = read_auth_file().unwrap_or_default();
    let mut sources: Vec<GoogleAuthSource> = Vec::new();
    if let Some(g) = resolve_gemini_cli_auth(&auth) {
        sources.push(g);
    }
    if let Some(a) = resolve_antigravity_auth() {
        sources.push(a);
    }
    sources
}
