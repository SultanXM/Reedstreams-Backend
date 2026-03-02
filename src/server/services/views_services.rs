// View counter service - tracks unique viewers per match using Redis
use async_trait::async_trait;
use mockall::automock;
use std::sync::Arc;
use tracing::{info, debug};

use crate::database::Database;
use crate::server::error::AppResult;

pub type DynViewsService = Arc<dyn ViewsServiceTrait + Send + Sync>;

/// Helper to convert Redis errors to anyhow errors
fn redis_err(e: redis::RedisError) -> anyhow::Error {
    anyhow::anyhow!("Redis error: {}", e)
}

/// Generate a unique view ID with timestamp to ensure every view counts
fn generate_view_id(match_id: &str, viewer_hash: &str) -> String {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{}:{}:{}", match_id, viewer_hash, timestamp)
}

#[automock]
#[async_trait]
pub trait ViewsServiceTrait {
    /// Increment view count for a match, returns new total
    async fn increment_view(&self, match_id: &str, viewer_key: &str) -> AppResult<u64>;
    
    /// Get current view count for a match
    async fn get_view_count(&self, match_id: &str) -> AppResult<u64>;
    
    /// Get view counts for multiple matches
    async fn get_view_counts(&self, match_ids: &[String]) -> AppResult<Vec<(String, u64)>>;
}

#[derive(Clone)]
pub struct ViewsService {
    db: Arc<Database>,
}

impl ViewsService {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Generate Redis key for match viewers set
    fn viewers_key(match_id: &str) -> String {
        format!("views:match:{}:viewers", match_id)
    }

    /// Generate Redis key for match view count (counter)
    fn count_key(match_id: &str) -> String {
        format!("views:match:{}:count", match_id)
    }
}

#[async_trait]
impl ViewsServiceTrait for ViewsService {
    async fn increment_view(&self, match_id: &str, viewer_key: &str) -> AppResult<u64> {
        let count_key = Self::count_key(match_id);
        let unique_view_id = generate_view_id(match_id, viewer_key);
        
        debug!("tracking view for match {} from viewer {} (id: {})", match_id, viewer_key, unique_view_id);

        match self.db.as_ref() {
            Database::Redis(_) => {
                // Use Redis for persistent storage
                let mut conn = self.db.redis_connection().clone();
                
                // Always increment - no deduplication!
                let count: u64 = redis::cmd("INCR")
                    .arg(&count_key)
                    .query_async(&mut conn)
                    .await
                    .map_err(redis_err)?;
                
                // Set expiration on counter (24 hours)
                let _: i32 = redis::cmd("EXPIRE")
                    .arg(&count_key)
                    .arg(86400i32)
                    .query_async(&mut conn)
                    .await
                    .map_err(redis_err)?;
                
                info!("view counted for match {}, total: {}", match_id, count);
                Ok(count)
            }
            Database::Memory(mem_db) => {
                // Use in-memory storage (for local dev/testing)
                let mut counts = mem_db.view_counts.lock().await;
                
                // Always increment - no deduplication!
                let count = counts.entry(match_id.to_string()).or_insert(0);
                *count += 1;
                info!("view counted for match {}, total: {}", match_id, *count);
                Ok(*count as u64)
            }
        }
    }

    async fn get_view_count(&self, match_id: &str) -> AppResult<u64> {
        let count_key = Self::count_key(match_id);
        
        match self.db.as_ref() {
            Database::Redis(_) => {
                let mut conn = self.db.redis_connection().clone();
                let count: u64 = redis::cmd("GET")
                    .arg(&count_key)
                    .query_async(&mut conn)
                    .await
                    .unwrap_or(0);
                Ok(count)
            }
            Database::Memory(mem_db) => {
                let counts = mem_db.view_counts.lock().await;
                let count = counts.get(match_id).copied().unwrap_or(0);
                Ok(count as u64)
            }
        }
    }

    async fn get_view_counts(&self, match_ids: &[String]) -> AppResult<Vec<(String, u64)>> {
        let mut results = Vec::with_capacity(match_ids.len());
        
        for match_id in match_ids {
            let count = self.get_view_count(match_id).await?;
            results.push((match_id.clone(), count));
        }
        
        Ok(results)
    }
}
