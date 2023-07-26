use k8s_openapi::api::core::v1::ResourceQuotaSpec;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Serialize, Deserialize, Default, Debug, Clone, JsonSchema)]
#[kube(
    group = "bacchus.io",
    version = "v1",
    kind = "UserBootstrap",
    shortname = "ub",
    plural = "userbootstraps",
    status = "UserBootstrapStatus",
    derive = "Default"
)]
pub struct UserBootstrapSpec {
    /// Kubernetes username
    pub username: String,
    /// ResourceQuota in namespace
    pub quota: Option<ResourceQuotaSpec>,
    // TODO
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
pub struct UserBootstrapStatus {
    // TODO
}
