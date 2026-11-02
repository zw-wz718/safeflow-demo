-- CARD-OWNED SECOND PROPOSAL, after card_catalog_v1.proposed.sql.
-- Echo reviews/installs the unique public migration; no number allocated here.
-- Business history is separate from public operations/permit/outbox/audit.
ALTER TABLE card_catalog_v1 ADD CONSTRAINT card_catalog_account_identity_v1 UNIQUE(card_id,account_id);
ALTER TABLE card_locks_v1 ADD CONSTRAINT card_lock_source_identity_v1 UNIQUE(lock_id,card_id,source);
ALTER TABLE card_locks_v1 ADD CONSTRAINT card_lock_event_identity_v1 UNIQUE(lock_id,card_id,source,event_scope,event_id);

CREATE TABLE card_freeze_operations_v1 (
    record_id TEXT PRIMARY KEY CHECK(btrim(record_id)<>''),
    owner_id TEXT NOT NULL CHECK(btrim(owner_id)<>''),
    account_id TEXT NOT NULL REFERENCES card_account_versions_v1(account_id),
    operation_id TEXT NOT NULL CHECK(btrim(operation_id)<>''),
    card_id TEXT NOT NULL,
    lock_id TEXT NOT NULL,
    source TEXT NOT NULL CHECK(source IN ('USER_SELF','ANOMALY_DISPUTE','RISK_CONTROL')),
    reason TEXT NOT NULL,
    event_scope TEXT,
    event_id TEXT,
    transaction_id TEXT,
    feedback_id TEXT,
    original_key TEXT NOT NULL CHECK(btrim(original_key)<>''),
    UNIQUE(owner_id,operation_id),
    UNIQUE(owner_id,original_key),
    UNIQUE(record_id,lock_id,reason),
    FOREIGN KEY(card_id,account_id) REFERENCES card_catalog_v1(card_id,account_id),
    FOREIGN KEY(lock_id,card_id,source) REFERENCES card_locks_v1(lock_id,card_id,source),
    FOREIGN KEY(lock_id,card_id,source,event_scope,event_id)
        REFERENCES card_locks_v1(lock_id,card_id,source,event_scope,event_id),
    CHECK (
        (source='USER_SELF' AND reason IN ('USER_REPORT_LOST','USER_SECURITY_CONCERN')
            AND event_scope IS NULL AND event_id IS NULL AND transaction_id IS NULL AND feedback_id IS NULL)
        OR (source='ANOMALY_DISPUTE' AND reason='NON_OWNER_TRANSACTION'
            AND event_scope IS NOT NULL AND btrim(event_scope)<>''
            AND event_id IS NOT NULL AND btrim(event_id)<>''
            AND transaction_id IS NOT NULL AND btrim(transaction_id)<>''
            AND feedback_id IS NOT NULL AND btrim(feedback_id)<>'')
        OR (source='RISK_CONTROL' AND reason='RISK_CONTROL_FLAG'
            AND event_scope IS NOT NULL AND btrim(event_scope)<>''
            AND event_id IS NOT NULL AND btrim(event_id)<>''
            AND transaction_id IS NULL AND feedback_id IS NULL)
    )
);
CREATE TABLE card_freeze_reasons_v1 (
    reason_history_id TEXT PRIMARY KEY CHECK(btrim(reason_history_id)<>''),
    lock_id TEXT NOT NULL,
    operation_record_id TEXT NOT NULL UNIQUE,
    reason TEXT NOT NULL,
    UNIQUE(operation_record_id,lock_id,reason_history_id),
    FOREIGN KEY(operation_record_id,lock_id,reason)
        REFERENCES card_freeze_operations_v1(record_id,lock_id,reason)
);
CREATE TABLE card_freeze_receipts_v1 (
    operation_record_id TEXT PRIMARY KEY,
    lock_id TEXT NOT NULL,
    reason_history_id TEXT NOT NULL UNIQUE,
    original_effect TEXT NOT NULL CHECK(original_effect IN ('CREATED','REUSED_ACTIVE')),
    FOREIGN KEY(operation_record_id,lock_id,reason_history_id)
        REFERENCES card_freeze_reasons_v1(operation_record_id,lock_id,reason_history_id)
);

-- Whole backend write uses the public lock order and retained account lock.
-- This guard checks mapping and advances the card version for a new reason/
-- operation even when the active lock is reused. Transaction ID is NOT auth.
CREATE FUNCTION card_freeze_insert_guard_v1() RETURNS trigger
LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
    PERFORM account_id FROM transfer_accounts_v1
        WHERE account_id=NEW.account_id AND owner_id=NEW.owner_id FOR UPDATE;
    IF NOT FOUND THEN RAISE EXCEPTION 'CARD_FREEZE_OWNER_MISMATCH'; END IF;
    UPDATE card_account_versions_v1 SET revision=revision+1 WHERE account_id=NEW.account_id;
    IF NOT FOUND THEN RAISE EXCEPTION 'CARD_CATALOG_UNINITIALIZED'; END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER card_freeze_insert_guard_v1 BEFORE INSERT ON card_freeze_operations_v1
FOR EACH ROW EXECUTE FUNCTION card_freeze_insert_guard_v1();

-- Append-only business history. A retry reads, never rewrites the first result.
CREATE FUNCTION card_freeze_history_immutable_v1() RETURNS trigger
LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
    RAISE EXCEPTION 'CARD_FREEZE_HISTORY_IMMUTABLE';
