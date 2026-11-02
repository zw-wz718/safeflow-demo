-- Keep migration 1 unchanged so already migrated schemas retain its checksum.
ALTER TABLE tasks ADD CONSTRAINT tasks_plan_identity_version_check CHECK (
    (
        jsonb_typeof(plan) = 'object'
        AND jsonb_typeof(plan->'task_id') = 'string'
        AND plan->>'task_id' = task_id
        AND jsonb_typeof(plan->'plan_version') = 'number'
        AND plan->'plan_version' = to_jsonb(plan_version)
    ) IS TRUE
);
