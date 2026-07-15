//! HTTP handlers for skills-catalog module — 对应 Node `skill-routes.js`。
//!
//! 12 个路由:
//! - `GET    /api/config/skills`                      → list
//! - `GET    /api/config/skills/catalog`              → curated sources
//! - `GET    /api/config/skills/catalog/source`       → browse one source
//! - `POST   /api/config/skills/scan`                 → ad-hoc scan
//! - `POST   /api/config/skills/install`              → install
//! - `GET    /api/config/skills/:name`                → get one
//! - `POST   /api/config/skills/:name`                → create
//! - `PATCH  /api/config/skills/:name`                → update
//! - `DELETE /api/config/skills/:name`                → delete
//! - `GET    /api/config/skills/:name/files/*path`    → read file
//! - `PUT    /api/config/skills/:name/files/*path`    → write file
//! - `DELETE /api/config/skills/:name/files/*path`    → delete file

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

use super::cache;
use super::git::DefaultGitRunner;
use super::install::{self, InstallRequest};
use super::scan::{is_valid_skill_name, scan_skills_repository};
use super::skills::{
    self, build_skill_md_content, delete_skill_supporting_file, discover_skills,
    get_skill_sources, read_skill_supporting_file, write_skill_supporting_file,
};

// ---------------------------------------------------------------------------
// 数据模型
// ---------------------------------------------------------------------------

/// 预定义 skill 来源。
static CURATED_SOURCES: &[CuratedSource] = &[
    CuratedSource {
        id: "anthropic",
        label: "Anthropic",
        description: "Skills published by Anthropic",
        source: "anthropics/skills",
        default_subpath: Some("skills"),
        source_type: "github",
    },
    CuratedSource {
        id: "clawdhub",
        label: "ClawdHub",
        description: "Community skills from ClawdHub",
        source: "clawdhub:registry",
        default_subpath: None,
        source_type: "clawdhub",
    },
];

struct CuratedSource {
    id: &'static str,
    label: &'static str,
    description: &'static str,
    source: &'static str,
    default_subpath: Option<&'static str>,
    source_type: &'static str,
}

#[derive(Deserialize)]
pub struct ListSkillsQuery {
    pub directory: Option<String>,
}

#[derive(Deserialize)]
pub struct CatalogSourceQuery {
    source_id: Option<String>,
    refresh: Option<bool>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
pub struct ScanBody {
    source: String,
    subpath: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateSkillBody {
    description: Option<String>,
    instructions: Option<String>,
    #[serde(default)]
    supporting_files: Option<Vec<SupportingFileBody>>,
}

#[derive(Deserialize)]
pub struct UpdateSkillBody {
    name: Option<String>,
    description: Option<String>,
    instructions: Option<String>,
    #[serde(default)]
    supporting_files: Option<Vec<SupportingFileOp>>,
    target_path: Option<String>,
}

#[derive(Deserialize)]
pub struct SupportingFileBody {
    path: String,
    content: String,
}

#[derive(Deserialize)]
pub struct SupportingFileOp {
    path: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    delete: Option<bool>,
}

#[derive(Deserialize)]
pub struct FileBody {
    content: Option<String>,
}

// ---------------------------------------------------------------------------
// Route 1: GET /api/config/skills — List all skills
// ---------------------------------------------------------------------------

pub async fn list_skills(
    State(_state): State<Arc<AppState>>,
    Query(query): Query<ListSkillsQuery>,
) -> ApiResult<Json<Value>> {
    let skills = discover_skills(query.directory.as_deref());
    Ok(Json(json!({ "skills": skills })))
}

// ---------------------------------------------------------------------------
// Route 2: GET /api/config/skills/catalog — Curated sources
// ---------------------------------------------------------------------------

pub async fn list_catalog_sources(
    State(_state): State<Arc<AppState>>,
) -> ApiResult<Json<Value>> {
    let sources: Vec<Value> = CURATED_SOURCES
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "label": s.label,
                "description": s.description,
                "source": s.source,
                "defaultSubpath": s.default_subpath,
                "sourceType": s.source_type,
            })
        })
        .collect();

    Ok(Json(json!({
        "ok": true,
        "sources": sources,
        "itemsBySource": {},
        "pageInfoBySource": {},
    })))
}

// ---------------------------------------------------------------------------
// Route 3: GET /api/config/skills/catalog/source — Browse one catalog source
// ---------------------------------------------------------------------------

