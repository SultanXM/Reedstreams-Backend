// basic ppvsu scraper
use async_trait::async_trait;
use mockall::automock;
use std::sync::Arc;
use tracing::{error, info, warn};

use std::collections::HashMap;

use crate::{
    database::stream::{DynStreamsRepository, Game},
    server::{
        dtos::stream_dto::{CategoryDto, GameDto, ResponseStreamDto},
        error::AppResult,
    },
};

use super::ppvsu_services::DynPpvsuService;

pub type DynStreamsService = Arc<dyn StreamsServiceTrait + Send + Sync>;

#[automock]
#[async_trait]
pub trait StreamsServiceTrait {
    async fn get_stream(&self, provider: String) -> AppResult<ResponseStreamDto>;
    async fn get_all_streams(&self) -> AppResult<Vec<ResponseStreamDto>>;
    async fn get_all_games(&self) -> AppResult<Vec<CategoryDto>>;
}

#[derive(Clone)]
pub struct StreamsService {
    repository: DynStreamsRepository,
    ppvsu_service: DynPpvsuService,
}

impl StreamsService {
    pub fn new(repository: DynStreamsRepository, ppvsu_service: DynPpvsuService) -> Self {
        Self {
            repository,
            ppvsu_service,
        }
    }

    /// Fetch via WARP when real IP is banned
    async fn fetch_via_warp(&self) -> anyhow::Result<Vec<Game>> {
        use std::process::Command;
        use std::time::Duration;
        use tokio::time::sleep;
        
        // Connect WARP
        info!("Connecting WARP...");
        Command::new("warp-cli").arg("connect").output()?;
        sleep(Duration::from_secs(5)).await;
        
        // Fetch
        let result = self.ppvsu_service.fetch_and_cache_games().await;
        
        // Disconnect WARP
        info!("Disconnecting WARP...");
        let _ = Command::new("warp-cli").arg("disconnect").output();
        
        match result {
            Ok(games) => Ok(games),
            Err(e) => Err(anyhow::anyhow!("WARP fetch failed: {}", e)),
        }
    }
}

#[async_trait]
impl StreamsServiceTrait for StreamsService {
    async fn get_stream(&self, provider: String) -> AppResult<ResponseStreamDto> {
        info!("retrieving stream for provider {:?}", provider);

        let stream = self
            .repository
            .get_stream(&provider)
            .await?
            .ok_or_else(|| {
                crate::server::error::Error::NotFound(format!(
                    "stream for provider {} not found",
                    provider
                ))
            })?;

        Ok(stream.into_dto())
    }

    async fn get_all_streams(&self) -> AppResult<Vec<ResponseStreamDto>> {
        info!("retrieving all streams");

        let streams = self
            .repository
            .get_all_streams()
            .await?
            .into_iter()
            .map(|s| s.into_dto())
            .collect();

        Ok(streams)
    }

    async fn get_all_games(&self) -> AppResult<Vec<CategoryDto>> {
        info!("retrieving all games");

        let last_fetch = self.repository.get_last_fetch_time("ppvsu").await?;
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| anyhow::anyhow!("System time before UNIX epoch"))?
            .as_secs() as i64;

        // 1.5 hours = 5400 seconds
        const CACHE_TTL: i64 = 5400;
        
        let cache_expired = match last_fetch {
            None => true,
            Some(last) => (current_time - last) > CACHE_TTL,
        };

        let games = if cache_expired {
            info!("Cache expired (or not found), fetching fresh data...");
            
            // Try normal fetch first
            match self.ppvsu_service.fetch_and_cache_games().await {
                Ok(games) => {
                    info!("Fetched successfully with real IP");
                    self.repository.set_last_fetch_time("ppvsu", current_time).await.ok();
                    games
                }
                Err(e) => {
                    warn!("Real IP fetch failed (banned?), trying WARP...: {}", e);
                    
                    // Use WARP
                    match self.fetch_via_warp().await {
                        Ok(games) => {
                            info!("Fetched successfully via WARP");
                            self.repository.set_last_fetch_time("ppvsu", current_time).await.ok();
                            games
                        }
                        Err(warp_err) => {
                            error!("WARP fetch also failed: {}", warp_err);
                            // Return old cache if exists, else error
                            self.repository.get_games("ppvsu").await?
                        }
                    }
                }
            }
        } else {
            let age = current_time - last_fetch.unwrap_or(0);
            info!("Cache is {} seconds old - using cached data", age);
            self.repository.get_games("ppvsu").await?
        };

        let mut categories_map: HashMap<String, Vec<GameDto>> = HashMap::new();

        for game in games {
            let category = game.category.clone();
            let game_dto = game.into_dto();
            categories_map.entry(category).or_default().push(game_dto);
        }

        let mut categories: Vec<CategoryDto> = categories_map
            .into_iter()
            .map(|(category, games)| CategoryDto { category, games })
            .collect();

        categories.sort_by(|a, b| a.category.cmp(&b.category));

        Ok(categories)
    }
}
