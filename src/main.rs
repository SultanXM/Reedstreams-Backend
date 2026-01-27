use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use dotenvy::dotenv;

use tracing::info;

use api::{AppConfig, EdgeApplicationServer, Logger, RedisDatabase};

// main function for edge version - no database, only redis
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    let config = Arc::new(AppConfig::parse());

    // init logger and sentry, guards are kept alive to flush logs and maintain sentry connection
    let _guards = Logger::init(config.cargo_env, config.sentry_dsn.clone());

    // logging is up to you, I like to use info! for general information on what to do
    info!("logger and env prepped (edge mode - no database)...");

    info!("connecting to redis...");

    let redis_db = RedisDatabase::connect(&config.redis_url)
        .await
        .expect("where is the redis connection!!");

    info!("redis connection ok, starting edge server...");

    // serve the routes (edge mode - no database, only redis)
    EdgeApplicationServer::serve(config, redis_db)
        .await
        .context("edge server failed to start")?;

    Ok(())
}
