-- Explicit native maintenance ONLY, after exact v42 verification; never auto-run
-- by bank startup or ordinary migrate(). No old principal/run/grant is updated.
CREATE TABLE payment_authority_continuations_v1 (
 singleton SMALLINT PRIMARY KEY CHECK(singleton=1),
 predecessor_run UUID NOT NULL UNIQUE REFERENCES payment_auto_runs_v1(run_id),
 run_id UUID NOT NULL UNIQUE REFERENCES payment_auto_runs_v1(run_id),
 producer_role TEXT NOT NULL UNIQUE REFERENCES payment_background_principals_v1(role_name),
 worker_role TEXT NOT NULL UNIQUE REFERENCES payment_background_principals_v1(role_name),
 epoch BIGINT NOT NULL CHECK(epoch=1),
 starts_at TIMESTAMPTZ NOT NULL,
 ends_at TIMESTAMPTZ NOT NULL CHECK(ends_at='2026-11-11T00:00:00+08:00'::timestamptz AND ends_at>starts_at),
 approval_sha256 TEXT NOT NULL CHECK(approval_sha256 ~ '^[0-9a-f]{64}$'),
 plan_sha256 TEXT NOT NULL CHECK(plan_sha256 ~ '^[0-9a-f]{64}$'),
 predecessor_snapshot JSONB NOT NULL,
 post_catalog_sha256 TEXT NOT NULL CHECK(post_catalog_sha256 ~ '^[0-9a-f]{64}$'),
 applied_by TEXT NOT NULL CHECK(applied_by='postgres'),
 recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
 CHECK(predecessor_run<>run_id AND producer_role<>worker_role)
);
CREATE TRIGGER payment_authority_continuation_immutable BEFORE UPDATE OR DELETE ON payment_authority_continuations_v1
 FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
REVOKE ALL ON TABLE payment_authority_continuations_v1 FROM PUBLIC;

-- Retain the old enabled run verbatim. Allow exactly one other enabled cohort;
-- it is selected explicitly by the immutable successor record below.
DROP INDEX payment_auto_one_enabled;
CREATE UNIQUE INDEX payment_auto_one_enabled ON payment_auto_runs_v1(enabled)
 WHERE enabled AND run_id<>'8bb1e389-1bb9-47ec-8ce4-bfe247b0bc06'::uuid;

CREATE FUNCTION payment_authority_window_v1() RETURNS BOOLEAN
 LANGUAGE sql VOLATILE SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
 SELECT EXISTS(SELECT 1 FROM payment_authority_continuations_v1 a
 JOIN payment_auto_runs_v1 r ON r.run_id=a.run_id AND r.enabled AND r.producer_role=a.producer_role AND r.worker_role=a.worker_role
 JOIN payment_background_principals_v1 p ON p.role_name=a.producer_role AND p.kind='PRODUCER'
 JOIN payment_background_principals_v1 w ON w.role_name=a.worker_role AND w.kind='WORKER'
 JOIN payment_completion_candidate_fences pf ON pf.kind=p.kind AND pf.authority_id=p.role_name
 JOIN payment_completion_candidate_fences wf ON wf.kind=w.kind AND wf.authority_id=w.role_name
 WHERE clock_timestamp()>=a.starts_at AND clock_timestamp()<a.ends_at
 AND p.active AND w.active AND pf.enabled AND wf.enabled
 AND p.epoch=a.epoch AND w.epoch=a.epoch AND pf.epoch=a.epoch AND wf.epoch=a.epoch
 AND p.valid_until=a.ends_at AND w.valid_until=a.ends_at AND pf.valid_until=a.ends_at AND wf.valid_until=a.ends_at
 AND p.source_id='safeflow-team-simulation' AND w.source_id=p.source_id
 AND p.implementation='foundation-unified-fictional-v1' AND w.implementation=p.implementation)
$$;

