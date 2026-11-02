-- Observations are not security decisions, registered execution requests or Permits.
CREATE TABLE payment_business_observations (
    observation_id TEXT PRIMARY KEY,
    operation_id TEXT NOT NULL REFERENCES payment_preparations(operation_id),
    ordinal BIGINT NOT NULL CHECK(ordinal>0),
    observation JSONB NOT NULL CHECK(jsonb_typeof(observation)='object'),
    observation_digest TEXT NOT NULL CHECK(observation_digest ~ '^[0-9a-f]{64}$'),
    UNIQUE(operation_id,ordinal)
);
CREATE FUNCTION payment_business_observation_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'PAYMENT_BUSINESS_OBSERVATION_IMMUTABLE';
END $$;
CREATE TRIGGER payment_business_observation_no_rewrite BEFORE UPDATE OR DELETE ON payment_business_observations
FOR EACH ROW EXECUTE FUNCTION payment_business_observation_immutable();
