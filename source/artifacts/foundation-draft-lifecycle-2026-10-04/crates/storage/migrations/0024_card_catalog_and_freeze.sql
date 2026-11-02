-- Adopted from card PR22 0ae8b578df7418df382d0fdd4097d243d6112a1b. Full proposals follow.
-- CARD-OWNED PROPOSAL. No public migration number; Echo owns installation.
-- Requires the adopted transfer_accounts_v1 in the caller's trusted schema.
-- Apply only through the public migrator, or in a dedicated integration test.
CREATE TABLE card_account_versions_v1 (
    account_id TEXT PRIMARY KEY REFERENCES transfer_accounts_v1(account_id),
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0)
);

CREATE TABLE card_catalog_v1 (
    card_id TEXT PRIMARY KEY CHECK (btrim(card_id) <> ''),
    account_id TEXT NOT NULL REFERENCES card_account_versions_v1(account_id),
    last_four TEXT NOT NULL CHECK (last_four ~ '^[0-9]{4}$')
);
CREATE INDEX card_catalog_account_v1 ON card_catalog_v1(account_id, card_id);

-- A current/history lock projection, not an authorized freeze write entry.
-- Event scope is supplied by a trusted provider, never guessed from an ID.
CREATE TABLE card_locks_v1 (
    lock_id TEXT PRIMARY KEY CHECK (btrim(lock_id) <> ''),
    card_id TEXT NOT NULL REFERENCES card_catalog_v1(card_id),
    source TEXT NOT NULL CHECK (source IN ('USER_SELF','ANOMALY_DISPUTE','RISK_CONTROL')),
    state TEXT NOT NULL CHECK (state IN ('ACTIVE','RELEASED')),
    event_scope TEXT,
    event_id TEXT,
    CHECK ((source='USER_SELF' AND event_scope IS NULL AND event_id IS NULL)
        OR (source<>'USER_SELF' AND event_scope IS NOT NULL AND btrim(event_scope)<>''
            AND event_id IS NOT NULL AND btrim(event_id)<>''))
);
CREATE INDEX card_locks_card_v1 ON card_locks_v1(card_id, lock_id);
CREATE UNIQUE INDEX card_single_active_self_v1 ON card_locks_v1(card_id)
    WHERE source='USER_SELF' AND state='ACTIVE';
CREATE UNIQUE INDEX card_single_active_event_v1
    ON card_locks_v1(card_id,source,event_scope,event_id)
    WHERE source<>'USER_SELF' AND state='ACTIVE';

-- The account is the common protection object even when there are NO locks.
-- Card moves/deletions, lock identity rewrites and ownership transfer are not
-- enabled by this increment. Fail rather than silently moving history.
CREATE FUNCTION card_catalog_change_v1() RETURNS trigger
LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'CARD_DELETE_NOT_ENABLED'; END IF;
    IF TG_OP='UPDATE' THEN
        IF NEW.card_id IS DISTINCT FROM OLD.card_id OR NEW.account_id IS DISTINCT FROM OLD.account_id
        THEN RAISE EXCEPTION 'CARD_REBIND_NOT_ENABLED'; END IF;
        IF NEW IS NOT DISTINCT FROM OLD THEN RETURN NEW; END IF;
    END IF;
    PERFORM account_id FROM transfer_accounts_v1 WHERE account_id=NEW.account_id FOR UPDATE;
    IF NOT FOUND THEN RAISE EXCEPTION 'CARD_ACCOUNT_MISSING'; END IF;
    UPDATE card_account_versions_v1 SET revision=revision+1 WHERE account_id=NEW.account_id;
    IF NOT FOUND THEN RAISE EXCEPTION 'CARD_CATALOG_UNINITIALIZED'; END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER card_catalog_change_v1 BEFORE INSERT OR UPDATE OR DELETE ON card_catalog_v1
FOR EACH ROW EXECUTE FUNCTION card_catalog_change_v1();

CREATE FUNCTION card_lock_change_v1() RETURNS trigger
LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
DECLARE target_account TEXT;
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'CARD_LOCK_DELETE_NOT_ENABLED'; END IF;
    IF TG_OP='UPDATE' THEN
        IF NEW.lock_id IS DISTINCT FROM OLD.lock_id OR NEW.card_id IS DISTINCT FROM OLD.card_id
            OR NEW.source IS DISTINCT FROM OLD.source OR NEW.event_scope IS DISTINCT FROM OLD.event_scope
            OR NEW.event_id IS DISTINCT FROM OLD.event_id
        THEN RAISE EXCEPTION 'CARD_LOCK_IDENTITY_IMMUTABLE'; END IF;
        IF NEW IS NOT DISTINCT FROM OLD THEN RETURN NEW; END IF;
    END IF;
    SELECT account_id INTO target_account FROM card_catalog_v1 WHERE card_id=NEW.card_id;
    IF NOT FOUND THEN RAISE EXCEPTION 'CARD_REFERENCE_MISSING'; END IF;
    PERFORM account_id FROM transfer_accounts_v1 WHERE account_id=target_account FOR UPDATE;
    IF NOT FOUND THEN RAISE EXCEPTION 'CARD_ACCOUNT_MISSING'; END IF;
    UPDATE card_account_versions_v1 SET revision=revision+1 WHERE account_id=target_account;
    IF NOT FOUND THEN RAISE EXCEPTION 'CARD_CATALOG_UNINITIALIZED'; END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER card_lock_change_v1 BEFORE INSERT OR UPDATE OR DELETE ON card_locks_v1
