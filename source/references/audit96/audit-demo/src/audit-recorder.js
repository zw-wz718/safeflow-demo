// Preserve raw Runtime events when trusted identity, occurrence time, or the
// business action/result mapping has not been supplied. C-AUD/T7 must still
// decide which fields are mandatory at production integration.
const REQUIRED_FIELDS = ['event_id', 'kind', 'task_id'];
const OPTIONAL_TEXT_FIELDS = [
  'node_id', 'operation_id', 'user_id', 'action', 'security_result',
  'confirmation', 'auth_result', 'result', 'tool_name',
];
const ALLOWED_FIELDS = new Set([
  ...REQUIRED_FIELDS,
  ...OPTIONAL_TEXT_FIELDS, 'timestamp', 'occurred_at', 'details',
]);

export class AuditInputError extends Error {
  constructor(message) {
    super(message);
    this.name = 'AuditInputError';
  }
}

function normalizeAuditEvent(input) {
  if (!input || typeof input !== 'object' || Array.isArray(input)) {
    throw new AuditInputError('audit event must be a JSON object');
  }

  const unsupported = Object.keys(input).filter((key) => !ALLOWED_FIELDS.has(key));
  if (unsupported.length) {
    throw new AuditInputError(`unsupported audit event fields: ${unsupported.join(', ')}; put domain-specific data in details`);
  }

  for (const field of REQUIRED_FIELDS) {
    if (typeof input[field] !== 'string' || !input[field].trim()) {
      throw new AuditInputError(`${field} is required for an operation event`);
    }
  }

  if (input.timestamp != null && input.occurred_at != null && input.timestamp !== input.occurred_at) {
    throw new AuditInputError('timestamp and occurred_at must match when both are provided');
  }
  const occurredAt = input.occurred_at ?? input.timestamp ?? null;
  if (occurredAt !== null) {
    const hasTimezone = typeof occurredAt === 'string' && /(?:Z|[+-]\d{2}:?\d{2})$/i.test(occurredAt);
    if (!hasTimezone || !Number.isFinite(Date.parse(occurredAt))) {
      throw new AuditInputError('occurred_at (or timestamp) must be null or an ISO date string with Z or an explicit timezone offset');
    }
  }
  if (input.details !== undefined && (!input.details || typeof input.details !== 'object' || Array.isArray(input.details))) {
    throw new AuditInputError('details must be a JSON object');
  }
  for (const field of OPTIONAL_TEXT_FIELDS) {
    if (input[field] !== undefined && input[field] !== null && (typeof input[field] !== 'string' || !input[field].trim())) {
      throw new AuditInputError(`${field} must be a non-empty string or null`);
    }
  }

  return {
    ...input,
    timestamp: occurredAt,
    details: input.details ?? {},
  };
}

// This facade depends only on the repository methods below. A PostgreSQL repository
// can replace SQLite later without changing callers of recordAudit().
export class AuditRecorder {
  constructor(repository) {
    const methods = [
      'recordAudit', 'recordAuditWithStatus', 'getAudit', 'getAuditByEventId', 'getAuditsByTask', 'getAuditByOperation', 'getLatestTask', 'verifyChain', 'close',
    ];
    for (const method of methods) {
      if (typeof repository?.[method] !== 'function') throw new TypeError(`audit repository must implement ${method}()`);
    }
    this.repository = repository;
  }

  recordAudit(input) { return this.repository.recordAudit(normalizeAuditEvent(input)); }
  recordAuditWithStatus(input) { return this.repository.recordAuditWithStatus(normalizeAuditEvent(input)); }
  getAudit(auditId) { return this.repository.getAudit(auditId); }
  getAuditByEventId(eventId) { return this.repository.getAuditByEventId(eventId); }
  getAuditsByTask(taskId) { return this.repository.getAuditsByTask(taskId); }
  getAuditByOperation(operationId) { return this.repository.getAuditByOperation(operationId); }
  getLatestTask() { return this.repository.getLatestTask(); }
  verifyChain(checkpoint = null) { return this.repository.verifyChain(checkpoint); }
  close() { return this.repository.close(); }
}
