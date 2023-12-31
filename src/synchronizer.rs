use std::{collections::BTreeMap, time::Duration};

use axum::Router;
use bacchus_gpu_controller::crd::{UserBootstrap, UserBootstrapStatus};
use futures::FutureExt;
use google_drive3::{
    hyper, hyper_rustls,
    oauth2::{self, ServiceAccountKey},
    DriveHub,
};
use json_patch::{AddOperation, PatchOperation, ReplaceOperation};
use k8s_openapi::{api::core::v1::ResourceQuotaSpec, apimachinery::pkg::api::resource::Quantity};
use kube::{
    api::{ListParams, Patch, PatchParams, PostParams},
    Api, ResourceExt,
};
use serde::Deserialize;
use stopper::Stopper;
use thiserror::Error;
use tokio::task::JoinHandle;

const PATCH_MANAGER: &str = "bacchus-gpu-controller.bacchus.io";

#[derive(Clone, Debug, Deserialize)]
struct Config {
    // for health check
    listen_addr: String,
    listen_port: u16,

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
    #[error("signal handling error")]
    SignalHandlerError,
}

#[derive(Clone, Debug, Deserialize)]
struct Row {
    // timestamp field (unused)
    // timestamp: String,

    // description (unused)
    // description: String,

    // email (unused)
    // email: String,

    // real name
    name: String,
    // department
    department: String,
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
    // mig request
    mig_request: i64,
    // authorized
    authorized: String,
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
    if header == "소속" {
        return Ok("department".to_string());
    }
    if header.contains("SNUCSE ID") {
        return Ok("id_username".to_string());
    }
    if header.contains("사용할 서버") {
        return Ok("gpu_server".to_string());
    }
    if header.contains("GPU 개수") {
        return Ok("gpu_request".to_string());
    }
    if header.contains("vCPU 개수") {
        return Ok("cpu_request".to_string());
    }
    if header.contains("메모리") {
        return Ok("memory_request".to_string());
    }
    if header.contains("스토리지") {
        return Ok("storage_request".to_string());
    }
    if header.contains("MiG 개수") {
        return Ok("mig_request".to_string());
    }
    if header.contains("요청 사유") {
        return Ok("description".to_string());
    }
    if header.contains("승인") {
        return Ok("authorized".to_string());
    }
    if header.contains("이메일") {
        return Ok("email".to_string());
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
        let row: Row = match result {
            Ok(row) => row,
            Err(e) => {
                tracing::warn!("row parsing error. skipping: {}", e);
                continue;
            }
        };
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
        tracing::info!("downloaded csv file");
        let content = String::from_utf8(hyper::body::to_bytes(resp.body_mut()).await?.to_vec())?;
        // filter rows by gpu server name
        let rows = parse_csv(content)?
            .into_iter()
            // NOTE: not exact match
            .filter(|row| row.gpu_server.contains(&config.gpu_server_name))
            .collect::<Vec<_>>();

        tracing::info!("target rows: {}", rows.len());

        // list all UserBootstrap resources
        let ubs = ub_api.list(&ListParams::default()).await?.items;
        for ub in ubs {
            let resource_name = match ub.metadata.name {
                Some(ref name) => name.to_string(),
                None => continue,
            };

            // find row which has same username (last match)
            let row = match rows.iter().rev().find(|row| {
                // if not authorized, skip
                let authorized = row.authorized.trim().to_lowercase();
                match authorized.as_str() {
                    "o" => {}
                    _ => return false,
                }
                row.id_username == resource_name
            }) {
                Some(row) => row,
                None => continue,
            };

            // let's update the quota!

            let mut patches = Vec::new();

            if ub.spec.quota.is_none() {
                patches.push(PatchOperation::Add(AddOperation {
                    path: "/spec/quota".to_string(),
                    value: serde_json::json!({}),
                }));
            }

            let quota = ResourceQuotaSpec {
                hard: Some(BTreeMap::from_iter(vec![
                    (
                        "requests.cpu".to_string(),
                        Quantity(row.cpu_request.to_string().into()),
                    ),
                    (
                        "requests.memory".to_string(),
                        Quantity(format!("{}Gi", row.memory_request).into()),
                    ),
                    (
                        "limits.cpu".to_string(),
                        Quantity(row.cpu_request.to_string().into()),
                    ),
                    (
                        "limits.memory".to_string(),
                        Quantity(format!("{}Gi", row.memory_request).into()),
                    ),
                    (
                        "requests.nvidia.com/gpu".to_string(),
                        Quantity(row.gpu_request.to_string().into()),
                    ),
                    (
                        "requests.storage".to_string(),
                        Quantity(format!("{}Gi", row.storage_request).into()),
                    ),
                    (
                        "requests.nvidia.com/mig-1g.10gb".to_string(),
                        Quantity(row.mig_request.to_string().into()),
                    ),
                ])),
                ..Default::default()
            };

            patches.push(PatchOperation::Replace(ReplaceOperation {
                path: "/spec/quota".to_string(),
                value: serde_json::json!(quota),
            }));

            // mark on status
            let status = serde_json::json!({
                "apiVersion": "bacchus.io/v1",
                "kind": "UserBootstrap",
                "metadata": {
                    "name": resource_name,
                    "resourceVersion": ub.resource_version(),
                },
                "status": UserBootstrapStatus {
                    synchronized_with_sheet: true,
                    ..Default::default()
                },
            });
            tracing::info!("updating status");
            ub_api
                .replace_status(
                    &resource_name,
                    &PostParams::default(),
                    serde_json::to_vec(&status).unwrap(),
                )
                .await?;

            tracing::info!(
                row.name,
                row.department,
                row.id_username,
                row.cpu_request,
                row.memory_request,
                row.gpu_request,
                row.storage_request,
                row.mig_request,
                "updating quota"
            );

            // apply patches
            let patch_params = PatchParams::apply(PATCH_MANAGER);
            ub_api
                .patch(
                    &resource_name,
                    &patch_params,
                    &Patch::Json::<()>(json_patch::Patch(patches)),
                )
                .await?;

            tracing::info!("quota updated")
        }
    }

