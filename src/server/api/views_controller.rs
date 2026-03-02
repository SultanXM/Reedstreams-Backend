// Views Controller - API endpoints for view counter
use axum::{
    extract::Path,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use tracing::{info, debug};

use crate::server::{
    error::AppResult,
    extractors::EdgeAuthentication,
    services::views_services::ViewsServiceTrait,
};

#[derive(Debug, Serialize)]
pub struct ViewResponse {
    pub views: u64,
    pub match_id: String,
    pub is_new_viewer: bool,
}

#[derive(Debug, Serialize)]
pub struct ViewCountResponse {
    pub views: u64,
    pub match_id: String,
}

pub struct ViewsController;

impl ViewsController {
    pub fn app() -> Router {
        Router::new()
            .route("/{match_id}", post(Self::track_view))
            .route("/{match_id}/count", get(Self::get_view_count))
    }

    /// POST /api/v1/views/{match_id}
    /// Track a view for a match
    /// Uses IP + User-Agent hash for deduplication
    async fn track_view(
        EdgeAuthentication(_client_id, services): EdgeAuthentication,
        Path(match_id): Path<String>,
    ) -> AppResult<impl IntoResponse> {
        debug!("tracking view for match: {}", match_id);

        // Use client_id as viewer hash (already computed from IP + User-Agent)
        let viewer_hash = _client_id;

        let views = services.views.increment_view(&match_id, &viewer_hash).await?;
        
        info!("view tracked for match {}: {} total views", match_id, views);

        let response = ViewResponse {
            views,
            match_id,
            is_new_viewer: true, // This could be determined by checking before/after
        };

        Ok((StatusCode::OK, Json(response)))
    }

    /// GET /api/v1/views/{match_id}/count
    /// Get current view count for a match
    async fn get_view_count(
        EdgeAuthentication(_client_id, services): EdgeAuthentication,
        Path(match_id): Path<String>,
    ) -> AppResult<impl IntoResponse> {
        debug!("getting view count for match: {}", match_id);

        let views = services.views.get_view_count(&match_id).await?;

        let response = ViewCountResponse {
            views,
            match_id,
        };

        Ok((StatusCode::OK, Json(response)))
    }
}
