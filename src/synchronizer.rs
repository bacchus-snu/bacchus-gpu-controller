use std::time::Duration;

use google_drive3::{
    hyper, hyper_rustls,
    oauth2::{self, ServiceAccountKey},
    DriveHub,
};
use serde::Deserialize;
use stopper::Stopper;
use thiserror::Error;

#[derive(Clone, Debug, Deserialize)]
struct Config {
    google_service_account_json_path: String,
    google_file_id: String,
    sync_interval_secs: u64,
}

#[derive(Error, Debug)]
enum Error {
    #[error("google auth error: {0}")]
    GoogleAuthError(String),
    #[error("file is not utf8: {0}")]
    Utf8Error(#[from] std::string::FromUtf8Error),
    #[error("google api error: {0}")]
    GoogleApiError(#[from] google_drive3::client::Error),
    #[error("request failed")]
    RequestFailed,
    #[error("hyper error: {0}")]
    HyperError(#[from] hyper::Error),
}

async fn synchronize_loop(
    key: ServiceAccountKey,
    config: Config,
    stopper: Stopper,
) -> Result<(), Error> {
    // initialize google drive client
    let auth = oauth2::authenticator::ServiceAccountAuthenticator::builder(key)
        .build()
        .await
        .map_err(|e| Error::GoogleAuthError(e.to_string()))?;
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .https_only()
        .enable_http1()
        .build();
    let client = hyper::Client::builder().build(https);
    let hub = DriveHub::new(client, auth);

    let mut interval = tokio::time::interval(Duration::from_secs(config.sync_interval_secs));
    while stopper.stop_future(interval.tick()).await.is_some() {
        // read file as csv
        let mut resp = hub
            .files()
            .export(&config.google_file_id, "text/csv")
            .doit()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::RequestFailed);
        }
        let content = String::from_utf8(hyper::body::to_bytes(resp.body_mut()).await?.to_vec())?;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // read config from env
    let config = envy::prefixed("CONF_").from_env::<Config>()?;

    // read key file from file
    let key = oauth2::read_service_account_key(&config.google_service_account_json_path).await?;

    // start synchronization loop
    synchronize_loop(key, config, Stopper::new()).await?;

    Ok(())
}
