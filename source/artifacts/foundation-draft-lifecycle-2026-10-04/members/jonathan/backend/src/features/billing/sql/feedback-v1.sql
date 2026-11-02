-- Billing DDL proposal. Echo integrates the ONE common migration chain/schema.
CREATE TABLE billing_report_bindings_v1 (
    owner_user_id text NOT NULL CHECK (octet_length(owner_user_id) BETWEEN 1 AND 200),
    report_id text NOT NULL CHECK (octet_length(report_id) BETWEEN 1 AND 200),
    header jsonb NOT NULL CHECK (jsonb_typeof(header) = 'object'),
    PRIMARY KEY (owner_user_id, report_id)
);
CREATE TABLE billing_feedback_v1 (
    public_operation_id text PRIMARY KEY CHECK (octet_length(public_operation_id) BETWEEN 1 AND 200),
    owner_user_id text NOT NULL,
    report_id text NOT NULL,
    source_id text NOT NULL CHECK (octet_length(source_id) BETWEEN 1 AND 200),
    account_id text NOT NULL CHECK (octet_length(account_id) BETWEEN 1 AND 200),
    transaction_id text NOT NULL CHECK (octet_length(transaction_id) BETWEEN 1 AND 200),
    payload jsonb NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    FOREIGN KEY (owner_user_id, report_id) REFERENCES billing_report_bindings_v1,
    -- Fail closed until D20 adopts versioned correction/revocation semantics.
    UNIQUE (owner_user_id, source_id, account_id, transaction_id)
);
CREATE TABLE billing_feedback_events_v1 (
    event_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    public_operation_id text NOT NULL UNIQUE REFERENCES billing_feedback_v1,
    recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    event_kind text NOT NULL CHECK (event_kind = 'BILLING_FEEDBACK_RECORDED_LOCAL_V1')
);
-- Trusted DB administration/schema is part of the trust boundary.
CREATE FUNCTION billing_reject_evidence_change_v1() RETURNS trigger
LANGUAGE plpgsql AS $$ BEGIN
    RAISE EXCEPTION 'billing evidence is immutable' USING ERRCODE = '55000';
END $$;
CREATE TRIGGER billing_report_immutable_v1 BEFORE UPDATE OR DELETE ON billing_report_bindings_v1
FOR EACH ROW EXECUTE FUNCTION billing_reject_evidence_change_v1();
CREATE TRIGGER billing_feedback_immutable_v1 BEFORE UPDATE OR DELETE ON billing_feedback_v1
FOR EACH ROW EXECUTE FUNCTION billing_reject_evidence_change_v1();
CREATE TRIGGER billing_event_immutable_v1 BEFORE UPDATE OR DELETE ON billing_feedback_events_v1
FOR EACH ROW EXECUTE FUNCTION billing_reject_evidence_change_v1();
