use std::collections::BTreeMap;

use axum::{extract, http::StatusCode, response, routing::post, Router};
use bacchus_gpu_controller::crd::UserBootstrap;
use json_patch::{AddOperation, PatchOperation};
use k8s_openapi::{api::core::v1::ResourceQuotaSpec, apimachinery::pkg::api::resource::Quantity};
use kube::core::{
    admission::{AdmissionRequest, AdmissionResponse, AdmissionReview, Operation},
    DynamicObject,
};
use thiserror::Error;

#[derive(Error, Debug)]
enum Error {}

impl response::IntoResponse for Error {
    fn into_response(self) -> response::Response {
        // TODO
        (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()).into_response()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let app = Router::new().route("/mutate", post(mutate_handler));

    // TODO: use config
    axum::Server::bind(&"0.0.0.0:12321".parse()?)
        .serve(app.into_make_service())
        .await?;

    // TODO: handle signal

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

    let resp = mutate(&req)?;

    Ok(response::Json(resp.into_review()))
}

fn mutate(req: &AdmissionRequest<DynamicObject>) -> Result<AdmissionResponse, Error> {
    // get username of requester
    let req_username = match req.user_info.username {
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
            // TODO: use adequate admin name
            if req_username != "admin" {
                let e = "Only admin can delete UserBootstrap";
                tracing::error!(e);
                return Ok(resp.deny(e));
            }
            // early return
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
    }

    Ok(resp)
}
