//! The spec-picker screen's data model.
//!
//! `boi dashboard` with no `SPEC_ID` opens a list of specs — running first,
//! then the most recent terminal specs. Each row is a [`SpecSummary`].

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::repo::phase_runs::SpecPhaseRollup;
use crate::repo::spec_runtime::SpecRuntimeRow;

/// How many terminal (completed/failed/canceled) specs the picker shows.
/// Running specs are always all shown; terminal specs are capped to the most
/// recent `MAX_TERMINAL`.
pub const MAX_TERMINAL: usize = 20;

/// One row of the spec picker.
#[derive(Debug, Clone, PartialEq)]
pub struct SpecSummary {
    /// The spec id.
    pub spec_id: String,
    /// The spec's human-readable title, sourced from the latest
    /// `spec_versions.snapshot.title`. `None` if no snapshot has been written
    /// or the snapshot omits a title — the renderer falls back to showing
    /// only the ID in that case.
    pub title: Option<String>,
    /// Lifecycle status (`running` / `completed` / `failed` / `canceled` / `queued`).
    pub status: String,
    /// When the spec started — `None` if not yet started.
    pub started_at: Option<DateTime<Utc>>,
    /// When it reached a terminal status — `None` while live.
    pub completed_at: Option<DateTime<Utc>>,
    /// Phase-run count.
    pub phase_count: i64,
}

impl SpecSummary {
    /// Whether the spec is still running (or queued) — i.e. not terminal.
    pub fn is_live(&self) -> bool {
        matches!(self.status.as_str(), "running" | "queued")
    }
}

/// Build the ordered picker list from a `spec_runtime` snapshot and the
/// grouped `phase_runs` rollup.
///
/// Order: all live (running/queued) specs first — newest start first — then
/// terminal specs newest-completed first, capped at [`MAX_TERMINAL`].
pub fn build_spec_list(specs: &[SpecRuntimeRow], rollups: &[SpecPhaseRollup]) -> Vec<SpecSummary> {
    let by_spec: HashMap<&str, &SpecPhaseRollup> =
        rollups.iter().map(|r| (r.spec_id.as_str(), r)).collect();

    let mut summaries: Vec<SpecSummary> = specs
        .iter()
        .map(|s| {
            let roll = by_spec.get(s.spec_id.as_str());
            SpecSummary {
                spec_id: s.spec_id.clone(),
                // Spec runtime rows do not carry the title; callers that have
                // a snapshot can fill this in via [`enrich_with_titles`].
                title: None,
                status: s.status.clone(),
                started_at: s.started_at,
                completed_at: s.completed_at,
                phase_count: roll.map_or(0, |r| r.phase_count),
            }
        })
        .collect();

    let (mut live, mut terminal): (Vec<_>, Vec<_>) =
        summaries.drain(..).partition(SpecSummary::is_live);

    // Live: newest start first.
    live.sort_by_key(|b| std::cmp::Reverse(b.started_at));
    // Terminal: newest completion first, capped.
    terminal.sort_by_key(|b| std::cmp::Reverse(b.completed_at));
    terminal.truncate(MAX_TERMINAL);

    live.into_iter().chain(terminal).collect()
}

/// Backfill `SpecSummary.title` for any row whose `spec_id` matches a key in
/// `titles`. Rows whose id is absent from the map keep their existing `title`
/// (usually `None`). Rows that already carry a title are left untouched.
///
/// The caller produces the map by fetching `spec_versions.snapshot.title` for
/// the spec ids that landed in the picker — see `poll::build_spec_list`.
/// Keeping the title lookup out of [`build_spec_list`] avoids forcing a
/// SQL-touching dependency into the pure layer.
pub fn enrich_with_titles<S: AsRef<str>, T: AsRef<str>>(
    summaries: &mut [SpecSummary],
    titles: &HashMap<S, T>,
) {
    for s in summaries.iter_mut() {
        if s.title.is_some() {
            continue;
        }
        if let Some(t) = titles
            .iter()
            .find(|(k, _)| k.as_ref() == s.spec_id)
            .map(|(_, v)| v.as_ref().to_string())
        {
            s.title = Some(t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::Value;

    fn spec(id: &str, status: &str, start: i64) -> SpecRuntimeRow {
        SpecRuntimeRow {
            spec_id: id.to_string(),
            current_version: 1,
            status: status.to_string(),
            failure_reason: None::<Value>,
            cancellation_reason: None::<Value>,
            started_at: Some(Utc.timestamp_opt(start, 0).unwrap()),
            completed_at: if status == "running" {
                None
            } else {
                Some(Utc.timestamp_opt(start + 100, 0).unwrap())
            },
            iterations_plan_critique: 0,
            iterations_spec_review: 0,
        }
    }

    #[test]
    fn build_spec_list_puts_running_specs_first() {
        let specs = vec![
            spec("S0000001a", "completed", 100),
            spec("S0000002b", "running", 50),
        ];
        let list = build_spec_list(&specs, &[]);
        assert_eq!(list[0].spec_id, "S0000002b", "running spec leads");
        assert_eq!(list[1].spec_id, "S0000001a");
        assert_eq!(list[0].phase_count, 0, "no rollup => zero");
        // Titles default to None; `enrich_with_titles` is the join point.
        assert!(list[0].title.is_none());
        assert!(list[1].title.is_none());
    }

    #[test]
    fn enrich_with_titles_backfills_known_specs_only() {
        let mut list = build_spec_list(
            &[
                spec("S0000001a", "completed", 100),
                spec("S0000002b", "running", 50),
            ],
            &[],
        );
        let mut titles: HashMap<&str, &str> = HashMap::new();
        titles.insert("S0000002b", "Add login flow");
        enrich_with_titles(&mut list, &titles);
        let by_id: HashMap<_, _> = list.iter().map(|s| (s.spec_id.as_str(), s)).collect();
        assert_eq!(
            by_id["S0000002b"].title.as_deref(),
            Some("Add login flow"),
            "matching spec gets its title",
        );
        assert!(
            by_id["S0000001a"].title.is_none(),
            "unmapped spec keeps None",
        );
    }

    #[test]
    fn enrich_with_titles_preserves_existing_titles() {
        let mut list = build_spec_list(&[spec("S0000003c", "running", 1)], &[]);
        list[0].title = Some("pre-set".into());
        let mut titles: HashMap<&str, &str> = HashMap::new();
        titles.insert("S0000003c", "from-snapshot");
        enrich_with_titles(&mut list, &titles);
        assert_eq!(
            list[0].title.as_deref(),
            Some("pre-set"),
            "a caller-set title is not overwritten",
        );
    }
}
