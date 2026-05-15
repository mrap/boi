//! Rendezvous (HRW) ranking + capability filtering.
//!
//! Per design §7 (Task assignment algorithm) and critique F-01:
//! HRW gives deterministic load distribution; the claim CAS in
//! `assign.rs` provides correctness. Capability filter narrows the
//! candidate set to nodes whose advertised caps satisfy the task's
//! `requires` clause (exact key=value match, set semantics).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use boi_cluster::nodes::{NodeCaps, NodeRecord};

/// View of a node used for assignment: identity joined with caps.
///
/// `boi-assign` owns this type so the assignment plane can reason
/// about identity + caps as one unit. Construct via [`AssignNode::new`]
/// when joining a `MembershipSnapshot` against a caps lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignNode {
    pub node: NodeRecord,
    pub caps: NodeCaps,
}

impl AssignNode {
    pub fn new(node: NodeRecord, caps: NodeCaps) -> Self {
        Self { node, caps }
    }

    pub fn id(&self) -> &str {
        &self.node.node_id
    }

    /// Resolve a cap key by checking dynamic first (operator overrides)
    /// then static. Mirrors the lookup order the design doc uses.
    pub fn cap(&self, key: &str) -> Option<&str> {
        self.caps
            .dynamic
            .get(key)
            .or_else(|| self.caps.r#static.get(key))
            .map(String::as_str)
    }
}

/// Task-level capability requirement: each entry is an exact match
/// against the node's advertised caps. Set semantics — every entry
/// must be satisfied for the node to be a candidate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapRequires {
    #[serde(default)]
    pub entries: BTreeMap<String, String>,
}

