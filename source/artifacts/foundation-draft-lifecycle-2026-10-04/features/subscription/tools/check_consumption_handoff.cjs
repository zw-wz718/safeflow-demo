'use strict';
// Replays the already reviewed 20 CLI checks. No network or business writes.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');

const packageRoot = path.resolve(__dirname, '..');
const repositoryRoot = path.resolve(packageRoot, '../../../..');
const pinPath = path.join(packageRoot, 'compatibility/billing-query-source-pin.v1.json');
const expectedPath = path.join(repositoryRoot, 'fixtures/subscription/SafeFlow_SUB-01_消费引用冲突实测_v0.1.json');
const sha256 = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
const safe = error => /^[A-Z_]+$/.test(error.message || '') ? error.message : 'HANDOFF_CHECK_FAILED';

function options(args) {
  const parsed = {};
  for (let i = 0; i < args.length; i += 2) {
    assert(['--work-dir', '--cargo'].includes(args[i]) && args[i + 1]
      && !parsed[args[i]], 'INVALID_ARGUMENTS');
    parsed[args[i]] = args[i + 1];
  }
  assert(parsed['--work-dir'], 'WORK_DIRECTORY_REQUIRED');
  return parsed;
}

function packageInputs() {
  const inputs = ['Cargo.toml', 'Cargo.lock', 'compatibility/billing-query-source-pin.v1.json'];
  function walk(relative) {
    for (const entry of fs.readdirSync(path.join(packageRoot, relative), { withFileTypes: true })) {
      const name = path.posix.join(relative, entry.name);
      if (entry.isDirectory()) walk(name);
      else if (entry.isFile() && name.endsWith('.rs')) inputs.push(name);
    }
  }
  for (const directory of ['src', 'examples', 'tests']) walk(directory);
  return inputs.map(name => path.join(packageRoot, name)).concat(expectedPath, __filename).sort();
}

function sourceHashes(paths) {
  return paths.map(file => ({
    path: path.relative(repositoryRoot, file).split(path.sep).join('/'),
    sha256: sha256(fs.readFileSync(file)),
  }));
}

function inputFor(name, capture) {
  const input = structuredClone(capture);
  if (name.startsWith('original/')) return { input, caseName: name.slice('original/'.length) };
  const complete = input.cases.find(c => c.name === 'complete');
  assert(complete, 'COMPLETE_CASE_MISSING');
  const rows = complete.read.observed_transactions;
  const values = { different: 'fictional-other-source', null: null, 'wrong-type': 42 };
  if (name.startsWith('source-conflict/')) {
    const variant = name.slice('source-conflict/'.length);
    assert(Object.hasOwn(values, variant), 'UNKNOWN_CHECK');
    rows.at(-1).source_id = values[variant];
  } else if (name === 'service-conflict/missing-upstream-reference') {
    delete rows[0].service_ref;
    rows[0].mock_service_ref = 'fictional-injected-service';
  } else if (name.startsWith('service-conflict/')) {
    const variant = name.slice('service-conflict/'.length);
    assert(Object.hasOwn(values, variant), 'UNKNOWN_CHECK');
    rows[0].mock_service_ref = variant === 'different' ? 'fictional-other-service' : values[variant];
  } else if (name === 'consistent-redundant-references') {
    for (const row of rows) {
      row.source_id = complete.read.source_id;
      row.mock_service_ref = row.service_ref ?? null;
    }
  } else throw new Error('UNKNOWN_CHECK');
  return { input, caseName: 'complete' };
}

