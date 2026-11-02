ALTER TABLE execution_permits
    ALTER COLUMN auth_grant_ref DROP NOT NULL,
    ADD COLUMN storage_version SMALLINT NOT NULL DEFAULT 1,
    ADD COLUMN authentication_state TEXT NOT NULL DEFAULT 'LEGACY_REQUIRED',
    ADD COLUMN required_gates JSONB;

-- Existing rows and old writers keep their original mandatory-reference meaning.
-- IS TRUE prevents SQL NULL from bypassing the gate/reference combination check.
ALTER TABLE execution_permits ADD CONSTRAINT execution_permits_authentication_check CHECK (
    (
        (storage_version=1 AND authentication_state='LEGACY_REQUIRED'
            AND required_gates IS NULL AND auth_grant_ref IS NOT NULL)
        OR
        (storage_version=2 AND authentication_state='NOT_REQUIRED'
            AND required_gates='["EXPLICIT_CONFIRMATION"]'::jsonb
            AND auth_grant_ref IS NULL)
        OR
        (storage_version=2 AND authentication_state='VERIFIED_REQUIRED'
            AND required_gates='["EXPLICIT_CONFIRMATION","STRONG_AUTHENTICATION"]'::jsonb
            AND auth_grant_ref IS NOT NULL
            AND auth_grant_ref !~ '^sim_grant_')
    ) IS TRUE
);
