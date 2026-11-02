-- Each accepted waiting revision can execute at most once. No lease or retry.
CREATE TABLE runtime_read_continuations (
    task_id TEXT NOT NULL REFERENCES runtime_read_claims(task_id),
    source_revision BIGINT NOT NULL CHECK (source_revision > 0),
    claim_id TEXT NOT NULL UNIQUE,
    contract_version SMALLINT NOT NULL CHECK (contract_version = 1),
    claim JSONB NOT NULL CHECK (jsonb_typeof(claim) = 'object'),
    status TEXT NOT NULL CHECK (status IN ('CLAIMED','SETTLED','FAILED')),
    receipt JSONB CHECK (receipt IS NULL OR jsonb_typeof(receipt) = 'object'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (task_id, source_revision),
    CHECK ((status = 'CLAIMED') = (receipt IS NULL))
);