pub async fn browse_catalog_source(
    State(_state): State<Arc<AppState>>,
    Query(query): Query<CatalogSourceQuery>,
) -> ApiResult<Json<Value>> {
    let source_id = query
        .source_id
        .as_deref()
        .ok_or_else(|| ApiError(oc_core::Error::BadRequest("sourceId is required".into())))?;

    let curated = CURATED_SOURCES.iter().find(|s| s.id == source_id);

    match curated {
        Some(source) => {
            let runner = DefaultGitRunner;
            let refresh = query.refresh.unwrap_or(false);

            if refresh {
                let cache_key = cache::get_cache_key(
                    source.source,
                    source.default_subpath,
                    None,
                );
                cache::clear_cache();
            }

            let scan_result = scan_skills_repository(
                source.source,
                None,
                source.default_subpath,
                &runner,
            );

            if scan_result.ok {
                let items = scan_result.items.unwrap_or_default();
                // 添加 sourceId 到每个 item
                let items_with_source: Vec<Value> = items
                    .into_iter()
                    .map(|item| {
                        json!({
                            "sourceId": source_id,
                            "skillName": item.skill_name,
                            "description": item.description,
                            "skillDir": item.skill_dir,
                            "sourcePath": format!("{}/{}", source.source, item.skill_dir),
                            "installed": null,
                        })
                    })
                    .collect();

                Ok(Json(json!({
                    "ok": true,
                    "items": items_with_source,
                    "nextCursor": null,
                })))
            } else {
                Ok(Json(json!({
                    "ok": false,
                    "items": [],
                    "nextCursor": null,
                    "error": scan_result.error.map(|e| json!({
                        "kind": e.kind,
                        "message": e.message,
                    })),
                })))
            }
        }
        None => Ok(Json(json!({
            "ok": false,
            "items": [],
            "nextCursor": null,
            "error": {
                "kind": "unknown",
                "message": format!("Unknown source: {}", source_id),
            },
        }))),
    }
}

// ---------------------------------------------------------------------------
// Route 4: POST /api/config/skills/scan — Ad-hoc scan
// ---------------------------------------------------------------------------

pub async fn scan_repository(
    State(_state): State<Arc<AppState>>,
    Json(body): Json<ScanBody>,
) -> ApiResult<Json<Value>> {
    let runner = DefaultGitRunner;
    let result = scan_skills_repository(&body.source, body.subpath.as_deref(), None, &runner);

    if result.ok {
        Ok(Json(json!({ "ok": true, "items": result.items })))
    } else {
        Ok(Json(json!({
            "ok": false,
            "error": result.error.map(|e| json!({
                "kind": e.kind,
                "message": e.message,
            })),
        })))
    }
}

// ---------------------------------------------------------------------------
// Route 5: POST /api/config/skills/install — Install skills
// ---------------------------------------------------------------------------

pub async fn install_skills(
    State(_state): State<Arc<AppState>>,
    Json(body): Json<InstallRequest>,
) -> ApiResult<Json<Value>> {
    let source = body
        .source
        .as_deref()
        .ok_or_else(|| ApiError(oc_core::Error::BadRequest("source is required".into())))?;
    let scope = body
        .scope
        .as_deref()
        .unwrap_or("user");
    let target_source = body
        .target_source
        .as_deref()
        .unwrap_or("opencode");
    let selections = body.selections.unwrap_or_default();
    let conflict_policy = body.conflict_policy.as_deref();
    let conflict_decisions = body.conflict_decisions.unwrap_or_default();
    let runner = DefaultGitRunner;

    let result = install::install_skills_from_repository(
        source,
        body.subpath.as_deref(),
        scope,
        target_source,
        &selections,
        conflict_policy,
        &conflict_decisions,
        None,
        &runner,
    );

    if result.ok {
        Ok(Json(json!({
            "ok": true,
            "installed": result.installed,
            "skipped": result.skipped,
            "requiresReload": true,
            "message": "Skills installed successfully. Reloading interface...",
            "reloadDelayMs": 2000,
        })))
    } else if let Some(ref err) = result.error {
        if err.kind == "conflicts" {
            return Ok(Json(json!({
                "ok": false,
                "error": {
                    "kind": "conflicts",
                    "conflicts": err.conflicts,
                },
            })));
        }
        if err.kind == "authRequired" {
            return Ok(Json(json!({
                "ok": false,
                "error": {
                    "kind": "authRequired",
                    "identities": [],
                    "sshOnly": err.ssh_only,
                },
            })));
        }
        Ok(Json(json!({
            "ok": false,
            "error": {
                "kind": err.kind,
                "message": err.message,
            },
        })))
    } else {
        Err(ApiError(oc_core::Error::Internal(
            "Install failed".into(),
        )))
    }
}

// ---------------------------------------------------------------------------
// Route 6: GET /api/config/skills/:name — Get single skill
// ---------------------------------------------------------------------------

