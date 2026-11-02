-- Native maintenance verifies the live v40 predecessor before DDL. The forward
-- record and all new DDL commit together; no deployment/digest/epoch is rewritten.
CREATE TABLE public_card_final_forward42_v1 (
 deployment_id UUID PRIMARY KEY REFERENCES public_card_final_deployment_v1(deployment_id),
 original_digest TEXT NOT NULL, from_digest TEXT NOT NULL, to_digest TEXT NOT NULL,
 predecessor_version BIGINT NOT NULL CHECK(predecessor_version=40),
 predecessor_checksum BYTEA NOT NULL,
 migration_version BIGINT NOT NULL CHECK(migration_version=42),
 migration_checksum BYTEA NOT NULL,
 recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER public_card_final_forward42_immutable BEFORE UPDATE OR DELETE ON public_card_final_forward42_v1
 FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
REVOKE ALL ON TABLE public_card_final_forward42_v1 FROM PUBLIC;
CREATE OR REPLACE FUNCTION public_card_final_expected_digest_v1(deployment UUID, original_digest TEXT) RETURNS TEXT
 LANGUAGE sql STABLE SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
 SELECT coalesce((SELECT d.to_digest FROM public_card_final_forward42_v1 d
  JOIN _sqlx_migrations m ON m.version=d.migration_version AND m.checksum=d.migration_checksum AND m.success
  JOIN _sqlx_migrations p ON p.version=d.predecessor_version AND p.checksum=d.predecessor_checksum AND p.success
  WHERE d.deployment_id=$1 AND d.original_digest=$2 AND d.from_digest=previous.digest),previous.digest)
 FROM (SELECT coalesce((SELECT c.to_digest FROM public_card_final_forward38_v1 c
  JOIN _sqlx_migrations m ON m.version=c.migration_version AND m.checksum=c.migration_checksum AND m.success
  WHERE c.deployment_id=$1 AND c.original_digest=$2 AND c.from_digest=prior.digest),prior.digest) AS digest
 FROM (SELECT coalesce((SELECT b.to_digest FROM public_card_final_forward37_v1 b
  JOIN _sqlx_migrations m ON m.version=b.migration_version AND m.checksum=b.migration_checksum AND m.success
  WHERE b.deployment_id=$1 AND b.original_digest=$2 AND b.from_digest=base.digest),base.digest) AS digest
 FROM (SELECT coalesce((SELECT a.to_digest FROM public_card_final_forward_bindings_v1 a
  JOIN _sqlx_migrations m ON m.version=a.migration_version AND m.checksum=a.migration_checksum AND m.success
  WHERE a.deployment_id=$1 AND a.from_digest=$2),$2) AS digest) base) prior) previous
$$;
-- Retain the existing NOLOGIN definer owner and private reader separation.
DO $$
DECLARE digest_owner name;
BEGIN
 SELECT pg_catalog.pg_get_userbyid(proowner) INTO digest_owner FROM pg_catalog.pg_proc
 WHERE oid='__SCHEMA__.public_card_final_expected_digest_v1(uuid,text)'::regprocedure;
 EXECUTE format('ALTER TABLE __SCHEMA__.public_card_final_forward42_v1 OWNER TO %I',digest_owner);
END $$;
