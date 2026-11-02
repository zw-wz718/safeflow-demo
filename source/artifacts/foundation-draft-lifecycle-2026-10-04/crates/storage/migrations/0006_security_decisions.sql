ALTER TABLE audit_events DROP CONSTRAINT audit_events_kind_check;
ALTER TABLE audit_events ADD CONSTRAINT audit_events_kind_check CHECK (
    kind IN ('TASK_CREATED','CHECKPOINT_SAVED','PLAN_REPLACED','OPERATION_CREATED',
             'OPERATION_TRANSITIONED','CHANGE_REJECTED','CONFIRMATION_PREPARED',
             'EXPLICIT_CONFIRMATION_RECORDED','CONFIRMATION_REVOKED',
             'PERMIT_SAVED','PERMIT_CONSUMED','PERMIT_REVOKED',
             'SIM_AUTH_CHALLENGE_CREATED','SIM_AUTH_FAILED','SIM_AUTH_VERIFIED',
             'SIM_AUTH_GRANT_ISSUED','SIM_AUTH_CHALLENGE_REVOKED','SIM_AUTH_GRANT_REVOKED',
             'SECURITY_DECISION_RECORDED','SECURITY_DECISION_REVOKED','SECURITY_DECISION_INVALIDATED')
);
CREATE TABLE security_decisions (
    stored_decision_id TEXT PRIMARY KEY CHECK (stored_decision_id ~ '^stored_decision_[0-9a-f-]{36}$'),
    request_id TEXT NOT NULL UNIQUE CHECK (request_id ~ '^decision_request_[0-9a-f-]{36}$'),
    contract_version SMALLINT NOT NULL CHECK (contract_version=1),
    evidence_kind TEXT NOT NULL CHECK (evidence_kind='PREFLIGHT_ONLY'),
    source_mode TEXT NOT NULL CHECK (source_mode='TRUSTED_RUNTIME_V1'),
    source_instance_id TEXT NOT NULL CHECK (source_instance_id ~ '^[0-9a-f-]{36}$'),
    source_decision_id TEXT NOT NULL CHECK (length(source_decision_id) BETWEEN 1 AND 200),
    phase TEXT NOT NULL CHECK (phase IN ('PRE_TOOL','PRE_AUTH')),
    owner_id TEXT NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    operation_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    tool_name TEXT NOT NULL CHECK (length(tool_name) BETWEEN 1 AND 200),
    task_revision BIGINT NOT NULL CHECK (task_revision>0),
    plan_version BIGINT NOT NULL CHECK (plan_version BETWEEN 1 AND 4294967295),
    operation_revision BIGINT NOT NULL CHECK (operation_revision>0),
    facts_version BIGINT NOT NULL CHECK (facts_version BETWEEN 1 AND 4294967295),
    arguments_hash TEXT NOT NULL CHECK (arguments_hash ~ '^[0-9a-f]{64}$'),
    binding_digest TEXT NOT NULL CHECK (binding_digest ~ '^[0-9a-f]{64}$'),
    snapshot_digest TEXT NOT NULL CHECK (snapshot_digest ~ '^[0-9a-f]{64}$'),
    session_digest TEXT NOT NULL CHECK (session_digest ~ '^[0-9a-f]{64}$'),
    session_binding_version BIGINT NOT NULL CHECK (session_binding_version>0),
    assembly_digest TEXT NOT NULL CHECK (assembly_digest ~ '^[0-9a-f]{64}$'),
    assembly_metadata JSONB NOT NULL CHECK (jsonb_typeof(assembly_metadata)='object' AND octet_length(assembly_metadata::text)<=16384),
    decision_digest TEXT NOT NULL CHECK (decision_digest ~ '^[0-9a-f]{64}$'),
    decision JSONB NOT NULL CHECK (jsonb_typeof(decision)='object' AND octet_length(decision::text)<=16384),
    plan_review_digest TEXT NOT NULL CHECK (plan_review_digest ~ '^[0-9a-f]{64}$'),
    evaluated_at_ms BIGINT NOT NULL CHECK (evaluated_at_ms>=0),
    expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms>evaluated_at_ms),
    status TEXT NOT NULL CHECK (status IN ('ACTIVE','REVOKED','INVALIDATED')),
    ended_at_ms BIGINT,
    invalidation_reason TEXT CHECK (invalidation_reason IN ('SNAPSHOT_CHANGED','ASSEMBLY_CHANGED','EXPIRED')),
    CHECK ((status='ACTIVE') = (ended_at_ms IS NULL)),
    CHECK ((status='INVALIDATED') = (invalidation_reason IS NOT NULL)),
    CHECK (ended_at_ms IS NULL OR ended_at_ms>=evaluated_at_ms),
    FOREIGN KEY (task_id,operation_id) REFERENCES operations(task_id,operation_id),
    UNIQUE (source_instance_id,source_decision_id)
);
CREATE INDEX security_decisions_task ON security_decisions(task_id);
CREATE TABLE security_decision_time_fence (
    scope SMALLINT PRIMARY KEY CHECK (scope=1),
    last_observed_ms BIGINT NOT NULL CHECK (last_observed_ms>=0)
);
