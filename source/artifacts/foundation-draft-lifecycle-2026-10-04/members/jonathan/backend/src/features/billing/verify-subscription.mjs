// Offline cross-module validation only; never writes to the peer worktree.
import { spawnSync } from 'node:child_process';
import { mkdtempSync, writeFileSync, unlinkSync, rmdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, resolve, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const here = dirname(fileURLToPath(import.meta.url));
const peer = resolve(process.argv[2] ?? (() => { throw Error('Provide a clean peer worktree at a76aa85'); })());
const binary = resolve(process.argv[3] ?? join(here, 'target/debug/billing-consumption.exe'));
const fixture = resolve(here, '../../../../fixtures/billing/consumption_pages.v1.json');
const expectedPeer = 'a76aa854ac13093d6bcf6d45e40457acb11f15b6';
function run(command, args, env = process.env) {
  const result = spawnSync(command, args, { encoding: 'utf8', windowsHide: true, env, timeout: 60000 });
  if (result.error || result.status !== 0) throw Error(result.error?.message ?? result.stderr.slice(-3000));
  return result.stdout;
}
assert.equal(run('git', ['-C', peer, 'rev-parse', 'HEAD']).trim(), expectedPeer, 'Peer version mismatch');
assert.equal(run('git', ['-C', peer, 'status', '--porcelain', '--untracked-files=no']).trim(), '', 'Peer tracked files changed');
const scratch = mkdtempSync(join(tmpdir(), 'safeflow-consumer-check-'));
const inputPath = join(scratch, 'fictional-consumer-input.json');
const cargoEnv = { ...process.env, CARGO_TARGET_DIR: join(here, 'target/peer-check') };
function billing(name) {
  const result = JSON.parse(run(binary, ['--fixture', fixture, '--case', name]));
  assert.equal(result.fictional, true);
  assert.equal(result.production_registered, false);
  return result.cases[0];
}
function recognize(name, simulatedLedger = false) {
  const output = billing(name);
  assert.ok(output.subscription_snapshot);
  const snapshot = output.subscription_snapshot;
  if (simulatedLedger) {
    // Explicit test fixture ONLY. The shipping reader never makes this assertion.
    snapshot.managed_subscriptions_read = { state: 'COMPLETE', is_complete: true, next_cursor: null, reason: null };
  }
  writeFileSync(inputPath, JSON.stringify({ schema_version: 1, fictional: true,
    production_registration: false, rule_version: null, sample_count: 1,
    samples: [{ sample_id: name, trusted_context_fixture: { user_id: 'user_demo',
      requested_account_ids: ['account_001'] }, snapshot }] }));
  const result = JSON.parse(run('cargo', ['run', '--locked', '--offline', '--quiet',
    '--manifest-path', join(peer, 'backend/src/features/subscription/Cargo.toml'),
    '--example', 'recognize_mock', '--', inputPath], cargoEnv));
  return result.reports[0].report;
}
try {
  const blocked = recognize('complete');
  assert.equal(blocked.filter_report.outcome, 'LEDGER_UNAVAILABLE');
  assert.equal(blocked.recognition_executed, false);
  const complete = recognize('complete', true);
  assert.equal(complete.status, 'REVIEWED');
  assert.equal(complete.groups.length, 1);
  assert.equal(complete.groups[0].status, 'SUSPECTED');
  assert.equal(complete.groups[0].actual_charges.length, 2);
  const different = recognize('same-merchant-different-services', true);
  assert.equal(different.groups.length, 2);
  assert.ok(different.groups.every(group => group.status === 'INSUFFICIENT_EVIDENCE'));
  const unknown = recognize('same-merchant-unknown-service', true);
  assert.equal(unknown.groups.length, 1);
  assert.equal(unknown.groups[0].status, 'NEEDS_SERVICE_EVIDENCE');
  const partial = billing('second-page-failed');
  assert.equal(partial.subscription_snapshot, null);
  assert.equal(partial.read.eligible_consumption.length, 0);
  console.log(JSON.stringify({ checks: 5, peer_commit: expectedPeer, production_integration: false,
    source: 'explicitly fictional', unknown_ledger_blocks_recognition: true,
    explicit_test_ledger_used_for_three_checks: true }));
} finally {
  // Only the known file and empty directory created by this invocation.
  try { unlinkSync(inputPath); } catch (error) { if (error.code !== 'ENOENT') throw error; }
  rmdirSync(scratch);
}