impl CapRequires {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.entries.insert(key.into(), value.into());
        self
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Rendezvous hash for one (task, node) pair.
///
/// Uses SHA-256 over `task_id || 0x1f || node_id` and folds the first
/// 8 bytes into a `u64`. SHA-256 is overkill for HRW but matches the
/// rest of the codebase's hashing dependency and is stable across
/// platforms — what HRW requires above all is a fixed, well-mixed
/// function.
fn hrw_hash(task_id: &str, node_id: &str) -> u64 {
    let mut h = Sha256::new();
    h.update(task_id.as_bytes());
    h.update([0x1f]);
    h.update(node_id.as_bytes());
    let digest = h.finalize();
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(buf)
}

/// Rank candidate nodes for a task by descending HRW score.
///
/// Ties (vanishingly unlikely with SHA-256) break by `node_id` so the
/// output is deterministic. Returns each node's `node_id` in priority
/// order — highest score first.
pub fn hrw_rank(task_id: &str, nodes: &[AssignNode]) -> Vec<String> {
    let mut scored: Vec<(u64, &str)> = nodes
        .iter()
        .map(|n| (hrw_hash(task_id, n.id()), n.id()))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().map(|(_, id)| id.to_string()).collect()
}

/// Filter nodes whose advertised caps satisfy `requires`. Empty
/// `requires` returns all nodes. Skips nodes flagged
/// `caps.dynamic.health=degraded` — the cooldown mechanism in F-06
/// uses this flag to take a flapping node out of rotation without
/// removing it from membership.
pub fn capability_filter(nodes: &[AssignNode], requires: &CapRequires) -> Vec<AssignNode> {
    nodes
        .iter()
        .filter(|n| {
            if n.caps.dynamic.get("health").map(String::as_str) == Some("degraded") {
                return false;
            }
            requires
                .entries
                .iter()
                .all(|(k, v)| n.cap(k) == Some(v.as_str()))
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_node(id: &str, static_caps: &[(&str, &str)]) -> AssignNode {
        let mut caps = NodeCaps::default();
        for (k, v) in static_caps {
            caps.r#static.insert((*k).into(), (*v).into());
        }
        AssignNode::new(
            NodeRecord {
                node_id: id.into(),
                addr: format!("127.0.0.1:{}", 7000 + id.len()),
                version: "0.1.0".into(),
                started_at: 1_700_000_000,
            },
            caps,
        )
    }

    fn mk_nodes(n: usize) -> Vec<AssignNode> {
        (0..n).map(|i| mk_node(&format!("node-{i}"), &[])).collect()
    }

    #[test]
    fn hrw_is_deterministic() {
        let nodes = mk_nodes(5);
        let a = hrw_rank("task-42", &nodes);
        let b = hrw_rank("task-42", &nodes);
        assert_eq!(a, b);
        assert_eq!(a.len(), 5);
    }

    #[test]
    fn hrw_ranking_independent_of_input_order() {
        let mut a = mk_nodes(5);
        let mut b = a.clone();
        b.reverse();
        assert_eq!(hrw_rank("task-x", &a), hrw_rank("task-x", &b));
        a.swap(0, 4);
        assert_eq!(hrw_rank("task-x", &a), hrw_rank("task-x", &b));
    }

    #[test]
    fn hrw_distributes_evenly_across_nodes() {
        let nodes = mk_nodes(5);
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for i in 0..100 {
            let task_id = format!("task-{i}");
            let top = hrw_rank(&task_id, &nodes).into_iter().next().unwrap();
            *counts.entry(top).or_default() += 1;
        }
        // Each of 5 nodes should win ~20 times. Allow wide slack so the
        // test is stable; the point is "no node dominates".
        for (id, c) in &counts {
            assert!(
                (5..=40).contains(c),
                "node {id} won {c}/100 — expected ~20",
            );
        }
        let total: usize = counts.values().sum();
        assert_eq!(total, 100);
        assert_eq!(counts.len(), 5);
    }

    #[test]
    fn capability_filter_excludes_mismatched_os() {
        let nodes = vec![
            mk_node("mac-1", &[("os", "mac")]),
            mk_node("linux-1", &[("os", "linux")]),
            mk_node("mac-2", &[("os", "mac")]),
        ];
        let req = CapRequires::new().with("os", "mac");
        let got: Vec<String> = capability_filter(&nodes, &req)
            .into_iter()
            .map(|n| n.id().to_string())
            .collect();
        assert_eq!(got, vec!["mac-1".to_string(), "mac-2".to_string()]);
    }

    #[test]
    fn empty_requires_returns_all() {
        let nodes = mk_nodes(3);
        let got = capability_filter(&nodes, &CapRequires::new());
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn multiple_required_caps_are_anded() {
        let nodes = vec![
            mk_node("a", &[("os", "mac"), ("arch", "arm64")]),
            mk_node("b", &[("os", "mac"), ("arch", "x86_64")]),
            mk_node("c", &[("os", "linux"), ("arch", "arm64")]),
        ];
        let req = CapRequires::new().with("os", "mac").with("arch", "arm64");
        let got: Vec<String> = capability_filter(&nodes, &req)
            .into_iter()
            .map(|n| n.id().to_string())
            .collect();
        assert_eq!(got, vec!["a".to_string()]);
    }

    #[test]
    fn degraded_health_excludes_node_from_filter() {
        let mut nodes = vec![
            mk_node("a", &[("os", "mac")]),
            mk_node("b", &[("os", "mac")]),
        ];
        nodes[0]
            .caps
            .dynamic
            .insert("health".into(), "degraded".into());
        let req = CapRequires::new().with("os", "mac");
        let got: Vec<String> = capability_filter(&nodes, &req)
            .into_iter()
            .map(|n| n.id().to_string())
            .collect();
        assert_eq!(got, vec!["b".to_string()]);
    }

    #[test]
    fn dynamic_cap_overrides_static_for_match() {
        let mut node = mk_node("a", &[("os", "mac")]);
        node.caps.dynamic.insert("os".into(), "linux".into());
        let req = CapRequires::new().with("os", "linux");
        let got = capability_filter(&[node], &req);
        assert_eq!(got.len(), 1);
    }
}
