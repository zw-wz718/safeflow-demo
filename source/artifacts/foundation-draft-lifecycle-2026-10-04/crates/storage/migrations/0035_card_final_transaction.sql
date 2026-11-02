-- Additive local-fiction self-freeze assembly. Existing member business unchanged.
DO $$ DECLARE old_values TEXT; BEGIN
 SELECT pg_get_constraintdef(oid) INTO old_values FROM pg_constraint
 WHERE conrelid='audit_events'::regclass AND conname='audit_events_kind_check';
 IF old_values IS NULL THEN RAISE EXCEPTION 'CARD_AUDIT_BASELINE_MISSING'; END IF;
 ALTER TABLE audit_events DROP CONSTRAINT audit_events_kind_check;
 EXECUTE 'ALTER TABLE audit_events ADD CONSTRAINT audit_events_kind_check CHECK (('||
 substring(old_values FROM 8 FOR length(old_values)-8)||') OR kind=''CARD_FREEZE_OPERATION_RECORDED'')';
END $$;
ALTER TABLE audit_events ADD CONSTRAINT card_domain_details_v1 CHECK (
 kind<>'CARD_FREEZE_OPERATION_RECORDED' OR (
 operation_id IS NOT NULL AND operation_revision IS NOT NULL AND
 details IN (
 '{"schema_version":1,"mapping":"card-freeze-operation-v1","action":"FREEZE_CARD","result":"OPERATION_RECORDED","original_effect":"CREATED"}'::jsonb,
 '{"schema_version":1,"mapping":"card-freeze-operation-v1","action":"FREEZE_CARD","result":"OPERATION_RECORDED","original_effect":"REUSED_ACTIVE"}'::jsonb)
 AND details->>'schema_version'='1') IS TRUE);

