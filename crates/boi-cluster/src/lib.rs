//! BOI cluster state plane — etcd-backed primitives used by `boi-node`.
//!
//! Phase 1 layers:
//! - T4BF7: typed [`EtcdClient`] wrapper + lease management (`client`).
//! - T7C09: schemas — `nodes`, `dispatch_queue`, `claims`, `hooks_hwm`.
//! - T5ABC (next): membership module on top of the above.

pub mod client;

pub mod claims;
pub mod dispatch_queue;
pub mod hooks_hwm;
pub mod membership;
pub mod nodes;

#[cfg(test)]
mod testutil;

pub use client::{ClusterError, EtcdClient, LeaseHandle, Result, TxnOp};
