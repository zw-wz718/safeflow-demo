ALTER TABLE security_decision_time_fence
    ADD COLUMN last_observed_us BIGINT NOT NULL DEFAULT 0
        CHECK (last_observed_us >= 0);

-- Historical millisecond observations have no recoverable submillisecond value.
-- Preserve the old field and conservatively fence the end of its entire bucket.
UPDATE security_decision_time_fence
SET last_observed_us = LEAST(
    last_observed_ms::NUMERIC * 1000 + 999,
    9223372036854775807::NUMERIC
)::BIGINT;
