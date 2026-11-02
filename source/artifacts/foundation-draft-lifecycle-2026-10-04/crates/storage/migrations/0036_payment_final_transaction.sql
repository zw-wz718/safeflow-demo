-- Additive completion, opt-in per existing WORKER login. No business rewrite.
-- __SCHEMA__ is the already validated private namespace, not client input.
CREATE TABLE payment_completion_logins_v1 (
 role_name TEXT PRIMARY KEY REFERENCES payment_background_principals_v1(role_name),
 writer_version TEXT NOT NULL CHECK(writer_version='completion-final-v1')
);
-- A maintenance-only append records a pre-migration-verified card binding.
-- Old deployment rows/digests and old binaries remain untouched/fail closed.
CREATE TABLE public_card_final_forward_bindings_v1 (
 deployment_id UUID PRIMARY KEY REFERENCES public_card_final_deployment_v1(deployment_id),
 from_digest TEXT NOT NULL, to_digest TEXT NOT NULL,
 migration_version BIGINT NOT NULL CHECK(migration_version=36),
 migration_checksum BYTEA NOT NULL, recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER public_card_final_forward_immutable BEFORE UPDATE OR DELETE ON public_card_final_forward_bindings_v1
 FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
REVOKE ALL ON TABLE public_card_final_forward_bindings_v1 FROM PUBLIC;
CREATE FUNCTION public_card_final_expected_digest_v1(deployment UUID, original_digest TEXT) RETURNS TEXT
 LANGUAGE sql STABLE SECURITY DEFINER SET search_path FROM CURRENT AS $$
 SELECT coalesce((SELECT b.to_digest FROM public_card_final_forward_bindings_v1 b
 JOIN _sqlx_migrations m ON m.version=b.migration_version AND m.checksum=b.migration_checksum AND m.success
 WHERE b.deployment_id=deployment AND b.from_digest=original_digest),original_digest)
$$;
ALTER FUNCTION public_card_final_expected_digest_v1(UUID,TEXT) SET search_path TO pg_catalog,__SCHEMA__,pg_temp;
CREATE TABLE payment_completion_transactions_v1 (
 transaction_id BIGINT PRIMARY KEY, backend_pid INTEGER NOT NULL,
 grant_id TEXT NOT NULL UNIQUE REFERENCES payment_completion_candidate_grants(grant_id),
 worker_role TEXT NOT NULL REFERENCES payment_completion_logins_v1(role_name),
 lease_epoch BIGINT NOT NULL, worker_epoch BIGINT NOT NULL, control_epoch BIGINT NOT NULL,
 original JSONB NOT NULL, decision JSONB NOT NULL,
 accounts_before JSONB NOT NULL, other_holds JSONB NOT NULL,
 started_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER payment_completion_context_immutable BEFORE UPDATE OR DELETE ON payment_completion_transactions_v1
 FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
REVOKE ALL ON TABLE payment_completion_logins_v1,payment_completion_transactions_v1 FROM PUBLIC;

-- SQL STABLE readers use the OUTER statement's one RC snapshot. Clock is read
-- once; writes additionally keep every mutable authority root locked to COMMIT.
CREATE FUNCTION payment_completion_authority_v1(gid TEXT, writing BOOLEAN) RETURNS JSONB
 LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE g RECORD; e JSONB; p RECORD; f RECORD; c RECORD; d RECORD; producer RECORD; expected_original JSONB;
 src RECORD; raw JSONB; cat JSONB; stamp TIMESTAMPTZ:=payment_background_clock_v1();
BEGIN
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

CREATE FUNCTION payment_completion_rows_v1(tab TEXT, v JSONB) RETURNS BOOLEAN
 LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE c RECORD; e JSONB;
BEGIN
 -- This policy never grants a privilege. Existing API/card readers keep their
 -- existing GRANTs; all WORKER principals, including revoked ones, are scoped.
 IF NOT EXISTS(SELECT 1 FROM payment_background_principals_v1 WHERE role_name=session_user::text AND kind='WORKER')
 AND NOT EXISTS(SELECT 1 FROM payment_completion_logins_v1 WHERE role_name=session_user::text) THEN RETURN true; END IF;
 SELECT * INTO c FROM payment_completion_transactions_v1 WHERE transaction_id=txid_current_if_assigned() AND backend_pid=pg_backend_pid() AND worker_role=session_user::text;
 IF NOT FOUND OR NOT EXISTS(SELECT 1 FROM payment_background_identity_v1() WHERE kind='WORKER' AND active) THEN RETURN false; END IF;
 e:=c.original->'evidence';
 RETURN CASE tab
 WHEN 'transfer_accounts_v1' THEN v->>'account_id' IN(e->>'payer_account_id',e->>'original_recipient_account_id')
 WHEN 'transfer_fund_orders_v1' THEN v->>'payer_account_id' IN(e->>'payer_account_id',e->>'original_recipient_account_id') OR v->>'recipient_account_id' IN(e->>'payer_account_id',e->>'original_recipient_account_id')
 WHEN 'transfer_ledger_v1' THEN v->>'account_id' IN(e->>'payer_account_id',e->>'original_recipient_account_id')
 WHEN 'transfer_business_orders_v1' THEN v->>'owner_id'=c.original->>'owner_id' AND v->>'operation_id'=c.original->>'business_operation_ref'
 WHEN 'transfer_drafts_v1' THEN v->>'owner_id'=c.original->>'owner_id' AND v->>'draft_id'=e->'snapshot'->'transfer'->>'draft_id'
 WHEN 'public_execution_requests' THEN v->>'operation_id'=c.original->>'execution_operation_id'
 WHEN 'execution_permits' THEN v->>'operation_id'=c.original->>'execution_operation_id'
 ELSE v->>'task_id'=c.original->>'task_id' END;
END $$;
-- Each policy is restricted to existing table privileges; no worker table or
-- row can be read without the nonforgeable backend+txid context.
DO $$ DECLARE t TEXT; BEGIN
 FOREACH t IN ARRAY ARRAY['tasks','operations','public_execution_requests','payment_preparations','payment_funds_execution_v1','server_confirmations','execution_permits','mock_draft_lifecycle_plans','transfer_drafts_v1','transfer_business_orders_v1','transfer_fund_orders_v1','transfer_accounts_v1','transfer_ledger_v1','audit_events'] LOOP
  EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY',t);
  EXECUTE format('CREATE POLICY completion_scope_v1 ON %I USING (payment_completion_rows_v1(%L,to_jsonb(%I.*))) WITH CHECK (payment_completion_rows_v1(%L,to_jsonb(%I.*)))',t,t,t,t,t);
 END LOOP;
END $$;

CREATE FUNCTION payment_completion_begin_v1(gid TEXT, lease BIGINT) RETURNS JSONB
 LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE g RECORD; d RECORD; j RECORD; original JSONB; accounts JSONB; holds JSONB; stamp TIMESTAMPTZ;
BEGIN
 IF current_setting('transaction_isolation')<>'read committed' OR current_setting('transaction_read_only')<>'off'
 OR has_schema_privilege(session_user,'__SCHEMA__','CREATE') OR has_database_privilege(session_user,current_database(),'TEMP')
 OR EXISTS(SELECT 1 FROM pg_class WHERE relnamespace=pg_my_temp_schema())
 THEN RAISE EXCEPTION 'COMPLETION_TRANSACTION_SCOPE'; END IF;
 SELECT * INTO g FROM payment_completion_candidate_grants WHERE grant_id=gid;
 SELECT * INTO d FROM payment_completion_candidate_decisions WHERE grant_id=gid;
 IF g.grant_id IS NULL OR d.decision_id IS NULL THEN RAISE EXCEPTION 'COMPLETION_ORIGINAL_UNAVAILABLE'; END IF;
 -- All authority roots first. Producer precedes account protection, never
 -- taken after accounts (a producer may itself be waiting on the payer).
 PERFORM authority_id FROM payment_completion_candidate_fences WHERE kind='WORKER' AND authority_id=session_user::text FOR SHARE;
 PERFORM authority_id FROM payment_completion_candidate_fences WHERE kind='PRODUCER' AND authority_id=d.envelope->>'producer_role' FOR SHARE;
 PERFORM account_id FROM card_safety_account_fences_v1 WHERE owner_id=g.owner_id AND account_id=g.evidence->>'payer_account_id' FOR SHARE;
 PERFORM grant_id FROM payment_completion_candidate_controls WHERE grant_id=gid FOR SHARE;
 PERFORM pg_advisory_xact_lock_shared(hashtextextended('__SCHEMA__billing-recovery-install-v2',0));
 -- Rust serde_json emits compact arrays; jsonb::text inserts spaces. These
 -- public/member locks use their original compact key bytes.
 PERFORM pg_advisory_xact_lock(hashtextextended('__SCHEMA__'||format('["public-owner-execution-v1",%s]',to_jsonb(g.owner_id)),0));
 PERFORM task_id FROM tasks WHERE task_id=g.task_id AND owner_id=g.owner_id FOR UPDATE;
 IF NOT FOUND THEN RAISE EXCEPTION 'COMPLETION_OWNER_CONFLICT'; END IF;
 PERFORM pg_advisory_xact_lock(hashtextextended(format('["transfer-business-key-v1",%s,%s]',to_jsonb(g.owner_id),to_jsonb(g.original_key)),0));
 PERFORM pg_advisory_xact_lock(hashtextextended(format('["transfer-business-operation-v1",%s,%s]',to_jsonb(g.owner_id),to_jsonb(g.business_operation_ref)),0));
 PERFORM operation_id FROM transfer_business_orders_v1 WHERE owner_id=g.owner_id AND operation_id=g.business_operation_ref FOR UPDATE;
 PERFORM operation_id FROM transfer_fund_orders_v1 WHERE owner_id=g.owner_id AND operation_id=g.business_operation_ref FOR UPDATE;
 PERFORM account_id FROM transfer_accounts_v1 WHERE account_id IN(g.evidence->>'payer_account_id',g.evidence->>'original_recipient_account_id') ORDER BY account_id FOR UPDATE;
 PERFORM singleton FROM public_unified_fictional_source_v1 WHERE singleton=1 FOR SHARE;
 -- The shared account protection is also honored by card/catalog writers.
 SELECT * INTO j FROM payment_completion_candidate_jobs WHERE grant_id=gid FOR UPDATE;
 IF EXISTS(SELECT 1 FROM payment_completion_candidate_receipts WHERE grant_id=gid) THEN
  RETURN jsonb_build_object('state','COMPLETED'); END IF;
 original:=payment_completion_authority_v1(gid,true);
 stamp:=payment_background_clock_v1();
 IF j.lease_owner IS DISTINCT FROM session_user::text OR j.lease_epoch IS DISTINCT FROM lease OR stamp>=j.lease_until
 OR j.lease_until>(original->>'completion_until')::timestamptz THEN RAISE EXCEPTION 'COMPLETION_LEASE_STALE'; END IF;
 IF NOT EXISTS(SELECT 1 FROM operations WHERE operation_id=g.execution_operation_id AND state='PROCESSING' AND revision=3)
 OR NOT EXISTS(SELECT 1 FROM transfer_fund_orders_v1 WHERE owner_id=g.owner_id AND operation_id=g.business_operation_ref AND state='PROCESSING')
 THEN RAISE EXCEPTION 'COMPLETION_RECONCILE_REQUIRED'; END IF;
 SELECT jsonb_agg(to_jsonb(a) ORDER BY account_id) INTO accounts FROM transfer_accounts_v1 a
 WHERE account_id IN(g.evidence->>'payer_account_id',g.evidence->>'original_recipient_account_id');
 SELECT coalesce(jsonb_agg(to_jsonb(f) ORDER BY owner_id,operation_id),'[]'::jsonb) INTO holds FROM transfer_fund_orders_v1 f
 WHERE payer_account_id IN(g.evidence->>'payer_account_id',g.evidence->>'original_recipient_account_id') AND (owner_id,operation_id)<>(g.owner_id,g.business_operation_ref) AND state='PROCESSING';
 IF EXISTS(SELECT 1 FROM transfer_ledger_v1 WHERE business_owner_id=g.owner_id AND operation_id=g.business_operation_ref)
 THEN RAISE EXCEPTION 'COMPLETION_PROCESSING_HAS_LEDGER'; END IF;
 INSERT INTO payment_completion_transactions_v1(transaction_id,backend_pid,grant_id,worker_role,lease_epoch,worker_epoch,control_epoch,original,decision,accounts_before,other_holds)
 SELECT txid_current(),pg_backend_pid(),gid,session_user::text,lease,(original->>'source_epoch')::bigint,c.control_epoch,original,to_jsonb(d),accounts,holds
 FROM payment_completion_candidate_controls c WHERE c.grant_id=gid;
 RETURN jsonb_build_object('state','ADMITTED','original',original,'decision',to_jsonb(d));
END $$;

CREATE FUNCTION payment_completion_outgoing_v1(gid TEXT) RETURNS BOOLEAN
 LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE o JSONB;
BEGIN
 o:=payment_completion_authority_v1(gid,true);
 IF NOT EXISTS(SELECT 1 FROM payment_completion_transactions_v1 WHERE transaction_id=txid_current_if_assigned() AND backend_pid=pg_backend_pid() AND worker_role=session_user::text AND grant_id=gid)
 THEN RAISE EXCEPTION 'COMPLETION_CONTEXT_REQUIRED'; END IF;
 IF NOT EXISTS(SELECT 1 FROM card_account_versions_v1 WHERE account_id=o->'evidence'->>'payer_account_id')
 THEN RAISE EXCEPTION 'COMPLETION_CARD_FACTS_UNAVAILABLE'; END IF;
 RETURN NOT EXISTS(SELECT 1 FROM card_locks_v1 l JOIN card_catalog_v1 c ON c.card_id=l.card_id WHERE c.account_id=o->'evidence'->>'payer_account_id' AND l.state='ACTIVE');
END $$;

-- All actual writers, including old API/session binaries, hit these gates.
-- The only worker writes are the member's exact deltas for this original.
CREATE FUNCTION payment_completion_mutation_v1() RETURNS trigger
 LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE c RECORD; g payment_completion_candidate_grants%ROWTYPE; v JSONB; oldv JSONB; target TEXT; before_row JSONB; delta BIGINT; wanted TEXT; worker BOOLEAN;
BEGIN
 worker:=EXISTS(SELECT 1 FROM payment_background_principals_v1 WHERE role_name=session_user::text AND kind='WORKER') OR EXISTS(SELECT 1 FROM payment_completion_logins_v1 WHERE role_name=session_user::text);
 v:=CASE WHEN TG_OP='DELETE' THEN to_jsonb(OLD) ELSE to_jsonb(NEW) END;
 oldv:=CASE WHEN TG_OP='INSERT' THEN NULL ELSE to_jsonb(OLD) END;
 IF TG_TABLE_NAME='transfer_accounts_v1' THEN
  IF NOT worker THEN RETURN CASE WHEN TG_OP='DELETE' THEN OLD ELSE NEW END; END IF;
 ELSIF TG_TABLE_NAME='operations' THEN
  SELECT * INTO g FROM payment_completion_candidate_grants WHERE execution_operation_id=v->>'operation_id';
 ELSE
  SELECT * INTO g FROM payment_completion_candidate_grants WHERE owner_id=coalesce(v->>'owner_id',v->>'business_owner_id') AND business_operation_ref=v->>'operation_id';
 END IF;
 IF NOT worker AND g.grant_id IS NULL THEN RETURN CASE WHEN TG_OP='DELETE' THEN OLD ELSE NEW END; END IF;
 SELECT * INTO c FROM payment_completion_transactions_v1 WHERE transaction_id=txid_current_if_assigned() AND backend_pid=pg_backend_pid() AND worker_role=session_user::text;
 IF NOT FOUND OR NOT worker OR (g.grant_id IS NOT NULL AND g.grant_id<>c.grant_id) OR TG_OP='DELETE'
 THEN RAISE EXCEPTION 'COMPLETION_CLOSED_WRITER_REQUIRED'; END IF;
 PERFORM payment_completion_authority_v1(c.grant_id,true);
 wanted:=CASE c.decision->>'outcome' WHEN 'SETTLE_LOCAL_SIMULATION' THEN 'SUCCEEDED' WHEN 'DEFINITE_SIMULATED_DECLINE' THEN 'FAILED' END;
 IF wanted IS NULL THEN RAISE EXCEPTION 'COMPLETION_NO_DECISION'; END IF;
 IF TG_TABLE_NAME='transfer_accounts_v1' THEN
  SELECT a INTO before_row FROM jsonb_array_elements(c.accounts_before) a WHERE a->>'account_id'=v->>'account_id';
  delta:=(c.original->'evidence'->>'amount_cents')::bigint * CASE WHEN v->>'account_id'=c.original->'evidence'->>'payer_account_id' THEN -1 ELSE 1 END;
  IF wanted<>'SUCCEEDED' OR TG_OP<>'UPDATE' OR before_row IS NULL OR oldv IS DISTINCT FROM before_row
  OR (v-'booked_cents') IS DISTINCT FROM (before_row-'booked_cents') OR (v->>'booked_cents')::bigint<>(before_row->>'booked_cents')::bigint+delta
  THEN RAISE EXCEPTION 'COMPLETION_ACCOUNT_DELTA'; END IF;
 ELSIF TG_TABLE_NAME='transfer_ledger_v1' THEN
  IF TG_OP<>'INSERT' OR wanted<>'SUCCEEDED' OR v->>'operation_id' IS DISTINCT FROM c.original->>'business_operation_ref'
  OR v->>'business_owner_id' IS DISTINCT FROM c.original->>'owner_id' OR (v->>'amount_cents')::bigint<>(c.original->'evidence'->>'amount_cents')::bigint
  OR NOT ((v->>'account_id'=c.original->'evidence'->>'payer_account_id' AND v->>'direction'='DEBIT') OR (v->>'account_id'=c.original->'evidence'->>'original_recipient_account_id' AND v->>'direction'='CREDIT'))
  THEN RAISE EXCEPTION 'COMPLETION_LEDGER_SCOPE'; END IF;
 ELSIF TG_TABLE_NAME='transfer_fund_orders_v1' THEN
  IF v->>'owner_id' IS DISTINCT FROM c.original->>'owner_id' OR v->>'operation_id' IS DISTINCT FROM c.original->>'business_operation_ref'
  OR TG_OP<>'UPDATE' OR oldv->>'state'<>'PROCESSING' OR v->>'state' IS DISTINCT FROM wanted OR (v-'state') IS DISTINCT FROM (oldv-'state')
  THEN RAISE EXCEPTION 'COMPLETION_FUND_TRANSITION'; END IF;
 ELSIF TG_TABLE_NAME='transfer_business_orders_v1' THEN
  IF TG_OP<>'UPDATE' OR oldv->'latest_receipt'->>'status'<>'PROCESSING' OR v->'latest_receipt'->>'status' IS DISTINCT FROM wanted
  OR (v-'latest_receipt'-'latest_recorded_at') IS DISTINCT FROM (oldv-'latest_receipt'-'latest_recorded_at')
  OR ((v->'latest_receipt')-'status'-'reason_code'-'completed_at') IS DISTINCT FROM ((oldv->'first_receipt')-'status'-'reason_code'-'completed_at')
  OR (wanted='FAILED' AND (v->'latest_receipt'->>'reason_code' IS DISTINCT FROM 'MOCK_BANK_DECLINED' OR v->'latest_receipt'->>'completed_at' IS NOT NULL))
  OR (wanted='SUCCEEDED' AND (v->'latest_receipt'->>'reason_code' IS NOT NULL OR v->'latest_receipt'->>'completed_at' IS NULL))
  THEN RAISE EXCEPTION 'COMPLETION_BUSINESS_TRANSITION'; END IF;
 ELSIF TG_TABLE_NAME='operations' THEN
  IF TG_OP<>'UPDATE' OR oldv->>'state'<>'PROCESSING' OR oldv->>'revision'<>'3' OR v->>'state' IS DISTINCT FROM wanted OR v->>'revision'<>'4'
  OR (v-'state'-'revision') IS DISTINCT FROM (oldv-'state'-'revision') THEN RAISE EXCEPTION 'COMPLETION_PUBLIC_TRANSITION'; END IF;
 END IF;
 RETURN NEW;
END $$;
DO $$ DECLARE t TEXT; BEGIN
 FOREACH t IN ARRAY ARRAY['transfer_accounts_v1','transfer_fund_orders_v1','transfer_business_orders_v1','transfer_ledger_v1','operations'] LOOP
  EXECUTE format('CREATE TRIGGER completion_closed_writer_v1 BEFORE INSERT OR UPDATE OR DELETE ON %I FOR EACH ROW EXECUTE FUNCTION payment_completion_mutation_v1()',t);
 END LOOP;
END $$;

CREATE FUNCTION payment_completion_bundle_v1(gid TEXT) RETURNS JSONB
 LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE g RECORD; d RECORD; r RECORD; o RECORD; b RECORD; f RECORD; a RECORD; outbox RECORD; ctx RECORD; wanted TEXT; n BIGINT; matching BIGINT; body JSONB;
BEGIN
 SELECT * INTO g FROM payment_completion_candidate_grants WHERE grant_id=gid;
 SELECT * INTO d FROM payment_completion_candidate_decisions WHERE grant_id=gid;
 SELECT * INTO r FROM payment_completion_candidate_receipts WHERE grant_id=gid;
 IF g.grant_id IS NULL OR d.decision_id IS NULL OR r.grant_id IS NULL THEN RAISE EXCEPTION 'COMPLETION_RECONCILE_REQUIRED'; END IF;
 wanted:=CASE d.outcome WHEN 'SETTLE_LOCAL_SIMULATION' THEN 'SUCCEEDED' WHEN 'DEFINITE_SIMULATED_DECLINE' THEN 'FAILED' END;
 SELECT * INTO o FROM operations WHERE operation_id=g.execution_operation_id;
 SELECT * INTO b FROM transfer_business_orders_v1 WHERE owner_id=g.owner_id AND operation_id=g.business_operation_ref;
 SELECT * INTO f FROM transfer_fund_orders_v1 WHERE owner_id=g.owner_id AND operation_id=g.business_operation_ref;
 SELECT count(*),count(*) FILTER(WHERE amount_cents=f.amount_cents AND completed_at=b.latest_receipt->>'completed_at'
 AND ((direction='DEBIT' AND account_id=f.payer_account_id) OR(direction='CREDIT' AND account_id=f.recipient_account_id))) INTO n,matching FROM transfer_ledger_v1 WHERE business_owner_id=g.owner_id AND operation_id=g.business_operation_ref;
 IF wanted IS NULL OR o.state IS DISTINCT FROM wanted OR o.revision IS DISTINCT FROM 4::bigint OR o.task_id IS DISTINCT FROM g.task_id
 OR o.business_operation_ref IS DISTINCT FROM g.business_operation_ref OR b.original_key IS DISTINCT FROM g.original_key
 OR b.request IS DISTINCT FROM g.evidence->'snapshot'->'transfer' OR b.first_receipt IS DISTINCT FROM g.evidence->'history'->'business'->'first_receipt'
 OR b.latest_receipt->>'status' IS DISTINCT FROM wanted OR (b.latest_receipt-'status'-'reason_code'-'completed_at') IS DISTINCT FROM (b.first_receipt-'status'-'reason_code'-'completed_at')
 OR f.state IS DISTINCT FROM wanted OR f.payer_account_id IS DISTINCT FROM g.evidence->>'payer_account_id' OR f.recipient_account_id IS DISTINCT FROM g.evidence->>'original_recipient_account_id'
 OR f.amount_cents IS DISTINCT FROM (g.evidence->>'amount_cents')::bigint
 OR (wanted='SUCCEEDED' AND (n<>2 OR matching<>2 OR b.latest_receipt->>'completed_at' IS NULL OR b.latest_receipt->>'reason_code' IS NOT NULL))
 OR (wanted='FAILED' AND (n<>0 OR b.latest_receipt->>'reason_code' IS DISTINCT FROM 'MOCK_BANK_DECLINED' OR b.latest_receipt->>'completed_at' IS NOT NULL))
 THEN RAISE EXCEPTION 'COMPLETION_RECONCILE_REQUIRED'; END IF;
 IF EXISTS(SELECT 1 FROM transfer_accounts_v1 aa WHERE aa.account_id IN(f.payer_account_id,f.recipient_account_id) AND
 (aa.booked_cents::numeric<>aa.opening_cents::numeric+coalesce((SELECT sum(CASE WHEN direction='DEBIT' THEN -amount_cents::numeric ELSE amount_cents::numeric END) FROM transfer_ledger_v1 WHERE account_id=aa.account_id),0)
 OR coalesce((SELECT sum(amount_cents::numeric) FROM transfer_fund_orders_v1 WHERE payer_account_id=aa.account_id AND state='PROCESSING'),0)>aa.booked_cents))
 OR (SELECT count(*) FROM transfer_accounts_v1 WHERE account_id IN(f.payer_account_id,f.recipient_account_id))<>2 THEN RAISE EXCEPTION 'COMPLETION_ACCOUNTING_CONFLICT'; END IF;
 body:=r.evidence;
 SELECT * INTO ctx FROM payment_completion_transactions_v1 WHERE grant_id=gid;
 IF NOT FOUND OR ctx.transaction_id IS DISTINCT FROM (body->>'transaction_id')::bigint
 OR ctx.original->>'evidence_digest' IS DISTINCT FROM g.evidence_digest OR ctx.original->'evidence' IS DISTINCT FROM g.evidence
 OR ctx.decision IS DISTINCT FROM to_jsonb(d) OR r.completed_at<g.first_submitted_at OR r.completed_at>=g.completion_until
 THEN RAISE EXCEPTION 'COMPLETION_RECEIPT_CONTEXT'; END IF;
 SELECT * INTO a FROM audit_events WHERE event_id=body->>'event_id';
 SELECT * INTO outbox FROM public_audit_outbox WHERE event_id=body->>'outbox_event_id';
 IF r.execution_operation_id IS DISTINCT FROM g.execution_operation_id OR r.grant_epoch IS DISTINCT FROM g.grant_epoch OR r.decision_id IS DISTINCT FROM d.decision_id OR r.public_revision<>4
 OR r.evidence_digest IS DISTINCT FROM encode(sha256(convert_to(body::text,'UTF8')),'hex')
 OR body->>'schema' IS DISTINCT FROM 'payment-completion-final-v1' OR body->>'mode' IS DISTINCT FROM 'PROJECT_SIMULATION'
 OR body->>'grant_digest' IS DISTINCT FROM g.evidence_digest OR body->>'result_id' IS DISTINCT FROM d.result_id OR body->>'decision_digest' IS DISTINCT FROM d.envelope_digest
 OR body->'decision' IS DISTINCT FROM to_jsonb(d) OR body->'public_operation' IS DISTINCT FROM (to_jsonb(o)-'created_at'-'updated_at')
 OR body->'business' IS DISTINCT FROM jsonb_build_object('request',b.request,'first_receipt',b.first_receipt,'latest_receipt',b.latest_receipt)
 OR body->'fund' IS DISTINCT FROM to_jsonb(f)
 OR body->'ledger' IS DISTINCT FROM (SELECT coalesce(jsonb_agg(to_jsonb(l) ORDER BY transaction_id),'[]'::jsonb) FROM transfer_ledger_v1 l WHERE business_owner_id=g.owner_id AND operation_id=g.business_operation_ref)
 OR (body->>'completed_at')::timestamptz IS DISTINCT FROM r.completed_at
 OR a.event_id IS NULL OR a.kind IS DISTINCT FROM 'OPERATION_TRANSITIONED' OR a.task_id IS DISTINCT FROM g.task_id OR a.operation_id IS DISTINCT FROM g.execution_operation_id OR a.operation_revision<>4
 OR a.details IS DISTINCT FROM jsonb_build_object('source','transfer-funds-v1','event','BACKGROUND_FINAL_RESULT','grant_id',gid,'result_id',d.result_id,'decision_digest',d.envelope_digest,'from','PROCESSING','to',wanted,'business_operation_ref',g.business_operation_ref,'first_receipt',b.first_receipt,'latest_receipt',b.latest_receipt)
 OR body->>'outbox_event_id' IS DISTINCT FROM a.event_id OR outbox.event_id IS NULL OR outbox.sequence<>a.sequence
 OR outbox.envelope IS DISTINCT FROM jsonb_build_object('storage_version',1,'owner_id',g.owner_id,'public_event',to_jsonb(a))
 THEN RAISE EXCEPTION 'COMPLETION_RECEIPT_INCOMPLETE'; END IF;
 RETURN jsonb_build_object('state','COMPLETED','scope','PROJECT_SIMULATION_PAYMENT','receipt',to_jsonb(r),'business_resubmitted',false);
END $$;

CREATE FUNCTION payment_completion_final_guard_v1(gid TEXT,lease BIGINT) RETURNS VOID
 LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE c RECORD; original JSONB; current_holds JSONB; a JSONB; actual RECORD; delta BIGINT; wanted TEXT;
BEGIN
 SELECT * INTO c FROM payment_completion_transactions_v1 WHERE transaction_id=txid_current_if_assigned() AND backend_pid=pg_backend_pid() AND worker_role=session_user::text AND grant_id=gid AND lease_epoch=lease;
 IF NOT FOUND THEN RAISE EXCEPTION 'COMPLETION_CONTEXT_REQUIRED'; END IF;
 original:=payment_completion_authority_v1(gid,true);
 IF original IS DISTINCT FROM c.original OR c.worker_epoch<>(original->>'source_epoch')::bigint
 OR NOT EXISTS(SELECT 1 FROM payment_completion_candidate_controls WHERE grant_id=gid AND control_epoch=c.control_epoch)
 OR NOT EXISTS(SELECT 1 FROM payment_completion_candidate_jobs WHERE grant_id=gid AND lease_epoch=lease AND lease_owner=session_user::text AND payment_background_clock_v1()<lease_until)
 THEN RAISE EXCEPTION 'COMPLETION_FINAL_AUTHORITY_CHANGED'; END IF;
 wanted:=c.decision->>'outcome';
 IF wanted='SETTLE_LOCAL_SIMULATION' AND NOT payment_completion_outgoing_v1(gid) THEN RAISE EXCEPTION 'COMPLETION_CARD_BLOCKED'; END IF;
 SELECT coalesce(jsonb_agg(to_jsonb(f) ORDER BY owner_id,operation_id),'[]'::jsonb) INTO current_holds FROM transfer_fund_orders_v1 f
 WHERE payer_account_id IN(original->'evidence'->>'payer_account_id',original->'evidence'->>'original_recipient_account_id')
 AND (owner_id,operation_id)<>(original->>'owner_id',original->>'business_operation_ref') AND state='PROCESSING';
 IF current_holds IS DISTINCT FROM c.other_holds THEN RAISE EXCEPTION 'COMPLETION_OTHER_HOLDS_CHANGED'; END IF;
 FOR a IN SELECT v FROM jsonb_array_elements(c.accounts_before) v LOOP
  SELECT * INTO actual FROM transfer_accounts_v1 WHERE account_id=a->>'account_id';
  delta:=CASE WHEN wanted='SETTLE_LOCAL_SIMULATION' THEN (original->'evidence'->>'amount_cents')::bigint*CASE WHEN a->>'account_id'=original->'evidence'->>'payer_account_id' THEN -1 ELSE 1 END ELSE 0 END;
  IF NOT FOUND OR (to_jsonb(actual)-'booked_cents') IS DISTINCT FROM (a-'booked_cents') OR actual.booked_cents<>(a->>'booked_cents')::bigint+delta THEN RAISE EXCEPTION 'COMPLETION_BALANCE_DELTA'; END IF;
 END LOOP;
 PERFORM payment_completion_bundle_v1(gid);
END $$;

CREATE FUNCTION payment_completion_finish_v1(gid TEXT,lease BIGINT) RETURNS JSONB
 LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE c RECORD; g RECORD; d RECORD; o RECORD; b RECORD; f RECORD; body JSONB; stamp TIMESTAMPTZ; eid TEXT; wanted TEXT;
BEGIN
 SELECT * INTO c FROM payment_completion_transactions_v1 WHERE transaction_id=txid_current_if_assigned() AND backend_pid=pg_backend_pid() AND worker_role=session_user::text AND grant_id=gid AND lease_epoch=lease;
 IF NOT FOUND THEN RAISE EXCEPTION 'COMPLETION_CONTEXT_REQUIRED'; END IF;
 PERFORM payment_completion_authority_v1(gid,true);
 SELECT * INTO g FROM payment_completion_candidate_grants WHERE grant_id=gid;
 SELECT * INTO d FROM payment_completion_candidate_decisions WHERE grant_id=gid;
 wanted:=CASE d.outcome WHEN 'SETTLE_LOCAL_SIMULATION' THEN 'SUCCEEDED' WHEN 'DEFINITE_SIMULATED_DECLINE' THEN 'FAILED' END;
 UPDATE operations SET state=wanted,revision=4 WHERE operation_id=g.execution_operation_id AND state='PROCESSING' AND revision=3 RETURNING * INTO o;
 IF NOT FOUND THEN RAISE EXCEPTION 'COMPLETION_PUBLIC_CAS'; END IF;
 SELECT * INTO b FROM transfer_business_orders_v1 WHERE owner_id=g.owner_id AND operation_id=g.business_operation_ref;
 SELECT * INTO f FROM transfer_fund_orders_v1 WHERE owner_id=g.owner_id AND operation_id=g.business_operation_ref;
 eid:='event_background_final_'||g.execution_operation_id;
 INSERT INTO audit_events(event_id,task_id,operation_id,kind,task_revision,operation_revision,details)
 SELECT eid,g.task_id,g.execution_operation_id,'OPERATION_TRANSITIONED',t.revision,4,
 jsonb_build_object('source','transfer-funds-v1','event','BACKGROUND_FINAL_RESULT','grant_id',gid,'result_id',d.result_id,'decision_digest',d.envelope_digest,'from','PROCESSING','to',wanted,'business_operation_ref',g.business_operation_ref,'first_receipt',b.first_receipt,'latest_receipt',b.latest_receipt)
 FROM tasks t WHERE task_id=g.task_id;
 stamp:=payment_background_clock_v1();
 body:=jsonb_build_object('schema','payment-completion-final-v1','mode','PROJECT_SIMULATION','grant_digest',g.evidence_digest,
 'result_id',d.result_id,'decision_digest',d.envelope_digest,'decision',to_jsonb(d),'public_operation',to_jsonb(o)-'created_at'-'updated_at',
 'business',jsonb_build_object('request',b.request,'first_receipt',b.first_receipt,'latest_receipt',b.latest_receipt),'fund',to_jsonb(f),
 'ledger',(SELECT coalesce(jsonb_agg(to_jsonb(l) ORDER BY transaction_id),'[]'::jsonb) FROM transfer_ledger_v1 l WHERE business_owner_id=g.owner_id AND operation_id=g.business_operation_ref),
 'event_id',eid,'outbox_event_id',eid,'completed_at',stamp,'transaction_id',txid_current());
 INSERT INTO payment_completion_candidate_receipts(execution_operation_id,grant_id,grant_epoch,decision_id,public_revision,evidence,evidence_digest,completed_at)
 VALUES(g.execution_operation_id,gid,g.grant_epoch,d.decision_id,4,body,encode(sha256(convert_to(body::text,'UTF8')),'hex'),stamp);
 PERFORM payment_completion_final_guard_v1(gid,lease);
 RETURN payment_completion_bundle_v1(gid);
END $$;

CREATE FUNCTION payment_completion_commit_v1() RETURNS trigger
 LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
BEGIN
 PERFORM payment_completion_final_guard_v1(NEW.grant_id,NEW.lease_epoch);
 RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER completion_commit_v1 AFTER INSERT ON payment_completion_transactions_v1
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION payment_completion_commit_v1();

CREATE FUNCTION payment_completion_recover_v1(gid TEXT) RETURNS JSONB
 LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE g RECORD;
BEGIN
 PERFORM payment_completion_authority_v1(gid,false);
 SELECT * INTO g FROM payment_completion_candidate_grants WHERE grant_id=gid;
 IF EXISTS(SELECT 1 FROM payment_completion_candidate_receipts WHERE grant_id=gid) THEN RETURN payment_completion_bundle_v1(gid); END IF;
 IF NOT EXISTS(SELECT 1 FROM operations WHERE operation_id=g.execution_operation_id AND state='PROCESSING' AND revision=3)
 OR NOT EXISTS(SELECT 1 FROM transfer_fund_orders_v1 WHERE owner_id=g.owner_id AND operation_id=g.business_operation_ref AND state='PROCESSING')
 OR NOT EXISTS(SELECT 1 FROM transfer_business_orders_v1 WHERE owner_id=g.owner_id AND operation_id=g.business_operation_ref AND latest_receipt->>'status'='PROCESSING')
 OR EXISTS(SELECT 1 FROM transfer_ledger_v1 WHERE business_owner_id=g.owner_id AND operation_id=g.business_operation_ref)
 THEN RAISE EXCEPTION 'COMPLETION_RECONCILE_REQUIRED'; END IF;
 RETURN NULL;
END $$;

-- Shared accepted-history validation also protects direct SQL callers of the
-- closed definer entry and terminal recovery; no old session is reactivated.
CREATE FUNCTION payment_completion_accepted_v1(gid TEXT) RETURNS VOID
 LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE g RECORD; r RECORD; p RECORD; q RECORD; a RECORD; c RECORD; t RECORD; h JSONB; e JSONB; bind JSONB;
BEGIN
 SELECT * INTO g FROM payment_completion_candidate_grants WHERE grant_id=gid;
 e:=g.evidence; h:=e->'history'; bind:=h->'request_binding'->'snapshot';
 SELECT * INTO r FROM public_execution_requests WHERE operation_id=g.execution_operation_id;
 SELECT * INTO a FROM payment_funds_execution_v1 WHERE execution_operation_id=g.execution_operation_id;
 SELECT * INTO p FROM execution_permits WHERE permit_id=h->>'permit_id';
 SELECT * INTO q FROM server_confirmations WHERE confirmation_ref=h->'confirmation'->>'confirmation_ref';
 SELECT * INTO c FROM payment_background_consents_v1 WHERE consent_id=(e->>'consent_id')::uuid;
 SELECT * INTO t FROM tasks WHERE task_id=g.task_id;
 IF r.operation_id IS NULL OR a.execution_operation_id IS NULL OR p.permit_id IS NULL OR q.confirmation_ref IS NULL OR c.consent_id IS NULL OR t.task_id IS NULL
 OR h->>'schema' IS DISTINCT FROM 'payment-accepted-history-candidate-v1' OR h->>'mode' IS DISTINCT FROM 'PROJECT_SIMULATION'
 OR h->>'preparation_operation_id' IS DISTINCT FROM g.preparation_operation_id OR h->>'execution_operation_id' IS DISTINCT FROM g.execution_operation_id
 OR h->>'business_operation_ref' IS DISTINCT FROM g.business_operation_ref OR h->>'original_key' IS DISTINCT FROM g.original_key
 OR h->'snapshot' IS DISTINCT FROM e->'snapshot' OR h->'request_payload' IS DISTINCT FROM e->'request_payload' OR h->>'request_digest' IS DISTINCT FROM e->>'request_digest'
 OR r.owner_id IS DISTINCT FROM g.owner_id OR r.task_id IS DISTINCT FROM g.task_id OR r.business_operation_ref IS DISTINCT FROM g.business_operation_ref OR r.idempotency_key IS DISTINCT FROM g.original_key
 OR r.submitted_at IS DISTINCT FROM g.first_submitted_at OR r.first_operation IS DISTINCT FROM h->'first_submission'->'operation'
 OR (h->'first_submission'->>'recorded_at')::timestamptz IS DISTINCT FROM g.first_submitted_at
 OR r.first_operation->>'state' IS DISTINCT FROM 'SUBMITTED' OR r.first_operation->>'revision' IS DISTINCT FROM '2'
 OR r.request_digest IS DISTINCT FROM e->>'request_digest' OR r.fingerprint_payload IS DISTINCT FROM e->'request_payload'
 OR r.operation_fingerprint IS DISTINCT FROM h->'request_binding'->>'fingerprint'
 OR a.preparation_operation_id IS DISTINCT FROM g.preparation_operation_id OR a.task_id IS DISTINCT FROM g.task_id OR a.execution_binding IS DISTINCT FROM bind
 OR a.association_digest IS DISTINCT FROM h->>'association_digest' OR a.snapshot_digest IS DISTINCT FROM h->>'snapshot_digest'
 OR t.owner_id IS DISTINCT FROM g.owner_id OR t.plan IS DISTINCT FROM h->'task'->'plan' OR t.revision IS DISTINCT FROM (h->'task'->>'revision')::bigint
 OR t.plan_version IS DISTINCT FROM (bind->>'plan_version')::bigint OR t.facts_version IS DISTINCT FROM (bind->>'facts_version')::bigint
 THEN RAISE EXCEPTION 'COMPLETION_ACCEPTED_BINDING_CONFLICT'; END IF;
 IF p.status IS DISTINCT FROM 'CONSUMED' OR p.storage_version<>4 OR p.authentication_state IS DISTINCT FROM 'ISOLATED_PAYMENT_VERIFIED' OR p.execution_scope IS DISTINCT FROM 'TRANSFER'
 OR p.operation_id IS DISTINCT FROM r.operation_id OR p.owner_id IS DISTINCT FROM r.owner_id OR p.task_id IS DISTINCT FROM r.task_id
 OR p.node_id IS DISTINCT FROM r.node_id OR p.tool_name IS DISTINCT FROM r.tool_name OR p.arguments_hash IS DISTINCT FROM r.arguments_hash
 OR p.plan_version IS DISTINCT FROM r.plan_version OR p.facts_version IS DISTINCT FROM r.facts_version OR p.task_revision IS DISTINCT FROM r.task_revision OR p.operation_revision IS DISTINCT FROM r.operation_revision
 OR p.fingerprint_payload IS DISTINCT FROM r.fingerprint_payload OR p.operation_fingerprint IS DISTINCT FROM r.operation_fingerprint
 OR p.auth_grant_ref IS DISTINCT FROM c.auth_ref OR p.confirmation_ref IS DISTINCT FROM q.confirmation_ref
 OR p.session_digest IS DISTINCT FROM h->>'session_digest' OR p.session_binding_version IS DISTINCT FROM (h->>'session_version')::bigint
 OR p.issued_at_ms IS DISTINCT FROM (h->>'permit_issued_ms')::bigint OR p.expires_at_ms IS DISTINCT FROM (h->>'permit_expires_ms')::bigint
 OR p.consumed_at_ms IS DISTINCT FROM (extract(epoch FROM (h->>'permit_consumed_at')::timestamptz)*1000)::bigint
 OR p.consumed_at_ms<p.issued_at_ms OR p.consumed_at_ms>=p.expires_at_ms OR (extract(epoch FROM g.first_submitted_at)*1000)<p.consumed_at_ms
 OR q.owner_id IS DISTINCT FROM g.owner_id OR q.task_id IS DISTINCT FROM g.task_id OR q.operation_id IS DISTINCT FROM g.preparation_operation_id
 OR q.confirmed_at IS NULL OR q.confirmed_at>g.first_submitted_at OR q.expires_at<=g.first_submitted_at OR (q.revoked_at IS NOT NULL AND q.revoked_at<=g.first_submitted_at)
 OR q.session_digest IS DISTINCT FROM p.session_digest OR q.session_binding_version IS DISTINCT FROM p.session_binding_version
 OR c.owner_id IS DISTINCT FROM g.owner_id OR c.task_id IS DISTINCT FROM g.task_id OR c.preparation_operation_id IS DISTINCT FROM g.preparation_operation_id
 OR c.account_id IS DISTINCT FROM e->>'payer_account_id' OR c.account_epoch IS DISTINCT FROM (e->>'account_epoch')::bigint
 OR c.display IS DISTINCT FROM e->'delegation_display' OR c.display->'snapshot' IS DISTINCT FROM e->'snapshot'
 OR c.session_digest IS DISTINCT FROM p.session_digest OR c.binding_version IS DISTINCT FROM p.session_binding_version
 OR c.confirmed_at IS NULL OR c.confirmed_at>g.first_submitted_at OR c.confirmed_until<=g.first_submitted_at OR c.auth_until<=g.first_submitted_at
 THEN RAISE EXCEPTION 'COMPLETION_ACCEPTED_AUTH_CONFLICT'; END IF;
 IF NOT EXISTS(SELECT 1 FROM payment_preparations pp WHERE pp.operation_id=g.preparation_operation_id AND pp.task_id=g.task_id AND pp.snapshot=e->'snapshot' AND pp.snapshot_digest=h->>'snapshot_digest' AND pp.confirmation_ref=q.confirmation_ref)
 OR NOT EXISTS(SELECT 1 FROM audit_events ev WHERE ev.task_id=g.task_id AND ev.kind='PERMIT_CONSUMED' AND ev.details->>'permit_id'=p.permit_id AND ev.details->>'operation_fingerprint'=p.operation_fingerprint)
 OR NOT EXISTS(SELECT 1 FROM audit_events ev WHERE ev.task_id=g.task_id AND ev.operation_id=g.execution_operation_id AND ev.kind='OPERATION_TRANSITIONED' AND ev.details->>'event'='SUBMISSION_RECORDED' AND ev.details->>'original_reference'=g.business_operation_ref)
 OR NOT EXISTS(SELECT 1 FROM audit_events ev WHERE ev.task_id=g.task_id AND ev.operation_id=g.preparation_operation_id AND ev.kind='EXPLICIT_CONFIRMATION_RECORDED' AND ev.details->>'confirmation_ref'=q.confirmation_ref AND ev.details->>'binding_digest'=q.binding_digest AND ev.details->>'status'='CONFIRMED')
 THEN RAISE EXCEPTION 'COMPLETION_ACCEPTED_TRAIL_INCOMPLETE'; END IF;
END $$;

-- RLS predicate reveals only a bool to callers that already have table rights.
-- All other functions remain private until the explicit writer-on grant.
-- Existing shared deferred assertions inspect their OWN domain's tables even
-- for unrelated operations. Run those read-only trigger checks as the trusted
-- schema owner, instead of granting workers private billing/subscription/card
-- tables. Trigger functions have no callable ordinary SQL entry/result.
DO $$ DECLARE name TEXT; BEGIN
 FOREACH name IN ARRAY ARRAY['billing_prepare_integrity_v2','billing_audit_check_v1','billing_audit_association_v1','subscription_a2_association_v1','public_card_final_constraint_v1'] LOOP
  EXECUTE format('ALTER FUNCTION %I.%I() SECURITY DEFINER','__SCHEMA__',name);
  EXECUTE format('ALTER FUNCTION %I.%I() SET search_path TO pg_catalog,%I,pg_temp','__SCHEMA__',name,'__SCHEMA__');
 END LOOP;
END $$;
-- These inherited invoker validators compare current_schema() with their
-- recorded namespace. Pin their own path to the trusted product schema before
-- catalog, so a catalog-first definer caller cannot change that namespace.
-- This is NOT an EXECUTE/SELECT grant or a new SECURITY DEFINER entry.
ALTER FUNCTION verify_billing_audit_v1(TEXT) SET search_path TO __SCHEMA__,pg_catalog,pg_temp;
ALTER FUNCTION verify_subscription_a2_v1(TEXT) SET search_path TO __SCHEMA__,pg_catalog,pg_temp;
DO $$ DECLARE f RECORD; BEGIN
 FOR f IN SELECT p.proname,pg_get_function_identity_arguments(p.oid) args FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
 WHERE n.nspname='__SCHEMA__' AND p.proname LIKE 'payment_completion_%_v1' LOOP
  EXECUTE format('ALTER FUNCTION %I.%I(%s) SET search_path TO pg_catalog,%I,pg_temp','__SCHEMA__',f.proname,f.args,'__SCHEMA__');
  EXECUTE format('ALTER FUNCTION %I.%I(%s) SET TimeZone TO ''UTC''','__SCHEMA__',f.proname,f.args);
  EXECUTE format('REVOKE ALL ON FUNCTION %I.%I(%s) FROM PUBLIC','__SCHEMA__',f.proname,f.args);
 END LOOP;
END $$;
GRANT EXECUTE ON FUNCTION payment_completion_rows_v1(TEXT,JSONB) TO PUBLIC;
