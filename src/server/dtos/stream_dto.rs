use serde::{Deserialize, Serialize};

use crate::database::stream::{Game, Stream};

impl Stream {
    pub fn into_dto(self) -> ResponseStreamDto {
        ResponseStreamDto {
            provider: self.provider,
            data: self.data,
        }
    }
}

impl Game {
    pub fn into_dto(self) -> GameDto {
        GameDto {
            id: self.id,
            name: self.name,
            poster: self.poster,
            start_time: self.start_time,
            end_time: self.end_time,
            cache_time: self.cache_time,
            video_link: self.video_link,
            category: self.category,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ResponseStreamDto {
    pub provider: String,
    pub data: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct StreamListResponse {
    pub streams: Vec<ResponseStreamDto>,
}

// This is what's returned in json form
#[derive(Serialize, Deserialize, Debug)]
pub struct GameDto {
    pub id: i64,
    pub name: String,
    pub poster: String,
    pub start_time: i64,
    pub end_time: i64,
    pub cache_time: i64,
    pub video_link: String,
    pub category: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CategoryDto {
    pub category: String,
    pub games: Vec<GameDto>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct GameListResponse {
    pub categories: Vec<CategoryDto>,
}

// Sportsurge-specific DTOs - simplified
#[derive(Serialize, Deserialize, Debug)]
pub struct SportsurgeEventDto {
    pub id: String,
    pub title: String,
    pub league: String,
    pub banner: String,
    pub start_time: i64,
    pub status: String,
    pub is_live: bool,
    pub event_path: String,
    pub embed_url: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SportsurgeEventListResponse {
    pub events: Vec<SportsurgeEventDto>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SportsurgeStreamResponse {
    pub event_id: String,
    pub embed_url: String,
}
