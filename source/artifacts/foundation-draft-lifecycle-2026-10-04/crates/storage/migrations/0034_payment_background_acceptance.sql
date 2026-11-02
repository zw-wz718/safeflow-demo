-- Adopted local fiction only. Existing 0031 remains historical and byte-stable.
ALTER TABLE execution_permits DROP CONSTRAINT execution_permits_authentication_check;
ALTER TABLE execution_permits ADD CONSTRAINT execution_permits_authentication_check CHECK ((
 (storage_version=1 AND authentication_state='LEGACY_REQUIRED' AND required_gates IS NULL AND auth_grant_ref IS NOT NULL AND execution_scope<>'ISOLATED_CARD') OR
 (storage_version=2 AND authentication_state='NOT_REQUIRED' AND required_gates='["EXPLICIT_CONFIRMATION"]'::jsonb AND auth_grant_ref IS NULL AND execution_scope<>'ISOLATED_CARD') OR
 (storage_version=2 AND authentication_state='VERIFIED_REQUIRED' AND required_gates='["EXPLICIT_CONFIRMATION","STRONG_AUTHENTICATION"]'::jsonb AND auth_grant_ref IS NOT NULL AND auth_grant_ref !~ '^(sim_grant_|isolated_card_grant_|isolated_payment_grant_)' AND execution_scope<>'ISOLATED_CARD') OR
 (storage_version=3 AND authentication_state='ISOLATED_CARD_VERIFIED' AND required_gates='["EXPLICIT_CONFIRMATION","STRONG_AUTHENTICATION"]'::jsonb AND auth_grant_ref ~ '^isolated_card_grant_[0-9a-f-]{36}$' AND execution_scope='ISOLATED_CARD' AND tool_name='card_freeze_isolated') OR
 (storage_version=4 AND authentication_state='ISOLATED_PAYMENT_VERIFIED' AND required_gates='["EXPLICIT_CONFIRMATION","STRONG_AUTHENTICATION"]'::jsonb AND auth_grant_ref ~ '^isolated_payment_grant_[0-9a-f-]{36}$' AND execution_scope='TRANSFER' AND tool_name='execute_confirmed_payment')
) IS TRUE);

CREATE TABLE payment_background_consents_v1 (
 consent_id UUID PRIMARY KEY, preparation_operation_id TEXT NOT NULL UNIQUE REFERENCES operations(operation_id),
 task_id TEXT NOT NULL REFERENCES tasks(task_id), owner_id TEXT NOT NULL, account_id TEXT NOT NULL,
 session_digest TEXT NOT NULL, binding_version BIGINT NOT NULL CHECK(binding_version>0),
 session_epoch BIGINT NOT NULL CHECK(session_epoch>0), account_epoch BIGINT NOT NULL CHECK(account_epoch>0),
 display JSONB NOT NULL CHECK(jsonb_typeof(display)='object'), display_digest TEXT NOT NULL,
 created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
 confirmed_at TIMESTAMPTZ, confirmed_until TIMESTAMPTZ,
 challenge_id UUID, host_generation UUID, code_hash TEXT,
 challenge_at TIMESTAMPTZ, challenge_until TIMESTAMPTZ,
 failures SMALLINT NOT NULL DEFAULT 0 CHECK(failures BETWEEN 0 AND 5),
 auth_at TIMESTAMPTZ, auth_until TIMESTAMPTZ, auth_ref TEXT UNIQUE,
 CHECK((confirmed_at IS NULL AND confirmed_until IS NULL) OR confirmed_until=confirmed_at+interval '5 minutes'),
 CHECK((challenge_id IS NULL AND challenge_until IS NULL) OR challenge_until=challenge_at+interval '2 minutes'),
 CHECK((auth_ref IS NULL AND auth_at IS NULL AND auth_until IS NULL) OR
 (auth_ref ~ '^isolated_payment_grant_[0-9a-f-]{36}$' AND auth_until=auth_at+interval '2 minutes'))
);
CREATE FUNCTION payment_background_consent_guard_v1() RETURNS trigger LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
 IF TG_OP='DELETE' OR (to_jsonb(NEW)-'confirmed_at'-'confirmed_until'-'challenge_id'-'host_generation'-'code_hash'-'challenge_at'-'challenge_until'-'failures'-'auth_at'-'auth_until'-'auth_ref') IS DISTINCT FROM
 (to_jsonb(OLD)-'confirmed_at'-'confirmed_until'-'challenge_id'-'host_generation'-'code_hash'-'challenge_at'-'challenge_until'-'failures'-'auth_at'-'auth_until'-'auth_ref') OR
 OLD.confirmed_at IS NOT NULL AND (NEW.confirmed_at,NEW.confirmed_until) IS DISTINCT FROM (OLD.confirmed_at,OLD.confirmed_until) OR
 OLD.challenge_id IS NOT NULL AND (NEW.challenge_id,NEW.host_generation,NEW.code_hash,NEW.challenge_at,NEW.challenge_until) IS DISTINCT FROM (OLD.challenge_id,OLD.host_generation,OLD.code_hash,OLD.challenge_at,OLD.challenge_until) OR
 OLD.auth_ref IS NOT NULL AND NEW IS DISTINCT FROM OLD OR NEW.failures<OLD.failures OR NEW.failures>OLD.failures+1 THEN
 RAISE EXCEPTION 'BACKGROUND_CONSENT_IMMUTABLE'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER payment_background_consent_immutable BEFORE UPDATE OR DELETE ON payment_background_consents_v1
 FOR EACH ROW EXECUTE FUNCTION payment_background_consent_guard_v1();

