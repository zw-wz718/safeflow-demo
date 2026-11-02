-- Additive fictional recipient display version. No account, source or old snapshot update.
CREATE TABLE public_recipient_profile_versions_v1 (
 revision TEXT PRIMARY KEY,
 artifact_sha256 TEXT NOT NULL,
 original_bytes BYTEA NOT NULL CHECK(octet_length(original_bytes) <= 4096),
 adopted_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE FUNCTION public_recipient_profile_versions_v1_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'RECIPIENT_PROFILE_IMMUTABLE'; END;
$$;
CREATE TRIGGER public_recipient_profile_versions_v1_no_rewrite BEFORE UPDATE OR DELETE
 ON public_recipient_profile_versions_v1 FOR EACH ROW EXECUTE FUNCTION public_recipient_profile_versions_v1_immutable();
