use std::sync::Arc;

use tracing::info;

use crate::{
    config::AppConfig,
    database::RedisDatabase,
    server::{
        services::{
            cookie_services::CookieService, ppvsu_services::PpvsuService,
            stream_services::StreamsService,
        },
        utils::signature_utils::SignatureUtil,
    },
};

use super::{
    cookie_services::DynCookieService, ppvsu_services::DynPpvsuService,
    proxy_cache_services::DynProxyCacheService, rate_limit_services::DynRateLimitService,
    stream_services::DynStreamsService,
};

/// edge services without database dependencies
/// only uses Redis (or valkey goated) for caching and rate limiting
#[derive(Clone)]
pub struct EdgeServices {
    pub signature_util: Arc<SignatureUtil>,
    pub streams: DynStreamsService,
    pub ppvsu: DynPpvsuService,
    pub rate_limit: DynRateLimitService,
    pub cookies: DynCookieService,
    pub proxy_cache: DynProxyCacheService,
    pub http: reqwest::Client,
    pub redis: Arc<RedisDatabase>,
    pub config: Arc<AppConfig>,
}

impl EdgeServices {
    pub fn new(redis_db: RedisDatabase, config: Arc<AppConfig>) -> Self {
        info!("starting edge services (no database)...");

        let signature_util = Arc::new(SignatureUtil::new(config.access_token_secret.clone()));

        info!("signature util ok, starting remaining services...");
        let redis_repository = Arc::new(redis_db);
        
        // High-performance HTTP client for 1000+ concurrent connections
        // Tuned for video streaming with connection pooling and keep-alive
        let http = reqwest::Client::builder()
            // Pool size: enough for 1000+ concurrent upstream connections
            .pool_max_idle_per_host(200)
            // Connection timeout for establishing new connections
            .connect_timeout(std::time::Duration::from_secs(10))
            // Overall request timeout - must be longer than health checks
            .timeout(std::time::Duration::from_secs(60))
            // Idle connections live longer for streaming workloads
            .pool_idle_timeout(std::time::Duration::from_secs(120))
            // TCP keep-alive to prevent connection drops
            .tcp_keepalive(std::time::Duration::from_secs(60))
            .build()
            .expect("Failed to build HTTP client");

        let ppvsu = Arc::new(PpvsuService::new(redis_repository.clone())) as DynPpvsuService;
        let streams = Arc::new(StreamsService::new(redis_repository.clone(), ppvsu.clone()))
            as DynStreamsService;

        let rate_limit = Arc::new(super::rate_limit_services::EdgeRateLimitService::new(
            redis_repository.clone(),
        )) as DynRateLimitService;

        let cookies = Arc::new(CookieService::new(redis_repository.clone())) as DynCookieService;

        // Passed http.clone() here to satisfy the 2-argument requirement
        let proxy_cache = Arc::new(super::proxy_cache_services::ProxyCacheService::new(
            redis_repository.clone(),
            http.clone(),
        )) as DynProxyCacheService;

        Self {
            signature_util,
            streams,
            ppvsu,
            rate_limit,
            cookies,
            proxy_cache,
            http,
            redis: redis_repository,
            config,
        }
    }
}
