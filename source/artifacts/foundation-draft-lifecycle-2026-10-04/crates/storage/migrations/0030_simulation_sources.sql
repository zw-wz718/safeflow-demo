-- Echo-owned local simulation storage; these facts never grant account access.
CREATE TABLE simulation_source_versions_v1 (
 source_id text NOT NULL, owner_id text NOT NULL, revision text NOT NULL,
 spec jsonb NOT NULL, digest text NOT NULL, initial jsonb,
 PRIMARY KEY(source_id,owner_id,revision)
);
CREATE TABLE simulation_source_rows_v1 (
 source_id text NOT NULL, owner_id text NOT NULL, revision text NOT NULL,
 account_id text NOT NULL, transaction_id text NOT NULL,
 completed_at timestamptz, body jsonb NOT NULL, digest text NOT NULL,
 PRIMARY KEY(source_id,owner_id,revision,account_id,transaction_id),
 FOREIGN KEY(source_id,owner_id,revision) REFERENCES simulation_source_versions_v1
);
CREATE INDEX simulation_source_window_v1 ON simulation_source_rows_v1(source_id,owner_id,revision,completed_at,account_id,transaction_id);
CREATE TABLE simulation_source_heads_v1 (
 source_id text NOT NULL, owner_id text NOT NULL, revision text NOT NULL,
 generation bigint NOT NULL CHECK(generation>0), active boolean NOT NULL,
 PRIMARY KEY(source_id,owner_id),
 FOREIGN KEY(source_id,owner_id,revision) REFERENCES simulation_source_versions_v1
);
CREATE FUNCTION simulation_source_immutable_v1() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'immutable simulation source version'; END $$;
CREATE TRIGGER simulation_source_version_immutable BEFORE UPDATE OR DELETE ON simulation_source_versions_v1 FOR EACH ROW EXECUTE FUNCTION simulation_source_immutable_v1();
CREATE TRIGGER simulation_source_row_immutable BEFORE UPDATE OR DELETE ON simulation_source_rows_v1 FOR EACH ROW EXECUTE FUNCTION simulation_source_immutable_v1();
-- Rows can only be attached in the transaction which creates their version.
CREATE FUNCTION simulation_source_append_v1() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NOT EXISTS(SELECT 1 FROM simulation_source_versions_v1 v
 WHERE v.source_id=NEW.source_id AND v.owner_id=NEW.owner_id AND v.revision=NEW.revision
 AND v.xmin::text=pg_current_xact_id()::text) THEN
 RAISE EXCEPTION 'sealed simulation source version'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER simulation_source_sealed BEFORE INSERT ON simulation_source_rows_v1 FOR EACH ROW EXECUTE FUNCTION simulation_source_append_v1();
