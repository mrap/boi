//! gRPC contracts for the BOI distributed runtime.
//!
//! Each plugin slot lives in its own package (`boi.<name>.v1`); the
//! file path encodes the major version per design §16 Q4. Backwards-
//! compatible additions bump the `plugin_proto_minor` returned by the
//! mandatory `Handshake` RPC.

pub mod workspace {
    pub mod v1 {
        tonic::include_proto!("boi.workspace.v1");
    }
}
pub mod pool {
    pub mod v1 {
        tonic::include_proto!("boi.pool.v1");
    }
}
pub mod router {
    pub mod v1 {
        tonic::include_proto!("boi.router.v1");
    }
}
pub mod provisioner {
    pub mod v1 {
        tonic::include_proto!("boi.provisioner.v1");
    }
}
pub mod hooks {
    pub mod v1 {
        tonic::include_proto!("boi.hooks.v1");
    }
}
pub mod cluster {
    pub mod v1 {
        tonic::include_proto!("boi.cluster.v1");
    }
}

/// The proto minor version this build of the host speaks. Bumped on
/// every backwards-compatible addition.
pub const HOST_PROTO_MINOR: u32 = 0;