function main() {
  const args = options(process.argv.slice(2));
  const workRoot = fs.realpathSync(args['--work-dir']);
  const sourceRoot = fs.realpathSync(repositoryRoot);
  const relative = path.relative(sourceRoot, workRoot);
  assert(relative !== '' && (path.isAbsolute(relative) || relative === '..'
    || relative.startsWith('..' + path.sep)), 'RESULT_DIRECTORY_INSIDE_SOURCE');
  const pin = JSON.parse(fs.readFileSync(pinPath, 'utf8'));
  assert.equal(pin.commit, '1044e918d7d58301e0ab49aa03016f035642245f', 'UNEXPECTED_BILLING_PIN');
  assert.equal(pin.capture, 'fixtures/subscription/billing-consumption-1044e918.v1.json', 'UNEXPECTED_CAPTURE_PATH');
  const capturePath = path.join(repositoryRoot, pin.capture);
  const captureText = fs.readFileSync(capturePath, 'utf8');
  assert.equal(sha256(Buffer.from(captureText.replace(/\r\n/g, '\n'))), pin.capture_sha256_lf, 'PINNED_CAPTURE_CHANGED');
  const capture = JSON.parse(captureText);
  const expected = JSON.parse(fs.readFileSync(expectedPath, 'utf8'));
  assert(expected.verified === true && expected.fictional === true
    && expected.production_registered === false && expected.formal_integration_accepted === false,
  'UNEXPECTED_REFERENCE_RESULTS');
  assert.equal(expected.cli_checks.length, 20, 'UNEXPECTED_CHECK_COUNT');
  assert.equal(new Set(expected.cli_checks.map(c => c.name)).size, 20, 'DUPLICATE_CHECK_NAME');
  const inputs = packageInputs().concat(capturePath);
  const before = sourceHashes(inputs);
  const runRoot = fs.mkdtempSync(path.join(workRoot, 'consumption-handoff-'));
  const receiptPath = path.join(runRoot, 'verification.json');
  const receipt = {
    mode: 'OFFLINE_FICTIONAL_CONSUMPTION_CLI_CHECKS', started_at: new Date().toISOString(),
    billing_commit: pin.commit, production_registered: false, formal_integration_accepted: false,
    cargo_tests_executed: false, verified: false, checks: [], input_hashes: before,
  };
  const save = () => fs.writeFileSync(receiptPath, JSON.stringify(receipt, null, 2) + '\n');
  const target = path.join(runRoot, 'target');
  const env = { ...process.env, CARGO_NET_OFFLINE: 'true', CARGO_INCREMENTAL: '0' };
  try {
    save();
    const build = spawnSync(args['--cargo'] || 'cargo', [
      'build', '--locked', '--offline', '--manifest-path', path.join(packageRoot, 'Cargo.toml'),
      '--target-dir', target, '--example', 'inspect_billing_query',
    ], { cwd: runRoot, env, windowsHide: true, encoding: 'utf8', timeout: 120000, maxBuffer: 8 * 1024 * 1024 });
    fs.writeFileSync(path.join(runRoot, 'build.stdout.log'), build.stdout || '');
    fs.writeFileSync(path.join(runRoot, 'build.stderr.log'), build.stderr || '');
    receipt.build = { exit_code: build.status, error_code: build.error?.code || null };
    assert(!build.error && build.status === 0, 'CLI_BUILD_FAILED');
    const binary = path.join(target, 'debug/examples/inspect_billing_query' + (process.platform === 'win32' ? '.exe' : ''));
    for (const check of expected.cli_checks) {
      const { input, caseName } = inputFor(check.name, capture);
      const filename = path.join(runRoot, '消费输入 ' + receipt.checks.length + '.json');
      fs.writeFileSync(filename, JSON.stringify(input));
      const actual = spawnSync(binary, [filename, caseName], {
        cwd: runRoot, env, windowsHide: true, encoding: 'utf8', timeout: 15000, maxBuffer: 8 * 1024 * 1024,
      });
      assert(!actual.error, 'CLI_EXECUTION_FAILED');
      assert.equal(actual.status, check.exit, 'CLI_EXIT_MISMATCH');
      if (check.exit !== 0) {
        assert.equal(actual.stdout, '', 'PARTIAL_SUCCESS_OUTPUT');
        assert.equal(actual.stderr.trim(), check.error, 'CLI_ERROR_MISMATCH');
      } else {
        assert.equal(actual.stderr, '', 'UNEXPECTED_CLI_STDERR');
        const report = JSON.parse(actual.stdout);
        assert.equal(report.status, 'EVIDENCE_BLOCKED', 'UNEXPECTED_RECOGNITION_STATUS');
        assert.equal(report.filter_report.outcome, check.outcome, 'UNEXPECTED_LEDGER_OUTCOME');
        assert.equal(report.recognition_executed, false, 'RECOGNITION_GATE_BYPASSED');
        assert.deepEqual(report.groups, [], 'UNEXPECTED_SUSPECTED_GROUPS');
        assert.equal(report.production_registered, false, 'PRODUCTION_CLAIM');
        assert.equal(report.team_contract_approved, false, 'CONTRACT_APPROVAL_CLAIM');
      }
      receipt.checks.push({ name: check.name, verified: true, exit_code: actual.status });
      save();
    }
    assert.deepEqual(sourceHashes(inputs), before, 'INPUT_CHANGED_DURING_CHECK');
    receipt.sources_unchanged = true;
    receipt.verified = true;
  } catch (error) {
    receipt.error = safe(error);
    process.exitCode = 1;
  } finally {
    receipt.finished_at = new Date().toISOString();
    save();
    console.log(JSON.stringify({ receipt: receiptPath, verified: receipt.verified,
      checks_passed: receipt.checks.length, error: receipt.error }));
  }
}

try { main(); } catch (error) { console.error(safe(error)); process.exitCode = 1; }
