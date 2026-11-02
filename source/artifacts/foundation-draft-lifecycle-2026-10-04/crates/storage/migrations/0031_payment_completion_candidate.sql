-- UNADOPTED local technical candidate. No principals, policy, grants or jobs
-- are provisioned. Existing execute/resolve never consult these tables.
CREATE TABLE payment_completion_candidate_fences (
    kind TEXT NOT NULL CHECK(kind IN ('POLICY','WORKER','PRODUCER','SOURCE','CARD_READ')),
    authority_id TEXT NOT NULL CHECK(length(btrim(authority_id)) BETWEEN 1 AND 200),
    epoch BIGINT NOT NULL CHECK(epoch>0),
    enabled BOOLEAN NOT NULL,
    valid_until TIMESTAMPTZ NOT NULL CHECK(isfinite(valid_until)),
    scope JSONB NOT NULL CHECK(jsonb_typeof(scope)='object'),
    adoption_ref TEXT NOT NULL CHECK(length(btrim(adoption_ref)) BETWEEN 1 AND 200),
    PRIMARY KEY(kind,authority_id)
);
CREATE TABLE payment_completion_candidate_grants (
    grant_id TEXT PRIMARY KEY CHECK(length(btrim(grant_id)) BETWEEN 1 AND 200),
    owner_id TEXT NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    preparation_operation_id TEXT NOT NULL REFERENCES payment_funds_execution_v1(preparation_operation_id),
    execution_operation_id TEXT NOT NULL REFERENCES public_execution_requests(operation_id),
    business_operation_ref TEXT NOT NULL,
    original_key TEXT NOT NULL,
    grant_epoch BIGINT NOT NULL CHECK(grant_epoch>0),
    first_submitted_at TIMESTAMPTZ NOT NULL CHECK(isfinite(first_submitted_at)),
    completion_until TIMESTAMPTZ NOT NULL CHECK(isfinite(completion_until) AND completion_until>first_submitted_at),
    evidence JSONB NOT NULL CHECK(jsonb_typeof(evidence)='object' AND octet_length(evidence::text)<=262144),
    evidence_digest TEXT NOT NULL CHECK(evidence_digest ~ '^[0-9a-f]{64}$'),
    policy_ref TEXT NOT NULL CHECK(length(btrim(policy_ref)) BETWEEN 1 AND 200),
    UNIQUE(execution_operation_id,grant_epoch),
    UNIQUE(grant_id,execution_operation_id,grant_epoch),
    FOREIGN KEY(owner_id,original_key) REFERENCES public_execution_requests(owner_id,idempotency_key),
    FOREIGN KEY(owner_id,business_operation_ref) REFERENCES public_execution_requests(owner_id,business_operation_ref)
);
-- FK existence alone cannot prevent mixing two real originals.
CREATE FUNCTION completion_candidate_original_v1() RETURNS trigger
LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
    IF NOT EXISTS(SELECT 1 FROM public_execution_requests r
        JOIN payment_funds_execution_v1 a ON a.execution_operation_id=r.operation_id
        WHERE r.owner_id=NEW.owner_id AND r.task_id=NEW.task_id
        AND r.operation_id=NEW.execution_operation_id AND r.idempotency_key=NEW.original_key
        AND r.business_operation_ref=NEW.business_operation_ref
        AND r.submitted_at=NEW.first_submitted_at AND r.first_operation IS NOT NULL
        AND a.preparation_operation_id=NEW.preparation_operation_id AND a.task_id=r.task_id) THEN
        RAISE EXCEPTION 'COMPLETION_CANDIDATE_ORIGINAL_CONFLICT';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER completion_candidate_original BEFORE INSERT ON payment_completion_candidate_grants
FOR EACH ROW EXECUTE FUNCTION completion_candidate_original_v1();

