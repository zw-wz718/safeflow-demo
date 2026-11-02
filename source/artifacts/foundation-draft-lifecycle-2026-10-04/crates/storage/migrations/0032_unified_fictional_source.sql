-- Public technical provenance only. Original member tables/checksums unchanged.
CREATE TABLE public_unified_fictional_source_v1 (
    singleton SMALLINT PRIMARY KEY CHECK (singleton = 1),
    source_id TEXT NOT NULL,
    source_revision TEXT NOT NULL,
    artifact_commit TEXT NOT NULL,
    artifact_sha256 TEXT NOT NULL,
    implementation_revision TEXT NOT NULL,
    original_bytes BYTEA NOT NULL CHECK (octet_length(original_bytes) <= 16384),
    active BOOLEAN NOT NULL,
    adopted_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE FUNCTION public_unified_fictional_source_v1_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND OLD.active AND NOT NEW.active
        AND (to_jsonb(OLD) - 'active') = (to_jsonb(NEW) - 'active') THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION 'UNIFIED_FICTIONAL_SOURCE_IMMUTABLE';
END;
$$;
CREATE TRIGGER public_unified_fictional_source_v1_no_rewrite
BEFORE UPDATE OR DELETE ON public_unified_fictional_source_v1
FOR EACH ROW EXECUTE FUNCTION public_unified_fictional_source_v1_immutable();
