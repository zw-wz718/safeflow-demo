-- Explicit native maintenance on the fixed46 parent. No original JSON, journal,
-- settings, notification or earlier migration is overwritten by installation.
CREATE TABLE subscription_calendar_adoptions47_v1 (
 request_id TEXT PRIMARY KEY CHECK(request_id='sf-calendar-reminder-20261101-v1'),
 revision TEXT NOT NULL UNIQUE CHECK(revision='safeflow-demo-service-calendar-20261101-next-v1'),
 artifact_bytes BYTEA NOT NULL,
 artifact_sha256 TEXT NOT NULL CHECK(artifact_sha256='fe66da754e616c719a4b18d7d1ef3910f14498202a3aaa6599aa25cff436e6bb' AND encode(sha256(artifact_bytes),'hex')=artifact_sha256),
 parent_sha256 TEXT NOT NULL CHECK(parent_sha256='086c5783ef1ab4cb8a2b8f139d1c43287cbdfe7b1fe22ea10af3f5696e4c948d'),
 role_name TEXT NOT NULL CHECK(role_name='sf_noon_reminder_20261101'),
 ends_at TIMESTAMPTZ NOT NULL CHECK(ends_at='2026-11-29T00:00:00+08:00'::timestamptz),
 transaction_id BIGINT NOT NULL CHECK(transaction_id>0),
 plan JSONB NOT NULL,receipt JSONB NOT NULL,
 task_id TEXT NOT NULL UNIQUE REFERENCES tasks(task_id),event_id TEXT NOT NULL UNIQUE REFERENCES audit_events(event_id),
 recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER subscription_calendar47_immutable BEFORE UPDATE OR DELETE ON subscription_calendar_adoptions47_v1
 FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
REVOKE ALL ON subscription_calendar_adoptions47_v1 FROM PUBLIC;
CREATE TABLE public_card_final_forward47_v1 (
 deployment_id UUID PRIMARY KEY REFERENCES public_card_final_deployment_v1(deployment_id),
 original_digest TEXT NOT NULL,from_digest TEXT NOT NULL,to_digest TEXT NOT NULL,
 predecessor_checksum BYTEA NOT NULL,migration_checksum BYTEA NOT NULL,upgrade_plan JSONB NOT NULL,
 recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER subscription_calendar_forward47_immutable BEFORE UPDATE OR DELETE ON public_card_final_forward47_v1
 FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
REVOKE ALL ON public_card_final_forward47_v1 FROM PUBLIC;

CREATE FUNCTION subscription_calendar_assert47_v1() RETURNS VOID
 LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
DECLARE r RECORD;t RECORD;a RECORD;o RECORD;
BEGIN
 SELECT * INTO r FROM subscription_calendar_adoptions47_v1;
 IF NOT FOUND THEN RAISE EXCEPTION 'CALENDAR47_NOT_ADOPTED'; END IF;
 SELECT * INTO t FROM tasks WHERE task_id=r.task_id;
 SELECT * INTO a FROM audit_events WHERE event_id=r.event_id;
 SELECT * INTO o FROM public_audit_outbox WHERE event_id=r.event_id;
 IF encode(sha256(r.artifact_bytes),'hex')<>r.artifact_sha256
 OR r.receipt->>'schema' IS DISTINCT FROM 'SUBSCRIPTION_CALENDAR_REMINDER_ADOPTED_V1'
 OR r.receipt->>'request_id' IS DISTINCT FROM r.request_id
 OR r.receipt->>'artifact_sha256' IS DISTINCT FROM r.artifact_sha256
 OR r.receipt->>'revision' IS DISTINCT FROM r.revision
 OR r.receipt->>'parent_sha256' IS DISTINCT FROM r.parent_sha256
 OR r.receipt->>'role_name' IS DISTINCT FROM r.role_name
 OR (r.receipt->>'ends_at')::timestamptz IS DISTINCT FROM r.ends_at
 OR r.receipt->>'plan_digest' IS DISTINCT FROM encode(sha256(convert_to(r.plan::text,'UTF8')),'hex')
 OR (r.receipt->>'transaction_id')::bigint IS DISTINCT FROM r.transaction_id
 OR r.receipt->'notification_sent' IS DISTINCT FROM 'false'::jsonb
 OR r.receipt->'payment_authorized' IS DISTINCT FROM 'false'::jsonb
 OR t.task_id IS NULL OR t.owner_id<>'user_001' OR t.state<>'COMPLETED'
 OR t.task_id<>'admin_calendar_reminder_20261101_v1'
 OR t.plan IS DISTINCT FROM r.plan->'task_plan' OR t.creation_digest IS DISTINCT FROM r.receipt->>'plan_digest'
 OR a.event_id IS NULL OR a.task_id IS DISTINCT FROM t.task_id OR a.operation_id IS NOT NULL
 OR a.kind<>'CHECKPOINT_SAVED' OR a.details IS DISTINCT FROM r.receipt
 OR o.event_id IS NULL OR o.sequence IS DISTINCT FROM a.sequence
 OR o.envelope IS DISTINCT FROM jsonb_build_object('storage_version',1,'owner_id','user_001','public_event',to_jsonb(a))
 THEN RAISE EXCEPTION 'CALENDAR47_EVIDENCE_INVALID'; END IF;
END $$;
REVOKE ALL ON FUNCTION subscription_calendar_assert47_v1() FROM PUBLIC;
CREATE FUNCTION subscription_calendar_current47_v1() RETURNS BYTEA
 LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
BEGIN
 IF NOT EXISTS(SELECT 1 FROM subscription_calendar_adoptions47_v1) THEN RETURN NULL; END IF;
 PERFORM subscription_calendar_assert47_v1();
 RETURN (SELECT artifact_bytes FROM subscription_calendar_adoptions47_v1);
END $$;
REVOKE ALL ON FUNCTION subscription_calendar_current47_v1() FROM PUBLIC;

CREATE FUNCTION demo_reminder_authority47_v1() RETURNS VOID
 LANGUAGE plpgsql VOLATILE SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
BEGIN
 IF session_user::text<>'sf_noon_reminder_20261101' OR clock_timestamp()>='2026-11-29T00:00:00+08:00'::timestamptz
 OR NOT EXISTS(SELECT 1 FROM pg_roles r WHERE r.rolname=session_user AND r.rolcanlogin
   AND NOT r.rolsuper AND NOT r.rolinherit AND NOT r.rolcreatedb AND NOT r.rolcreaterole
   AND NOT r.rolbypassrls AND NOT r.rolreplication AND r.rolconnlimit=16 AND r.rolconfig IS NULL
   AND r.rolvaliduntil='2026-11-29T00:00:00+08:00'::timestamptz
   AND NOT EXISTS(SELECT 1 FROM pg_auth_members m WHERE m.member=r.oid OR m.roleid=r.oid))
 OR NOT EXISTS(SELECT 1 FROM demo_authority_extensions46_v1 WHERE request_id='sf-demo-validity-20261101-nov28-v1' AND target_ends_at='2026-11-29T00:00:00+08:00'::timestamptz)
 OR NOT EXISTS(SELECT 1 FROM public_unified_fictional_source_v1 WHERE singleton=1 AND active AND source_id='safeflow-team-simulation' AND source_revision='safeflow-team-simulation-20261026-v1' AND artifact_sha256='85b6fc1b3c708c617586afc3e8ea61a78e525d0811439f267c93620238e8c49d')
 OR NOT EXISTS(SELECT 1 FROM transfer_accounts_v1 WHERE owner_id='user_001' AND account_id='account_001')
 OR EXISTS(SELECT 1 FROM card_safety_account_fences_v1 WHERE owner_id='user_001' AND account_id='account_001' AND revoked)
 THEN RAISE EXCEPTION 'REMINDER47_AUTHORITY_UNAVAILABLE'; END IF;
 PERFORM subscription_calendar_assert47_v1();
END $$;
REVOKE ALL ON FUNCTION demo_reminder_authority47_v1() FROM PUBLIC;
-- Before writes AND at COMMIT: a connection authenticated before the boundary
-- cannot use PG password expiry alone to keep saving after the cutoff.
CREATE FUNCTION demo_reminder_write_guard47_v1() RETURNS trigger
 LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
DECLARE previous JSONB; proposed JSONB; n INTEGER;
BEGIN
 IF session_user::text='sf_noon_reminder_20261101' THEN
  PERFORM demo_reminder_authority47_v1();
  IF TG_OP='DELETE' OR TG_TABLE_NAME='subscription_runtime_records' AND to_jsonb(NEW)->>'owner_id'<>'user_001'
   OR TG_TABLE_NAME='subscription_reminder_attempts_v1' AND to_jsonb(NEW)->>'owner_id'<>'user_001'
   OR TG_TABLE_NAME='subscription_simulated_notifications_v1' AND NOT EXISTS(SELECT 1 FROM subscription_reminder_attempts_v1 WHERE attempt_id=(to_jsonb(NEW)->>'attempt_id')::uuid AND owner_id='user_001')
  THEN RAISE EXCEPTION 'REMINDER47_OWNER_MISMATCH'; END IF;
  IF TG_TABLE_NAME='subscription_host_generations_v1' AND (TG_OP<>'UPDATE' OR to_jsonb(NEW) IS DISTINCT FROM to_jsonb(OLD))
  THEN RAISE EXCEPTION 'REMINDER47_HOST_LOCK_ONLY'; END IF;
  IF TG_TABLE_NAME='subscription_runtime_records' THEN
   IF TG_OP<>'UPDATE' THEN RAISE EXCEPTION 'REMINDER47_EXISTING_ONLY'; END IF;
   previous=to_jsonb(OLD)->'journal'; proposed=to_jsonb(NEW)->'journal';
   n=jsonb_array_length(previous->'commands');
   IF proposed-'commands' IS DISTINCT FROM previous-'commands'
    OR jsonb_array_length(proposed->'commands') IS DISTINCT FROM n+1
    OR coalesce(proposed->'commands'->n->>'kind','') NOT IN ('ReserveScan','ResolveScan')
    OR EXISTS(SELECT 1 FROM jsonb_array_elements(previous->'commands') WITH ORDINALITY e(v,i) WHERE e.v IS DISTINCT FROM proposed->'commands'->(e.i::integer-1))
   THEN RAISE EXCEPTION 'REMINDER47_SCAN_ONLY'; END IF;
  END IF;
 END IF;
 RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION demo_reminder_write_guard47_v1() FROM PUBLIC;
CREATE TRIGGER demo_reminder_journal47 BEFORE INSERT OR UPDATE OR DELETE ON subscription_runtime_records FOR EACH ROW EXECUTE FUNCTION demo_reminder_write_guard47_v1();
CREATE TRIGGER demo_reminder_host47 BEFORE INSERT OR UPDATE OR DELETE ON subscription_host_generations_v1 FOR EACH ROW EXECUTE FUNCTION demo_reminder_write_guard47_v1();
CREATE TRIGGER demo_reminder_attempt47 BEFORE INSERT ON subscription_reminder_attempts_v1 FOR EACH ROW EXECUTE FUNCTION demo_reminder_write_guard47_v1();
CREATE TRIGGER demo_reminder_notification47 BEFORE INSERT ON subscription_simulated_notifications_v1 FOR EACH ROW EXECUTE FUNCTION demo_reminder_write_guard47_v1();
CREATE CONSTRAINT TRIGGER demo_reminder_journal_commit47 AFTER INSERT OR UPDATE ON subscription_runtime_records DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION demo_reminder_write_guard47_v1();
CREATE CONSTRAINT TRIGGER demo_reminder_attempt_commit47 AFTER INSERT ON subscription_reminder_attempts_v1 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION demo_reminder_write_guard47_v1();
CREATE CONSTRAINT TRIGGER demo_reminder_notification_commit47 AFTER INSERT ON subscription_simulated_notifications_v1 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION demo_reminder_write_guard47_v1();
CREATE FUNCTION subscription_calendar_complete47_v1() RETURNS trigger
 LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
 PERFORM subscription_calendar_assert47_v1();
 IF NEW.transaction_id<>txid_current() OR clock_timestamp()>=NEW.ends_at
 OR NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname=NEW.role_name AND rolcanlogin AND rolvaliduntil=NEW.ends_at)
 THEN RAISE EXCEPTION 'CALENDAR47_INSTALL_INCOMPLETE'; END IF;
 RETURN NULL;
END $$;
REVOKE ALL ON FUNCTION subscription_calendar_complete47_v1() FROM PUBLIC;
CREATE CONSTRAINT TRIGGER subscription_calendar47_complete AFTER INSERT ON subscription_calendar_adoptions47_v1 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION subscription_calendar_complete47_v1();
