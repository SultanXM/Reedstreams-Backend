use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Mutex};
use tracing::info;

/// In-memory key-value store with TTL support
/// This replaces Redis for standalone/edge deployments without external dependencies
#[derive(Debug, Clone)]
pub struct InMemoryDatabase {
    // Main data store: key -> (value, optional_expiry)
    data: Arc<RwLock<HashMap<String, (String, Option<Instant>)>>>,
}

impl InMemoryDatabase {
    pub async fn new() -> anyhow::Result<Self> {
        info!("In-memory database initialized (no external Redis)");
        Ok(Self {
            data: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Clean up expired keys (called periodically or on access)
    async fn cleanup_expired(&self) {
        let mut data = self.data.write().await;
        let now = Instant::now();
        data.retain(|_, (_, expiry)| expiry.map_or(true, |e| e > now));
    }

    /// Get a value by key (returns None if expired)
    pub async fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        // Periodically clean up (simple approach: clean on every Nth call could be added)
        self.cleanup_expired().await;

        let data = self.data.read().await;

        if let Some((value, expiry)) = data.get(key) {
            if let Some(exp) = expiry {
                if Instant::now() > *exp {
                    return Ok(None); // Expired
                }
            }
            return Ok(Some(value.clone()));
        }

        Ok(None)
    }

    /// Set a value without TTL
    pub async fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let mut data = self.data.write().await;
        data.insert(key.to_string(), (value.to_string(), None));
        Ok(())
    }

    /// Set a value with TTL in seconds
    pub async fn set_ex(&self, key: &str, value: &str, ttl_secs: u64) -> anyhow::Result<()> {
        let mut data = self.data.write().await;
        let expiry = Instant::now() + Duration::from_secs(ttl_secs);
        data.insert(key.to_string(), (value.to_string(), Some(expiry)));
        Ok(())
    }

    /// Delete a key
    pub async fn del(&self, key: &str) -> anyhow::Result<u32> {
        let mut data = self.data.write().await;
        if data.remove(key).is_some() {
            Ok(1)
        } else {
            Ok(0)
        }
    }

    /// Delete multiple keys
    pub async fn del_multiple(&self, keys: &[String]) -> anyhow::Result<u32> {
        let mut data = self.data.write().await;
        let mut count = 0;
        for key in keys {
            if data.remove(key).is_some() {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Get TTL for a key (returns -1 if no expiry, -2 if not exists)
    pub async fn ttl(&self, key: &str) -> anyhow::Result<i64> {
        let data = self.data.read().await;

        if let Some((_, expiry)) = data.get(key) {
            match expiry {
                Some(exp) => {
                    let remaining = exp.duration_since(Instant::now()).as_secs() as i64;
                    if remaining > 0 {
                        Ok(remaining)
                    } else {
                        Ok(-2) // Expired
                    }
                }
                None => Ok(-1), // No expiry
            }
        } else {
            Ok(-2) // Key doesn't exist
        }
    }

    /// Scan keys matching a pattern (simplified: supports * wildcard at end only)
    pub async fn scan(&self, pattern: &str) -> anyhow::Result<Vec<String>> {
        self.cleanup_expired().await;

        let data = self.data.read().await;
        let mut keys = Vec::new();

        // Simple pattern matching: support "prefix:*" or "*" patterns
        if pattern == "*" {
            keys.extend(data.keys().cloned());
        } else if pattern.ends_with('*') {
            let prefix = &pattern[..pattern.len() - 1];
            for key in data.keys() {
                if key.starts_with(prefix) {
                    keys.push(key.clone());
                }
            }
        } else if pattern.contains('*') {
            // Handle patterns like "provider:*:something" - simplified
            let parts: Vec<&str> = pattern.split('*').collect();
            for key in data.keys() {
                if key.starts_with(parts[0]) && key.ends_with(parts[parts.len() - 1]) {
                    keys.push(key.clone());
                }
            }
        } else {
            // Exact match
            if data.contains_key(pattern) {
                keys.push(pattern.to_string());
            }
        }

        Ok(keys)
    }

    /// Get multiple values by keys (MGET equivalent)
    pub async fn mget(&self, keys: &[String]) -> anyhow::Result<Vec<Option<String>>> {
        let data = self.data.read().await;
        let mut results = Vec::new();
        let now = Instant::now();

        for key in keys {
            let value = data.get(key).and_then(|(v, expiry)| {
                if let Some(exp) = expiry {
                    if now > *exp {
                        return None;
                    }
                }
                Some(v.clone())
            });
            results.push(value);
        }

        Ok(results)
    }

    /// Increment a key and set TTL if it doesn't exist
    pub async fn incr(&self, key: &str, delta: u32) -> anyhow::Result<u32> {
        let mut data = self.data.write().await;

        let entry = data.entry(key.to_string()).or_insert_with(|| {
            ("0".to_string(), Some(Instant::now() + Duration::from_secs(60))) // Default TTL
        });

        // Update expiry if needed (keep existing or set new)
        if entry.1.is_none() {
            entry.1 = Some(Instant::now() + Duration::from_secs(60));
        }

        let current: u32 = entry.0.parse().unwrap_or(0);
        let new_value = current + delta;
        entry.0 = new_value.to_string();

        Ok(new_value)
    }

    /// Health check - returns response time in milliseconds
    pub async fn health_check(&self) -> anyhow::Result<f64> {
        let start = std::time::Instant::now();
        // Just do a simple operation
        let _ = self.get("__health_check__").await?;
        let elapsed = start.elapsed();
        Ok(elapsed.as_secs_f64() * 1000.0)
    }
}

/// Wrapper to provide a compatible interface with RedisDatabase
#[derive(Debug, Clone)]
pub struct MemoryDatabase {
    pub store: InMemoryDatabase,
    /// Viewers tracking: match_id -> set of viewer keys (for view counter)
    pub viewers: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    /// View counts: match_id -> count
    pub view_counts: Arc<Mutex<HashMap<String, u64>>>,
}

impl MemoryDatabase {
    pub async fn connect(_connection_string: &str) -> anyhow::Result<Self> {
        // Ignore connection string for in-memory, or we could parse it for config
        let store = InMemoryDatabase::new().await?;
        Ok(Self { 
            store,
            viewers: Arc::new(Mutex::new(HashMap::new())),
            view_counts: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn health_check(&self) -> anyhow::Result<f64> {
        self.store.health_check().await
    }
}

/// Create an in-memory database (no external Redis needed)
pub async fn create_memory_db() -> anyhow::Result<MemoryDatabase> {
    MemoryDatabase::connect("memory://localhost").await
}
