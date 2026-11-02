-- Native organizer-only release of this one adopted demonstration card.
-- No web/model release grant, no history deletion, no funds/claim modification.
CREATE TABLE demo_card_releases45_v1 (
 request_id TEXT PRIMARY KEY CHECK(request_id='sf-card-release-20261101-user001-v1'),
 owner_id TEXT NOT NULL CHECK(owner_id='user_001'),
 account_id TEXT NOT NULL CHECK(account_id='account_001'),
 card_id TEXT NOT NULL CHECK(card_id='demo_card_salary_v1'),
 lock_id TEXT NOT NULL UNIQUE REFERENCES card_locks_v1(lock_id),
 record_id TEXT NOT NULL REFERENCES card_freeze_operations_v1(record_id),
 before_revision BIGINT NOT NULL CHECK(before_revision>0),
 after_revision BIGINT NOT NULL CHECK(after_revision=before_revision+1),
 transaction_id BIGINT NOT NULL CHECK(transaction_id=txid_current()),
 plan JSONB NOT NULL, receipt JSONB NOT NULL,
 task_id TEXT NOT NULL UNIQUE REFERENCES tasks(task_id),
 event_id TEXT NOT NULL UNIQUE REFERENCES audit_events(event_id),
 recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER demo_card_release_immutable BEFORE UPDATE OR DELETE ON demo_card_releases45_v1
 FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
REVOKE ALL ON TABLE demo_card_releases45_v1 FROM PUBLIC;
CREATE TABLE public_card_final_forward45_v1 (
 deployment_id UUID PRIMARY KEY REFERENCES public_card_final_deployment_v1(deployment_id),
 original_digest TEXT NOT NULL, from_digest TEXT NOT NULL, to_digest TEXT NOT NULL,
 predecessor_checksum BYTEA NOT NULL, migration_checksum BYTEA NOT NULL, upgrade_plan JSONB NOT NULL,
 recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER demo_card_forward45_immutable BEFORE UPDATE OR DELETE ON public_card_final_forward45_v1
 FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
REVOKE ALL ON TABLE public_card_final_forward45_v1 FROM PUBLIC;

CREATE FUNCTION demo_card_release_assert45_v1(rid TEXT) RETURNS VOID
 LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
DECLARE r RECORD; l RECORD; f RECORD; a RECORD; t RECORD; o RECORD;
BEGIN
 SELECT * INTO r FROM demo_card_releases45_v1 WHERE request_id=rid;
 IF NOT FOUND THEN RAISE EXCEPTION 'DEMO_RELEASE_MISSING'; END IF;
 SELECT * INTO l FROM card_locks_v1 WHERE lock_id=r.lock_id;
 SELECT * INTO f FROM card_freeze_operations_v1 WHERE record_id=r.record_id;
 SELECT * INTO a FROM audit_events WHERE event_id=r.event_id;
 SELECT * INTO t FROM tasks WHERE task_id=r.task_id;
 SELECT * INTO o FROM public_audit_outbox WHERE event_id=r.event_id;
 IF l.lock_id IS NULL OR l.card_id IS DISTINCT FROM r.card_id OR l.source IS DISTINCT FROM 'USER_SELF'
 OR l.state IS DISTINCT FROM 'RELEASED' OR l.event_scope IS NOT NULL OR l.event_id IS NOT NULL
 OR f.lock_id IS DISTINCT FROM r.lock_id OR f.card_id IS DISTINCT FROM r.card_id
 OR f.owner_id IS DISTINCT FROM r.owner_id OR f.account_id IS DISTINCT FROM r.account_id
 OR f.source IS DISTINCT FROM 'USER_SELF' OR f.reason NOT IN('USER_SECURITY_CONCERN','USER_REPORT_LOST')
 OR NOT EXISTS(SELECT 1 FROM card_catalog_v1 c JOIN transfer_accounts_v1 x USING(account_id)
 WHERE c.card_id=r.card_id AND c.account_id=r.account_id AND c.last_four='6628' AND x.owner_id=r.owner_id)
 OR t.task_id IS NULL OR t.owner_id IS DISTINCT FROM r.owner_id OR t.state IS DISTINCT FROM 'COMPLETED'
 OR t.plan IS DISTINCT FROM r.plan->'task_plan' OR t.creation_digest IS DISTINCT FROM r.receipt->>'plan_digest'
 OR a.event_id IS NULL OR a.kind IS DISTINCT FROM 'CHECKPOINT_SAVED' OR a.operation_id IS NOT NULL
 OR a.task_id IS DISTINCT FROM r.task_id OR a.details IS DISTINCT FROM r.receipt
 OR o.event_id IS NULL OR o.sequence IS DISTINCT FROM a.sequence
 OR o.envelope IS DISTINCT FROM jsonb_build_object('storage_version',1,'owner_id',r.owner_id,'public_event',to_jsonb(a))
 OR r.receipt->>'schema' IS DISTINCT FROM 'DEMO_CARD_USER_SELF_RELEASED_V1'
 OR r.receipt->>'request_id' IS DISTINCT FROM r.request_id OR r.receipt->>'lock_id' IS DISTINCT FROM r.lock_id
 OR r.receipt->>'record_id' IS DISTINCT FROM r.record_id
 OR (r.receipt->>'before_revision')::bigint IS DISTINCT FROM r.before_revision
 OR (r.receipt->>'after_revision')::bigint IS DISTINCT FROM r.after_revision
 OR (r.receipt->>'transaction_id')::bigint IS DISTINCT FROM r.transaction_id
 THEN RAISE EXCEPTION 'DEMO_RELEASE_INCOMPLETE'; END IF;
END $$;
REVOKE ALL ON FUNCTION demo_card_release_assert45_v1(TEXT) FROM PUBLIC;
CREATE FUNCTION demo_card_release_complete45_v1() RETURNS trigger
 LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
 PERFORM demo_card_release_assert45_v1(NEW.request_id);
 IF (SELECT revision FROM card_account_versions_v1 WHERE account_id=NEW.account_id) IS DISTINCT FROM NEW.after_revision
 OR NEW.transaction_id<>txid_current() THEN RAISE EXCEPTION 'DEMO_RELEASE_VERSION'; END IF;
 RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER demo_card_release_complete AFTER INSERT ON demo_card_releases45_v1
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION demo_card_release_complete45_v1();
REVOKE ALL ON FUNCTION demo_card_release_complete45_v1() FROM PUBLIC;

-- Retain original freeze writer fence. Only the fixed native administrator,
-- a same-transaction receipt, and ACTIVE -> RELEASED on the original self lock
-- can reach this branch. API/producer/worker/customer/model never receive it.
CREATE OR REPLACE FUNCTION public_card_final_writer_v1() RETURNS trigger LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
 IF EXISTS(SELECT 1 FROM public_card_final_deployment_v1 WHERE singleton=1) THEN
  PERFORM singleton FROM public_card_final_deployment_v1 WHERE singleton=1 AND active FOR SHARE;
  IF NOT FOUND THEN RAISE EXCEPTION 'CARD_FINAL_WRITER_VERSION_BLOCKED'; END IF;
  IF TG_TABLE_NAME='card_locks_v1' AND TG_OP='UPDATE' THEN
   IF session_user::text='postgres' AND current_user::text='postgres'
   AND (SELECT rolsuper FROM pg_roles WHERE rolname=session_user)
   AND current_database()='billing_test_noon_acceptance_20261029'
   AND current_schema()='sf_unified_noon_acceptance'
   AND OLD.card_id='demo_card_salary_v1' AND OLD.source='USER_SELF' AND OLD.state='ACTIVE'
   AND NEW.state='RELEASED' AND (to_jsonb(NEW)-'state')=(to_jsonb(OLD)-'state')
   AND EXISTS(SELECT 1 FROM demo_card_releases45_v1 r WHERE r.lock_id=OLD.lock_id AND r.transaction_id=txid_current()
     AND r.before_revision=(SELECT revision FROM card_account_versions_v1 WHERE account_id=r.account_id))
   THEN RETURN NEW; END IF;
   RAISE EXCEPTION 'CARD_FINAL_UNLOCK_CLOSED';
  END IF;
  IF NOT EXISTS(SELECT 1 FROM public_card_final_logins_v1 WHERE role_name=session_user::text AND reader_version='card-final-v1')
  THEN RAISE EXCEPTION 'CARD_FINAL_WRITER_VERSION_BLOCKED'; END IF;
 END IF;
 RETURN NEW;
END $$;

-- A NEW self-freeze may create a NEW lock after this exact recorded release.
-- Replays still return the old result before reaching this check. No old lock
-- is reopened; other sources and unproven released locks keep coordination.
CREATE FUNCTION card_fresh_self_after_release45_v1(card TEXT) RETURNS BOOLEAN
 LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
BEGIN
 IF $1<>'demo_card_salary_v1' OR NOT EXISTS(SELECT 1 FROM demo_card_releases45_v1 WHERE card_id=$1)
 THEN RETURN false; END IF;
 PERFORM demo_card_release_assert45_v1('sf-card-release-20261101-user001-v1');
 RETURN NOT EXISTS(SELECT 1 FROM card_locks_v1 l WHERE l.card_id=$1 AND l.source='USER_SELF' AND l.state='RELEASED'
 AND NOT EXISTS(SELECT 1 FROM demo_card_releases45_v1 r WHERE r.lock_id=l.lock_id));
END $$;
REVOKE ALL ON FUNCTION card_fresh_self_after_release45_v1(TEXT) FROM PUBLIC;
