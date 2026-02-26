// Edge server module - no database, only Redis
mod api;
pub mod dtos;
pub mod error;
pub mod extractors;
pub mod services;
pub mod utils;

use std::future::ready;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::Extension;
use axum::extract::MatchedPath;
use axum::http::header::{self, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use axum::http::method;
use axum::http::request::Parts as RequestParts;
use axum::http::{HeaderValue, Request};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{BoxError, Json, Router, error_handling::HandleErrorLayer, http::StatusCode};
use lazy_static::lazy_static;
use method::Method;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
use serde_json::json;
use tower::{ServiceBuilder, buffer::BufferLayer, limit::RateLimitLayer};
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    trace::TraceLayer,
};
use tracing::{debug, info};

use crate::config::AppConfig;
use crate::database::RedisDatabase;
use crate::server::services::edge_services::EdgeServices;

lazy_static! {
    // 60 second timeout for video streaming (large segments)
    // Must be longer than Fly.io health check timeout (10s)
    static ref HTTP_TIMEOUT: u64 = 60;
    static ref EXPONENTIAL_SECONDS: &'static [f64] = &[
        0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0,
    ];
}

static APP_START_TIME: OnceLock<Instant> = OnceLock::new();

pub fn get_uptime_seconds() -> u64 {
    APP_START_TIME
        .get()
        .map(|start| start.elapsed().as_secs())
        .unwrap_or(0)
}

pub fn get_app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

macro_rules! cors_builder {
    (
        origins: $origins:expr,
        preview_origins: $preview_origins:expr,
        methods: $methods:expr,
        headers: $headers:expr
    ) => {{
        let origins: Vec<String> = $origins;
        let preview_origins: Vec<String> = $preview_origins;

        CorsLayer::new()
            .allow_origin(AllowOrigin::predicate(
                move |origin: &HeaderValue, _request_parts: &RequestParts| {
                    let origin_str = origin.to_str().unwrap_or("");
                    if origins.iter().any(|s| s == "*") {
                        return true;
                    }
                    if let Some(host) = origin_str
                        .strip_prefix("https://")
                        .or_else(|| origin_str.strip_prefix("http://"))
                    {
                        if origins.iter().any(|s| host.ends_with(s.as_str())) {
                            return true;
                        }
                        if preview_origins.iter().any(|s| host.ends_with(s.as_str())) {
                            return true;
                        }
                    }
                    if origins.iter().any(|s| s.ends_with(origin_str))
                        || preview_origins.iter().any(|s| s.ends_with(origin_str))
                    {
                        return true;
                    }
                    false
                },
            ))
            .allow_methods($methods)
            .allow_headers($headers)
            .allow_credentials(true)
    }};
}

pub struct EdgeApplicationServer;

