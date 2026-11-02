-- Row locking is not source-management authority. Existing readers may retain
-- an UPDATE lock column, but their real writes cannot change any head fact.
-- Use the login identity, never current_user promoted by a reachable definer.
CREATE FUNCTION simulation_source_head_authority_v1() RETURNS trigger
 LANGUAGE plpgsql SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
DECLARE manager boolean;
BEGIN
 IF TG_TABLE_SCHEMA <> '__SCHEMA__' OR TG_TABLE_NAME <> 'simulation_source_heads_v1'
 THEN RAISE EXCEPTION 'SIMULATION_SOURCE_AUTHORITY_SCOPE'; END IF;
 -- Never permit deleting a generation and recreating an old fence, including
 -- through a future function accidentally granted to a non-manager.
 IF TG_OP IN ('DELETE','TRUNCATE') THEN RAISE EXCEPTION 'SIMULATION_SOURCE_HEAD_DELETE_FORBIDDEN'; END IF;
 IF TG_OP='UPDATE' AND NEW IS NOT DISTINCT FROM OLD THEN RETURN NEW; END IF;
 SELECT pg_catalog.pg_has_role(session_user,c.relowner,'USAGE') INTO manager
 FROM pg_catalog.pg_class c WHERE c.oid=TG_RELID;
 IF manager IS DISTINCT FROM true
 THEN RAISE EXCEPTION 'SIMULATION_SOURCE_MANAGEMENT_REQUIRED'; END IF;
 IF TG_OP='INSERT' THEN
  IF NEW.generation<>1 THEN RAISE EXCEPTION 'SIMULATION_SOURCE_INITIAL_GENERATION'; END IF;
 ELSIF NEW.source_id IS DISTINCT FROM OLD.source_id OR NEW.owner_id IS DISTINCT FROM OLD.owner_id
  OR OLD.generation=9223372036854775807 OR NEW.generation IS DISTINCT FROM OLD.generation+1
 THEN RAISE EXCEPTION 'SIMULATION_SOURCE_GENERATION_MUST_ADVANCE';
 END IF;
 RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION simulation_source_head_authority_v1() FROM PUBLIC;
CREATE TRIGGER simulation_source_head_authority BEFORE INSERT OR UPDATE OR DELETE
 ON simulation_source_heads_v1 FOR EACH ROW EXECUTE FUNCTION simulation_source_head_authority_v1();
CREATE TRIGGER simulation_source_head_no_truncate BEFORE TRUNCATE
 ON simulation_source_heads_v1 FOR EACH STATEMENT EXECUTE FUNCTION simulation_source_head_authority_v1();

-- The API legitimately starts a new host, but that UPDATE capability must not
-- restore an earlier host fence either. No member business rule is changed.
CREATE FUNCTION subscription_host_generation_advance_v1() RETURNS trigger
 LANGUAGE plpgsql SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
BEGIN
 IF TG_TABLE_SCHEMA <> '__SCHEMA__' OR TG_TABLE_NAME <> 'subscription_host_generations_v1'
 THEN RAISE EXCEPTION 'SUBSCRIPTION_HOST_GENERATION_SCOPE'; END IF;
 IF TG_OP IN ('DELETE','TRUNCATE') THEN RAISE EXCEPTION 'SUBSCRIPTION_HOST_DELETE_FORBIDDEN'; END IF;
 IF TG_OP='UPDATE' AND NEW IS NOT DISTINCT FROM OLD THEN RETURN NEW; END IF;
 IF TG_OP='INSERT' THEN
  IF NEW.generation<>1 THEN RAISE EXCEPTION 'SUBSCRIPTION_HOST_INITIAL_GENERATION'; END IF;
 ELSIF NEW.owner_id IS DISTINCT FROM OLD.owner_id OR OLD.generation=9223372036854775807
  OR NEW.generation IS DISTINCT FROM OLD.generation+1
 THEN RAISE EXCEPTION 'SUBSCRIPTION_HOST_GENERATION_MUST_ADVANCE';
 END IF;
 RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION subscription_host_generation_advance_v1() FROM PUBLIC;
