//! Outside-workspace 授权系统。
//!
//! 移植 `packages/web/server/lib/fs/routes.js` 的
//! `mintOutsideFileGrant` / `resolveOutsideFileGrant`。
//!
//! 用于 stat / read / raw 路由的 `allowOutsideWorkspace=true` 模式:
//! 客户端通过 `outsideFileGrant` query 参数携带 grant token,
//! 服务端验证 token 有效 + scope 匹配 + canonical path 精确相等。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

/// Grant 存储条目。
struct GrantEntry {
    canonical_path: PathBuf,
    base: PathBuf,
    scopes: HashSet<String>,
    expires_at: Instant,
}

/// Grant store — 进程内单例, 通过 `Arc` 在 handler 间共享。
pub struct GrantStore {
    grants: Mutex<HashMap<String, GrantEntry>>,
    ttl: Duration,
}

/// Grant mint 结果 (对应 Node `mintOutsideFileGrant` 返回值)。
#[derive(Serialize, Debug)]
pub struct MintedGrant {
    pub path: String,
    #[serde(rename = "outsideFileGrant")]
    pub token: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: u64, // Unix epoch millis
}

/// Grant resolve 结果。
pub struct ResolvedGrant {
    #[allow(dead_code)]
    pub base: PathBuf,
    pub resolved: PathBuf,
}

