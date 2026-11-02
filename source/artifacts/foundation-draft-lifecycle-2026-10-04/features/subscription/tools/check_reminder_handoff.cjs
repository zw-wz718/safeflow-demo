'use strict';
// Checks fixed fictional output only; never sends notifications or invokes a network API.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');

const packageRoot = path.resolve(__dirname, '..');
const repositoryRoot = path.resolve(packageRoot, '../../../..');
const reference = 'fixtures/subscription/SafeFlow_SUB-03_统一提醒实现实测_v0.3.json';
const referenceHash = 'ffaf5783482fe8d9dcb0881524535daaad04e5390b8ec54de2a7a36f8ee13c41';
const digest = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
const safe = error => /^([A-Z_]+)(?:\s|$)/.exec(error.message || '')?.[1] || 'REMINDER_CHECK_FAILED';

function options(args) {
  const parsed = {};
  for (let i = 0; i < args.length; i += 2) {
    assert(['--work-dir', '--cargo'].includes(args[i]) && args[i + 1] && !parsed[args[i]], 'INVALID_ARGUMENTS');
    parsed[args[i]] = args[i + 1];
  }
  assert(parsed['--work-dir'], 'WORK_DIRECTORY_REQUIRED');
  return parsed;
}
function inputs() {
  const files = ['Cargo.toml', 'Cargo.lock'];
  function visit(relative) {
    for (const entry of fs.readdirSync(path.join(packageRoot, relative), { withFileTypes: true })) {
      const name = path.posix.join(relative, entry.name);
      assert(!entry.isSymbolicLink(), 'SYMLINK_INPUT_REFUSED');
      if (entry.isDirectory()) visit(name);
      else if (entry.isFile() && name.endsWith('.rs')) files.push(name);
    }
  }
  for (const name of ['src', 'examples', 'tests']) visit(name);
  return files.map(name => path.join(packageRoot, name)).concat(
    path.join(repositoryRoot, reference),
    path.join(repositoryRoot, 'fixtures/subscription/billing-consumption-1044e918.v1.json'),
    __filename,
  ).sort();
}
const hashes = files => files.map(file => ({
  path: path.relative(repositoryRoot, file).split(path.sep).join('/'),
  sha256: digest(fs.readFileSync(file)),
}));
function verifyMockClaims(value) {
  if (!value || typeof value !== 'object') return;
  for (const [key, child] of Object.entries(value)) {
    if (['production_registered', 'security_kernel_connected', 'database_connected',
      'audit_integrated', 'merchant_action_executed', 'notification_sent'].includes(key)) {
      assert.equal(child, false, 'PRODUCTION_CLAIM');
    }
    if (key === 'fictional') assert.equal(child, true, 'FICTIONAL_MARKER_MISSING');
    verifyMockClaims(child);
  }
}

