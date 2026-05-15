//! Typed client for the hooks plugin.
//!
//! Per Q6 the host distinguishes between best-effort and audit
//! deliveries when calling `Emit`: best-effort events MAY be dropped
//! under backpressure; audit events block the producer until the
//! plugin acks the `sequence`.

use boi_proto::hooks::v1 as pb;
pub use pb::hooks_client::HooksClient;
pub use pb::{DeliveryTier, EmitRequest, EmitResponse, HandshakeRequest, HandshakeResponse};

pub struct HooksPlugin<T> {
    pub inner: HooksClient<T>,
}

impl<T> HooksPlugin<T> {
    pub fn new(inner: HooksClient<T>) -> Self {
        Self { inner }
    }
}

/// Returns true if this tier requires durable persistence before
/// the host acks the producer.
pub fn requires_durability(tier: DeliveryTier) -> bool {
    matches!(tier, DeliveryTier::Audit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_is_durable_best_effort_is_not() {
        assert!(requires_durability(DeliveryTier::Audit));
        assert!(!requires_durability(DeliveryTier::BestEffort));
        assert!(!requires_durability(DeliveryTier::Unspecified));
    }
}
