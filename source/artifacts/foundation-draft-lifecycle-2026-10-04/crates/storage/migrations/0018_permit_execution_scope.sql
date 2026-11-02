-- Legacy rows carry no verified domain classification. Do not infer a new
-- authorization from their arguments or silently classify them as transfer.
ALTER TABLE execution_permits ADD COLUMN execution_scope TEXT NOT NULL DEFAULT 'LEGACY'
    CHECK (execution_scope IN ('LEGACY','COMMON','TRANSFER'));

CREATE FUNCTION preserve_permit_execution_scope() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.execution_scope IS DISTINCT FROM OLD.execution_scope THEN
        RAISE EXCEPTION 'immutable permit execution scope';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER preserve_permit_execution_scope BEFORE UPDATE ON execution_permits
FOR EACH ROW EXECUTE FUNCTION preserve_permit_execution_scope();
