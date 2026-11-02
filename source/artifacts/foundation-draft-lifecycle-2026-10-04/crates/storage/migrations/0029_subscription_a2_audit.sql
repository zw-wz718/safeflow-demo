-- Unfrozen r1 isolated native candidate. Existing migrations and default gates stay.
ALTER TABLE audit_events DROP CONSTRAINT audit_events_kind_check;
ALTER TABLE audit_events ADD CONSTRAINT audit_events_kind_check CHECK (kind IN (
'TASK_CREATED','CHECKPOINT_SAVED','PLAN_REPLACED','OPERATION_CREATED','OPERATION_TRANSITIONED',
'CHANGE_REJECTED','CONFIRMATION_PREPARED','EXPLICIT_CONFIRMATION_RECORDED','CONFIRMATION_REVOKED',
'PERMIT_SAVED','PERMIT_CONSUMED','PERMIT_REVOKED','SIM_AUTH_CHALLENGE_CREATED','SIM_AUTH_FAILED',
'SIM_AUTH_VERIFIED','SIM_AUTH_GRANT_ISSUED','SIM_AUTH_CHALLENGE_REVOKED','SIM_AUTH_GRANT_REVOKED',
'SECURITY_DECISION_RECORDED','SECURITY_DECISION_REVOKED','SECURITY_DECISION_INVALIDATED',
'EXECUTION_REQUEST_RESERVED','BILLING_FEEDBACK_SAVED','SUBSCRIPTION_LOCAL_CHANGE_RECORDED'));

CREATE FUNCTION subscription_a2_details_v1(d jsonb) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$
DECLARE stamp text; parsed timestamptz;
BEGIN
 IF jsonb_typeof(d)<>'object' OR (SELECT count(*) FROM jsonb_object_keys(d))<>9
 OR NOT d ?& ARRAY['mapping_version','action','result','business_ref','business_occurred_at',
 'business_time_source','fictional','production_registered','notification_sent']
 OR d->>'mapping_version'<>'safeflow.subscription.local-result.v1'
 OR NOT ((d->>'action'='REGISTRATION' AND d->>'result'='ACTIVE')
 OR (d->>'action'='REMINDER_SETTINGS' AND d->>'result'='SETTINGS_SAVED')
 OR (d->>'action'='RECORD_CANCELLATION' AND d->>'result'='RECORDED')
 OR (d->>'action'='REVOKE_CANCELLATION' AND d->>'result'='REVOKED'))
 OR d->>'business_time_source'<>'FICTIONAL_COMMAND_NOW'
 OR d->'fictional'<>'true'::jsonb OR d->'production_registered'<>'false'::jsonb
 OR d->'notification_sent'<>'false'::jsonb
 OR jsonb_typeof(d->'business_ref')<>'string'
 OR d->>'business_ref'!~'^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
 OR d->>'business_ref'='00000000-0000-0000-0000-000000000000'
 OR EXISTS(SELECT 1 FROM jsonb_each(d) e WHERE e.key NOT IN('fictional','production_registered','notification_sent') AND jsonb_typeof(e.value)<>'string')
 THEN RETURN false; END IF;
 stamp=d->>'business_occurred_at';
 IF stamp IS NULL OR stamp ~ '[\r\n]' OR stamp !~ '^[0-9]{4}-(0[1-9]|1[0-2])-(0[1-9]|[12][0-9]|3[01])[Tt]([01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9](\.[0-9]+)?([Zz]|[+-]([01][0-9]|2[0-3]):[0-5][0-9])$'
 OR left(stamp,4)='0000' THEN RETURN false; END IF;
 -- Date cast rejects impossible Gregorian dates; explicit syntax rejects PG's
 -- normalisation of hour24/leapseconds/offsets/loose whitespace before cast.
 PERFORM substring(stamp,1,10)::date;
 parsed=stamp::timestamptz;
 RETURN parsed IS NOT NULL;
