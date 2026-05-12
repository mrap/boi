//! Typed client for the provisioner plugin.
//!
//! Note: per design, the provisioner only ever receives a JoinToken
//! issued by the core. The plugin never reads or writes etcd; only
//! the freshly-provisioned node does, using the token to register
//! itself under `/boi/nodes/`.

use boi_proto::provisioner::v1 as pb;
pub use pb::provisioner_client::ProvisionerClient;
pub use pb::{
    CapHint, DeprovisionRequest, DeprovisionResponse, HandshakeRequest, HandshakeResponse,
    JoinToken, ProvisionRequest, ProvisionResponse,
};

pub struct ProvisionerPlugin<T> {
    pub inner: ProvisionerClient<T>,
}

impl<T> ProvisionerPlugin<T> {
    pub fn new(inner: ProvisionerClient<T>) -> Self {
        Self { inner }
    }
}
