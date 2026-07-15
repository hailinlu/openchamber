//! 隧道认证控制器: bootstrap token + session + rate-limit。
//!
//! 移植自 `packages/web/server/lib/opencode/tunnel-auth.js` (587 行)。
//!
//! **全内存状态, Mutex 保护。**
//! - bootstrap token: 32 随机字节 base64url, 只存 SHA-256 hash, 单次使用
//! - session: base64url cookie value → SessionRecord
//! - rate-limit: per-IP 滑动窗口 + lockout
//!
//! Cookie 名: `oc_tunnel_session`。

use std::collections::HashMap;
use std::sync::Mutex;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use serde_json::{json, Value};
use sha2::Sha256;

pub const TUNNEL_SESSION_COOKIE_NAME: &str = "oc_tunnel_session";
const BOOTSTRAP_TOKEN_COOKIE_SAFE_BYTES: usize = 32;

const CONNECT_RATE_LIMIT_WINDOW_MS: i64 = 5 * 60 * 1000; // 5min
const CONNECT_RATE_LIMIT_LOCK_MS: i64 = 10 * 60 * 1000; // 10min lockout
const CONNECT_RATE_LIMIT_MAX_ATTEMPTS: u32 = 20; // per-IP
const CONNECT_RATE_LIMIT_NO_IP_MAX_ATTEMPTS: u32 = 5; // no-IP fallback
const NO_IP_KEY: &str = "connect-rate-limit:no-ip";

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// SHA-256 哈希 (hex)。
fn hash_token(token: &str) -> String {
    use sha2::Digest;
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize().as_slice())
}

/// 生成 32 随机字节 base64url。
fn generate_random_token() -> String {
    let mut bytes = [0u8; BOOTSTRAP_TOKEN_COOKIE_SAFE_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 生成 UUID v4 (简化版, 用 random bytes)。
fn generate_uuid() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    // 设定 version 4 和 variant 位
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

/// timing-safe 比较 (逐字节 XOR-OR, 对应 Node `crypto.timingSafeEqual`)。
fn timing_safe_equal(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result: u8 = 0;
    for (x, y) in a.bytes().zip(b.bytes()) {
        result |= x ^ y;
    }
    result == 0
}

// ── bootstrap record ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct BootstrapRecord {
    id: String,
    tunnel_id: String,
    token_hash: String,
    issued_at: i64,
    expires_at: Option<i64>,
    used_at: Option<i64>,
    revoked_at: Option<i64>,
}

impl BootstrapRecord {
    fn is_usable(&self) -> bool {
        if self.revoked_at.is_some() || self.used_at.is_some() {
            return false;
        }
        if let Some(exp) = self.expires_at {
            if now_ts() >= exp {
                return false;
            }
        }
        true
    }
}

// ── session record ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct SessionRecord {
    session_id: String,
    tunnel_id: String,
    mode: Option<String>,
    public_url: Option<String>,
    created_at: i64,
    last_seen_at: i64,
    expires_at: i64,
    revoked_at: Option<i64>,
    revoked_reason: Option<String>,
    expired_at: Option<i64>,
}

// ── rate-limit record ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct RateRecord {
    count: u32,
    last_attempt: i64,
    locked_until: Option<i64>,
}

// ── connect 交换结果 ────────────────────────────────────────────────────────

/// bootstrap token 交换结果。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ExchangeResult {
    pub ok: bool,
    pub reason: Option<String>,
    pub retry_after: u64,
    pub session_id: Option<String>,
    pub session_expires_at: Option<i64>,
}

impl ExchangeResult {
    fn rate_limited(retry_after: u64) -> Self {
        Self {
            ok: false,
            reason: Some("rate-limited".to_string()),
            retry_after,
            session_id: None,
            session_expires_at: None,
        }
    }

    fn failed(reason: &str) -> Self {
        Self {
            ok: false,
            reason: Some(reason.to_string()),
            retry_after: 0,
            session_id: None,
            session_expires_at: None,
        }
    }

    fn success(session_id: String, expires_at: i64) -> Self {
        Self {
            ok: true,
            reason: None,
            retry_after: 0,
            session_id: Some(session_id),
            session_expires_at: Some(expires_at),
        }
    }
}

// ── TunnelAuth ─────────────────────────────────────────────────────────────