function main() {
  const args = options(process.argv.slice(2));
  const workRoot = fs.realpathSync(args['--work-dir']);
  const sourceRoot = fs.realpathSync(repositoryRoot);
  assert(fs.statSync(workRoot).isDirectory(), 'WORK_DIRECTORY_REQUIRED');
  const relative = path.relative(sourceRoot, workRoot);
  assert(relative && (path.isAbsolute(relative) || relative === '..' || relative.startsWith('..' + path.sep)),
    'RESULT_DIRECTORY_INSIDE_SOURCE');
  const text = fs.readFileSync(path.join(repositoryRoot, reference), 'utf8');
  assert.equal(digest(Buffer.from(text.replace(/\r\n/g, '\n'))), referenceHash, 'REVIEWED_REFERENCE_CHANGED');
  const expected = JSON.parse(text).replay;
  assert.equal(expected.case_count, 9, 'REFERENCE_CASE_COUNT_MISMATCH');
  assert.equal(expected.cases.length, 9, 'REFERENCE_CASE_COUNT_MISMATCH');
  verifyMockClaims(expected);
  const files = inputs(), before = hashes(files);
  const directory = fs.mkdtempSync(path.join(workRoot, 'reminder-handoff-'));
  const receiptPath = path.join(directory, 'verification.json');
  const receipt = {
    mode: 'FIXED_FICTIONAL_REMINDER_HANDOFF', started_at: new Date().toISOString(),
    reference, reference_sha256_lf: referenceHash, input_hashes: before,
    production_registered: false, notification_sent: false, formal_integration_accepted: false,
    cargo_tests_executed: false, verified: false, cases: [], rejected_arguments: [],
  };
  const save = () => fs.writeFileSync(receiptPath, JSON.stringify(receipt, null, 2) + '\n');
  try {
    save();
    const target = path.join(directory, 'target');
    const build = spawnSync(args['--cargo'] || 'cargo', [
      'build', '--locked', '--offline', '--manifest-path', path.join(packageRoot, 'Cargo.toml'),
      '--target-dir', target, '--example', 'replay_reminders_mock',
    ], { cwd: directory, env: { ...process.env, CARGO_NET_OFFLINE: 'true', CARGO_INCREMENTAL: '0' },
      windowsHide: true, encoding: 'utf8', timeout: 120000, maxBuffer: 8 * 1024 * 1024 });
    fs.writeFileSync(path.join(directory, 'build.stdout.log'), build.stdout || '');
    fs.writeFileSync(path.join(directory, 'build.stderr.log'), build.stderr || '');
    receipt.build = { exit_code: build.status, error_code: build.error?.code || null };
    assert(!build.error && build.status === 0, 'BUILD_FAILED');
    const binary = path.join(target, 'debug/examples/replay_reminders_mock' + (process.platform === 'win32' ? '.exe' : ''));
    const actual = spawnSync(binary, [], { cwd: directory, windowsHide: true, encoding: 'utf8',
      timeout: 15000, maxBuffer: 8 * 1024 * 1024 });
    assert(!actual.error && actual.status === 0, 'REPLAY_FAILED');
    assert.equal(actual.stderr, '', 'UNEXPECTED_REPLAY_STDERR');
    const replay = JSON.parse(actual.stdout);
    fs.writeFileSync(path.join(directory, 'actual-replay.json'), JSON.stringify(replay, null, 2) + '\n');
    assert.deepEqual(replay, expected, 'REPLAY_DIFFERS_FROM_REVIEWED_REFERENCE');
    assert.equal(new Set(replay.cases.map(item => item.name)).size, 9, 'DUPLICATE_CASE');
    verifyMockClaims(replay);
    for (const item of replay.cases) receipt.cases.push({ name: item.name, verified: true });
    for (const argument of ['--confirmed', '--user-id', '--recipient', '--publish']) {
      const result = spawnSync(binary, [argument, 'fixture-value'], { cwd: directory,
        windowsHide: true, encoding: 'utf8', timeout: 15000 });
      assert(!result.error && result.status === 2, 'EXTERNAL_ARGUMENT_ACCEPTED');
      assert.equal(result.stdout, '', 'PARTIAL_SUCCESS_OUTPUT');
      assert.match(result.stderr, /^FIXED_MOCK_ONLY:/, 'UNEXPECTED_ARGUMENT_ERROR');
      receipt.rejected_arguments.push({ argument, exit_code: result.status, stdout_empty: true });
    }
    assert.deepEqual(hashes(files), before, 'INPUT_CHANGED_DURING_CHECK');
    receipt.sources_unchanged = true;
    receipt.verified = true;
  } catch (error) {
    receipt.error = safe(error);
    process.exitCode = 1;
  } finally {
    receipt.finished_at = new Date().toISOString();
    save();
    console.log(JSON.stringify({ receipt: receiptPath, verified: receipt.verified,
      reminder_cases: receipt.cases.length, rejected_arguments: receipt.rejected_arguments.length,
      cargo_tests_executed: false, error: receipt.error }));
  }
}
try { main(); } catch (error) { console.error(safe(error)); process.exitCode = 1; }
