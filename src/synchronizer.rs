use std::{collections::BTreeMap, time::Duration};

use bacchus_gpu_controller::crd::UserBootstrap;
use google_drive3::{
    hyper, hyper_rustls,
    oauth2::{self, ServiceAccountKey},
    DriveHub,
};
use k8s_openapi::{api::core::v1::ResourceQuotaSpec, apimachinery::pkg::api::resource::Quantity};
use kube::{
    api::{ListParams, Patch, PatchParams},
    Api,
};
use serde::Deserialize;
use stopper::Stopper;
use thiserror::Error;

#[derive(Clone, Debug, Deserialize)]
struct Config {
    google_service_account_json_path: String,
    google_file_id: String,
    #[serde(default = "default_sync_interval_secs")]
    sync_interval_secs: u64,
    gpu_server_name: String,
}

const fn default_sync_interval_secs() -> u64 {
    60
}

#[derive(Error, Debug)]
enum Error {
    #[error("kube error: {0}")]
    KubeError(#[from] kube::Error),
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
    #[error("csv header error: {0}")]
    CsvHeaderError(String),
    #[error("csv parsing error: {0}")]
    CsvParseError(#[from] csv::Error),
}

#[derive(Clone, Debug, Deserialize)]
struct Row {
    // timestamp field (unused)
    // timestamp: String,

    // real name
    name: String,
    // username in id.snucse.org
    id_username: String,
    // gpu server name
    gpu_server: String,
    // gpu request
    gpu_request: i64,
    // cpu request
    cpu_request: i64,
    // memory request
    memory_request: i64,
    // storage request
    storage_request: i64,
}

// try to infer header name with heuristics
fn try_infer_header(header: impl AsRef<str>) -> Result<String, Error> {
    let header = header.as_ref();
    if header == "타임스탬프" {
        return Ok("timestamp".to_string());
    }
    if header == "이름" {
        return Ok("name".to_string());
    }
    if header.contains("id.snucse.org") && header.contains("이름") {
        return Ok("id_username".to_string());
    }
    if header.contains("GPU 서버") {
        return Ok("gpu_server".to_string());
    }
    if header.contains("GPU 개수") {
        return Ok("gpu_request".to_string());
    }
    if header.contains("CPU 코어") || header.contains("CPU 개수") {
        return Ok("cpu_request".to_string());
    }
    if header.contains("메모리") {
        return Ok("memory_request".to_string());
    }
    if header.contains("스토리지") {
        return Ok("storage_request".to_string());
    }

    return Err(Error::CsvHeaderError(format!(
        "unknown header: \"{}\"",
        header
    )));
}

fn parse_csv(content: impl AsRef<str>) -> Result<Vec<Row>, Error> {
    let mut rdr = csv::Reader::from_reader(content.as_ref().as_bytes());

    // rename headers with heuristics
    let headers = rdr.headers()?.clone();
    let new_headers = headers
        .iter()
        .map(|header| try_infer_header(header))
        .collect::<Result<Vec<_>, _>>()?;
    rdr.set_headers(csv::StringRecord::from(new_headers));

    // deserialize rows
    let mut rows = Vec::new();
    for result in rdr.deserialize() {
        let row: Row = result?;
        rows.push(row);
    }
    Ok(rows)
}

async fn synchronize_loop(
    client: kube::Client,
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
    let hub = DriveHub::new(hyper::Client::builder().build(https), auth);

    // UserBootstrap api
    let ub_api = Api::<UserBootstrap>::all(client.clone());

    let mut interval = tokio::time::interval(Duration::from_secs(config.sync_interval_secs));
    while stopper.stop_future(interval.tick()).await.is_some() {
        tracing::info!("starting synchronization");
        // read file as csv
        let mut resp = hub
            .files()
            // reference: https://developers.google.com/drive/api/guides/ref-export-formats
            .export(&config.google_file_id, "text/csv")
            .doit()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::RequestFailed);
        }
        let content = String::from_utf8(hyper::body::to_bytes(resp.body_mut()).await?.to_vec())?;
        // filter rows by gpu server name
        let rows = parse_csv(content)?
            .into_iter()
            .filter(|row| row.gpu_server == config.gpu_server_name)
            .collect::<Vec<_>>();

        // list all UserBootstrap resources
        let ubs = ub_api.list(&ListParams::default()).await?.items;
        for ub in ubs {
            let resource_name = match ub.metadata.name {
                Some(ref name) => name.to_string(),
                None => continue,
            };

            // find row which has same username
            let row = match rows.iter().find(|row| row.id_username == resource_name) {
                Some(row) => row,
                None => continue,
            };

            // let's update the quota!

            let mut new_ub = ub.clone();

            let quota = ResourceQuotaSpec {
                hard: Some(BTreeMap::from_iter(vec![
                    (
                        "requests.cpu".to_string(),
                        Quantity(row.cpu_request.to_string().into()),
                    ),
                    (
                        "requests.memory".to_string(),
                        Quantity(format!("{}Mi", row.memory_request).into()),
                    ),
                    (
                        "requests.nvidia.com/gpu".to_string(),
                        Quantity(row.gpu_request.to_string().into()),
                    ),
                    (
                        "requests.storage".to_string(),
                        Quantity(format!("{}Gi", row.storage_request).into()),
                    ),
                ])),
                ..Default::default()
            };

            new_ub.spec.quota = Some(quota);

            tracing::info!(
                row.name,
                row.id_username,
                row.cpu_request,
                row.memory_request,
                row.gpu_request,
                row.storage_request,
                "updating quota"
            );

            // apply patches
            let patch_params = PatchParams::default();
            ub_api
                .patch(&resource_name, &patch_params, &Patch::Apply(new_ub))
                .await?;
        }
    }

    Ok(())
}

async fn shutdown_signal(stopper: Stopper) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("signal received, starting graceful shutdown");

    stopper.stop();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // read config from env
    let config = envy::prefixed("CONF_").from_env::<Config>()?;

    // read key file from file
    let key = oauth2::read_service_account_key(&config.google_service_account_json_path).await?;

    // initialize kube client
    let client = kube::Client::try_default().await?;

    let stopper = Stopper::new();

    // wait for signal
    let signal_future = shutdown_signal(stopper.clone());
    tokio::spawn(async move {
        signal_future.await;
    });

    // start synchronization loop
    synchronize_loop(client, key, config, stopper).await?;

    Ok(())
}
