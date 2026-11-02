-- Versioned internal associations do not reserve a public funds request/key.
-- The public owner/business uniqueness and immutable submission remain untouched.
CREATE TABLE payment_execution_links (
    preparation_operation_id TEXT PRIMARY KEY REFERENCES payment_preparations(operation_id),
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    owner_id TEXT NOT NULL,
    registration_key TEXT NOT NULL UNIQUE,
    registration_operation_id TEXT NOT NULL UNIQUE,
    business_operation_ref TEXT NOT NULL,
    business_version BIGINT NOT NULL CHECK(business_version>0),
    confirmation_ref TEXT NOT NULL REFERENCES server_confirmations(confirmation_ref),
    preparation_binding JSONB NOT NULL CHECK(jsonb_typeof(preparation_binding)='object'),
    registration_binding JSONB NOT NULL CHECK(jsonb_typeof(registration_binding)='object'),
    snapshot_digest TEXT NOT NULL CHECK(snapshot_digest ~ '^[0-9a-f]{64}$'),
    link_digest TEXT NOT NULL CHECK(link_digest ~ '^[0-9a-f]{64}$'),
    registered_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    FOREIGN KEY(task_id,registration_operation_id) REFERENCES operations(task_id,operation_id)
);
CREATE INDEX payment_execution_link_business ON payment_execution_links(owner_id,business_operation_ref);
CREATE FUNCTION payment_execution_link_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'PAYMENT_EXECUTION_LINK_IMMUTABLE';
END $$;
CREATE TRIGGER payment_execution_link_no_rewrite BEFORE UPDATE OR DELETE ON payment_execution_links
FOR EACH ROW EXECUTE FUNCTION payment_execution_link_immutable();

CREATE FUNCTION payment_registration_operation_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS(SELECT 1 FROM payment_execution_links WHERE registration_operation_id=OLD.operation_id) THEN
        RAISE EXCEPTION 'PAYMENT_REGISTRATION_IS_NOT_EXECUTION_AUTHORITY';
    END IF;
    IF TG_OP='DELETE' THEN RETURN OLD; END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER payment_registration_operation_no_execution BEFORE UPDATE OR DELETE ON operations
FOR EACH ROW EXECUTE FUNCTION payment_registration_operation_immutable();
