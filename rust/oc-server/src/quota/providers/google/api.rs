//! Google provider - api。

use reqwest::Client;
use serde_json::Value;

const GOOGLE_PRIMARY_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";

const GOOGLE_ENDPOINTS: &[&str] = &[
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
    "https://autopush-cloudcode-pa.sandbox.googleapis.com",
    GOOGLE_PRIMARY_ENDPOINT,
];

const GOOGLE_HEADERS: &[(&str, &str)] = &[
    ("User-Agent", "antigravity/1.11.5 windows/amd64"),
    ("X-Goog-Api-Client", "google-cloud-sdk vscode_cloudshelleditor/0.1"),
    (
        "Client-Metadata",
        r#"{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}"#,
    ),
];

pub async fn refresh_google_access_token(
    refresh_token: &str,
    client_id: &str,
    client_secret: &str,
) -> Option<String> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    body.get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub async fn fetch_google_quota_buckets(
    client: &Client,
    access_token: &str,
    project_id: &str,
) -> Option<Value> {
    let body = if project_id.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::json!({"project": project_id})
    };
    let resp = client
        .post(format!(
            "{}/v1internal:retrieveUserQuota",
            GOOGLE_PRIMARY_ENDPOINT
        ))
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .json(&body)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok()
}

pub async fn fetch_google_models(
    client: &Client,
    access_token: &str,
    project_id: &str,
) -> Option<Value> {
    let body = if project_id.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::json!({"project": project_id})
    };

    for endpoint in GOOGLE_ENDPOINTS {
        let resp = client
            .post(format!("{}/v1internal:fetchAvailableModels", endpoint))
            .bearer_auth(access_token)
            .header("Content-Type", "application/json")
            .header("User-Agent", GOOGLE_HEADERS[0].1)
            .header("X-Goog-Api-Client", GOOGLE_HEADERS[1].1)
            .header("Client-Metadata", GOOGLE_HEADERS[2].1)
            .json(&body)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(_) => continue,
        };
        if resp.status().is_success() {
            return resp.json::<Value>().await.ok();
        }
    }
    None
}
