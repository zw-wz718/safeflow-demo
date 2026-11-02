-- Append only. Isolated simulation approval never changes legacy auth meaning.
ALTER TABLE execution_permits DROP CONSTRAINT execution_permits_execution_scope_check;
ALTER TABLE execution_permits ADD CONSTRAINT execution_permits_execution_scope_check CHECK
 (execution_scope IN ('LEGACY','COMMON','TRANSFER','ISOLATED_CARD'));
ALTER TABLE execution_permits DROP CONSTRAINT execution_permits_authentication_check;
ALTER TABLE execution_permits ADD CONSTRAINT execution_permits_authentication_check CHECK ((
 (storage_version=1 AND authentication_state='LEGACY_REQUIRED' AND required_gates IS NULL AND auth_grant_ref IS NOT NULL AND execution_scope<>'ISOLATED_CARD') OR
 (storage_version=2 AND authentication_state='NOT_REQUIRED' AND required_gates='["EXPLICIT_CONFIRMATION"]'::jsonb AND auth_grant_ref IS NULL AND execution_scope<>'ISOLATED_CARD') OR
 (storage_version=2 AND authentication_state='VERIFIED_REQUIRED' AND required_gates='["EXPLICIT_CONFIRMATION","STRONG_AUTHENTICATION"]'::jsonb AND auth_grant_ref IS NOT NULL AND auth_grant_ref !~ '^(sim_grant_|isolated_card_grant_)' AND execution_scope<>'ISOLATED_CARD') OR
 (storage_version=3 AND authentication_state='ISOLATED_CARD_VERIFIED' AND required_gates='["EXPLICIT_CONFIRMATION","STRONG_AUTHENTICATION"]'::jsonb AND auth_grant_ref ~ '^isolated_card_grant_[0-9a-f-]{36}$' AND execution_scope='ISOLATED_CARD' AND tool_name='card_freeze_isolated')
) IS TRUE);

CREATE TABLE card_safety_session_fences_v1 (
 session_digest TEXT PRIMARY KEY CHECK(session_digest ~ '^[0-9a-f]{64}$'),
 owner_id TEXT NOT NULL, binding_version BIGINT NOT NULL CHECK(binding_version>0),
 epoch BIGINT NOT NULL DEFAULT 1 CHECK(epoch>0), revoked BOOLEAN NOT NULL DEFAULT false
);
CREATE TABLE card_safety_account_fences_v1 (
 owner_id TEXT NOT NULL, account_id TEXT NOT NULL REFERENCES transfer_accounts_v1(account_id),
 epoch BIGINT NOT NULL DEFAULT 1 CHECK(epoch>0), revoked BOOLEAN NOT NULL DEFAULT false,
 PRIMARY KEY(owner_id,account_id)
);
CREATE TABLE public_card_safety_requests_v1 (
 operation_id TEXT PRIMARY KEY REFERENCES operations(operation_id),
 task_id TEXT NOT NULL UNIQUE REFERENCES tasks(task_id), owner_id TEXT NOT NULL,
 nonce UUID NOT NULL, session_digest TEXT NOT NULL REFERENCES card_safety_session_fences_v1(session_digest),
 binding_version BIGINT NOT NULL CHECK(binding_version>0),
 session_epoch BIGINT NOT NULL CHECK(session_epoch>0), account_epoch BIGINT NOT NULL CHECK(account_epoch>0),
 account_id TEXT NOT NULL, card_id TEXT NOT NULL,
 input JSONB NOT NULL CHECK(jsonb_typeof(input)='object'), input_digest TEXT NOT NULL CHECK(input_digest ~ '^[0-9a-f]{64}$'),
 confirmation_ref TEXT NOT NULL UNIQUE REFERENCES server_confirmations(confirmation_ref),
 created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
 state TEXT NOT NULL DEFAULT 'PREPARED' CHECK(state IN ('PREPARED','ADMITTED','RECONCILE','RECORDED','REVOKED')),
 UNIQUE(owner_id,nonce), FOREIGN KEY(owner_id,account_id) REFERENCES card_safety_account_fences_v1(owner_id,account_id)
);
CREATE TABLE card_safety_challenges_v1 (
 challenge_id UUID PRIMARY KEY, operation_id TEXT NOT NULL UNIQUE REFERENCES public_card_safety_requests_v1(operation_id),
 host_generation UUID NOT NULL, code_hash TEXT NOT NULL CHECK(code_hash ~ '^[0-9a-f]{64}$'),
 failures SMALLINT NOT NULL DEFAULT 0 CHECK(failures BETWEEN 0 AND 5),
 state TEXT NOT NULL DEFAULT 'PENDING' CHECK(state IN ('PENDING','FAILED','VERIFIED','REVOKED')),
 created_ms BIGINT NOT NULL CHECK(created_ms>=0), expires_ms BIGINT NOT NULL CHECK(expires_ms=created_ms+120000),
 verified_ms BIGINT, grant_expires_ms BIGINT, grant_ref TEXT UNIQUE,
 CHECK ((state='VERIFIED' AND verified_ms IS NOT NULL AND grant_expires_ms=verified_ms+120000 AND grant_ref ~ '^isolated_card_grant_[0-9a-f-]{36}$') OR
        (state<>'VERIFIED' AND verified_ms IS NULL AND grant_expires_ms IS NULL AND grant_ref IS NULL))
);
CREATE FUNCTION card_safety_immutable_v1() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' THEN RAISE EXCEPTION 'card safety history cannot be deleted'; END IF;
 IF TG_TABLE_NAME='public_card_safety_requests_v1' THEN
   IF (to_jsonb(NEW)-'state') IS DISTINCT FROM (to_jsonb(OLD)-'state') OR OLD.state<>NEW.state AND (OLD.state<>'PREPARED' OR NEW.state NOT IN ('ADMITTED','REVOKED')) THEN
     RAISE EXCEPTION 'card original immutable';
   END IF;
 ELSIF TG_TABLE_NAME IN ('card_safety_session_fences_v1','card_safety_account_fences_v1') THEN
   IF (to_jsonb(NEW)-'epoch'-'revoked') IS DISTINCT FROM (to_jsonb(OLD)-'epoch'-'revoked') OR NOT (OLD.revoked=false AND NEW.revoked=true AND NEW.epoch=OLD.epoch+1) THEN
     RAISE EXCEPTION 'only one-way durable revocation';
   END IF;
 ELSE
   IF (to_jsonb(NEW)-'failures'-'state'-'verified_ms'-'grant_expires_ms'-'grant_ref') IS DISTINCT FROM
      (to_jsonb(OLD)-'failures'-'state'-'verified_ms'-'grant_expires_ms'-'grant_ref') OR OLD.state<>'PENDING' OR
      NOT ((NEW.state IN ('PENDING','FAILED') AND NEW.failures=OLD.failures+1 AND (NEW.state='FAILED')=(NEW.failures=5)) OR
           (NEW.state='VERIFIED' AND NEW.failures=OLD.failures AND NEW.verified_ms>=OLD.created_ms AND NEW.verified_ms<OLD.expires_ms) OR
           (NEW.state='REVOKED' AND NEW.failures=OLD.failures)) THEN RAISE EXCEPTION 'invalid challenge transition'; END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER card_safety_original BEFORE UPDATE OR DELETE ON public_card_safety_requests_v1 FOR EACH ROW EXECUTE FUNCTION card_safety_immutable_v1();
