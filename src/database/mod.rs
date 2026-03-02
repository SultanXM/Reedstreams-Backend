mod redis_connection;
mod memory_connection;

pub mod stream;

pub use redis_connection::*;
pub use memory_connection::*;

use tracing::info;

/// Unified database type that can be either Redis or in-memory
#[derive(Debug, Clone)]
pub enum Database {
    Redis(RedisDatabase),
    Memory(MemoryDatabase),
}

impl Database {
    /// Connect to database - uses Redis if URL provided, otherwise falls back to in-memory
    pub async fn connect(connection_string: &str) -> anyhow::Result<Self> {
        if connection_string.is_empty() || connection_string == "memory://localhost" {
            info!("Using in-memory database (no persistence)");
            let db = MemoryDatabase::connect(connection_string).await?;
            Ok(Database::Memory(db))
        } else {
            info!("Connecting to Redis...");
            let db = RedisDatabase::connect(connection_string).await?;
            Ok(Database::Redis(db))
        }
    }

    /// Create in-memory database directly
    pub async fn in_memory() -> anyhow::Result<Self> {
        info!("Using in-memory database (no persistence)");
        let db = MemoryDatabase::connect("memory://localhost").await?;
        Ok(Database::Memory(db))
    }

    /// Health check
    pub async fn health_check(&self) -> anyhow::Result<f64> {
        match self {
            Database::Redis(db) => db.health_check().await,
            Database::Memory(db) => db.health_check().await,
        }
    }

    /// Get internal Redis connection (panics if using memory - use with caution)
    pub fn redis_connection(&self) -> &redis::aio::MultiplexedConnection {
        match self {
            Database::Redis(db) => &db.connection,
            Database::Memory(_) => panic!("Requested Redis connection but using in-memory database"),
        }
    }
}

/// Helper to check if we should use memory or Redis
pub fn should_use_memory(connection_string: Option<&str>) -> bool {
    match connection_string {
        None => true,
        Some(s) => s.is_empty() || s == "memory://localhost",
    }
}
