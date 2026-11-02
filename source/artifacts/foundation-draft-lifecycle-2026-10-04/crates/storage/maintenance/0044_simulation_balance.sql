-- Native reviewed maintenance only. Original opening balances, source bytes,
-- transfer ledger and maintenance43 remain immutable. Not a payment API.
CREATE TABLE simulation_balance_adjustments_v1 (
 adjustment_id TEXT PRIMARY KEY CHECK(adjustment_id='sf-million-20261031-user001-v1'),
 owner_id TEXT NOT NULL CHECK(owner_id='user_001'),
 account_id TEXT NOT NULL REFERENCES transfer_accounts_v1(account_id) CHECK(account_id='account_001'),
 before_cents BIGINT NOT NULL CHECK(before_cents>=0),
 target_cents BIGINT NOT NULL CHECK(target_cents=100000000),
 delta_cents BIGINT NOT NULL CHECK(delta_cents=target_cents-before_cents),
 expected_digest TEXT NOT NULL CHECK(expected_digest ~ '^[0-9a-f]{64}$'),
 plan JSONB NOT NULL,
 task_id TEXT NOT NULL UNIQUE REFERENCES tasks(task_id),
 event_id TEXT NOT NULL UNIQUE REFERENCES audit_events(event_id),
 receipt JSONB NOT NULL,
 recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
 CHECK(receipt->>'schema'='SIMULATION_BALANCE_ADJUSTED_V1'
 AND receipt->>'adjustment_id'=adjustment_id
 AND receipt->>'owner_id'=owner_id AND receipt->>'account_id'=account_id
 AND (receipt->>'before_cents')::bigint=before_cents
 AND (receipt->>'target_cents')::bigint=target_cents
 AND (receipt->>'delta_cents')::bigint=delta_cents
 AND receipt->>'expected_digest'=expected_digest)
);
CREATE TRIGGER simulation_balance_adjustment_immutable BEFORE UPDATE OR DELETE ON simulation_balance_adjustments_v1
 FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
REVOKE ALL ON TABLE simulation_balance_adjustments_v1 FROM PUBLIC;

-- This ledger is part of the REAL balance equation, never a display overlay.
-- Fail closed on an incomplete/broken receipt or missing transactional outbox.
CREATE FUNCTION simulation_balance_net_v1(account TEXT) RETURNS NUMERIC
 LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
BEGIN
 IF EXISTS(SELECT 1 FROM simulation_balance_adjustments_v1 s
 LEFT JOIN tasks t ON t.task_id=s.task_id
 LEFT JOIN audit_events e ON e.event_id=s.event_id
 LEFT JOIN public_audit_outbox o ON o.event_id=e.event_id
 WHERE s.account_id=$1 AND (t.owner_id IS DISTINCT FROM s.owner_id
 OR t.state IS DISTINCT FROM 'COMPLETED' OR e.task_id IS DISTINCT FROM s.task_id
 OR e.kind IS DISTINCT FROM 'CHECKPOINT_SAVED' OR e.operation_id IS NOT NULL
 OR e.details IS DISTINCT FROM s.receipt OR o.event_id IS NULL
 OR o.envelope->'public_event'->'details' IS DISTINCT FROM s.receipt)) THEN
 RAISE EXCEPTION 'SIMULATION_ADJUSTMENT_EVIDENCE_INVALID'; END IF;
 RETURN (SELECT coalesce(sum(delta_cents::numeric),0) FROM simulation_balance_adjustments_v1 WHERE account_id=$1);
END $$;
REVOKE ALL ON FUNCTION simulation_balance_net_v1(TEXT) FROM PUBLIC;