CREATE TABLE payment_completion_candidate_controls (
    execution_operation_id TEXT PRIMARY KEY REFERENCES public_execution_requests(operation_id),
    grant_id TEXT NOT NULL UNIQUE,
    grant_epoch BIGINT NOT NULL CHECK(grant_epoch>0),
    control_epoch BIGINT NOT NULL CHECK(control_epoch>0),
    paused BOOLEAN NOT NULL,
    revoked BOOLEAN NOT NULL,
    FOREIGN KEY(grant_id,execution_operation_id,grant_epoch)
        REFERENCES payment_completion_candidate_grants(grant_id,execution_operation_id,grant_epoch)
);
CREATE TABLE payment_completion_candidate_decisions (
    decision_id TEXT PRIMARY KEY CHECK(length(btrim(decision_id)) BETWEEN 1 AND 200),
    grant_id TEXT NOT NULL UNIQUE REFERENCES payment_completion_candidate_grants(grant_id),
    source_id TEXT NOT NULL CHECK(length(btrim(source_id)) BETWEEN 1 AND 200),
    source_epoch BIGINT NOT NULL CHECK(source_epoch>0),
    result_id TEXT NOT NULL CHECK(length(btrim(result_id)) BETWEEN 1 AND 200),
    decision_seq SMALLINT NOT NULL CHECK(decision_seq=1),
    outcome TEXT NOT NULL CHECK(outcome IN ('SETTLE_LOCAL_SIMULATION','DEFINITE_SIMULATED_DECLINE')),
    envelope JSONB NOT NULL CHECK(jsonb_typeof(envelope)='object' AND octet_length(envelope::text)<=262144),
    envelope_digest TEXT NOT NULL CHECK(envelope_digest ~ '^[0-9a-f]{64}$'),
    fact_at TIMESTAMPTZ NOT NULL CHECK(isfinite(fact_at)),
    received_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp() CHECK(isfinite(received_at) AND received_at>=fact_at),
    UNIQUE(source_id,source_epoch,result_id),
    UNIQUE(decision_id,grant_id)
);
CREATE TABLE payment_completion_candidate_jobs (
    grant_id TEXT PRIMARY KEY REFERENCES payment_completion_candidate_grants(grant_id),
    lease_epoch BIGINT NOT NULL DEFAULT 0 CHECK(lease_epoch>=0),
    lease_owner TEXT CHECK(length(btrim(lease_owner)) BETWEEN 1 AND 200),
    leased_at TIMESTAMPTZ,
    lease_until TIMESTAMPTZ,
    CHECK((lease_epoch=0 AND lease_owner IS NULL AND leased_at IS NULL AND lease_until IS NULL)
        OR (lease_epoch>0 AND lease_owner IS NOT NULL AND leased_at IS NOT NULL
        AND lease_until IS NOT NULL AND isfinite(leased_at) AND isfinite(lease_until)
        AND lease_until>leased_at AND lease_until<=leased_at+interval '5 minutes'))
);
CREATE TABLE payment_completion_candidate_receipts (
    execution_operation_id TEXT PRIMARY KEY,
    grant_id TEXT NOT NULL UNIQUE,
    grant_epoch BIGINT NOT NULL,
    decision_id TEXT NOT NULL UNIQUE,
    public_revision BIGINT NOT NULL CHECK(public_revision=4),
    evidence JSONB NOT NULL CHECK(jsonb_typeof(evidence)='object' AND octet_length(evidence::text)<=262144),
    evidence_digest TEXT NOT NULL CHECK(evidence_digest ~ '^[0-9a-f]{64}$'),
    completed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp() CHECK(isfinite(completed_at)),
    FOREIGN KEY(grant_id,execution_operation_id,grant_epoch)
        REFERENCES payment_completion_candidate_grants(grant_id,execution_operation_id,grant_epoch),
    FOREIGN KEY(decision_id,grant_id) REFERENCES payment_completion_candidate_decisions(decision_id,grant_id)
);
CREATE FUNCTION completion_candidate_immutable_v1() RETURNS trigger
LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN RAISE EXCEPTION 'COMPLETION_CANDIDATE_IMMUTABLE'; END $$;
CREATE TRIGGER completion_candidate_grant_immutable BEFORE UPDATE OR DELETE ON payment_completion_candidate_grants
FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
CREATE TRIGGER completion_candidate_decision_immutable BEFORE UPDATE OR DELETE ON payment_completion_candidate_decisions
FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
CREATE TRIGGER completion_candidate_receipt_immutable BEFORE UPDATE OR DELETE ON payment_completion_candidate_receipts
FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();

CREATE FUNCTION completion_candidate_monotone_v1() RETURNS trigger
LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'COMPLETION_CANDIDATE_NO_RESET'; END IF;
    IF TG_TABLE_NAME='payment_completion_candidate_fences' THEN
        IF NEW.kind<>OLD.kind OR NEW.authority_id<>OLD.authority_id OR NEW.epoch<=OLD.epoch THEN
            RAISE EXCEPTION 'COMPLETION_CANDIDATE_FENCE_EPOCH'; END IF;
    ELSIF TG_TABLE_NAME='payment_completion_candidate_controls' THEN
        IF NEW.execution_operation_id<>OLD.execution_operation_id OR NEW.control_epoch<>OLD.control_epoch+1
            OR NEW.grant_epoch<OLD.grant_epoch
            OR (NEW.grant_epoch=OLD.grant_epoch AND (NEW.grant_id<>OLD.grant_id OR (OLD.revoked AND NOT NEW.revoked)))
            OR (NEW.grant_epoch>OLD.grant_epoch AND NEW.grant_id=OLD.grant_id) THEN
            RAISE EXCEPTION 'COMPLETION_CANDIDATE_CONTROL_EPOCH'; END IF;
    ELSE
        IF NEW.grant_id<>OLD.grant_id OR NEW.lease_epoch<>OLD.lease_epoch+1 THEN
            RAISE EXCEPTION 'COMPLETION_CANDIDATE_LEASE_EPOCH'; END IF;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER completion_candidate_fence_epoch BEFORE UPDATE OR DELETE ON payment_completion_candidate_fences
FOR EACH ROW EXECUTE FUNCTION completion_candidate_monotone_v1();
CREATE TRIGGER completion_candidate_control_epoch BEFORE UPDATE OR DELETE ON payment_completion_candidate_controls
FOR EACH ROW EXECUTE FUNCTION completion_candidate_monotone_v1();
CREATE TRIGGER completion_candidate_job_epoch BEFORE UPDATE OR DELETE ON payment_completion_candidate_jobs
FOR EACH ROW EXECUTE FUNCTION completion_candidate_monotone_v1();
REVOKE ALL ON TABLE payment_completion_candidate_fences,payment_completion_candidate_grants,
    payment_completion_candidate_controls,payment_completion_candidate_decisions,
    payment_completion_candidate_jobs,payment_completion_candidate_receipts FROM PUBLIC;
REVOKE ALL ON FUNCTION completion_candidate_original_v1(),completion_candidate_immutable_v1(),
    completion_candidate_monotone_v1() FROM PUBLIC;