-- Authenticated DB login + durable positive registration, not a caller label.
CREATE FUNCTION payment_background_clock_v1() RETURNS TIMESTAMPTZ LANGUAGE sql VOLATILE AS $$ SELECT clock_timestamp() $$;
CREATE TABLE payment_background_api_logins_v1(role_name TEXT PRIMARY KEY,active BOOLEAN NOT NULL);
CREATE FUNCTION payment_background_api_identity_v1() RETURNS BOOLEAN
 LANGUAGE sql SECURITY DEFINER SET search_path FROM CURRENT AS $$
 SELECT EXISTS(SELECT 1 FROM payment_background_api_logins_v1 a JOIN pg_roles r ON r.rolname=a.role_name
 WHERE a.role_name=session_user::text AND a.active AND NOT r.rolsuper AND NOT r.rolcreaterole AND NOT r.rolcreatedb AND NOT r.rolbypassrls)
 AND NOT has_schema_privilege(session_user,(SELECT relnamespace FROM pg_class WHERE oid='payment_background_api_logins_v1'::regclass),'CREATE')
 AND NOT has_table_privilege(session_user,'payment_completion_candidate_grants','INSERT,UPDATE,DELETE')
 AND NOT has_table_privilege(session_user,'payment_completion_candidate_decisions','INSERT,UPDATE,DELETE')
$$;
CREATE FUNCTION payment_background_accept_v1(cid UUID,body JSONB) RETURNS TEXT
 LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE c RECORD; p RECORD; r RECORD; a RECORD; f RECORD; src RECORD; cat JSONB; gid TEXT; d TEXT;
