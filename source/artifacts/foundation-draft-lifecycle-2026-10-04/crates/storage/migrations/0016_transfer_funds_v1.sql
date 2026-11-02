-- TRANSFER-OWNED PROPOSAL, not an installed/frozen public migration.
-- Apply after business_orders_v1.proposed.sql in an isolated database only.
CREATE TABLE IF NOT EXISTS transfer_drafts_v1 (
    owner_id TEXT NOT NULL,
    draft_id TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    business_version BIGINT NOT NULL CHECK (business_version BETWEEN 1 AND 4294967295),
    expires_at BIGINT NOT NULL CHECK (expires_at > 0),
    status TEXT NOT NULL CHECK (status IN ('READY','NEEDS_INFORMATION','EXPIRED','CANCELLED')),
    details JSONB CHECK (details IS NULL OR jsonb_typeof(details) = 'object'),
    submitted BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (owner_id,draft_id),
    UNIQUE (owner_id,operation_id)
);

CREATE TABLE IF NOT EXISTS transfer_accounts_v1 (
    account_id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    opening_cents BIGINT NOT NULL CHECK (opening_cents >= 0),
    booked_cents BIGINT NOT NULL CHECK (booked_cents >= 0)
);
CREATE TABLE IF NOT EXISTS transfer_payee_routes_v1 (
    owner_id TEXT NOT NULL,
    payee_id TEXT NOT NULL,
    recipient_account_id TEXT NOT NULL REFERENCES transfer_accounts_v1(account_id),
    PRIMARY KEY(owner_id,payee_id)
);
CREATE TABLE IF NOT EXISTS transfer_fund_orders_v1 (
    owner_id TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    payer_account_id TEXT NOT NULL REFERENCES transfer_accounts_v1(account_id),
    recipient_account_id TEXT NOT NULL REFERENCES transfer_accounts_v1(account_id),
    amount_cents BIGINT NOT NULL CHECK(amount_cents > 0),
    state TEXT NOT NULL CHECK(state IN ('PROCESSING','SUCCEEDED','FAILED')),
    PRIMARY KEY(owner_id,operation_id),
    FOREIGN KEY(owner_id,operation_id) REFERENCES transfer_business_orders_v1(owner_id,operation_id),
    CHECK(payer_account_id <> recipient_account_id)
);
CREATE INDEX IF NOT EXISTS transfer_pending_funds_v1 ON transfer_fund_orders_v1(payer_account_id) WHERE state='PROCESSING';
CREATE TABLE IF NOT EXISTS transfer_ledger_v1 (
    transaction_id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    business_owner_id TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    account_id TEXT NOT NULL REFERENCES transfer_accounts_v1(account_id),
    direction TEXT NOT NULL CHECK(direction IN ('DEBIT','CREDIT')),
    amount_cents BIGINT NOT NULL CHECK(amount_cents > 0),
    completed_at TEXT NOT NULL,
    FOREIGN KEY(business_owner_id,operation_id) REFERENCES transfer_fund_orders_v1(owner_id,operation_id),
    UNIQUE(business_owner_id,operation_id,direction)
);
CREATE OR REPLACE FUNCTION transfer_ledger_v1_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'TRANSFER_LEDGER_IMMUTABLE'; END;
$$;
DROP TRIGGER IF EXISTS transfer_ledger_v1_no_rewrite ON transfer_ledger_v1;
CREATE TRIGGER transfer_ledger_v1_no_rewrite BEFORE UPDATE OR DELETE ON transfer_ledger_v1
FOR EACH ROW EXECUTE FUNCTION transfer_ledger_v1_immutable();
