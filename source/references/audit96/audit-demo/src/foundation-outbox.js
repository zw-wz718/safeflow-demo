import { AuditInputError } from './audit-recorder.js';

const ENVELOPE_KEYS = ['storage_version', 'owner_id', 'public_event'];
const PUBLIC_EVENT_KEYS = [
  'sequence', 'event_id', 'task_id', 'operation_id', 'kind', 'task_revision',
  'operation_revision', 'details', 'occurred_at',
];
const REQUEST_KEYS = ['event_id', 'envelope', 'digest', 'historical', 'enqueued_at'];

function object(value, label) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new AuditInputError(`${label} must be a JSON object`);
  }
  return value;
}

function exactKeys(value, expected, label) {
  const actual = Object.keys(value).sort();
  const wanted = [...expected].sort();
  if (actual.length !== wanted.length || actual.some((key, index) => key !== wanted[index])) {
    throw new AuditInputError(`${label} fields do not match the supported foundation outbox v1 shape`);
  }
}

function requiredText(value, label) {
  if (typeof value !== 'string' || !value.trim() || value.length > 200 || /[\u0000-\u001f\u007f]/.test(value)) {
    throw new AuditInputError(`${label} must be a non-empty text value of at most 200 characters`);
  }
  return value;
}

function nullableText(value, label) {
  return value === null ? null : requiredText(value, label);
}

function positiveSafeInteger(value, label, nullable = false) {
  if (nullable && value === null) return null;
  if (!Number.isSafeInteger(value) || value < 1) throw new AuditInputError(`${label} must be a positive safe integer${nullable ? ' or null' : ''}`);
  return value;
}

function zonedTimestamp(value, label) {
  if (typeof value !== 'string' || !/(?:Z|[+-]\d{2}:?\d{2})$/i.test(value) || !Number.isFinite(Date.parse(value))) {
    throw new AuditInputError(`${label} must be an ISO timestamp with Z or an explicit timezone offset`);
  }
  return value;
}

/**
 * Convert the current common-base Postgres Outbox worker row into the
 * audit-owned immutable record shape. The original envelope and all source
 * metadata are retained under details; uncertain semantics stay null.
 */
export function foundationOutboxToAuditEvent(input) {
  const request = object(input, 'foundation outbox request');
  exactKeys(request, REQUEST_KEYS, 'foundation outbox request');

  const envelope = object(request.envelope, 'envelope');
  exactKeys(envelope, ENVELOPE_KEYS, 'envelope');
  if (envelope.storage_version !== 1) throw new AuditInputError('unsupported foundation outbox storage_version');

  const publicEvent = object(envelope.public_event, 'envelope.public_event');
  exactKeys(publicEvent, PUBLIC_EVENT_KEYS, 'envelope.public_event');
  const eventId = requiredText(request.event_id, 'event_id');
  if (requiredText(publicEvent.event_id, 'public_event.event_id') !== eventId) {
    throw new AuditInputError('event_id does not match envelope.public_event.event_id');
  }
  const ownerId = requiredText(envelope.owner_id, 'envelope.owner_id');
  const taskId = requiredText(publicEvent.task_id, 'public_event.task_id');
  const kind = requiredText(publicEvent.kind, 'public_event.kind');
  const operationId = nullableText(publicEvent.operation_id, 'public_event.operation_id');
  const sequence = positiveSafeInteger(publicEvent.sequence, 'public_event.sequence');
  const taskRevision = positiveSafeInteger(publicEvent.task_revision, 'public_event.task_revision');
  const operationRevision = positiveSafeInteger(publicEvent.operation_revision, 'public_event.operation_revision', true);
  const occurredAt = zonedTimestamp(publicEvent.occurred_at, 'public_event.occurred_at');
  const sourceDetails = object(publicEvent.details, 'public_event.details');
  if (!/^[0-9a-f]{64}$/.test(request.digest)) throw new AuditInputError('digest must be a lowercase SHA-256 hex string');
  if (typeof request.historical !== 'boolean') throw new AuditInputError('historical must be a boolean');
  const enqueuedAt = zonedTimestamp(request.enqueued_at, 'enqueued_at');

  const preservedPublicEvent = {
    ...publicEvent,
    sequence,
    event_id: eventId,
    task_id: taskId,
    operation_id: operationId,
    kind,
    task_revision: taskRevision,
    operation_revision: operationRevision,
    details: sourceDetails,
    occurred_at: occurredAt,
  };

  return {
    event_id: eventId,
    kind,
    task_id: taskId,
    node_id: null,
    operation_id: operationId,
    user_id: null,
    action: null,
    // The foundation timestamp is the time of its public audit row, not a
    // confirmed business occurrence time. Keep the source value only below.
    timestamp: null,
    security_result: null,
    confirmation: null,
    auth_result: null,
    result: null,
    tool_name: null,
    details: {
      source: 'foundation-public-audit-outbox-v1',
      foundation: {
        storage_version: envelope.storage_version,
        owner_id: ownerId,
        digest: request.digest,
        historical: request.historical,
        enqueued_at: enqueuedAt,
        public_event: preservedPublicEvent,
      },
    },
  };
}
