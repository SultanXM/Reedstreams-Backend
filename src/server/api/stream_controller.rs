use axum::Router;
use axum::extract::{Json, Path};
use axum::routing::{delete, get};
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use serde::Serialize;
use tracing::debug;
use tracing::info;

use crate::server::dtos::stream_dto::{GameDto, GameListResponse, ResponseStreamDto, SportsurgeEventDto, SportsurgeEventListResponse, SportsurgeStreamResponse};
use crate::server::error::AppResult;
use crate::server::extractors::EdgeAuthentication;
use crate::server::utils::signature_utils::SignatureUtil;

pub struct StreamController;

#[derive(Serialize)]
pub struct SignedUrlResponse {
    pub signed_url: String,
    pub expires_at: i64,
}

impl StreamController {
    pub fn app() -> Router {
        Router::new()
            // all routes
            .route("/", get(Self::get_all_streams_endpoint))
            // ppvsu routes
            .route("/ppvsu/cache", delete(Self::clear_ppvsu_cache_endpoint))
            .route("/ppvsu/{id}", get(Self::get_ppvsu_game_endpoint))
            .route(
                "/ppvsu/{id}/decode",
                get(Self::get_ppvsu_decoded_game_endpoint),
            )
            .route("/ppvsu/{id}/signed-url", get(Self::get_signed_url_endpoint))
            // sportsurge routes
            .route("/sportsurge", get(Self::get_sportsurge_events_endpoint))
            .route("/sportsurge/{id}/embed", get(Self::get_sportsurge_embed_endpoint))
            .route("/sportsurge/refresh", get(Self::refresh_sportsurge_endpoint))
            .route("/sportsurge/cache", get(Self::clear_sportsurge_cache_endpoint))
            .route("/sportsurge/cache", delete(Self::clear_sportsurge_cache_endpoint))
            .route("/{provider}", get(Self::get_stream_endpoint))
    }

    pub async fn get_all_streams_endpoint(
        EdgeAuthentication(_client_id, services): EdgeAuthentication,
    ) -> AppResult<Json<GameListResponse>> {
        info!("recieved request to retrieve all games with auto-fetch");

        let categories = services.streams.get_all_games().await?;

        Ok(Json(GameListResponse { categories }))
    }

    pub async fn get_stream_endpoint(
        EdgeAuthentication(_client_id, services): EdgeAuthentication,
        Path(provider): Path<String>,
    ) -> AppResult<Json<ResponseStreamDto>> {
        info!(
            "recieved request to retrieve stream for provider {:?}",
            provider
        );

        let stream = services.streams.get_stream(provider).await?;

        Ok(Json(stream))
    }

    pub async fn get_ppvsu_game_endpoint(
        EdgeAuthentication(_client_id, services): EdgeAuthentication,
        Path(id): Path<i64>,
    ) -> AppResult<Json<GameDto>> {
        info!("recieved request to fetch ppvsu game with id {}", id);

        let game = services.ppvsu.get_game_by_id(id).await?;

        Ok(Json(game.into_dto()))
    }

    pub async fn get_ppvsu_decoded_game_endpoint(
        EdgeAuthentication(_client_id, services): EdgeAuthentication,
        Path(id): Path<i64>,
    ) -> AppResult<Json<serde_json::Value>> {
        debug!("recieved reques to decode ppvsu game with id {}", id);
        let game = services.ppvsu.get_game_by_id(id).await?;
        let link = services.ppvsu.fetch_video_link(&game.video_link).await?;
        Ok(Json(serde_json::json!({
            "decoded_link": link
        })))
    }

    pub async fn clear_ppvsu_cache_endpoint(
        EdgeAuthentication(_client_id, services): EdgeAuthentication,
    ) -> AppResult<Json<serde_json::Value>> {
        info!("recieved request to clear ppvsu cache");

        services.ppvsu.clear_cache().await?;

        Ok(Json(serde_json::json!({
            "success": true,
            "message": "Cache cleared successfully"
        })))
    }

    pub async fn get_signed_url_endpoint(
        EdgeAuthentication(client_id, services): EdgeAuthentication,
        Path(id): Path<i64>,
    ) -> AppResult<Json<SignedUrlResponse>> {
        info!("received request to generate signed URL for game {}", id);

        let game = services.ppvsu.get_game_by_id(id).await?;
        let link = services.ppvsu.fetch_video_link(&game.video_link).await?;

        let encoded_url = URL_SAFE
            .encode(link.as_bytes())
            .trim_end_matches('=')
            .to_string();

        // gen expiry (12 hours from now)
        let expiry = SignatureUtil::generate_expiry(12);

        // For edge, we sign with the client_id (IP + User-Agent hash) instead of user_id
        let signature =
            services
                .signature_util
                .generate_signature(&client_id, expiry, &encoded_url);

        let signed_url = format!(
            "/api/v1/proxy?url={}&schema=sports&sig={}&exp={}&client={}",
            encoded_url,
            signature,
            expiry,
            urlencoding::encode(&client_id)
        );

        info!("generated signed URL for game {} (expires: {})", id, expiry);

        Ok(Json(SignedUrlResponse {
            signed_url,
            expires_at: expiry,
        }))
    }

    // ===================================================================
    // SPORTSURGE ENDPOINTS
    // ===================================================================

    pub async fn get_sportsurge_events_endpoint(
        EdgeAuthentication(_client_id, services): EdgeAuthentication,
    ) -> AppResult<Json<SportsurgeEventListResponse>> {
        info!("getting sportsurge events");

        let events = services.sportsurge.get_events().await?;
        
        let dtos: Vec<SportsurgeEventDto> = events
            .into_iter()
            .map(|event| SportsurgeEventDto {
                id: event.id,
                title: event.title,
                league: event.league,
                banner: crate::server::services::sportsurge_scraper::DEFAULT_MATCH_BANNER.to_string(),
                start_time: event.start_time,
                status: event.status,
                is_live: event.is_live,
                event_path: event.event_path,
                embed_url: None,
            })
            .collect();

        Ok(Json(SportsurgeEventListResponse { events: dtos }))
    }

    pub async fn get_sportsurge_embed_endpoint(
        EdgeAuthentication(_client_id, services): EdgeAuthentication,
        Path(id): Path<String>,
    ) -> AppResult<Json<SportsurgeStreamResponse>> {
        info!("getting sportsurge embed for event {}", id);

        let embed_url = services.sportsurge.get_stream_url(&id).await?;

        Ok(Json(SportsurgeStreamResponse {
            event_id: id,
            embed_url,
        }))
    }

    pub async fn refresh_sportsurge_endpoint(
        EdgeAuthentication(_client_id, services): EdgeAuthentication,
    ) -> AppResult<Json<serde_json::Value>> {
        info!("force refreshing sportsurge cache");

        services.sportsurge.clear_cache().await?;
        let events = services.sportsurge.scrape_events().await?;

        Ok(Json(serde_json::json!({
            "success": true,
            "count": events.len(),
            "message": "Sportsurge re-scraped"
        })))
    }

    pub async fn clear_sportsurge_cache_endpoint(
        EdgeAuthentication(_client_id, services): EdgeAuthentication,
    ) -> AppResult<Json<serde_json::Value>> {
        info!("clearing sportsurge cache");

        services.sportsurge.clear_cache().await?;

        Ok(Json(serde_json::json!({
            "success": true,
            "message": "Cache cleared"
        })))
    }
}
