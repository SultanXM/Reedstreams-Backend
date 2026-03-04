use async_trait::async_trait;
use chrono::Utc;

use crate::database::Database;

use super::{Game, Stream, StreamsRepository};

#[async_trait]
impl StreamsRepository for Database {
    // gets all streams from a provider
    async fn get_stream(&self, provider: &str) -> anyhow::Result<Option<Stream>> {
        match self {
            #[allow(unused_imports)]
            Database::Redis(db) => {
                use redis::AsyncCommands;
                let mut conn = db.connection.clone();
                let data: Option<String> = conn.get(provider).await?;
                Ok(data.map(|d| Stream {
                    provider: provider.to_string(),
                    data: d,
                }))
            }
            Database::Memory(db) => {
                let data = db.store.get(provider).await?;
                Ok(data.map(|d| Stream {
                    provider: provider.to_string(),
                    data: d,
                }))
            }
        }
    }

    // get all streams no matter the provider
    async fn get_all_streams(&self) -> anyhow::Result<Vec<Stream>> {
        match self {
            #[allow(unused_imports)]
            Database::Redis(db) => {
                use redis::AsyncCommands;
                let mut conn = db.connection.clone();
                let current_time = Utc::now().timestamp();
                let twenty_four_hours = 24 * 60 * 60;
                let pattern = "*";

                let mut keys: Vec<String> = Vec::new();
                let mut cursor = 0u64;

                loop {
                    let (new_cursor, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                        .arg(cursor)
                        .arg("MATCH")
                        .arg(pattern)
                        .query_async(&mut conn)
                        .await?;

                    keys.extend(batch);
                    cursor = new_cursor;

                    if cursor == 0 {
                        break;
                    }
                }

                let mut streams = Vec::new();
                for key in keys {
                    if key.contains(':') {
                        let parts: Vec<&str> = key.split(':').collect();

                        if parts.len() != 2 {
                            continue;
                        }

                        let Ok(game_id) = parts[1].parse::<i64>() else {
                            continue;
                        };

                        let Some(game) = self.get_game(parts[0], game_id).await? else {
                            continue;
                        };

                        if current_time - game.start_time > twenty_four_hours {
                            self.delete_game(parts[0], game_id).await?;
                            continue;
                        }
                    }

                    if let Some(stream) = self.get_stream(&key).await? {
                        streams.push(stream);
                    }
                }

                Ok(streams)
            }
            Database::Memory(db) => {
                let current_time = Utc::now().timestamp();
                let twenty_four_hours = 24 * 60 * 60;
                let pattern = "*";

                let keys = db.store.scan(pattern).await?;
                let mut streams = Vec::new();

                for key in keys {
                    if key.contains(':') {
                        let parts: Vec<&str> = key.split(':').collect();

                        if parts.len() != 2 {
                            continue;
                        }

                        let Ok(game_id) = parts[1].parse::<i64>() else {
                            continue;
                        };

                        let Some(game) = self.get_game(parts[0], game_id).await? else {
                            continue;
                        };

                        if current_time - game.start_time > twenty_four_hours {
                            self.delete_game(parts[0], game_id).await?;
                            continue;
                        }
                    }

                    if let Some(stream) = self.get_stream(&key).await? {
                        streams.push(stream);
                    }
                }

                Ok(streams)
            }
        }
    }

    // store a game with provider and id
    async fn store_game(&self, provider: &str, game: &Game) -> anyhow::Result<()> {
        match self {
            #[allow(unused_imports)]
            Database::Redis(db) => {
                use redis::AsyncCommands;
                let mut conn = db.connection.clone();
                let key = format!("{}:{}", provider, game.id);
                let value = serde_json::to_string(game)?;
                let _: () = conn.set(&key, value).await?;
                Ok(())
            }
            Database::Memory(db) => {
                let key = format!("{}:{}", provider, game.id);
                let value = serde_json::to_string(game)?;
                db.store.set(&key, &value).await?;
                Ok(())
            }
        }
    }

    // get a game with provider and id
    async fn get_game(&self, provider: &str, game_id: i64) -> anyhow::Result<Option<Game>> {
        match self {
            #[allow(unused_imports)]
            Database::Redis(db) => {
                use redis::AsyncCommands;
                let mut conn = db.connection.clone();
                let key = format!("{}:{}", provider, game_id);
                let data: Option<String> = conn.get(&key).await?;
                Ok(data.and_then(|json| serde_json::from_str::<Game>(&json).ok()))
            }
            Database::Memory(db) => {
                let key = format!("{}:{}", provider, game_id);
                let data = db.store.get(&key).await?;
                Ok(data.and_then(|json| serde_json::from_str::<Game>(&json).ok()))
            }
        }
    }

    // get all games from a provider
    async fn get_games(&self, provider: &str) -> anyhow::Result<Vec<Game>> {
        match self {
            #[allow(unused_imports)]
            Database::Redis(db) => {
                use redis::AsyncCommands;
                let mut conn = db.connection.clone();
                let pattern = format!("{}:*", provider);
                let mut keys = Vec::new();
                let mut cursor = 0u64;

                loop {
                    let (new_cursor, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                        .arg(cursor)
                        .arg("MATCH")
                        .arg(&pattern)
                        .query_async(&mut conn)
                        .await?;

                    keys.extend(batch);
                    cursor = new_cursor;

                    if cursor == 0 {
                        break;
                    }
                }

                if keys.is_empty() {
                    return Ok(Vec::new());
                }

                let values: Vec<Option<String>> =
                    redis::cmd("MGET").arg(&keys).query_async(&mut conn).await?;

                let games = values
                    .into_iter()
                    .flatten()
                    .filter_map(|json| serde_json::from_str::<Game>(&json).ok())
                    .collect();

                Ok(games)
            }
            Database::Memory(db) => {
                let pattern = format!("{}:*", provider);
                let keys = db.store.scan(&pattern).await?;

                if keys.is_empty() {
                    return Ok(Vec::new());
                }

                let values = db.store.mget(&keys).await?;

                let games = values
                    .into_iter()
                    .flatten()
                    .filter_map(|json| serde_json::from_str::<Game>(&json).ok())
                    .collect();

                Ok(games)
            }
        }
    }

    // flush it from storage
    async fn delete_game(&self, provider: &str, game_id: i64) -> anyhow::Result<()> {
        match self {
            #[allow(unused_imports)]
            Database::Redis(db) => {
                use redis::AsyncCommands;
                let mut conn = db.connection.clone();
                let key = format!("{}:{}", provider, game_id);
                let _: () = conn.del(&key).await?;
                Ok(())
            }
            Database::Memory(db) => {
                let key = format!("{}:{}", provider, game_id);
                let _ = db.store.del(&key).await?;
                Ok(())
            }
        }
    }

    // used mainly for debugging
    async fn clear_cache(&self, provider: &str) -> anyhow::Result<()> {
        match self {
            #[allow(unused_imports)]
            Database::Redis(db) => {
                use redis::AsyncCommands;
                let mut conn = db.connection.clone();
                let pattern = format!("{}:*", provider);
                let mut keys = Vec::new();
                let mut cursor = 0u64;

                loop {
                    let (new_cursor, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                        .arg(cursor)
                        .arg("MATCH")
                        .arg(&pattern)
                        .arg("COUNT")
                        .arg(100)
                        .query_async(&mut conn)
                        .await?;

                    keys.extend(batch);
                    cursor = new_cursor;

                    if cursor == 0 {
                        break;
                    }
                }

                if !keys.is_empty() {
                    let _: () = conn.del(keys).await?;
                }

                Ok(())
            }
            Database::Memory(db) => {
                let pattern = format!("{}:*", provider);
                let keys = db.store.scan(&pattern).await?;

                if !keys.is_empty() {
                    let _ = db.store.del_multiple(&keys).await?;
                }

                Ok(())
            }
        }
    }

    // last time the streams were fetched
    async fn set_last_fetch_time(&self, provider: &str, timestamp: i64) -> anyhow::Result<()> {
        match self {
            #[allow(unused_imports)]
            Database::Redis(db) => {
                use redis::AsyncCommands;
                let mut conn = db.connection.clone();
                let key = format!("{}:last_fetch", provider);
                let _: () = conn.set(&key, timestamp).await?;
                Ok(())
            }
            Database::Memory(db) => {
                let key = format!("{}:last_fetch", provider);
                db.store.set(&key, &timestamp.to_string()).await?;
                Ok(())
            }
        }
    }

    // get the above
    async fn get_last_fetch_time(&self, provider: &str) -> anyhow::Result<Option<i64>> {
        match self {
            #[allow(unused_imports)]
            Database::Redis(db) => {
                use redis::AsyncCommands;
                let mut conn = db.connection.clone();
                let key = format!("{}:last_fetch", provider);
                let timestamp: Option<i64> = conn.get(&key).await?;
                Ok(timestamp)
            }
            Database::Memory(db) => {
                let key = format!("{}:last_fetch", provider);
                let timestamp = db.store.get(&key).await?;
                Ok(timestamp.and_then(|s| s.parse().ok()))
            }
        }
    }

    // get cached video link by stream_path
    async fn get_video_link(&self, stream_path: &str) -> anyhow::Result<Option<String>> {
        match self {
            #[allow(unused_imports)]
            Database::Redis(db) => {
                use redis::AsyncCommands;
                let mut conn = db.connection.clone();
                let key = format!("videolink:{}", stream_path);
                let link: Option<String> = conn.get(&key).await?;
                Ok(link)
            }
            Database::Memory(db) => {
                let key = format!("videolink:{}", stream_path);
                db.store.get(&key).await
            }
        }
    }

    // cache video link with TTL
    async fn set_video_link(
        &self,
        stream_path: &str,
        video_link: &str,
        ttl_secs: u64,
    ) -> anyhow::Result<()> {
        match self {
            #[allow(unused_imports)]
            Database::Redis(db) => {
                use redis::AsyncCommands;
                let mut conn = db.connection.clone();
                let key = format!("videolink:{}", stream_path);
                let _: () = conn.set_ex(&key, video_link, ttl_secs).await?;
                Ok(())
            }
            Database::Memory(db) => {
                let key = format!("videolink:{}", stream_path);
                db.store.set_ex(&key, video_link, ttl_secs).await
            }
        }
    }
}
