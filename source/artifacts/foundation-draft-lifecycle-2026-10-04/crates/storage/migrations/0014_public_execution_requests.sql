ALTER TABLE audit_events DROP CONSTRAINT audit_events_kind_check;
ALTER TABLE audit_events ADD CONSTRAINT audit_events_kind_check CHECK (
    kind IN ('TASK_CREATED','CHECKPOINT_SAVED','PLAN_REPLACED','OPERATION_CREATED',
             'OPERATION_TRANSITIONED','CHANGE_REJECTED','CONFIRMATION_PREPARED',
             'EXPLICIT_CONFIRMATION_RECORDED','CONFIRMATION_REVOKED',
             'PERMIT_SAVED','PERMIT_CONSUMED','PERMIT_REVOKED',
             'SIM_AUTH_CHALLENGE_CREATED','SIM_AUTH_FAILED','SIM_AUTH_VERIFIED',
             'SIM_AUTH_GRANT_ISSUED','SIM_AUTH_CHALLENGE_REVOKED','SIM_AUTH_GRANT_REVOKED',
             'SECURITY_DECISION_RECORDED','SECURITY_DECISION_REVOKED','SECURITY_DECISION_INVALIDATED',
             'EXECUTION_REQUEST_RESERVED')
);

CREATE TABLE public_execution_requests (
    contract_version SMALLINT NOT NULL CHECK (contract_version=1),
    owner_id TEXT NOT NULL CHECK (length(owner_id) BETWEEN 1 AND 200),
    idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 200),
    business_operation_ref TEXT NOT NULL CHECK (length(business_operation_ref) BETWEEN 1 AND 200),
    business_version BIGINT NOT NULL CHECK (business_version>0),
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    operation_id TEXT NOT NULL UNIQUE,
    node_id TEXT NOT NULL CHECK (length(node_id) BETWEEN 1 AND 200),
    tool_name TEXT NOT NULL CHECK (length(tool_name) BETWEEN 1 AND 200),
    task_revision BIGINT NOT NULL CHECK (task_revision>0),
    operation_revision BIGINT NOT NULL CHECK (operation_revision>0),
    plan_version BIGINT NOT NULL CHECK (plan_version BETWEEN 1 AND 4294967295),
    facts_version BIGINT NOT NULL CHECK (facts_version BETWEEN 1 AND 4294967295),
    arguments_hash TEXT NOT NULL CHECK (arguments_hash ~ '^[0-9a-f]{64}$'),
    operation_fingerprint TEXT NOT NULL CHECK (operation_fingerprint ~ '^[0-9a-f]{64}$'),
    fingerprint_payload JSONB NOT NULL CHECK (jsonb_typeof(fingerprint_payload)='object'
        AND octet_length(fingerprint_payload::text)<=65536),
    request_digest TEXT NOT NULL CHECK (request_digest ~ '^[0-9a-f]{64}$'),
    reserved_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    first_operation JSONB CHECK (jsonb_typeof(first_operation)='object'
        AND octet_length(first_operation::text)<=8192),
    submitted_at TIMESTAMPTZ,
    PRIMARY KEY(owner_id,idempotency_key),
    UNIQUE(owner_id,business_operation_ref),
    FOREIGN KEY(task_id,operation_id) REFERENCES operations(task_id,operation_id),
    CHECK ((first_operation IS NULL AND submitted_at IS NULL) OR
           (first_operation IS NOT NULL AND submitted_at IS NOT NULL))
);

-- Keep original identity/parameters and the first public submission permanent.
CREATE FUNCTION preserve_public_execution_request() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='DELETE' THEN
        RAISE EXCEPTION 'public execution request cannot be deleted';
    END IF;
    IF (to_jsonb(NEW)-'first_operation'-'submitted_at') IS DISTINCT FROM
       (to_jsonb(OLD)-'first_operation'-'submitted_at') OR
       (OLD.first_operation IS NOT NULL AND
        (NEW.first_operation IS DISTINCT FROM OLD.first_operation OR
         NEW.submitted_at IS DISTINCT FROM OLD.submitted_at)) THEN
        RAISE EXCEPTION 'public execution request is immutable';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER public_execution_request_immutable BEFORE UPDATE OR DELETE
    ON public_execution_requests FOR EACH ROW EXECUTE FUNCTION preserve_public_execution_request();
