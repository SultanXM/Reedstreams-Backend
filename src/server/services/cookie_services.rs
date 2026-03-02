use std::sync::Arc;

use tracing::{debug, error};

use crate::database::Database;

/// ttl of 24hrs
const COOKIE_TTL_SECONDS: u64 = 86400;

pub type DynCookieService = Arc<dyn CookieServiceTrait + Send + Sync>;

#[async_trait::async_trait]
pub trait CookieServiceTrait {
    async fn get_cookies(&self, domain: &str) -> Option<String>;

    async fn store_cookies(&self, domain: &str, cookies: &[String]);
}

pub struct CookieService {
    db: Arc<Database>,
}

impl CookieService {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    fn cookie_key(&self, domain: &str) -> String {
        format!("proxy_cookies:{}", domain)
    }

    pub fn extract_domain(url: &str) -> Option<String> {
        url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
    }
}

// this stuff should probably be in the database repository type of files
#[async_trait::async_trait]
impl CookieServiceTrait for CookieService {
    async fn get_cookies(&self, domain: &str) -> Option<String> {
        let key = self.cookie_key(domain);

        match self.db.as_ref() {
            Database::Redis(db) => {
                use redis::AsyncCommands;
                let mut conn = db.connection.clone();
                let result: Result<Option<String>, redis::RedisError> = conn.get(&key).await;

                match result {
                    Ok(Some(cookies)) => {
                        debug!(
                            "Loaded cookies for domain {}: {} bytes",
                            domain,
                            cookies.len()
                        );
                        Some(cookies)
                    }
                    Ok(None) => None,
                    Err(e) => {
                        error!("Failed to get cookies for domain {}: {}", domain, e);
                        None
                    }
                }
            }
            Database::Memory(db) => {
                let result = db.store.get(&key).await;

                match result {
                    Ok(Some(cookies)) => {
                        debug!(
                            "Loaded cookies for domain {}: {} bytes",
                            domain,
                            cookies.len()
                        );
                        Some(cookies)
                    }
                    Ok(None) => None,
                    Err(e) => {
                        error!("Failed to get cookies for domain {}: {}", domain, e);
                        None
                    }
                }
            }
        }
    }

    async fn store_cookies(&self, domain: &str, cookies: &[String]) {
        if cookies.is_empty() {
            return;
        }

        let key = self.cookie_key(domain);

        let mut cookie_map: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        if let Some(existing) = self.get_cookies(domain).await {
            for cookie_str in existing.split("; ") {
                if let Some((name, _)) = cookie_str.split_once('=') {
                    cookie_map.insert(name.to_string(), cookie_str.to_string());
                }
            }
        }

        // parse new cookies and merge (new values override old)
        for cookie in cookies {
            // Set-Cookie format: name=value; attr1; attr2...
            // only want the name=value part
            let Some(cookie_value) = cookie.split(';').next() else {
                continue;
            };
            let Some((name, _)) = cookie_value.split_once('=') else {
                continue;
            };
            cookie_map.insert(name.trim().to_string(), cookie_value.trim().to_string());
        }

        // join all cookies into a single Cookie header value
        let cookie_header: String = cookie_map.values().cloned().collect::<Vec<_>>().join("; ");

        match self.db.as_ref() {
            Database::Redis(db) => {
                use redis::AsyncCommands;
                let mut conn = db.connection.clone();
                let result: Result<(), redis::RedisError> =
                    conn.set_ex(&key, &cookie_header, COOKIE_TTL_SECONDS).await;

                match result {
                    Ok(_) => {
                        debug!(
                            "Stored {} cookies for domain {} (TTL: {}s)",
                            cookie_map.len(),
                            domain,
                            COOKIE_TTL_SECONDS
                        );
                    }
                    Err(e) => {
                        error!("Failed to store cookies for domain {}: {}", domain, e);
                    }
                }
            }
            Database::Memory(db) => {
                let result = db.store.set_ex(&key, &cookie_header, COOKIE_TTL_SECONDS).await;

                match result {
                    Ok(_) => {
                        debug!(
                            "Stored {} cookies for domain {} (TTL: {}s)",
                            cookie_map.len(),
                            domain,
                            COOKIE_TTL_SECONDS
                        );
                    }
                    Err(e) => {
                        error!("Failed to store cookies for domain {}: {}", domain, e);
                    }
                }
            }
        }
    }
}