    Ok(())
}

async fn shutdown_signal(
    tx: tokio::sync::broadcast::Sender<()>,
    stopper: Stopper,
) -> Result<(), Error> {
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
    tx.send(()).map_err(|_| Error::SignalHandlerError)?;

    Ok(())
}

async fn flatten_error<T>(handle: JoinHandle<Result<T, Error>>) -> Result<T, Error> {
    match handle.await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(err)) => Err(err),
        Err(_) => panic!("join error"),
    }
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

    let (signal_tx, mut signal_rx) = tokio::sync::broadcast::channel::<()>(1);

    // create health check endpoint
    let app = Router::new().route("/health", axum::routing::get(|| async { "pong" }));
    let addr = format!("{}:{}", config.listen_addr, config.listen_port);
    tracing::info!("starting http server on {}", addr);
    let http_server_handle = {
        let server = axum::Server::bind(&addr.parse()?)
            .serve(app.into_make_service())
            .with_graceful_shutdown(async move { signal_rx.recv().await }.map(|_| ()));
        let server = async {
            server.await?;
            Ok::<_, Error>(())
        };
        tokio::spawn(server)
    };

    // start synchronization loop
    let synchronizer_handle = {
        let stopper = stopper.clone();
        tokio::spawn(synchronize_loop(client, key, config, stopper))
    };

    // wait for signal
    let shutdown_signal_handle = {
        let stopper = stopper.clone();
        tokio::spawn(shutdown_signal(signal_tx, stopper))
    };

    // join all handles
    let _ = tokio::try_join!(
        flatten_error(synchronizer_handle),
        flatten_error(http_server_handle),
        flatten_error(shutdown_signal_handle)
    )?;

    tracing::info!("synchronizer gracefully shutted down");

    Ok(())
}
