use k8s_openapi::api::{
    core::v1::ResourceQuotaSpec,
    rbac::v1::{Role, RoleRef, Subject},
};
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
    pub kube_username: String,
    /// ResourceQuota in namespace
    pub quota: Option<ResourceQuotaSpec>,
    /// Role in namespace. Optional.
    /// If not specified, additional Role is not created.
    pub role: Option<Role>,
    /// RoleBinding in namespace
    /// If not specified, admission controller will create default RoleBinding
    pub rolebinding: Option<RoleBinding>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
pub struct UserBootstrapStatus {
    // TODO
}

/// RoleBinding without metadata
#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
pub struct RoleBinding {
    pub role_ref: RoleRef,
    pub subjects: Option<Vec<Subject>>,
}
