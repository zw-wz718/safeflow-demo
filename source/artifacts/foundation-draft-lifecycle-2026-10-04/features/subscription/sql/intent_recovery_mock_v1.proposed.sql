-- Dedicated fictional simulation only, not the shared migration chain.
CREATE SCHEMA IF NOT EXISTS safeflow_subscription_mock_v1;
CREATE TABLE IF NOT EXISTS safeflow_subscription_mock_v1.intent_recovery (
    owner_id TEXT PRIMARY KEY CHECK (length(btrim(owner_id)) > 0),
    version BIGINT NOT NULL CHECK (version > 0),
    checkpoint JSONB NOT NULL,
    CHECK (checkpoint @> '{"schema_version":1,"fictional":true,"production_registered":false}'::jsonb),
    CHECK ((checkpoint -> 'ledger' ->> 'owner_user_id') IS NOT DISTINCT FROM owner_id)
);
