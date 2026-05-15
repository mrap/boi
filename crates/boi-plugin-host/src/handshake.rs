//! Plugin proto handshake.
//!
//! Every plugin exposes a `Handshake(HandshakeRequest) returns
//! (HandshakeResponse)` RPC. The host calls it immediately after the
//! `BOI_READY\n` ready signal and:
//!
//! 1. Records the plugin's reported `plugin_proto_minor`.
//! 2. Collects the advertised capability set.
//! 3. Rejects the connection if the plugin's major version (encoded
//!    in the proto package — `boi.<name>.v1`) differs from the
//!    host's expected major. The host only links a single major at a
//!    time, so a mismatch is effectively impossible at the wire
//!    level — but a defensive check guards against a misconfigured
//!    plugin shipping the wrong stub.

use std::collections::BTreeSet;

use thiserror::Error;

/// Major version the host links against today (file-name versioning,
/// Q4). When the host bumps to `v2` this becomes `2`.
pub const HOST_PROTO_MAJOR: u32 = 1;

#[derive(Debug, Error)]
pub enum HandshakeError {
    #[error("plugin major version mismatch: host speaks v{host}, plugin reported v{plugin}")]
    MajorMismatch { host: u32, plugin: u32 },
    #[error("plugin minor version v{plugin} is newer than host v{host} — refusing to load")]
    MinorAhead { host: u32, plugin: u32 },
    #[error("rpc transport error: {0}")]
    Transport(String),
}

/// Outcome of a successful handshake.
#[derive(Debug, Clone)]
pub struct NegotiatedPlugin {
    pub major: u32,
    pub minor: u32,
    pub capabilities: BTreeSet<String>,
}

impl NegotiatedPlugin {
    pub fn has_capability(&self, cap: &str) -> bool {
        self.capabilities.contains(cap)
    }
}

/// Validate a plugin's handshake response against the host's
/// expectations. The major check is purely defensive (see module
/// docs). The minor check allows the plugin to be at-or-behind the
/// host's minor; a plugin that reports a newer minor than the host
/// is refused because the host cannot guarantee it understands the
/// plugin's extended messages.
pub fn validate(
    plugin_major: u32,
    plugin_minor: u32,
    host_minor: u32,
    capabilities: impl IntoIterator<Item = String>,
) -> Result<NegotiatedPlugin, HandshakeError> {
    if plugin_major != HOST_PROTO_MAJOR {
        return Err(HandshakeError::MajorMismatch {
            host: HOST_PROTO_MAJOR,
            plugin: plugin_major,
        });
    }
    if plugin_minor > host_minor {
        return Err(HandshakeError::MinorAhead {
            host: host_minor,
            plugin: plugin_minor,
        });
    }
    Ok(NegotiatedPlugin {
        major: plugin_major,
        minor: plugin_minor,
        capabilities: capabilities.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_matching_major_and_equal_minor() {
        let n = validate(1, 0, 0, ["fast-fetch".to_string()]).unwrap();
        assert_eq!(n.major, 1);
        assert_eq!(n.minor, 0);
        assert!(n.has_capability("fast-fetch"));
    }

    #[test]
    fn rejects_wrong_major() {
        let err = validate(2, 0, 0, std::iter::empty()).unwrap_err();
        matches!(err, HandshakeError::MajorMismatch { .. });
    }

    #[test]
    fn rejects_plugin_minor_ahead_of_host() {
        let err = validate(1, 5, 0, std::iter::empty()).unwrap_err();
        matches!(err, HandshakeError::MinorAhead { .. });
    }

    #[test]
    fn accepts_plugin_minor_behind_host() {
        let n = validate(1, 0, 3, std::iter::empty()).unwrap();
        assert_eq!(n.minor, 0);
    }
}
