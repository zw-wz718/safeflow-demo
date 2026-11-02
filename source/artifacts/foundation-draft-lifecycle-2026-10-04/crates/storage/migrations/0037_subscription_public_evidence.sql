-- Public confirmation evidence is separate from the unchanged member Journal.
-- Extend the original native-only installer solely to the already adopted team
-- database. This does not permit an API role to install/alter source authority.
CREATE OR REPLACE FUNCTION subscription_a2_install_only_v1() RETURNS trigger LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
DECLARE team_scope boolean;
BEGIN
 team_scope := false;
 IF starts_with(current_schema(),'sf_unified_') THEN
  SELECT EXISTS(SELECT 1 FROM simulation_source_heads_v1 h
   JOIN simulation_source_versions_v1 v USING(source_id,owner_id,revision)
   JOIN public_unified_fictional_source_v1 u ON u.source_id=h.source_id
   WHERE h.owner_id=NEW.owner_id AND h.active AND u.active
   AND h.revision='billing-team-consumption-proposed-20261027-v1'
   AND u.artifact_commit='7c03f03b832aa17c174b1bbdd4a6ad3282e01422'
   AND u.source_revision='safeflow-team-simulation-20261026-v1'
   AND v.spec->>'owner'=NEW.owner_id AND v.initial IS NOT NULL) INTO team_scope;
 END IF;
 IF current_setting('safeflow.subscription_a2_install',true) IS DISTINCT FROM txid_current()::text
 OR NOT (starts_with(current_database(),'billing_test_') OR starts_with(current_database(),'subscription_test_'))
 OR NOT (starts_with(current_schema(),'sf_subscription_test_') OR starts_with(current_schema(),'sf_http_test_') OR team_scope)
 THEN RAISE EXCEPTION 'explicit isolated subscription installation required'; END IF;
 IF TG_TABLE_NAME='public_subscription_a2_sources_v1' THEN
  IF team_scope AND NEW.source_revision<>'billing-team-consumption-proposed-20261027-v1'
  THEN RAISE EXCEPTION 'fixed team source required'; END IF;
  IF NEW.database_name<>current_database() OR NEW.database_oid<>(SELECT oid FROM pg_database WHERE datname=current_database())
   OR NEW.namespace<>current_schema() OR NEW.namespace_oid<>current_schema()::regnamespace::oid
   OR NEW.server_port<>current_setting('port')::integer
  THEN RAISE EXCEPTION 'subscription source scope mismatch'; END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE TABLE subscription_public_evidence_v1 (
 operation_id TEXT PRIMARY KEY REFERENCES subscription_public_bindings_v1(operation_id),
 business_ref TEXT NOT NULL UNIQUE REFERENCES subscription_public_bindings_v1(business_ref),
 owner_id TEXT NOT NULL REFERENCES subscription_runtime_records(owner_id),
 evidence_digest TEXT NOT NULL CHECK (evidence_digest ~ '^[0-9a-f]{64}$'),
 body JSONB NOT NULL CHECK (jsonb_typeof(body)='object'),
 body_sha256 TEXT NOT NULL CHECK (body_sha256=encode(sha256(convert_to(body::text,'UTF8')),'hex')),
 CHECK ((body->>'operation_id'=operation_id AND body->>'business_ref'=business_ref AND body->>'owner'=owner_id) IS TRUE)
);
CREATE TRIGGER subscription_public_evidence_immutable BEFORE UPDATE OR DELETE ON subscription_public_evidence_v1
 FOR EACH ROW EXECUTE FUNCTION protect_subscription_binding_v1();
CREATE FUNCTION verify_subscription_public_evidence_v1() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE b subscription_public_bindings_v1; e subscription_public_evidence_v1; op text;
BEGIN
 op=NEW.operation_id;
 SELECT * INTO b FROM subscription_public_bindings_v1 WHERE operation_id=op;
 IF b.public_parameters ? 'public_evidence' THEN
  SELECT * INTO e FROM subscription_public_evidence_v1 WHERE operation_id=op;
  IF e.operation_id IS NULL OR e.owner_id IS DISTINCT FROM b.owner_id
    OR e.business_ref IS DISTINCT FROM b.business_ref
    OR e.evidence_digest IS DISTINCT FROM b.public_parameters->>'public_evidence'
    OR e.body->'member_binding' IS DISTINCT FROM b.member_binding
    OR e.body->>'session_digest' IS DISTINCT FROM b.session_digest
    OR (e.body->>'binding_version')::bigint IS DISTINCT FROM b.binding_version
  THEN RAISE EXCEPTION 'subscription public evidence association missing or changed'; END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE CONSTRAINT TRIGGER subscription_public_evidence_binding AFTER INSERT ON subscription_public_bindings_v1
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION verify_subscription_public_evidence_v1();
CREATE CONSTRAINT TRIGGER subscription_public_evidence_record AFTER INSERT ON subscription_public_evidence_v1
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION verify_subscription_public_evidence_v1();

-- A precise, immutable continuation of already verified card deployments.
-- No digest wildcard, rewrite or automatic reactivation is introduced.
CREATE TABLE public_card_final_forward37_v1 (
 deployment_id UUID PRIMARY KEY REFERENCES public_card_final_deployment_v1(deployment_id),
 original_digest TEXT NOT NULL, from_digest TEXT NOT NULL, to_digest TEXT NOT NULL,
 migration_version BIGINT NOT NULL CHECK(migration_version=37),
 migration_checksum BYTEA NOT NULL, recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER public_card_final_forward37_immutable BEFORE UPDATE OR DELETE ON public_card_final_forward37_v1
 FOR EACH ROW EXECUTE FUNCTION completion_candidate_immutable_v1();
REVOKE ALL ON TABLE public_card_final_forward37_v1 FROM PUBLIC;
CREATE OR REPLACE FUNCTION public_card_final_expected_digest_v1(deployment UUID, original_digest TEXT) RETURNS TEXT
 LANGUAGE sql STABLE SECURITY DEFINER SET search_path FROM CURRENT AS $$
 SELECT coalesce((SELECT b.to_digest FROM public_card_final_forward37_v1 b
  JOIN _sqlx_migrations m ON m.version=b.migration_version AND m.checksum=b.migration_checksum AND m.success
  WHERE b.deployment_id=$1 AND b.original_digest=$2 AND b.from_digest=base.digest),base.digest)
 FROM (SELECT coalesce((SELECT b.to_digest FROM public_card_final_forward_bindings_v1 b
  JOIN _sqlx_migrations m ON m.version=b.migration_version AND m.checksum=b.migration_checksum AND m.success
  WHERE b.deployment_id=$1 AND b.from_digest=$2),$2) AS digest) base
$$;
ALTER FUNCTION public_card_final_expected_digest_v1(UUID,TEXT) SET search_path TO pg_catalog,__SCHEMA__,pg_temp;
