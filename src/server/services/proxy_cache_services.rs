use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use redis::AsyncCommands;
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

use crate::database::RedisDatabase;

const M3U8_TTL_SECONDS: u64 = 10;
const SEGMENT_TTL_SECONDS: u64 = 300;

pub type DynProxyCacheService = Arc<dyn ProxyCacheServiceTrait + Send + Sync>;

#[async_trait::async_trait]
pub trait ProxyCacheServiceTrait {
    /// Pipeline check Redis for both m3u8 and segment caches in one round trip.
    /// Returns (Option<m3u8_text>, Option<segment_bytes>).
    async fn get_cached(&self, url: &str) -> (Option<String>, Option<Vec<u8>>);

    /// Cache raw m3u8 text (before URL rewriting) with short TTL.
    async fn cache_m3u8(&self, url: &str, text: &str);

    /// Cache segment bytes with longer TTL.
    async fn cache_segment(&self, url: &str, bytes: &[u8]);

    /// Wait for an in-flight prefetch of the given URL.
    /// Returns `Some(bytes)` if the prefetch completes and the segment is in cache,
    /// or `None` if no prefetch is in-flight or the wait times out.
    async fn wait_for_inflight(&self, url: &str) -> Option<Vec<u8>>;

    /// Pre-fetch a list of segment URLs in the background, caching each in Redis.
    /// Skips URLs already cached. Caps concurrent upstream fetches at 5.
    async fn prefetch_segments(&self, urls: Vec<String>);
}

pub struct ProxyCacheService {
    redis: Arc<RedisDatabase>,
    http: reqwest::Client,
    inflight: Mutex<HashMap<String, Arc<Notify>>>,
}