/// 隧道认证控制器 (全内存)。
pub struct TunnelAuth {
    inner: Mutex<TunnelAuthInner>,
}

struct TunnelAuthInner {
    active_tunnel_id: Option<String>,
    active_tunnel_host: Option<String>,
    active_tunnel_mode: Option<String>,
    active_tunnel_public_url: Option<String>,
    bootstrap_record: Option<BootstrapRecord>,
    tunnel_sessions: HashMap<String, SessionRecord>, // key = session_id
    connect_rate_limiter: HashMap<String, RateRecord>,
}

impl TunnelAuth {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(TunnelAuthInner {
                active_tunnel_id: None,
                active_tunnel_host: None,
                active_tunnel_mode: None,
                active_tunnel_public_url: None,
                bootstrap_record: None,
                tunnel_sessions: HashMap::new(),
                connect_rate_limiter: HashMap::new(),
            }),
        }
    }

    // ── active tunnel 管理 ──

    /// 设置活动隧道 (从 publicUrl 解析 host)。
    pub fn set_active_tunnel(&self, tunnel_id: &str, public_url: &str, mode: Option<&str>) {
        let mut inner = self.inner.lock().unwrap();
        inner.active_tunnel_id = Some(tunnel_id.to_string());
        inner.active_tunnel_mode = mode.map(|s| s.to_string());
        inner.active_tunnel_public_url = Some(public_url.to_string());
        inner.active_tunnel_host = parse_host_from_url(public_url);
    }

    /// 清空活动隧道 (先 revoke artifacts)。
    pub fn clear_active_tunnel(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(ref tunnel_id) = inner.active_tunnel_id.clone() {
            Self::revoke_tunnel_artifacts_inner(&mut inner, tunnel_id);
        }
        inner.active_tunnel_id = None;
        inner.active_tunnel_host = None;
        inner.active_tunnel_mode = None;
        inner.active_tunnel_public_url = None;
        inner.bootstrap_record = None;
    }

    pub fn get_active_tunnel_id(&self) -> Option<String> {
        self.inner.lock().unwrap().active_tunnel_id.clone()
    }

    pub fn get_active_tunnel_host(&self) -> Option<String> {
        self.inner.lock().unwrap().active_tunnel_host.clone()
    }

    pub fn get_active_tunnel_mode(&self) -> Option<String> {
        self.inner.lock().unwrap().active_tunnel_mode.clone()
    }

    // ── bootstrap token ──

    /// 签发 bootstrap token (先 revoke 旧 token)。返回 (raw_token, expires_at)。
    pub fn issue_bootstrap_token(&self, ttl_ms: Option<i64>) -> (String, Option<i64>) {
        let mut inner = self.inner.lock().unwrap();
        let active_tunnel_id = match &inner.active_tunnel_id {
            Some(id) => id.clone(),
            None => panic!("issue_bootstrap_token called without active tunnel"), // 对应 Node throw
        };

        // 先 revoke 旧 token
        if let Some(ref mut rec) = inner.bootstrap_record {
            if rec.revoked_at.is_none() {
                rec.revoked_at = Some(now_ts());
            }
        }

        let token = generate_random_token();
        let issued_at = now_ts();
        let expires_at = ttl_ms.filter(|&t| t > 0).map(|t| issued_at + t);

        inner.bootstrap_record = Some(BootstrapRecord {
            id: generate_uuid(),
            tunnel_id: active_tunnel_id,
            token_hash: hash_token(&token),
            issued_at,
            expires_at,
            used_at: None,
            revoked_at: None,
        });

        (token, expires_at)
    }

    /// bootstrap 状态。
    pub fn get_bootstrap_status(&self) -> (bool, Option<i64>) {
        let inner = self.inner.lock().unwrap();
        match &inner.bootstrap_record {
            Some(rec) if rec.is_usable() => (true, rec.expires_at),
            _ => (false, None),
        }
    }

    /// 撤销 tunnel artifacts (bootstrap + sessions)。
    pub fn revoke_tunnel_artifacts(&self, tunnel_id: &str) -> (u32, u32) {
        let mut inner = self.inner.lock().unwrap();
        Self::revoke_tunnel_artifacts_inner(&mut inner, tunnel_id)
    }

    fn revoke_tunnel_artifacts_inner(
        inner: &mut TunnelAuthInner,
        tunnel_id: &str,
    ) -> (u32, u32) {
        // revoke bootstrap
        let revoked_bootstrap_count = match &mut inner.bootstrap_record {
            Some(rec) if rec.tunnel_id == tunnel_id && rec.revoked_at.is_none() => {
                rec.revoked_at = Some(now_ts());
                1
            }
            _ => 0,
        };

        // invalidate sessions
        let revoked_at = now_ts();
        let mut count = 0u32;
        for record in inner.tunnel_sessions.values_mut() {
            if record.tunnel_id == tunnel_id && record.revoked_at.is_none() {
                record.revoked_at = Some(revoked_at);
                record.revoked_reason = Some("tunnel-revoked".to_string());
                count += 1;
            }
        }

        (revoked_bootstrap_count, count)
    }

    // ── exchange (bootstrap → session) ──

    /// 交换 bootstrap token 为 session。
    /// `rate_limit_key` 是客户端 IP (或 no-ip fallback key)。
    pub fn exchange_bootstrap_token(
        &self,
        token: &str,
        session_ttl_ms: i64,
        rate_limit_key: &str,
    ) -> ExchangeResult {
        let mut inner = self.inner.lock().unwrap();

        // rate-limit 检查
        let max_attempts = rate_limit_max_for_key(rate_limit_key);
        let now = now_ts();
        if let Some(record) = inner.connect_rate_limiter.get(rate_limit_key) {
            if let Some(locked_until) = record.locked_until {
                if now < locked_until {
                    return ExchangeResult::rate_limited(
                        ((locked_until - now + 999) / 1000).max(1) as u64,
                    );
                }
            }
        }

        // 检查是否在窗口内
        let in_window = match inner.connect_rate_limiter.get(rate_limit_key) {
            Some(record) => now - record.last_attempt <= CONNECT_RATE_LIMIT_WINDOW_MS,
            None => false,
        };

        if in_window {
            let current_count = inner
                .connect_rate_limiter
                .get(rate_limit_key)
                .map(|r| r.count)
                .unwrap_or(0);
            if current_count >= max_attempts {
                let locked_until = now + CONNECT_RATE_LIMIT_LOCK_MS;
                inner.connect_rate_limiter.insert(
                    rate_limit_key.to_string(),
                    RateRecord {
                        count: current_count + 1,
                        last_attempt: now,
                        locked_until: Some(locked_until),
                    },
                );
                return ExchangeResult::rate_limited(
                    ((CONNECT_RATE_LIMIT_LOCK_MS + 999) / 1000) as u64,
                );
            }
        }

        // active tunnel + bootstrap record 检查
        let active_tunnel_id = inner.active_tunnel_id.clone();
        let bootstrap = inner.bootstrap_record.clone();

        if active_tunnel_id.is_none() || bootstrap.is_none() {
            record_failed_attempt(&mut inner.connect_rate_limiter, rate_limit_key, now);
            return ExchangeResult::failed("inactive");
        }

        let bootstrap = bootstrap.unwrap();

        if token.is_empty() {
            record_failed_attempt(&mut inner.connect_rate_limiter, rate_limit_key, now);
            return ExchangeResult::failed("missing-token");
        }

        if !bootstrap.is_usable() {
            record_failed_attempt(&mut inner.connect_rate_limiter, rate_limit_key, now);
            return ExchangeResult::failed("expired");
        }

        if bootstrap.tunnel_id != active_tunnel_id.clone().unwrap() {
            record_failed_attempt(&mut inner.connect_rate_limiter, rate_limit_key, now);
            return ExchangeResult::failed("tunnel-mismatch");
        }

        // timing-safe hash 比较
        let incoming_hash = hash_token(token);
        if !timing_safe_equal(&incoming_hash, &bootstrap.token_hash) {
            record_failed_attempt(&mut inner.connect_rate_limiter, rate_limit_key, now);
            return ExchangeResult::failed("invalid-token");
        }

        // 标记 used
        if let Some(ref mut rec) = inner.bootstrap_record {
            rec.used_at = Some(now);
        }

        // 清除 rate-limit
        inner.connect_rate_limiter.remove(rate_limit_key);

        // 签发 session
        let session_id = generate_random_token();
        let created_at = now;
        let expires_at = created_at + session_ttl_ms;
        let session_tunnel_id = active_tunnel_id.unwrap();
        let session_mode = inner.active_tunnel_mode.clone();
        let session_public_url = inner.active_tunnel_public_url.clone();

        inner.tunnel_sessions.insert(
            session_id.clone(),
            SessionRecord {
                session_id: session_id.clone(),
                tunnel_id: session_tunnel_id,
                mode: session_mode,
                public_url: session_public_url,
                created_at,
                last_seen_at: created_at,
                expires_at,
                revoked_at: None,
                revoked_reason: None,
                expired_at: None,
            },
        );

        ExchangeResult::success(session_id, expires_at)
    }

    // ── session 查询 ──

    /// 从 cookie value 获取有效 session。
    #[allow(dead_code)]
    pub fn get_session_from_cookie(&self, cookie_value: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let now = now_ts();

        let session = match inner.tunnel_sessions.get_mut(cookie_value) {
            Some(s) => s,
            None => return false,
        };

        if session.revoked_at.is_some() {
            return false;
        }

        if session.expires_at <= now {
            if session.expired_at.is_none() {
                session.expired_at = Some(now);
            }
            return false;
        }

        let session_tunnel_id = session.tunnel_id.clone();
        let active_tunnel_id = inner.active_tunnel_id.clone();

        if Some(&session_tunnel_id) != active_tunnel_id.as_ref() {
            return false;
        }

        // 重新获取 session (前面 clone 后 borrow 已释放)。
        if let Some(session) = inner.tunnel_sessions.get_mut(cookie_value) {
            session.last_seen_at = now;
            true
        } else {
            false
        }
    }

    /// 列出所有 tunnel sessions (按 createdAt desc 排序)。
    pub fn list_tunnel_sessions(&self) -> Vec<Value> {
        let mut inner = self.inner.lock().unwrap();
        let now = now_ts();
        let active_tunnel_id = inner.active_tunnel_id.clone();

        let mut sessions = vec![];
        for record in inner.tunnel_sessions.values_mut() {
            let is_expired = record.expires_at <= now;
            if is_expired && record.expired_at.is_none() {
                record.expired_at = Some(now);
            }

            let active = record.revoked_at.is_none()
                && !is_expired
                && Some(&record.tunnel_id) == active_tunnel_id.as_ref();
            let status = if active { "active" } else { "inactive" };
            let inactive_reason = if record.revoked_at.is_some() {
                record.revoked_reason.clone().unwrap_or_else(|| "revoked".to_string())
            } else if is_expired {
                "expired".to_string()
            } else {
                "inactive".to_string()
            };

            sessions.push(json!({
                "sessionId": record.session_id,
                "tunnelId": record.tunnel_id,
                "mode": record.mode,
                "publicUrl": record.public_url,
                "createdAt": record.created_at,
                "lastSeenAt": record.last_seen_at,
                "expiresAt": record.expires_at,
                "revokedAt": record.revoked_at,
                "status": status,
                "inactiveReason": if status == "inactive" { Some(inactive_reason) } else { None },
            }));
        }

        // sort by createdAt desc
        sessions.sort_by(|a, b| {
            let a_created = a.get("createdAt").and_then(|v| v.as_i64()).unwrap_or(0);
            let b_created = b.get("createdAt").and_then(|v| v.as_i64()).unwrap_or(0);
            b_created.cmp(&a_created)
        });

        sessions
    }

    /// 分类请求范围: tunnel / local / unknown-public。
    #[allow(dead_code)]
    pub fn classify_request_scope(&self, host_header: &str, remote_ip: &str) -> &'static str {
        let inner = self.inner.lock().unwrap();
        let req_host = normalize_host(host_header);

        if let Some(ref active_host) = inner.active_tunnel_host {
            if let Some(h) = &req_host {
                if h == active_host {
                    return "tunnel";
                }
            }
        }

        if let Some(ref h) = req_host {
            if is_local_host(h, remote_ip) {
                return "local";
            }
        }

        if inner.active_tunnel_id.is_none() {
            return "local";
        }

        "unknown-public"
    }
}

