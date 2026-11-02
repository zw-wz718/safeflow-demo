-- Unified standalone fictional subscription runtime, NOT a public migration.
CREATE SCHEMA IF NOT EXISTS safeflow_subscription_mock_v1;
CREATE TABLE IF NOT EXISTS safeflow_subscription_mock_v1.runtime_recovery (
    owner_id TEXT PRIMARY KEY CHECK (length(btrim(owner_id)) > 0),
    version BIGINT NOT NULL CHECK (version > 0),
    journal JSONB NOT NULL,
    CHECK (journal @> '{"schema_version":1,"fictional":true,"production_registered":false}'::jsonb),
    CHECK ((journal -> 'initial' ->> 'owner_user_id') IS NOT DISTINCT FROM owner_id),
    CHECK ((jsonb_typeof(journal -> 'commands') = 'array') IS TRUE),
    CHECK (version = jsonb_array_length(journal -> 'commands') + 1)
);
