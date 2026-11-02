import { createHash, randomUUID } from 'node:crypto';
import { copyFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { DatabaseSync } from 'node:sqlite';

const INPUT_FIELDS = ['event_id', 'kind', 'task_id', 'node_id', 'operation_id', 'user_id', 'action', 'timestamp', 'security_result', 'confirmation', 'auth_result', 'result', 'tool_name', 'details'];
const sha256 = (text) => createHash('sha256').update(text, 'utf8').digest('hex');
const eventFingerprint = (record) => sha256(stableJson(Object.fromEntries(
  INPUT_FIELDS.map((field) => [field, record[field] ?? null]),
)));
const HASH_VERSION_V1 = 'local-js-v1';
const HASH_VERSION_V2 = 'local-js-v2';
const HASH_VERSION = HASH_VERSION_V2;

function stableJson(value) {
  if (Array.isArray(value)) return `[${value.map(stableJson).join(',')}]`;
  if (value && typeof value === 'object') return `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${stableJson(value[key])}`).join(',')}}`;
  return JSON.stringify(value);
}

// Snapshot new input before hashing it. The historical hash format above must
// remain unchanged, but undefined, sparse arrays, and non-finite numbers do
// not have a valid representation in that format.
function copyJsonValue(value, path = 'details', ancestors = new Set()) {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return value;
  if (typeof value === 'number' && Number.isFinite(value)) return value;
  if (!value || typeof value !== 'object') throw new TypeError(`${path} must contain JSON values only`);
  if (ancestors.has(value)) throw new TypeError(`${path} must not contain a cycle`);
  const prototype = Object.getPrototypeOf(value);
  if (!Array.isArray(value) && prototype !== Object.prototype && prototype !== null) {
    throw new TypeError(`${path} must contain plain JSON objects only`);
  }
  if (Object.getOwnPropertySymbols(value).length) throw new TypeError(`${path} must not contain symbol keys`);
  ancestors.add(value);
  try {
    if (Array.isArray(value)) {
      if (prototype !== Array.prototype) throw new TypeError(`${path} must contain plain JSON arrays only`);
      for (const key of Reflect.ownKeys(value)) {
        if (key === 'length') continue;
        if (!/^(0|[1-9]\d*)$/.test(key) || Number(key) >= value.length) {
          throw new TypeError(`${path} must not contain extra array properties`);
        }
      }
      return Array.from({ length: value.length }, (_, index) => {
        if (!Object.hasOwn(value, index)) throw new TypeError(`${path}[${index}] must not be empty`);
        const descriptor = Object.getOwnPropertyDescriptor(value, index);
        if (!descriptor.enumerable || !Object.hasOwn(descriptor, 'value')) {
          throw new TypeError(`${path}[${index}] must be an enumerable JSON value`);
        }
        return copyJsonValue(descriptor.value, `${path}[${index}]`, ancestors);
      });
    }
    const result = {};
    for (const [key, descriptor] of Object.entries(Object.getOwnPropertyDescriptors(value))) {
      if (!descriptor.enumerable || !Object.hasOwn(descriptor, 'value')) {
        throw new TypeError(`${path}.${key} must be an enumerable JSON value`);
      }
      Object.defineProperty(result, key, {
        value: copyJsonValue(descriptor.value, `${path}.${key}`, ancestors),
        enumerable: true,
        writable: true,
        configurable: true,
      });
    }
    return result;
  } finally {
    ancestors.delete(value);
  }
}

function recordHashV1(record) {
  return sha256(stableJson({
    audit_id: record.audit_id,
    event_id: record.event_id,
    kind: record.kind,
    task_id: record.task_id,
    node_id: record.node_id,
    operation_id: record.operation_id,
    user_id: record.user_id,
    action: record.action,
    timestamp: record.timestamp,
    recorded_at: record.recorded_at,
    security_result: record.security_result,
    confirmation: record.confirmation,
    auth_result: record.auth_result,
    result: record.result,
    tool_name: record.tool_name,
    details: record.details,
    prev_hash: record.prev_hash,
  }));
}

function recordHashV2(record) {
  return sha256(stableJson({
    hash_version: HASH_VERSION_V2,
    sequence: record.sequence,
    audit_id: record.audit_id,
    event_id: record.event_id,
    kind: record.kind,
    task_id: record.task_id,
    node_id: record.node_id,
    operation_id: record.operation_id,
    user_id: record.user_id,
    action: record.action,
    timestamp: record.timestamp,
    recorded_at: record.recorded_at,
    security_result: record.security_result,
    confirmation: record.confirmation,
    auth_result: record.auth_result,
    result: record.result,
    tool_name: record.tool_name,
    details: record.details,
    prev_hash: record.prev_hash,
  }));
}

function recordHash(record) {
  return record.hash_version === HASH_VERSION_V2 ? recordHashV2(record) : recordHashV1(record);
}

function legacyRecordHash(record) {
  return sha256(stableJson({
    audit_id: record.audit_id,
    event_id: record.event_id,
    kind: record.kind,
    task_id: record.task_id,
    node_id: record.node_id,
    operation_id: record.operation_id,
    user_id: record.user_id,
    action: record.action,
    timestamp: record.timestamp,
    security_result: record.security_result,
    confirmation: record.confirmation,
    auth_result: record.auth_result,
    result: record.result,
    tool_name: record.tool_name,
    details: JSON.parse(record.details_json),
    prev_hash: record.prev_hash,
  }));
}

function publicRecord(row) {
  if (!row) return null;
  return {
    audit_id: row.audit_id,
    sequence: row.sequence,
    hash_version: row.hash_version ?? null,
    event_id: row.event_id,
    kind: row.kind,
    task_id: row.task_id,
    node_id: row.node_id,
    operation_id: row.operation_id,
    user_id: row.user_id,
    action: row.action,
    timestamp: row.timestamp,
    recorded_at: row.recorded_at,
    security_result: row.security_result,
    confirmation: row.confirmation,
    auth_result: row.auth_result,
    result: row.result,
    tool_name: row.tool_name,
    details: JSON.parse(row.details_json),
    prev_hash: row.prev_hash,
    record_hash: row.record_hash,
  };
}

export class AuditConflictError extends Error {
  constructor(message) { super(message); this.name = 'AuditConflictError'; }
}

// A duplicate delivery must never be acknowledged from a damaged stored row.
// Keep this distinct from bad caller input/conflicts so the HTTP receiver
// reports storage unavailable (503), not a caller error (400/409).
export class AuditIntegrityError extends Error {
  constructor(message) { super(message); this.name = 'AuditIntegrityError'; }
}

function checkedDuplicateRecord(row) {
  let record;
  try {
    record = publicRecord(row);
  } catch (error) {
    if (!(error instanceof SyntaxError)) throw error;
    throw new AuditIntegrityError(`Stored audit details are invalid for audit_id ${row.audit_id}`);
  }
  if (!record.details || typeof record.details !== 'object' || Array.isArray(record.details)) {
    throw new AuditIntegrityError(`Stored audit details are invalid for audit_id ${row.audit_id}`);
  }
  if (row.record_hash !== recordHash(record)) {
    throw new AuditIntegrityError(`Stored audit hash mismatch for audit_id ${row.audit_id}`);
  }
  if (row.fingerprint_version !== 'sha256-v1' && row.fingerprint_version !== null) {
    throw new AuditIntegrityError(`Unsupported stored audit fingerprint version for audit_id ${row.audit_id}`);
  }
  // Historical rows without a marker used the same repository fingerprint
  // shape before the version column existed. Reconstruct it from the stored
  // immutable fields; never trust the unmarked fingerprint on its own.
  if (row.event_fingerprint !== eventFingerprint(record)) {
    throw new AuditIntegrityError(`Stored audit fingerprint cannot be verified for audit_id ${row.audit_id}`);
  }
  return record;
}

export class AuditStore {
  constructor(databasePath = 'data/audit-demo.db') {
    this.db = new DatabaseSync(databasePath);
    this.db.exec(`
      PRAGMA busy_timeout = 5000;
      PRAGMA journal_mode = WAL;
      CREATE TABLE IF NOT EXISTS audit_records (
        sequence INTEGER PRIMARY KEY AUTOINCREMENT,
        audit_id TEXT NOT NULL UNIQUE,
        event_id TEXT NOT NULL UNIQUE,
        event_fingerprint TEXT NOT NULL,
        fingerprint_version TEXT,
        kind TEXT NOT NULL,
        task_id TEXT NOT NULL,
        node_id TEXT,
        operation_id TEXT,
        user_id TEXT,
        action TEXT,
        timestamp TEXT,
        recorded_at TEXT NOT NULL,
        security_result TEXT,
        confirmation TEXT,
        auth_result TEXT,
        result TEXT,
        tool_name TEXT,
        details_json TEXT NOT NULL,
        prev_hash TEXT,
        record_hash TEXT NOT NULL,
        hash_version TEXT
      );
      CREATE INDEX IF NOT EXISTS idx_audit_task ON audit_records(task_id, sequence);
      CREATE INDEX IF NOT EXISTS idx_audit_operation ON audit_records(operation_id, sequence);
    `);
    this.migrateRecordedAt(databasePath);
    this.migrateFingerprintVersion();
    this.migrateHashVersion(databasePath);
    this.migrateNullableRuntimeFields(databasePath);
  }

  migrateFingerprintVersion() {
    const columns = this.db.prepare('PRAGMA table_info(audit_records)').all().map((column) => column.name);
    if (!columns.includes('fingerprint_version')) {
      // Existing rows remain unmarked: older databases may contain an opaque
      // deduplication fingerprint, which is not part of their chain hash.
      this.db.exec('ALTER TABLE audit_records ADD COLUMN fingerprint_version TEXT');
    }
  }

  migrateHashVersion(databasePath) {
    const columns = this.db.prepare('PRAGMA table_info(audit_records)').all().map((column) => column.name);
    const hasHashVersion = columns.includes('hash_version');
    const rows = this.db.prepare('SELECT * FROM audit_records ORDER BY sequence').all();
    let expectedPreviousHash = null;
    let needsMigration = !hasHashVersion;
    for (const row of rows) {
      const record = publicRecord(row);
      const version = record.hash_version ?? HASH_VERSION_V1;
      if (![HASH_VERSION_V1, HASH_VERSION_V2].includes(version)) {
        throw new Error(`Existing audit chain has unsupported hash version; migration stopped at audit_id ${row.audit_id}`);
      }
      if (row.prev_hash !== expectedPreviousHash || row.record_hash !== recordHash({ ...record, hash_version: version })) {
        throw new Error(`Existing audit chain failed verification; hash-version migration stopped at audit_id ${row.audit_id}`);
      }
      expectedPreviousHash = row.record_hash;
      if (record.hash_version == null) needsMigration = true;
    }
    if (!needsMigration) return;

    const backupPath = `${resolve(databasePath)}.pre-hash-version-${Date.now()}.bak`;
    const checkpoint = this.db.prepare('PRAGMA wal_checkpoint(TRUNCATE)').get();
    if (checkpoint?.busy) throw new Error('Could not checkpoint the audit database; hash-version migration stopped');
    copyFileSync(resolve(databasePath), backupPath);
    this.db.exec('BEGIN IMMEDIATE');
    try {
      if (!hasHashVersion) this.db.exec('ALTER TABLE audit_records ADD COLUMN hash_version TEXT');
      this.db.prepare('UPDATE audit_records SET hash_version = ? WHERE hash_version IS NULL').run(HASH_VERSION_V1);
      this.db.exec('COMMIT');
      this.migrationBackupPath = this.migrationBackupPath ?? backupPath;
      this.migrationBackupPaths = [...(this.migrationBackupPaths ?? []), backupPath];
    } catch (error) {
      this.db.exec('ROLLBACK');
      throw error;
    }
  }

  migrateRecordedAt(databasePath) {
    const columns = this.db.prepare('PRAGMA table_info(audit_records)').all().map((column) => column.name);
    if (columns.includes('recorded_at')) return;

    const oldRows = this.db.prepare('SELECT * FROM audit_records ORDER BY sequence').all();
    let expectedPreviousHash = null;
    for (const row of oldRows) {
      if (row.prev_hash !== expectedPreviousHash || row.record_hash !== legacyRecordHash(row)) {
        throw new Error(`Existing audit chain failed verification; migration stopped at audit_id ${row.audit_id}`);
      }
      expectedPreviousHash = row.record_hash;
    }

    const backupPath = `${resolve(databasePath)}.pre-recorded-at-${Date.now()}.bak`;
    const checkpoint = this.db.prepare('PRAGMA wal_checkpoint(TRUNCATE)').get();
    if (checkpoint?.busy) throw new Error('Could not checkpoint the audit database; migration stopped');
    copyFileSync(resolve(databasePath), backupPath);
    this.db.exec('BEGIN IMMEDIATE');
    try {
      this.db.exec('ALTER TABLE audit_records ADD COLUMN recorded_at TEXT');
      this.db.exec('UPDATE audit_records SET recorded_at = timestamp WHERE recorded_at IS NULL');
      const rows = this.db.prepare('SELECT * FROM audit_records ORDER BY sequence').all();
      const update = this.db.prepare('UPDATE audit_records SET prev_hash = ?, record_hash = ? WHERE sequence = ?');
      let previousHash = null;
      for (const row of rows) {
        const record = { ...publicRecord(row), prev_hash: previousHash };
        record.record_hash = recordHash(record);
        update.run(record.prev_hash, record.record_hash, row.sequence);
        previousHash = record.record_hash;
      }
      this.db.exec('COMMIT');
      this.migrationBackupPath = backupPath;
      this.migrationBackupPaths = [...(this.migrationBackupPaths ?? []), backupPath];
    } catch (error) {
      this.db.exec('ROLLBACK');
      throw error;
    }
  }

  migrateNullableRuntimeFields(databasePath) {
    const nullableFields = ['operation_id', 'user_id', 'action', 'timestamp', 'security_result', 'confirmation', 'auth_result', 'result'];
    const columns = this.db.prepare('PRAGMA table_info(audit_records)').all();
    const byName = new Map(columns.map((column) => [column.name, column]));
    if (nullableFields.every((field) => byName.has(field) && byName.get(field).notnull === 0)) return;

    const oldRows = this.db.prepare('SELECT * FROM audit_records ORDER BY sequence').all();
    let expectedPreviousHash = null;
    for (const row of oldRows) {
      if (row.prev_hash !== expectedPreviousHash || row.record_hash !== recordHash(publicRecord(row))) {
        throw new Error(`Existing audit chain failed verification; nullable-field migration stopped at audit_id ${row.audit_id}`);
      }
      expectedPreviousHash = row.record_hash;
    }

    const backupPath = `${resolve(databasePath)}.pre-nullable-fields-${Date.now()}.bak`;
    const checkpoint = this.db.prepare('PRAGMA wal_checkpoint(TRUNCATE)').get();
    if (checkpoint?.busy) throw new Error('Could not checkpoint the audit database; nullable-field migration stopped');
    copyFileSync(resolve(databasePath), backupPath);
    this.db.exec('BEGIN IMMEDIATE');
    try {
      this.db.exec(`CREATE TABLE audit_records_nullable_migration (
        sequence INTEGER PRIMARY KEY AUTOINCREMENT,
        audit_id TEXT NOT NULL UNIQUE,
        event_id TEXT NOT NULL UNIQUE,
        event_fingerprint TEXT NOT NULL,
        fingerprint_version TEXT,
        kind TEXT NOT NULL,
        task_id TEXT NOT NULL,
        node_id TEXT,
        operation_id TEXT,
        user_id TEXT,
        action TEXT,
        timestamp TEXT,
        recorded_at TEXT NOT NULL,
        security_result TEXT,
        confirmation TEXT,
        auth_result TEXT,
        result TEXT,
        tool_name TEXT,
        details_json TEXT NOT NULL,
        prev_hash TEXT,
        record_hash TEXT NOT NULL,
        hash_version TEXT
      )`);
      this.db.exec(`INSERT INTO audit_records_nullable_migration (
        sequence,audit_id,event_id,event_fingerprint,fingerprint_version,kind,task_id,node_id,operation_id,user_id,action,
        timestamp,recorded_at,security_result,confirmation,auth_result,result,tool_name,details_json,prev_hash,record_hash,hash_version
      ) SELECT sequence,audit_id,event_id,event_fingerprint,${byName.has('fingerprint_version') ? 'fingerprint_version' : 'NULL'},kind,task_id,node_id,operation_id,user_id,action,
        timestamp,recorded_at,security_result,confirmation,auth_result,result,tool_name,details_json,prev_hash,record_hash,hash_version
        FROM audit_records ORDER BY sequence`);
      this.db.exec('DROP TABLE audit_records; ALTER TABLE audit_records_nullable_migration RENAME TO audit_records;');
      this.db.exec(`CREATE INDEX IF NOT EXISTS idx_audit_task ON audit_records(task_id, sequence);
        CREATE INDEX IF NOT EXISTS idx_audit_operation ON audit_records(operation_id, sequence);`);
      this.db.exec('COMMIT');
      this.migrationBackupPath = backupPath;
      this.migrationBackupPaths = [...(this.migrationBackupPaths ?? []), backupPath];
    } catch (error) {
      this.db.exec('ROLLBACK');
      throw error;
    }
  }

  // Mirrors the existing Rust AuditEvent shape, with audit-only storage metadata added.
  recordAudit(input) {
    return this.recordAuditWithStatus(input).record;
  }

  recordAuditWithStatus(input) {
    const event = {
      event_id: input.event_id,
      kind: input.kind,
      task_id: input.task_id,
      node_id: input.node_id ?? null,
      operation_id: input.operation_id ?? null,
      user_id: input.user_id ?? null,
      action: input.action ?? null,
      timestamp: input.timestamp ?? null,
      recorded_at: new Date().toISOString(),
      security_result: input.security_result ?? null,
      confirmation: input.confirmation ?? null,
      auth_result: input.auth_result ?? null,
      result: input.result ?? null,
      tool_name: input.tool_name ?? null,
      details: input.details ?? {},
    };
    for (const field of ['event_id', 'kind', 'task_id']) {
      if (typeof event[field] !== 'string' || !event[field].trim()) throw new TypeError(`${field} is required`);
    }
    for (const field of ['node_id', 'operation_id', 'user_id', 'action', 'timestamp', 'security_result', 'confirmation', 'auth_result', 'result', 'tool_name']) {
      if (event[field] !== null && (typeof event[field] !== 'string' || !event[field].trim())) {
        throw new TypeError(`${field} must be a non-empty string or null`);
      }
    }
    if (!event.details || typeof event.details !== 'object' || Array.isArray(event.details)) throw new TypeError('details must be an object');
    event.details = copyJsonValue(event.details);
    const fingerprint = eventFingerprint(event);

    this.db.exec('BEGIN IMMEDIATE');
    try {
      const existing = this.db.prepare('SELECT * FROM audit_records WHERE event_id = ?').get(event.event_id);
      if (existing) {
        const storedRecord = checkedDuplicateRecord(existing);
        if (existing.event_fingerprint !== fingerprint) throw new AuditConflictError('event_id already exists with different content');
        this.db.exec('COMMIT');
        return { record: storedRecord, duplicate: true };
      }
      const previous = this.db.prepare('SELECT sequence, record_hash FROM audit_records ORDER BY sequence DESC LIMIT 1').get();
      const previousSequence = previous?.sequence ?? 0;
      const sqliteSequence = this.db.prepare("SELECT seq FROM sqlite_sequence WHERE name = 'audit_records'").get()?.seq ?? 0;
      const sequence = Math.max(previousSequence, sqliteSequence) + 1;
      if (!Number.isSafeInteger(sequence)) throw new RangeError('audit sequence exceeds JavaScript safe integer range');
      const record = {
        audit_id: randomUUID(), sequence, hash_version: HASH_VERSION_V2,
        ...event, prev_hash: previous?.record_hash ?? null,
      };
      record.record_hash = recordHash(record);
      this.db.prepare(`INSERT INTO audit_records (
        sequence,audit_id,event_id,event_fingerprint,fingerprint_version,kind,task_id,node_id,operation_id,user_id,action,timestamp,
        recorded_at,security_result,confirmation,auth_result,result,tool_name,details_json,prev_hash,record_hash,hash_version
      ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`).run(
        record.sequence, record.audit_id, record.event_id, fingerprint, 'sha256-v1', record.kind, record.task_id, record.node_id,
        record.operation_id, record.user_id, record.action, record.timestamp, record.recorded_at, record.security_result,
        record.confirmation, record.auth_result, record.result, record.tool_name,
        stableJson(record.details), record.prev_hash, record.record_hash, record.hash_version,
      );
      this.db.exec('COMMIT');
      return {
        record: publicRecord(this.db.prepare('SELECT * FROM audit_records WHERE audit_id = ?').get(record.audit_id)),
        duplicate: false,
      };
    } catch (error) {
      this.db.exec('ROLLBACK');
      throw error;
    }
  }

  getAudit(auditId) { return publicRecord(this.db.prepare('SELECT * FROM audit_records WHERE audit_id = ?').get(auditId)); }
  getAuditByEventId(eventId) { return publicRecord(this.db.prepare('SELECT * FROM audit_records WHERE event_id = ?').get(eventId)); }
  getAuditsByTask(taskId) { return this.db.prepare('SELECT * FROM audit_records WHERE task_id = ? ORDER BY sequence').all(taskId).map(publicRecord); }
  getAuditByOperation(operationId) { return this.db.prepare('SELECT * FROM audit_records WHERE operation_id = ? ORDER BY sequence').all(operationId).map(publicRecord); }
  getLatestTask() {
    const latest = this.db.prepare('SELECT task_id FROM audit_records ORDER BY sequence DESC LIMIT 1').get();
    return latest ? this.getAuditsByTask(latest.task_id) : [];
  }

  verifyChain(checkpoint = null) {
    if (checkpoint !== null && (
      !checkpoint || typeof checkpoint !== 'object' || Array.isArray(checkpoint) ||
      !Number.isSafeInteger(checkpoint.sequence) || checkpoint.sequence < 1 ||
      !Number.isSafeInteger(checkpoint.count) || checkpoint.count < 1 ||
      typeof checkpoint.record_hash !== 'string' || !/^[0-9a-f]{64}$/.test(checkpoint.record_hash)
    )) throw new TypeError('checkpoint requires a positive sequence, positive count, and SHA-256 record_hash');

    const rows = this.db.prepare('SELECT * FROM audit_records ORDER BY sequence').all();
    const hashVersions = [...new Set(rows.map((row) => row.hash_version ?? HASH_VERSION_V1))];
    const reportedHashVersion = hashVersions.length ? hashVersions.join('+') : HASH_VERSION;
    let previousHash = null;
    let previousSequence = 0;
    let anchoredCount = null;
    let checkpointMismatch = null;
    let legacyFingerprintCount = 0;
    const failure = (checked, row, reason, checkpointStatus = checkpointMismatch ? 'MISMATCH' : anchoredCount === null ? 'NOT_CHECKED' : 'MATCHED') => ({
      valid: false, local_valid: reason.startsWith('CHECKPOINT_'),
      checked, broken_audit_id: row?.audit_id ?? null,
      first_break_sequence: row?.sequence ?? checkpoint?.sequence ?? null,
      reason, hash_version: reportedHashVersion, hash_versions: hashVersions,
      checkpoint: checkpoint ? {
        status: checkpointStatus, sequence: checkpoint.sequence,
        ...(checkpointStatus === 'MATCHED' ? { anchored_count: anchoredCount } : {}),
        ...(checkpointStatus === 'MISMATCH' ? { reason: checkpointMismatch?.reason ?? reason } : {}),
      } : { status: 'NOT_CHECKED' },
    });
    for (let index = 0; index < rows.length; index += 1) {
      const row = rows[index];
      let record;
      try {
        record = publicRecord(row);
      } catch (error) {
        if (!(error instanceof SyntaxError)) throw error;
        return failure(index, row, 'INVALID_DETAILS_JSON');
      }
      if (!record.details || typeof record.details !== 'object' || Array.isArray(record.details)) {
        return failure(index, row, 'INVALID_DETAILS_JSON');
      }
      if (![HASH_VERSION_V1, HASH_VERSION_V2].includes(record.hash_version)) {
        return failure(index, row, 'UNSUPPORTED_HASH_VERSION');
      }
      if (record.sequence !== previousSequence + 1) return failure(index, row, 'SEQUENCE_GAP');
      if (record.prev_hash !== previousHash) return failure(index, row, 'PREVIOUS_HASH_MISMATCH');
      if (record.record_hash !== recordHash(record)) return failure(index, row, 'RECORD_HASH_MISMATCH');
      if (row.fingerprint_version !== 'sha256-v1' && row.fingerprint_version !== null) {
        return failure(index, row, 'UNSUPPORTED_FINGERPRINT_VERSION');
      }
      if (row.event_fingerprint !== eventFingerprint(record)) {
        return failure(index, row, row.fingerprint_version === null ? 'UNVERIFIABLE_LEGACY_FINGERPRINT' : 'EVENT_FINGERPRINT_MISMATCH');
      }
      if (row.fingerprint_version === null) {
        legacyFingerprintCount += 1;
      }
      if (checkpoint && row.sequence === checkpoint.sequence) {
        if (index + 1 !== checkpoint.count) {
          checkpointMismatch = { row, reason: 'CHECKPOINT_COUNT_MISMATCH' };
        } else if (record.record_hash !== checkpoint.record_hash) {
          checkpointMismatch = { row, reason: 'CHECKPOINT_HASH_MISMATCH' };
        } else {
          anchoredCount = index + 1;
        }
      }
      previousHash = record.record_hash;
      previousSequence = record.sequence;
    }
    // A checkpoint disagreement does not prove that the rest of the local
    // chain is sound. Report it only after scanning all local records.
    if (checkpointMismatch) {
      return failure(rows.length, checkpointMismatch.row, checkpointMismatch.reason, 'MISMATCH');
    }
    if (checkpoint && anchoredCount === null) {
      return failure(rows.length, null, 'CHECKPOINT_RECORD_MISSING', 'MISSING');
    }
    return {
      valid: true, local_valid: true, checked: rows.length,
      broken_audit_id: null, first_break_sequence: null,
      reason: null, hash_version: reportedHashVersion, hash_versions: hashVersions,
      head_sequence: rows.at(-1)?.sequence ?? null, head_hash: previousHash,
      legacy_fingerprint_count: legacyFingerprintCount,
      checkpoint: checkpoint ? {
        status: 'MATCHED', sequence: checkpoint.sequence,
        anchored_count: anchoredCount, unanchored_count: rows.length - anchoredCount,
      } : { status: 'NOT_CHECKED' },
    };
  }

  close() { this.db.close(); }
}
