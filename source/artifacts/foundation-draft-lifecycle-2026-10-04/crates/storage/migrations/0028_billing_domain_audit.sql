-- LOCAL CANDIDATE, not an adoption/installation of a shared sender.
ALTER TABLE audit_events DROP CONSTRAINT audit_events_kind_check;
ALTER TABLE audit_events ADD CONSTRAINT audit_events_kind_check CHECK (
 kind IN ('TASK_CREATED','CHECKPOINT_SAVED','PLAN_REPLACED','OPERATION_CREATED',
 'OPERATION_TRANSITIONED','CHANGE_REJECTED','CONFIRMATION_PREPARED',
 'EXPLICIT_CONFIRMATION_RECORDED','CONFIRMATION_REVOKED','PERMIT_SAVED',
 'PERMIT_CONSUMED','PERMIT_REVOKED','SIM_AUTH_CHALLENGE_CREATED','SIM_AUTH_FAILED',
 'SIM_AUTH_VERIFIED','SIM_AUTH_GRANT_ISSUED','SIM_AUTH_CHALLENGE_REVOKED',
 'SIM_AUTH_GRANT_REVOKED','SECURITY_DECISION_RECORDED','SECURITY_DECISION_REVOKED',
 'SECURITY_DECISION_INVALIDATED','EXECUTION_REQUEST_RESERVED','BILLING_FEEDBACK_SAVED'));
ALTER TABLE audit_events ADD CONSTRAINT billing_domain_details_v1 CHECK (
 kind <> 'BILLING_FEEDBACK_SAVED' OR (operation_id IS NOT NULL AND operation_revision IS NOT NULL
 AND details->>'schema_version'='1'
 AND details = '{"mapping_version":"billing-feedback-saved-v1","schema_version":1,"action":"SAVE_BILLING_FEEDBACK","result":"SAVED","lock_authorized":false}'::jsonb));

