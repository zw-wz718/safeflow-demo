-- Reuse the decision store; this mode cannot be mistaken for a funds Tool/Policy review.
ALTER TABLE security_decisions DROP CONSTRAINT security_decisions_source_mode_check;
ALTER TABLE security_decisions ADD CONSTRAINT security_decisions_source_mode_check CHECK (
    (source_mode='TRUSTED_RUNTIME_V1' AND lifecycle_anchor IS NULL)
    OR (source_mode='TRUSTED_RUNTIME_LIFECYCLE_V2' AND lifecycle_anchor IS NOT NULL)
    OR (source_mode='TRUSTED_PAYMENT_MINIMUM_V1' AND lifecycle_anchor IS NOT NULL
        AND phase='PRE_AUTH' AND tool_name='foundation_payment_intent'
        AND decision->>'outcome' IN ('READY_TO_CONFIRM','WAITING_AUTH'))
);
CREATE UNIQUE INDEX payment_minimum_one_active ON security_decisions(operation_id)
WHERE source_mode='TRUSTED_PAYMENT_MINIMUM_V1' AND status='ACTIVE';