FOR EACH ROW EXECUTE FUNCTION card_lock_change_v1();

CREATE FUNCTION card_version_no_reset_v1() RETURNS trigger
LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'CARD_VERSION_DELETE_NOT_ENABLED'; END IF;
    IF NEW.account_id IS DISTINCT FROM OLD.account_id OR NEW.revision <= OLD.revision
    THEN RAISE EXCEPTION 'CARD_VERSION_MUST_ADVANCE'; END IF;
    PERFORM account_id FROM transfer_accounts_v1 WHERE account_id=OLD.account_id FOR UPDATE;
    RETURN NEW;
END; $$;
CREATE TRIGGER card_version_no_reset_v1 BEFORE UPDATE OR DELETE ON card_account_versions_v1
FOR EACH ROW EXECUTE FUNCTION card_version_no_reset_v1();

-- Cross-owner changes are explicitly outside the current member protocol.
-- This proposal touches the adopted account table only via this guard trigger;
-- Echo must review/install it with the module DDL, not bypass it in a callback.
CREATE FUNCTION card_owner_change_guard_v1() RETURNS trigger
LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
    IF NEW.owner_id IS DISTINCT FROM OLD.owner_id AND EXISTS (
        SELECT 1 FROM card_account_versions_v1 WHERE account_id=OLD.account_id
    ) THEN RAISE EXCEPTION 'CARD_OWNER_CHANGE_NOT_ENABLED'; END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER card_owner_change_guard_v1 BEFORE UPDATE OF owner_id ON transfer_accounts_v1
FOR EACH ROW EXECUTE FUNCTION card_owner_change_guard_v1();

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


-- Foundation provenance: no source is registered or catalog initialized by DDL.
CREATE TABLE public_card_catalog_adoptions_v1 (
    account_id TEXT NOT NULL REFERENCES card_account_versions_v1(account_id),
    owner_id TEXT NOT NULL,
    ordinal BIGINT NOT NULL CHECK(ordinal > 0),
    source_id TEXT NOT NULL CHECK(btrim(source_id)<>''),
    implementation_revision TEXT NOT NULL CHECK(btrim(implementation_revision)<>''),
    source_revision TEXT NOT NULL CHECK(btrim(source_revision)<>''),
    catalog_digest TEXT NOT NULL CHECK(catalog_digest ~ '^[0-9a-f]{64}$'),
    adopted_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(account_id,ordinal),
    UNIQUE(account_id,source_id,implementation_revision,source_revision)
);
CREATE FUNCTION card_adoption_immutable_v1() RETURNS trigger
LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN RAISE EXCEPTION 'CARD_ADOPTION_IMMUTABLE'; END $$;
CREATE TRIGGER card_adoption_immutable_v1 BEFORE UPDATE OR DELETE ON public_card_catalog_adoptions_v1
FOR EACH ROW EXECUTE FUNCTION card_adoption_immutable_v1();
REVOKE ALL ON TABLE card_account_versions_v1,card_catalog_v1,card_locks_v1,
    card_freeze_operations_v1,card_freeze_reasons_v1,card_freeze_receipts_v1,
    public_card_catalog_adoptions_v1 FROM PUBLIC;
REVOKE ALL ON FUNCTION card_catalog_change_v1(),card_lock_change_v1(),
    card_version_no_reset_v1(),card_owner_change_guard_v1(),card_freeze_insert_guard_v1(),
    card_freeze_history_immutable_v1(),card_freeze_complete_v1(),card_adoption_immutable_v1() FROM PUBLIC;

-- Dedicated confirmed-payment branch; original lifecycle plan/confirmation stay intact.
ALTER TABLE public_execution_requests ADD COLUMN lifecycle_anchor TEXT
    CHECK(lifecycle_anchor IS NULL OR lifecycle_anchor ~ '^[0-9a-f]{64}$');
ALTER TABLE execution_permits ADD COLUMN lifecycle_anchor TEXT
    CHECK(lifecycle_anchor IS NULL OR lifecycle_anchor ~ '^[0-9a-f]{64}$');
CREATE TABLE payment_funds_execution_v1 (
    preparation_operation_id TEXT PRIMARY KEY REFERENCES payment_preparations(operation_id),
    execution_operation_id TEXT NOT NULL UNIQUE REFERENCES operations(operation_id),
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    original_binding JSONB NOT NULL,
    execution_binding JSONB NOT NULL,
    execution_plan JSONB NOT NULL,
    snapshot_digest TEXT NOT NULL CHECK(snapshot_digest ~ '^[0-9a-f]{64}$'),
    association_digest TEXT NOT NULL CHECK(association_digest ~ '^[0-9a-f]{64}$')
);
CREATE TRIGGER payment_funds_execution_immutable BEFORE UPDATE OR DELETE ON payment_funds_execution_v1
FOR EACH ROW EXECUTE FUNCTION card_adoption_immutable_v1();
REVOKE ALL ON TABLE payment_funds_execution_v1 FROM PUBLIC;
