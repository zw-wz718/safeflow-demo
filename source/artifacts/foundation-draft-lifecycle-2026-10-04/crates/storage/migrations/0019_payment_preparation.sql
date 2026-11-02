-- Same-task preparation branch, pinned to an existing public plan revision.
-- This is not installed as an executable task plan or a funds Tool registration.
CREATE TABLE payment_preparations (
    operation_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    request JSONB NOT NULL CHECK (jsonb_typeof(request)='object'),
    request_ticket TEXT NOT NULL,
    task_revision BIGINT NOT NULL CHECK(task_revision>0),
    draft_id TEXT NOT NULL,
    draft_version BIGINT NOT NULL CHECK(draft_version>0),
    snapshot JSONB NOT NULL CHECK(jsonb_typeof(snapshot)='object'),
    snapshot_digest TEXT NOT NULL CHECK(snapshot_digest ~ '^[0-9a-f]{64}$'),
    base_plan_digest TEXT NOT NULL CHECK(base_plan_digest ~ '^[0-9a-f]{64}$'),
    preparation_plan JSONB NOT NULL CHECK(jsonb_typeof(preparation_plan)='object'),
    display_ticket TEXT NOT NULL UNIQUE,
    confirmation_ref TEXT NOT NULL UNIQUE REFERENCES server_confirmations(confirmation_ref),
    invalid_reason TEXT CHECK(invalid_reason IN ('REJECTED','SOURCE_CHANGED','SESSION_CHANGED','EXPIRED')),
    FOREIGN KEY(task_id,operation_id) REFERENCES operations(task_id,operation_id),
    UNIQUE(task_id,request_ticket),
    UNIQUE(task_id,task_revision,draft_id,draft_version)
);
CREATE FUNCTION payment_preparation_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='DELETE' OR (to_jsonb(NEW)-'invalid_reason') IS DISTINCT FROM (to_jsonb(OLD)-'invalid_reason')
       OR (OLD.invalid_reason IS NOT NULL AND NEW.invalid_reason IS DISTINCT FROM OLD.invalid_reason) THEN
        RAISE EXCEPTION 'PAYMENT_PREPARATION_IMMUTABLE';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER payment_preparation_no_rewrite BEFORE UPDATE OR DELETE ON payment_preparations
FOR EACH ROW EXECUTE FUNCTION payment_preparation_immutable();
