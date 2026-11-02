-- Planning provenance only: this table reserves neither a funds key nor a business number.
CREATE TABLE payment_agent_bindings (
    operation_id TEXT PRIMARY KEY REFERENCES payment_preparations(operation_id),
    binding JSONB NOT NULL CHECK(jsonb_typeof(binding)='object'),
    binding_digest TEXT NOT NULL CHECK(binding_digest ~ '^[0-9a-f]{64}$')
);
CREATE FUNCTION payment_agent_binding_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'PAYMENT_AGENT_BINDING_IMMUTABLE';
END $$;
CREATE TRIGGER payment_agent_binding_no_rewrite BEFORE UPDATE OR DELETE ON payment_agent_bindings
FOR EACH ROW EXECUTE FUNCTION payment_agent_binding_immutable();
