use std::{sync::Arc, time::Duration};

use futures::StreamExt;
use k8s_openapi::api::core::v1::{Namespace, ResourceQuota};
use kube::{
    runtime::{controller::Action, watcher, Controller},
    Api, Client, Resource,
};
use thiserror::Error;
use tracing::info;

use bacchus_gpu_controller::crd::UserBootstrap;

// const PATCH_MANAGER: &'static str = "bacchus-gpu-controller.bacchus.io";

#[derive(Error, Debug)]
enum ControllerError {
    #[error("missing object key: {0}")]
    MissingObjectKey(&'static str),
}

struct Data {
    client: Client,
}

async fn reconcile(obj: Arc<UserBootstrap>, ctx: Arc<Data>) -> Result<Action, ControllerError> {
    let _client = ctx.client.clone();
    let _oref = obj.controller_owner_ref(&()).unwrap();

    let ub_name = obj.metadata.name.clone().ok_or_else(|| {
        tracing::error!("failed to get name");
        ControllerError::MissingObjectKey(".metadata.name")
    })?;

    tracing::info!("reconciling {}", ub_name);

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let client = Client::try_default().await?;

    let ub_api = Api::<UserBootstrap>::all(client.clone());
    let ns_api = Api::<Namespace>::all(client.clone());
    let quota_api = Api::<ResourceQuota>::all(client.clone());
    // TODO

    let data = Arc::new(Data { client });
    Controller::new(ub_api, watcher::Config::default())
        .owns(ns_api, watcher::Config::default())
        .owns(quota_api, watcher::Config::default())
        .shutdown_on_signal()
        .run(reconcile, error_policy, data)
        .for_each(|res| async move {
            match res {
                Ok(o) => println!("reconciled {:?}", o),
                Err(e) => eprintln!("reconcile failed: {}", e),
            }
        })
        .await;

    info!("controller shutting down");

    Ok(())
}