BEGIN
 IF NOT EXISTS(SELECT 1 FROM payment_background_api_logins_v1 WHERE role_name=session_user::text AND active) THEN RAISE EXCEPTION 'BACKGROUND_API_IDENTITY_REQUIRED'; END IF;
 SELECT * INTO c FROM payment_background_consents_v1 WHERE consent_id=cid FOR UPDATE;
 IF NOT FOUND OR c.confirmed_at IS NULL OR clock_timestamp()>=c.confirmed_until OR c.auth_ref IS NULL OR clock_timestamp()>=c.auth_until OR body->>'consent_id' IS DISTINCT FROM cid::text THEN RAISE EXCEPTION 'BACKGROUND_EXPLICIT_AUTH_REQUIRED'; END IF;
 SELECT * INTO f FROM card_safety_session_fences_v1 WHERE session_digest=c.session_digest FOR SHARE;
 IF NOT FOUND OR f.revoked OR f.owner_id<>c.owner_id OR f.binding_version<>c.binding_version OR f.epoch<>c.session_epoch THEN RAISE EXCEPTION 'BACKGROUND_SESSION_REVOKED'; END IF;
 SELECT * INTO f FROM card_safety_account_fences_v1 WHERE owner_id=c.owner_id AND account_id=c.account_id FOR SHARE;
 IF NOT FOUND OR f.revoked OR f.epoch<>c.account_epoch THEN RAISE EXCEPTION 'BACKGROUND_ACCOUNT_REVOKED'; END IF;
 SELECT * INTO a FROM payment_funds_execution_v1 WHERE preparation_operation_id=c.preparation_operation_id;
 IF NOT FOUND THEN RAISE EXCEPTION 'BACKGROUND_ASSOCIATION_REQUIRED'; END IF;
 SELECT * INTO r FROM public_execution_requests WHERE operation_id=a.execution_operation_id;
 IF NOT FOUND OR body IS NULL OR jsonb_typeof(body) IS DISTINCT FROM 'object' OR r.owner_id<>c.owner_id OR r.task_id<>c.task_id OR r.first_operation IS NULL OR
 r.fingerprint_payload IS DISTINCT FROM body->'request_payload' OR r.request_digest IS DISTINCT FROM body->>'request_digest' OR
 r.fingerprint_payload->'normalized_arguments' IS DISTINCT FROM c.display->'snapshot' OR body->'delegation_display' IS DISTINCT FROM c.display OR
 body->>'schema' IS DISTINCT FROM 'payment-background-acceptance-v1' OR body->>'mode' IS DISTINCT FROM 'PROJECT_SIMULATION' OR
 body->>'owner_id' IS DISTINCT FROM c.owner_id OR body->'snapshot' IS DISTINCT FROM c.display->'snapshot' OR
 body->>'payer_account_id' IS DISTINCT FROM c.account_id OR (body->>'account_epoch')::bigint IS DISTINCT FROM c.account_epoch THEN RAISE EXCEPTION 'BACKGROUND_REQUEST_CONFLICT'; END IF;
 SELECT * INTO p FROM execution_permits WHERE operation_id=r.operation_id;
 IF NOT FOUND OR p.storage_version<>4 OR p.authentication_state<>'ISOLATED_PAYMENT_VERIFIED' OR p.auth_grant_ref<>c.auth_ref OR p.status<>'CONSUMED' OR p.owner_id<>c.owner_id OR p.session_digest<>c.session_digest OR p.session_binding_version<>c.binding_version OR
 NOT EXISTS(SELECT 1 FROM operations WHERE operation_id=r.operation_id AND state='PROCESSING' AND revision=3) THEN RAISE EXCEPTION 'BACKGROUND_CLOSED_PERMIT_REQUIRED'; END IF;
 SELECT * INTO f FROM transfer_fund_orders_v1 WHERE owner_id=c.owner_id AND operation_id=r.business_operation_ref;
 IF NOT FOUND OR f.state<>'PROCESSING' OR f.payer_account_id<>c.account_id OR f.recipient_account_id IS DISTINCT FROM body->>'original_recipient_account_id' OR f.amount_cents IS DISTINCT FROM (body->>'amount_cents')::bigint THEN RAISE EXCEPTION 'BACKGROUND_HOLD_REQUIRED'; END IF;
 SELECT * INTO src FROM public_unified_fictional_source_v1 WHERE singleton=1 FOR SHARE;
 IF NOT FOUND OR NOT src.active OR src.source_id IS DISTINCT FROM body->>'source_id' OR src.source_revision IS DISTINCT FROM body->>'source_revision' OR src.implementation_revision IS DISTINCT FROM body->>'source_implementation' OR src.artifact_sha256 IS DISTINCT FROM body->>'source_sha256' THEN RAISE EXCEPTION 'BACKGROUND_SOURCE_REQUIRED'; END IF;
 SELECT to_jsonb(v) INTO cat FROM (SELECT owner_id,source_id,implementation_revision,ordinal,source_revision,catalog_digest FROM public_card_catalog_adoptions_v1 WHERE account_id=c.account_id ORDER BY ordinal DESC LIMIT 1) v;
 IF cat IS NULL OR cat IS DISTINCT FROM body->'catalog_adoption' THEN RAISE EXCEPTION 'BACKGROUND_CATALOG_REQUIRED'; END IF;
 -- A source/catalog wait must not extend the first explicit confirmation/auth.
 IF clock_timestamp()>=c.confirmed_until OR clock_timestamp()>=c.auth_until THEN RAISE EXCEPTION 'BACKGROUND_EXPLICIT_AUTH_EXPIRED_AFTER_WAIT'; END IF;
 gid:='background_'||cid::text;
 d:=encode(sha256(convert_to(body::text,'UTF8')),'hex');
 INSERT INTO payment_completion_candidate_grants(grant_id,owner_id,task_id,preparation_operation_id,execution_operation_id,business_operation_ref,original_key,grant_epoch,first_submitted_at,completion_until,evidence,evidence_digest,policy_ref)
 VALUES(gid,c.owner_id,c.task_id,c.preparation_operation_id,r.operation_id,r.business_operation_ref,r.idempotency_key,1,r.submitted_at,r.submitted_at+interval '30 minutes',body,d,'sf-isolated-demo-policy-20261027-v1');
 INSERT INTO payment_completion_candidate_controls(execution_operation_id,grant_id,grant_epoch,control_epoch,paused,revoked) VALUES(r.operation_id,gid,1,1,false,false);
 INSERT INTO payment_completion_candidate_jobs(grant_id) VALUES(gid);
 RETURN gid;
