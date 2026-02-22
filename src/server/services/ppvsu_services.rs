// all the stream related functions, im not commenting on all of them, they're pretty readable
use async_trait::async_trait;
use base64::Engine;
use chacha20::ChaCha20;
use chacha20::cipher::{KeyIvInit, StreamCipher};
use flate2::read::GzDecoder;
use mockall::automock;
use std::io::Read;
use std::sync::Arc;
use tracing::{error, info};

use crate::{
    database::{
        RedisDatabase,
        stream::{DynStreamsRepository, Game, PpvsuApiResponse, PpvsuStreamDetailResponse},
    },
    server::error::{AppResult, Error},
};

pub type DynPpvsuService = Arc<dyn PpvsuServiceTrait + Send + Sync>;

/// ROT-71 cipher - rotates ASCII characters by 71 positions
/// This transforms the custom charset to valid standard base64
/// Range: 33 ('!') to 126 ('~') = 94 characters
fn rot71_decode(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            let code = c as u32;
            if (33..=126).contains(&code) {
                char::from_u32(33 + ((code - 33) + 71) % 94).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

fn encode_variant(mut n: usize, out: &mut Vec<u8>) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

/// Parse protobuf message with 2 length-delimited fields
/// 1 (0x0a): Custom charset encoded ciphertext (requires ROT-71 → base64 → ChaCha20)
/// 2 (0x12): stream name
fn parse_protobuf(buffer: &[u8]) -> AppResult<(String, Option<String>)> {
    let mut offset = 0;
    let mut field1: Option<String> = None;
    let mut field2: Option<String> = None;

    while offset < buffer.len() {
        let tag = buffer[offset];
        offset += 1;

        let mut length: usize = 0;
        let mut shift = 0;
        loop {
            if offset >= buffer.len() {
                break;
            }
            let byte = buffer[offset];
            offset += 1;
            length |= ((byte & 0x7F) as usize) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
        }

        if offset + length > buffer.len() {
            break;
        }

        let field_data = &buffer[offset..offset + length];
        offset += length;

        match tag {
            0x0a => {
                field1 = Some(String::from_utf8_lossy(field_data).to_string());
            }
            0x12 => {
                field2 = Some(String::from_utf8_lossy(field_data).to_string());
            }
            _ => {}
        }
    }

    field1.map(|f1| (f1, field2)).ok_or_else(|| {
        Error::InternalServerErrorWithContext("failed to extract field1 from protobuf".to_string())
    })
}

/// ChaCha20 decryption with counter=1
/// Key: full `island` header (32 bytes UTF-8)
/// Nonce: first 12 bytes of decoded ciphertext
/// Counter starts at 1, not 0 (critical for correct decryption)
fn chacha20_decrypt(decoded_data: &[u8], key: &str) -> AppResult<String> {
    use chacha20::cipher::StreamCipherSeek;

    if decoded_data.len() < 12 {
        return Err(Error::InternalServerErrorWithContext(
            "decoded data too short to contain nonce".to_string(),
        ));
    }

    let key_bytes = key.as_bytes();
    if key_bytes.len() != 32 {
        return Err(Error::InternalServerErrorWithContext(format!(
            "key must be 32 bytes, got {}",
            key_bytes.len()
        )));
    }

    // First 12 bytes are the nonce, rest is ciphertext
    let nonce = &decoded_data[..12];
    let ciphertext = &decoded_data[12..];

    // Create cipher with 32-byte key and 12-byte nonce
    let mut cipher = ChaCha20::new(key_bytes.into(), nonce.into());

    // Seek to block 1 (64 bytes) - counter starts at 1, not 0
    cipher.seek(64u64);

    let mut buffer = ciphertext.to_vec();
    cipher.apply_keystream(&mut buffer);

    // Extract URL (ends with .m3u8, may have trailing garbage)
    let plaintext = String::from_utf8_lossy(&buffer);
    if let Some(end_idx) = plaintext.find(".m3u8") {
        Ok(plaintext[..end_idx + 5].to_string())
    } else {
        // Fallback: return up to first null byte or non-printable char
        let clean: String = plaintext
            .chars()
            .take_while(|c| c.is_ascii() && !c.is_ascii_control())
            .collect();
        Ok(clean)
    }
}

/// New decryption pipeline (2024 update)
/// Parse protobuf → field1 (custom charset encoded)
/// ROT-71 decode field1 → standard base64
/// Base64 decode → [nonce (12 bytes) || ciphertext]
/// ChaCha20 decrypt with island header as key, counter=1
fn decrypt_stream_url(encrypted_blob: &[u8], island_header: &str) -> AppResult<String> {
    // Step 1: Parse protobuf to extract field1 (encoded ciphertext)
    let (encoded_ciphertext, _stream_name) = parse_protobuf(encrypted_blob)?;

    // Step 2: ROT-71 transform to get valid standard base64
    let base64_ciphertext = rot71_decode(&encoded_ciphertext);

    // Step 3: Base64 decode to get binary [nonce || ciphertext]
    let decoded_data = base64::engine::general_purpose::STANDARD
        .decode(&base64_ciphertext)
        .map_err(|e| {
            Error::InternalServerErrorWithContext(format!(
                "failed to base64 decode after ROT-71: {}",
                e
            ))
        })?;

    // Step 4: ChaCha20 decrypt (nonce is first 12 bytes, counter=1)
    let decrypted_url = chacha20_decrypt(&decoded_data, island_header)?;

    Ok(decrypted_url)
}

#[automock]
#[async_trait]
pub trait PpvsuServiceTrait {
    async fn fetch_and_cache_games(&self) -> AppResult<Vec<Game>>;
    async fn fetch_video_link(&self, iframe_url: &str) -> AppResult<String>;
    async fn get_games_with_refresh(&self) -> AppResult<Vec<Game>>;
    async fn get_game_by_id(&self, game_id: i64) -> AppResult<Game>;
    async fn clear_cache(&self) -> AppResult<()>;
    async fn get_current_timestamp(&self) -> AppResult<i64>;
    async fn is_cache_stale(&self, cache_time: i64, current_time: i64) -> bool;
}

#[derive(Clone)]
pub struct PpvsuService {
    repository: DynStreamsRepository,
    http_client: reqwest::Client,
}

impl PpvsuService {
    pub fn new(redis: Arc<RedisDatabase>) -> Self {
        // i like to make it look like a real browser but it's really not needed
        let http_client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:144.0) Gecko/20100101 Firefox/144.0")
            .timeout(std::time::Duration::from_secs(30))
            .http2_adaptive_window(true)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            repository: redis,
            http_client,
        }
    }

    async fn refetch_game(&self, game_id: i64) -> AppResult<Game> {
        info!("refetching game {} from ppvs.su API", game_id);

        let response = self
            .http_client
            .get(format!("https://api.ppv.to/api/streams/{}", game_id))
            .header("Accept", "application/json, text/plain, */*")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Referer", "https://api.ppv.to/api/streams/")
            .header("Origin", "https://api.ppv.to/api/streams")
            .header("Sec-Fetch-Dest", "empty")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Site", "same-origin")
            .send()
            .await
            .map_err(|e| {
                error!("failed to fetch game {}: {}", game_id, e);
                Error::InternalServerErrorWithContext(format!("failed to fetch game: {}", e))
            })?;

        let detail_response: PpvsuStreamDetailResponse = response.json().await.map_err(|e| {
            error!("failed to parse game response: {}", e);
            Error::InternalServerErrorWithContext(format!("failed to parse game response: {}", e))
        })?;

        if !detail_response.success {
            return Err(Error::InternalServerErrorWithContext(
                "ppvs.su API returned success=false".to_string(),
            ));
        }

        let data = detail_response.data;

        let iframe = data
            .sources
            .first()
            .map(|s| s.data.clone())
            .ok_or_else(|| Error::NotFound("no sources found for stream".to_string()))?;

        // previous logic of storing the games that were already at the pure link, instead i need
        // to return the iframe and decode it later so i don't get ip banned :(
        //
        // let video_link = self.fetch_video_link(&iframe).await?;

        let cache_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| {
                Error::InternalServerErrorWithContext("System time before UNIX epoch".to_string())
            })?
            .as_secs() as i64;

        let game = Game {
            id: data.id,
            name: data.name,
            poster: data.poster,
            start_time: data.start_timestamp,
            end_time: data.end_timestamp,
            cache_time,
            video_link: iframe,
            category: data.category_name.unwrap_or_else(|| "Unknown".to_string()),
        };

        self.repository.store_game("ppvsu", &game).await?;

        Ok(game)
    }
}

