use std::{collections::BTreeMap, path::Path, time::Duration};

use axum::{
    extract,
    http::StatusCode,
    response,
    routing::{get, post},
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use bacchus_gpu_controller::crd::UserBootstrap;
use json_patch::{AddOperation, PatchOperation};
use k8s_openapi::{api::core::v1::ResourceQuotaSpec, apimachinery::pkg::api::resource::Quantity};
use kube::core::{
    admission::{AdmissionRequest, AdmissionResponse, AdmissionReview, Operation},
    DynamicObject,
};
use serde::Deserialize;
use stopper::Stopper;
use thiserror::Error;

#[derive(Clone, Debug, Deserialize)]
struct Config {
    listen_addr: String,
    listen_port: u16,

    cert_path: String,
    key_path: String,
}

#[derive(Error, Debug)]
enum Error {
    #[error("failed to read cert: {0}")]
    CertReadError(#[from] std::io::Error),
    #[error("failed to serialize patch: {0}")]
    SerializePatchError(#[from] kube::core::admission::SerializePatchError),
}

impl response::IntoResponse for Error {
    fn into_response(self) -> response::Response {
        // TODO
        (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()).into_response()
    }
}

async fn shutdown_signal(handle: axum_server::Handle, stopper: Stopper) {
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
    handle.graceful_shutdown(Some(Duration::from_secs(10)));
}

async fn get_cert_hash(cert: impl AsRef<Path>, key: impl AsRef<Path>) -> Result<String, Error> {
    let cert_content = tokio::fs::read(cert).await?;
    let key_content = tokio::fs::read(key).await?;

    Ok(sha256::digest(&[cert_content, key_content].concat()))
}

// very simple cert reloader based on file content hash
async fn cert_reloader(
    cert: impl AsRef<Path>,
    key: impl AsRef<Path>,
    tls_config: RustlsConfig,
    stopper: Stopper,
) -> Result<(), Error> {
    // get initial hash
    let mut hash = get_cert_hash(&cert, &key).await?;
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    while let Some(_) = stopper.stop_future(interval.tick()).await {
        // calc new hash
        let new_hash = get_cert_hash(&cert, &key).await?;
        // if hash is different, reload cert
        if hash != new_hash {
            tracing::info!("cert changed, reloading...");
            tls_config.reload_from_pem_file(&cert, &key).await?;
            tracing::info!("cert reloading done.");
            hash = new_hash;
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // read config from env
    let config = envy::prefixed("CONF_").from_env::<Config>()?;

    // prepare tls server config
    let tls_config = RustlsConfig::from_pem_file(&config.cert_path, &config.key_path).await?;

    let stopper = Stopper::new();

    let app = Router::new()
        .route("/mutate", post(mutate_handler))
        .route("/health", get(|| async { "pong" }));

    let handle = axum_server::Handle::new();

    // start cert reloader
    tokio::spawn(cert_reloader(
        config.cert_path.clone(),
        config.key_path.clone(),
        tls_config.clone(),
        stopper.clone(),
    ));

    // wait for signal
    let signal_future = shutdown_signal(handle.clone(), stopper);
    tokio::spawn(async move {
        signal_future.await;
    });

    tracing::info!(
        "starting tls server on {}:{}",
        config.listen_addr,
        config.listen_port
    );

    // start tls-enabled http server
    axum_server::bind_rustls(
        format!("{}:{}", config.listen_addr, config.listen_port).parse()?,
        tls_config,
    )
    .handle(handle.clone())
    .serve(app.into_make_service())
    .await?;

    tracing::info!("received signal. shutting down...");

    Ok(())
}

async fn mutate_handler(
    extract::Json(req): extract::Json<AdmissionReview<DynamicObject>>,
) -> Result<response::Json<AdmissionReview<DynamicObject>>, Error> {
    // convert AdmissionReview to AdmissionRequest
    let req: AdmissionRequest<_> = match req.try_into() {
        Ok(req) => req,
        Err(error) => {
            tracing::error!(%error, "invalid request");
            return Ok(response::Json(
                AdmissionResponse::invalid(error.to_string()).into_review(),
            ));
        }
    };

    tracing::info!(?req, "received admission request");

    let resp = mutate(&req)?;

    Ok(response::Json(resp.into_review()))
}

fn mutate(req: &AdmissionRequest<DynamicObject>) -> Result<AdmissionResponse, Error> {
    // get username of requester
    let _req_username = match req.user_info.username {
        Some(ref username) => username,
        None => {
            let e = "Cannot get requester's username from request";
            tracing::error!(e);
            return Ok(AdmissionResponse::invalid(e));
        }
    };

    let resp: AdmissionResponse = req.into();

    match req.operation {
        Operation::Create => {
            // check if user is in gpu group
            let is_in_group = req
                .user_info
                .groups
                .clone()
                .unwrap_or_default()
                // TODO: use group name from config
                .contains(&"gpu".to_string());

            if !is_in_group {
                let e = "User is not in gpu group";
                tracing::error!(e);
                return Ok(resp.deny(e));
            }
        }
        Operation::Delete => {
            // NOTE: we are not going to deny delete operation
            // because regular users are prevented from deleting
            // resource by kubernetes rbac.
            // so early return
            return Ok(resp);
        }
        // TODO: handle update?
        _ => {
            let e = "Invalid operation";
            tracing::error!(e);
            return Ok(AdmissionResponse::invalid(e));
        }
    };

    let obj = if let Some(ref obj) = req.object {
        obj
    } else {
        // NOTE: object will empty if operation is delete, so this should not be happen.
        // but just in case, return early.
        return Ok(resp);
    };

    // parse request object
    let ub: UserBootstrap = match obj.clone().try_parse() {
        Ok(ub) => ub,
        Err(error) => {
            tracing::error!(%error, "Request is not UserBootstrap resource");
            return Ok(AdmissionResponse::invalid(error.to_string()));
        }
    };

    let mut patches = Vec::new();

    // if quota key is empty, fill it
    if ub.spec.quota.is_none() {
        patches.push(PatchOperation::Add(AddOperation {
            path: "/spec/quota".to_string(),
            value: serde_json::json!({}),
        }));

        // add default quota
        // TODO: use default quota from config
        let default_quota: ResourceQuotaSpec = ResourceQuotaSpec {
            hard: Some(BTreeMap::from_iter(vec![
                ("requests.cpu".to_string(), Quantity("1".into())),
                ("requests.memory".to_string(), Quantity("500Mi".into())),
            ])),
            ..Default::default()
        };

        patches.push(PatchOperation::Add(AddOperation {
            path: "/spec/quota/hard".to_string(),
            value: serde_json::to_value(default_quota).unwrap(),
        }));
        Ok(resp.with_patch(json_patch::Patch(patches))?)
    } else {
        Ok(resp)
    }
}