END; $$;
CREATE TRIGGER card_freeze_operation_immutable_v1 BEFORE UPDATE OR DELETE ON card_freeze_operations_v1
FOR EACH ROW EXECUTE FUNCTION card_freeze_history_immutable_v1();
CREATE TRIGGER card_freeze_reason_immutable_v1 BEFORE UPDATE OR DELETE ON card_freeze_reasons_v1
FOR EACH ROW EXECUTE FUNCTION card_freeze_history_immutable_v1();
CREATE TRIGGER card_freeze_receipt_immutable_v1 BEFORE UPDATE OR DELETE ON card_freeze_receipts_v1
FOR EACH ROW EXECUTE FUNCTION card_freeze_history_immutable_v1();

-- No operation may commit without its matching reason and original receipt.
-- Deferred until transaction end so an operation and result are saved together.
CREATE FUNCTION card_freeze_complete_v1() RETURNS trigger
LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM card_freeze_reasons_v1 r JOIN card_freeze_receipts_v1 p
            ON p.operation_record_id=r.operation_record_id AND p.lock_id=r.lock_id
                AND p.reason_history_id=r.reason_history_id
        WHERE r.operation_record_id=NEW.record_id AND r.lock_id=NEW.lock_id AND r.reason=NEW.reason
    ) THEN RAISE EXCEPTION 'CARD_FREEZE_ORIGINAL_RESULT_INCOMPLETE'; END IF;
    RETURN NEW;
END; $$;
CREATE CONSTRAINT TRIGGER card_freeze_complete_v1 AFTER INSERT ON card_freeze_operations_v1
DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION card_freeze_complete_v1();

-- Single-statement atomic persistence. Sequential statements inside the function
-- keep version triggers ordered, avoiding multiple CTEs updating one version row.
-- Invoker privileges only; this function is NOT an authorization/permit gate.
CREATE FUNCTION card_save_freeze_v1(
    p_lock TEXT,p_card TEXT,p_source TEXT,p_scope TEXT,p_event TEXT,p_create BOOLEAN,
    p_record TEXT,p_owner TEXT,p_account TEXT,p_operation TEXT,p_reason TEXT,
    p_transaction TEXT,p_feedback TEXT,p_key TEXT,p_history TEXT,p_effect TEXT
) RETURNS BIGINT LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
    PERFORM account_id FROM transfer_accounts_v1
        WHERE account_id=p_account AND owner_id=p_owner FOR UPDATE;
    IF NOT FOUND THEN RAISE EXCEPTION 'CARD_FREEZE_OWNER_MISMATCH'; END IF;
    PERFORM card_id FROM card_catalog_v1 WHERE card_id=p_card AND account_id=p_account;
    IF NOT FOUND THEN RAISE EXCEPTION 'CARD_FREEZE_CARD_MISMATCH'; END IF;
    IF p_create IS NULL OR p_effect IS DISTINCT FROM
        (CASE WHEN p_create THEN 'CREATED' ELSE 'REUSED_ACTIVE' END)
    THEN RAISE EXCEPTION 'CARD_FREEZE_EFFECT_MISMATCH'; END IF;
    IF p_create THEN
        IF EXISTS (SELECT 1 FROM card_locks_v1 WHERE card_id=p_card AND source=p_source
            AND event_scope IS NOT DISTINCT FROM p_scope AND event_id IS NOT DISTINCT FROM p_event
            AND state='RELEASED')
        THEN RAISE EXCEPTION 'CARD_FREEZE_REOPEN_REQUIRES_COORDINATION'; END IF;
        INSERT INTO card_locks_v1(lock_id,card_id,source,state,event_scope,event_id)
            VALUES(p_lock,p_card,p_source,'ACTIVE',p_scope,p_event);
    ELSE
        PERFORM lock_id FROM card_locks_v1 WHERE lock_id=p_lock AND card_id=p_card
            AND source=p_source AND state='ACTIVE'
            AND event_scope IS NOT DISTINCT FROM p_scope AND event_id IS NOT DISTINCT FROM p_event;
        IF NOT FOUND THEN RAISE EXCEPTION 'CARD_FREEZE_REUSED_LOCK_MISMATCH'; END IF;
    END IF;
    INSERT INTO card_freeze_operations_v1(record_id,owner_id,account_id,operation_id,card_id,
        lock_id,source,reason,event_scope,event_id,transaction_id,feedback_id,original_key)
        VALUES(p_record,p_owner,p_account,p_operation,p_card,p_lock,p_source,p_reason,
            p_scope,p_event,p_transaction,p_feedback,p_key);
    INSERT INTO card_freeze_reasons_v1(reason_history_id,lock_id,operation_record_id,reason)
        VALUES(p_history,p_lock,p_record,p_reason);
    INSERT INTO card_freeze_receipts_v1(operation_record_id,lock_id,reason_history_id,original_effect)
        VALUES(p_record,p_lock,p_history,p_effect);
    RETURN 1;
END; $$;
-- Echo grants only the approved backend role after reviewing public wiring.
REVOKE ALL ON FUNCTION card_save_freeze_v1(TEXT,TEXT,TEXT,TEXT,TEXT,BOOLEAN,
    TEXT,TEXT,TEXT,TEXT,TEXT,TEXT,TEXT,TEXT,TEXT,TEXT) FROM PUBLIC;