CREATE TABLE public_card_final_forward44_v1 (
 deployment_id UUID PRIMARY KEY REFERENCES public_card_final_deployment_v1(deployment_id),
 original_digest TEXT NOT NULL, from_digest TEXT NOT NULL, to_digest TEXT NOT NULL,
 predecessor_checksum BYTEA NOT NULL, migration_checksum BYTEA NOT NULL,
 upgrade_plan JSONB NOT NULL,
 recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER public_card_final_forward44_immutable BEFORE UPDATE OR DELETE ON public_card_final_forward44_v1
 FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
REVOKE ALL ON TABLE public_card_final_forward44_v1 FROM PUBLIC;

-- Bounded startup assertion for registered components, including the producer
-- which has no account/table-reading rights. Return only the installed checksum.
CREATE FUNCTION simulation_balance_revision44_v1() RETURNS BYTEA
 LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
BEGIN
 IF (SELECT max(version) FROM _sqlx_migrations) IS DISTINCT FROM 44
 OR NOT EXISTS(SELECT 1 FROM public_card_final_deployment_v1 d
 JOIN public_card_final_forward44_v1 f USING(deployment_id)
 WHERE public_card_final_expected_digest_v1(d.deployment_id,d.object_digest)=public_card_final_objects_v1()) THEN
 RAISE EXCEPTION 'SIMULATION_BALANCE_REVISION_INVALID'; END IF;
 PERFORM simulation_balance_net_v1('account_001');
 RETURN (SELECT checksum FROM _sqlx_migrations WHERE version=44 AND success);
END $$;
REVOKE ALL ON FUNCTION simulation_balance_revision44_v1() FROM PUBLIC;

-- All late writers meet the same task lock as the terminal checkpoint. A
-- durable execution claim/funds acceptance can never race past the checkpoint.
-- This separate fence row is updated even though the original task is kept
-- byte-for-byte. RR writers with an older snapshot must serialize/retry; an
-- ordinary SELECT after FOR UPDATE alone would still see their old snapshot.
CREATE TABLE draft_intent_fences44_v1 (
 task_id TEXT PRIMARY KEY REFERENCES tasks(task_id),
 writes BIGINT NOT NULL DEFAULT 0 CHECK(writes>=0),
 ended BOOLEAN NOT NULL DEFAULT false
);
INSERT INTO draft_intent_fences44_v1(task_id,ended)
 SELECT task_id,true FROM audit_events WHERE kind='CHECKPOINT_SAVED'
 AND details->>'schema' IN('UNEXECUTED_DRAFT_TERMINATED_V1','NONFUNDING_UNKNOWN_DRAFT_CLOSED_V1') GROUP BY task_id;
REVOKE ALL ON TABLE draft_intent_fences44_v1 FROM PUBLIC;
CREATE FUNCTION draft_intent_end_fence44_v1() RETURNS trigger
 LANGUAGE plpgsql SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
BEGIN
 IF NEW.kind='CHECKPOINT_SAVED' AND NEW.details->>'schema' IN('UNEXECUTED_DRAFT_TERMINATED_V1','NONFUNDING_UNKNOWN_DRAFT_CLOSED_V1') THEN
  INSERT INTO draft_intent_fences44_v1(task_id,ended) VALUES(NEW.task_id,true)
  ON CONFLICT(task_id) DO UPDATE SET ended=true;
 END IF;
 RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION draft_intent_end_fence44_v1() FROM PUBLIC;
CREATE TRIGGER draft_intent_terminal_fence BEFORE INSERT ON audit_events
 FOR EACH ROW EXECUTE FUNCTION draft_intent_end_fence44_v1();
CREATE FUNCTION draft_intent_write_guard_v1() RETURNS trigger
 LANGUAGE plpgsql SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
DECLARE stopped BOOLEAN;
BEGIN
 PERFORM task_id FROM tasks WHERE task_id=NEW.task_id FOR UPDATE;
 INSERT INTO draft_intent_fences44_v1 AS f(task_id,writes) VALUES(NEW.task_id,1)
 ON CONFLICT(task_id) DO UPDATE SET writes=f.writes+1 WHERE NOT f.ended RETURNING ended INTO stopped;
 IF NOT FOUND OR stopped OR EXISTS(SELECT 1 FROM audit_events WHERE task_id=NEW.task_id AND kind='CHECKPOINT_SAVED'
 AND details->>'schema' IN('UNEXECUTED_DRAFT_TERMINATED_V1','NONFUNDING_UNKNOWN_DRAFT_CLOSED_V1')) THEN
 RAISE EXCEPTION 'DRAFT_INTENT_TERMINATED_OR_NONFUNDING_CLOSED'; END IF;
 RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION draft_intent_write_guard_v1() FROM PUBLIC;
CREATE TRIGGER draft_intent_claim_guard BEFORE INSERT OR UPDATE ON mock_draft_lifecycle_claims
 FOR EACH ROW EXECUTE FUNCTION draft_intent_write_guard_v1();
CREATE TRIGGER draft_intent_action_guard BEFORE INSERT OR UPDATE ON mock_draft_lifecycle_actions
 FOR EACH ROW EXECUTE FUNCTION draft_intent_write_guard_v1();
CREATE TRIGGER draft_intent_confirmation_guard BEFORE INSERT OR UPDATE ON server_confirmations
 FOR EACH ROW EXECUTE FUNCTION draft_intent_write_guard_v1();
CREATE TRIGGER draft_intent_preparation_guard BEFORE INSERT OR UPDATE ON payment_preparations
 FOR EACH ROW EXECUTE FUNCTION draft_intent_write_guard_v1();
CREATE TRIGGER draft_intent_request_guard BEFORE INSERT OR UPDATE ON public_execution_requests
 FOR EACH ROW EXECUTE FUNCTION draft_intent_write_guard_v1();
CREATE TRIGGER draft_intent_transfer_guard BEFORE INSERT OR UPDATE ON transfer_drafts_v1
 FOR EACH ROW EXECUTE FUNCTION draft_intent_write_guard_v1();
CREATE TRIGGER draft_intent_permit_guard BEFORE INSERT OR UPDATE ON execution_permits
 FOR EACH ROW EXECUTE FUNCTION draft_intent_write_guard_v1();

-- Preserve migration36 bundle verification; extend only the actual accounting equation.
CREATE OR REPLACE FUNCTION payment_completion_bundle_v1(gid TEXT) RETURNS JSONB
 LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp SET timezone TO 'UTC' AS $$
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
 (aa.booked_cents::numeric<>aa.opening_cents::numeric+simulation_balance_net_v1(aa.account_id)+coalesce((SELECT sum(CASE WHEN direction='DEBIT' THEN -amount_cents::numeric ELSE amount_cents::numeric END) FROM transfer_ledger_v1 WHERE account_id=aa.account_id),0)
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