EXCEPTION WHEN others THEN RETURN false;
END $$;
ALTER TABLE audit_events ADD CONSTRAINT subscription_a2_details_check CHECK (
 kind<>'SUBSCRIPTION_LOCAL_CHANGE_RECORDED' OR
 (operation_id IS NOT NULL AND operation_revision IS NOT NULL AND subscription_a2_details_v1(details) IS TRUE));
CREATE UNIQUE INDEX subscription_a2_one_domain ON audit_events(operation_id)
 WHERE kind='SUBSCRIPTION_LOCAL_CHANGE_RECORDED';

CREATE TABLE public_subscription_a2_sources_v1 (
 source_id uuid PRIMARY KEY, owner_id text NOT NULL UNIQUE REFERENCES subscription_runtime_records(owner_id),
 source_revision text NOT NULL CHECK(length(source_revision) BETWEEN 1 AND 200),
 database_name text NOT NULL, database_oid oid NOT NULL, namespace text NOT NULL,
 namespace_oid oid NOT NULL, server_port integer NOT NULL,
 member_codec text NOT NULL CHECK(member_codec='safeflow.subscription.fictional-journal.v1'),
 mapping_version text NOT NULL CHECK(mapping_version='safeflow.subscription.local-result.v1'),
 legacy_count bigint NOT NULL CHECK(legacy_count>=0)
);
CREATE TABLE public_subscription_a2_legacy_v1 (
 operation_id text PRIMARY KEY REFERENCES subscription_public_bindings_v1(operation_id),
 business_ref text NOT NULL UNIQUE REFERENCES subscription_public_bindings_v1(business_ref),
 owner_id text NOT NULL, source_id uuid NOT NULL REFERENCES public_subscription_a2_sources_v1,
 facts jsonb NOT NULL CHECK(jsonb_typeof(facts)='object'),
 facts_digest text NOT NULL CHECK(facts_digest ~ '^[0-9a-f]{64}$')
);
CREATE TABLE public_subscription_a2_links_v1 (
 operation_id text PRIMARY KEY REFERENCES subscription_public_bindings_v1(operation_id),
 business_ref text NOT NULL UNIQUE REFERENCES subscription_public_bindings_v1(business_ref),
 owner_id text NOT NULL, source_id uuid NOT NULL REFERENCES public_subscription_a2_sources_v1,
 event_id text NOT NULL UNIQUE REFERENCES audit_events(event_id),
 facts jsonb NOT NULL CHECK(jsonb_typeof(facts)='object'),
 facts_digest text NOT NULL CHECK(facts_digest ~ '^[0-9a-f]{64}$')
);
CREATE FUNCTION subscription_a2_install_only_v1() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF current_setting('safeflow.subscription_a2_install',true) IS DISTINCT FROM txid_current()::text
 OR NOT (starts_with(current_database(),'billing_test_') OR starts_with(current_database(),'subscription_test_'))
 OR NOT (starts_with(current_schema(),'sf_subscription_test_') OR starts_with(current_schema(),'sf_http_test_'))
 THEN RAISE EXCEPTION 'explicit isolated subscription installation required'; END IF;
 IF TG_TABLE_NAME='public_subscription_a2_sources_v1' THEN
 IF (
 NEW.database_name<>current_database() OR NEW.database_oid<>(SELECT oid FROM pg_database WHERE datname=current_database())
 OR NEW.namespace<>current_schema() OR NEW.namespace_oid<>current_schema()::regnamespace::oid
 OR NEW.server_port<>current_setting('port')::integer) THEN RAISE EXCEPTION 'subscription source scope mismatch'; END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER subscription_a2_source_install BEFORE INSERT ON public_subscription_a2_sources_v1
 FOR EACH ROW EXECUTE FUNCTION subscription_a2_install_only_v1();
CREATE TRIGGER subscription_a2_legacy_install BEFORE INSERT ON public_subscription_a2_legacy_v1
 FOR EACH ROW EXECUTE FUNCTION subscription_a2_install_only_v1();
CREATE TRIGGER subscription_a2_source_immutable BEFORE UPDATE OR DELETE ON public_subscription_a2_sources_v1
 FOR EACH ROW EXECUTE FUNCTION protect_subscription_binding_v1();
