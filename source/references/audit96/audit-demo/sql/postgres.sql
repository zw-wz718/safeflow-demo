-- PostgreSQL receiver-schema proposal only; this file has not been applied.
-- Source fields remain nullable because C-AUD/T7 is not frozen. Foundation
-- Outbox source JSON is retained under details; no business mapping is inferred.
-- local-js-v1/v2 are local candidates, not the cross-language D-23 contract.
CREATE TABLE IF NOT EXISTS audit_records (
    sequence BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    audit_id UUID NOT NULL UNIQUE,
    event_id TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    task_id TEXT NOT NULL,
    node_id TEXT,
    operation_id TEXT,
    user_id TEXT,
    action TEXT,
    timestamp TIMESTAMPTZ,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    security_result TEXT,
    confirmation TEXT,
    auth_result TEXT,
    result TEXT,
    tool_name TEXT,
    details JSONB NOT NULL DEFAULT '{}'::jsonb,
    prev_hash CHAR(64),
    record_hash CHAR(64) NOT NULL,
    hash_version TEXT NOT NULL,
    event_fingerprint CHAR(64) NOT NULL,
    CONSTRAINT audit_hash_version CHECK (hash_version IN ('local-js-v1', 'local-js-v2')),
    CONSTRAINT audit_record_hash_format CHECK (record_hash ~ '^[0-9a-f]{64}$'),
    CONSTRAINT audit_prev_hash_format CHECK (prev_hash IS NULL OR prev_hash ~ '^[0-9a-f]{64}$')
);
CREATE INDEX IF NOT EXISTS idx_audit_task_sequence ON audit_records(task_id, sequence);
CREATE INDEX IF NOT EXISTS idx_audit_operation_sequence ON audit_records(operation_id, sequence);
