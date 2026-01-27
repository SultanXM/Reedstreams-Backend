use async_trait::async_trait;
use chrono::Utc;
use redis::AsyncCommands;

use crate::database::RedisDatabase;

use super::{Game, Stream, StreamsRepository};

#[async_trait]
impl StreamsRepository for RedisDatabase {
    // gets all streams from a provider
    async fn get_stream(&self, provider: &str) -> anyhow::Result<Option<Stream>> {
        let mut conn = self.connection.clone();

        let data: Option<String> = conn.get(provider).await?;

        Ok(data.map(|d| Stream {
            provider: provider.to_string(),
            data: d,
        }))
    }

    // get all streams no matter the provider (I only have one right now)
    //
    // This previously used .keys("*") lol
    // please never use .keys in redis, if your cache is too big it WILL explode
    async fn get_all_streams(&self) -> anyhow::Result<Vec<Stream>> {
        let mut conn = self.connection.clone();

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

        if keys.is_empty() {
            return Ok(Vec::new());
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

    // store a stream with provider and id
    async fn store_game(&self, provider: &str, game: &Game) -> anyhow::Result<()> {
        let mut conn = self.connection.clone();

        let key = format!("{}:{}", provider, game.id);
        let value = serde_json::to_string(game)?;

        let _: () = conn.set(&key, value).await?;

        Ok(())
    }
    // get a game with provider and id
    async fn get_game(&self, provider: &str, game_id: i64) -> anyhow::Result<Option<Game>> {
        let mut conn = self.connection.clone();

        let key = format!("{}:{}", provider, game_id);
        let data: Option<String> = conn.get(&key).await?;

        Ok(data.and_then(|json| serde_json::from_str::<Game>(&json).ok()))
    }

    // get all games from a matcher in format provider:*
    async fn get_games(&self, provider: &str) -> anyhow::Result<Vec<Game>> {
        let mut conn = self.connection.clone();

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

    // flush it from redis
    async fn delete_game(&self, provider: &str, game_id: i64) -> anyhow::Result<()> {
        let mut conn = self.connection.clone();

        let key = format!("{}:{}", provider, game_id);
        let _: () = conn.del(&key).await?;

        Ok(())
    }

    // used mainly for debugging, this isn't present in the live version but can be if needed
    async fn clear_cache(&self, provider: &str) -> anyhow::Result<()> {
        let mut conn = self.connection.clone();

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

    // last time the streams were fetched because this needs to update a good amount
    async fn set_last_fetch_time(&self, provider: &str, timestamp: i64) -> anyhow::Result<()> {
        let mut conn = self.connection.clone();

        let key = format!("{}:last_fetch", provider);
        let _: () = conn.set(&key, timestamp).await?;

        Ok(())
    }

    // get the above
    async fn get_last_fetch_time(&self, provider: &str) -> anyhow::Result<Option<i64>> {
        let mut conn = self.connection.clone();

        let key = format!("{}:last_fetch", provider);
        let timestamp: Option<i64> = conn.get(&key).await?;

        Ok(timestamp)
    }

    // get cached video link by stream_path
    async fn get_video_link(&self, stream_path: &str) -> anyhow::Result<Option<String>> {
        let mut conn = self.connection.clone();

        let key = format!("videolink:{}", stream_path);
        let link: Option<String> = conn.get(&key).await?;

        Ok(link)
    }

    // cache video link with TTL
    async fn set_video_link(
        &self,
        stream_path: &str,
        video_link: &str,
        ttl_secs: u64,
    ) -> anyhow::Result<()> {
        let mut conn = self.connection.clone();

        let key = format!("videolink:{}", stream_path);
        let _: () = conn.set_ex(&key, video_link, ttl_secs).await?;

        Ok(())
    }
}