CREATE TABLE public_card_final_deployment_v1 (
 singleton SMALLINT PRIMARY KEY CHECK(singleton=1), deployment_id UUID NOT NULL UNIQUE,
 source_id TEXT NOT NULL, implementation TEXT NOT NULL,
 schema_oid OID NOT NULL, database_oid OID NOT NULL, reader_version TEXT NOT NULL CHECK(reader_version='card-final-v1'),
 active BOOLEAN NOT NULL, epoch BIGINT NOT NULL CHECK(epoch>0), object_digest TEXT NOT NULL, activated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TABLE public_card_final_logins_v1(role_name TEXT PRIMARY KEY,reader_version TEXT NOT NULL CHECK(reader_version='card-final-v1'));
CREATE TABLE public_card_legacy_members_v1 (
 record_id TEXT PRIMARY KEY REFERENCES card_freeze_operations_v1(record_id), bundle JSONB NOT NULL,
 classification TEXT NOT NULL CHECK(classification='LEGACY_UNLINKED')
);
CREATE TABLE public_card_legacy_operations_v1 (
 operation_id TEXT PRIMARY KEY REFERENCES public_card_safety_requests_v1(operation_id), original JSONB NOT NULL
);
-- Legacy/Common fact-version tombstones remain exactly one. Card retry slots
-- require a prior server-confirmed rollback and a newly issued permit ID.
ALTER TABLE execution_permits ADD COLUMN card_attempt_no BIGINT NOT NULL DEFAULT 0 CHECK(card_attempt_no>=0);
ALTER TABLE execution_permits ADD CONSTRAINT card_permit_attempt_scope_v1 CHECK((execution_scope='ISOLATED_CARD' OR card_attempt_no=0) IS TRUE);
ALTER TABLE execution_permits DROP CONSTRAINT execution_permits_operation_id_facts_version_key;
ALTER TABLE execution_permits ADD UNIQUE(operation_id,facts_version,card_attempt_no);
CREATE UNIQUE INDEX card_one_active_permit_v1 ON execution_permits(operation_id,facts_version) WHERE execution_scope='ISOLATED_CARD' AND status='ACTIVE';
CREATE TABLE public_card_attempts_v1 (
 operation_id TEXT NOT NULL REFERENCES public_card_safety_requests_v1(operation_id), attempt_no BIGINT NOT NULL CHECK(attempt_no>0),
 token UUID NOT NULL UNIQUE, state TEXT NOT NULL CHECK(state IN ('ADMITTED','ROLLBACK_VERIFIED','BUSINESS_COMMITTED')),
 admitted_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(), business_txid BIGINT,
 permit_id TEXT NOT NULL REFERENCES execution_permits(permit_id), rollback_txid BIGINT, rollback_confirmed_at TIMESTAMPTZ,
 PRIMARY KEY(operation_id,attempt_no)
);
CREATE TABLE public_card_results_v1 (
 operation_id TEXT PRIMARY KEY REFERENCES public_card_safety_requests_v1(operation_id),
 member_operation_id TEXT NOT NULL UNIQUE, canonical_key TEXT NOT NULL UNIQUE,
 record_id TEXT NOT NULL UNIQUE REFERENCES card_freeze_operations_v1(record_id),
 lock_id TEXT NOT NULL, reason_history_id TEXT NOT NULL, original_effect TEXT NOT NULL CHECK(original_effect IN ('CREATED','REUSED_ACTIVE')),
 attempt_no BIGINT NOT NULL, business_txid BIGINT NOT NULL, post_facts JSONB NOT NULL,
 first_submission JSONB NOT NULL, result_digest TEXT NOT NULL,
 FOREIGN KEY(operation_id,attempt_no) REFERENCES public_card_attempts_v1(operation_id,attempt_no)
);
CREATE TABLE public_card_audit_links_v1 (
 operation_id TEXT PRIMARY KEY REFERENCES public_card_results_v1(operation_id),
 event_id TEXT NOT NULL UNIQUE REFERENCES audit_events(event_id), record_id TEXT NOT NULL UNIQUE REFERENCES card_freeze_operations_v1(record_id),
 request_digest TEXT NOT NULL, result_digest TEXT NOT NULL
);
-- Persistent one-to-one public/member identities exist before any business attempt.
CREATE UNIQUE INDEX public_card_original_member_v1 ON public_card_safety_requests_v1 ((input->>'member_operation_id'));
CREATE UNIQUE INDEX public_card_original_key_v1 ON public_card_safety_requests_v1 ((input->>'canonical_key'));
CREATE UNIQUE INDEX public_card_original_record_v1 ON public_card_safety_requests_v1 ((input->>'operation_record_id'));

CREATE FUNCTION public_card_final_immutable_v1() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_TABLE_NAME='public_card_final_deployment_v1' THEN
  IF TG_OP='UPDATE' AND OLD.active AND NOT NEW.active AND NEW.epoch=OLD.epoch+1
  AND (to_jsonb(NEW)-'active'-'epoch')=(to_jsonb(OLD)-'active'-'epoch') THEN RETURN NEW; END IF;
 ELSIF TG_TABLE_NAME='public_card_attempts_v1' THEN
 IF TG_OP='UPDATE' AND OLD.state='ADMITTED'
 AND NEW.state IN ('ROLLBACK_VERIFIED','BUSINESS_COMMITTED')
 AND (to_jsonb(NEW)-'state'-'business_txid'-'rollback_txid'-'rollback_confirmed_at')=(to_jsonb(OLD)-'state'-'business_txid'-'rollback_txid'-'rollback_confirmed_at')
 AND ((NEW.state='BUSINESS_COMMITTED' AND NEW.business_txid IS NOT NULL AND NEW.rollback_txid IS NULL AND NEW.rollback_confirmed_at IS NULL) OR (NEW.state='ROLLBACK_VERIFIED' AND NEW.business_txid IS NULL AND NEW.rollback_txid IS NOT NULL AND NEW.rollback_confirmed_at IS NOT NULL)) THEN RETURN NEW; END IF;
 END IF;
 RAISE EXCEPTION 'CARD_FINAL_HISTORY_IMMUTABLE';
END $$;
CREATE FUNCTION public_card_final_assert_v1(op TEXT) RETURNS VOID LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
DECLARE q RECORD; m RECORD; r RECORD; h RECORD; l RECORD; a RECORD; b RECORD; t RECORD; o RECORD; p RECORD; c RECORD; attempt RECORD; request RECORD;
BEGIN
 SELECT * INTO q FROM public_card_safety_requests_v1 WHERE operation_id=op;
 IF NOT FOUND THEN RAISE EXCEPTION 'CARD_FINAL_REQUEST_MISSING'; END IF;
 SELECT * INTO r FROM public_card_results_v1 WHERE operation_id=op;
 IF NOT FOUND THEN RAISE EXCEPTION 'CARD_FINAL_RESULT_MISSING'; END IF;
 SELECT * INTO m FROM card_freeze_operations_v1 WHERE record_id=r.record_id;
 IF NOT FOUND OR m.owner_id IS DISTINCT FROM q.owner_id OR m.account_id IS DISTINCT FROM q.account_id OR m.card_id IS DISTINCT FROM q.card_id
 OR m.operation_id IS DISTINCT FROM q.input->>'member_operation_id' OR m.original_key IS DISTINCT FROM q.input->>'canonical_key'
 OR m.record_id IS DISTINCT FROM q.input->>'operation_record_id' OR m.source IS DISTINCT FROM 'USER_SELF' OR m.reason IS DISTINCT FROM 'USER_SECURITY_CONCERN'
 OR m.event_scope IS NOT NULL OR m.event_id IS NOT NULL OR m.transaction_id IS NOT NULL OR m.feedback_id IS NOT NULL THEN RAISE EXCEPTION 'CARD_FINAL_MEMBER_BINDING'; END IF;
 SELECT * INTO h FROM card_freeze_reasons_v1 WHERE operation_record_id=m.record_id;
 IF NOT FOUND OR h.reason_history_id IS DISTINCT FROM q.input->>'reason_history_id' OR h.lock_id IS DISTINCT FROM m.lock_id OR h.reason IS DISTINCT FROM m.reason THEN RAISE EXCEPTION 'CARD_FINAL_REASON_BINDING'; END IF;
 SELECT * INTO p FROM card_freeze_receipts_v1 WHERE operation_record_id=m.record_id;
 IF NOT FOUND OR p.lock_id IS DISTINCT FROM m.lock_id OR p.reason_history_id IS DISTINCT FROM h.reason_history_id OR p.original_effect IS DISTINCT FROM r.original_effect THEN RAISE EXCEPTION 'CARD_FINAL_RECEIPT_BINDING'; END IF;
 IF r.member_operation_id IS DISTINCT FROM m.operation_id OR r.canonical_key IS DISTINCT FROM m.original_key OR r.lock_id IS DISTINCT FROM m.lock_id OR r.reason_history_id IS DISTINCT FROM h.reason_history_id
 OR r.original_effect='CREATED' AND m.lock_id IS DISTINCT FROM q.input->>'lock_id' THEN RAISE EXCEPTION 'CARD_FINAL_RESULT_BINDING'; END IF;
 SELECT * INTO b FROM card_locks_v1 WHERE lock_id=m.lock_id;
 IF NOT FOUND OR b.card_id IS DISTINCT FROM m.card_id OR b.source IS DISTINCT FROM m.source OR b.event_scope IS DISTINCT FROM m.event_scope OR b.event_id IS DISTINCT FROM m.event_id THEN RAISE EXCEPTION 'CARD_FINAL_LOCK_BINDING'; END IF;
 SELECT * INTO l FROM public_card_audit_links_v1 WHERE operation_id=op;
 IF NOT FOUND OR l.record_id IS DISTINCT FROM m.record_id OR l.request_digest IS DISTINCT FROM q.input_digest OR l.result_digest IS DISTINCT FROM r.result_digest THEN RAISE EXCEPTION 'CARD_FINAL_LINK_BINDING'; END IF;
 IF r.result_digest IS DISTINCT FROM encode(sha256(convert_to((to_jsonb(r)-'result_digest')::text,'UTF8')),'hex') THEN RAISE EXCEPTION 'CARD_FINAL_RESULT_DIGEST'; END IF;
 SELECT * INTO a FROM audit_events WHERE event_id=l.event_id;
 IF NOT FOUND OR a.task_id IS DISTINCT FROM q.task_id OR a.operation_id IS DISTINCT FROM op OR a.task_revision<>3 OR a.operation_revision<>3 OR a.kind IS DISTINCT FROM 'CARD_FREEZE_OPERATION_RECORDED'
 OR a.details IS DISTINCT FROM jsonb_build_object('schema_version',1,'mapping','card-freeze-operation-v1','action','FREEZE_CARD','result','OPERATION_RECORDED','original_effect',r.original_effect) THEN RAISE EXCEPTION 'CARD_FINAL_EVENT_BINDING'; END IF;
 SELECT * INTO b FROM public_audit_outbox WHERE event_id=l.event_id;
 IF NOT FOUND OR b.sequence IS DISTINCT FROM a.sequence OR b.envelope IS DISTINCT FROM jsonb_build_object('storage_version',1,'owner_id',q.owner_id,'public_event',to_jsonb(a)) THEN RAISE EXCEPTION 'CARD_FINAL_OUTBOX_BINDING'; END IF;
 SELECT * INTO t FROM tasks WHERE task_id=q.task_id;
 SELECT * INTO o FROM operations WHERE operation_id=op;
 IF t.owner_id IS DISTINCT FROM q.owner_id OR t.state IS DISTINCT FROM 'COMPLETED' OR t.revision<>3 OR t.plan_version<>1 OR t.facts_version<>1 OR t.checkpoint IS DISTINCT FROM '{"kind":"FINISHED"}'::jsonb
 OR t.completed_nodes IS DISTINCT FROM jsonb_build_array(o.node_id) OR o.task_id IS DISTINCT FROM t.task_id OR o.state IS DISTINCT FROM 'SUCCEEDED' OR o.revision<>3 OR o.business_operation_ref IS DISTINCT FROM m.operation_id OR o.arguments_hash IS DISTINCT FROM q.input_digest
 OR o.plan_version<>1 OR o.facts_version<>1 OR t.plan->'nodes' IS NULL THEN RAISE EXCEPTION 'CARD_FINAL_PUBLIC_COMPLETION'; END IF;
 IF t.plan IS DISTINCT FROM jsonb_build_object('contract_version',1,'task_id',q.task_id,'plan_version',1,'nodes',jsonb_build_array(jsonb_build_object('type','TOOL','id',o.node_id,'tool','card_freeze_isolated','arguments',q.input,'depends_on','[]'::jsonb)))
 OR r.first_submission IS DISTINCT FROM ((to_jsonb(o)-'created_at'-'updated_at')||'{"state":"SUBMITTED","revision":2}'::jsonb)
 OR (r.post_facts->>'account_id' IS NOT DISTINCT FROM q.account_id AND r.post_facts->'unlock_route' IS NOT DISTINCT FROM q.input->'facts'->'unlock_route'
     AND (r.post_facts->>'revision') ~ '^card-pg-v1:[0-9]+$' AND substring(r.post_facts->>'revision' FROM 12)::bigint>substring(q.input->'facts'->>'revision' FROM 12)::bigint
     AND r.post_facts->'active_card_lock_ids' @> jsonb_build_array(m.lock_id)
     AND EXISTS(SELECT 1 FROM jsonb_array_elements(r.post_facts->'cards') v WHERE v->>'card_id'=q.card_id AND v->>'account_id'=q.account_id AND EXISTS(SELECT 1 FROM jsonb_array_elements(v->'active_locks') k WHERE k->>'id'=m.lock_id))) IS NOT TRUE
 THEN RAISE EXCEPTION 'CARD_FINAL_PLAN_OR_FACTS'; END IF;
 SELECT * INTO p FROM public_execution_requests WHERE operation_id=op;
 IF NOT FOUND OR p.owner_id IS DISTINCT FROM q.owner_id OR p.task_id IS DISTINCT FROM q.task_id OR p.idempotency_key IS DISTINCT FROM m.original_key OR p.business_operation_ref IS DISTINCT FROM m.operation_id
 OR p.fingerprint_payload->'normalized_arguments' IS DISTINCT FROM q.input OR p.arguments_hash IS DISTINCT FROM q.input_digest OR p.first_operation IS DISTINCT FROM r.first_submission
 OR p.first_operation->>'state' IS DISTINCT FROM 'SUBMITTED' OR p.first_operation->>'revision' IS DISTINCT FROM '2' OR p.submitted_at IS NULL THEN RAISE EXCEPTION 'CARD_FINAL_SUBMISSION'; END IF;
 request:=p;
 SELECT * INTO c FROM server_confirmations WHERE confirmation_ref=q.confirmation_ref;
 IF NOT FOUND OR c.owner_id IS DISTINCT FROM q.owner_id OR c.task_id IS DISTINCT FROM q.task_id OR c.operation_id IS DISTINCT FROM op OR c.node_id IS DISTINCT FROM o.node_id OR c.tool_name IS DISTINCT FROM 'card_freeze_isolated'
 OR c.arguments_hash IS DISTINCT FROM q.input_digest OR c.session_digest IS DISTINCT FROM q.session_digest OR c.session_binding_version IS DISTINCT FROM q.binding_version
 OR c.plan_version<>1 OR c.facts_version<>1 OR c.task_revision<>1 OR c.operation_revision<>1 OR c.confirmed_at IS NULL THEN RAISE EXCEPTION 'CARD_FINAL_CONFIRMATION'; END IF;
 SELECT * INTO p FROM execution_permits WHERE permit_id=(SELECT permit_id FROM public_card_attempts_v1 WHERE operation_id=op AND attempt_no=r.attempt_no);
 IF NOT FOUND OR p.status IS DISTINCT FROM 'CONSUMED' OR p.execution_scope IS DISTINCT FROM 'ISOLATED_CARD' OR p.storage_version<>3 OR p.authentication_state IS DISTINCT FROM 'ISOLATED_CARD_VERIFIED'
 OR p.fingerprint_payload IS DISTINCT FROM jsonb_build_object('user_id',q.owner_id,'operation_id',op,'tool_name','card_freeze_isolated','resource_id',q.account_id,'normalized_arguments',q.input,'plan_version',1,'schema_version',1)
 OR p.fingerprint_payload IS DISTINCT FROM request.fingerprint_payload OR p.operation_fingerprint IS DISTINCT FROM request.operation_fingerprint
 OR p.owner_id IS DISTINCT FROM q.owner_id OR p.operation_id IS DISTINCT FROM op OR p.task_id IS DISTINCT FROM q.task_id OR p.node_id IS DISTINCT FROM o.node_id OR p.tool_name IS DISTINCT FROM c.tool_name
 OR p.arguments_hash IS DISTINCT FROM q.input_digest OR p.binding_digest IS DISTINCT FROM c.binding_digest OR p.confirmation_ref IS DISTINCT FROM c.confirmation_ref OR p.session_digest IS DISTINCT FROM c.session_digest OR p.session_binding_version IS DISTINCT FROM c.session_binding_version
 OR p.task_revision<>1 OR p.operation_revision<>1 OR p.plan_version<>1 OR p.facts_version<>1 OR p.card_attempt_no<>r.attempt_no-1
 OR request.node_id IS DISTINCT FROM p.node_id OR request.tool_name IS DISTINCT FROM p.tool_name OR request.task_revision<>1 OR request.operation_revision<>1 OR request.plan_version<>1 OR request.facts_version<>1 OR request.business_version<>1
 THEN RAISE EXCEPTION 'CARD_FINAL_CONSUMED_PERMIT'; END IF;
 SELECT * INTO attempt FROM public_card_attempts_v1 WHERE operation_id=op AND attempt_no=r.attempt_no;
 IF NOT FOUND OR attempt.state IS DISTINCT FROM 'BUSINESS_COMMITTED' OR attempt.business_txid IS DISTINCT FROM r.business_txid THEN RAISE EXCEPTION 'CARD_FINAL_ATTEMPT_BINDING'; END IF;
END $$;
CREATE FUNCTION public_card_final_constraint_v1() RETURNS trigger LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
DECLARE op TEXT; rec TEXT;
BEGIN
 IF NOT EXISTS(SELECT 1 FROM public_card_final_deployment_v1 WHERE singleton=1) THEN
  IF TG_TABLE_NAME IN ('public_card_results_v1','public_card_audit_links_v1','public_card_attempts_v1') THEN RAISE EXCEPTION 'CARD_FINAL_NOT_ACTIVATED'; END IF;
  IF TG_TABLE_NAME='audit_events' THEN
   IF NEW.kind='CARD_FREEZE_OPERATION_RECORDED' THEN RAISE EXCEPTION 'CARD_FINAL_NOT_ACTIVATED'; END IF;
  END IF;
  RETURN NULL;
 END IF;
 IF TG_TABLE_NAME='card_locks_v1' THEN
  SELECT operation_id INTO op FROM public_card_results_v1 WHERE lock_id=NEW.lock_id AND original_effect='CREATED';
  IF op IS NULL THEN RAISE EXCEPTION 'CARD_FINAL_UNLINKED_NEW_LOCK'; END IF;
 ELSIF TG_TABLE_NAME='card_freeze_operations_v1' THEN
  rec:=NEW.record_id;
 ELSIF TG_TABLE_NAME IN ('card_freeze_reasons_v1','card_freeze_receipts_v1') THEN rec:=NEW.operation_record_id;
 ELSIF TG_TABLE_NAME='audit_events' THEN
  IF NEW.kind<>'CARD_FREEZE_OPERATION_RECORDED' THEN RETURN NULL; END IF;
  op:=NEW.operation_id;
 ELSIF TG_TABLE_NAME='public_audit_outbox' THEN
  SELECT operation_id INTO op FROM audit_events WHERE event_id=NEW.event_id AND kind='CARD_FREEZE_OPERATION_RECORDED';
  IF op IS NULL THEN RETURN NULL; END IF;
 ELSIF TG_TABLE_NAME='tasks' THEN
  SELECT operation_id INTO op FROM public_card_results_v1 WHERE operation_id IN (SELECT operation_id FROM operations WHERE task_id=NEW.task_id);
  IF op IS NULL THEN RETURN NULL; END IF;
 ELSE op:=NEW.operation_id;
 END IF;
 IF rec IS NOT NULL THEN
  IF EXISTS(SELECT 1 FROM public_card_legacy_members_v1 WHERE record_id=rec) THEN RETURN NULL; END IF;
  SELECT operation_id INTO op FROM public_card_results_v1 WHERE record_id=rec;
  IF op IS NULL THEN RAISE EXCEPTION 'CARD_FINAL_UNLINKED_NEW_MEMBER'; END IF;
 END IF;
 IF EXISTS(SELECT 1 FROM public_card_legacy_operations_v1 WHERE operation_id=op) THEN RETURN NULL; END IF;
 IF NOT EXISTS(SELECT 1 FROM public_card_results_v1 WHERE operation_id=op) THEN
  IF TG_TABLE_NAME IN ('operations','public_execution_requests','public_card_attempts_v1') THEN RETURN NULL; END IF;
 END IF;
 PERFORM public_card_final_assert_v1(op); RETURN NULL;
END $$;
DO $$ DECLARE name TEXT; BEGIN
 FOREACH name IN ARRAY ARRAY['public_card_results_v1','public_card_audit_links_v1','public_card_legacy_members_v1','public_card_legacy_operations_v1','public_card_final_deployment_v1','public_card_final_logins_v1','public_card_attempts_v1'] LOOP
  EXECUTE format('CREATE TRIGGER card_final_immutable BEFORE UPDATE OR DELETE ON %I FOR EACH ROW EXECUTE FUNCTION public_card_final_immutable_v1()',name);
  EXECUTE format('REVOKE ALL ON TABLE %I FROM PUBLIC',name);
 END LOOP;
 FOREACH name IN ARRAY ARRAY['card_locks_v1','card_freeze_operations_v1','card_freeze_reasons_v1','card_freeze_receipts_v1','public_card_results_v1','public_card_audit_links_v1','public_card_attempts_v1','operations','tasks','audit_events','public_audit_outbox','public_execution_requests'] LOOP
  EXECUTE format('CREATE CONSTRAINT TRIGGER card_final_complete AFTER INSERT OR UPDATE ON %I DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public_card_final_constraint_v1()',name);
 END LOOP;
END $$;
REVOKE ALL ON FUNCTION public_card_final_immutable_v1(),public_card_final_assert_v1(TEXT),public_card_final_constraint_v1() FROM PUBLIC;

-- Final writer fence applies before member functions, including old callers.
-- Disabled deployment keeps all completion/legacy constraints in place.
CREATE FUNCTION public_card_final_writer_v1() RETURNS trigger LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
 IF EXISTS(SELECT 1 FROM public_card_final_deployment_v1 WHERE singleton=1) THEN
  PERFORM singleton FROM public_card_final_deployment_v1 WHERE singleton=1 AND active FOR SHARE;
  IF NOT FOUND OR NOT EXISTS(SELECT 1 FROM public_card_final_logins_v1 WHERE role_name=session_user::text AND reader_version='card-final-v1') THEN RAISE EXCEPTION 'CARD_FINAL_WRITER_VERSION_BLOCKED'; END IF;
  IF TG_TABLE_NAME='card_locks_v1' AND TG_OP<>'INSERT' THEN RAISE EXCEPTION 'CARD_FINAL_UNLOCK_CLOSED'; END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE FUNCTION public_card_final_legacy_seal_v1() RETURNS trigger LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
 IF EXISTS(SELECT 1 FROM public_card_final_deployment_v1) THEN RAISE EXCEPTION 'CARD_FINAL_LEGACY_SET_SEALED'; END IF;
 RETURN NEW;
END $$;
DO $$ DECLARE name TEXT; BEGIN
 FOREACH name IN ARRAY ARRAY['card_locks_v1','card_freeze_operations_v1','card_freeze_reasons_v1','card_freeze_receipts_v1'] LOOP
  EXECUTE format('CREATE TRIGGER card_final_writer BEFORE INSERT OR UPDATE ON %I FOR EACH ROW EXECUTE FUNCTION public_card_final_writer_v1()',name);
 END LOOP;
 FOREACH name IN ARRAY ARRAY['public_card_legacy_members_v1','public_card_legacy_operations_v1'] LOOP
  EXECUTE format('CREATE TRIGGER card_final_legacy_seal BEFORE INSERT ON %I FOR EACH ROW EXECUTE FUNCTION public_card_final_legacy_seal_v1()',name);
 END LOOP;
END $$;
REVOKE ALL ON FUNCTION public_card_final_writer_v1(),public_card_final_legacy_seal_v1() FROM PUBLIC;

-- Catalog/migration identity, not customer data. DDL maintenance must stop the
-- writer and use a forward reviewed activation instead of silently replacing it.
CREATE FUNCTION public_card_final_objects_v1() RETURNS TEXT LANGUAGE sql STABLE SET search_path FROM CURRENT AS $$
 SELECT encode(sha256(convert_to(jsonb_build_object(
 'migrations',(SELECT jsonb_agg(jsonb_build_array(version,encode(checksum,'hex'),success) ORDER BY version) FROM _sqlx_migrations),
 'relations',(SELECT jsonb_agg(jsonb_build_array(c.oid,c.relname,c.relkind) ORDER BY c.relname) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema()),
 'functions',(SELECT jsonb_agg(jsonb_build_array(p.oid,p.proname,pg_get_functiondef(p.oid)) ORDER BY p.oid) FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname=current_schema() AND p.prokind='f'),
 'triggers',(SELECT jsonb_agg(jsonb_build_array(t.oid,pg_get_triggerdef(t.oid),t.tgenabled) ORDER BY t.oid) FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema()),
 'constraints',(SELECT jsonb_agg(jsonb_build_array(c.oid,pg_get_constraintdef(c.oid)) ORDER BY c.oid) FROM pg_constraint c JOIN pg_namespace n ON n.oid=c.connamespace WHERE n.nspname=current_schema())
 )::text,'UTF8')),'hex')
$$;
REVOKE ALL ON FUNCTION public_card_final_objects_v1() FROM PUBLIC;
