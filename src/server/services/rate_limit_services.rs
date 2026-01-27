use std::sync::Arc;

use redis::AsyncCommands;
use tracing::{debug, error, info, warn};

use crate::database::RedisDatabase;

#[derive(Clone)]
pub struct RateLimitConfig {
    /// maximum requests per window for general API calls
    pub max_requests_per_window: u32,
    /// window duration in seconds for rate limiting
    pub window_seconds: u64,
    /// maximum errors before a user gets timed out
    pub max_errors_before_timeout: u32,
    /// error tracking window in seconds
    pub error_window_seconds: u64,
    /// timeout duration in seconds when error threshold is exceeded
    pub timeout_duration_seconds: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            // these should all be changed as you see fit
            max_requests_per_window: 500, // 500 requests per window (very generous)
            window_seconds: 60,           // per minute
            max_errors_before_timeout: 50, // 50 errors triggers timeout
            error_window_seconds: 600,    // within 10 minutes
            timeout_duration_seconds: 300, // 5 minute timeout
        }
    }
}

#[derive(Debug, Clone)]
pub enum RateLimitResult {
    /// request is allowed
    Allowed { remaining: u32, reset_at: i64 },
    /// user has exceeded rate limit
    RateLimited { retry_after: u64 },
    /// user is timed out due to too many errors
    TimedOut { reason: String, retry_after: u64 },
}

pub type DynRateLimitService = Arc<dyn RateLimitServiceTrait + Send + Sync>;

#[async_trait::async_trait]
pub trait RateLimitServiceTrait {
    /// check if a request should be allowed
    async fn check_rate_limit(&self, client_id: &str) -> RateLimitResult;

    /// record an error for a client (proxy failures, etc.)
    async fn record_error(&self, client_id: &str, error_type: &str);

    /// check if client is currently timed out
    async fn is_user_timed_out(&self, client_id: &str) -> Option<(String, u64)>;

    /// manually timeout a client
    async fn timeout_user(&self, client_id: &str, reason: &str, duration_seconds: u64);

    /// clear a client's timeout
    async fn clear_timeout(&self, client_id: &str) -> bool;

    /// get error count for a client
    async fn get_error_count(&self, client_id: &str) -> u32;

    /// check if client is exempt from rate limiting
    async fn is_exempt(&self, client_id: &str) -> bool;

    /// set a client as exempt from rate limiting
    async fn set_exempt(&self, client_id: &str, exempt: bool);
}

/// rate limiting based on client identifiers (probably not the most reliable so you can just
/// increaxe the limits if you see timeouts occuring too much)
pub struct EdgeRateLimitService {
    redis: Arc<RedisDatabase>,
    config: RateLimitConfig,
}

impl EdgeRateLimitService {
    pub fn new(redis: Arc<RedisDatabase>) -> Self {
        Self {
            redis,
            config: RateLimitConfig::default(),
        }
    }

    fn rate_limit_key(&self, client_id: &str) -> String {
        format!("edge_rate_limit:{}", client_id)
    }

    fn error_count_key(&self, client_id: &str) -> String {
        format!("edge_error_count:{}", client_id)
    }

    fn timeout_key(&self, client_id: &str) -> String {
        format!("edge_timeout:{}", client_id)
    }
}

#[async_trait::async_trait]
impl RateLimitServiceTrait for EdgeRateLimitService {
    async fn check_rate_limit(&self, client_id: &str) -> RateLimitResult {
        if let Some((reason, retry_after)) = self.is_user_timed_out(client_id).await {
            return RateLimitResult::TimedOut {
                reason,
                retry_after,
            };
        }

        let key = self.rate_limit_key(client_id);
        let mut conn = self.redis.connection.clone();

        let result: Result<(u32, i32, i64), redis::RedisError> = redis::pipe()
            .atomic()
            .incr(&key, 1u32)
            .expire(&key, self.config.window_seconds as i64)
            .ttl(&key)
            .query_async(&mut conn)
            .await;

        match result {
            Ok((count, _expire_result, ttl)) => {
                let reset_at = chrono::Utc::now().timestamp() + ttl;

                if count > self.config.max_requests_per_window {
                    debug!(
                        "Client {} rate limited: {} requests in window",
                        client_id, count
                    );
                    RateLimitResult::RateLimited {
                        retry_after: ttl.max(1) as u64,
                    }
                } else {
                    RateLimitResult::Allowed {
                        remaining: self.config.max_requests_per_window.saturating_sub(count),
                        reset_at,
                    }
                }
            }
            Err(e) => {
                error!("Rate limit check failed for client {}: {}", client_id, e);
                RateLimitResult::Allowed {
                    remaining: 0,
                    reset_at: chrono::Utc::now().timestamp() + self.config.window_seconds as i64,
                }
            }
        }
    }

