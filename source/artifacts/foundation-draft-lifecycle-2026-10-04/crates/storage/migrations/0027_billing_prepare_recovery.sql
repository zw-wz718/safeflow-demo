-- Public recovery only. Original member tables and old migration checksums stay intact.
CREATE TABLE billing_prepare_realms_v2 (
 singleton boolean PRIMARY KEY DEFAULT true CHECK(singleton),
 realm uuid NOT NULL UNIQUE,
 namespace text NOT NULL,
 source_id text NOT NULL,
 source_revision text NOT NULL,
 continuity text NOT NULL,
 policy_digest text NOT NULL CHECK(policy_digest ~ '^[0-9a-f]{64}$'),
 enabled boolean NOT NULL,
 installed_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TABLE billing_prepare_requests_v2 (
 realm uuid NOT NULL REFERENCES billing_prepare_realms_v2(realm),
 nonce uuid NOT NULL,
 owner_id text NOT NULL,
 input_identity jsonb,
 identity_digest text,
 parameters jsonb,
 accounts jsonb,
 task_id text UNIQUE REFERENCES tasks(task_id) DEFERRABLE INITIALLY DEFERRED,
 operation_id text UNIQUE REFERENCES operations(operation_id) DEFERRABLE INITIALLY DEFERRED,
 confirmation_ref text UNIQUE REFERENCES server_confirmations(confirmation_ref) DEFERRABLE INITIALLY DEFERRED,
 session_digest text,
 binding_version bigint CHECK(binding_version > 0),
 logical_source text,
 source_id text,
 account_id text,
 transaction_id text,
 state text NOT NULL CHECK(state IN ('PREPARED_OPEN','EXECUTION_ADMITTED','SAVED','CLOSED_ROLLBACK','ABANDONED')),
 revision bigint NOT NULL CHECK(revision > 0),
 attempt_count smallint NOT NULL CHECK(attempt_count IN (0,1)),
 fence_revision bigint CHECK(fence_revision > 0),
 closed_at timestamptz,
 PRIMARY KEY(realm,nonce),
 CHECK((state='ABANDONED') = (fence_revision IS NOT NULL AND closed_at IS NOT NULL)),
 CHECK(fence_revision IS NULL OR fence_revision=revision),
 CHECK((state='PREPARED_OPEN' AND attempt_count=0) OR
       (state IN ('EXECUTION_ADMITTED','SAVED','CLOSED_ROLLBACK') AND attempt_count=1) OR state='ABANDONED'),
 CHECK((task_id IS NULL AND operation_id IS NULL AND confirmation_ref IS NULL AND input_identity IS NULL
        AND identity_digest IS NULL AND parameters IS NULL AND accounts IS NULL AND session_digest IS NULL
        AND binding_version IS NULL AND logical_source IS NULL AND source_id IS NULL AND account_id IS NULL
        AND transaction_id IS NULL AND state='ABANDONED' AND attempt_count=0)
    OR (task_id IS NOT NULL AND operation_id IS NOT NULL AND confirmation_ref IS NOT NULL
        AND input_identity IS NOT NULL AND identity_digest IS NOT NULL AND parameters IS NOT NULL
        AND accounts IS NOT NULL AND session_digest IS NOT NULL AND binding_version IS NOT NULL
        AND logical_source IS NOT NULL AND source_id IS NOT NULL AND account_id IS NOT NULL AND transaction_id IS NOT NULL))
);
CREATE UNIQUE INDEX billing_prepare_active_claim_v2 ON billing_prepare_requests_v2
 (realm,owner_id,logical_source,source_id,account_id,transaction_id) WHERE state<>'ABANDONED';
CREATE TABLE billing_prepare_attempts_v2 (
 realm uuid NOT NULL,
 nonce uuid NOT NULL,
 attempt_no smallint NOT NULL CHECK(attempt_no=1),
 claim uuid NOT NULL UNIQUE,
 request_digest text NOT NULL,
 state text NOT NULL CHECK(state IN ('ADMITTED','BUSINESS_COMMITTED','ROLLBACK_VERIFIED')),
 business_txid bigint,
 rollback_proof text,
 PRIMARY KEY(realm,nonce,attempt_no),
 FOREIGN KEY(realm,nonce) REFERENCES billing_prepare_requests_v2 DEFERRABLE INITIALLY DEFERRED,
 CHECK((state='ROLLBACK_VERIFIED')=(rollback_proof IS NOT NULL)),
 CHECK((state='ADMITTED')=(business_txid IS NULL)),
 CHECK(business_txid IS NULL OR business_txid>0)
);
CREATE TABLE billing_prepare_rate_v2 (
 realm uuid NOT NULL REFERENCES billing_prepare_realms_v2(realm),
 owner_id text NOT NULL,
 mode text NOT NULL CHECK(mode IN ('READ','WRITE')),
 at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX billing_prepare_rate_lookup_v2 ON billing_prepare_rate_v2(realm,mode,at,owner_id);

CREATE FUNCTION billing_prepare_monotone_v2() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' THEN RAISE EXCEPTION 'recovery history cannot be deleted'; END IF;
 IF (to_jsonb(NEW)-ARRAY['state','revision','attempt_count','fence_revision','closed_at']) IS DISTINCT FROM
    (to_jsonb(OLD)-ARRAY['state','revision','attempt_count','fence_revision','closed_at']) THEN
   RAISE EXCEPTION 'recovery identity is immutable';
 END IF;
 IF NEW.revision<>OLD.revision+1 OR NOT (
   (OLD.state='PREPARED_OPEN' AND NEW.state IN ('ABANDONED','EXECUTION_ADMITTED')) OR
   (OLD.state='EXECUTION_ADMITTED' AND NEW.state IN ('SAVED','CLOSED_ROLLBACK')) OR
   (OLD.state='CLOSED_ROLLBACK' AND NEW.state='ABANDONED')) THEN
   RAISE EXCEPTION 'invalid recovery transition';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER billing_prepare_monotone_v2 BEFORE UPDATE OR DELETE ON billing_prepare_requests_v2
 FOR EACH ROW EXECUTE FUNCTION billing_prepare_monotone_v2();
CREATE FUNCTION billing_attempt_monotone_v2() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' OR OLD.state<>'ADMITTED' OR NEW.state NOT IN ('BUSINESS_COMMITTED','ROLLBACK_VERIFIED')
 OR NEW.realm<>OLD.realm OR NEW.nonce<>OLD.nonce OR NEW.claim<>OLD.claim
 OR NEW.request_digest<>OLD.request_digest OR NEW.attempt_no<>OLD.attempt_no THEN
  RAISE EXCEPTION 'immutable execution admission';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER billing_attempt_monotone_v2 BEFORE UPDATE OR DELETE ON billing_prepare_attempts_v2
 FOR EACH ROW EXECUTE FUNCTION billing_attempt_monotone_v2();

CREATE FUNCTION billing_prepare_integrity_v2() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r billing_prepare_requests_v2; t tasks; o operations; c server_confirmations;
 a billing_prepare_attempts_v2; saved boolean; probe jsonb;
BEGIN
 probe := COALESCE(to_jsonb(NEW),to_jsonb(OLD));
 FOR r IN SELECT * FROM billing_prepare_requests_v2 r0 WHERE
   (r0.realm::text=probe->>'realm' AND r0.nonce::text=probe->>'nonce') OR
   r0.task_id=probe->>'task_id' OR r0.operation_id=probe->>'operation_id' OR
   r0.confirmation_ref=probe->>'confirmation_ref' OR r0.task_id=probe->'envelope'->'public_event'->>'task_id'
 LOOP
 IF r.task_id IS NULL THEN CONTINUE; END IF;
 SELECT * INTO t FROM tasks WHERE task_id=r.task_id;
 SELECT * INTO o FROM operations WHERE operation_id=r.operation_id;
 SELECT * INTO c FROM server_confirmations WHERE confirmation_ref=r.confirmation_ref;
 IF t.owner_id IS DISTINCT FROM r.owner_id OR o.task_id IS DISTINCT FROM r.task_id
 OR c.owner_id IS DISTINCT FROM r.owner_id OR c.task_id IS DISTINCT FROM r.task_id
 OR c.operation_id IS DISTINCT FROM r.operation_id OR c.session_digest IS DISTINCT FROM r.session_digest
 OR c.session_binding_version IS DISTINCT FROM r.binding_version
 OR c.arguments_hash IS DISTINCT FROM o.arguments_hash OR c.task_revision<>4 OR c.operation_revision<>2
 OR r.input_identity->'parameters' IS DISTINCT FROM r.parameters
 OR r.input_identity->>'owner_id' IS DISTINCT FROM r.owner_id
 OR r.input_identity->>'realm' IS DISTINCT FROM r.realm::text THEN
   RAISE EXCEPTION 'incomplete recovery association';
 END IF;
 SELECT * INTO a FROM billing_prepare_attempts_v2 WHERE realm=r.realm AND nonce=r.nonce;
 IF (r.attempt_count=0 AND a.claim IS NOT NULL) OR (r.attempt_count=1 AND a.claim IS NULL)
 OR (r.state='EXECUTION_ADMITTED' AND a.state IS DISTINCT FROM 'ADMITTED')
 OR (r.state='SAVED' AND a.state IS DISTINCT FROM 'BUSINESS_COMMITTED')
 OR (r.state='CLOSED_ROLLBACK' AND a.state IS DISTINCT FROM 'ROLLBACK_VERIFIED') THEN
   RAISE EXCEPTION 'incomplete recovery attempt';
 END IF;
 saved := EXISTS(SELECT 1 FROM public_billing_feedback_results_v1 WHERE operation_id=r.operation_id);
 IF (r.state='SAVED')<>saved OR saved<>EXISTS(SELECT 1 FROM billing_feedback_v1 WHERE public_operation_id=r.operation_id) THEN
   RAISE EXCEPTION 'incomplete recovery business result';
 END IF;
 IF (SELECT count(DISTINCT kind) FROM audit_events WHERE task_id=r.task_id AND
   ((kind='TASK_CREATED' AND task_revision=1) OR (kind='OPERATION_CREATED' AND task_revision=2 AND operation_revision=1)
    OR (kind='CHECKPOINT_SAVED' AND task_revision=3) OR (kind='OPERATION_TRANSITIONED' AND task_revision=4 AND operation_revision=2)
    OR (kind='CONFIRMATION_PREPARED' AND task_revision=4 AND operation_revision=2)))<>5
 OR EXISTS(SELECT 1 FROM audit_events e LEFT JOIN public_audit_outbox b USING(event_id) WHERE e.task_id=r.task_id
    AND (b.event_id IS NULL OR b.envelope->'public_event' IS DISTINCT FROM to_jsonb(e))) THEN
   RAISE EXCEPTION 'incomplete original audit/outbox';
 END IF;
 IF saved AND (t.state<>'COMPLETED' OR o.state<>'SUCCEEDED'
   OR NOT EXISTS(SELECT 1 FROM public_execution_requests WHERE operation_id=r.operation_id AND owner_id=r.owner_id AND first_operation IS NOT NULL)
   OR (SELECT count(*) FROM execution_permits WHERE operation_id=r.operation_id AND status='CONSUMED')<>1) THEN
   RAISE EXCEPTION 'incomplete saved execution';
 END IF;
 IF r.state IN ('PREPARED_OPEN','EXECUTION_ADMITTED','CLOSED_ROLLBACK') AND (t.state<>'RUNNING' OR t.revision<>4 OR o.state<>'READY' OR o.revision<>2) THEN
   RAISE EXCEPTION 'invalid unexecuted snapshot';
 END IF;
 IF r.state='ABANDONED' AND (t.state<>'CANCELLED' OR o.state<>'CANCELLED' OR c.status<>'REVOKED'
    OR (r.attempt_count=1 AND a.state<>'ROLLBACK_VERIFIED')
    OR EXISTS(SELECT 1 FROM execution_permits WHERE operation_id=r.operation_id AND status IN ('ACTIVE','CONSUMED'))
    OR EXISTS(SELECT 1 FROM public_execution_requests WHERE operation_id=r.operation_id AND first_operation IS NOT NULL)) THEN
   RAISE EXCEPTION 'incomplete closing barrier';
 END IF;
 END LOOP;
 RETURN NEW;
END $$;
CREATE CONSTRAINT TRIGGER billing_prepare_integrity_v2 AFTER INSERT OR UPDATE ON billing_prepare_requests_v2
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_prepare_integrity_v2();
CREATE CONSTRAINT TRIGGER billing_attempt_integrity_v2 AFTER INSERT OR UPDATE ON billing_prepare_attempts_v2
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_prepare_integrity_v2();

-- Revalidate the final committed graph when any associated public row changes.
CREATE CONSTRAINT TRIGGER billing_association_0_v2 AFTER INSERT OR UPDATE OR DELETE ON tasks
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_prepare_integrity_v2();
CREATE CONSTRAINT TRIGGER billing_association_1_v2 AFTER INSERT OR UPDATE OR DELETE ON operations
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_prepare_integrity_v2();
CREATE CONSTRAINT TRIGGER billing_association_2_v2 AFTER INSERT OR UPDATE OR DELETE ON server_confirmations
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_prepare_integrity_v2();
CREATE CONSTRAINT TRIGGER billing_association_3_v2 AFTER INSERT OR UPDATE OR DELETE ON execution_permits
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_prepare_integrity_v2();
CREATE CONSTRAINT TRIGGER billing_association_4_v2 AFTER INSERT OR UPDATE OR DELETE ON public_execution_requests
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_prepare_integrity_v2();
CREATE CONSTRAINT TRIGGER billing_association_5_v2 AFTER INSERT OR UPDATE OR DELETE ON audit_events
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_prepare_integrity_v2();
CREATE CONSTRAINT TRIGGER billing_association_6_v2 AFTER INSERT OR UPDATE OR DELETE ON public_audit_outbox
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_prepare_integrity_v2();
CREATE CONSTRAINT TRIGGER billing_association_7_v2 AFTER INSERT OR UPDATE OR DELETE ON public_billing_feedback_results_v1
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_prepare_integrity_v2();
CREATE CONSTRAINT TRIGGER billing_association_8_v2 AFTER INSERT OR UPDATE OR DELETE ON billing_feedback_v1
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_prepare_integrity_v2();

CREATE FUNCTION billing_realm_immutable_v2() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' OR (to_jsonb(NEW)-'enabled') IS DISTINCT FROM (to_jsonb(OLD)-'enabled') THEN
   RAISE EXCEPTION 'recovery realm history is immutable';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER billing_realm_immutable_v2 BEFORE UPDATE OR DELETE ON billing_prepare_realms_v2
 FOR EACH ROW EXECUTE FUNCTION billing_realm_immutable_v2();

-- Durable monotonic high-water mark remains after expiring operational counters.
CREATE TABLE billing_prepare_rate_clock_v2 (
 realm uuid PRIMARY KEY REFERENCES billing_prepare_realms_v2(realm),
 high_water timestamptz NOT NULL
);
