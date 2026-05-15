//! Plugin host runtime.
//!
//! This crate owns the lifecycle of every plugin process: it spawns
//! the child, waits for the `BOI_READY\n` ready signal (F-11), runs
//! the mandatory `Handshake` RPC (Q4 file-name major versioning),
//! enforces the 3-restarts-in-5-min restart policy (F-20), and
//! exposes typed per-plugin clients.
//!
//! Per-plugin clients live in their own modules:
//! [`workspace`], [`pool`], [`router`], [`provisioner`], [`hooks`].

pub mod handshake;
pub mod hooks;
pub mod lifecycle;
pub mod pool;
pub mod provisioner;
pub mod router;
pub mod workspace;

pub use handshake::{HandshakeError, NegotiatedPlugin};
pub use lifecycle::{
    Plugin, PluginConfig, PluginHandle, PluginHealth, PluginKind, ReadyError, RestartPolicy,
};
