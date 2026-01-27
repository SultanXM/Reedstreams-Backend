#[derive(clap::ValueEnum, Clone, Debug, Copy)]
pub enum CargoEnv {
    Development,
    Production,
}

#[derive(clap::Parser)]
pub struct AppConfig {
    // production or development
    #[clap(long, env, value_enum)]
    pub cargo_env: CargoEnv,

    // port that the app will bind to
    #[clap(long, env, default_value = "5000")]
    pub port: u16,

    // db is here if ever needed, also default to sqlite but postgres is recommended
    // #[clap(long, env, default_value = "sqlite:///app/db.sqlite")]
    // pub database_url: String,

    // redis url for the connection
    #[clap(long, env)]
    pub redis_url: String,

    // db based and not needed for the edge
    //
    // option to run migrations on each startup
    // #[clap(long, env)]
    // pub run_migrations: bool,

    // this is needed to generate signatures, have it be anything secure
    // like 'openssl rand -base64 32'
    #[clap(long, env)]
    pub access_token_secret: String,

    // below are all secrets that are db specific, they're used to sign sessions and keys
    // #[clap(long, env)]
    // pub refresh_token_secret: String,

    // #[clap(long, env)]
    // pub registration_key_secret: String,

    // this should be either * for allowing everything, or a comma seperated list of domains like
    // example.com,something.com
    #[clap(long, env)]
    pub cors_origin: String,

    // same as above but used for preview environments to stress or test the api.
    #[clap(long, env)]
    pub preview_cors_origin: String,

    // seed the database also not needed for edge
    // #[clap(long, env)]
    // pub seed: bool,

    // optional sentry integration
    #[clap(long, env)]
    pub sentry_dsn: Option<String>,
}

impl Default for AppConfig {
    // defaults aren't really needed here but it's here as a bad fallback
    fn default() -> Self {
        Self {
            cargo_env: CargoEnv::Development,
            port: 5000,
            // database_url: "sqlite:///app/db.sqlite".to_string(),
            redis_url: "redis://localhost:6379".to_string(),
            // run_migrations: false,
            access_token_secret: "default-access-secret".to_string(),
            // refresh_token_secret: "default-refresh-secret".to_string(),
            // registration_key_secret: "default-registration-secret".to_string(),
            cors_origin: "*".to_string(),
            preview_cors_origin: "*".to_string(),
            // seed: false,
            sentry_dsn: None,
        }
    }
}
