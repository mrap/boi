-- BOI v2 schema amendment 0003 — drop `phase_runs.cost_usd`.
--
-- Per the 2026-06-01 directive: "Strip $ everywhere, keep tokens
-- everywhere." The per-run token-count columns (added in 0002) stay
-- untouched; only the per-run dollar cost gets ripped out of the
-- schema, alongside the pricing module that produced it.
--
-- SQLite >= 3.35 supports `ALTER TABLE ... DROP COLUMN` directly,
-- with no need for the create-new-table / copy-rows dance that
-- earlier versions required. Confirmed sqlite3 3.51.0 on this host;
-- the libsqlite3-sys that sqlx bundles is well past 3.35 as well.
--
-- Forward-only (see 0001's header). Adding a 0004 to bring this
-- column back is the supported reversal path — never edit this file.

ALTER TABLE phase_runs DROP COLUMN cost_usd;
