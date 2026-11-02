ALTER TABLE audit_events DROP CONSTRAINT audit_events_kind_check;
ALTER TABLE audit_events ADD CONSTRAINT audit_events_kind_check CHECK (
    kind IN ('TASK_CREATED','CHECKPOINT_SAVED','PLAN_REPLACED','OPERATION_CREATED',
             'OPERATION_TRANSITIONED','CHANGE_REJECTED','CONFIRMATION_PREPARED',
             'EXPLICIT_CONFIRMATION_RECORDED','CONFIRMATION_REVOKED',
             'PERMIT_SAVED','PERMIT_CONSUMED','PERMIT_REVOKED',
             'SIM_AUTH_CHALLENGE_CREATED','SIM_AUTH_FAILED','SIM_AUTH_VERIFIED',
             'SIM_AUTH_GRANT_ISSUED','SIM_AUTH_CHALLENGE_REVOKED','SIM_AUTH_GRANT_REVOKED')
);
CREATE TABLE simulation_auth_challenges (
    challenge_id TEXT PRIMARY KEY,
    contract_version SMALLINT NOT NULL CHECK (contract_version=1),
    mode TEXT NOT NULL CHECK (mode='FOUNDATION_ISOLATED_V1'),
    owner_id TEXT NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    operation_id TEXT NOT NULL UNIQUE,
    node_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    task_revision BIGINT NOT NULL CHECK (task_revision>0),
    plan_version BIGINT NOT NULL CHECK (plan_version BETWEEN 1 AND 4294967295),
    operation_revision BIGINT NOT NULL CHECK (operation_revision>0),
    facts_version BIGINT NOT NULL CHECK (facts_version>0),
    arguments_hash TEXT NOT NULL CHECK (arguments_hash ~ '^[0-9a-f]{64}$'),
    binding_digest TEXT NOT NULL CHECK (binding_digest ~ '^[0-9a-f]{64}$'),
    session_digest TEXT NOT NULL CHECK (session_digest ~ '^[0-9a-f]{64}$'),
    session_binding_version BIGINT NOT NULL CHECK (session_binding_version>0),
    confirmation_ref TEXT NOT NULL REFERENCES server_confirmations(confirmation_ref),
    secret_digest TEXT NOT NULL CHECK (secret_digest ~ '^[0-9a-f]{64}$'),
    secret_bytes INTEGER NOT NULL CHECK (secret_bytes BETWEEN 16 AND 64),
    max_failures INTEGER NOT NULL CHECK (max_failures BETWEEN 1 AND 100),
    failures INTEGER NOT NULL CHECK (failures BETWEEN 0 AND max_failures),
    challenge_ttl_ms BIGINT NOT NULL CHECK (challenge_ttl_ms>0),
    grant_ttl_ms BIGINT NOT NULL CHECK (grant_ttl_ms>0),
    created_at_ms BIGINT NOT NULL CHECK (created_at_ms>=0),
    expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms>created_at_ms),
    status TEXT NOT NULL CHECK (status IN ('PENDING','SUCCEEDED','FAILED','REVOKED')),
    completed_at_ms BIGINT,
    revoked_at_ms BIGINT,
    CHECK ((status='REVOKED') = (revoked_at_ms IS NOT NULL)),
    CHECK ((status NOT IN ('SUCCEEDED','FAILED') OR completed_at_ms IS NOT NULL)
        AND (status<>'PENDING' OR completed_at_ms IS NULL)),
    CHECK (completed_at_ms IS NULL OR (completed_at_ms>=created_at_ms AND completed_at_ms<expires_at_ms)),
    CHECK (revoked_at_ms IS NULL OR revoked_at_ms>=created_at_ms),
    CHECK ((status<>'FAILED' OR failures=max_failures) AND (status NOT IN ('PENDING','SUCCEEDED') OR failures<max_failures)),
    FOREIGN KEY (task_id,operation_id) REFERENCES operations(task_id,operation_id)
);
CREATE TABLE simulation_auth_grants (
    grant_id TEXT PRIMARY KEY,
    contract_version SMALLINT NOT NULL CHECK (contract_version=1),
    mode TEXT NOT NULL CHECK (mode='FOUNDATION_ISOLATED_V1'),
    challenge_id TEXT NOT NULL UNIQUE REFERENCES simulation_auth_challenges(challenge_id),
    issued_at_ms BIGINT NOT NULL CHECK (issued_at_ms>=0),
    expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms>issued_at_ms),
    status TEXT NOT NULL CHECK (status IN ('ACTIVE','REVOKED')),
    revoked_at_ms BIGINT,
    CHECK ((status='REVOKED') = (revoked_at_ms IS NOT NULL)),
    CHECK (revoked_at_ms IS NULL OR revoked_at_ms>=issued_at_ms)
);
CREATE TABLE simulation_auth_submissions (
    submission_id TEXT PRIMARY KEY,
    challenge_id TEXT NOT NULL REFERENCES simulation_auth_challenges(challenge_id),
    input_digest TEXT NOT NULL CHECK (input_digest ~ '^[0-9a-f]{64}$'),
    result TEXT NOT NULL CHECK (result IN ('VERIFIED','NOT_VERIFIED')),
    failures INTEGER NOT NULL CHECK (failures BETWEEN 0 AND 100),
    grant_id TEXT REFERENCES simulation_auth_grants(grant_id),
    submitted_at_ms BIGINT NOT NULL CHECK (submitted_at_ms>=0),
    CHECK ((result='VERIFIED') = (grant_id IS NOT NULL))
);
CREATE INDEX simulation_auth_challenges_task ON simulation_auth_challenges(task_id);

-- Independent, short transactions record trusted time even when authorization rolls back.
CREATE TABLE simulation_auth_time_fence (
    scope SMALLINT PRIMARY KEY CHECK (scope=1),
    last_observed_ms BIGINT NOT NULL CHECK (last_observed_ms>=0)
);
