CREATE TABLE planning_confirmation_requests (
    task_id TEXT PRIMARY KEY REFERENCES tasks(task_id),
    owner_id TEXT NOT NULL,
    operation_id TEXT NOT NULL UNIQUE,
    initial_task JSONB NOT NULL CHECK (jsonb_typeof(initial_task) = 'object'),
    initial_digest TEXT NOT NULL CHECK (initial_digest ~ '^[0-9a-f]{64}$'),
    registration_hash TEXT NOT NULL CHECK (registration_hash ~ '^[0-9a-f]{64}$')
);

CREATE TABLE planning_confirmation_displays (
    display_ticket TEXT PRIMARY KEY CHECK (display_ticket ~ '^display_[0-9a-f]{32}$'),
    task_id TEXT NOT NULL REFERENCES planning_confirmation_requests(task_id),
    session_digest TEXT NOT NULL CHECK (session_digest ~ '^[0-9a-f]{64}$'),
    binding_version BIGINT NOT NULL CHECK (binding_version > 0),
    snapshot_digest TEXT NOT NULL CHECK (snapshot_digest ~ '^[0-9a-f]{64}$'),
    pre_tool_request TEXT NOT NULL UNIQUE,
    pre_auth_request TEXT NOT NULL UNIQUE,
    pre_tool_decision TEXT REFERENCES security_decisions(stored_decision_id),
    pre_auth_decision TEXT REFERENCES security_decisions(stored_decision_id),
    confirmation_ref TEXT REFERENCES server_confirmations(confirmation_ref),
    display_digest TEXT CHECK (display_digest ~ '^[0-9a-f]{64}$'),
    UNIQUE (task_id, session_digest, binding_version),
    CHECK (
        (pre_tool_decision IS NULL AND pre_auth_decision IS NULL AND confirmation_ref IS NULL AND display_digest IS NULL)
        OR
        (pre_tool_decision IS NOT NULL AND pre_auth_decision IS NOT NULL AND confirmation_ref IS NOT NULL AND display_digest IS NOT NULL)
    )
);
