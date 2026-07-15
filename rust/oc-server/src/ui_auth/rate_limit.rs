//! 登录限速器。
//!
//! 移植自 `ui-auth.js` lines 25-210。
//! 全内存状态, Mutex<HashMap> 保护。
//! per-IP key (no-IP fallback: `'rate-limit:no-ip'`, 3 次限制)。
//! 5min 窗口内 ≥maxAttempts 次 → 锁定 15min。

use std::collections::HashMap;
use std::sync::Mutex;

use super::types::get_rate_limit_key;
use super::{
    RATE_LIMIT_LOCKOUT_MS, RATE_LIMIT_MAX_ATTEMPTS, RATE_LIMIT_NO_IP_MAX_ATTEMPTS, RATE_LIMIT_WINDOW_MS,
};

/// 限速记录。
struct RateRecord {
    count: u32,
    last_attempt: i64, // epoch ms
    locked_until: Option<i64>, // epoch ms
}

/// 限速检查结果。
pub struct RateLimitResult {
    pub allowed: bool,
    pub limit: u32,
    pub remaining: u32,
    pub reset: i64, // epoch seconds
    pub retry_after: Option<i32>, // seconds, when locked
    #[allow(dead_code)]
    pub locked: bool,
}

/// 登录限速器 (全内存)。
pub struct LoginRateLimiter {
    inner: Mutex<HashMap<String, RateRecord>>,
}