CREATE TRIGGER subscription_host_generation_advance BEFORE INSERT OR UPDATE OR DELETE
 ON subscription_host_generations_v1 FOR EACH ROW EXECUTE FUNCTION subscription_host_generation_advance_v1();
CREATE TRIGGER subscription_host_generation_no_truncate BEFORE TRUNCATE
 ON subscription_host_generations_v1 FOR EACH STATEMENT EXECUTE FUNCTION subscription_host_generation_advance_v1();

-- Preserve every old deployment and exact predecessor. 37 and 38 are separate
-- immutable bindings, even when both migrations run in one maintenance window.
CREATE TABLE public_card_final_forward38_v1 (
 deployment_id UUID PRIMARY KEY REFERENCES public_card_final_deployment_v1(deployment_id),
 original_digest TEXT NOT NULL, from_digest TEXT NOT NULL, to_digest TEXT NOT NULL,
 migration_version BIGINT NOT NULL CHECK(migration_version=38),
 migration_checksum BYTEA NOT NULL, recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER public_card_final_forward38_immutable BEFORE UPDATE OR DELETE ON public_card_final_forward38_v1
 FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
REVOKE ALL ON TABLE public_card_final_forward38_v1 FROM PUBLIC;
CREATE OR REPLACE FUNCTION public_card_final_expected_digest_v1(deployment UUID, original_digest TEXT) RETURNS TEXT
 LANGUAGE sql STABLE SECURITY DEFINER SET search_path TO pg_catalog,__SCHEMA__,pg_temp AS $$
 SELECT coalesce((SELECT c.to_digest FROM public_card_final_forward38_v1 c
  JOIN _sqlx_migrations m ON m.version=c.migration_version AND m.checksum=c.migration_checksum AND m.success
  WHERE c.deployment_id=$1 AND c.original_digest=$2 AND c.from_digest=prior.digest),prior.digest)
 FROM (SELECT coalesce((SELECT b.to_digest FROM public_card_final_forward37_v1 b
  JOIN _sqlx_migrations m ON m.version=b.migration_version AND m.checksum=b.migration_checksum AND m.success
  WHERE b.deployment_id=$1 AND b.original_digest=$2 AND b.from_digest=base.digest),base.digest) AS digest
 FROM (SELECT coalesce((SELECT a.to_digest FROM public_card_final_forward_bindings_v1 a
  JOIN _sqlx_migrations m ON m.version=a.migration_version AND m.checksum=a.migration_checksum AND m.success
  WHERE a.deployment_id=$1 AND a.from_digest=$2),$2) AS digest) base) prior
$$;

-- Maintenance may run as an administrator while existing objects belong to a
-- NOLOGIN owner. Preserve that separation; the old definer must read its new
-- private binding table without granting the API raw table access.
DO $$
DECLARE source_owner name; host_owner name; digest_owner name;
BEGIN
 SELECT pg_catalog.pg_get_userbyid(relowner) INTO source_owner FROM pg_catalog.pg_class
 WHERE oid='__SCHEMA__.simulation_source_heads_v1'::regclass;
 SELECT pg_catalog.pg_get_userbyid(relowner) INTO host_owner FROM pg_catalog.pg_class
 WHERE oid='__SCHEMA__.subscription_host_generations_v1'::regclass;
 SELECT pg_catalog.pg_get_userbyid(proowner) INTO digest_owner FROM pg_catalog.pg_proc
 WHERE oid='__SCHEMA__.public_card_final_expected_digest_v1(uuid,text)'::regprocedure;
 EXECUTE format('ALTER FUNCTION __SCHEMA__.simulation_source_head_authority_v1() OWNER TO %I',source_owner);
 EXECUTE format('ALTER FUNCTION __SCHEMA__.subscription_host_generation_advance_v1() OWNER TO %I',host_owner);
 EXECUTE format('ALTER TABLE __SCHEMA__.public_card_final_forward38_v1 OWNER TO %I',digest_owner);
END $$;
