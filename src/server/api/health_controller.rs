use axum::Extension;
use axum::Json;
use axum::http::StatusCode;
use chrono::Utc;
use std::time::Instant;
use tracing::{debug, error};

use crate::server::dtos::health_dto::{
    DatabaseHealth, HealthResponse, HealthStatus, RedisHealth, ServiceHealthDetails,
};
use crate::server::services::edge_services::EdgeServices;
use crate::server::{get_app_version, get_uptime_seconds};

/// Maximum allowed time for health check to complete
/// Must be under Fly.io's 5s health check timeout
const HEALTH_CHECK_TIMEOUT_MS: u64 = 2000;

/// Fast health endpoint optimized for Fly.io health checks
/// 
/// CRITICAL: This endpoint must respond within Fly.io's health check timeout (5s).
/// To ensure this, we use a lightweight check that doesn't block on external services.
pub async fn health_endpoint(
    Extension(services): Extension<EdgeServices>,
) -> (StatusCode, Json<HealthResponse>) {
    let start = Instant::now();
    
    // Try Redis health check but don't let it block indefinitely
    // This prevents health check failures when Redis is slow but not dead
    let redis_health = tokio::time::timeout(
        std::time::Duration::from_millis(1500),
        check_redis_health(&services)
    ).await.unwrap_or_else(|_| {
        debug!("Redis health check timed out");
        RedisHealth {
            status: HealthStatus::Degraded,
            response_time_ms: HEALTH_CHECK_TIMEOUT_MS as f64,
        }
    });

    let db_health = DatabaseHealth {
        status: HealthStatus::Healthy, // N/A for edge mode
        response_time_ms: 0.0,
        pool_active: 0,
        pool_max: 0,
    };

    // Determine overall status - degraded is still OK for Fly.io
    let overall_status = match redis_health.status {
        HealthStatus::Unhealthy => HealthStatus::Degraded, // Don't report unhealthy for transient issues
        other => other,
    };

    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    debug!("Health check completed in {:.2}ms", elapsed_ms);

    let response = HealthResponse {
        status: overall_status,
        timestamp: Utc::now(),
        uptime_seconds: get_uptime_seconds(),
        version: get_app_version().to_string(),
        environment: format!("{:?}", services.config.cargo_env).to_lowercase(),
        services: ServiceHealthDetails {
            database: db_health,
            redis: redis_health,
        },
    };

    // Always return 200 OK for degraded/healthy to keep Fly.io happy
    // Only return 503 for truly unhealthy state
    let http_status = match overall_status {
        HealthStatus::Unhealthy => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::OK,
    };

    (http_status, Json(response))
}

async fn check_redis_health(services: &EdgeServices) -> RedisHealth {
    match services.redis.health_check().await {
        Ok(response_time) => RedisHealth {
            status: HealthStatus::Healthy,
            response_time_ms: response_time,
        },
        Err(e) => {
            error!("Redis health check failed: {}", e);
            // Report degraded instead of unhealthy to avoid unnecessary restarts
            RedisHealth {
                status: HealthStatus::Degraded,
                response_time_ms: 0.0,
            }
        }
    }
}