impl GrantStore {
    pub fn new(ttl: Duration) -> Self {
        Self {
            grants: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// Mint 一个新的 grant。
    ///
    /// 步骤:
    ///   1. realpath(target_path) → canonical path
    ///   2. stat 验证是普通文件
    ///   3. 清理过期 grant
    ///   4. 生成 UUID token
    ///   5. 存入 Map
    pub async fn mint(
        &self,
        target_path: &str,
        scopes: Vec<String>,
    ) -> Result<MintedGrant, oc_core::Error> {
        let trimmed = target_path.trim();
        if trimmed.is_empty() {
            return Err(oc_core::Error::BadRequest("Path is required".into()));
        }

        // realpath
        let canonical = tokio::fs::canonicalize(trimmed)
            .await
            .map_err(|_| oc_core::Error::BadRequest("File not found".into()))?;

        // stat 验证是文件
        let meta = tokio::fs::metadata(&canonical)
            .await
            .map_err(oc_core::Error::Io)?;
        if !meta.is_file() {
            return Err(oc_core::Error::BadRequest(
                "Outside file grants require a file path".into(),
            ));
        }

        // 清理过期
        self.prune();

        // 生成 token
        let token = uuid::Uuid::new_v4().to_string();

        // scopes → Set (空则默认 read)
        let scope_set: HashSet<String> = if scopes.is_empty() {
            vec!["read".to_string()].into_iter().collect()
        } else {
            scopes.into_iter().collect()
        };

        let base = canonical.parent().unwrap_or_else(|| Path::new("/")).to_path_buf();
        let expires_at = Instant::now() + self.ttl;
        let expires_at_epoch = chrono::Utc::now().timestamp_millis() as u64
            + self.ttl.as_millis() as u64;

        let entry = GrantEntry {
            canonical_path: canonical.clone(),
            base,
            scopes: scope_set,
            expires_at,
        };

        {
            let mut grants = self.grants.lock().unwrap();
            grants.insert(token.clone(), entry);
        }

        Ok(MintedGrant {
            path: canonical.to_string_lossy().to_string(),
            token,
            expires_at: expires_at_epoch,
        })
    }

    /// 验证 grant。
    ///
    /// 检查: token 存在 → 未过期 → scope 匹配 → canonical path 精确相等。
    pub async fn resolve(
        &self,
        token: &str,
        target_path: &str,
        scope: &str,
    ) -> Result<ResolvedGrant, oc_core::Error> {
        if token.is_empty() {
            return Err(oc_core::Error::BadRequest("Outside file grant is required".into()));
        }

        // 清理过期
        self.prune();

        // 查找 token
        let canonical = tokio::fs::canonicalize(target_path)
            .await
            .map_err(|_| oc_core::Error::BadRequest("Invalid path".into()))?;

        let grant_data = {
            let grants = self.grants.lock().unwrap();
            grants.get(token).map(|g| (g.canonical_path.clone(), g.base.clone(), g.scopes.clone()))
        };

        let (grant_canonical, grant_base, grant_scopes) = grant_data.ok_or_else(|| {
            oc_core::Error::BadRequest("Invalid or expired outside file grant".into())
        })?;

        // scope 检查
        if !grant_scopes.contains(scope) {
            return Err(oc_core::Error::BadRequest(format!(
                "Grant does not include '{}' scope",
                scope
            )));
        }

        // canonical path 精确相等
        if canonical != grant_canonical {
            return Err(oc_core::Error::BadRequest(
                "Path does not match granted path".into(),
            ));
        }

        Ok(ResolvedGrant {
            base: grant_base,
            resolved: canonical,
        })
    }

    /// 清理过期 grant。
    fn prune(&self) {
        let now = Instant::now();
        let mut grants = self.grants.lock().unwrap();
        grants.retain(|_, entry| entry.expires_at > now);
    }

    /// 测试辅助: 当前 grant 数量。
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.grants.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn mint_and_resolve() {
        let store = GrantStore::new(Duration::from_secs(600));

        // 创建临时文件
        let dir = std::env::temp_dir();
        let file_path = dir.join(format!("oc-test-grant-{}", uuid::Uuid::new_v4()));
        let mut f = std::fs::File::create(&file_path).unwrap();
        writeln!(f, "test content").unwrap();

        // mint
        let minted = store
            .mint(file_path.to_str().unwrap(), vec!["read".into(), "stat".into()])
            .await
            .unwrap();
        assert_eq!(minted.path, file_path.canonicalize().unwrap().to_string_lossy().to_string());
        assert!(!minted.token.is_empty());

        // resolve (read scope)
        let resolved = store
            .resolve(&minted.token, file_path.to_str().unwrap(), "read")
            .await;
        assert!(resolved.is_ok());

        // resolve (stat scope — 也应该通过)
        let resolved = store
            .resolve(&minted.token, file_path.to_str().unwrap(), "stat")
            .await;
        assert!(resolved.is_ok());

        // resolve (raw scope — 应失败)
        let resolved = store
            .resolve(&minted.token, file_path.to_str().unwrap(), "raw")
            .await;
        assert!(resolved.is_err());

        // cleanup
        std::fs::remove_file(&file_path).ok();
    }

    #[tokio::test]
    async fn mint_empty_path_fails() {
        let store = GrantStore::new(Duration::from_secs(600));
        let result = store.mint("", vec![]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn mint_nonexistent_fails() {
        let store = GrantStore::new(Duration::from_secs(600));
        let result = store
            .mint("/nonexistent/path/to/file.txt", vec![])
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn mint_directory_fails() {
        let store = GrantStore::new(Duration::from_secs(600));
        let dir = std::env::temp_dir();
        let result = store.mint(dir.to_str().unwrap(), vec![]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn resolve_invalid_token_fails() {
        let store = GrantStore::new(Duration::from_secs(600));
        let dir = std::env::temp_dir();
        let result = store
            .resolve("invalid-token", dir.to_str().unwrap(), "read")
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn prune_removes_expired() {
        let store = GrantStore::new(Duration::from_millis(1));
        // 手动插入一个已过期的 grant
        {
            let mut grants = store.grants.lock().unwrap();
            grants.insert(
                "test-token".to_string(),
                GrantEntry {
                    canonical_path: PathBuf::from("/tmp/test"),
                    base: PathBuf::from("/tmp"),
                    scopes: vec!["read".to_string()].into_iter().collect(),
                    expires_at: Instant::now() - Duration::from_secs(1),
                },
            );
        }
        assert_eq!(store.len(), 1);
        store.prune();
        assert_eq!(store.len(), 0);
    }
}
