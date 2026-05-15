//! Typed client for the provisioner plugin.
//!
//! Note: per design, the provisioner only ever receives a JoinToken
//! issued by the core. The plugin never reads or writes etcd; only
//! the freshly-provisioned node does, using the token to register
//! itself under `/boi/nodes/`.

use std::time::Duration;

use prost_types::Duration as ProtoDuration;
use uuid::Uuid;

use boi_proto::provisioner::v1 as pb;
pub use pb::provisioner_client::ProvisionerClient;
pub use pb::{
    CapHint, DeprovisionRequest, DeprovisionResponse, HandshakeRequest, HandshakeResponse,
    JoinToken, ProvisionRequest, ProvisionResponse,
};

/// Default provision deadline when no override is configured.
pub const DEFAULT_PROVISION_DEADLINE: Duration = Duration::from_secs(60);

pub struct ProvisionerPlugin<T> {
    pub inner: ProvisionerClient<T>,
}

impl<T> ProvisionerPlugin<T> {
    pub fn new(inner: ProvisionerClient<T>) -> Self {
        Self { inner }
    }
}

/// Build a [`ProvisionRequest`], generating a fresh `request_id` and
/// applying the given `bootstrap_url` and `deadline`.
pub fn build_provision_request(
    join_token: JoinToken,
    cap_hint: CapHint,
    spec_id: String,
    bootstrap_url: String,
    deadline: Option<Duration>,
) -> ProvisionRequest {
    let d = deadline.unwrap_or(DEFAULT_PROVISION_DEADLINE);
    ProvisionRequest {
        join_token: Some(join_token),
        cap_hint: Some(cap_hint),
        spec_id,
        request_id: Uuid::new_v4().to_string(),
        boi_bootstrap_url: bootstrap_url,
        deadline: Some(ProtoDuration {
            seconds: d.as_secs() as i64,
            nanos: d.subsec_nanos() as i32,
        }),
    }
}
