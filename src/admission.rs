use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

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
use k8s_openapi::{
    api::{
        core::v1::ResourceQuotaSpec,
        rbac::v1::{RoleRef, Subject},
    },
    apimachinery::pkg::api::resource::Quantity,
};
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

    // this will be used to tell the user is authenticated with oidc
    // which means that the user is not an admin
    // also, username stripped of this prefix will be used as the actual username.
    oidc_username_prefix: String,

    default_role_name: String,
    #[serde(deserialize_with = "comma_seperated_deserialize")]
    // group name list that will be used to determine if the user is authorized or not
    authorized_group_names: Vec<String>,
}

fn comma_seperated_deserialize<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let str_sequence = String::deserialize(deserializer)?;
    Ok(str_sequence
        .split(',')
        .map(|item| item.to_owned())
        .collect())
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

    Ok(sha256::digest([cert_content, key_content].concat()))
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
    while stopper.stop_future(interval.tick()).await.is_some() {
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

#[derive(Clone, Debug)]
struct AppState {
    config: Config,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // read config from env
    let config = envy::prefixed("CONF_").from_env::<Config>()?;

    // prepare tls server config
    let tls_config = RustlsConfig::from_pem_file(&config.cert_path, &config.key_path).await?;

    let stopper = Stopper::new();

    let state = Arc::new(AppState {
        config: config.clone(),
    });

    let app = Router::new()
        .route("/mutate", post(mutate_handler))
        .route("/health", get(|| async { "pong" }))
        .with_state(state);

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

    let addr = format!("{}:{}", config.listen_addr, config.listen_port);
    tracing::info!("starting tls server on {}", addr);

    // start tls-enabled http server
    axum_server::bind_rustls(addr.parse()?, tls_config)
        .handle(handle.clone())
        .serve(app.into_make_service())
        .await?;

    tracing::info!("received signal. shutting down...");

    Ok(())
}

async fn mutate_handler(
    extract::State(state): extract::State<Arc<AppState>>,
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

    let resp = mutate(&req, &state)?;

    Ok(response::Json(resp.into_review()))
}

struct Username {
    original_username: String,
    kube_username: String,
    kind: UsernameKind,
}

enum UsernameKind {
    Normal,
    Admin,
}

impl Username {
    fn from(username: impl AsRef<str>, prefix: impl AsRef<str>) -> Self {
        if username.as_ref().starts_with(prefix.as_ref()) {
            Self {
                original_username: username.as_ref().to_string(),
                kube_username: username
                    .as_ref()
                    .to_string()
                    .strip_prefix(prefix.as_ref())
                    .unwrap()
                    .to_string(),
                kind: UsernameKind::Normal,
            }
        } else {
            // if username is not prefixed with oidc prefix, assume admin user
            Self {
                original_username: username.as_ref().to_string(),
                kube_username: username.as_ref().to_string(),
                kind: UsernameKind::Admin,
            }
        }
    }
}

fn mutate(
    req: &AdmissionRequest<DynamicObject>,
    state: &AppState,
) -> Result<AdmissionResponse, Error> {
    // check if username is prefixed with oidc prefix
    let username = {
        // get username of requester
        let req_username = match req.user_info.username {
            Some(ref username) => username,
            None => {
                let e = "cannot get requester's username from request";
                tracing::error!(e);
                return Ok(AdmissionResponse::invalid(e));
            }
        };

        Username::from(req_username, &state.config.oidc_username_prefix)
    };

    let resp: AdmissionResponse = req.into();

    // check if user is in authorized group
    let is_in_group = req
        .user_info
        .groups
        .clone()
        .unwrap_or_default()
        .iter()
        .map(|group| state.config.authorized_group_names.contains(group))
        .any(|is_in_group| is_in_group);

    match req.operation {
        Operation::Create => {
            // if user is not in authorized group and user is normal user, deny
            match username.kind {
                UsernameKind::Normal if !is_in_group => {
                    let e = "user is not in authorized group";
                    tracing::error!(e);
                    return Ok(resp.deny(e));
                }
                _ => {}
            }
        }
        Operation::Delete => {
            // if user is normal user, deny
            if let UsernameKind::Normal = username.kind {
                let e = "normal user is not allowed to delete resource";
                tracing::error!(e);
                return Ok(resp.deny(e));
            }

            // early return
            return Ok(resp);
        }
        Operation::Update => {
            // if user is normal user, deny
            if let UsernameKind::Normal = username.kind {
                let e = "normal user is not allowed to update resource";
                tracing::error!(e);
                return Ok(resp.deny(e));
            }

            // continue processing
        }
        _ => {
            let e = "invalid operation";
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

    // get resource name
    let resource_name = match obj.metadata.name.clone() {
        Some(name) => name,
        None => {
            let e = "cannot get resource name from request";
            tracing::error!(e);
            return Ok(AdmissionResponse::invalid(e));
        }
    };

    // deny if username is not match with resource name
    match username.kind {
        UsernameKind::Normal if username.kube_username != resource_name => {
            let e = "username not match with resource name";
            tracing::error!(e);
            return Ok(resp.deny(e));
        }
        _ => {}
    }

    // parse request object
    let ub: UserBootstrap = match obj.clone().try_parse() {
        Ok(ub) => ub,
        Err(error) => {
            tracing::error!(%error, "Request is not UserBootstrap resource");
            return Ok(AdmissionResponse::invalid(error.to_string()));
        }
    };

    let mut patches = Vec::new();

    match username.kind {
        UsernameKind::Normal => {
            // if user is normal user, fill kube username
            patches.push(PatchOperation::Add(AddOperation {
                path: "/spec/kube_username".to_string(),
                value: serde_json::json!(username.kube_username),
            }));
        }
        UsernameKind::Admin => {
            // admin can set kube_username field to any value.
            // if kube_username is empty, deny.
            if ub
                .spec
                .kube_username
                .clone()
                .unwrap_or("".to_string())
                .is_empty()
            {
                let e = "kube_username field is empty. you are an admin, so fill it";
                tracing::error!(e);
                return Ok(resp.deny(e));
            }
        }
    }

    // if quota key is empty, fill it
    if ub.spec.quota.is_none() {
        patches.push(PatchOperation::Add(AddOperation {
            path: "/spec/quota".to_string(),
            value: serde_json::json!({}),
        }));

        // add default quota
        // TODO: use default quota from config
        let default_quota = ResourceQuotaSpec {
            hard: Some(BTreeMap::from_iter(vec![
                ("requests.cpu".to_string(), Quantity("1".into())),
                ("requests.memory".to_string(), Quantity("500Mi".into())),
            ])),
            ..Default::default()
        };

        patches.push(PatchOperation::Add(AddOperation {
            path: "/spec/quota".to_string(),
            value: serde_json::to_value(default_quota).unwrap(),
        }));
    } else {
        // if quota key is not empty and user is normal user, deny
        if let UsernameKind::Normal = username.kind {
            let e = "quota field is not empty. you are a normal user, so leave it empty";
            tracing::error!(e);
            return Ok(resp.deny(e));
        }
    }

    // if rolebinding key is empty, create default
    if ub.spec.rolebinding.is_none() {
        patches.push(PatchOperation::Add(AddOperation {
            path: "/spec/rolebinding".to_string(),
            value: serde_json::json!({}),
        }));

        let subject_name = match username.kind {
            // use username as subject name
            UsernameKind::Normal => username.original_username,
            // use object's kube_username field as subject name
            UsernameKind::Admin => ub.spec.kube_username.clone().unwrap(),
        };

        let rb = bacchus_gpu_controller::crd::RoleBinding {
            role_ref: RoleRef {
                name: state.config.default_role_name.to_string(),
                api_group: "rbac.authorization.k8s.io".into(),
                kind: "ClusterRole".into(),
            },
            subjects: Some(vec![Subject {
                kind: "User".into(),
                name: subject_name,
                api_group: Some("rbac.authorization.k8s.io".into()),
                ..Default::default()
            }]),
        };

        patches.push(PatchOperation::Add(AddOperation {
            path: "/spec/rolebinding".to_string(),
            value: serde_json::to_value(rb).unwrap(),
        }));
    } else {
        // if rolebinding key is not empty and user is normal user, deny
        if let UsernameKind::Normal = username.kind {
            let e = "rolebinding field is not empty. you are a normal user, so leave it empty";
            tracing::error!(e);
            return Ok(resp.deny(e));
        }
    }

    if patches.is_empty() {
        Ok(resp)
    } else {
        Ok(resp.with_patch(json_patch::Patch(patches))?)
    }
}