CREATE TRIGGER card_safety_session BEFORE UPDATE OR DELETE ON card_safety_session_fences_v1 FOR EACH ROW EXECUTE FUNCTION card_safety_immutable_v1();
CREATE TRIGGER card_safety_account BEFORE UPDATE OR DELETE ON card_safety_account_fences_v1 FOR EACH ROW EXECUTE FUNCTION card_safety_immutable_v1();
CREATE TRIGGER card_safety_challenge BEFORE UPDATE OR DELETE ON card_safety_challenges_v1 FOR EACH ROW EXECUTE FUNCTION card_safety_immutable_v1();
-- Capability to mutate these rows belongs only to trusted backend deployment.
REVOKE ALL ON card_safety_session_fences_v1,card_safety_account_fences_v1,public_card_safety_requests_v1,card_safety_challenges_v1 FROM PUBLIC;
REVOKE ALL ON FUNCTION card_safety_immutable_v1() FROM PUBLIC;

-- The adopted v1 document permits no membership/ownership/opening/route
-- revision API. Enforce that exact frozen source in the database as well:
-- booked balances remain member-owned mutable business data. This closes
-- insertion/deletion phantoms after a complete-source check without inventing
-- a future source-update protocol or trusting an in-process observer.
CREATE FUNCTION public_unified_card_source_scope_v1() RETURNS trigger
LANGUAGE plpgsql SET search_path FROM CURRENT AS $$
BEGIN
 IF TG_TABLE_NAME='transfer_accounts_v1' AND TG_OP='UPDATE' THEN
   IF NEW.account_id IS NOT DISTINCT FROM OLD.account_id AND NEW.owner_id IS NOT DISTINCT FROM OLD.owner_id AND NEW.opening_cents IS NOT DISTINCT FROM OLD.opening_cents THEN RETURN NEW; END IF;
 END IF;
 PERFORM singleton FROM public_unified_fictional_source_v1 WHERE singleton=1 FOR SHARE;
 IF FOUND THEN RAISE EXCEPTION 'ADOPTED_UNIFIED_V1_SCOPE_IMMUTABLE'; END IF;
 IF TG_OP='DELETE' THEN RETURN OLD; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER public_unified_card_accounts_scope_v1 BEFORE INSERT OR UPDATE OR DELETE ON transfer_accounts_v1 FOR EACH ROW EXECUTE FUNCTION public_unified_card_source_scope_v1();
CREATE TRIGGER public_unified_card_routes_scope_v1 BEFORE INSERT OR UPDATE OR DELETE ON transfer_payee_routes_v1 FOR EACH ROW EXECUTE FUNCTION public_unified_card_source_scope_v1();
REVOKE ALL ON FUNCTION public_unified_card_source_scope_v1() FROM PUBLIC;