-- No backfill: this trigger is installed while tasks INSERT is fenced by DDL.
-- A pre-install/uncommitted task cannot acquire successor authority later by
-- renewing its confirmation, code, consent, preparation or business reference.
CREATE TABLE payment_authority_accepted_tasks_v1 (
 task_id TEXT PRIMARY KEY REFERENCES tasks(task_id),
 owner_id TEXT NOT NULL,
 creation_digest TEXT NOT NULL CHECK(creation_digest ~ '^[0-9a-f]{64}$'),
 task_created_at TIMESTAMPTZ NOT NULL,
 accepted_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
 run_id UUID NOT NULL REFERENCES payment_auto_runs_v1(run_id)
);
CREATE TRIGGER payment_authority_accepted_task_immutable BEFORE UPDATE OR DELETE ON payment_authority_accepted_tasks_v1
 FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
REVOKE ALL ON TABLE payment_authority_accepted_tasks_v1 FROM PUBLIC;
CREATE FUNCTION payment_authority_task_accept_v1() RETURNS trigger
 LANGUAGE plpgsql SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
BEGIN
 INSERT INTO payment_authority_accepted_tasks_v1(task_id,owner_id,creation_digest,task_created_at,run_id)
 SELECT NEW.task_id,NEW.owner_id,NEW.creation_digest,NEW.created_at,a.run_id
 FROM payment_authority_continuations_v1 a
 WHERE a.singleton=1 AND payment_authority_window_v1()
 AND NEW.created_at>=a.starts_at AND NEW.created_at<a.ends_at;
 RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION payment_authority_task_accept_v1() FROM PUBLIC;
CREATE TRIGGER payment_authority_task_accept AFTER INSERT ON tasks
 FOR EACH ROW EXECUTE FUNCTION payment_authority_task_accept_v1();

-- Additional gate, never a substitute for original acceptance/confirmation/OTP.
-- Reject before hold/acceptance transaction commits; old completed history is not touched.
CREATE FUNCTION payment_authority_accept_guard_v1() RETURNS trigger
 LANGUAGE plpgsql SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
DECLARE a RECORD; c RECORD;
BEGIN
 SELECT * INTO a FROM payment_authority_continuations_v1 WHERE singleton=1;
 IF NOT FOUND THEN RAISE EXCEPTION 'NEW_AUTHORITY_NOT_INSTALLED'; END IF;
 PERFORM run_id FROM payment_auto_runs_v1 WHERE run_id=a.run_id FOR SHARE;
 PERFORM authority_id FROM payment_completion_candidate_fences
 WHERE authority_id IN(a.producer_role,a.worker_role) ORDER BY kind FOR SHARE;
 SELECT * INTO c FROM payment_background_consents_v1 WHERE 'background_'||consent_id::text=NEW.grant_id;
 IF NOT payment_authority_window_v1() OR c.consent_id IS NULL
 OR NEW.owner_id<>'user_001' OR NEW.evidence->>'payer_account_id' IS DISTINCT FROM 'account_001'
 OR NEW.first_submitted_at<a.starts_at OR NEW.first_submitted_at>=a.ends_at
 OR c.created_at<a.starts_at OR c.confirmed_at IS NULL OR c.confirmed_at<a.starts_at
 OR c.challenge_at IS NULL OR c.challenge_at<a.starts_at OR c.auth_at IS NULL OR c.auth_at<a.starts_at
 OR c.task_id IS DISTINCT FROM NEW.task_id OR c.preparation_operation_id IS DISTINCT FROM NEW.preparation_operation_id
 OR NOT EXISTS(SELECT 1 FROM tasks t JOIN payment_authority_accepted_tasks_v1 accepted ON accepted.task_id=t.task_id
   JOIN payment_preparations pp ON pp.task_id=t.task_id AND pp.operation_id=NEW.preparation_operation_id
   JOIN transfer_drafts_v1 draft ON draft.owner_id=t.owner_id AND draft.draft_id=pp.draft_id AND draft.task_id=t.task_id
   JOIN payment_funds_execution_v1 funds ON funds.task_id=t.task_id AND funds.preparation_operation_id=pp.operation_id AND funds.execution_operation_id=NEW.execution_operation_id
   JOIN public_execution_requests request ON request.task_id=t.task_id AND request.operation_id=funds.execution_operation_id
   WHERE t.task_id=NEW.task_id AND t.owner_id=NEW.owner_id AND accepted.owner_id=t.owner_id
   AND accepted.run_id=a.run_id AND accepted.creation_digest=t.creation_digest AND accepted.task_created_at=t.created_at
   AND accepted.accepted_at>=a.starts_at AND accepted.accepted_at<a.ends_at AND t.created_at>=a.starts_at
   AND request.owner_id=NEW.owner_id AND request.business_operation_ref=NEW.business_operation_ref
   AND request.idempotency_key=NEW.original_key AND request.reserved_at>=accepted.accepted_at
   AND request.submitted_at=NEW.first_submitted_at)
 THEN RAISE EXCEPTION 'NEW_AUTHORITY_NEW_CONFIRMATION_AND_AUTH_REQUIRED'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER payment_authority_new_acceptance BEFORE INSERT ON payment_completion_candidate_grants
 FOR EACH ROW EXECUTE FUNCTION payment_authority_accept_guard_v1();