pub async fn get_skill(
    State(_state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let sources = get_skill_sources(&name, None);
    let exists = sources.md.exists;

    Ok(Json(json!({
        "name": name,
        "sources": sources,
        "scope": sources.md.scope,
        "source": sources.md.source,
        "exists": exists,
    })))
}

// ---------------------------------------------------------------------------
// Route 7: POST /api/config/skills/:name — Create skill
// ---------------------------------------------------------------------------

pub async fn create_skill(
    State(_state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<CreateSkillBody>,
) -> ApiResult<Json<Value>> {
    // Validate name
    if !is_valid_skill_name(&name) {
        return Err(ApiError(oc_core::Error::BadRequest(format!(
            "Invalid skill name: {}",
            name
        ))));
    }

    // Check if already exists
    let sources = get_skill_sources(&name, None);
    if sources.md.exists {
        return Err(ApiError(oc_core::Error::BadRequest(format!(
            "Skill '{}' already exists",
            name
        ))));
    }

    // Determine target directory (user opencode by default)
    let target_dir = install::get_target_skill_dir("user", "opencode", None).join(&name);
    std::fs::create_dir_all(&target_dir).map_err(|e| {
        ApiError(oc_core::Error::Internal(format!("Failed to create dir: {}", e)))
    })?;

    // Write SKILL.md
    let instructions = body.instructions.as_deref().unwrap_or("");
    let content = build_skill_md_content(&name, body.description.as_deref(), instructions);
    std::fs::write(target_dir.join("SKILL.md"), &content).map_err(|e| {
        ApiError(oc_core::Error::Internal(format!("Failed to write SKILL.md: {}", e)))
    })?;

    // Write supporting files
    if let Some(supporting_files) = &body.supporting_files {
        for file in supporting_files {
            let _ = write_skill_supporting_file(
                target_dir.to_str().unwrap_or(""),
                &file.path,
                &file.content,
            );
        }
    }

    Ok(Json(json!({
        "success": true,
        "requiresReload": true,
        "message": format!("Skill '{}' created successfully. Reloading interface...", name),
        "reloadDelayMs": 2000,
    })))
}

// ---------------------------------------------------------------------------
// Route 8: PATCH /api/config/skills/:name — Update skill
// ---------------------------------------------------------------------------

pub async fn update_skill(
    State(_state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<UpdateSkillBody>,
) -> ApiResult<Json<Value>> {
    let sources = get_skill_sources(&name, None);
    let skill_dir = sources
        .md
        .dir
        .ok_or_else(|| ApiError(oc_core::Error::NotFound(format!("Skill '{}' not found", name))))?;

    // Update SKILL.md content if instructions changed
    if body.instructions.is_some() || body.description.is_some() || body.name.is_some() {
        let skill_md_path = std::path::Path::new(&skill_dir).join("SKILL.md");
        let existing = std::fs::read_to_string(&skill_md_path)
            .unwrap_or_else(|_| build_skill_md_content(&name, None, ""));
        let new_name = body.name.as_deref().unwrap_or(&name);
        let new_desc = body.description.as_deref().or(sources.md.description.as_deref());
        let new_instr = body.instructions.as_deref().unwrap_or("");
        let content = build_skill_md_content(new_name, new_desc, new_instr);
        std::fs::write(&skill_md_path, &content).map_err(|e| {
            ApiError(oc_core::Error::Internal(format!("Failed to update SKILL.md: {}", e)))
        })?;
    }

    // Handle supporting file operations
    if let Some(supporting_files) = &body.supporting_files {
        for op in supporting_files {
            if op.delete.unwrap_or(false) {
                let _ = delete_skill_supporting_file(&skill_dir, &op.path);
            } else if let Some(ref content) = op.content {
                let _ = write_skill_supporting_file(&skill_dir, &op.path, content);
            }
        }
    }

    Ok(Json(json!({
        "success": true,
        "requiresReload": true,
        "message": format!("Skill '{}' updated successfully. Reloading interface...", name),
        "reloadDelayMs": 2000,
    })))
}

// ---------------------------------------------------------------------------
// Route 9: DELETE /api/config/skills/:name — Delete skill
// ---------------------------------------------------------------------------

pub async fn delete_skill(
    State(_state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let sources = get_skill_sources(&name, None);
    let skill_dir = sources
        .md
        .dir
        .ok_or_else(|| ApiError(oc_core::Error::NotFound(format!("Skill '{}' not found", name))))?;

    std::fs::remove_dir_all(&skill_dir).map_err(|e| {
        ApiError(oc_core::Error::Internal(format!("Failed to delete skill: {}", e)))
    })?;

    Ok(Json(json!({
        "success": true,
        "requiresReload": true,
        "message": format!("Skill '{}' deleted successfully. Reloading interface...", name),
        "reloadDelayMs": 2000,
    })))
}

// ---------------------------------------------------------------------------
// Routes 10-12: Supporting file CRUD
// ---------------------------------------------------------------------------

/// GET /api/config/skills/:name/files/*path
pub async fn read_skill_file(
    State(_state): State<Arc<AppState>>,
    Path((name, file_path)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let sources = get_skill_sources(&name, None);
    let skill_dir = sources
        .md
        .dir
        .ok_or_else(|| ApiError(oc_core::Error::NotFound(format!("Skill '{}' not found", name))))?;

    let content = read_skill_supporting_file(&skill_dir, &file_path)?;

    Ok(Json(json!({
        "path": file_path,
        "content": content,
    })))
}

/// PUT /api/config/skills/:name/files/*path
pub async fn write_skill_file(
    State(_state): State<Arc<AppState>>,
    Path((name, file_path)): Path<(String, String)>,
    Json(body): Json<FileBody>,
) -> ApiResult<Json<Value>> {
    let content = body
        .content
        .ok_or_else(|| ApiError(oc_core::Error::BadRequest("content is required".into())))?;

    let sources = get_skill_sources(&name, None);
    let skill_dir = sources
        .md
        .dir
        .ok_or_else(|| ApiError(oc_core::Error::NotFound(format!("Skill '{}' not found", name))))?;

    write_skill_supporting_file(&skill_dir, &file_path, &content)?;

    Ok(Json(json!({
        "success": true,
        "message": format!("File '{}' saved successfully", file_path),
    })))
}

/// DELETE /api/config/skills/:name/files/*path
pub async fn delete_skill_file(
    State(_state): State<Arc<AppState>>,
    Path((name, file_path)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let sources = get_skill_sources(&name, None);
    let skill_dir = sources
        .md
        .dir
        .ok_or_else(|| ApiError(oc_core::Error::NotFound(format!("Skill '{}' not found", name))))?;

    delete_skill_supporting_file(&skill_dir, &file_path)?;

    Ok(Json(json!({
        "success": true,
        "message": format!("File '{}' deleted successfully", file_path),
    })))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::state::AppState;
    use axum::body::Body;
    use axum::http::Request;
    use clap::Parser;
    use std::collections::HashMap;
    use tower::ServiceExt;

    fn test_state() -> Arc<AppState> {
        let config = Config::try_parse_from(["oc-server"]).unwrap();
        Arc::new(AppState::new(
            config,
            "http://127.0.0.1:4096".to_string(),
            "Basic test".to_string(),
        ))
    }

    #[tokio::test]
    async fn test_list_skills_empty() {
        let state = test_state();
        let app = axum::Router::new()
            .route("/api/config/skills", axum::routing::get(list_skills))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/config/skills")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = axum::body::to_bytes(resp.into_body(), 100_000)
            .await
            .map(|b| serde_json::from_slice(&b).unwrap())
            .unwrap();
        assert!(body.get("skills").is_some());
    }

    #[tokio::test]
    async fn test_catalog_sources() {
        let state = test_state();
        let app = axum::Router::new()
            .route("/api/config/skills/catalog", axum::routing::get(list_catalog_sources))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/config/skills/catalog")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = axum::body::to_bytes(resp.into_body(), 100_000)
            .await
            .map(|b| serde_json::from_slice(&b).unwrap())
            .unwrap();
        assert_eq!(body["ok"], true);
        assert!(body["sources"].as_array().map_or(false, |a| a.len() >= 2));
    }

    #[tokio::test]
    async fn test_get_nonexistent_skill() {
        let state = test_state();
        let app = axum::Router::new()
            .route("/api/config/skills/{name}", axum::routing::get(get_skill))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/config/skills/nonexistent-test-skill")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = axum::body::to_bytes(resp.into_body(), 100_000)
            .await
            .map(|b| serde_json::from_slice(&b).unwrap())
            .unwrap();
        assert_eq!(body["exists"], false);
    }

    #[tokio::test]
    async fn test_scan_with_invalid_source() {
        let state = test_state();
        let app = axum::Router::new()
            .route("/api/config/skills/scan", axum::routing::post(scan_repository))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/config/skills/scan")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({"source": ""})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = axum::body::to_bytes(resp.into_body(), 100_000)
            .await
            .map(|b| serde_json::from_slice(&b).unwrap())
            .unwrap();
        assert_eq!(body["ok"], false);
    }
}
