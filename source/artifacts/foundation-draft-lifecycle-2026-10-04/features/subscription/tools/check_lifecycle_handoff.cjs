'use strict';
// Runs the reviewed fixed mock only. No network, notification or business writes.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');

const packageRoot = path.resolve(__dirname, '..');
const repositoryRoot = path.resolve(packageRoot, '../../../..');
const reference = 'fixtures/subscription/SafeFlow_SUB-02_完整模拟流程实测_v0.1.json';
const referenceHash = '5b76d6c5d864b50fb6f6e3ab7cec8cf8c0ee3588066dccc65bb015c705a10151';
const capture = 'fixtures/subscription/billing-consumption-1044e918.v1.json';
const digest = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
const safe = error => {
  const code = /^([A-Z_]+)(?:\s|$)/.exec(error.message || '');
  return code ? code[1] : 'LIFECYCLE_CHECK_FAILED';
};

function options(args) {
  const result = {};
  for (let i = 0; i < args.length; i += 2) {
    assert(['--work-dir', '--cargo'].includes(args[i]) && args[i + 1] && !result[args[i]], 'INVALID_ARGUMENTS');
    result[args[i]] = args[i + 1];
  }
  assert(result['--work-dir'], 'WORK_DIRECTORY_REQUIRED');
  return result;
}
function inputs() {
  const files = ['Cargo.toml', 'Cargo.lock'];
  function visit(relative) {
    for (const entry of fs.readdirSync(path.join(packageRoot, relative), { withFileTypes: true })) {
      const next = path.posix.join(relative, entry.name);
      if (entry.isDirectory()) visit(next);
      else if (entry.isFile() && next.endsWith('.rs')) files.push(next);
      else if (entry.isSymbolicLink()) assert.fail('SYMLINK_INPUT_REFUSED');
    }
  }
  visit('src'); visit('examples');
  return files.map(p => path.join(packageRoot, p)).concat(path.join(repositoryRoot, reference), path.join(repositoryRoot, capture), __filename).sort();
}
const hashes = files => files.map(file => ({ path: path.relative(repositoryRoot, file).split(path.sep).join('/'), sha256: digest(fs.readFileSync(file)) }));

function main() {
  const args = options(process.argv.slice(2));
  const sourceRoot = fs.realpathSync(repositoryRoot);
  const workRoot = fs.realpathSync(path.resolve(args['--work-dir']));
  assert(fs.statSync(workRoot).isDirectory(), 'WORK_DIRECTORY_REQUIRED');
  const relative = path.relative(sourceRoot, workRoot);
  assert(relative && (path.isAbsolute(relative) || relative === '..' || relative.startsWith('..' + path.sep)), 'RESULT_DIRECTORY_INSIDE_SOURCE');
  const text = fs.readFileSync(path.join(repositoryRoot, reference), 'utf8');
  assert.equal(digest(Buffer.from(text.replace(/\r\n/g, '\n'))), referenceHash, 'REVIEWED_REFERENCE_CHANGED');
  const expected = JSON.parse(text).replay;
  const files = inputs(), before = hashes(files);
  const directory = fs.mkdtempSync(path.join(workRoot, 'lifecycle-handoff-'));
  const receiptPath = path.join(directory, 'verification.json');
  const receipt = { started_at: new Date().toISOString(), mode: 'FIXED_FICTIONAL_LIFECYCLE_HANDOFF',
    production_registered: false, audit_integrated: false, notification_sent: false, cargo_tests_executed: false,
    reference, reference_sha256_lf: referenceHash, input_hashes: before, verified: false, steps: [], rejected_arguments: [] };
  const save = () => fs.writeFileSync(receiptPath, JSON.stringify(receipt, null, 2) + '\n');
  const env = { ...process.env, CARGO_NET_OFFLINE: 'true', CARGO_INCREMENTAL: '0' };
  try {
    save();
    const target = path.join(directory, 'target');
    const build = spawnSync(args['--cargo'] || 'cargo', ['build', '--locked', '--offline', '--manifest-path',
      path.join(packageRoot, 'Cargo.toml'), '--target-dir', target, '--example', 'replay_lifecycle_mock'],
    { cwd: directory, env, windowsHide: true, encoding: 'utf8', timeout: 120000, maxBuffer: 8 * 1024 * 1024 });
    fs.writeFileSync(path.join(directory, 'build.stdout.log'), build.stdout || '');
    fs.writeFileSync(path.join(directory, 'build.stderr.log'), build.stderr || '');
    receipt.build = { exit_code: build.status, error_code: build.error?.code || null };
    assert(!build.error && build.status === 0, 'BUILD_FAILED');
    const binary = path.join(target, 'debug/examples/replay_lifecycle_mock' + (process.platform === 'win32' ? '.exe' : ''));
    const actual = spawnSync(binary, [], { cwd: directory, env, windowsHide: true, encoding: 'utf8', timeout: 15000, maxBuffer: 8 * 1024 * 1024 });
    assert(!actual.error && actual.status === 0, 'REPLAY_FAILED');
    assert.equal(actual.stderr, '', 'UNEXPECTED_REPLAY_STDERR');
    const replay = JSON.parse(actual.stdout);
    fs.writeFileSync(path.join(directory, 'actual-replay.json'), JSON.stringify(replay, null, 2) + '\n');
    assert.deepEqual(replay, expected, 'REPLAY_DIFFERS_FROM_REVIEWED_REFERENCE');
    assert.equal(replay.fictional, true, 'FICTIONAL_MARKER_MISSING');
    for (const key of ['production_registered', 'security_kernel_connected', 'database_connected', 'audit_integrated', 'merchant_action_executed', 'notification_sent']) assert.equal(replay[key], false, 'PRODUCTION_CLAIM');
    assert.equal(replay.step_count, 12, 'STEP_COUNT_MISMATCH');
    assert.equal(replay.steps.length, 12, 'STEP_COUNT_MISMATCH');
    assert.equal(new Set(replay.steps.map(s => s.step)).size, 12, 'DUPLICATE_STEP');
    for (const step of replay.steps) receipt.steps.push({ name: step.step, verified: true });
    for (const argument of ['--confirmed', '--user-id', '--publish']) {
      const result = spawnSync(binary, [argument], { cwd: directory, env, windowsHide: true, encoding: 'utf8', timeout: 15000 });
      assert(!result.error && result.status === 2, 'EXTERNAL_ARGUMENT_ACCEPTED');
      assert.equal(result.stdout, '', 'PARTIAL_SUCCESS_OUTPUT');
      assert.match(result.stderr, /^FIXED_MOCK_ONLY:/, 'UNEXPECTED_ARGUMENT_ERROR');
      receipt.rejected_arguments.push(argument);
    }
    assert.deepEqual(hashes(files), before, 'INPUT_CHANGED_DURING_CHECK');
    receipt.sources_unchanged = true;
    receipt.verified = true;
  } catch (error) {
    receipt.error = safe(error);
    process.exitCode = 1;
  } finally {
    receipt.finished_at = new Date().toISOString(); save();
    console.log(JSON.stringify({ verified: receipt.verified, lifecycle_steps: receipt.steps.length,
      rejected_arguments: receipt.rejected_arguments.length, cargo_tests_executed: false, receipt: receiptPath, error: receipt.error }));
  }
}
try { main(); } catch (error) { console.error(safe(error)); process.exitCode = 1; }