END $$;
CREATE FUNCTION payment_background_revoke_v1(op TEXT,owner_ref TEXT,session_hash TEXT,version BIGINT) RETURNS VOID
 LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE f RECORD;
BEGIN
 IF NOT EXISTS(SELECT 1 FROM payment_background_api_logins_v1 WHERE role_name=session_user::text AND active) THEN RAISE EXCEPTION 'BACKGROUND_API_IDENTITY_REQUIRED'; END IF;
 INSERT INTO card_safety_session_fences_v1(session_digest,owner_id,binding_version) VALUES(session_hash,owner_ref,version) ON CONFLICT DO NOTHING;
 SELECT * INTO f FROM card_safety_session_fences_v1 WHERE session_digest=session_hash AND owner_id=owner_ref FOR SHARE;
 IF NOT FOUND OR f.revoked OR f.binding_version<>version THEN RAISE EXCEPTION 'BACKGROUND_CURRENT_SESSION_REQUIRED'; END IF;
 IF NOT EXISTS(SELECT 1 FROM payment_completion_candidate_grants WHERE execution_operation_id=op AND owner_id=owner_ref) THEN RAISE EXCEPTION 'BACKGROUND_OWNER_MISMATCH'; END IF;
 PERFORM execution_operation_id FROM payment_completion_candidate_controls WHERE execution_operation_id=op FOR UPDATE;
 UPDATE payment_completion_candidate_controls SET revoked=true,control_epoch=control_epoch+1 WHERE execution_operation_id=op AND NOT revoked;
END $$;
CREATE TABLE payment_background_principals_v1 (
 role_name TEXT PRIMARY KEY, kind TEXT NOT NULL CHECK(kind IN ('PRODUCER','WORKER')),
 source_id TEXT NOT NULL CHECK(source_id='safeflow-team-simulation'),
 implementation TEXT NOT NULL CHECK(length(implementation)>0), epoch BIGINT NOT NULL CHECK(epoch>0),
 active BOOLEAN NOT NULL, valid_until TIMESTAMPTZ NOT NULL,
 FOREIGN KEY(kind,role_name) REFERENCES payment_completion_candidate_fences(kind,authority_id)
);
CREATE FUNCTION payment_background_identity_v1() RETURNS TABLE(kind TEXT,active BOOLEAN,epoch BIGINT)
 LANGUAGE sql SECURITY DEFINER SET search_path FROM CURRENT AS $$
 SELECT p.kind,(p.active AND f.enabled AND p.epoch=f.epoch AND clock_timestamp()<p.valid_until AND clock_timestamp()<f.valid_until),p.epoch
 FROM payment_background_principals_v1 p JOIN payment_completion_candidate_fences f ON f.kind=p.kind AND f.authority_id=p.role_name
 WHERE p.role_name=session_user::text
$$;
-- Return only after locking the same account authorization/control/source roots.
CREATE FUNCTION payment_background_original_v1(gid TEXT, expected_kind TEXT) RETURNS JSONB
 LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE p RECORD; f RECORD; g RECORD; c RECORD; a RECORD; e JSONB; cat JSONB; current_source RECORD;
BEGIN
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

CREATE FUNCTION payment_background_decide_v1(gid TEXT,rid TEXT,action TEXT) RETURNS JSONB
 LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE original JSONB; previous RECORD; body JSONB; stamp TIMESTAMPTZ; h TEXT; result JSONB;