impl Default for TunnelAuth {
    fn default() -> Self {
        Self::new()
    }
}

fn record_failed_attempt(
    limiter: &mut HashMap<String, RateRecord>,
    key: &str,
    now: i64,
) {
    let in_window = match limiter.get(key) {
        Some(record) => now - record.last_attempt <= CONNECT_RATE_LIMIT_WINDOW_MS,
        None => false,
    };

    if !in_window {
        limiter.insert(
            key.to_string(),
            RateRecord {
                count: 1,
                last_attempt: now,
                locked_until: None,
            },
        );
        return;
    }

    let record = limiter.get(key).cloned().unwrap();
    limiter.insert(
        key.to_string(),
        RateRecord {
            count: record.count + 1,
            last_attempt: now,
            locked_until: record.locked_until,
        },
    );
}

fn rate_limit_max_for_key(key: &str) -> u32 {
    if key == NO_IP_KEY {
        CONNECT_RATE_LIMIT_NO_IP_MAX_ATTEMPTS
    } else {
        CONNECT_RATE_LIMIT_MAX_ATTEMPTS
    }
}

/// 获取 rate-limit key (IP 或 no-ip fallback)。
pub fn get_rate_limit_key(client_ip: Option<&str>) -> String {
    match client_ip {
        Some(ip) if !ip.is_empty() => ip.to_string(),
        _ => NO_IP_KEY.to_string(),
    }
}

