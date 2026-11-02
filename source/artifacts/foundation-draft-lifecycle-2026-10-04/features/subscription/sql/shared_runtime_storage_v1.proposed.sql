-- REVIEW TEMPLATE ONLY. Not a public migration; not referenced by any runner.
-- The block is deliberately commented and has an unresolved approved-schema name.
-- Echo owns the migration number, final codec, owner binding, permissions and DDL.
/*
CREATE TABLE __APPROVED_SCHEMA__.subscription_runtime_records (
    owner_id TEXT PRIMARY KEY CHECK (length(btrim(owner_id)) > 0),
    storage_version BIGINT NOT NULL CHECK (storage_version > 0),
    encoding_version TEXT NOT NULL CHECK (length(btrim(encoding_version)) > 0),
    journal JSONB NOT NULL,
    CHECK ((jsonb_typeof(journal) = 'object') IS TRUE),
    CHECK ((jsonb_typeof(journal -> 'commands') = 'array') IS TRUE),
    CHECK (storage_version = jsonb_array_length(journal -> 'commands') + 1),
    CHECK ((journal -> 'initial' ->> 'owner_user_id') IS NOT DISTINCT FROM owner_id)
);
-- Encoding allow-list and authenticated owner reference are not decided here.
-- Before enabling a shared codec, review validation, access control, immutable
-- original operations and journal-prefix protection described in STORAGE_HANDOFF.
-- This template must NOT be applied by deleting fictional flags from mock data.
*/
