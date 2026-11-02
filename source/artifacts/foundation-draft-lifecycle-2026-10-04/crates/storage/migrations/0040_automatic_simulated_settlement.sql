-- Opt-in NEW-original PROJECT_SIMULATION cohort. No historical enrollment,
-- new funds writer, decision replacement, or default activation.
CREATE TABLE payment_auto_runs_v1 (
 run_id UUID PRIMARY KEY,
 producer_role TEXT NOT NULL REFERENCES payment_background_principals_v1(role_name),
 worker_role TEXT NOT NULL REFERENCES payment_background_principals_v1(role_name),
 enabled BOOLEAN NOT NULL DEFAULT true,
 activated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
 CHECK(producer_role<>worker_role)
);
CREATE UNIQUE INDEX payment_auto_one_enabled ON payment_auto_runs_v1(enabled) WHERE enabled;
CREATE TABLE payment_auto_originals_v1 (
 grant_id TEXT PRIMARY KEY REFERENCES payment_completion_candidate_grants(grant_id),
 run_id UUID NOT NULL REFERENCES payment_auto_runs_v1(run_id),
 result_id TEXT NOT NULL UNIQUE CHECK(result_id ~ '^automatic_simulation_v1_[0-9a-f]{64}$')
);
CREATE FUNCTION payment_auto_immutable_v1() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_TABLE_NAME='payment_auto_runs_v1' AND TG_OP='UPDATE' AND OLD.enabled AND NOT NEW.enabled
 AND (to_jsonb(NEW)-'enabled')=(to_jsonb(OLD)-'enabled') THEN RETURN NEW; END IF;
 RAISE EXCEPTION 'AUTOMATIC_SIMULATION_IMMUTABLE';
END $$;
CREATE TRIGGER payment_auto_run_guard BEFORE UPDATE OR DELETE ON payment_auto_runs_v1
 FOR EACH ROW EXECUTE FUNCTION payment_auto_immutable_v1();
CREATE TRIGGER payment_auto_original_guard BEFORE UPDATE OR DELETE ON payment_auto_originals_v1
 FOR EACH ROW EXECUTE FUNCTION payment_auto_immutable_v1();

CREATE FUNCTION payment_auto_enroll_v1() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE r RECORD;
BEGIN
 SELECT * INTO r FROM payment_auto_runs_v1 WHERE enabled FOR SHARE;
 IF NOT FOUND THEN RETURN NEW; END IF;
 -- Acceptance itself has already checked the closed permit/consent/source.
 -- This extra scope only narrows the approved public shared-account demo.
 IF NEW.owner_id='user_001' AND NEW.evidence->>'payer_account_id'='account_001'
 AND NEW.policy_ref='sf-isolated-demo-policy-20261027-v1'
 AND NEW.evidence->>'schema'='payment-background-acceptance-v1'
 AND NEW.evidence->>'mode'='PROJECT_SIMULATION'
 AND NEW.evidence->>'source_id'='safeflow-team-simulation'
 AND NEW.first_submitted_at>=r.activated_at
 AND EXISTS(SELECT 1 FROM payment_background_consents_v1 c WHERE
 'background_'||c.consent_id::text=NEW.grant_id AND c.owner_id=NEW.owner_id
 AND c.auth_ref IS NOT NULL AND c.confirmed_at IS NOT NULL) THEN
 INSERT INTO payment_auto_originals_v1(grant_id,run_id,result_id)
 VALUES(NEW.grant_id,r.run_id,'automatic_simulation_v1_'||encode(sha256(convert_to(NEW.grant_id,'UTF8')),'hex'));
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER payment_auto_new_original AFTER INSERT ON payment_completion_candidate_grants
 FOR EACH ROW EXECUTE FUNCTION payment_auto_enroll_v1();

