'use strict';
// Local test assembly only. No checkout, source edits, network or business writes.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const { spawnSync } = require('node:child_process');
const assert = require('node:assert/strict');
const fixedCommit = '3beed995bd8ed93517899144c3957d7928da45de';
const subscriptionRoot = path.resolve(__dirname, '..');
const pinFile = path.join(subscriptionRoot, 'compatibility/billing-source-pin.json');
const hash = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
const blobHash = bytes => crypto.createHash('sha1').update(Buffer.from('blob ' + bytes.length + '\0')).update(bytes).digest('hex');
function options(args) {
  const parsed = {};
  const allowed = ['--billing-repository', '--card-package', '--work-dir', '--cargo'];
  for (let i = 0; i < args.length; i += 2) {
    assert(allowed.includes(args[i]) && args[i + 1] && !parsed[args[i]], 'INVALID_ARGUMENTS');
    parsed[args[i]] = args[i + 1];
  }
  for (const key of allowed.slice(0, 3)) assert(parsed[key], 'MISSING_ARGUMENT');
  return parsed;
}
function sourceHashes(root, relativePaths) {
  return relativePaths.map(relative => ({ path: relative, sha256: hash(fs.readFileSync(path.join(root, relative))) }));
}
function packageSources(root) {
  const names = ['Cargo.toml'];
  if (fs.existsSync(path.join(root, 'Cargo.lock'))) names.push('Cargo.lock');
  function walk(relative) {
    for (const entry of fs.readdirSync(path.join(root, relative), { withFileTypes: true })) {
      const next = path.posix.join(relative, entry.name);
      if (entry.isDirectory()) walk(next);
      else if (entry.isFile() && next.endsWith('.rs')) names.push(next);
    }
  }
  walk('src');
  return names.sort();
}
function safe(error) { return /^[A-Z_]+$/.test(error.message || '') ? error.message : 'BOUNDARY_CHECK_FAILED'; }
function main() {
  const args = options(process.argv.slice(2));
  const billingRepository = fs.realpathSync(args['--billing-repository']);
  const billingRoot = path.join(billingRepository, 'backend/src/features/billing');
  const cardRoot = fs.realpathSync(args['--card-package']);
  const workRoot = fs.realpathSync(args['--work-dir']);
  for (const sourceRoot of [billingRepository, cardRoot, subscriptionRoot]) {
    const relative = path.relative(sourceRoot, workRoot);
    const inside = relative === '' || (!path.isAbsolute(relative)
      && relative !== '..' && !relative.startsWith('..' + path.sep));
    assert(!inside, 'RESULT_DIRECTORY_INSIDE_SOURCE');
  }
  const billingPin = JSON.parse(fs.readFileSync(pinFile, 'utf8'));
  assert.equal(billingPin.commit, fixedCommit, 'UNEXPECTED_PIN');
  for (const file of billingPin.files) {
    const bytes = fs.readFileSync(path.join(billingRepository, file.path));
    assert.equal(blobHash(bytes), file.blob_sha1, 'PINNED_BILLING_SOURCE_CHANGED');
  }
  const manifest = p => fs.readFileSync(path.join(p, 'Cargo.toml'), 'utf8');
  assert(manifest(cardRoot).includes('name = "safeflow-card-facts"'), 'UNEXPECTED_CARD_PACKAGE');
  assert(manifest(subscriptionRoot).includes('name = "safeflow-subscription-evidence"'), 'UNEXPECTED_SUBSCRIPTION_PACKAGE');
  const runRoot = fs.mkdtempSync(path.join(workRoot, 'billing-card-boundary-'));
  const receipt = { started_at: new Date().toISOString(), mode: 'ISOLATED_READ_ONLY_BOUNDARY_TESTS',
    billing_commit: fixedCommit, source_blobs_verified: billingPin.files.length,
    recognition_executed: false, production_registered: false, formal_integration_accepted: false,
    steps: [], verified: false };
  const save = () => fs.writeFileSync(path.join(runRoot, 'verification.json'), JSON.stringify(receipt, null, 2) + '\n');
  const before = {
    billing: sourceHashes(billingRepository, billingPin.files.map(f => f.path)),
    card: sourceHashes(cardRoot, packageSources(cardRoot)),
    subscription: sourceHashes(subscriptionRoot, [...packageSources(subscriptionRoot), 'compatibility/billing_card_boundary.rs']),
  };
  receipt.input_hashes = before;
  const fixture = path.join(billingRepository, 'fixtures/billing/upstream/mock_transfer_200.2a72b5d.json');
  const env = { ...process.env, SF_PINNED_TRANSFER_FIXTURE_PATH: fixture,
    CARGO_TARGET_DIR: path.join(runRoot, 'target'), CARGO_NET_OFFLINE: 'true' };
  const cargo = args['--cargo'] || 'cargo';
  function run(name, argv) {
    const r = spawnSync(cargo, argv, { cwd: runRoot, env, windowsHide: true,
      encoding: 'utf8', timeout: 120000, maxBuffer: 8 * 1024 * 1024 });
    fs.writeFileSync(path.join(runRoot, name + '.stdout.log'), r.stdout || '');
    fs.writeFileSync(path.join(runRoot, name + '.stderr.log'), r.stderr || '');
    receipt.steps.push({ name, exit_code: r.status, error_code: r.error?.code || null }); save();
    assert(!r.error && r.status === 0, 'CHECK_STEP_FAILED');
    return r.stdout;
  }
  try {
    const slash = p => p.replaceAll('\\', '/');
    const toml = '[package]\nname = "safeflow-billing-card-boundary-check"\nversion = "0.1.0"\nedition = "2021"\npublish = false\n\n[workspace]\n\n[dependencies]\n'
      + 'safeflow-billing-source-reader = { path = ' + JSON.stringify(slash(billingRoot)) + ' }\n'
      + 'safeflow-card-facts = { path = ' + JSON.stringify(slash(cardRoot)) + ' }\n'
      + 'safeflow-subscription-evidence = { path = ' + JSON.stringify(slash(subscriptionRoot)) + ' }\n'
      + 'serde_json = "1"\n';
    fs.writeFileSync(path.join(runRoot, 'Cargo.toml'), toml);
    fs.mkdirSync(path.join(runRoot, 'src'));
    fs.copyFileSync(path.join(subscriptionRoot, 'compatibility/billing_card_boundary.rs'), path.join(runRoot, 'src/lib.rs'));
    run('lock', ['generate-lockfile', '--offline']);
    run('format', ['fmt', '--', '--check']);
    run('clippy', ['clippy', '--locked', '--offline', '--all-targets', '--', '-D', 'warnings']);
    const tests = run('tests', ['test', '--locked', '--offline']);
    const totals = [...tests.matchAll(/test result: ok\. (\d+) passed; (\d+) failed;/g)];
    assert(totals.length > 0 && totals.every(m => m[2] === '0'), 'TEST_RESULTS_MISSING');
    receipt.boundary_tests_passed = totals.reduce((sum, m) => sum + Number(m[1]), 0);
    assert.equal(receipt.boundary_tests_passed, 10, 'UNEXPECTED_TEST_COUNT');
    const after = {
      billing: sourceHashes(billingRepository, billingPin.files.map(f => f.path)),
      card: sourceHashes(cardRoot, packageSources(cardRoot)),
      subscription: sourceHashes(subscriptionRoot, [...packageSources(subscriptionRoot), 'compatibility/billing_card_boundary.rs']),
    };
    assert.deepEqual(after, before, 'INPUT_CHANGED_DURING_CHECK');
    receipt.sources_unchanged = true;
    receipt.harness_lock_sha256 = hash(fs.readFileSync(path.join(runRoot, 'Cargo.lock')));
    receipt.verified = true;
  } catch (error) { receipt.error = safe(error); process.exitCode = 1; }
  finally { receipt.finished_at = new Date().toISOString(); save(); console.log(JSON.stringify({ receipt: path.join(runRoot, 'verification.json'), verified: receipt.verified, tests_passed: receipt.boundary_tests_passed, error: receipt.error })); }
}
try { main(); } catch (error) { console.error(safe(error)); process.exitCode = 1; }
