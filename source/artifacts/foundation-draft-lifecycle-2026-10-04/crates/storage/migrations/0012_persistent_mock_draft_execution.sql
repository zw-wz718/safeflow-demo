CREATE TABLE mock_draft_execution_claims (
    task_id TEXT PRIMARY KEY REFERENCES planning_confirmation_requests(task_id),
    operation_id TEXT NOT NULL UNIQUE REFERENCES operations(operation_id),
    claim_id TEXT NOT NULL UNIQUE,
    owner_id TEXT NOT NULL,
    source JSONB NOT NULL CHECK (jsonb_typeof(source) = 'object'),
    source_digest TEXT NOT NULL CHECK (source_digest ~ '^[0-9a-f]{64}$'),
    status TEXT NOT NULL CHECK (status IN ('CLAIMED', 'COMPLETED')),
    receipt JSONB,
    CHECK ((status='CLAIMED' AND receipt IS NULL) OR (status='COMPLETED' AND receipt IS NOT NULL))
);
