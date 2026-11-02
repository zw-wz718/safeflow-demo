-- Only the fictional c163 member codec is adopted. The member owns replay.
CREATE TABLE subscription_runtime_records (
    owner_id TEXT PRIMARY KEY CHECK (length(btrim(owner_id)) > 0),
    storage_version BIGINT NOT NULL CHECK (storage_version > 0),
    encoding_version TEXT NOT NULL CHECK (encoding_version = 'safeflow.subscription.fictional-journal.v1'),
    journal JSONB NOT NULL,
    CHECK ((jsonb_typeof(journal) = 'object') IS TRUE),
    CHECK ((jsonb_typeof(journal->'initial') = 'object') IS TRUE),
    CHECK ((jsonb_typeof(journal->'commands') = 'array') IS TRUE),
    CHECK ((journal->'schema_version' = '1'::jsonb) IS TRUE),
    CHECK ((journal->>'rule_version' = 'LOCAL_D22_HANNAH_2026_10_12_V1') IS TRUE),
    CHECK ((journal->'fictional' = 'true'::jsonb) IS TRUE),
    CHECK ((journal->'production_registered' = 'false'::jsonb) IS TRUE),
    CHECK ((journal->'initial'->>'owner_user_id' = owner_id) IS TRUE),
    CHECK (storage_version = jsonb_array_length(journal->'commands') + 1)
);
CREATE FUNCTION protect_subscription_journal_v1() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN RAISE EXCEPTION 'subscription deletion forbidden'; END IF;
    IF NEW.owner_id IS DISTINCT FROM OLD.owner_id
       OR NEW.encoding_version IS DISTINCT FROM OLD.encoding_version
       OR (NEW.journal - 'commands') IS DISTINCT FROM (OLD.journal - 'commands')
       OR NEW.storage_version < OLD.storage_version
       OR EXISTS (
           SELECT 1 FROM jsonb_array_elements(OLD.journal->'commands') WITH ORDINALITY AS p(v,n)
           WHERE NEW.journal->'commands'->((p.n-1)::integer) IS DISTINCT FROM p.v
       ) THEN RAISE EXCEPTION 'subscription immutable prefix violation'; END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER subscription_journal_immutable BEFORE UPDATE OR DELETE ON subscription_runtime_records
FOR EACH ROW EXECUTE FUNCTION protect_subscription_journal_v1();

-- Host fencing is separate from the member's domain state machine. No implicit scan.
CREATE TABLE subscription_host_generations_v1 (
    owner_id TEXT PRIMARY KEY REFERENCES subscription_runtime_records(owner_id),
    generation BIGINT NOT NULL CHECK (generation > 0),
    instance_id TEXT NOT NULL CHECK (length(instance_id) = 36)
);
-- Private immutable B -> public execution and exact member original binding.
-- These facts never constitute a confirmation, permit or adopted audit mapping.
CREATE TABLE subscription_public_bindings_v1 (
    business_ref TEXT PRIMARY KEY CHECK (length(business_ref) = 36),
    owner_id TEXT NOT NULL REFERENCES subscription_runtime_records(owner_id),
    operation_id TEXT NOT NULL UNIQUE,
    idempotency_key TEXT NOT NULL,
    session_digest TEXT NOT NULL,
    binding_version BIGINT NOT NULL CHECK (binding_version > 0),
    writer_generation BIGINT NOT NULL CHECK (writer_generation > 0),
    member_binding JSONB NOT NULL CHECK (jsonb_typeof(member_binding) = 'object'),
    original_request JSONB NOT NULL CHECK (jsonb_typeof(original_request) = 'object'),
    public_parameters JSONB NOT NULL CHECK (jsonb_typeof(public_parameters) = 'object'),
    UNIQUE(owner_id,idempotency_key)
);
CREATE FUNCTION protect_subscription_binding_v1() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'subscription original binding is immutable'; END $$;
CREATE TRIGGER subscription_binding_immutable BEFORE UPDATE OR DELETE ON subscription_public_bindings_v1
FOR EACH ROW EXECUTE FUNCTION protect_subscription_binding_v1();

-- Request idempotency is distinct from domain target and execution key.
-- Full input/source/session association and original result are immutable.
CREATE TABLE subscription_preparations_v1 (
    nonce TEXT PRIMARY KEY CHECK (length(nonce)=36),
    request_identity JSONB NOT NULL CHECK (jsonb_typeof(request_identity)='object'),
    business_ref TEXT NOT NULL UNIQUE REFERENCES subscription_public_bindings_v1(business_ref),
    prepared_task JSONB NOT NULL CHECK (jsonb_typeof(prepared_task)='object'),
    prepared_operation JSONB NOT NULL CHECK (jsonb_typeof(prepared_operation)='object'),
    preview JSONB NOT NULL CHECK (jsonb_typeof(preview)='object')
);
CREATE TRIGGER subscription_preparation_immutable BEFORE UPDATE OR DELETE ON subscription_preparations_v1
FOR EACH ROW EXECUTE FUNCTION protect_subscription_binding_v1();
