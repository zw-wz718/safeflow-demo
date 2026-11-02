-- Local in-app simulation only. Reservation identity is allocated by the host
-- and saved with the member ReserveScan transaction, never synthesized by UI.
CREATE TABLE subscription_reminder_attempts_v1 (
 attempt_id uuid PRIMARY KEY,
 owner_id text NOT NULL,
 source_position bigint NOT NULL CHECK(source_position >= 0),
 ordinal integer NOT NULL CHECK(ordinal >= 0),
 original_request jsonb NOT NULL,
 message jsonb NOT NULL,
 binding jsonb NOT NULL,
 reserved_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 UNIQUE(owner_id,source_position,ordinal)
);
CREATE TABLE subscription_simulated_notifications_v1 (
 attempt_id uuid PRIMARY KEY REFERENCES subscription_reminder_attempts_v1(attempt_id),
 message_sha256 text NOT NULL,
 receipt jsonb NOT NULL,
 persisted_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE FUNCTION subscription_simulation_immutable_v1() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'SIMULATED_REMINDER_FACT_IMMUTABLE'; END $$;
CREATE TRIGGER reminder_attempt_immutable BEFORE UPDATE OR DELETE OR TRUNCATE ON subscription_reminder_attempts_v1
 FOR EACH STATEMENT EXECUTE FUNCTION subscription_simulation_immutable_v1();
CREATE TRIGGER simulated_notification_immutable BEFORE UPDATE OR DELETE OR TRUNCATE ON subscription_simulated_notifications_v1
 FOR EACH STATEMENT EXECUTE FUNCTION subscription_simulation_immutable_v1();
REVOKE ALL ON subscription_reminder_attempts_v1,subscription_simulated_notifications_v1 FROM PUBLIC;
REVOKE ALL ON FUNCTION subscription_simulation_immutable_v1() FROM PUBLIC;
