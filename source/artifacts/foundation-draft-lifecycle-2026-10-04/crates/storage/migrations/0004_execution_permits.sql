ALTER TABLE audit_events DROP CONSTRAINT audit_events_kind_check;
ALTER TABLE audit_events ADD CONSTRAINT audit_events_kind_check CHECK (
    kind IN ('TASK_CREATED','CHECKPOINT_SAVED','PLAN_REPLACED','OPERATION_CREATED',
             'OPERATION_TRANSITIONED','CHANGE_REJECTED','CONFIRMATION_PREPARED',
             'EXPLICIT_CONFIRMATION_RECORDED','CONFIRMATION_REVOKED',
             'PERMIT_SAVED','PERMIT_CONSUMED','PERMIT_REVOKED')
);
CREATE TABLE execution_permits (
    permit_id TEXT PRIMARY KEY CHECK (permit_id ~ '^permit_[0-9a-f-]{36}$'),
    contract_version SMALLINT NOT NULL CHECK (contract_version=1),
    decision_id TEXT NOT NULL CHECK (length(decision_id) BETWEEN 1 AND 200),
    owner_id TEXT NOT NULL CHECK (length(owner_id) BETWEEN 1 AND 200),
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    operation_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    task_revision BIGINT NOT NULL CHECK (task_revision>0),
    plan_version BIGINT NOT NULL CHECK (plan_version BETWEEN 1 AND 4294967295),
    operation_revision BIGINT NOT NULL CHECK (operation_revision>0),
    facts_version BIGINT NOT NULL CHECK (facts_version BETWEEN 1 AND 4294967295),
    arguments_hash TEXT NOT NULL CHECK (arguments_hash ~ '^[0-9a-f]{64}$'),
    binding_digest TEXT NOT NULL CHECK (binding_digest ~ '^[0-9a-f]{64}$'),
    operation_fingerprint TEXT NOT NULL CHECK (operation_fingerprint ~ '^[0-9a-f]{64}$'),
    fingerprint_payload JSONB NOT NULL CHECK (jsonb_typeof(fingerprint_payload)='object'),
    session_digest TEXT NOT NULL CHECK (session_digest ~ '^[0-9a-f]{64}$'),
    session_binding_version BIGINT NOT NULL CHECK (session_binding_version>0),
    confirmation_ref TEXT NOT NULL REFERENCES server_confirmations(confirmation_ref),
    auth_grant_ref TEXT NOT NULL CHECK (length(auth_grant_ref) BETWEEN 1 AND 200),
    issued_at_ms BIGINT NOT NULL CHECK (issued_at_ms>=0),
    expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms>issued_at_ms),
    status TEXT NOT NULL CHECK (status IN ('ACTIVE','CONSUMED','REVOKED','EXPIRED')),
    consumed_at_ms BIGINT,
    CHECK ((status='CONSUMED') = (consumed_at_ms IS NOT NULL)),
    CHECK (consumed_at_ms IS NULL OR (consumed_at_ms>=issued_at_ms AND consumed_at_ms<expires_at_ms)),
    FOREIGN KEY (task_id,operation_id) REFERENCES operations(task_id,operation_id),
    -- This minimum slice retains a tombstone: it never reissues for the same fact version.
    UNIQUE (operation_id,facts_version)
);
CREATE INDEX execution_permits_task ON execution_permits(task_id);
