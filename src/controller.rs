use std::{sync::Arc, time::Duration};

use axum::Router;
use futures::{FutureExt, StreamExt};
use k8s_openapi::api::core::v1::{Namespace, ResourceQuota};
use kube::{
    api::{Patch, PatchParams},
    core::ObjectMeta,
    runtime::{controller::Action, watcher, Controller},
    Api, Client, Resource,
};
use thiserror::Error;
use tracing::info;

use bacchus_gpu_controller::crd::UserBootstrap;

const PATCH_MANAGER: &'static str = "bacchus-gpu-controller.bacchus.io";

#[derive(Error, Debug)]
enum ControllerError {
    #[error("missing object key: {0}")]
    MissingObjectKey(&'static str),
    #[error("patch failed: {0}")]
    PatchFailed(#[source] kube::Error),
}

struct Data {
    client: Client,
}

async fn reconcile(obj: Arc<UserBootstrap>, ctx: Arc<Data>) -> Result<Action, ControllerError> {
    let client = ctx.client.clone();
    let oref = obj.controller_owner_ref(&()).unwrap();

    // get name from UserBootstrap metadata
    let name = obj.metadata.name.clone().ok_or_else(|| {
        tracing::error!("failed to get name");
        ControllerError::MissingObjectKey(".metadata.name")
    })?;

    tracing::info!("reconciling {}", name);

    // reconcile namespace
    let ns = Namespace {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            owner_references: Some(vec![oref]),
            ..Default::default()
        },
        ..Default::default()
    };

    let ns_api = Api::<Namespace>::all(client.clone());

    ns_api
        .patch(&name, &PatchParams::apply(PATCH_MANAGER), &Patch::Apply(ns))
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
                ..Default::default()
            },
            spec: Some(quota_spec),
            ..Default::default()
        };

        let quota_api = Api::<ResourceQuota>::namespaced(client.clone(), &name);

        quota_api
            .patch(
                &name,
                &PatchParams::apply(PATCH_MANAGER),
                &Patch::Apply(quota),
            )
            .await
            .map_err(|e| {
                tracing::error!("failed to patch resource quota: {}", e);
                ControllerError::PatchFailed(e)
            })?;
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

async fn shutdown_signal(tx: tokio::sync::broadcast::Sender<()>) {
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

    tx.send(()).expect("failed to broadcast shutdown signal");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let (signal_tx, mut signal_rx) = tokio::sync::broadcast::channel::<()>(1);

    let client = Client::try_default().await?;

    let ub_api = Api::<UserBootstrap>::all(client.clone());
    let ns_api = Api::<Namespace>::all(client.clone());
    let quota_api = Api::<ResourceQuota>::all(client.clone());
    let rolebinding_api = Api::<ResourceQuota>::all(client.clone());

    let data = Arc::new(Data { client });
    let controller_handle = tokio::spawn(
        Controller::new(ub_api, watcher::Config::default())
            .owns(ns_api, watcher::Config::default())
            .owns(quota_api, watcher::Config::default())
            .owns(rolebinding_api, watcher::Config::default())
            .graceful_shutdown_on(async move { signal_rx.recv().await }.map(|_| ()))
            .run(reconcile, error_policy, data)
            .for_each(|res| async move {
                match res {
                    Ok(o) => println!("reconciled {:?}", o),
                    Err(e) => eprintln!("reconcile failed: {}", e),
                }
            }),
    );

    let mut signal_rx = signal_tx.subscribe();
    // create health check endpoint
    let app = Router::new().route("/health", axum::routing::get(|| async { "pong" }));

    let addr = "0.0.0.0:12322";
    tracing::info!("starting server on {}", addr);
    let http_server_handle = tokio::spawn(
        axum::Server::bind(&addr.parse()?)
            .serve(app.into_make_service())
            .with_graceful_shutdown(async move { signal_rx.recv().await }.map(|_| ())),
    );

    // wait for signal
    shutdown_signal(signal_tx).await;

    info!("received signal. shutting down...");

    // join all handles
    let _ = tokio::try_join!(controller_handle, http_server_handle)?;

    info!("controller gracefully shutted down");

    Ok(())
}