const VIDEO_LINK_CACHE_TTL_SECS: u64 = 300;

#[async_trait]
impl PpvsuServiceTrait for PpvsuService {
    async fn fetch_video_link(&self, iframe_url: &str) -> AppResult<String> {
        info!("fetching video link from iframe: {}", iframe_url);

        let url = reqwest::Url::parse(iframe_url).map_err(|e| {
            error!("failed to parse iframe URL: {}", e);
            Error::BadRequest(format!("failed to parse iframe URL: {}", e))
        })?;

        let base_url = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));

        // extract the path after /embed/ (e.g., "nfl/2026-01-17/buf-den")
        let path = url.path();
        let stream_path = path.strip_prefix("/embed/").ok_or_else(|| {
            error!("iframe URL doesn't contain /embed/ path");
            Error::BadRequest("iframe URL doesn't contain /embed/ path".to_string())
        })?;

        // check cache first using stream_path as key
        if let Ok(Some(cached_link)) = self.repository.get_video_link(stream_path).await {
            info!("cache hit for video link: {}", stream_path);
            return Ok(cached_link);
        }

        info!(
            "cache miss, posting to {}/fetch with path: {}",
            base_url, stream_path
        );

        // this should be a function to be honest but we have to encode the varint because of
        // protobuf req
        let mut protobuf_header: Vec<u8> = Vec::new();
        protobuf_header.push(0x0A);
        let path_bytes = stream_path.as_bytes();
        encode_variant(path_bytes.len(), &mut protobuf_header);
        protobuf_header.extend_from_slice(path_bytes);

        // POST to /fetch endpoint to get the encrypted blob
        let response = self
            .http_client
            .post(format!("{}/fetch", base_url))
            .header("Accept", "*/*")
            .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:148.0) Gecko/20100101 Firefox/148.0")
            .header("Accept-Encoding", "gzip, deflate, br, zstd")
            .header("Content-Type", "application/octet-stream")
            .header("TE", "trailers")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Origin", &base_url)
            .header("Referer", iframe_url)
            .body(protobuf_header)
            .send()
            .await
            .map_err(|e| {
                error!("fetch endpoint request failed: {}", e);
                Error::InternalServerErrorWithContext(format!("fetch endpoint request failed: {}", e))
            })?;

        if !response.status().is_success() {
            error!("fetch endpoint returned status: {}", response.status());
            return Err(Error::InternalServerErrorWithContext(format!(
                "fetch endpoint returned status: {}",
                response.status()
            )));
        }

        let island_header = response
            .headers()
            .get("island")
            .and_then(|h| h.to_str().ok())
            .ok_or_else(|| {
                error!("missing 'island' header in response");
                Error::InternalServerErrorWithContext(
                    "missing 'island' header in response".to_string(),
                )
            })?
            .to_string();

        info!("received 'island' header ({} chars)", island_header.len());

        let encrypted_blob = response.bytes().await.map_err(|e| {
            error!("failed to read response bytes: {}", e);
            Error::InternalServerErrorWithContext(format!("failed to read response bytes: {}", e))
        })?;
        info!("received encrypted blob ({} chars)", encrypted_blob.len());

        // Protobuf parse → ROT-71 decode → Base64 decode → ChaCha20 decrypt
        let video_link = decrypt_stream_url(&encrypted_blob, &island_header)?;
        info!("decrypted video link: {}", video_link);

        // Cache the decrypted video link
        if let Err(e) = self
            .repository
            .set_video_link(stream_path, &video_link, VIDEO_LINK_CACHE_TTL_SECS)
            .await
        {
            error!("failed to cache video link: {}", e);
            // Don't fail the request, just log the error
        }

        Ok(video_link)
    }
    async fn fetch_and_cache_games(&self) -> AppResult<Vec<Game>> {
        // this is to maybe avoid the 403s that happen when cloudflare bans the ip
        //
        // i don't actually think this does anything because i think i'm hitting a rate limit but
        // this makes it look more legitimate anyways so whatever
        //
        // also just going to drop the future here because there is no point for me to actually
        // check it
        let _ = self.http_client.get("https://api.ppv.to/api/ping")
            .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:146.0) Gecko/20100101 Firefox/146.0")
            .header("Accept", "application/json")
            .header("Accept-Language", "en-US,en;q=0.5")
            .header("Accept-Encoding", "gzip, deflate, br, zstd")
            .header("Referer", "https://ppv.to/")
            .header("Origin", "https://ppv.to")
            .header("Sec-GPC", "1")
            .send();
        let response = self
            .http_client
            .get("https://api.ppv.to/api/streams")
            .header("Accept", "application/json, text/plain, */*")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Accept-Encoding", "gzip, deflate, br")
            .header("Referer", "https://api.ppv.to/api/streams/")
            .header("Origin", "https://api.ppv.to/api/streams")
            .header("DNT", "1")
            .header("Sec-Fetch-Dest", "empty")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Site", "same-origin")
            .send()
            .await
            .map_err(|e| {
                error!("failed to fetch ppvs.su API: {}", e);
                Error::InternalServerErrorWithContext(format!("failed to fetch ppvs.su API: {}", e))
            })?;

        info!(
            "received response from ppvs.su with status: {}",
            response.status()
        );

        let response_bytes = response.bytes().await.map_err(|e| {
            error!("failed to read response body: {}", e);
            Error::InternalServerErrorWithContext(format!(
                "failed to read ppvs.su API response body: {}",
                e
            ))
        })?;

        let decoded_text =
            if response_bytes.len() > 2 && response_bytes[0] == 0x1f && response_bytes[1] == 0x8b {
                let mut decoder = GzDecoder::new(&response_bytes[..]);
                let mut decompressed = String::new();
                decoder.read_to_string(&mut decompressed).map_err(|e| {
                    error!("failed to decompress gzip response: {}", e);
                    Error::InternalServerErrorWithContext(format!(
                        "failed to decompress gzip response: {}",
                        e
                    ))
                })?;
                decompressed
            } else {
                String::from_utf8(response_bytes.to_vec()).map_err(|e| {
                    error!("failed to convert response to UTF-8: {}", e);
                    Error::InternalServerErrorWithContext(format!(
                        "failed to convert response to UTF-8: {}",
                        e
                    ))
                })?
            };

        let api_response: PpvsuApiResponse = serde_json::from_str(&decoded_text).map_err(|e| {
            error!("failed to parse JSON response: {}", e);
            Error::InternalServerErrorWithContext(format!(
                "failed to parse ppvs.su API response: {}",
                e
            ))
        })?;

        if !api_response.success {
            return Err(Error::InternalServerErrorWithContext(
                "ppvs.su API returned success=false".to_string(),
            ));
        }

        let cache_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| {
                Error::InternalServerErrorWithContext("System time before UNIX epoch".to_string())
            })?
            .as_secs() as i64;

        let mut games: Vec<Game> = Vec::new();
        let mut game_mem: Game;
        for category in api_response.streams {
            for stream in category.streams {
                if let Some(iframe) = stream.iframe.clone() {
                    game_mem = Game {
                        id: stream.id,
                        name: stream.name,
                        poster: stream.poster,
                        start_time: stream.starts_at,
                        end_time: stream.ends_at,
                        cache_time,
                        video_link: iframe.clone(),
                        category: category.category.clone(),
                    };
                    games.push(game_mem.clone());

                    self.repository.store_game("ppvsu", &game_mem).await?;
                }
            }
        }
        // this logic works fine if i want eagerly evaluate all the adless video links before
        // storing but this gets me ip banned which i don't really want so i'll decode it on fetch
        // instead
        // let mut fetch_tasks = Vec::new();

        // // fun part of making a million threads and praying they all work
        // for category in api_response.streams {
        //     for stream in category.streams {
        //         if let Some(iframe) = stream.iframe {
        //             info!("queueing stream: {} (id: {})", stream.name, stream.id);

        //             let service_clone = self.clone();
        //             let iframe_clone = iframe.clone();
        //             let stream_id = stream.id;
        //             let stream_name = stream.name.clone();
        //             let stream_poster = stream.poster.clone();
        //             let stream_starts_at = stream.starts_at;
        //             let stream_ends_at = stream.ends_at;
        //             let stream_category = category.category.clone();

        //             let task = tokio::spawn(async move {
        //                 match service_clone.fetch_video_link(&iframe_clone).await {
        //                     Ok(video_link) => {
        //                         info!(
        //                             "successfully fetched video link for stream: {}",
        //                             stream_name
        //                         );
        //                         let game = Game {
        //                             id: stream_id,
        //                             name: stream_name,
        //                             poster: stream_poster,
        //                             start_time: stream_starts_at,
        //                             end_time: stream_ends_at,
        //                             cache_time,
        //                             video_link,
        //                             category: stream_category,
        //                         };

        //                         // store immediately after fetch completes
        //                         if let Err(e) =
        //                             service_clone.repository.store_game("ppvsu", &game).await
        //                         {
        //                             error!("failed to store game {}: {}", game.id, e);
        //                             None
        //                         } else {
        //                             Some(game)
        //                         }
        //                     }
        //                     Err(e) => {
        //                         error!(
        //                             "failed to fetch video link for stream {}: {}",
        //                             stream_id, e
        //                         );
        //                         None
        //                     }
        //                 }
        //             });

        //             fetch_tasks.push(task);
        //         }
        //     }
        // }

        // info!("fetching video links for {} streams", fetch_tasks.len());

        // let results = futures::future::join_all(fetch_tasks).await;

        // let mut games = Vec::new();
        // for result in results {
        //     match result {
        //         Ok(Some(game)) => {
        //             games.push(game);
        //         }
        //         Ok(None) => {}
        //         Err(e) => {
        //             error!("task panicked: {}", e);
        //         }
        //     }
        // }

        info!("cached {} games from ppvs.su", games.len());
        Ok(games)
    }

    async fn get_games_with_refresh(&self) -> AppResult<Vec<Game>> {
        info!("retrieving games with refresh logic");

        let cache_time = self.repository.get_last_fetch_time("ppvsu").await?;
        let current_time = self.get_current_timestamp().await?;

        match cache_time {
            Some(last_fetch) if !self.is_cache_stale(last_fetch, current_time).await => {
                let cache_age = current_time - last_fetch;
                info!(
                    "overall cache is fresh (last fetch {} seconds ago)",
                    cache_age
                );
                self.repository.get_games("ppvsu").await.map_err(|e| {
                    error!("failed to get games from cache: {}", e);
                    Error::InternalServerErrorWithContext(format!(
                        "failed to get games from cache: {}",
                        e
                    ))
                })
            }
            _ => {
                if let Some(last_fetch) = cache_time {
                    let cache_age = current_time - last_fetch;
                    info!(
                        "overall cache is stale (last fetch {} seconds ago), refetching all games",
                        cache_age
                    );
                } else {
                    info!("no cache found, fetching all games");
                }

                self.repository.clear_cache("ppvsu").await?;
                let games = self.fetch_and_cache_games().await?;
                self.repository
                    .set_last_fetch_time("ppvsu", current_time)
                    .await?;
                Ok(games)
            }
        }

        // let one_hour = 3600;
        // let games = self.repository.get_games("ppvsu").await?;

        // let mut refresh_tasks = Vec::new();
        // let mut fresh_games = Vec::new();

        // for game in games {
        //     let cache_age = current_time - game.cache_time;

        //     if cache_age > one_hour {
        //         info!(
        //             "game {} is stale (cached {} seconds ago), queueing for refetch",
        //             game.id, cache_age
        //         );

        //         let service_clone = self.clone();
        //         let game_id = game.id;
        //         let old_game = game.clone();

        //         let task = tokio::spawn(async move {
        //             match service_clone.refetch_game(game_id).await {
        //                 Ok(new_game) => (Some(new_game), None, None),
        //                 Err(e) => {
        //                     error!("failed to refetch game {}: {}", game_id, e);

        //                     if e.to_string().contains("404") || e.to_string().contains("not found")
        //                     {
        //                         info!("game {} no longer exists, marking for deletion", game_id);
        //                         (None, Some(game_id), None)
        //                     } else {
        //                         info!("keeping old version of game {}", game_id);
        //                         (None, None, Some(old_game))
        //                     }
        //                 }
        //             }
        //         });

        //         refresh_tasks.push(task);
        //     } else {
        //         fresh_games.push(game);
        //     }
        // }

        // info!(
        //     "refetching {} stale games concurrently",
        //     refresh_tasks.len()
        // );

        // let results = futures::future::join_all(refresh_tasks).await;

        // let mut refreshed_games = fresh_games;
        // for result in results {
        //     match result {
        //         Ok((Some(new_game), _, _)) => {
        //             refreshed_games.push(new_game);
        //         }
        //         Ok((None, Some(game_id_to_delete), None)) => {
        //             if let Err(del_err) = self
        //                 .repository
        //                 .delete_game("ppvsu", game_id_to_delete)
        //                 .await
        //             {
        //                 error!("failed to delete game {}: {}", game_id_to_delete, del_err);
        //             }
        //         }
        //         Ok((None, None, Some(old_game))) => {
        //             refreshed_games.push(old_game);
        //         }
        //         Ok(_) => {}
        //         Err(e) => {
        //             error!("refresh task panicked: {}", e);
        //         }
        //     }
        // }

        // Ok(refreshed_games)
    }

    async fn get_game_by_id(&self, game_id: i64) -> AppResult<Game> {
        info!("fetching game {} from cache or API", game_id);

        if let Some(cached_game) = self.repository.get_game("ppvsu", game_id).await? {
            let current_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| {
                    Error::InternalServerErrorWithContext(
                        "System time before UNIX epoch".to_string(),
                    )
                })?
                .as_secs() as i64;

            let cache_age = current_time - cached_game.cache_time;
            let one_hour = 3600;

            if cache_age <= one_hour {
                info!(
                    "returning cached game {} (age: {} seconds)",
                    game_id, cache_age
                );
                return Ok(cached_game);
            }

            info!(
                "cached game {} is stale (age: {} seconds), refetching",
                game_id, cache_age
            );
        } else {
            info!("game {} not in cache, fetching from API", game_id);
        }

        let game = self
            .refetch_game(game_id)
            .await
            .map_err(|e| Error::NotFound(format!("game {} not found: {}", game_id, e)))?;

        Ok(game)
    }

    async fn clear_cache(&self) -> AppResult<()> {
        info!("clearing ppvsu cache");

        self.repository.clear_cache("ppvsu").await.map_err(|e| {
            error!("failed to clear ppvsu cache: {}", e);
            Error::InternalServerErrorWithContext(format!("failed to clear cache: {}", e))
        })?;

        info!("ppvsu cache cleared successfully");
        Ok(())
    }

    async fn get_current_timestamp(&self) -> AppResult<i64> {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .map_err(|_| {
                Error::InternalServerErrorWithContext("System time before UNIX epoch".to_string())
            })
    }

    async fn is_cache_stale(&self, cache_time: i64, current_time: i64) -> bool {
        const ONE_HOUR: i64 = 3600;
        current_time - cache_time > ONE_HOUR
    }
}
