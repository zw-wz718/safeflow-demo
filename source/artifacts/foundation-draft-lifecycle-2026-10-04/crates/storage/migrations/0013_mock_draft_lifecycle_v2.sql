CREATE TABLE mock_draft_lifecycle_tasks (
    task_id TEXT PRIMARY KEY REFERENCES tasks(task_id),
    lifecycle_version SMALLINT NOT NULL CHECK (lifecycle_version=2),
    scope TEXT NOT NULL CHECK (scope='MOCK_DRAFT_LIFECYCLE_ONLY'),
    initial_task JSONB NOT NULL CHECK (jsonb_typeof(initial_task)='object'),
    initial_digest TEXT NOT NULL CHECK (initial_digest ~ '^[0-9a-f]{64}$'),
    registration_hash TEXT NOT NULL CHECK (registration_hash ~ '^[0-9a-f]{64}$'),
    active_operation TEXT NOT NULL,
    selection_ticket TEXT NOT NULL UNIQUE,
    last_success TEXT
);
CREATE TABLE mock_draft_lifecycle_plans (
    task_id TEXT NOT NULL REFERENCES mock_draft_lifecycle_tasks(task_id),
    plan_version BIGINT NOT NULL CHECK (plan_version BETWEEN 1 AND 4294967295),
    plan JSONB NOT NULL CHECK (jsonb_typeof(plan)='object' AND octet_length(plan::text)<=65536),
    plan_digest TEXT NOT NULL CHECK (plan_digest ~ '^[0-9a-f]{64}$'),
    registration_hash TEXT NOT NULL CHECK (registration_hash ~ '^[0-9a-f]{64}$'),
    completed_prefix JSONB NOT NULL CHECK (jsonb_typeof(completed_prefix)='array'),
    previous_version BIGINT,
    previous_digest TEXT,
    PRIMARY KEY(task_id,plan_version),
    FOREIGN KEY(task_id,previous_version) REFERENCES mock_draft_lifecycle_plans(task_id,plan_version),
    CHECK ((plan_version=1 AND previous_version IS NULL AND previous_digest IS NULL)
        OR (plan_version>1 AND previous_version IS NOT NULL AND previous_version=plan_version-1 AND previous_digest IS NOT NULL AND previous_digest ~ '^[0-9a-f]{64}$'))
);
CREATE TABLE mock_draft_lifecycle_actions (
    operation_id TEXT PRIMARY KEY REFERENCES operations(operation_id),
    task_id TEXT NOT NULL REFERENCES mock_draft_lifecycle_tasks(task_id),
    node_id TEXT NOT NULL,
    plan_version BIGINT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('CREATE_MOCK_DRAFT','MODIFY_MOCK_DRAFT','CANCEL_MOCK_DRAFT')),
    parameters JSONB NOT NULL CHECK (jsonb_typeof(parameters)='object' AND octet_length(parameters::text)<=4096),
    request_ticket TEXT NOT NULL,
    request JSONB NOT NULL CHECK (jsonb_typeof(request)='object'),
    request_digest TEXT NOT NULL CHECK (request_digest ~ '^[0-9a-f]{64}$'),
    observed_draft JSONB,
    archived BOOLEAN NOT NULL DEFAULT false,
    UNIQUE(task_id,node_id),
    UNIQUE(task_id,plan_version),
    UNIQUE(task_id,request_ticket),
    UNIQUE(task_id,operation_id),
    UNIQUE(task_id,operation_id,node_id,plan_version,action),
    FOREIGN KEY(task_id,plan_version) REFERENCES mock_draft_lifecycle_plans(task_id,plan_version),
    FOREIGN KEY(task_id,operation_id) REFERENCES operations(task_id,operation_id),
    CHECK ((action='CREATE_MOCK_DRAFT' AND observed_draft IS NULL) OR
        (action<>'CREATE_MOCK_DRAFT' AND observed_draft IS NOT NULL AND jsonb_typeof(observed_draft)='object'))
);
ALTER TABLE mock_draft_lifecycle_tasks ADD CONSTRAINT lifecycle_active_action_fk
    FOREIGN KEY(task_id,active_operation) REFERENCES mock_draft_lifecycle_actions(task_id,operation_id) DEFERRABLE INITIALLY DEFERRED;
