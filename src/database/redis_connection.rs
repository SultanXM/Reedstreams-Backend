use anyhow::Context;
use redis::Client;
use redis::aio::MultiplexedConnection;
use std::time::Instant;
use tracing::info;

#[derive(Debug, Clone)]
pub struct RedisDatabase {
    pub connection: MultiplexedConnection,
}

// this one is so much simpler than postgres oh my god
// not sure if its my problem or upstash but fetching takes a fucking year
impl RedisDatabase {
    pub async fn connect(connection_string: &str) -> anyhow::Result<Self> {
        let client = Client::open(connection_string).context("Failed to create Redis client")?;

        let connection = client
            .get_multiplexed_tokio_connection()
            .await
            .context("Failed to connect to Redis database")?;

        info!("Redis connection established");

        Ok(Self { connection })
    }

    /// does a ping health check, not needed but it's here and is nice
    pub async fn health_check(&self) -> anyhow::Result<f64> {
        let start = Instant::now();

        let mut conn = self.connection.clone();
        let _: String = redis::cmd("PING")
            .query_async(&mut conn)
            .await
            .context("Redis health check failed")?;

        let elapsed = start.elapsed();
        Ok(elapsed.as_secs_f64() * 1000.0) // milliseconds
    }
}