impl EdgeApplicationServer {
    pub async fn serve(config: Arc<AppConfig>, redis_db: RedisDatabase) -> anyhow::Result<()> {
        // initialize app start time for uptime tracking
        let _ = APP_START_TIME.set(Instant::now());

        // do this however you like, i use the prometheus exporter because grafana is nice
        let recorder_handle = PrometheusBuilder::new()
            .set_buckets_for_metric(
                Matcher::Full(String::from("http_requests_duration_seconds")),
                *EXPONENTIAL_SECONDS,
            )
            .context("could not setup metric buckets")?
            .install_recorder()
            .context("can't run the metric recorder")?;

        let services = EdgeServices::new(redis_db, config.clone());

        // CORS configuration
        let cors_origins: Vec<String> = config
            .cors_origin
            .split(",")
            .map(|s| {
                s.trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .to_string()
            })
            .collect();
        let preview_cors_origins: Vec<String> = config
            .preview_cors_origin
            .split(",")
            .map(|s| {
                s.trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .to_string()
            })
            .collect();

        let cors = cors_builder!(
            origins: cors_origins.clone(),
            preview_origins: preview_cors_origins.clone(),
            methods: vec![
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::DELETE,
                Method::OPTIONS,
            ],
            headers: vec![AUTHORIZATION, CONTENT_TYPE, ACCEPT]
        );

        let proxy_cors = cors_builder!(
            origins: cors_origins,
            preview_origins: preview_cors_origins,
            methods: vec![Method::GET, Method::OPTIONS],
            headers: vec![AUTHORIZATION, CONTENT_TYPE, header::RANGE]
        )
        .expose_headers([header::CONTENT_LENGTH, header::CONTENT_RANGE]);

        // edge routes, just streams, proxy, and health
        let api_routes = Router::new()
            .nest("/streams", api::stream_controller::StreamController::app())
            .route("/health", get(api::health_controller::health_endpoint))
            .layer(cors);

        let proxy_routes = Router::new()
            .nest("/proxy", api::proxy_controller::ProxyController::app())
            .layer(proxy_cors);

        let router = Router::new()
            .nest("/api/v1", api_routes.merge(proxy_routes))
            .route("/", get(api::health_controller::health_endpoint))
            .route("/metrics", get(move || ready(recorder_handle.render())))
            .layer(
                ServiceBuilder::new()
                    .layer(TraceLayer::new_for_http())
                    .layer(HandleErrorLayer::new(Self::handle_timeout_error))
                    .timeout(Duration::from_secs(*HTTP_TIMEOUT))
                    .layer(Extension(services))
                    // Buffer: 2048 requests can be queued (double for 1000+ concurrent)
                    .layer(BufferLayer::new(2048))
                    // Rate limit: 50 requests per second per IP (generous for video streaming)
                    .layer(RateLimitLayer::new(50, Duration::from_secs(1))),
            )
            .route_layer(middleware::from_fn(Self::track_metrics));

        let router = router.fallback(Self::handle_404);

        let port = format!("0.0.0.0:{}", config.port);
        let addr = tokio::net::TcpListener::bind(&port).await.unwrap();

        info!("Edge setup completed, initialized server on port {port}");
        debug!(
            "routes initialized (edge mode: no auth), listening on port {}",
            &port
        );

        axum::serve(addr, router)
            .with_graceful_shutdown(Self::shutdown_signal())
            .await
            .context("axum serving failed")?;

        Ok(())
    }

    // custom timeout layer
    async fn handle_timeout_error(err: BoxError) -> (StatusCode, Json<serde_json::Value>) {
        if err.is::<tower::timeout::error::Elapsed>() {
            (
                StatusCode::REQUEST_TIMEOUT,
                Json(json!({
                    "error":
                        format!(
                            "request took longer than the configured {} second timeout",
                            *HTTP_TIMEOUT
                        )
                })),
            )
        } else {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": format!("unhandled internal error: {}", err)
                })),
            )
        }
    }

    async fn track_metrics(request: Request<axum::body::Body>, next: Next) -> impl IntoResponse {
        let path = if let Some(matched_path) = request.extensions().get::<MatchedPath>() {
            matched_path.as_str().to_owned()
        } else {
            request.uri().path().to_owned()
        };
        let start = Instant::now();
        let method = request.method().clone();
        let response = next.run(request).await;
        let latency = start.elapsed().as_secs_f64();
        let status = response.status().as_u16().to_string();

        metrics::counter!("http_requests_total", "method" => method.to_string(), "path" => path.clone(), "status" => status.clone()).increment(1);

        metrics::histogram!("http_requests_duration_seconds", "method" => method.to_string(), "path" => path, "status" => status).record(latency);

        response
    }

    async fn shutdown_signal() {
        tokio::signal::ctrl_c()
            .await
            .expect("how did we crash listening for SIGINT");
        println!("signal shutdown");
    }

    async fn handle_404() -> impl IntoResponse {
        (
            StatusCode::NOT_FOUND,
            axum::response::Json(serde_json::json!({
            "errors":{
            "message": vec!(String::from("This resource doesn't exist.")),}
            })),
        )
    }
}
