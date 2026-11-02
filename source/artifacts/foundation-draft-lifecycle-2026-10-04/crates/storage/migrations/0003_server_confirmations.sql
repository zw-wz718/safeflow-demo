-- Keep migrations 1/2 and their checksums unchanged.
ALTER TABLE audit_events DROP CONSTRAINT audit_events_kind_check;
ALTER TABLE audit_events ADD CONSTRAINT audit_events_kind_check CHECK (
    kind IN ('TASK_CREATED','CHECKPOINT_SAVED','PLAN_REPLACED','OPERATION_CREATED',
             'OPERATION_TRANSITIONED','CHANGE_REJECTED','CONFIRMATION_PREPARED',
             'EXPLICIT_CONFIRMATION_RECORDED','CONFIRMATION_REVOKED')
);

CREATE TABLE server_confirmations (
    confirmation_ref TEXT PRIMARY KEY CHECK (confirmation_ref ~ '^confirmation_[0-9a-f-]{36}$'),
    contract_version SMALLINT NOT NULL CHECK (contract_version = 1),
    owner_id TEXT NOT NULL CHECK (length(owner_id) BETWEEN 1 AND 200),
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    operation_id TEXT NOT NULL,
    node_id TEXT NOT NULL CHECK (length(node_id) BETWEEN 1 AND 200),
    tool_name TEXT NOT NULL CHECK (tool_name ~ '^[a-z0-9_]{1,200}$'),
    session_digest TEXT NOT NULL CHECK (session_digest ~ '^[0-9a-f]{64}$'),
    session_binding_version BIGINT NOT NULL CHECK (session_binding_version > 0),
    task_revision BIGINT NOT NULL CHECK (task_revision > 0),
    plan_version BIGINT NOT NULL CHECK (plan_version BETWEEN 1 AND 4294967295),
    operation_revision BIGINT NOT NULL CHECK (operation_revision > 0),
    facts_version BIGINT NOT NULL CHECK (facts_version > 0),
    arguments_hash TEXT NOT NULL CHECK (arguments_hash ~ '^[0-9a-f]{64}$'),
    binding_digest TEXT NOT NULL CHECK (binding_digest ~ '^[0-9a-f]{64}$'),
    status TEXT NOT NULL CHECK (status IN ('PREPARED','CONFIRMED','REVOKED')),
    prepared_at TIMESTAMPTZ NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL CHECK (expires_at > prepared_at),
    confirmed_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    CHECK ((status = 'PREPARED' AND confirmed_at IS NULL AND revoked_at IS NULL)
        OR (status = 'CONFIRMED' AND confirmed_at IS NOT NULL AND revoked_at IS NULL)
        OR (status = 'REVOKED' AND revoked_at IS NOT NULL)),
    CHECK (confirmed_at IS NULL OR (confirmed_at >= prepared_at AND confirmed_at < expires_at)),
    CHECK (revoked_at IS NULL OR revoked_at >= prepared_at),
    FOREIGN KEY (task_id, operation_id) REFERENCES operations(task_id, operation_id),
    UNIQUE (task_id, operation_id, session_digest, session_binding_version, binding_digest)
);
CREATE INDEX server_confirmations_task ON server_confirmations(task_id);
