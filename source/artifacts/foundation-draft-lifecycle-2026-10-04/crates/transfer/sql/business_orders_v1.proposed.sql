-- Business-owner proposal only. Echo must integrate into the ONE public migration
-- chain after review. Never run this against a shared/demo database directly.
-- This is a receipt repository, NOT an account balance, permit, public Operation,
-- public execution-request mapping, audit event or Outbox implementation.
CREATE TABLE IF NOT EXISTS transfer_business_orders_v1 (
    owner_id TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    original_key TEXT NOT NULL,
    request JSONB NOT NULL CHECK (jsonb_typeof(request) = 'object'),
    first_receipt JSONB NOT NULL CHECK (jsonb_typeof(first_receipt) = 'object'),
    latest_receipt JSONB NOT NULL CHECK (jsonb_typeof(latest_receipt) = 'object'),
    source_recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    latest_recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (owner_id, operation_id),
    UNIQUE (owner_id, original_key)
);
