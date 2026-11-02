"""Build/check the exported source without using production credentials or services."""
from pathlib import Path
import argparse
import json
import os
import subprocess

ROOT = Path(__file__).resolve().parents[1]
BACKEND = ROOT / 'artifacts/foundation-draft-lifecycle-2026-10-04'
parser = argparse.ArgumentParser()
parser.add_argument('action', choices=['check', 'build'])
parser.add_argument('--target-dir', type=Path, required=True)
parser.add_argument('--offline', action='store_true')
args = parser.parse_args()
target = args.target_dir.resolve()
if target.is_relative_to(ROOT):
    parser.error('--target-dir must be outside the source directory')
env = {k: v for k, v in os.environ.items() if not k.upper().startswith(
    ('SAFEFLOW_', 'SF_', 'PG', 'BILLING_', 'AUDIT_')) and k.upper() not in {
    'DATABASE_URL', 'RUSTFLAGS', 'RUSTDOCFLAGS', 'CARGO_ENCODED_RUSTFLAGS',
    'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER'}}
env['CARGO_TARGET_DIR'] = str(target)
env['CARGO_BUILD_JOBS'] = '2'
env['CARGO_PROFILE_DEV_DEBUG'] = '0'
env['CARGO_PROFILE_TEST_DEBUG'] = '0'
for key, rel in json.loads((ROOT / 'package-tools/fixture-closure.json').read_text('utf8'))['fixture_env'].items():
    fixture = ROOT / rel
    if not fixture.is_file():
        raise FileNotFoundError(rel)
    env[key] = str(fixture)
common = ['--locked'] + (['--offline'] if args.offline else [])
commands = (
    [['check', '--workspace', '--all-targets']] if args.action == 'check' else [
        ['build', '-p', 'safeflow-api', '--example', 'bank_site', '--example', 'persistent_subscription_reminders'],
        ['build', '-p', 'safeflow-storage', '--example', 'payment_background_actor'],
    ]
)
for command in commands:
    result = subprocess.run(['cargo', '+1.98.1', *command, *common], cwd=BACKEND, env=env)
    if result.returncode:
        raise SystemExit(result.returncode)