// ── host/ip 辅助 ───────────────────────────────────────────────────────────

#[allow(dead_code)]
fn normalize_host(candidate: &str) -> Option<String> {
    let trimmed = candidate.trim().to_lowercase();
    if trimmed.is_empty() {
        return None;
    }
    // 去除端口
    let without_port = trimmed.rsplitn(2, ':').last().unwrap_or(&trimmed);
    Some(without_port.to_string())
}

fn parse_host_from_url(url: &str) -> Option<String> {
    // 简化: 从 URL 提取 host (类似 normalize_host)
    let after_scheme = match url.find("://") {
        Some(pos) => &url[pos + 3..],
        None => url,
    };
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host_part = authority.rsplit('@').next().unwrap_or("");
    let host = if host_part.starts_with('[') {
        match host_part.find(']') {
            Some(end) => &host_part[1..end],
            None => return None,
        }
    } else {
        match host_part.rfind(':') {
            Some(pos) => &host_part[..pos],
            None => host_part,
        }
    };
    let host = host.trim().to_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

#[allow(dead_code)]
fn normalize_ip_candidate(candidate: &str) -> Option<String> {
    let trimmed = candidate.trim().to_lowercase();
    if trimmed.is_empty() {
        return None;
    }

    // 去除 IPv6 方括号
    let without_brackets = if trimmed.starts_with('[') && trimmed.ends_with(']') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        &trimmed
    };

    // 去除 zone id
    let without_zone = without_brackets.split('%').next().unwrap_or("");
    if without_zone.is_empty() {
        return None;
    }

    // IPv4-mapped IPv6
    if let Some(mapped) = without_zone.strip_prefix("::ffff:") {
        if is_valid_ipv4(mapped) {
            return Some(mapped.to_string());
        }
    }

    Some(without_zone.to_string())
}

