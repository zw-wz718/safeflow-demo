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

-- Public-only archive of REAL source calls, committed with the original read receipt.
CREATE TABLE public_billing_reports_v1 (
    owner_id text NOT NULL,
    report_id text NOT NULL,
    task_id text NOT NULL REFERENCES tasks(task_id),
    read_operation_id text NOT NULL,
    result_ref text NOT NULL,
    claim_id text NOT NULL,
    claim_kind text NOT NULL CHECK (claim_kind IN ('ROOT','CONTINUATION')),
    receipt_digest text NOT NULL,
    result_digest text NOT NULL,
    archive jsonb NOT NULL CHECK (jsonb_typeof(archive)='object' AND octet_length(archive::text)<=8388608),
    archive_digest text NOT NULL,
    PRIMARY KEY(owner_id, report_id),
    UNIQUE(task_id, read_operation_id)
);
CREATE TRIGGER public_billing_report_immutable_v1 BEFORE UPDATE OR DELETE ON public_billing_reports_v1
FOR EACH ROW EXECUTE FUNCTION billing_reject_evidence_change_v1();

CREATE TABLE public_billing_feedback_results_v1 (
    operation_id text PRIMARY KEY REFERENCES operations(operation_id),
    owner_id text NOT NULL,
    report_id text NOT NULL,
    request_digest text NOT NULL,
    staged jsonb NOT NULL,
    confirmation_ref text NOT NULL,
    session_digest text NOT NULL,
    binding_version bigint NOT NULL CHECK (binding_version>0),
    FOREIGN KEY(owner_id, report_id) REFERENCES public_billing_reports_v1(owner_id, report_id),
    FOREIGN KEY(operation_id) REFERENCES billing_feedback_v1(public_operation_id)
);
CREATE TRIGGER public_billing_feedback_result_immutable_v1 BEFORE UPDATE OR DELETE ON public_billing_feedback_results_v1
FOR EACH ROW EXECUTE FUNCTION billing_reject_evidence_change_v1();
CREATE TRIGGER billing_feedback_immutable_v1 BEFORE UPDATE OR DELETE ON billing_feedback_v1
FOR EACH ROW EXECUTE FUNCTION billing_reject_evidence_change_v1();
CREATE TRIGGER billing_event_immutable_v1 BEFORE UPDATE OR DELETE ON billing_feedback_events_v1
FOR EACH ROW EXECUTE FUNCTION billing_reject_evidence_change_v1();