-- No broad table grants. A registered producer/worker can see only its own
-- enabled cohort and only minimal original/result identifiers and presence.
CREATE FUNCTION payment_auto_scan_v1(batch UUID, after_id TEXT, cap INTEGER) RETURNS JSONB
 LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE k TEXT; result JSONB;
BEGIN
 SELECT kind INTO k FROM payment_background_identity_v1() WHERE active;
 IF k IS NULL OR cap IS NULL OR cap NOT BETWEEN 1 AND 32 OR after_id IS NULL THEN RAISE EXCEPTION 'AUTOMATIC_SIMULATION_IDENTITY_REQUIRED'; END IF;
 SELECT coalesce(jsonb_agg(to_jsonb(q) ORDER BY q.grant_id),'[]'::jsonb) INTO result FROM (
 SELECT m.grant_id,m.result_id,EXISTS(SELECT 1 FROM payment_completion_candidate_decisions d WHERE d.grant_id=m.grant_id) AS has_decision
 FROM payment_auto_originals_v1 m JOIN payment_auto_runs_v1 r USING(run_id)
 WHERE r.run_id=batch AND r.enabled AND m.grant_id>after_id
 AND ((k='PRODUCER' AND r.producer_role=session_user::text) OR (k='WORKER' AND r.worker_role=session_user::text))
 AND NOT EXISTS(SELECT 1 FROM payment_completion_candidate_receipts v WHERE v.grant_id=m.grant_id)
 ORDER BY m.grant_id LIMIT cap) q;
 RETURN result;
END $$;

-- Serialize hard disable with NEW decision insertion and final commit. The
-- original independent authority, original worker and deferred funds guards
-- still apply; this trigger never writes any business or completion result.
CREATE FUNCTION payment_auto_action_guard_v1() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE r RECORD; m RECORD;
BEGIN
 SELECT * INTO m FROM payment_auto_originals_v1 WHERE grant_id=NEW.grant_id;
 IF NOT FOUND THEN RETURN NEW; END IF;
 SELECT * INTO r FROM payment_auto_runs_v1 WHERE run_id=m.run_id FOR SHARE;
 IF NOT FOUND OR NOT r.enabled THEN RAISE EXCEPTION 'AUTOMATIC_SIMULATION_DISABLED'; END IF;
 IF TG_TABLE_NAME='payment_completion_candidate_decisions' THEN
 IF session_user::text=r.producer_role AND (NEW.result_id<>m.result_id OR NEW.outcome<>'SETTLE_LOCAL_SIMULATION')
 THEN RAISE EXCEPTION 'AUTOMATIC_SIMULATION_RESULT_CONFLICT'; END IF;
 ELSE
 IF session_user::text<>r.worker_role THEN RAISE EXCEPTION 'AUTOMATIC_SIMULATION_WORKER_REQUIRED'; END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER payment_auto_decision_guard BEFORE INSERT ON payment_completion_candidate_decisions
 FOR EACH ROW EXECUTE FUNCTION payment_auto_action_guard_v1();
CREATE CONSTRAINT TRIGGER payment_auto_commit_guard AFTER INSERT ON payment_completion_candidate_receipts
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION payment_auto_action_guard_v1();

REVOKE ALL ON TABLE payment_auto_runs_v1,payment_auto_originals_v1 FROM PUBLIC;
REVOKE ALL ON FUNCTION payment_auto_immutable_v1(),payment_auto_enroll_v1(),payment_auto_scan_v1(UUID,TEXT,INTEGER),payment_auto_action_guard_v1() FROM PUBLIC;
DO $$
DECLARE ns TEXT:=current_schema(); f RECORD;
BEGIN
 FOR f IN SELECT p.proname,pg_get_function_identity_arguments(p.oid) AS args FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
 WHERE n.nspname=ns AND p.proname LIKE 'payment_auto_%'
 LOOP EXECUTE format('ALTER FUNCTION %I.%I(%s) SET search_path TO pg_catalog,%I,pg_temp',ns,f.proname,f.args,ns); END LOOP;
END $$;