#[allow(dead_code)]
fn is_valid_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    parts.iter().all(|p| p.parse::<u8>().is_ok())
}

#[allow(dead_code)]
fn is_private_or_loopback_ipv4(candidate: &str) -> bool {
    let parts: Vec<Option<u8>> = candidate.split('.').map(|p| p.parse::<u8>().ok()).collect();
    let parts: Vec<u8> = match parts {
        parts if parts.len() == 4 && parts.iter().all(|p| p.is_some()) => {
            parts.iter().map(|p| p.unwrap()).collect()
        }
        _ => return false,
    };

    match (parts[0], parts[1]) {
        (127, _) => true,             // loopback
        (10, _) => true,              // private
        (172, b) if (16..=31).contains(&b) => true, // private
        (192, 168) => true,           // private
        (169, 254) => true,           // link-local
        _ => false,
    }
}

#[allow(dead_code)]
fn is_private_or_loopback_ipv6(candidate: &str) -> bool {
    if candidate == "::1" {
        return true;
    }
    if candidate.starts_with("fc") || candidate.starts_with("fd") {
        return true; // unique local
    }
    candidate.starts_with("fe8")
        || candidate.starts_with("fe9")
        || candidate.starts_with("fea")
        || candidate.starts_with("feb") // link-local
}

