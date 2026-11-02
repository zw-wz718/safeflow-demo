import { mkdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { AuditStore } from './audit-store.js';
import { recordSimulatedTransfer } from './demo-transfer.js';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const dbPath = resolve(root, 'data/audit-demo.db');
mkdirSync(dirname(dbPath), { recursive: true });
const store = new AuditStore(dbPath);
try {
  const demo = recordSimulatedTransfer(store, process.argv[2] ?? 'success');
  console.log(JSON.stringify({ ...demo, chain: store.verifyChain() }, null, 2));
} finally { store.close(); }
