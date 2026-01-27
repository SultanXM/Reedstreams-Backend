use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: HealthStatus,
    pub timestamp: DateTime<Utc>,
    pub uptime_seconds: u64,
    pub version: String,
    pub environment: String,
    pub services: ServiceHealthDetails,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ServiceHealthDetails {
    pub database: DatabaseHealth,
    pub redis: RedisHealth,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DatabaseHealth {
    pub status: HealthStatus,
    pub response_time_ms: f64,
    pub pool_active: u32,
    pub pool_max: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RedisHealth {
    pub status: HealthStatus,
    pub response_time_ms: f64,
}
