CREATE TABLE tasks (
    task_id TEXT PRIMARY KEY CHECK (length(task_id) BETWEEN 1 AND 200),
    owner_id TEXT NOT NULL CHECK (length(owner_id) BETWEEN 1 AND 200),
    contract_version SMALLINT NOT NULL CHECK (contract_version = 1),
    state TEXT NOT NULL CHECK (state IN ('RUNNING','WAITING_USER','WAITING_EXTERNAL','COMPLETED','FAILED','CANCELLED')),
    revision BIGINT NOT NULL CHECK (revision > 0),
    plan_version BIGINT NOT NULL CHECK (plan_version > 0 AND plan_version <= 4294967295),
    facts_version BIGINT NOT NULL CHECK (facts_version > 0),
    plan JSONB NOT NULL CHECK (jsonb_typeof(plan) = 'object'
        AND plan->>'task_id' = task_id AND (plan->>'plan_version')::bigint = plan_version),
    creation_digest TEXT NOT NULL CHECK (creation_digest ~ '^[0-9a-f]{64}$'),
    checkpoint JSONB NOT NULL CHECK (jsonb_typeof(checkpoint) = 'object'),
    completed_nodes JSONB NOT NULL CHECK (jsonb_typeof(completed_nodes) = 'array'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE operations (
    operation_id TEXT PRIMARY KEY CHECK (length(operation_id) BETWEEN 1 AND 200),
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    node_id TEXT NOT NULL CHECK (length(node_id) BETWEEN 1 AND 200),
    state TEXT NOT NULL CHECK (state IN ('DRAFT','READY','SUBMITTED','PROCESSING','SUCCEEDED','FAILED','CANCELLED')),
    revision BIGINT NOT NULL CHECK (revision > 0),
    plan_version BIGINT NOT NULL CHECK (plan_version > 0 AND plan_version <= 4294967295),
    facts_version BIGINT NOT NULL CHECK (facts_version > 0),
    arguments_hash TEXT NOT NULL CHECK (arguments_hash ~ '^[0-9a-f]{64}$'),
    business_operation_ref TEXT CHECK (length(business_operation_ref) BETWEEN 1 AND 200),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (task_id, operation_id),
    UNIQUE (task_id, node_id),
    CHECK (state NOT IN ('SUBMITTED','PROCESSING','SUCCEEDED','FAILED') OR business_operation_ref IS NOT NULL)
);

CREATE TABLE audit_events (
    sequence BIGSERIAL PRIMARY KEY,
    event_id TEXT NOT NULL UNIQUE,
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    operation_id TEXT,
    kind TEXT NOT NULL CHECK (kind IN ('TASK_CREATED','CHECKPOINT_SAVED','PLAN_REPLACED','OPERATION_CREATED','OPERATION_TRANSITIONED','CHANGE_REJECTED')),
    task_revision BIGINT NOT NULL CHECK (task_revision > 0),
    operation_revision BIGINT CHECK (operation_revision > 0),
    details JSONB NOT NULL CHECK (jsonb_typeof(details) = 'object'),
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    FOREIGN KEY (task_id, operation_id) REFERENCES operations(task_id, operation_id)
);
CREATE INDEX audit_events_task_sequence ON audit_events(task_id, sequence);
CREATE INDEX operations_task ON operations(task_id);