impl LoginRateLimiter {
    pub fn new() -> Self {
        LoginRateLimiter {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 获取限速配置 (移植自 `getRateLimitConfig`, ui-auth.js:51-62)。
    fn get_config(key: &str) -> u32 {
        if key == "rate-limit:no-ip" {
            RATE_LIMIT_NO_IP_MAX_ATTEMPTS
        } else {
            RATE_LIMIT_MAX_ATTEMPTS
        }
    }

    /// 检查限速 (移植自 `checkRateLimit`, ui-auth.js:71-144)。
    pub fn check(&self, client_ip: Option<&str>) -> RateLimitResult {
        let key = get_rate_limit_key(client_ip);
        let now = now_millis();
        let max_attempts = Self::get_config(&key);

        let mut inner = self.inner.lock().unwrap();
        let record = inner.get(&key);

        // 1. 已锁定
        if let Some(rec) = record {
            if let Some(locked_until) = rec.locked_until {
                if now < locked_until {
                    return RateLimitResult {
                        allowed: false,
                        retry_after: Some(ceil_div(locked_until - now, 1000) as i32),
                        locked: true,
                        limit: max_attempts,
                        remaining: 0,
                        reset: ceil_div(locked_until, 1000),
                    };
                }
            }
        }

        // 2. 锁定已过期 → 清除
        if let Some(rec) = record {
            if let Some(locked_until) = rec.locked_until {
                if now >= locked_until {
                    inner.remove(&key);
                }
            }
        }

        // 重新获取 (可能已被删除)
        let record = inner.get(&key);

        // 3. 无记录或窗口已过
        if record.is_none() {
            return RateLimitResult {
                allowed: true,
                limit: max_attempts,
                remaining: max_attempts,
                reset: ceil_div(now + RATE_LIMIT_WINDOW_MS, 1000),
                retry_after: None,
                locked: false,
            };
        }

        let rec = record.unwrap();
        if now - rec.last_attempt > RATE_LIMIT_WINDOW_MS {
            return RateLimitResult {
                allowed: true,
                limit: max_attempts,
                remaining: max_attempts,
                reset: ceil_div(now + RATE_LIMIT_WINDOW_MS, 1000),
                retry_after: None,
                locked: false,
            };
        }

        // 提取所需数据 (释放不可变借用)
        let rec_count = rec.count;
        let rec_last_attempt = rec.last_attempt;

        // 4. 超限 → 锁定
        if rec_count >= max_attempts {
            let locked_until = now + RATE_LIMIT_LOCKOUT_MS;
            inner.insert(
                key.clone(),
                RateRecord {
                    count: rec_count + 1,
                    last_attempt: now,
                    locked_until: Some(locked_until),
                },
            );
            return RateLimitResult {
                allowed: false,
                retry_after: Some(ceil_div(RATE_LIMIT_LOCKOUT_MS, 1000) as i32),
                locked: true,
                limit: max_attempts,
                remaining: 0,
                reset: ceil_div(locked_until, 1000),
            };
        }

        // 5. 窗口内未超限
        let remaining = max_attempts - rec_count;
        let reset = ceil_div(rec_last_attempt + RATE_LIMIT_WINDOW_MS, 1000);
        RateLimitResult {
            allowed: true,
            limit: max_attempts,
            remaining,
            reset,
            retry_after: None,
            locked: false,
        }
    }

    /// 记录失败尝试 (移植自 `recordFailedAttempt`, ui-auth.js:146-168)。
    pub fn record_failure(&self, client_ip: Option<&str>) {
        let key = get_rate_limit_key(client_ip);
        let now = now_millis();

        let mut inner = self.inner.lock().unwrap();
        let should_reset = match inner.get(&key) {
            None => true,
            Some(rec) => now - rec.last_attempt > RATE_LIMIT_WINDOW_MS,
        };

        if should_reset {
            inner.insert(
                key,
                RateRecord {
                    count: 1,
                    last_attempt: now,
                    locked_until: None,
                },
            );
        } else {
            let rec = inner.get_mut(&key).unwrap();
            rec.count += 1;
            rec.last_attempt = now;
        }
    }

    /// 清除限速 (移植自 `clearRateLimit`, ui-auth.js:170-179)。
    pub fn clear(&self, client_ip: Option<&str>) {
        let key = get_rate_limit_key(client_ip);
        let mut inner = self.inner.lock().unwrap();
        inner.remove(&key);
    }

    /// 清理过期/stale 记录 (移植自 `cleanupRateLimitRecords`, ui-auth.js:181-194)。
    #[allow(dead_code)]
    pub fn cleanup(&self) {
        let now = now_millis();
        let mut inner = self.inner.lock().unwrap();
        inner.retain(|_, rec| {
            let is_expired = rec.locked_until.is_some_and(|lu| now >= lu);
            let is_stale = now - rec.last_attempt > super::RATE_LIMIT_CLEANUP_MS;
            !(is_expired || is_stale)
        });
    }
}

impl Default for LoginRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

// ─── 辅助函数 ────────────────────────────────────────

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn ceil_div(a: i64, b: i64) -> i64 {
    (a + b - 1) / b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limit_first_attempt_allowed() {
        let limiter = LoginRateLimiter::new();
        let result = limiter.check(Some("1.2.3.4"));
        assert!(result.allowed);
        assert_eq!(result.limit, 10);
        assert_eq!(result.remaining, 10);
        assert!(!result.locked);
    }

    #[test]
    fn test_rate_limit_no_ip_fallback() {
        let limiter = LoginRateLimiter::new();
        let result = limiter.check(None);
        assert!(result.allowed);
        assert_eq!(result.limit, 3); // no-IP max
    }

    #[test]
    fn test_rate_limit_locks_after_max() {
        let limiter = LoginRateLimiter::new();
        // 10 次失败
        for _ in 0..10 {
            limiter.record_failure(Some("1.2.3.4"));
        }
        // 第 11 次检查应被锁
        let result = limiter.check(Some("1.2.3.4"));
        assert!(!result.allowed);
        assert!(result.locked);
        assert!(result.retry_after.is_some());
    }

    #[test]
    fn test_rate_limit_no_ip_locks_after_3() {
        let limiter = LoginRateLimiter::new();
        for _ in 0..3 {
            limiter.record_failure(None);
        }
        let result = limiter.check(None);
        assert!(!result.allowed);
        assert!(result.locked);
    }

    #[test]
    fn test_rate_limit_clear_resets() {
        let limiter = LoginRateLimiter::new();
        for _ in 0..10 {
            limiter.record_failure(Some("1.2.3.4"));
        }
        limiter.clear(Some("1.2.3.4"));
        let result = limiter.check(Some("1.2.3.4"));
        assert!(result.allowed);
    }

    #[test]
    fn test_rate_limit_different_ips_independent() {
        let limiter = LoginRateLimiter::new();
        for _ in 0..10 {
            limiter.record_failure(Some("1.2.3.4"));
        }
        // Different IP still allowed
        let result = limiter.check(Some("5.6.7.8"));
        assert!(result.allowed);
    }

    #[test]
    fn test_rate_limit_progressive_remaining() {
        let limiter = LoginRateLimiter::new();
        limiter.record_failure(Some("1.2.3.4"));
        let result = limiter.check(Some("1.2.3.4"));
        assert!(result.allowed);
        assert_eq!(result.remaining, 9);
    }
}