BEGIN
 IF rid IS NULL OR length(btrim(rid)) NOT BETWEEN 1 AND 200 OR action IS NULL OR
 action NOT IN ('SETTLE_LOCAL_SIMULATION','DEFINITE_SIMULATED_DECLINE') THEN RAISE EXCEPTION 'BACKGROUND_DECISION_INVALID'; END IF;
 original:=payment_background_original_v1(gid,'PRODUCER');
 -- Serialize producer submissions for this original; immutable unique indexes remain final protection.
 PERFORM grant_id FROM payment_completion_candidate_grants WHERE grant_id=gid FOR UPDATE;
 -- Even a same-result replay may have waited for this grant lock. Revalidate
 -- the authoritative deadline and login before exposing the prior decision.
 IF payment_background_clock_v1()>=(original->>'completion_until')::timestamptz OR
 NOT EXISTS(SELECT 1 FROM payment_background_identity_v1() WHERE kind='PRODUCER' AND active) THEN RAISE EXCEPTION 'BACKGROUND_EXPIRED_AFTER_WAIT'; END IF;
 SELECT * INTO previous FROM payment_completion_candidate_decisions WHERE grant_id=gid OR
 (source_id=original->>'source_id' AND source_epoch=(original->>'source_epoch')::bigint AND result_id=rid);
 IF FOUND THEN
 IF previous.grant_id<>gid OR previous.result_id<>rid OR previous.outcome<>action OR
 previous.source_epoch<>(original->>'source_epoch')::bigint OR previous.envelope->>'producer_role'<>session_user::text THEN
 RAISE EXCEPTION 'BACKGROUND_DECISION_CONFLICT'; END IF;
 RETURN to_jsonb(previous);
 END IF;
 stamp:=payment_background_clock_v1();
 IF stamp>=(original->>'completion_until')::timestamptz THEN RAISE EXCEPTION 'BACKGROUND_EXPIRED'; END IF;
 IF NOT EXISTS(SELECT 1 FROM payment_background_identity_v1() WHERE kind='PRODUCER' AND active) THEN RAISE EXCEPTION 'BACKGROUND_PRINCIPAL_EXPIRED_AFTER_WAIT'; END IF;
 body:=jsonb_build_object('schema','payment-simulation-decision-v1','mode','PROJECT_SIMULATION',
 'original',original,'producer_role',session_user::text,'result_id',rid,'decision_seq',1,'outcome',action,'fact_at',stamp);
 h:=encode(sha256(convert_to(body::text,'UTF8')),'hex');
 INSERT INTO payment_completion_candidate_decisions(decision_id,grant_id,source_id,source_epoch,result_id,decision_seq,outcome,envelope,envelope_digest,fact_at,received_at)
 VALUES('decision_'||gid,gid,original->>'source_id',(original->>'source_epoch')::bigint,rid,1,action,body,h,stamp,payment_background_clock_v1()) RETURNING to_jsonb(payment_completion_candidate_decisions.*) INTO result;
 RETURN result;
END $$;

CREATE FUNCTION payment_background_worker_v1(gid TEXT) RETURNS JSONB
 LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE original JSONB; d RECORD; j RECORD; stamp TIMESTAMPTZ; producer RECORD; facts JSONB;
