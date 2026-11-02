-- Independent fictional local test schema, NOT the shared migration chain.
-- Only snapshots are stored. No business operations, grants or audit records.
CREATE SCHEMA IF NOT EXISTS safeflow_subscription_mock_v1;
CREATE TABLE IF NOT EXISTS safeflow_subscription_mock_v1.ledger_snapshots (
    owner_id TEXT NOT NULL CHECK (length(btrim(owner_id)) > 0),
    snapshot_id TEXT NOT NULL CHECK (length(btrim(snapshot_id)) > 0),
    snapshot JSONB NOT NULL,
    PRIMARY KEY (owner_id, snapshot_id),
    CHECK (snapshot @> '{"schema_version":1,"fictional":true,"production_registered":false}'::jsonb),
    CHECK ((snapshot ->> 'owner_user_id') IS NOT DISTINCT FROM owner_id),
    CHECK ((snapshot ->> 'snapshot_id') IS NOT DISTINCT FROM snapshot_id)
);
