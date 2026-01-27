// basic ppvsu scraper
use async_trait::async_trait;
use mockall::automock;
use std::sync::Arc;
use tracing::info;

use std::collections::HashMap;

use crate::{
    database::stream::DynStreamsRepository,
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
        info!("retrieving all games with auto-fetch");

        let last_fetch = self.repository.get_last_fetch_time("ppvsu").await?;

        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| anyhow::anyhow!("System time before UNIX epoch"))?
            .as_secs() as i64;

        let one_hour = 3600;
        let should_fetch = match last_fetch {
            None => {
                info!("no previous fetch found, fetching all games from API");
                true
            }
            Some(last_time) => {
                let age = current_time - last_time;
                if age > one_hour {
                    info!("last fetch was {} seconds ago (> 1 hour), refetching", age);
                    true
                } else {
                    info!("last fetch was {} seconds ago, using cache", age);
                    false
                }
            }
        };

        let games = if should_fetch {
            // cache is old so we drop it
            info!("Dumping cache of ppvsu:* matchers");
            self.repository.clear_cache("ppvsu").await?;

            info!("fetching all games from ppvs.su API");
            let games = self.ppvsu_service.fetch_and_cache_games().await?;
            self.repository
                .set_last_fetch_time("ppvsu", current_time)
                .await?;
            games
        } else {
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
