import assert from 'node:assert/strict';
import test from 'node:test';
import { AuditInputError } from '../src/audit-recorder.js';
import { foundationOutboxToAuditEvent } from '../src/foundation-outbox.js';

const sample = () => ({
  event_id: 'evt-foundation-operation-created',
  envelope: {
    storage_version: 1,
    owner_id: 'owner-from-foundation-envelope',
    public_event: {
      sequence: 17,
      event_id: 'evt-foundation-operation-created',
      task_id: 'task-foundation-demo',
      operation_id: 'op-foundation-demo',
      kind: 'OPERATION_CREATED',
      task_revision: 2,
      operation_revision: 1,
      details: { source: 'candidate-test', draft_version: 1 },
      occurred_at: '2026-10-19T09:00:00.000Z',
    },
  },
  digest: 'a'.repeat(64),
  historical: false,
  enqueued_at: '2026-10-19T09:00:01.000Z',
});

test('foundation public outbox v1 maps only known fields and preserves the source envelope facts', () => {
  const input = sample();
  const auditEvent = foundationOutboxToAuditEvent(input);
  assert.equal(auditEvent.event_id, input.event_id);
  assert.equal(auditEvent.kind, 'OPERATION_CREATED');
  assert.equal(auditEvent.task_id, input.envelope.public_event.task_id);
  assert.equal(auditEvent.operation_id, input.envelope.public_event.operation_id);
  assert.equal(auditEvent.timestamp, null, 'base occurred_at is not inferred to be bank/business occurrence time');
  assert.equal(auditEvent.user_id, null, 'owner_id from the body is not an authenticated user context');
  assert.equal(auditEvent.action, null);
  assert.equal(auditEvent.result, null);
  assert.deepEqual(auditEvent.details.foundation, {
    storage_version: 1,
    owner_id: 'owner-from-foundation-envelope',
    digest: 'a'.repeat(64),
    historical: false,
    enqueued_at: input.enqueued_at,
    public_event: input.envelope.public_event,
  });
});

test('foundation outbox adapter fails closed on unknown fields, mismatched IDs, and unsafe timestamps', () => {
  assert.throws(() => foundationOutboxToAuditEvent({ ...sample(), extra: true }), AuditInputError);
  assert.throws(() => foundationOutboxToAuditEvent({ ...sample(), event_id: 'different-event-id' }), /does not match/);
  assert.throws(() => foundationOutboxToAuditEvent({ ...sample(), envelope: { ...sample().envelope, storage_version: 2 } }), /storage_version/);
  const futureShape = sample();
  futureShape.envelope.public_event.new_contract_field = 'not mapped';
  assert.throws(() => foundationOutboxToAuditEvent(futureShape), /shape/);
  const noTimezone = sample();
  noTimezone.envelope.public_event.occurred_at = '2026-10-19T09:00:00';
  assert.throws(() => foundationOutboxToAuditEvent(noTimezone), /timezone/);
});