-- New acceptance alone enrolls; there is no historical INSERT SELECT/backfill.
CREATE OR REPLACE FUNCTION payment_auto_enroll_v1() RETURNS trigger
 LANGUAGE plpgsql SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
DECLARE a RECORD; r RECORD;
BEGIN
 SELECT * INTO a FROM payment_authority_continuations_v1 WHERE singleton=1;
 SELECT * INTO r FROM payment_auto_runs_v1 WHERE run_id=a.run_id AND enabled FOR SHARE;
 IF r.run_id IS NULL OR NOT payment_authority_window_v1()
 OR NEW.first_submitted_at<a.starts_at OR NEW.first_submitted_at>=a.ends_at
 THEN RAISE EXCEPTION 'NEW_AUTHORITY_ENROLLMENT_UNAVAILABLE'; END IF;
 INSERT INTO payment_auto_originals_v1(grant_id,run_id,result_id)
 VALUES(NEW.grant_id,r.run_id,'automatic_simulation_v1_'||encode(sha256(convert_to(NEW.grant_id,'UTF8')),'hex'));
 RETURN NEW;
END $$;

-- STABLE is deliberate: completion's existing STABLE authority reader calls it.
-- Existing begin/final/deferred guards still lock/recheck original mutable roots.
CREATE FUNCTION payment_authority_original_guard_v1(gid TEXT, expected_kind TEXT) RETURNS VOID
 LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
DECLARE a RECORD; member_run UUID; expected_role TEXT; g RECORD;
BEGIN
 SELECT * INTO a FROM payment_authority_continuations_v1 WHERE singleton=1;
 SELECT run_id INTO member_run FROM payment_auto_originals_v1 WHERE grant_id=gid;
 IF session_user::text IN(a.producer_role,a.worker_role) OR member_run=a.run_id THEN
  expected_role:=CASE expected_kind WHEN 'PRODUCER' THEN a.producer_role WHEN 'WORKER' THEN a.worker_role ELSE NULL END;
  SELECT * INTO g FROM payment_completion_candidate_grants WHERE grant_id=gid;
  IF expected_role IS NULL OR session_user::text<>expected_role OR member_run IS DISTINCT FROM a.run_id
  OR g.grant_id IS NULL OR g.first_submitted_at<a.starts_at OR g.first_submitted_at>=a.ends_at
  OR clock_timestamp()<a.starts_at OR clock_timestamp()>=a.ends_at
  OR NOT EXISTS(SELECT 1 FROM payment_auto_runs_v1 r WHERE r.run_id=a.run_id AND r.enabled AND r.producer_role=a.producer_role AND r.worker_role=a.worker_role)
  OR NOT EXISTS(SELECT 1 FROM payment_background_principals_v1 p JOIN payment_completion_candidate_fences f ON f.kind=p.kind AND f.authority_id=p.role_name
    WHERE p.role_name=a.producer_role AND p.active AND f.enabled AND p.epoch=a.epoch AND f.epoch=a.epoch AND clock_timestamp()<p.valid_until AND clock_timestamp()<f.valid_until)
  OR NOT EXISTS(SELECT 1 FROM payment_background_principals_v1 p JOIN payment_completion_candidate_fences f ON f.kind=p.kind AND f.authority_id=p.role_name
    WHERE p.role_name=a.worker_role AND p.active AND f.enabled AND p.epoch=a.epoch AND f.epoch=a.epoch AND clock_timestamp()<p.valid_until AND clock_timestamp()<f.valid_until)
  THEN RAISE EXCEPTION 'NEW_AUTHORITY_ORIGINAL_NOT_ASSIGNED'; END IF;
 END IF;