ALTER TABLE mock_draft_lifecycle_tasks ADD CONSTRAINT lifecycle_last_success_fk
    FOREIGN KEY(task_id,last_success) REFERENCES mock_draft_lifecycle_actions(task_id,operation_id) DEFERRABLE INITIALLY DEFERRED;
CREATE TABLE mock_draft_lifecycle_displays (
    task_id TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    session_digest TEXT NOT NULL CHECK (session_digest ~ '^[0-9a-f]{64}$'),
    binding_version BIGINT NOT NULL CHECK (binding_version>0),
    lifecycle_anchor TEXT NOT NULL CHECK (lifecycle_anchor ~ '^[0-9a-f]{64}$'),
    ticket TEXT NOT NULL UNIQUE,
    pre_tool_request TEXT NOT NULL UNIQUE,
    pre_auth_request TEXT NOT NULL UNIQUE,
    pre_tool TEXT REFERENCES security_decisions(stored_decision_id),
    pre_auth TEXT REFERENCES security_decisions(stored_decision_id),
    confirmation_ref TEXT REFERENCES server_confirmations(confirmation_ref),
    display_digest TEXT CHECK (display_digest ~ '^[0-9a-f]{64}$'),
    display JSONB,
    PRIMARY KEY(operation_id,session_digest,binding_version),
    FOREIGN KEY(task_id,operation_id) REFERENCES mock_draft_lifecycle_actions(task_id,operation_id),
    CHECK ((pre_tool IS NULL AND pre_auth IS NULL AND confirmation_ref IS NULL AND display_digest IS NULL AND display IS NULL)
        OR (pre_tool IS NOT NULL AND pre_auth IS NOT NULL AND confirmation_ref IS NOT NULL AND display_digest IS NOT NULL AND display IS NOT NULL AND jsonb_typeof(display)='object'))
);
CREATE TABLE mock_draft_lifecycle_claims (
    operation_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    plan_version BIGINT NOT NULL,
    action TEXT NOT NULL,
    claim_id TEXT NOT NULL UNIQUE,
    source JSONB NOT NULL CHECK (jsonb_typeof(source)='object'),
    source_digest TEXT NOT NULL CHECK (source_digest ~ '^[0-9a-f]{64}$'),
    status TEXT NOT NULL CHECK (status IN ('CLAIMED','SUCCEEDED')),
    receipt JSONB,
    receipt_digest TEXT,
    UNIQUE(task_id,node_id),
    UNIQUE(task_id,plan_version),
    FOREIGN KEY(task_id,operation_id,node_id,plan_version,action) REFERENCES mock_draft_lifecycle_actions(task_id,operation_id,node_id,plan_version,action),
    CHECK ((status='CLAIMED' AND receipt IS NULL AND receipt_digest IS NULL)
        OR (status='SUCCEEDED' AND receipt IS NOT NULL AND jsonb_typeof(receipt)='object' AND receipt_digest IS NOT NULL AND receipt_digest ~ '^[0-9a-f]{64}$'))
);
CREATE UNIQUE INDEX lifecycle_one_unsettled ON mock_draft_lifecycle_claims(task_id) WHERE status='CLAIMED';
CREATE UNIQUE INDEX lifecycle_one_create ON mock_draft_lifecycle_claims(task_id) WHERE action='CREATE_MOCK_DRAFT';
ALTER TABLE server_confirmations ADD COLUMN lifecycle_anchor TEXT CHECK (lifecycle_anchor ~ '^[0-9a-f]{64}$');
ALTER TABLE security_decisions ADD COLUMN lifecycle_anchor TEXT CHECK (lifecycle_anchor ~ '^[0-9a-f]{64}$');
ALTER TABLE security_decisions DROP CONSTRAINT security_decisions_source_mode_check;
ALTER TABLE security_decisions ADD CONSTRAINT security_decisions_source_mode_check CHECK (
    (source_mode='TRUSTED_RUNTIME_V1' AND lifecycle_anchor IS NULL)
    OR (source_mode='TRUSTED_RUNTIME_LIFECYCLE_V2' AND lifecycle_anchor IS NOT NULL)
);
