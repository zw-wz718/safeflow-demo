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
