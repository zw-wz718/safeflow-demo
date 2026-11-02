-- Permanent per-task claim. No lease expiry or automatic retry after interruption.
CREATE TABLE runtime_read_claims (
    task_id TEXT PRIMARY KEY REFERENCES tasks(task_id),
    claim_id TEXT NOT NULL UNIQUE,
    contract_version SMALLINT NOT NULL CHECK (contract_version = 1),
    initial_task JSONB NOT NULL CHECK (jsonb_typeof(initial_task) = 'object'),
    session_hash TEXT NOT NULL CHECK (session_hash ~ '^[0-9a-f]{64}$'),
    session_binding JSONB NOT NULL CHECK (jsonb_typeof(session_binding) = 'object'),
    bindings_hash TEXT NOT NULL CHECK (bindings_hash ~ '^[0-9a-f]{64}$'),
    status TEXT NOT NULL CHECK (status IN ('CLAIMED','SETTLED','FAILED')),
    receipt JSONB CHECK (receipt IS NULL OR jsonb_typeof(receipt) = 'object'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CHECK ((status = 'CLAIMED') = (receipt IS NULL))
);