CREATE TABLE public_billing_audit_sources_v1 (
 source_binding_id uuid PRIMARY KEY,
 primary_identity text NOT NULL CHECK (length(primary_identity) BETWEEN 1 AND 200),
 database_name text NOT NULL, database_oid oid NOT NULL, server_port integer NOT NULL,
 namespace text NOT NULL CHECK (namespace=current_schema()),
 member_namespace text NOT NULL CHECK (member_namespace='billing_feedback_v1'),
 member_revision text NOT NULL, source_id text NOT NULL, source_revision text NOT NULL,
 mapping_version text NOT NULL CHECK (mapping_version='billing-feedback-saved-v1'),
 UNIQUE(source_id,source_revision,member_revision)
);
CREATE TABLE public_billing_audit_activations_v1 (
 source_binding_id uuid PRIMARY KEY REFERENCES public_billing_audit_sources_v1,
 legacy_count bigint NOT NULL CHECK (legacy_count>=0),
 installed_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TABLE public_billing_audit_legacy_v1 (
 operation_id text PRIMARY KEY REFERENCES public_billing_feedback_results_v1,
 source_binding_id uuid NOT NULL REFERENCES public_billing_audit_sources_v1,
 owner_id text NOT NULL, request_digest text NOT NULL
);
CREATE TABLE public_billing_audit_links_v1 (
 event_id text PRIMARY KEY REFERENCES audit_events(event_id),
 operation_id text NOT NULL UNIQUE REFERENCES public_billing_feedback_results_v1,
 member_event_id bigint NOT NULL REFERENCES billing_feedback_events_v1,
 source_binding_id uuid NOT NULL REFERENCES public_billing_audit_activations_v1,
 owner_id text NOT NULL, task_id text NOT NULL REFERENCES tasks,
 kind text NOT NULL CHECK (kind='BILLING_FEEDBACK_SAVED'),
 request_digest text NOT NULL, facts_digest text NOT NULL CHECK(facts_digest ~ '^[0-9a-f]{64}$'),
 UNIQUE(source_binding_id,owner_id,operation_id,kind),
 UNIQUE(source_binding_id,member_event_id,kind)
);
CREATE FUNCTION billing_audit_install_only_v1() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF current_setting('safeflow.billing_audit_install',true) IS DISTINCT FROM txid_current()::text THEN
   RAISE EXCEPTION 'explicit trusted billing audit installation required';
 END IF;
 IF TG_TABLE_NAME='public_billing_audit_legacy_v1' AND EXISTS(
  SELECT 1 FROM public_billing_audit_activations_v1 WHERE source_binding_id=NEW.source_binding_id) THEN
  RAISE EXCEPTION 'legacy cannot be extended after activation';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER billing_audit_source_install BEFORE INSERT ON public_billing_audit_sources_v1
 FOR EACH ROW EXECUTE FUNCTION billing_audit_install_only_v1();
CREATE TRIGGER billing_audit_activation_install BEFORE INSERT ON public_billing_audit_activations_v1
 FOR EACH ROW EXECUTE FUNCTION billing_audit_install_only_v1();
CREATE TRIGGER billing_audit_legacy_install BEFORE INSERT ON public_billing_audit_legacy_v1
 FOR EACH ROW EXECUTE FUNCTION billing_audit_install_only_v1();
CREATE TRIGGER billing_audit_source_immutable BEFORE UPDATE OR DELETE ON public_billing_audit_sources_v1
 FOR EACH ROW EXECUTE FUNCTION billing_reject_evidence_change_v1();
CREATE TRIGGER billing_audit_activation_immutable BEFORE UPDATE OR DELETE ON public_billing_audit_activations_v1
 FOR EACH ROW EXECUTE FUNCTION billing_reject_evidence_change_v1();
CREATE TRIGGER billing_audit_legacy_immutable BEFORE UPDATE OR DELETE ON public_billing_audit_legacy_v1
 FOR EACH ROW EXECUTE FUNCTION billing_reject_evidence_change_v1();
CREATE TRIGGER billing_audit_link_immutable BEFORE UPDATE OR DELETE ON public_billing_audit_links_v1
 FOR EACH ROW EXECUTE FUNCTION billing_reject_evidence_change_v1();
REVOKE ALL ON public_billing_audit_sources_v1, public_billing_audit_activations_v1,
 public_billing_audit_legacy_v1, public_billing_audit_links_v1 FROM PUBLIC;

-- Called on the writer's original connection and by deferred triggers. No network,
-- repair, new IDs or time-based legacy inference. SQL validates full stored facts;
-- Rust also rebuilds the archive through the original member report algorithm.
CREATE FUNCTION verify_billing_audit_v1(op text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE r record; s record; l record; legacy record; a record; x record; t record; o record; p record;
 expected_payload jsonb; expected_header jsonb; target jsonb; expected_staged jsonb;
BEGIN
 SELECT result.*,f.source_id,f.account_id,f.transaction_id,f.payload,h.header,
   e.event_id AS member_id,e.event_kind,e.recorded_at,b.archive,b.archive_digest
 INTO r FROM public_billing_feedback_results_v1 result
 JOIN billing_feedback_v1 f ON f.public_operation_id=result.operation_id
   AND f.owner_user_id=result.owner_id AND f.report_id=result.report_id
 JOIN billing_report_bindings_v1 h ON h.owner_user_id=f.owner_user_id AND h.report_id=f.report_id
 JOIN billing_feedback_events_v1 e ON e.public_operation_id=f.public_operation_id
 JOIN public_billing_reports_v1 b ON b.owner_id=result.owner_id AND b.report_id=result.report_id
 WHERE result.operation_id=op;
 IF NOT FOUND THEN RAISE EXCEPTION 'billing full result missing'; END IF;
 SELECT * INTO p FROM public_execution_requests WHERE operation_id=op;
 SELECT * INTO o FROM operations WHERE operation_id=op;
 SELECT * INTO t FROM tasks WHERE task_id=o.task_id;
 SELECT value INTO target FROM jsonb_array_elements(r.archive->'report'->'evidence')
 WHERE value->'reference'=p.fingerprint_payload->'normalized_arguments'->'transaction';
 expected_payload=jsonb_build_object('public_operation_id',op,'owner_user_id',r.owner_id,
  'report_id',r.report_id,'rule_version',r.archive->'report'->'rule_version',
  'snapshot_id',r.archive->'report'->'read'->'snapshot_id',
  'calculated_at',r.archive->'metadata'->'calculated_at','request',r.archive->'request',
  'reference',target->'reference','transaction',target->'transaction',
  'choice',p.fingerprint_payload->'normalized_arguments'->'choice');
 expected_header=jsonb_build_object('owner_user_id',r.owner_id,'report_id',r.report_id,
  'request',r.archive->'request','source_id',r.source_id,
  'snapshot_id',expected_payload->'snapshot_id','rule_version',expected_payload->'rule_version',
  'calculated_at',expected_payload->'calculated_at');
 expected_staged=jsonb_build_object('state','STAGED_AWAITING_PUBLIC_COMMIT','public_operation_id',op,
  'replayed',false,'event',jsonb_build_object('event_id',r.member_id,'event_kind',r.event_kind,
  'recorded_at',to_char(r.recorded_at AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS.US"Z"')));
 IF p.operation_id IS NULL OR target IS NULL OR r.payload IS DISTINCT FROM expected_payload
 OR r.header IS DISTINCT FROM expected_header OR r.staged IS DISTINCT FROM expected_staged
 OR r.member_id<=0 OR r.event_kind<>'BILLING_FEEDBACK_RECORDED_LOCAL_V1'
 OR r.payload->'reference' IS DISTINCT FROM jsonb_build_object('source_id',r.source_id,
   'account_id',r.account_id,'transaction_id',r.transaction_id)
 OR r.archive->>'source_id' IS DISTINCT FROM r.source_id
 OR r.archive->>'owner' IS DISTINCT FROM r.owner_id
 OR p.owner_id IS DISTINCT FROM r.owner_id OR t.owner_id IS DISTINCT FROM r.owner_id
 OR p.task_id IS DISTINCT FROM o.task_id OR p.node_id IS DISTINCT FROM o.node_id
 OR o.plan_version IS DISTINCT FROM p.plan_version OR o.facts_version IS DISTINCT FROM p.facts_version
 OR o.arguments_hash IS DISTINCT FROM p.arguments_hash OR t.plan_version IS DISTINCT FROM p.plan_version
 OR t.facts_version IS DISTINCT FROM p.facts_version
 OR p.tool_name<>'save_billing_feedback' OR p.business_operation_ref<>op OR o.business_operation_ref<>op
 OR p.fingerprint_payload->'normalized_arguments'->>'report_id' IS DISTINCT FROM r.report_id
 OR p.fingerprint_payload->'normalized_arguments'->>'original_digest' IS DISTINCT FROM r.archive_digest
 OR t.state<>'COMPLETED' OR t.checkpoint<>'{"kind":"FINISHED"}'::jsonb
 OR t.completed_nodes IS DISTINCT FROM '["review_feedback","save_feedback"]'::jsonb
 OR t.plan IS DISTINCT FROM jsonb_build_object('contract_version',1,'task_id',o.task_id,'plan_version',p.plan_version,
   'nodes',jsonb_build_array(jsonb_build_object('type','USER_DECISION','id','review_feedback','depends_on','[]'::jsonb),
     jsonb_build_object('type','TOOL','id','save_feedback','tool','save_billing_feedback',
       'arguments',p.fingerprint_payload->'normalized_arguments','depends_on','["review_feedback"]'::jsonb)))
 OR o.state<>'SUCCEEDED' OR o.revision<>p.operation_revision+2
 OR t.revision<>p.task_revision+3
 THEN RAISE EXCEPTION 'billing complete facts contradict'; END IF;
 SELECT src.* INTO s FROM public_billing_audit_sources_v1 src
 JOIN public_billing_audit_activations_v1 act USING(source_binding_id)
 WHERE src.source_id=r.source_id AND src.source_revision=r.archive->>'source_revision'
 AND src.member_revision=r.archive->>'member_revision';
 SELECT * INTO l FROM public_billing_audit_links_v1 WHERE operation_id=op;
 SELECT * INTO legacy FROM public_billing_audit_legacy_v1 WHERE operation_id=op;
 IF s.source_binding_id IS NULL THEN
   IF l.event_id IS NOT NULL OR legacy.operation_id IS NOT NULL THEN RAISE EXCEPTION 'unregistered billing link'; END IF;
   RETURN;
 END IF;
 IF s.namespace<>current_schema() OR s.database_name<>current_database()
 OR s.database_oid<>(SELECT oid FROM pg_database WHERE datname=current_database())
 OR s.server_port<>current_setting('port')::integer THEN RAISE EXCEPTION 'billing source namespace mismatch'; END IF;
 IF legacy.operation_id IS NOT NULL THEN
   IF l.event_id IS NOT NULL OR legacy.source_binding_id<>s.source_binding_id
   OR legacy.owner_id<>r.owner_id OR legacy.request_digest<>r.request_digest
   THEN RAISE EXCEPTION 'legacy billing contradiction'; END IF;
   RETURN;
 END IF;
 IF l.event_id IS NULL OR l.source_binding_id<>s.source_binding_id OR l.owner_id<>r.owner_id
 OR l.task_id<>o.task_id OR l.member_event_id<>r.member_id OR l.request_digest<>r.request_digest
 THEN RAISE EXCEPTION 'billing audit link missing or contradictory'; END IF;
 SELECT * INTO a FROM audit_events WHERE event_id=l.event_id;
 SELECT * INTO x FROM public_audit_outbox WHERE event_id=l.event_id;
 IF a.kind IS DISTINCT FROM 'BILLING_FEEDBACK_SAVED' OR a.operation_id IS DISTINCT FROM op
 OR a.task_id IS DISTINCT FROM o.task_id OR a.task_revision IS DISTINCT FROM t.revision
 OR a.operation_revision IS DISTINCT FROM o.revision
 OR x.event_id IS NULL OR x.sequence IS DISTINCT FROM a.sequence OR x.historical
 OR x.envelope IS DISTINCT FROM jsonb_build_object('storage_version',1,'owner_id',r.owner_id,'public_event',to_jsonb(a))
 THEN RAISE EXCEPTION 'billing audit/outbox contradiction'; END IF;
END $$;

CREATE FUNCTION billing_audit_check_v1() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE op text;
BEGIN
 IF TG_TABLE_NAME='audit_events' THEN
   IF NEW.kind<>'BILLING_FEEDBACK_SAVED' AND NOT EXISTS(
    SELECT 1 FROM public_billing_audit_links_v1 WHERE event_id=NEW.event_id) THEN RETURN NEW; END IF;
   op=NEW.operation_id;
   IF NOT EXISTS(SELECT 1 FROM public_billing_audit_links_v1 WHERE event_id=NEW.event_id) THEN
     RAISE EXCEPTION 'orphan billing domain event';
   END IF;
 ELSIF TG_TABLE_NAME='public_audit_outbox' THEN
   SELECT operation_id INTO op FROM audit_events WHERE event_id=NEW.event_id AND kind='BILLING_FEEDBACK_SAVED';
   IF op IS NULL THEN RETURN NEW; END IF;
 ELSE op=NEW.operation_id;
 END IF;
 PERFORM verify_billing_audit_v1(op);
 RETURN NEW;
END $$;
CREATE CONSTRAINT TRIGGER billing_domain_event_complete AFTER INSERT OR UPDATE ON audit_events
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_audit_check_v1();
CREATE CONSTRAINT TRIGGER billing_domain_result_complete AFTER INSERT OR UPDATE ON public_billing_feedback_results_v1
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_audit_check_v1();
CREATE CONSTRAINT TRIGGER billing_domain_link_complete AFTER INSERT OR UPDATE ON public_billing_audit_links_v1
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_audit_check_v1();
CREATE CONSTRAINT TRIGGER billing_domain_outbox_complete AFTER INSERT OR UPDATE ON public_audit_outbox
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_audit_check_v1();

CREATE FUNCTION billing_audit_install_complete_v1() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE s record; n bigint; op text;
BEGIN
 SELECT src.*,act.legacy_count INTO s FROM public_billing_audit_sources_v1 src
 LEFT JOIN public_billing_audit_activations_v1 act USING(source_binding_id)
 WHERE src.source_binding_id=NEW.source_binding_id;
 SELECT count(*) INTO n FROM public_billing_audit_legacy_v1 WHERE source_binding_id=s.source_binding_id;
 IF s.legacy_count IS NULL OR s.legacy_count<>n THEN RAISE EXCEPTION 'incomplete billing activation'; END IF;
 FOR op IN SELECT operation_id FROM public_billing_audit_legacy_v1 WHERE source_binding_id=s.source_binding_id LOOP
   PERFORM verify_billing_audit_v1(op);
 END LOOP;
 -- Only at activation: every already committed result of this adopted source
 -- must be enumerated, not made a new success or excluded by timestamp.
 IF TG_TABLE_NAME='public_billing_audit_activations_v1' AND EXISTS(
  SELECT 1 FROM public_billing_feedback_results_v1 r JOIN public_billing_reports_v1 b
  ON b.owner_id=r.owner_id AND b.report_id=r.report_id
  WHERE b.archive->>'source_id'=s.source_id AND b.archive->>'source_revision'=s.source_revision
  AND b.archive->>'member_revision'=s.member_revision
  AND NOT EXISTS(SELECT 1 FROM public_billing_audit_legacy_v1 x WHERE x.operation_id=r.operation_id
    AND x.source_binding_id=s.source_binding_id)) THEN RAISE EXCEPTION 'unlisted preactivation result'; END IF;
 RETURN NEW;
END $$;
CREATE CONSTRAINT TRIGGER billing_source_complete AFTER INSERT ON public_billing_audit_sources_v1
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_audit_install_complete_v1();
CREATE CONSTRAINT TRIGGER billing_activation_complete AFTER INSERT ON public_billing_audit_activations_v1
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_audit_install_complete_v1();
CREATE CONSTRAINT TRIGGER billing_legacy_complete AFTER INSERT ON public_billing_audit_legacy_v1
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_audit_install_complete_v1();

CREATE FUNCTION billing_audit_association_v1() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE op text;
BEGIN
 -- Existing inactive/legacy paths retain their old writer semantics. Active
 -- adopted evidence must keep its entire cross-table graph after any mutation.
 FOR op IN SELECT operation_id FROM public_billing_audit_links_v1 LOOP
   PERFORM verify_billing_audit_v1(op);
 END LOOP;
 RETURN NEW;
END $$;
CREATE CONSTRAINT TRIGGER billing_domain_task_association AFTER UPDATE OR DELETE ON tasks
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_audit_association_v1();
CREATE CONSTRAINT TRIGGER billing_domain_operation_association AFTER UPDATE OR DELETE ON operations
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_audit_association_v1();
CREATE CONSTRAINT TRIGGER billing_domain_request_association AFTER UPDATE OR DELETE ON public_execution_requests
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_audit_association_v1();
CREATE CONSTRAINT TRIGGER billing_domain_member_association AFTER UPDATE OR DELETE ON billing_feedback_v1
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_audit_association_v1();
CREATE CONSTRAINT TRIGGER billing_domain_header_association AFTER UPDATE OR DELETE ON billing_report_bindings_v1
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_audit_association_v1();
CREATE CONSTRAINT TRIGGER billing_domain_local_event_association AFTER UPDATE OR DELETE ON billing_feedback_events_v1
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION billing_audit_association_v1();
