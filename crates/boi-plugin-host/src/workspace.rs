//! Typed client for the workspace plugin.
//!
//! Thin wrapper over the tonic-generated `WorkspaceClient`; future
//! phases will layer retry, telemetry, and capability gating here.

use boi_proto::workspace::v1 as pb;
pub use pb::workspace_client::WorkspaceClient;
pub use pb::{
    CleanupRequest, CleanupResponse, ExecRequest, ExecResponse, FetchRequest, FetchResponse,
    HandshakeRequest, HandshakeResponse, ProvisionRequest, ProvisionResponse, SetupRequest,
    SetupResponse, VerifyRequest, VerifyResponse,
};

/// Newtype tag so callers can't accidentally swap a workspace client
/// for a pool client at the API boundary.
pub struct WorkspacePlugin<T> {
    pub inner: WorkspaceClient<T>,
}

impl<T> WorkspacePlugin<T> {
    pub fn new(inner: WorkspaceClient<T>) -> Self {
        Self { inner }
    }
}
