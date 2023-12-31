use std::{sync::Arc, time::Duration};

use axum::Router;
use futures::{FutureExt, StreamExt};
use k8s_openapi::api::{
    core::v1::{Namespace, ResourceQuota},
    rbac::v1::{Role, RoleBinding},
};
use kube::{
    api::{Patch, PatchParams},
    core::ObjectMeta,
    runtime::{controller::Action, watcher, Controller},
    Api, Client, Resource,
};
use serde::Deserialize;
use thiserror::Error;
use tokio::task::JoinHandle;
use tracing::info;

use bacchus_gpu_controller::crd::UserBootstrap;

const PATCH_MANAGER: &str = "bacchus-gpu-controller.bacchus.io";

#[derive(Clone, Debug, Deserialize)]
struct Config {
    listen_addr: String,
    listen_port: u16,
}

#[derive(Error, Debug)]
enum ControllerError {
    #[error("missing object key: {0}")]
    MissingObjectKey(&'static str),
    #[error("patch failed: {0}")]
    PatchFailed(#[source] kube::Error),
}

#[derive(Error, Debug)]
enum Error {
    #[error("http server error: {0}")]
    HttpServerError(String),
    #[error("signal handling error")]
    SignalHandlerError,
}

struct Data {
    client: Client,
}

async fn reconcile(obj: Arc<UserBootstrap>, ctx: Arc<Data>) -> Result<Action, ControllerError> {
    let client = ctx.client.clone();
    let oref = obj.controller_owner_ref(&()).unwrap();

    // get name from UserBootstrap metadata
    let name = obj
        .metadata
        .name
        .clone()
        .ok_or_else(|| {
            tracing::error!("failed to get name");
            ControllerError::MissingObjectKey(".metadata.name")
        })?
        .to_lowercase();

    tracing::info!("reconciling {}", name);

    let patch_params = PatchParams::apply(PATCH_MANAGER).force();

    // reconcile namespace
    let ns = Namespace {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        },
        ..Default::default()
    };

    let ns_api = Api::<Namespace>::all(client.clone());

    ns_api
        .patch(&name, &patch_params, &Patch::Apply(ns))
        .await
        .map_err(|e| {
            tracing::error!("failed to patch namespace: {}", e);
            ControllerError::PatchFailed(e)
        })?;

    // reconcile resource quota
    if let Some(quota_spec) = obj.spec.quota.clone() {
        let quota = ResourceQuota {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                owner_references: Some(vec![oref.clone()]),
                ..Default::default()
            },
            spec: Some(quota_spec),
            ..Default::default()
        };

        let quota_api = Api::<ResourceQuota>::namespaced(client.clone(), &name);

        quota_api
            .patch(&name, &patch_params, &Patch::Apply(quota))
            .await
            .map_err(|e| {
                tracing::error!("failed to patch resource quota: {}", e);
                ControllerError::PatchFailed(e)
            })?;
    }

    // reconcile role
    if let Some(mut role) = obj.spec.role.clone() {
        let role_api = Api::<Role>::namespaced(client.clone(), &name);
        role.metadata.owner_references = Some(vec![oref.clone()]);

        role_api
            .patch(&name, &patch_params, &Patch::Apply(role))
            .await
            .map_err(|e| {
                tracing::error!("failed to patch role: {}", e);
                ControllerError::PatchFailed(e)
            })?;
    }

    // reconcile rolebinding
    if let Some(rolebinding) = obj.spec.rolebinding.clone() {
        // if the UserBootstrap is not synchronized with the sheet, then don't create rolebinding
        if let Some(status) = obj.status.clone() {
            if status.synchronized_with_sheet {
                let rolebinding_with_meta = RoleBinding {
                    metadata: ObjectMeta {
                        name: Some(name.clone()),
                        owner_references: Some(vec![oref]),
                        ..Default::default()
                    },
                    role_ref: rolebinding.role_ref,
                    subjects: rolebinding.subjects,
                };

                let rolebinding_api = Api::<RoleBinding>::namespaced(client.clone(), &name);

                rolebinding_api
                    .patch(&name, &patch_params, &Patch::Apply(rolebinding_with_meta))
                    .await
                    .map_err(|e| {
                        tracing::error!("failed to patch rolebinding: {}", e);
                        ControllerError::PatchFailed(e)
                    })?;
            }
        }
    }

    Ok(Action::requeue(Duration::from_secs(30)))
}

fn error_policy(obj: Arc<UserBootstrap>, error: &ControllerError, _ctx: Arc<Data>) -> Action {
    let obj_name = obj
        .metadata
        .name
        .clone()
        .unwrap_or_else(|| "<unknown>".to_string());
    let obj_namespace = obj
        .metadata
        .namespace
        .clone()
        .unwrap_or_else(|| "<unknown>".to_string());
    tracing::error!(
        "error reconciling \"{}/{}\": {}",
        obj_namespace,
        obj_name,
        error
    );
    Action::requeue(Duration::from_secs(3))
}

async fn shutdown_signal(tx: tokio::sync::broadcast::Sender<()>) -> Result<(), Error> {
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

    let (signal_tx, mut signal_rx) = tokio::sync::broadcast::channel::<()>(1);

    let client = Client::try_default().await?;

    let ub_api = Api::<UserBootstrap>::all(client.clone());
    let ns_api = Api::<Namespace>::all(client.clone());
    let quota_api = Api::<ResourceQuota>::all(client.clone());
    let role_api = Api::<Role>::all(client.clone());
    let rolebinding_api = Api::<RoleBinding>::all(client.clone());

    let data = Arc::new(Data { client });
    let controller_handle = {
        let controller = Controller::new(ub_api, watcher::Config::default())
            .owns(ns_api, watcher::Config::default())
            .owns(quota_api, watcher::Config::default())
            .owns(role_api, watcher::Config::default())
            .owns(rolebinding_api, watcher::Config::default())
            .graceful_shutdown_on(async move { signal_rx.recv().await }.map(|_| ()))
            .run(reconcile, error_policy, data)
            .for_each(|res| async move {
                match res {
                    Ok(o) => tracing::info!("reconciled {:?}", o),
                    Err(e) => tracing::error!("reconcile failed: {}", e),
                }
            });
        let controller = async {
            controller.await;
            Ok::<_, Error>(())
        };
        tokio::spawn(controller)
    };

    let mut signal_rx = signal_tx.subscribe();
    // create health check endpoint
    let app = Router::new().route("/health", axum::routing::get(|| async { "pong" }));

    let addr = format!("{}:{}", config.listen_addr, config.listen_port);
    tracing::info!("starting server on {}", addr);
    let http_server_handle = {
        let server = axum::Server::bind(&addr.parse()?)
            .serve(app.into_make_service())
            .with_graceful_shutdown(async move { signal_rx.recv().await }.map(|_| ()));
        let server = async {
            server.await.map_err(|e| {
                tracing::error!("server error: {}", e);
                Error::HttpServerError(e.to_string())
            })?;
            Ok::<_, Error>(())
        };
        tokio::spawn(server)
    };

    // wait for signal
    let shutdown_handle = tokio::spawn(shutdown_signal(signal_tx));

    // join all handles
    let _ = tokio::try_join!(
        flatten_error(controller_handle),
        flatten_error(http_server_handle),
        flatten_error(shutdown_handle),
    )?;

    info!("controller gracefully shutted down");

    Ok(())
}