    async fn record_error(&self, client_id: &str, error_type: &str) {
        let key = self.error_count_key(client_id);
        let mut conn = self.redis.connection.clone();

        let result: Result<(u32, i32), redis::RedisError> = redis::pipe()
            .atomic()
            .incr(&key, 1u32)
            .expire(&key, self.config.error_window_seconds as i64)
            .query_async(&mut conn)
            .await;

        match result {
            Ok((count, _expire_result)) => {
                debug!(
                    "Client {} error recorded ({}): count now {}",
                    client_id, error_type, count
                );

                if count >= self.config.max_errors_before_timeout {
                    warn!(
                        "Client {} exceeded error threshold ({} errors), applying timeout",
                        client_id, count
                    );
                    self.timeout_user(
                        client_id,
                        &format!(
                            "Automatic timeout: {} errors in {} seconds",
                            count, self.config.error_window_seconds
                        ),
                        self.config.timeout_duration_seconds,
                    )
                    .await;
                }
            }
            Err(e) => {
                error!("Failed to record error for client {}: {}", client_id, e);
            }
        }
    }

    async fn is_user_timed_out(&self, client_id: &str) -> Option<(String, u64)> {
        let key = self.timeout_key(client_id);
        let mut conn = self.redis.connection.clone();

        let result: Result<(Option<String>, i64), redis::RedisError> = redis::pipe()
            .get(&key)
            .ttl(&key)
            .query_async(&mut conn)
            .await;

        match result {
            Ok((Some(reason), ttl)) if ttl > 0 => Some((reason, ttl as u64)),
            Ok(_) => None,
            Err(e) => {
                error!("Failed to check timeout for client {}: {}", client_id, e);
                None
            }
        }
    }

    async fn timeout_user(&self, client_id: &str, reason: &str, duration_seconds: u64) {
        let key = self.timeout_key(client_id);
        let mut conn = self.redis.connection.clone();

        let result: Result<(), redis::RedisError> =
            conn.set_ex(&key, reason, duration_seconds).await;

        match result {
            Ok(_) => {
                info!(
                    "Client {} timed out for {} seconds: {}",
                    client_id, duration_seconds, reason
                );
            }
            Err(e) => {
                error!("Failed to timeout client {}: {}", client_id, e);
            }
        }
    }

    async fn clear_timeout(&self, client_id: &str) -> bool {
        let key = self.timeout_key(client_id);
        let mut conn = self.redis.connection.clone();

        let result: Result<i32, redis::RedisError> = conn.del(&key).await;

        match result {
            Ok(deleted) => deleted > 0,
            Err(e) => {
                error!("Failed to clear timeout for client {}: {}", client_id, e);
                false
            }
        }
    }

    async fn get_error_count(&self, client_id: &str) -> u32 {
        let key = self.error_count_key(client_id);
        let mut conn = self.redis.connection.clone();

        let result: Result<Option<u32>, redis::RedisError> = conn.get(&key).await;

        match result {
            Ok(Some(count)) => count,
            Ok(None) => 0,
            Err(e) => {
                error!("Failed to get error count for client {}: {}", client_id, e);
                0
            }
        }
    }

    async fn is_exempt(&self, _client_id: &str) -> bool {
        // no exemptions in edge mode - everyone gets rate limited equally
        false
    }

    async fn set_exempt(&self, _client_id: &str, _exempt: bool) {
        // just noop in the edge mode
    }
}