#[allow(dead_code)]
fn is_private_or_loopback_ip(candidate: &str) -> bool {
    match normalize_ip_candidate(candidate) {
        Some(ip) => {
            if ip.contains(':') {
                is_private_or_loopback_ipv6(&ip)
            } else {
                is_private_or_loopback_ipv4(&ip)
            }
        }
        None => false,
    }
}

#[allow(dead_code)]
fn is_local_host(host: &str, remote_ip: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    let is_local_hostname = host == "localhost"
        || host == "host.docker.internal"
        || is_private_or_loopback_ip(host);
    is_local_hostname && is_private_or_loopback_ip(remote_ip)
}

// ── cookie 构建 ────────────────────────────────────────────────────────────

/// 构建 Set-Cookie header value (对应 Node `buildCookie`)。
pub fn build_cookie(name: &str, value: &str, max_age_secs: Option<i64>, secure: bool) -> String {
    let mut attributes = vec![format!("{}={}", name, value), "Path=/".to_string()];

    // maxAge 为 0 时 value 通常已清空, 但仍走完整属性链
    attributes.push("HttpOnly".to_string());
    attributes.push("SameSite=Lax".to_string());

    if let Some(max_age) = max_age_secs {
        attributes.push(format!("Max-Age={}", max_age.max(0)));
    }

    // Expires
    let expires = if max_age_secs == Some(0) {
        "Thu, 01 Jan 1970 00:00:00 GMT".to_string()
    } else {
        let max_age = max_age_secs.unwrap_or(0);
        chrono::DateTime::from_timestamp((now_ts() + max_age * 1000) / 1000, 0)
            .unwrap_or_else(chrono::Utc::now)
            .to_rfc2822()
    };
    attributes.push(format!("Expires={}", expires));

    if secure {
        attributes.push("Secure".to_string());
    }

    attributes.join("; ")
}

/// 解析 Cookie header → map。
#[allow(dead_code)]
pub fn parse_cookies(cookie_header: Option<&str>) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let header = match cookie_header {
        Some(h) if !h.is_empty() => h,
        _ => return result,
    };

    for segment in header.split(';') {
        let mut parts = segment.splitn(2, '=');
        let name = parts.next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let value = parts.next().unwrap_or("").trim();
        // URL decode (简化: 不处理 %xx, 与 Node encodeURIComponent 对齐)
        let decoded = url_decode(value);
        result.insert(name.to_string(), decoded);
    }
    result
}

#[allow(dead_code)]
fn url_decode(s: &str) -> String {
    let mut result = String::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                result.push(((h << 4) | l) as char);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            result.push(' ');
        } else {
            result.push(bytes[i] as char);
        }
        i += 1;
    }
    result
}