BEGIN
 original:=payment_background_original_v1(gid,'WORKER');
 SELECT * INTO d FROM payment_completion_candidate_decisions WHERE grant_id=gid;
 IF NOT FOUND THEN RETURN jsonb_build_object('state','WAITING_TRUSTED_DECISION','business_executed',false); END IF;
 IF d.envelope_digest<>encode(sha256(convert_to(d.envelope::text,'UTF8')),'hex') OR d.decision_seq<>1 OR
 d.envelope->'original'->>'evidence_digest'<>original->>'evidence_digest' THEN RAISE EXCEPTION 'BACKGROUND_DECISION_CORRUPT'; END IF;
 -- Recheck producer's exact epoch and scope; worker epoch is independent.
 SELECT * INTO producer FROM payment_completion_candidate_fences WHERE kind='PRODUCER' AND authority_id=d.envelope->>'producer_role' FOR SHARE;
 IF NOT FOUND OR NOT producer.enabled OR producer.epoch<>d.source_epoch OR clock_timestamp()>=producer.valid_until THEN RAISE EXCEPTION 'BACKGROUND_DECISION_SOURCE_REVOKED'; END IF;
 IF NOT EXISTS(SELECT 1 FROM payment_background_principals_v1 p JOIN payment_completion_candidate_fences f ON f.kind=p.kind AND f.authority_id=p.role_name::text
 WHERE p.role_name=(d.envelope->>'producer_role')::name AND p.kind='PRODUCER' AND p.active AND f.enabled AND p.epoch=d.source_epoch AND f.epoch=p.epoch
 AND p.source_id=d.source_id AND p.implementation=original->>'source_implementation'
 AND clock_timestamp()<p.valid_until AND clock_timestamp()<f.valid_until) THEN RAISE EXCEPTION 'BACKGROUND_DECISION_SOURCE_REVOKED'; END IF;
 SELECT jsonb_build_object('facts_revision',v.revision,'cards',
 (SELECT jsonb_agg(jsonb_build_object('card_id',c.card_id,'last_four',c.last_four) ORDER BY c.card_id) FROM card_catalog_v1 c WHERE c.account_id=original->'evidence'->>'payer_account_id'),
 'active_locks',(SELECT coalesce(jsonb_agg(to_jsonb(l) ORDER BY l.lock_id),'[]'::jsonb) FROM card_locks_v1 l JOIN card_catalog_v1 c ON c.card_id=l.card_id WHERE c.account_id=original->'evidence'->>'payer_account_id' AND l.state='ACTIVE')) INTO facts
 FROM card_account_versions_v1 v WHERE v.account_id=original->'evidence'->>'payer_account_id';
 IF facts IS NULL THEN RAISE EXCEPTION 'BACKGROUND_CARD_FACTS_UNAVAILABLE'; END IF;
 SELECT * INTO j FROM payment_completion_candidate_jobs WHERE grant_id=gid FOR UPDATE;
 IF NOT FOUND THEN RAISE EXCEPTION 'BACKGROUND_JOB_MISSING'; END IF;
 stamp:=payment_background_clock_v1();
 IF stamp>=(original->>'completion_until')::timestamptz THEN RAISE EXCEPTION 'BACKGROUND_EXPIRED'; END IF;
 IF NOT EXISTS(SELECT 1 FROM payment_background_identity_v1() WHERE kind='WORKER' AND active) OR clock_timestamp()>=producer.valid_until THEN RAISE EXCEPTION 'BACKGROUND_PRINCIPAL_EXPIRED_AFTER_WAIT'; END IF;
 IF NOT EXISTS(SELECT 1 FROM payment_background_principals_v1 p WHERE p.role_name=d.envelope->>'producer_role' AND p.kind='PRODUCER' AND p.active AND p.epoch=d.source_epoch AND clock_timestamp()<p.valid_until) THEN RAISE EXCEPTION 'BACKGROUND_DECISION_SOURCE_EXPIRED_AFTER_WAIT'; END IF;
 IF j.lease_until IS NOT NULL AND stamp<j.lease_until THEN RETURN jsonb_build_object('state','LEASE_BUSY','business_executed',false); END IF;
 UPDATE payment_completion_candidate_jobs SET lease_epoch=lease_epoch+1,lease_owner=session_user::text,leased_at=stamp,lease_until=LEAST(stamp+interval '30 seconds',(original->>'completion_until')::timestamptz) WHERE grant_id=gid RETURNING * INTO j;
 RETURN jsonb_build_object('state','READY_FOR_STEP08','original',original,'decision',to_jsonb(d),'lease',to_jsonb(j),'card_facts',facts,'business_executed',false);
END $$;
REVOKE ALL ON TABLE payment_background_consents_v1,payment_background_principals_v1,payment_background_api_logins_v1 FROM PUBLIC;
REVOKE ALL ON FUNCTION payment_background_consent_guard_v1(),payment_background_original_v1(TEXT,TEXT),
 payment_background_decide_v1(TEXT,TEXT,TEXT),payment_background_worker_v1(TEXT),payment_background_identity_v1(),payment_background_clock_v1(),
 payment_background_accept_v1(UUID,JSONB),payment_background_revoke_v1(TEXT,TEXT,TEXT,BIGINT),payment_background_api_identity_v1() FROM PUBLIC;

-- Pin every new routine to catalog builtins, this deployment's trusted schema,
-- then explicitly pg_temp. An implicit pg_temp would precede trusted relations.
-- Product logins receive USAGE only, never CREATE on the trusted schema.
DO $$
DECLARE ns TEXT:=current_schema(); f RECORD;
BEGIN
 FOR f IN SELECT p.proname,pg_get_function_identity_arguments(p.oid) AS args
 FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
 WHERE n.nspname=ns AND (p.proname LIKE 'payment_background_%' OR p.proname='completion_candidate_original_v1')
 LOOP
  EXECUTE format('ALTER FUNCTION %I.%I(%s) SET search_path TO pg_catalog,%I,pg_temp',ns,f.proname,f.args,ns);
 END LOOP;
END $$;