impl ProxyCacheService {
    pub fn new(redis: Arc<RedisDatabase>) -> Self {
        Self {
            redis,
            http: reqwest::Client::new(),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    fn hash_url(url: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(url.as_bytes());
        hex::encode(hasher.finalize())
    }

    fn m3u8_key(url: &str) -> String {
        format!("pcache:m3u8:{}", Self::hash_url(url))
    }

    fn segment_key(url: &str) -> String {
        format!("pcache:seg:{}", Self::hash_url(url))
    }

    /// Fetch a single segment from upstream with sports-style headers, decompress, and cache it.
    async fn fetch_and_cache_segment(
        http: &reqwest::Client,
        redis: &Arc<RedisDatabase>,
        url: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let accept_encoding = "gzip, deflate, br, zstd";

        let mut request_builder = http.get(url);

        if url.contains("strm.poocloud.in") {
            request_builder = request_builder
                .header(reqwest::header::ORIGIN, "https://ppvs.su")
                .header(reqwest::header::ACCEPT, "*/*")
                .header(reqwest::header::ACCEPT_ENCODING, accept_encoding)
                .header(reqwest::header::REFERER, "https://modistreams.org/")
                .header(
                    reqwest::header::USER_AGENT,
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
                );
        } else {
            request_builder = request_builder
                .header(reqwest::header::REFERER, "https://api.ppvs.su/api/streams/")
                .header(reqwest::header::ORIGIN, "https://api.ppvs.su/api/streams")
                .header(
                    reqwest::header::USER_AGENT,
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
                )
                .header(reqwest::header::ACCEPT_ENCODING, accept_encoding)
                .header(reqwest::header::ACCEPT, "*/*");
        }

        let response = request_builder.send().await?;

        if !response.status().is_success() {
            return Err(format!("Upstream returned {}", response.status()).into());
        }

        let content_encoding = response
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let bytes = response.bytes().await?;

        let decompressed: Vec<u8> = match content_encoding.as_deref() {
            Some("zstd") => zstd::decode_all(&bytes[..])?,
            Some("gzip") => {
                use std::io::Read;
                let mut decoder = flate2::read::GzDecoder::new(&bytes[..]);
                let mut decomp = Vec::new();
                decoder.read_to_end(&mut decomp)?;
                decomp
            }
            _ => bytes.to_vec(),
        };

        // Cache in Redis
        let key = Self::segment_key(url);
        let mut conn = redis.connection.clone();
        let _: Result<(), redis::RedisError> = conn
            .set_ex(&key, &decompressed[..], SEGMENT_TTL_SECONDS)
            .await;

        debug!(
            "Prefetched and cached segment ({} bytes): {}",
            decompressed.len(),
            url
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl ProxyCacheServiceTrait for ProxyCacheService {
    async fn get_cached(&self, url: &str) -> (Option<String>, Option<Vec<u8>>) {
        let m3u8_key = Self::m3u8_key(url);
        let seg_key = Self::segment_key(url);
        let mut conn = self.redis.connection.clone();

        // Pipeline both GETs into a single round trip
        let result: Result<(Option<String>, Option<Vec<u8>>), redis::RedisError> = redis::pipe()
            .get(&m3u8_key)
            .get(&seg_key)
            .query_async(&mut conn)
            .await;

        match result {
            Ok((m3u8, seg)) => {
                if m3u8.is_some() {
                    debug!("Proxy cache HIT (m3u8) for {}", url);
                }
                if seg.is_some() {
                    debug!("Proxy cache HIT (segment) for {}", url);
                }
                (m3u8, seg)
            }
            Err(e) => {
                error!("Proxy cache GET failed: {}", e);
                (None, None)
            }
        }
    }

    async fn cache_m3u8(&self, url: &str, text: &str) {
        let key = Self::m3u8_key(url);
        let mut conn = self.redis.connection.clone();

        let result: Result<(), redis::RedisError> = conn.set_ex(&key, text, M3U8_TTL_SECONDS).await;

        match result {
            Ok(_) => debug!(
                "Cached m3u8 ({} bytes, TTL {}s)",
                text.len(),
                M3U8_TTL_SECONDS
            ),
            Err(e) => error!("Failed to cache m3u8: {}", e),
        }
    }

    async fn cache_segment(&self, url: &str, bytes: &[u8]) {
        let key = Self::segment_key(url);
        let mut conn = self.redis.connection.clone();

        let result: Result<(), redis::RedisError> =
            conn.set_ex(&key, bytes, SEGMENT_TTL_SECONDS).await;

        match result {
            Ok(_) => debug!(
                "Cached segment ({} bytes, TTL {}s)",
                bytes.len(),
                SEGMENT_TTL_SECONDS
            ),
            Err(e) => error!("Failed to cache segment: {}", e),
        }
    }

    async fn wait_for_inflight(&self, url: &str) -> Option<Vec<u8>> {
        let notify = {
            let lock = self.inflight.lock().unwrap();
            lock.get(url).cloned()
        };

        let notify = notify?;

        debug!("Waiting for inflight prefetch: {}", url);

        let wait_result =
            tokio::time::timeout(std::time::Duration::from_secs(3), notify.notified()).await;

        if wait_result.is_err() {
            warn!("Timed out waiting for inflight prefetch: {}", url);
            return None;
        }

        // Prefetch completed, check Redis for the cached segment
        let seg_key = Self::segment_key(url);
        let mut conn = self.redis.connection.clone();
        let result: Result<Option<Vec<u8>>, redis::RedisError> = conn.get(&seg_key).await;

        match result {
            Ok(Some(bytes)) => {
                debug!(
                    "Got segment from cache after inflight wait ({} bytes): {}",
                    bytes.len(),
                    url
                );
                Some(bytes)
            }
            Ok(None) => {
                warn!(
                    "Inflight prefetch completed but segment not in cache: {}",
                    url
                );
                None
            }
            Err(e) => {
                error!("Redis GET failed after inflight wait: {}", e);
                None
            }
        }
    }

    async fn prefetch_segments(&self, urls: Vec<String>) {
        if urls.is_empty() {
            return;
        }

        // Pipeline EXISTS checks for all segment keys in one round trip
        let mut conn = self.redis.connection.clone();
        let mut pipe = redis::pipe();
        for url in &urls {
            pipe.exists(Self::segment_key(url));
        }

        let exists_results: Vec<bool> = match pipe.query_async(&mut conn).await {
            Ok(results) => results,
            Err(e) => {
                error!("Prefetch EXISTS pipeline failed: {}", e);
                return;
            }
        };

        let uncached: Vec<String> = urls
            .into_iter()
            .zip(exists_results.into_iter())
            .filter(|(_, exists)| !exists)
            .map(|(url, _)| url)
            .collect();

        if uncached.is_empty() {
            debug!("All segments already cached, skipping prefetch");
            return;
        }

        info!("Prefetching {} segments", uncached.len());

        // Register inflight notifiers for each uncached URL
        {
            let mut lock = self.inflight.lock().unwrap();
            for url in &uncached {
                lock.entry(url.clone())
                    .or_insert_with(|| Arc::new(Notify::new()));
            }
        }

        let semaphore = Arc::new(Semaphore::new(5));
        let mut join_set = JoinSet::new();

        // Spawn a task for each fetch — all go in-flight immediately,
        // semaphore gates the actual upstream requests to 5 concurrent
        for url in uncached {
            let http = self.http.clone();
            let redis = self.redis.clone();
            let sem = semaphore.clone();
            join_set.spawn(async move {
                let _permit = sem.acquire().await.expect("semaphore closed");
                let result = Self::fetch_and_cache_segment(&http, &redis, &url).await;
                (url, result)
            });
        }

        // Pop completed results as they land and handle inflight notifications
        while let Some(completed) = join_set.join_next().await {
            match completed {
                Ok((url, result)) => {
                    let notify = {
                        let mut lock = self.inflight.lock().unwrap();
                        lock.remove(&url)
                    };
                    if let Some(notify) = notify {
                        notify.notify_waiters();
                    }
                    if let Err(e) = result {
                        error!("Prefetch failed for {}: {}", url, e);
                    }
                }
                Err(e) => error!("Prefetch task panicked: {}", e),
            }
        }
    }
}