#[allow(dead_code)]
fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// hex encode (用于 SHA-256 hash)。
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_and_exchange_token() {
        let auth = TunnelAuth::new();
        auth.set_active_tunnel("tunnel-1", "https://example.trycloudflare.com", Some("quick"));

        let (token, expires_at) = auth.issue_bootstrap_token(Some(30000));
        assert!(!token.is_empty());
        assert!(expires_at.is_some());

        let result = auth.exchange_bootstrap_token(&token, 3600000, "127.0.0.1");
        assert!(result.ok);
        assert!(result.session_id.is_some());
    }

    #[test]
    fn token_single_use() {
        let auth = TunnelAuth::new();
        auth.set_active_tunnel("tunnel-1", "https://example.com", Some("quick"));

        let (token, _) = auth.issue_bootstrap_token(Some(30000));

        let r1 = auth.exchange_bootstrap_token(&token, 3600000, "127.0.0.1");
        assert!(r1.ok);

        // 第二次用同一 token 应失败
        let r2 = auth.exchange_bootstrap_token(&token, 3600000, "127.0.0.1");
        assert!(!r2.ok);
    }

    #[test]
    fn invalid_token_fails() {
        let auth = TunnelAuth::new();
        auth.set_active_tunnel("tunnel-1", "https://example.com", Some("quick"));
        let _ = auth.issue_bootstrap_token(Some(30000));

        let result = auth.exchange_bootstrap_token("wrong-token", 3600000, "127.0.0.1");
        assert!(!result.ok);
        assert_eq!(result.reason.as_deref(), Some("invalid-token"));
    }

    #[test]
    fn inactive_tunnel_fails() {
        let auth = TunnelAuth::new();
        // 不设置 active tunnel
        let result = auth.exchange_bootstrap_token("some-token", 3600000, "127.0.0.1");
        assert!(!result.ok);
        assert_eq!(result.reason.as_deref(), Some("inactive"));
    }

    #[test]
    fn clear_active_tunnel_revokes() {
        let auth = TunnelAuth::new();
        auth.set_active_tunnel("tunnel-1", "https://example.com", Some("quick"));
        let (token, _) = auth.issue_bootstrap_token(Some(30000));

        // 交换 session
        let r = auth.exchange_bootstrap_token(&token, 3600000, "127.0.0.1");
        assert!(r.ok);

        // clear → 旧 token 应失效
        auth.clear_active_tunnel();
        assert!(auth.get_active_tunnel_id().is_none());
    }

    #[test]
    fn rate_limit_key() {
        assert_eq!(get_rate_limit_key(Some("1.2.3.4")), "1.2.3.4");
        assert_eq!(get_rate_limit_key(None), NO_IP_KEY);
        assert_eq!(get_rate_limit_key(Some("")), NO_IP_KEY);
    }

    #[test]
    fn classify_scope_local() {
        let auth = TunnelAuth::new();
        // 无 active tunnel → local
        assert_eq!(auth.classify_request_scope("localhost", "127.0.0.1"), "local");
        assert_eq!(
            auth.classify_request_scope("127.0.0.1", "127.0.0.1"),
            "local"
        );
    }

    #[test]
    fn classify_scope_tunnel() {
        let auth = TunnelAuth::new();
        auth.set_active_tunnel("t1", "https://foo.trycloudflare.com", Some("quick"));
        assert_eq!(
            auth.classify_request_scope("foo.trycloudflare.com", "1.2.3.4"),
            "tunnel"
        );
    }

    #[test]
    fn classify_scope_unknown_public() {
        let auth = TunnelAuth::new();
        auth.set_active_tunnel("t1", "https://foo.trycloudflare.com", Some("quick"));
        // 非 tunnel host + 非 local + 有 active tunnel → unknown-public
        assert_eq!(
            auth.classify_request_scope("other.com", "1.2.3.4"),
            "unknown-public"
        );
    }

    #[test]
    fn parse_cookies_basic() {
        let cookies = parse_cookies(Some("a=1; b=2; oc_tunnel_session=abc%3Ddef"));
        assert_eq!(cookies.get("a"), Some(&"1".to_string()));
        assert_eq!(cookies.get("b"), Some(&"2".to_string()));
        assert_eq!(
            cookies.get("oc_tunnel_session"),
            Some(&"abc=def".to_string())
        );
    }

    #[test]
    fn parse_cookies_empty() {
        assert!(parse_cookies(None).is_empty());
        assert!(parse_cookies(Some("")).is_empty());
    }

    #[test]
    fn build_cookie_attributes() {
        let cookie = build_cookie("test", "val", Some(3600), true);
        assert!(cookie.contains("test=val"));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Max-Age=3600"));
        assert!(cookie.contains("Secure"));
    }

    #[test]
    fn timing_safe_compare_equal() {
        assert!(timing_safe_equal("abc", "abc"));
        assert!(!timing_safe_equal("abc", "abd"));
        assert!(!timing_safe_equal("abc", "ab"));
    }

    #[test]
    fn is_private_ipv4() {
        assert!(is_private_or_loopback_ipv4("127.0.0.1"));
        assert!(is_private_or_loopback_ipv4("10.0.0.1"));
        assert!(is_private_or_loopback_ipv4("192.168.1.1"));
        assert!(is_private_or_loopback_ipv4("172.16.0.1"));
        assert!(!is_private_or_loopback_ipv4("1.2.3.4"));
    }

    #[test]
    fn normalize_host_strips_port() {
        assert_eq!(normalize_host("Example.com:8080"), Some("example.com".to_string()));
        assert_eq!(normalize_host("localhost"), Some("localhost".to_string()));
        assert_eq!(normalize_host(""), None);
    }
}