CREATE TRIGGER subscription_a2_legacy_immutable BEFORE UPDATE OR DELETE ON public_subscription_a2_legacy_v1
 FOR EACH ROW EXECUTE FUNCTION protect_subscription_binding_v1();
CREATE TRIGGER subscription_a2_link_immutable BEFORE UPDATE OR DELETE ON public_subscription_a2_links_v1
 FOR EACH ROW EXECUTE FUNCTION protect_subscription_binding_v1();
REVOKE ALL ON public_subscription_a2_sources_v1,public_subscription_a2_legacy_v1,public_subscription_a2_links_v1 FROM PUBLIC;

-- Rust first loads the original full member codec and typed historical getter;
-- this SQL independently requires the complete saved graph on the same tx.
CREATE FUNCTION verify_subscription_a2_v1(op text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE b record; s record; l record; a record; x record; q record; p record; c record; t record; o record;
 f jsonb; cmd jsonb; expected jsonb; prefix jsonb; old boolean; act text;
BEGIN
 SELECT * INTO b FROM subscription_public_bindings_v1 WHERE operation_id=op;
 IF NOT FOUND THEN RAISE EXCEPTION 'subscription binding missing'; END IF;
 SELECT * INTO s FROM public_subscription_a2_sources_v1 WHERE owner_id=b.owner_id;
 IF NOT FOUND THEN
  IF EXISTS(SELECT 1 FROM audit_events WHERE operation_id=op AND kind='SUBSCRIPTION_LOCAL_CHANGE_RECORDED')
  OR EXISTS(SELECT 1 FROM public_subscription_a2_links_v1 WHERE operation_id=op)
  OR EXISTS(SELECT 1 FROM public_subscription_a2_legacy_v1 WHERE operation_id=op) THEN
   RAISE EXCEPTION 'unactivated subscription event'; END IF;
  RETURN;
 END IF;
 IF s.database_name<>current_database() OR s.database_oid<>(SELECT oid FROM pg_database WHERE datname=current_database())
 OR s.namespace<>current_schema() OR s.namespace_oid<>current_schema()::regnamespace::oid
 OR s.server_port<>current_setting('port')::integer THEN RAISE EXCEPTION 'subscription source scope changed'; END IF;
 SELECT * INTO o FROM operations WHERE operation_id=op;
 IF o.state IS DISTINCT FROM 'SUCCEEDED' THEN
  IF EXISTS(SELECT 1 FROM public_subscription_a2_links_v1 WHERE operation_id=op)
   OR EXISTS(SELECT 1 FROM public_subscription_a2_legacy_v1 WHERE operation_id=op)
   OR EXISTS(SELECT 1 FROM audit_events WHERE operation_id=op AND kind='SUBSCRIPTION_LOCAL_CHANGE_RECORDED')
   THEN RAISE EXCEPTION 'subscription success without original result'; END IF;
  RETURN;
 END IF;
 SELECT * INTO l FROM public_subscription_a2_legacy_v1 WHERE operation_id=op;
 old=FOUND;
 IF old THEN
  IF EXISTS(SELECT 1 FROM public_subscription_a2_links_v1 WHERE operation_id=op)
  OR EXISTS(SELECT 1 FROM audit_events WHERE operation_id=op AND kind='SUBSCRIPTION_LOCAL_CHANGE_RECORDED')
  THEN RAISE EXCEPTION 'legacy event backfill forbidden'; END IF;
 ELSE
  SELECT * INTO l FROM public_subscription_a2_links_v1 WHERE operation_id=op;
  IF NOT FOUND THEN RAISE EXCEPTION 'subscription domain link missing'; END IF;
 END IF;
 f=l.facts; cmd=f->'original_command';
 SELECT * INTO t FROM tasks WHERE task_id=o.task_id;
 SELECT * INTO q FROM public_execution_requests WHERE operation_id=op;
 SELECT * INTO p FROM execution_permits WHERE permit_id=f->>'permit_id';
 SELECT * INTO c FROM server_confirmations WHERE confirmation_ref=p.confirmation_ref;
 SELECT journal INTO prefix FROM subscription_runtime_records WHERE owner_id=b.owner_id;
 prefix=jsonb_set(prefix,'{commands}',COALESCE((SELECT jsonb_agg(v ORDER BY n)
 FROM jsonb_array_elements(prefix->'commands') WITH ORDINALITY j(v,n)
 WHERE n<=jsonb_array_length(f->'journal_prefix'->'commands')),'[]'::jsonb));
 IF l.source_id IS DISTINCT FROM s.source_id OR l.owner_id IS DISTINCT FROM b.owner_id
 OR l.business_ref IS DISTINCT FROM b.business_ref OR f->'public_binding' IS DISTINCT FROM to_jsonb(b)
 OR f->'original_binding' IS DISTINCT FROM b.member_binding
 OR f->'public_request' IS DISTINCT FROM to_jsonb(q) OR f->'db_task' IS DISTINCT FROM to_jsonb(t)
 OR f->'db_operation' IS DISTINCT FROM to_jsonb(o) OR f->'db_permit' IS DISTINCT FROM to_jsonb(p)
 OR f->'db_confirmation' IS DISTINCT FROM to_jsonb(c)
 OR prefix IS DISTINCT FROM f->'journal_prefix'
 OR prefix->'commands'->-1 IS DISTINCT FROM cmd
 OR b.original_request->'original_binding' IS DISTINCT FROM b.member_binding
 OR b.original_request->'command_kind' IS DISTINCT FROM cmd->'kind'
 OR b.member_binding->'context'->>'user_id' IS DISTINCT FROM b.owner_id
 OR b.member_binding->'context'->>'operation_id' IS DISTINCT FROM op
 OR b.member_binding->'context'->>'idempotency_key' IS DISTINCT FROM b.idempotency_key
 OR b.member_binding->'context'->>'task_id' IS DISTINCT FROM o.task_id
 OR b.member_binding->'context'->>'node_id' IS DISTINCT FROM o.node_id
 OR b.member_binding->'context'->'plan_version' IS DISTINCT FROM to_jsonb(o.plan_version)
 OR b.public_parameters->>'business_ref' IS DISTINCT FROM b.business_ref
 OR q.owner_id IS DISTINCT FROM b.owner_id OR q.business_operation_ref IS DISTINCT FROM b.business_ref
 OR q.fingerprint_payload->'normalized_arguments' IS DISTINCT FROM b.public_parameters
 OR q.first_operation IS NULL OR q.submitted_at IS NULL
 OR p.operation_id IS DISTINCT FROM op OR p.status IS DISTINCT FROM 'CONSUMED'
 OR p.execution_scope IS DISTINCT FROM 'COMMON' OR p.fingerprint_payload IS DISTINCT FROM q.fingerprint_payload
 OR p.session_digest IS DISTINCT FROM b.session_digest OR p.session_binding_version IS DISTINCT FROM b.binding_version
 OR c.status IS DISTINCT FROM 'CONFIRMED' OR c.confirmed_at IS NULL
 OR c.session_digest IS DISTINCT FROM b.session_digest OR c.session_binding_version IS DISTINCT FROM b.binding_version
 OR c.operation_id IS DISTINCT FROM op OR c.binding_digest IS DISTINCT FROM p.binding_digest
 OR o.revision IS DISTINCT FROM q.operation_revision+2 OR t.revision IS DISTINCT FROM q.task_revision+3
 OR t.state IS DISTINCT FROM 'COMPLETED' OR t.owner_id IS DISTINCT FROM b.owner_id
 THEN RAISE EXCEPTION 'subscription complete original facts contradiction'; END IF;
 act=b.public_parameters->>'action';
 IF (act='REGISTRATION' AND (cmd->>'kind'<>'Register' OR cmd->'receipt'->>'state'<>'ACTIVE'))
 OR (act='REMINDER_SETTINGS' AND cmd->>'kind'<>'Settings')
 OR (act='RECORD_CANCELLATION' AND (cmd->>'kind'<>'Intent' OR b.member_binding->>'kind'<>'RECORD'
 OR cmd->'receipt'->>'kind'<>'RECORD' OR cmd->'receipt'->>'local_intent_state'<>'RECORDED'
 OR cmd->'receipt'->>'ledger_state_at_operation'<>'CANCELLING'))
 OR (act='REVOKE_CANCELLATION' AND (cmd->>'kind'<>'Intent' OR b.member_binding->>'kind'<>'REVOKE'
 OR cmd->'receipt'->>'kind'<>'REVOKE' OR cmd->'receipt'->>'local_intent_state'<>'REVOKED'
 OR cmd->'receipt'->>'ledger_state_at_operation'<>'ACTIVE'))
 OR cmd->'receipt'->>'operation_id' IS DISTINCT FROM op
 THEN RAISE EXCEPTION 'subscription original action/result contradiction'; END IF;
 expected=jsonb_build_object('mapping_version',s.mapping_version,'action',act,'result',CASE act
 WHEN 'REGISTRATION' THEN 'ACTIVE' WHEN 'REMINDER_SETTINGS' THEN 'SETTINGS_SAVED'
 WHEN 'RECORD_CANCELLATION' THEN 'RECORDED' WHEN 'REVOKE_CANCELLATION' THEN 'REVOKED' END,
 'business_ref',b.business_ref,'business_occurred_at',cmd->'now','business_time_source','FICTIONAL_COMMAND_NOW',
 'fictional',cmd->'receipt'->'fictional','production_registered',cmd->'receipt'->'production_registered',
 'notification_sent',cmd->'receipt'->'notification_sent');
 IF subscription_a2_details_v1(expected) IS NOT TRUE THEN RAISE EXCEPTION 'invalid original subscription details'; END IF;
 IF old THEN RETURN; END IF;
 SELECT * INTO a FROM audit_events WHERE event_id=l.event_id;
 SELECT * INTO x FROM public_audit_outbox WHERE event_id=l.event_id;
 IF a.kind IS DISTINCT FROM 'SUBSCRIPTION_LOCAL_CHANGE_RECORDED' OR a.operation_id IS DISTINCT FROM op
 OR a.task_id IS DISTINCT FROM o.task_id OR a.task_revision IS DISTINCT FROM t.revision
 OR a.operation_revision IS DISTINCT FROM o.revision OR a.details IS DISTINCT FROM expected
 OR x.event_id IS NULL OR x.sequence IS DISTINCT FROM a.sequence OR x.historical IS DISTINCT FROM false
 OR x.envelope IS DISTINCT FROM jsonb_build_object('storage_version',1,'owner_id',b.owner_id,'public_event',to_jsonb(a))
 THEN RAISE EXCEPTION 'subscription event/outbox contradiction'; END IF;
END $$;

CREATE FUNCTION subscription_a2_association_v1() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE op text;
BEGIN
 IF TG_TABLE_NAME='audit_events' THEN
  IF NEW.kind<>'SUBSCRIPTION_LOCAL_CHANGE_RECORDED' THEN RETURN NEW; END IF;
  IF NOT EXISTS(SELECT 1 FROM public_subscription_a2_links_v1 WHERE event_id=NEW.event_id) THEN
   RAISE EXCEPTION 'orphan subscription domain event'; END IF;
  PERFORM verify_subscription_a2_v1(NEW.operation_id);
 ELSIF TG_TABLE_NAME='public_subscription_a2_links_v1' THEN
  PERFORM verify_subscription_a2_v1(NEW.operation_id);
 END IF;
 -- Closed candidate only; verify existing successes and staged failures too.
 FOR op IN SELECT b.operation_id FROM subscription_public_bindings_v1 b
 JOIN public_subscription_a2_sources_v1 s ON s.owner_id=b.owner_id LOOP
  PERFORM verify_subscription_a2_v1(op);
 END LOOP;
 RETURN NEW;
END $$;
CREATE CONSTRAINT TRIGGER subscription_a2_event AFTER INSERT OR UPDATE ON audit_events DEFERRABLE INITIALLY DEFERRED
 FOR EACH ROW EXECUTE FUNCTION subscription_a2_association_v1();
CREATE CONSTRAINT TRIGGER subscription_a2_link AFTER INSERT ON public_subscription_a2_links_v1 DEFERRABLE INITIALLY DEFERRED
 FOR EACH ROW EXECUTE FUNCTION subscription_a2_association_v1();
CREATE CONSTRAINT TRIGGER subscription_a2_outbox AFTER INSERT OR UPDATE OR DELETE ON public_audit_outbox DEFERRABLE INITIALLY DEFERRED
 FOR EACH ROW EXECUTE FUNCTION subscription_a2_association_v1();
CREATE CONSTRAINT TRIGGER subscription_a2_operation AFTER INSERT OR UPDATE OR DELETE ON operations DEFERRABLE INITIALLY DEFERRED
 FOR EACH ROW EXECUTE FUNCTION subscription_a2_association_v1();
CREATE CONSTRAINT TRIGGER subscription_a2_task AFTER UPDATE OR DELETE ON tasks DEFERRABLE INITIALLY DEFERRED
 FOR EACH ROW EXECUTE FUNCTION subscription_a2_association_v1();
CREATE CONSTRAINT TRIGGER subscription_a2_request AFTER INSERT OR UPDATE OR DELETE ON public_execution_requests DEFERRABLE INITIALLY DEFERRED
 FOR EACH ROW EXECUTE FUNCTION subscription_a2_association_v1();
CREATE CONSTRAINT TRIGGER subscription_a2_journal AFTER INSERT OR UPDATE OR DELETE ON subscription_runtime_records DEFERRABLE INITIALLY DEFERRED
 FOR EACH ROW EXECUTE FUNCTION subscription_a2_association_v1();
CREATE CONSTRAINT TRIGGER subscription_a2_permit AFTER UPDATE OR DELETE ON execution_permits DEFERRABLE INITIALLY DEFERRED
 FOR EACH ROW EXECUTE FUNCTION subscription_a2_association_v1();
CREATE CONSTRAINT TRIGGER subscription_a2_confirmation AFTER UPDATE OR DELETE ON server_confirmations DEFERRABLE INITIALLY DEFERRED
 FOR EACH ROW EXECUTE FUNCTION subscription_a2_association_v1();

CREATE FUNCTION subscription_a2_install_complete_v1() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE s record; op text;
BEGIN
 SELECT * INTO s FROM public_subscription_a2_sources_v1 WHERE source_id=NEW.source_id;
 IF s.legacy_count IS DISTINCT FROM (SELECT count(*) FROM public_subscription_a2_legacy_v1 WHERE source_id=s.source_id)
 OR EXISTS(SELECT 1 FROM subscription_public_bindings_v1 b JOIN operations o USING(operation_id)
 WHERE b.owner_id=s.owner_id AND o.state='SUCCEEDED'
 AND NOT EXISTS(SELECT 1 FROM public_subscription_a2_legacy_v1 l WHERE l.operation_id=b.operation_id AND l.source_id=s.source_id))
 THEN RAISE EXCEPTION 'incomplete exact subscription legacy enumeration'; END IF;
 FOR op IN SELECT operation_id FROM public_subscription_a2_legacy_v1 WHERE source_id=s.source_id LOOP
  PERFORM verify_subscription_a2_v1(op);
 END LOOP;
 RETURN NEW;
END $$;
CREATE CONSTRAINT TRIGGER subscription_a2_install_complete AFTER INSERT ON public_subscription_a2_sources_v1 DEFERRABLE INITIALLY DEFERRED
 FOR EACH ROW EXECUTE FUNCTION subscription_a2_install_complete_v1();
CREATE CONSTRAINT TRIGGER subscription_a2_legacy_complete AFTER INSERT ON public_subscription_a2_legacy_v1 DEFERRABLE INITIALLY DEFERRED
 FOR EACH ROW EXECUTE FUNCTION subscription_a2_install_complete_v1();
