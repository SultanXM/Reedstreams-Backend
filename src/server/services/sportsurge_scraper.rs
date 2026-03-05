use async_trait::async_trait;
use mockall::automock;
use scraper::{Html, Selector};
use regex::Regex;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

use crate::{
    database::{
        Database,
        stream::{Game, StreamsRepository},
    },
    server::error::{AppResult, Error},
};

pub type DynSportsurgeScraper = Arc<dyn SportsurgeScraperTrait + Send + Sync>;

// Cache of 30 mins
const CACHE_TTL_SECONDS: i64 = 1800;

// Base domain for building URLs
const SPORTSURGE_BASE: &str = "https://sportsurge.ws";
// NCAA basketball streams page
const SPORTSURGE_LISTINGS_URL: &str = "https://sportsurge.ws/ncaa/livestreams2";

// Default banner for matches
pub const DEFAULT_MATCH_BANNER: &str = "https://images.unsplash.com/photo-1546519638-68e109498ffc?w=1200&h=600&fit=crop";

/// Generate a short hash (8 chars) from a string
fn short_hash(s: &str) -> String {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    format!("{:08x}", hasher.finish())[..8].to_string()
}

// What we extract from homepage
#[derive(Debug, Clone)]
pub struct SportsurgeEvent {
    pub id: String,        // Short 8-char hash
    pub title: String,
    pub league: String,
    pub event_path: String, // Full path for fetching
    pub status: String,
    pub start_time: i64,
    pub is_live: bool,
}

#[automock]
#[async_trait]
pub trait SportsurgeScraperTrait {
    // Get all events from homepage
    async fn scrape_events(&self) -> AppResult<Vec<SportsurgeEvent>>;

    // Get cached events
    async fn get_events(&self) -> AppResult<Vec<SportsurgeEvent>>;

    // Get embed URL for a specific event by ID (for backward compatibility)
    async fn get_stream_url(&self, event_id: &str) -> AppResult<String>;

    // Clear cache
    async fn clear_cache(&self) -> AppResult<()>;
}

#[derive(Clone)]
pub struct SportsurgeScraper {
    db: Arc<Database>,
    http: reqwest::Client,
}

impl SportsurgeScraper {
    pub fn new(db: Arc<Database>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("http client build failed");

        Self { db, http }
    }

    // Parse time string like "7:00 PM" to timestamp
    fn parse_time_to_timestamp(&self, time_str: &str) -> i64 {
        use chrono::{NaiveTime, Utc};
        
        let time_str = time_str.trim();
        
        // Try to parse time formats like "7:00 PM", "19:00", "Live"
        if time_str.to_lowercase().contains("live") {
            return Utc::now().timestamp();
        }
        
        // Try 12-hour format with AM/PM
        let formats = ["%I:%M %p", "%I:%M%p", "%H:%M"];
        
        for format in &formats {
            if let Ok(time) = NaiveTime::parse_from_str(time_str, format) {
                let now = Utc::now();
                let datetime = now.date_naive().and_time(time);
                return datetime.and_utc().timestamp();
            }
        }
        
        // Default to current time if parsing fails
        Utc::now().timestamp()
    }

    // Parse the homepage HTML
    fn parse_homepage(&self, html: &str) -> AppResult<Vec<SportsurgeEvent>> {
        let document = Html::parse_document(html);
        let mut events = Vec::new();
        
        let event_selector = Selector::parse("a.MaclariListele").map_err(|_| {
            Error::InternalServerErrorWithContext("invalid CSS selector".into())
        })?;

        for element in document.select(&event_selector) {
            // Get the href URL of event
            let href = element.value().attr("href").unwrap_or_default();
            if href.is_empty() {
                continue;
            }

            // Extract path (e.g., "/nba/team1-team2-12345" -> "nba/team1-team2-12345")
            let event_path = href.trim_start_matches('/').to_string();
            
            // Use short hash for ID instead of long path
            let id = short_hash(&event_path);

            // Get league from ListelemeDuzen div
            let league = element
                .select(&Selector::parse("div.ListelemeDuzen").unwrap())
                .next()
                .map(|el| el.text().collect::<String>().trim().to_string())
                .unwrap_or_else(|| "Unknown".to_string());

            // Get status/time from third div
            let status = element
                .select(&Selector::parse("div.ListelemeDuzen:nth-child(3)").unwrap())
                .next()
                .map(|el| el.text().collect::<String>().trim().to_string())
                .unwrap_or_default();

            let is_live = status.to_lowercase().contains("live");

            // Get match title from team names
            let title = self.extract_title(&element);
            if title.is_empty() {
                continue;
            }

            // Parse start time from status
            let start_time = self.parse_time_to_timestamp(&status);

            events.push(SportsurgeEvent {
                id,
                title,
                league,
                event_path,
                status,
                start_time,
                is_live,
            });
        }

        info!("parsed {} events from sportsurge", events.len());
        Ok(events)
    }

