use axum::Extension;
use axum::Json;
use axum::http::StatusCode;
use chrono::Utc;
use tracing::error;

use crate::server::dtos::health_dto::{
    DatabaseHealth, HealthResponse, HealthStatus, RedisHealth, ServiceHealthDetails,
};
use crate::server::services::edge_services::EdgeServices;
use crate::server::{get_app_version, get_uptime_seconds};

/// health endpoint - only checks redis
/// if this isn't wanted comment out the health endpoint in ../mod.rs
pub async fn health_endpoint(
    Extension(services): Extension<EdgeServices>,
) -> (StatusCode, Json<HealthResponse>) {
    let redis_health = check_redis_health(&services).await;

    // just gonna leave this code here and disable it
    let db_health = DatabaseHealth {
        status: HealthStatus::Healthy, // N/A for edge mode
        response_time_ms: 0.0,
        pool_active: 0,
        pool_max: 0,
    };

    // depend on just redis here
    let overall_status = if redis_health.status == HealthStatus::Unhealthy {
        HealthStatus::Unhealthy
    } else {
        HealthStatus::Healthy
    };

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

    let http_status = match overall_status {
        HealthStatus::Healthy => StatusCode::OK,
        HealthStatus::Degraded => StatusCode::OK,
        HealthStatus::Unhealthy => StatusCode::SERVICE_UNAVAILABLE,
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
            RedisHealth {
                status: HealthStatus::Unhealthy,
                response_time_ms: 0.0,
            }
        }
    }
}