END $$;
REVOKE ALL ON FUNCTION payment_authority_window_v1(),payment_authority_accept_guard_v1(),payment_authority_original_guard_v1(TEXT,TEXT) FROM PUBLIC;

CREATE TABLE public_card_final_forward43_v1 (
 deployment_id UUID PRIMARY KEY REFERENCES public_card_final_deployment_v1(deployment_id),
 original_digest TEXT NOT NULL, from_digest TEXT NOT NULL, to_digest TEXT NOT NULL,
 predecessor_version BIGINT NOT NULL CHECK(predecessor_version=42),
 predecessor_checksum BYTEA NOT NULL,
 migration_version BIGINT NOT NULL CHECK(migration_version=43),
 migration_checksum BYTEA NOT NULL,
 recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER public_card_final_forward43_immutable BEFORE UPDATE OR DELETE ON public_card_final_forward43_v1
 FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
REVOKE ALL ON TABLE public_card_final_forward43_v1 FROM PUBLIC;

-- Original body retained; sole added entry gate.
CREATE OR REPLACE FUNCTION payment_background_original_v1(gid TEXT, expected_kind TEXT) RETURNS JSONB
 LANGUAGE plpgsql SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
DECLARE p RECORD; f RECORD; g RECORD; c RECORD; a RECORD; e JSONB; cat JSONB; current_source RECORD;
BEGIN
 PERFORM payment_authority_original_guard_v1(gid,expected_kind);
 SELECT * INTO p FROM payment_background_principals_v1 WHERE role_name=session_user AND kind=expected_kind;
 IF NOT FOUND THEN RAISE EXCEPTION 'BACKGROUND_IDENTITY_REQUIRED'; END IF;
 SELECT * INTO f FROM payment_completion_candidate_fences WHERE kind=p.kind AND authority_id=p.role_name::text FOR SHARE;
 IF NOT FOUND OR NOT f.enabled OR f.epoch<>p.epoch OR NOT p.active OR
 clock_timestamp()>=f.valid_until OR clock_timestamp()>=p.valid_until THEN RAISE EXCEPTION 'BACKGROUND_PRINCIPAL_REVOKED'; END IF;
 SELECT * INTO g FROM payment_completion_candidate_grants WHERE grant_id=gid;
 IF NOT FOUND THEN RAISE EXCEPTION 'BACKGROUND_ORIGINAL_UNAVAILABLE'; END IF;
 e:=g.evidence;
 IF g.evidence_digest IS DISTINCT FROM encode(sha256(convert_to(e::text,'UTF8')),'hex') OR
 g.policy_ref IS DISTINCT FROM 'sf-isolated-demo-policy-20261027-v1' OR e->>'schema' IS DISTINCT FROM 'payment-background-acceptance-v1' OR
 NOT f.scope @> jsonb_build_object('accounts',jsonb_build_array(jsonb_build_object('owner_id',g.owner_id,'account_id',e->>'payer_account_id'))) OR
 p.source_id IS DISTINCT FROM e->>'source_id' OR p.implementation IS DISTINCT FROM e->>'source_implementation' THEN RAISE EXCEPTION 'BACKGROUND_SCOPE_MISMATCH'; END IF;
 SELECT * INTO a FROM card_safety_account_fences_v1 WHERE owner_id=g.owner_id AND account_id=e->>'payer_account_id' FOR SHARE;
 IF NOT FOUND OR a.revoked OR a.epoch<>(e->>'account_epoch')::bigint THEN RAISE EXCEPTION 'BACKGROUND_ACCOUNT_REVOKED'; END IF;
 SELECT * INTO c FROM payment_completion_candidate_controls WHERE grant_id=gid FOR SHARE;
 IF NOT FOUND OR c.revoked OR c.paused OR c.grant_epoch<>g.grant_epoch THEN RAISE EXCEPTION 'BACKGROUND_GRANT_REVOKED'; END IF;
 -- Same order as first acceptance: authorization roots, account, source.
 PERFORM account_id FROM transfer_accounts_v1 WHERE account_id=e->>'payer_account_id' AND owner_id=g.owner_id FOR SHARE;
 IF NOT FOUND THEN RAISE EXCEPTION 'BACKGROUND_OWNER_MISMATCH'; END IF;
 SELECT * INTO current_source FROM public_unified_fictional_source_v1 WHERE singleton=1 FOR SHARE;
 IF NOT FOUND OR NOT current_source.active OR current_source.source_id<>e->>'source_id' OR
 current_source.source_revision<>e->>'source_revision' OR current_source.implementation_revision<>e->>'source_implementation' OR
 current_source.artifact_sha256<>e->>'source_sha256' THEN RAISE EXCEPTION 'BACKGROUND_SOURCE_WITHDRAWN'; END IF;
 SELECT to_jsonb(v) INTO cat FROM (SELECT owner_id,source_id,implementation_revision,ordinal,source_revision,catalog_digest FROM public_card_catalog_adoptions_v1
 WHERE account_id=e->>'payer_account_id' ORDER BY ordinal DESC LIMIT 1) v;
 IF cat IS NULL OR cat IS DISTINCT FROM e->'catalog_adoption' THEN RAISE EXCEPTION 'BACKGROUND_CATALOG_CHANGED'; END IF;
 IF (SELECT jsonb_agg(jsonb_build_object('card_id',v.card_id,'last_four',v.last_four) ORDER BY v.card_id) FROM card_catalog_v1 v WHERE v.account_id=e->>'payer_account_id') IS DISTINCT FROM
 (SELECT v->'cards' FROM jsonb_array_elements(convert_from(current_source.original_bytes,'UTF8')::jsonb->'card_catalogs') v WHERE v->>'owner'=g.owner_id AND v->>'account'=e->>'payer_account_id') THEN RAISE EXCEPTION 'BACKGROUND_CATALOG_INCOMPLETE'; END IF;
 IF payment_background_clock_v1()>=g.completion_until OR g.completion_until<>g.first_submitted_at+interval '30 minutes' THEN RAISE EXCEPTION 'BACKGROUND_EXPIRED'; END IF;
 IF NOT EXISTS(SELECT 1 FROM operations o JOIN public_execution_requests r ON r.operation_id=o.operation_id
 JOIN transfer_fund_orders_v1 b ON b.owner_id=r.owner_id AND b.operation_id=r.business_operation_ref
 WHERE o.operation_id=g.execution_operation_id AND o.state='PROCESSING' AND o.revision=3
 AND r.owner_id=g.owner_id AND r.task_id=g.task_id AND r.idempotency_key=g.original_key AND r.business_operation_ref=g.business_operation_ref
 AND r.submitted_at=g.first_submitted_at AND r.fingerprint_payload=e->'request_payload' AND r.request_digest=e->>'request_digest'
 AND b.state='PROCESSING' AND b.payer_account_id=e->>'payer_account_id' AND b.recipient_account_id=e->>'original_recipient_account_id'
 AND b.amount_cents=(e->>'amount_cents')::bigint) THEN RAISE EXCEPTION 'BACKGROUND_ORIGINAL_CONFLICT'; END IF;
 RETURN jsonb_build_object('grant_id',g.grant_id,'grant_epoch',g.grant_epoch,'evidence_digest',g.evidence_digest,
 'execution_operation_id',g.execution_operation_id,'task_id',g.task_id,'owner_id',g.owner_id,
 'business_operation_ref',g.business_operation_ref,'original_key',g.original_key,'completion_until',g.completion_until,
 'evidence',e,'source_epoch',p.epoch,'source_id',p.source_id,'source_implementation',p.implementation);
END $$;

-- Original body retained; sole added entry gate.
CREATE OR REPLACE FUNCTION payment_completion_authority_v1(gid TEXT, writing BOOLEAN) RETURNS JSONB
 LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp SET TimeZone TO 'UTC' AS $$
DECLARE g RECORD; e JSONB; p RECORD; f RECORD; c RECORD; d RECORD; producer RECORD; expected_original JSONB;
 src RECORD; raw JSONB; cat JSONB; stamp TIMESTAMPTZ:=payment_background_clock_v1();
BEGIN
 PERFORM payment_authority_original_guard_v1(gid,'WORKER');
 SELECT * INTO g FROM payment_completion_candidate_grants WHERE grant_id=gid;
 IF NOT FOUND THEN RAISE EXCEPTION 'COMPLETION_ORIGINAL_UNAVAILABLE'; END IF;
 e:=g.evidence;
 SELECT * INTO p FROM payment_background_principals_v1 WHERE role_name=session_user::text AND kind='WORKER';
 SELECT * INTO f FROM payment_completion_candidate_fences WHERE kind='WORKER' AND authority_id=session_user::text;
 IF p.role_name IS NULL OR f.authority_id IS NULL OR NOT p.active OR NOT f.enabled OR p.epoch<>f.epoch
 OR stamp>=p.valid_until OR stamp>=f.valid_until
 OR NOT EXISTS(SELECT 1 FROM payment_completion_logins_v1 WHERE role_name=session_user::text AND writer_version='completion-final-v1')
 OR p.source_id IS DISTINCT FROM e->>'source_id' OR p.implementation IS DISTINCT FROM e->>'source_implementation'
 OR NOT f.scope @> jsonb_build_object('accounts',jsonb_build_array(jsonb_build_object('owner_id',g.owner_id,'account_id',e->>'payer_account_id')))
 THEN RAISE EXCEPTION 'COMPLETION_WORKER_UNAUTHORIZED'; END IF;
 IF g.policy_ref IS DISTINCT FROM 'sf-isolated-demo-policy-20261027-v1'
 OR e->>'schema' IS DISTINCT FROM 'payment-background-acceptance-v1' OR e->>'mode' IS DISTINCT FROM 'PROJECT_SIMULATION'
 OR g.evidence_digest IS DISTINCT FROM encode(sha256(convert_to(e::text,'UTF8')),'hex')
 OR g.completion_until IS DISTINCT FROM g.first_submitted_at+interval '30 minutes'
 OR e->>'owner_id' IS DISTINCT FROM g.owner_id THEN RAISE EXCEPTION 'COMPLETION_GRANT_CORRUPT'; END IF;
 IF NOT EXISTS(SELECT 1 FROM card_safety_account_fences_v1 WHERE owner_id=g.owner_id AND account_id=e->>'payer_account_id'
 AND NOT revoked AND epoch=(e->>'account_epoch')::bigint) THEN RAISE EXCEPTION 'COMPLETION_ACCOUNT_UNAUTHORIZED'; END IF;
 SELECT * INTO src FROM public_unified_fictional_source_v1 WHERE singleton=1;
 IF NOT FOUND OR NOT src.active OR src.source_id IS DISTINCT FROM e->>'source_id'
 OR src.source_revision IS DISTINCT FROM e->>'source_revision' OR src.implementation_revision IS DISTINCT FROM e->>'source_implementation'
 OR src.artifact_sha256 IS DISTINCT FROM e->>'source_sha256'
 OR src.artifact_commit IS DISTINCT FROM '7c03f03b832aa17c174b1bbdd4a6ad3282e01422'
 OR src.artifact_sha256 IS DISTINCT FROM '85b6fc1b3c708c617586afc3e8ea61a78e525d0811439f267c93620238e8c49d'
 OR encode(sha256(src.original_bytes),'hex') IS DISTINCT FROM src.artifact_sha256
 THEN RAISE EXCEPTION 'COMPLETION_SOURCE_UNAVAILABLE'; END IF;
 raw:=convert_from(src.original_bytes,'UTF8')::jsonb;
 IF (SELECT jsonb_agg(jsonb_build_array(account_id,owner_id,opening_cents) ORDER BY account_id) FROM transfer_accounts_v1)
 IS DISTINCT FROM (SELECT jsonb_agg(jsonb_build_array(v->'account'->>'account_id',v->>'owner_id',((v->>'opening_balance')::numeric*100)::bigint) ORDER BY v->'account'->>'account_id') FROM jsonb_array_elements(raw->'transfer_plan'->'accounts') v)
 OR (SELECT jsonb_agg(jsonb_build_object('owner_id',owner_id,'payee_id',payee_id,'recipient_account_id',recipient_account_id) ORDER BY owner_id,payee_id) FROM transfer_payee_routes_v1)
 IS DISTINCT FROM jsonb_build_array(raw->'transfer_plan'->'route')
 THEN RAISE EXCEPTION 'COMPLETION_SOURCE_SCOPE_CHANGED'; END IF;
 SELECT to_jsonb(a) INTO cat FROM (SELECT owner_id,source_id,implementation_revision,ordinal,source_revision,catalog_digest
 FROM public_card_catalog_adoptions_v1 WHERE account_id=e->>'payer_account_id' ORDER BY ordinal DESC LIMIT 1) a;
 IF cat IS NULL OR cat IS DISTINCT FROM e->'catalog_adoption'
 OR (SELECT jsonb_agg(jsonb_build_object('card_id',card_id,'last_four',last_four) ORDER BY card_id) FROM card_catalog_v1 WHERE account_id=e->>'payer_account_id')
 IS DISTINCT FROM (SELECT v->'cards' FROM jsonb_array_elements(raw->'card_catalogs') v WHERE v->>'owner'=g.owner_id AND v->>'account'=e->>'payer_account_id')
 THEN RAISE EXCEPTION 'COMPLETION_CATALOG_CHANGED'; END IF;
 IF writing THEN
  SELECT * INTO c FROM payment_completion_candidate_controls WHERE execution_operation_id=g.execution_operation_id;
  IF NOT FOUND OR c.grant_id IS DISTINCT FROM gid OR c.grant_epoch<>g.grant_epoch OR c.paused OR c.revoked OR stamp>=g.completion_until
  THEN RAISE EXCEPTION 'COMPLETION_DELEGATION_INACTIVE'; END IF;
  SELECT * INTO d FROM payment_completion_candidate_decisions WHERE grant_id=gid;
  IF NOT FOUND OR d.decision_seq<>1 OR d.envelope_digest IS DISTINCT FROM encode(sha256(convert_to(d.envelope::text,'UTF8')),'hex')
  OR d.envelope->>'schema' IS DISTINCT FROM 'payment-simulation-decision-v1' OR d.envelope->>'mode' IS DISTINCT FROM 'PROJECT_SIMULATION'
  OR d.envelope->>'result_id' IS DISTINCT FROM d.result_id OR d.envelope->>'outcome' IS DISTINCT FROM d.outcome
  OR d.envelope->'decision_seq' IS DISTINCT FROM '1'::jsonb OR (d.envelope->>'fact_at')::timestamptz IS DISTINCT FROM d.fact_at
  OR d.envelope->'original'->>'grant_id' IS DISTINCT FROM gid
  OR d.envelope->'original'->>'evidence_digest' IS DISTINCT FROM g.evidence_digest
  OR d.envelope->'original'->'evidence' IS DISTINCT FROM e OR d.source_id IS DISTINCT FROM e->>'source_id'
  OR d.fact_at<g.first_submitted_at OR d.fact_at>=g.completion_until
  THEN RAISE EXCEPTION 'COMPLETION_DECISION_CORRUPT'; END IF;
  SELECT pp.* INTO producer FROM payment_background_principals_v1 pp JOIN payment_completion_candidate_fences ff
   ON ff.kind=pp.kind AND ff.authority_id=pp.role_name WHERE pp.kind='PRODUCER' AND pp.role_name=d.envelope->>'producer_role'
   AND pp.active AND ff.enabled AND pp.epoch=d.source_epoch AND ff.epoch=pp.epoch AND stamp<pp.valid_until AND stamp<ff.valid_until
   AND pp.source_id=d.source_id AND pp.implementation=e->>'source_implementation'
   AND ff.scope @> jsonb_build_object('accounts',jsonb_build_array(jsonb_build_object('owner_id',g.owner_id,'account_id',e->>'payer_account_id')));
  IF NOT FOUND THEN RAISE EXCEPTION 'COMPLETION_PRODUCER_UNAUTHORIZED'; END IF;
  expected_original:=jsonb_build_object('grant_id',g.grant_id,'grant_epoch',g.grant_epoch,'evidence_digest',g.evidence_digest,
   'execution_operation_id',g.execution_operation_id,'task_id',g.task_id,'owner_id',g.owner_id,
   'business_operation_ref',g.business_operation_ref,'original_key',g.original_key,'completion_until',g.completion_until,
   'evidence',e,'source_epoch',d.source_epoch,'source_id',producer.source_id,'source_implementation',producer.implementation);
  IF ((d.envelope->'original')-'completion_until') IS DISTINCT FROM (expected_original-'completion_until')
  OR (d.envelope->'original'->>'completion_until')::timestamptz IS DISTINCT FROM g.completion_until THEN RAISE EXCEPTION 'COMPLETION_DECISION_ORIGINAL_MISMATCH'; END IF;
 END IF;
 PERFORM payment_completion_accepted_v1(gid);
 RETURN jsonb_build_object('grant_id',g.grant_id,'grant_epoch',g.grant_epoch,'evidence_digest',g.evidence_digest,
 'execution_operation_id',g.execution_operation_id,'task_id',g.task_id,'owner_id',g.owner_id,
 'business_operation_ref',g.business_operation_ref,'original_key',g.original_key,'completion_until',g.completion_until,
 'evidence',e,'source_epoch',p.epoch,'source_id',p.source_id,'source_implementation',p.implementation);
END $$;

CREATE OR REPLACE FUNCTION public_card_final_expected_digest_v1(deployment UUID, original_digest TEXT) RETURNS TEXT
 LANGUAGE sql STABLE SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
 SELECT coalesce((SELECT f.to_digest FROM public_card_final_forward43_v1 f
 JOIN _sqlx_migrations m ON m.version=43 AND m.success AND m.checksum=f.migration_checksum
 JOIN _sqlx_migrations p ON p.version=42 AND p.success AND p.checksum=f.predecessor_checksum
 WHERE f.deployment_id=$1 AND f.original_digest=$2 AND f.from_digest=prior42.digest),prior42.digest)
 FROM (
 SELECT coalesce((SELECT d.to_digest FROM public_card_final_forward42_v1 d
  JOIN _sqlx_migrations m ON m.version=d.migration_version AND m.checksum=d.migration_checksum AND m.success
  JOIN _sqlx_migrations p ON p.version=d.predecessor_version AND p.checksum=d.predecessor_checksum AND p.success
  WHERE d.deployment_id=$1 AND d.original_digest=$2 AND d.from_digest=previous.digest),previous.digest)
 FROM (SELECT coalesce((SELECT c.to_digest FROM public_card_final_forward38_v1 c
  JOIN _sqlx_migrations m ON m.version=c.migration_version AND m.checksum=c.migration_checksum AND m.success
  WHERE c.deployment_id=$1 AND c.original_digest=$2 AND c.from_digest=prior.digest),prior.digest) AS digest
 FROM (SELECT coalesce((SELECT b.to_digest FROM public_card_final_forward37_v1 b
  JOIN _sqlx_migrations m ON m.version=b.migration_version AND m.checksum=b.migration_checksum AND m.success
  WHERE b.deployment_id=$1 AND b.original_digest=$2 AND b.from_digest=base.digest),base.digest) AS digest
 FROM (SELECT coalesce((SELECT a.to_digest FROM public_card_final_forward_bindings_v1 a
  JOIN _sqlx_migrations m ON m.version=a.migration_version AND m.checksum=a.migration_checksum AND m.success
  WHERE a.deployment_id=$1 AND a.from_digest=$2),$2) AS digest) base) prior) previous
 ) prior42(digest)
$$;