    // Extract match title from element
    fn extract_title(&self, element: &scraper::ElementRef) -> String {
        // Try to get team names first
        let team_rows: Vec<_> = element
            .select(&Selector::parse("div.team-name-event-row").unwrap())
            .collect();

        if team_rows.len() >= 2 {
            let team1 = self.extract_team_name(&team_rows[0]);
            let team2 = self.extract_team_name(&team_rows[1]);
            
            if !team1.is_empty() && !team2.is_empty() {
                return format!("{} vs {}", team1, team2);
            }
        }

        // Fallback to h4 element
        element
            .select(&Selector::parse("h4").unwrap())
            .next()
            .map(|el| el.text().collect::<String>().trim().to_string())
            .unwrap_or_default()
    }

    fn extract_team_name(&self, element: &scraper::ElementRef) -> String {
        element
            .select(&Selector::parse("span").unwrap())
            .next()
            .map(|el| el.text().collect::<String>().trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                element.text().collect::<String>().trim().to_string().into()
            })
            .unwrap_or_default()
    }

    // Parse event page to extract embed URL from iframe
    fn parse_event_page(&self, html: &str) -> Option<String> {
        let document = Html::parse_document(html);
        
        // First: try to find the main player iframe (cx-iframe is the main player)
        if let Ok(selector) = Selector::parse("iframe#cx-iframe") {
            if let Some(iframe) = document.select(&selector).next() {
                if let Some(src) = iframe.value().attr("src") {
                    if !src.is_empty() && !src.starts_with("javascript:") {
                        info!("found cx-iframe with src: {}", src);
                        
                        // The src might be a base URL like "https://gooz.aapmains.net/new-stream-embed/"
                        // We should look for stream IDs in the page JavaScript
                        let stream_id = self.extract_stream_id_from_js(html);
                        
                        if let Some(id) = stream_id {
                            // Build full embed URL with stream ID
                            let base = src.trim_end_matches('/');
                            let full_url = format!("{}/{}", base, id);
                            info!("built embed URL with stream ID: {}", full_url);
                            return Some(full_url);
                        }
                        
                        // No stream ID found, return the base URL as fallback
                        return Some(src.to_string());
                    }
                }
            }
        }
        
        // Fallback: try other iframe selectors
        let fallback_selectors = [
            "iframe[src*=embed]",
            "iframe[src*=stream]",
            "iframe[src*=player]",
            "iframe",
        ];
        
        for selector_str in &fallback_selectors {
            if let Ok(selector) = Selector::parse(selector_str) {
                for iframe in document.select(&selector) {
                    if let Some(src) = iframe.value().attr("src") {
                        if !src.is_empty() 
                            && !src.starts_with("javascript:")
                            && !src.starts_with("about:blank") {
                            info!("found iframe with selector '{}', src: {}", selector_str, src);
                            return Some(src.to_string());
                        }
                    }
                }
            }
        }

        warn!("no valid iframe found on event page");
        None
    }
    
    /// Extract stream ID from JavaScript in the page
    fn extract_stream_id_from_js(&self, html: &str) -> Option<String> {
        // Look for stream IDs in common patterns
        // Pattern 1: data-stream-id attribute
        let data_stream_re = Regex::new(r#"data-stream-id=["']([^"']+)["']"#).ok()?;
        if let Some(cap) = data_stream_re.captures(html) {
            if let Some(id) = cap.get(1) {
                return Some(id.as_str().to_string());
            }
        }
        
        // Pattern 2: streamId in JavaScript objects
        let js_stream_re = Regex::new(r#"streamId["']?\s*:\s*["']([^"']+)["']"#).ok()?;
        if let Some(cap) = js_stream_re.captures(html) {
            if let Some(id) = cap.get(1) {
                return Some(id.as_str().to_string());
            }
        }
        
        // Pattern 3: Look for changeStream function with stream ID
        if let Some(pos) = html.find("changeStream") {
            let snippet = &html[pos..std::cmp::min(pos + 800, html.len())];
            
            if let Some(embed_pos) = snippet.find("new-stream-embed/'") {
                let after_embed = &snippet[embed_pos + 17..];
                if let Some(plus_pos) = after_embed.find('+') {
                    let after_plus = after_embed[plus_pos + 1..].trim_start();
                    if let Some(first_quote) = after_plus.chars().next() {
                        if first_quote == '\'' || first_quote == '"' {
                            let rest = &after_plus[1..];
                            if let Some(end_pos) = rest.find(first_quote) {
                                let stream_id = &rest[..end_pos];
                                if !stream_id.is_empty() && stream_id.len() < 100 {
                                    return Some(stream_id.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        
        None
    }

    // Store events in cache
    async fn store_in_cache(&self, events: &[SportsurgeEvent]) -> AppResult<()> {
        let cache_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::InternalServerErrorWithContext("time error".into()))?
            .as_secs() as i64;

        for event in events {
            // Store game data with path-based ID
            let game = Game {
                id: event.id.chars().fold(0i64, |acc, c| acc.wrapping_add(c as i64)),
                name: event.title.clone(),
                poster: DEFAULT_MATCH_BANNER.to_string(),
                start_time: event.start_time,
                end_time: event.start_time + 7200,
                cache_time,
                video_link: event.event_path.clone(),
                category: event.league.clone(),
            };

            if let Err(e) = self.db.store_game("sportsurge", &game).await {
                warn!("failed to store event {}: {}", event.id, e);
            }
        }

        self.db.set_last_fetch_time("sportsurge", cache_time).await?;
        Ok(())
    }
}

#[async_trait]
impl SportsurgeScraperTrait for SportsurgeScraper {
    async fn scrape_events(&self) -> AppResult<Vec<SportsurgeEvent>> {
        info!("scraping sportsurge: {}", SPORTSURGE_LISTINGS_URL);

        let resp = self.http
            .get(SPORTSURGE_LISTINGS_URL)
            .header("Accept", "text/html")
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .await
            .map_err(|e| {
                error!("failed to fetch homepage: {}", e);
                Error::InternalServerErrorWithContext(format!("fetch failed: {}", e))
            })?;

        if !resp.status().is_success() {
            return Err(Error::InternalServerErrorWithContext(
                format!("homepage returned {}", resp.status())
            ));
        }

        let html = resp.text().await.map_err(|e| {
            error!("failed to read html: {}", e);
            Error::InternalServerErrorWithContext(format!("read failed: {}", e))
        })?;

        let events = self.parse_homepage(&html)?;
        self.store_in_cache(&events).await?;

        Ok(events)
    }

    async fn get_events(&self) -> AppResult<Vec<SportsurgeEvent>> {
        let last_fetch = self.db.get_last_fetch_time("sportsurge").await?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::InternalServerErrorWithContext("time error".into()))?
            .as_secs() as i64;

        let should_scrape = match last_fetch {
            None => true,
            Some(last) => (now - last) > CACHE_TTL_SECONDS,
        };

        if should_scrape {
            self.scrape_events().await?;
        }

        let games = self.db.get_games("sportsurge").await?;

        let events: Vec<SportsurgeEvent> = games
            .into_iter()
            .map(|g| {
                // Reconstruct ID from event_path (stored in video_link)
                let event_path = g.video_link;
                let id = short_hash(&event_path);
                SportsurgeEvent {
                    id,
                    title: g.name,
                    league: g.category,
                    event_path,
                    status: if g.end_time > now { "LIVE".to_string() } else { "Scheduled".to_string() },
                    start_time: g.start_time,
                    is_live: g.end_time > now,
                }
            })
            .collect();

        Ok(events)
    }

    async fn get_stream_url(&self, event_id: &str) -> AppResult<String> {
        info!("getting stream URL for event_id: {}", event_id);
        
        // Find the event to get its path
        let events = self.get_events().await?;
        let event_count = events.len();
        
        // Debug: log available event IDs
        if events.is_empty() {
            warn!("no events found in cache");
        } else {
            debug!("available events: {:?}", events.iter().map(|e| &e.id).collect::<Vec<_>>());
        }
        
        let event = events
            .into_iter()
            .find(|e| e.id == event_id)
            .ok_or_else(|| {
                warn!("event {} not found in {} events", event_id, event_count);
                Error::NotFound(format!("event {} not found", event_id))
            })?;

        info!("found event: {} with path: {}", event.id, event.event_path);

        // Normalize event_path - strip protocol and domain if present (handles old cache data)
        let clean_path = if event.event_path.starts_with("http") {
            let stripped = event.event_path
                .trim_start_matches("https://")
                .trim_start_matches("http://");
            stripped
                .split_once('/')
                .map(|(_, path)| path.to_string())
                .unwrap_or_else(|| stripped.to_string())
        } else {
            event.event_path.clone()
        };

        // Check cache first using clean_path as key (prevents collisions)
        let cache_key = format!("sportsurge:embed:{}", clean_path);
        
        if let Ok(Some(cached)) = self.db.get_video_link(&cache_key).await {
            info!("returning cached embed URL for {}", event_id);
            return Ok(cached);
        }

        // Build full event URL
        let event_url = format!("{}/{}", SPORTSURGE_BASE, clean_path);
        info!("fetching event page: {}", event_url);

        // Fetch and parse event page
        let resp = self.http
            .get(&event_url)
            .header("Accept", "text/html")
            .header("Referer", SPORTSURGE_LISTINGS_URL)
            .send()
            .await
            .map_err(|e| {
                error!("failed to fetch event page {}: {}", event_url, e);
                Error::InternalServerErrorWithContext(e.to_string())
            })?;

        let html = resp.text().await
            .map_err(|e| {
                error!("failed to read event page HTML: {}", e);
                Error::InternalServerErrorWithContext(e.to_string())
            })?;

        let embed_url = self.parse_event_page(&html)
            .ok_or_else(|| {
                warn!("no iframe#cx-iframe found on event page {}", event_url);
                Error::NotFound("no embed URL found".into())
            })?;

        info!("found embed URL for {}: {}", event_id, embed_url);

        // Cache the embed URL for 5 minutes
        let _ = self.db.set_video_link(&cache_key, &embed_url, 300).await;

        Ok(embed_url)
    }

    async fn clear_cache(&self) -> AppResult<()> {
        self.db.clear_cache("sportsurge").await?;
        Ok(())
    }
}
