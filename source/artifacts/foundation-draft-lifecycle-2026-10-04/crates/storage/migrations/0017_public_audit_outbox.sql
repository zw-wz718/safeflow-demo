-- Internal public evidence, NOT a C-AUD business-event DTO or a bank command.
CREATE TABLE public_audit_outbox (
    event_id TEXT PRIMARY KEY REFERENCES audit_events(event_id),
    sequence BIGINT NOT NULL UNIQUE,
    envelope JSONB NOT NULL CHECK (jsonb_typeof(envelope)='object'),
    enqueued_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    historical BOOLEAN NOT NULL,
    receiver TEXT,
    receipt_ref TEXT,
    acknowledged_at TIMESTAMPTZ,
    CHECK ((receiver IS NULL AND receipt_ref IS NULL AND acknowledged_at IS NULL)
        OR (receiver IS NOT NULL AND receipt_ref IS NOT NULL
            AND length(receiver) BETWEEN 1 AND 200 AND length(receipt_ref) BETWEEN 1 AND 200 AND acknowledged_at IS NOT NULL))
);
CREATE INDEX public_audit_outbox_pending ON public_audit_outbox(sequence) WHERE acknowledged_at IS NULL;

CREATE FUNCTION enqueue_public_audit() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO public_audit_outbox(event_id,sequence,envelope,historical)
    SELECT NEW.event_id,NEW.sequence,
        jsonb_build_object('storage_version',1,'owner_id',t.owner_id,'public_event',to_jsonb(NEW)),false
    FROM tasks t WHERE t.task_id=NEW.task_id;
    IF NOT FOUND THEN RAISE EXCEPTION 'public audit owner missing'; END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER public_audit_enqueue AFTER INSERT ON audit_events
    FOR EACH ROW EXECUTE FUNCTION enqueue_public_audit();

-- Preserve original IDs and original public persistence time on upgrade.
-- This does not infer a business occurrence time or claim past delivery.
INSERT INTO public_audit_outbox(event_id,sequence,envelope,historical)
SELECT a.event_id,a.sequence,
    jsonb_build_object('storage_version',1,'owner_id',t.owner_id,'public_event',to_jsonb(a)),true
FROM audit_events a JOIN tasks t USING(task_id);

CREATE FUNCTION protect_public_audit_outbox() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'public outbox is retained'; END IF;
    IF ROW(NEW.event_id,NEW.sequence,NEW.envelope,NEW.enqueued_at,NEW.historical)
       IS DISTINCT FROM ROW(OLD.event_id,OLD.sequence,OLD.envelope,OLD.enqueued_at,OLD.historical)
       OR (OLD.acknowledged_at IS NOT NULL AND NEW IS DISTINCT FROM OLD) THEN
        RAISE EXCEPTION 'public outbox evidence is immutable';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER public_audit_outbox_immutable BEFORE UPDATE OR DELETE ON public_audit_outbox
    FOR EACH ROW EXECUTE FUNCTION protect_public_audit_outbox();
