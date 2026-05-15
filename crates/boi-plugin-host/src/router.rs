//! Typed client for the router plugin.
//!
//! Default behavior is passthrough: when the plugin returns an empty
//! `chosen_node_id` or the RPC fails, callers fall back to the first
//! candidate they supplied. The passthrough helper below encodes that
//! contract so call sites can stay terse.

use boi_proto::router::v1 as pb;
pub use pb::router_client::RouterClient;
pub use pb::{HandshakeRequest, HandshakeResponse, RouteRequest, RouteResponse};

pub struct RouterPlugin<T> {
    pub inner: RouterClient<T>,
}

impl<T> RouterPlugin<T> {
    pub fn new(inner: RouterClient<T>) -> Self {
        Self { inner }
    }
}

/// Passthrough fallback — pick the first candidate.
pub fn passthrough_default<'a>(candidates: &'a [String]) -> Option<&'a String> {
    candidates.first()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_picks_first() {
        let c = vec!["a".to_string(), "b".to_string()];
        assert_eq!(passthrough_default(&c).unwrap(), "a");
        let empty: Vec<String> = vec![];
        assert!(passthrough_default(&empty).is_none());
    }
}
