-- One explicitly authorized extension of the still-active v43 cohort on v45.
-- The old immutable v43 row/CHECK and all per-payment deadlines remain intact.
CREATE TABLE demo_authority_extensions46_v1 (
 request_id TEXT PRIMARY KEY CHECK(request_id='sf-demo-validity-20261101-nov28-v1'),
 run_id UUID NOT NULL UNIQUE REFERENCES payment_auto_runs_v1(run_id) CHECK(run_id='f0c0a930-8ded-4bc1-9ad0-9165ef7cdf30'::uuid),
 previous_ends_at TIMESTAMPTZ NOT NULL CHECK(previous_ends_at='2026-11-11T00:00:00+08:00'::timestamptz),
 target_ends_at TIMESTAMPTZ NOT NULL CHECK(target_ends_at='2026-11-29T00:00:00+08:00'::timestamptz AND target_ends_at>previous_ends_at),
 transaction_id BIGINT NOT NULL CHECK(transaction_id>0),
 plan JSONB NOT NULL, receipt JSONB NOT NULL,
 task_id TEXT NOT NULL UNIQUE REFERENCES tasks(task_id),
 event_id TEXT NOT NULL UNIQUE REFERENCES audit_events(event_id),
 recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER demo_authority_extension46_immutable BEFORE UPDATE OR DELETE ON demo_authority_extensions46_v1
 FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
REVOKE ALL ON TABLE demo_authority_extensions46_v1 FROM PUBLIC;
CREATE TABLE public_card_final_forward46_v1 (
 deployment_id UUID PRIMARY KEY REFERENCES public_card_final_deployment_v1(deployment_id),
 original_digest TEXT NOT NULL, from_digest TEXT NOT NULL, to_digest TEXT NOT NULL,
 predecessor_checksum BYTEA NOT NULL, migration_checksum BYTEA NOT NULL, upgrade_plan JSONB NOT NULL,
 recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER demo_authority_forward46_immutable BEFORE UPDATE OR DELETE ON public_card_final_forward46_v1
 FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
REVOKE ALL ON TABLE public_card_final_forward46_v1 FROM PUBLIC;

-- Preserve starts_at, run, epoch, roles and task-adoption history. Only the
-- approved effective end is projected; no UPDATE of the original v43 record.
CREATE FUNCTION payment_authority_current46_v1() RETURNS SETOF payment_authority_continuations_v1
 LANGUAGE sql STABLE SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
 SELECT (jsonb_populate_record(NULL::__SCHEMA__.payment_authority_continuations_v1,
   to_jsonb(a)||jsonb_build_object('ends_at',coalesce(e.target_ends_at,a.ends_at)))).*
 FROM payment_authority_continuations_v1 a LEFT JOIN demo_authority_extensions46_v1 e
 ON e.run_id=a.run_id AND e.previous_ends_at=a.ends_at
 AND e.request_id='sf-demo-validity-20261101-nov28-v1'
$$;
REVOKE ALL ON FUNCTION payment_authority_current46_v1() FROM PUBLIC;

-- Invoker-only predicate. It confers no rights and recognizes only this
-- transaction's fixed receipt, never a historical receipt in a later tx.
CREATE FUNCTION demo_authority_expiry_allowed46_v1(old_row JSONB,new_row JSONB,table_name TEXT) RETURNS BOOLEAN
 LANGUAGE plpgsql STABLE SET search_path FROM CURRENT AS $$
BEGIN
 IF session_user::text<>'postgres' OR current_user::text<>'postgres' THEN RETURN false; END IF;
 RETURN (SELECT rolsuper FROM pg_roles WHERE rolname=session_user)
 AND clock_timestamp()<(old_row->>'valid_until')::timestamptz
 AND (new_row->>'valid_until')::timestamptz='2026-11-29T00:00:00+08:00'::timestamptz
 AND (old_row->>'valid_until')::timestamptz='2026-11-11T00:00:00+08:00'::timestamptz
 AND (new_row-'valid_until')=(old_row-'valid_until')
 AND EXISTS(SELECT 1 FROM demo_authority_extensions46_v1 e WHERE e.transaction_id=txid_current()
   AND e.plan->'before'->table_name @> jsonb_build_array(old_row));
END $$;
REVOKE ALL ON FUNCTION demo_authority_expiry_allowed46_v1(JSONB,JSONB,TEXT) FROM PUBLIC;

CREATE FUNCTION demo_authority_expiry_writer46_v1() RETURNS trigger
 LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
 IF NEW.valid_until IS DISTINCT FROM OLD.valid_until THEN
  IF demo_authority_expiry_allowed46_v1(to_jsonb(OLD),to_jsonb(NEW),TG_TABLE_NAME) IS DISTINCT FROM true
  THEN RAISE EXCEPTION 'DEMO_AUTHORITY_EXPIRY_WRITE_REJECTED'; END IF;
 END IF;
 RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION demo_authority_expiry_writer46_v1() FROM PUBLIC;
CREATE TRIGGER demo_principal_expiry46 BEFORE UPDATE ON payment_background_principals_v1
 FOR EACH ROW EXECUTE FUNCTION demo_authority_expiry_writer46_v1();
CREATE TRIGGER demo_fence_expiry46 BEFORE UPDATE ON payment_completion_candidate_fences
 FOR EACH ROW EXECUTE FUNCTION demo_authority_expiry_writer46_v1();

CREATE FUNCTION demo_authority_extension_assert46_v1(rid TEXT) RETURNS VOID
 LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
DECLARE e RECORD; a RECORD; t RECORD; o RECORD;
BEGIN
 SELECT * INTO e FROM demo_authority_extensions46_v1 WHERE request_id=rid;
 IF NOT FOUND THEN RAISE EXCEPTION 'DEMO_AUTHORITY_EXTENSION_MISSING'; END IF;
 SELECT * INTO a FROM audit_events WHERE event_id=e.event_id;
 SELECT * INTO t FROM tasks WHERE task_id=e.task_id;
 SELECT * INTO o FROM public_audit_outbox WHERE event_id=e.event_id;
 IF e.run_id<>'f0c0a930-8ded-4bc1-9ad0-9165ef7cdf30'::uuid
 OR e.plan->>'schema' IS DISTINCT FROM 'DEMO_AUTHORITY_EXTENSION_PLAN_V1'
 OR e.plan->'before'->'continuation' IS DISTINCT FROM (SELECT to_jsonb(c) FROM payment_authority_continuations_v1 c WHERE singleton=1)
 OR e.receipt->>'schema' IS DISTINCT FROM 'DEMO_AUTHORITY_EXTENDED_V1'
 OR e.receipt->>'request_id' IS DISTINCT FROM e.request_id
 OR e.receipt->>'plan_digest' IS DISTINCT FROM encode(sha256(convert_to(e.plan::text,'UTF8')),'hex')
 OR (e.receipt->>'transaction_id')::bigint IS DISTINCT FROM e.transaction_id
 OR e.receipt->>'run_id' IS DISTINCT FROM e.run_id::text
 OR (e.receipt->>'previous_ends_at')::timestamptz IS DISTINCT FROM e.previous_ends_at
 OR (e.receipt->>'target_ends_at')::timestamptz IS DISTINCT FROM e.target_ends_at
 OR e.receipt->'payment_authorized' IS DISTINCT FROM 'false'::jsonb
 OR t.task_id IS NULL OR t.owner_id<>'user_001' OR t.state<>'COMPLETED'
 OR t.plan IS DISTINCT FROM e.plan->'task_plan'
 OR t.creation_digest IS DISTINCT FROM e.receipt->>'plan_digest'
 OR a.event_id IS NULL OR a.task_id IS DISTINCT FROM t.task_id OR a.operation_id IS NOT NULL
 OR a.kind<>'CHECKPOINT_SAVED' OR a.details IS DISTINCT FROM e.receipt
 OR o.event_id IS NULL OR o.sequence IS DISTINCT FROM a.sequence
 OR o.envelope IS DISTINCT FROM jsonb_build_object('storage_version',1,'owner_id','user_001','public_event',to_jsonb(a))
 THEN RAISE EXCEPTION 'DEMO_AUTHORITY_EXTENSION_INCOMPLETE'; END IF;
END $$;
REVOKE ALL ON FUNCTION demo_authority_extension_assert46_v1(TEXT) FROM PUBLIC;
CREATE FUNCTION demo_authority_extension_complete46_v1() RETURNS trigger
 LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
DECLARE p RECORD;
BEGIN
 PERFORM demo_authority_extension_assert46_v1(NEW.request_id);
 IF NEW.transaction_id<>txid_current() OR clock_timestamp()>=NEW.previous_ends_at OR NOT payment_authority_window_v1()
 OR (SELECT count(*) FROM payment_background_principals_v1 WHERE role_name IN('sf_noon_producer_20261030','sf_noon_worker_20261030') AND active AND epoch=1 AND valid_until=NEW.target_ends_at)<>2
 OR (SELECT count(*) FROM payment_completion_candidate_fences WHERE authority_id IN('sf_noon_producer_20261030','sf_noon_worker_20261030') AND enabled AND epoch=1 AND valid_until=NEW.target_ends_at)<>2
 OR (SELECT count(*) FROM pg_roles WHERE rolname IN('sf_noon_producer_20261030','sf_noon_worker_20261030') AND rolcanlogin AND rolvaliduntil=NEW.target_ends_at)<>2
 THEN RAISE EXCEPTION 'DEMO_AUTHORITY_EXTENSION_NOT_ATOMIC'; END IF;
 FOR p IN SELECT value FROM jsonb_array_elements(NEW.plan->'before'->'payment_background_principals_v1') LOOP
  IF NOT EXISTS(SELECT 1 FROM payment_background_principals_v1 x WHERE (to_jsonb(x)-'valid_until')=(p.value-'valid_until'))
  THEN RAISE EXCEPTION 'DEMO_AUTHORITY_PRINCIPAL_DRIFT'; END IF;
 END LOOP;
 FOR p IN SELECT value FROM jsonb_array_elements(NEW.plan->'before'->'payment_completion_candidate_fences') LOOP
  IF NOT EXISTS(SELECT 1 FROM payment_completion_candidate_fences x WHERE (to_jsonb(x)-'valid_until')=(p.value-'valid_until'))
  THEN RAISE EXCEPTION 'DEMO_AUTHORITY_FENCE_DRIFT'; END IF;
 END LOOP;
 FOR p IN SELECT value FROM jsonb_array_elements(NEW.plan->'before'->'roles') LOOP
  IF NOT EXISTS(SELECT 1 FROM pg_roles x WHERE (to_jsonb(x)-'rolvaliduntil'-'rolpassword')=(p.value-'rolvaliduntil'))
  THEN RAISE EXCEPTION 'DEMO_AUTHORITY_ROLE_DRIFT'; END IF;
 END LOOP;
 RETURN NULL;
END $$;
REVOKE ALL ON FUNCTION demo_authority_extension_complete46_v1() FROM PUBLIC;
CREATE CONSTRAINT TRIGGER demo_authority_extension46_complete AFTER INSERT ON demo_authority_extensions46_v1
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION demo_authority_extension_complete46_v1();
