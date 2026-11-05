#!/usr/bin/env python3
"""Run local gates sequentially and retain bounded, credential-free evidence."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import shutil
import subprocess
import sys
import time
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def write_report(path, report):
    """Replace complete reports atomically; an interrupted write preserves the old one."""
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode='w', dir=path.parent, delete=False) as output:
            temporary = Path(output.name)
            json.dump(report, output, indent=2, allow_nan=False)
            output.write('\n')
            output.flush()
            os.fsync(output.fileno())
        temporary.replace(path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def git(*args):
    return subprocess.check_output(['git', *args], cwd=ROOT).decode().strip()


def identity():
    # Include uncommitted source without copying source or secrets to the report.
    digest = hashlib.sha256()
    paths = subprocess.check_output(['git', 'ls-files', '-z', '--cached', '--others', '--exclude-standard'], cwd=ROOT)
    for name in sorted(set(paths.split(b'\0')) - {b''}):
        path = ROOT / os.fsdecode(name)
        digest.update(name + b'\0')
        if path.is_symlink():
            digest.update(os.readlink(path).encode())
        elif path.is_file():
            with path.open('rb') as source:
                for chunk in iter(lambda: source.read(1024 * 1024), b''):
                    digest.update(chunk)
        else:
            digest.update(b'<deleted>')
    return {'revision': git('rev-parse', 'HEAD'), 'dirty': bool(git('status', '--porcelain')),
            'sourceSha256': digest.hexdigest()}


def gates(profile):
    pnpm = ['pnpm'] if shutil.which('pnpm') else ['corepack', 'pnpm']
    checks = [
        ('operator-tooling', ['python3', '-m', 'unittest', 'discover', '-s', 'self-hosting', '-p', 'test_local_*.py'], '.', 120),
        ('recovery-contract', ['node', '--test', 'self-hosting/recovery-drill.test.mjs'], '.', 120),
        ('release-signature-contract', ['node', '--test', 'self-hosting/verify-release-images.test.mjs'], '.', 120),
        ('abuse-drill-contract', ['node', '--test', 'self-hosting/drills/abuse/abuse-model.test.mjs'], '.', 120),
        ('rust-format', ['cargo', 'fmt', '--all', '--', '--check'], 'server-rs', 120),
        ('vendored-cel', ['cargo', 'test', '--release', '--locked', '--manifest-path',
                          'vendor/cel/Cargo.toml', '--features', 'json'], 'server-rs', 900),
    ]
    if profile == 'local':
        checks += [
            ('permissions', ['cargo', 'test', '--release', '--locked', '-p', 'instant-permissions', '--lib'], 'server-rs', 900),
            ('realtime-cache', ['cargo', 'test', '--release', '--locked', '-p', 'instant-realtime', '--lib'], 'server-rs', 900),
            ('server-realtime', ['cargo', 'test', '--release', '--locked', '-p', 'instant-server', '--lib', 'runtime_realtime::tests'], 'server-rs', 900),
            ('private-metrics', ['cargo', 'test', '--release', '--locked', '-p', 'instant-server', '--lib', 'metrics::tests'], 'server-rs', 900),
            ('logging', ['cargo', 'test', '--release', '--locked', '-p', 'instant-server', '--lib', 'telemetry::tests'], 'server-rs', 900),
            ('http', ['cargo', 'test', '--release', '--locked', '-p', 'instant-http', '--lib'], 'server-rs', 900),
            ('dashboard-types', [*pnpm, 'check'], 'client/www', 300),
            ('dashboard-models', [*pnpm, 'test'], 'client/www', 300),
            ('sdk-core-contracts', [*pnpm, 'exec', 'vitest', 'run', '--project', 'node'], 'client/packages/core', 300),
            ('sdk-core-types', [*pnpm, 'run', 'test:types'], 'client/packages/core', 300),
            ('mobile-dependency-contracts', ['node', 'scripts/mobile-dependency-contracts.test.cjs'], 'client', 120),
        ]
    return checks


def run_check(argv, cwd, timeout, env):
    start = time.monotonic()
    # Output is intentionally discarded: arbitrary tools can print credentials.
    # A failed gate is rerun directly for diagnosis, never persisted wholesale.
    try:
        child = subprocess.Popen(argv, cwd=cwd, env=env, stdout=subprocess.DEVNULL,
                                 stderr=subprocess.DEVNULL, start_new_session=True)
    except OSError:
        return {'status': 'blocked', 'reason': 'command unavailable', 'durationSeconds': 0}
    try:
        code = child.wait(timeout=timeout)
        return {'status': 'passed' if code == 0 else 'failed', 'exitCode': code,
                'durationSeconds': round(time.monotonic() - start, 3)}
    except (subprocess.TimeoutExpired, KeyboardInterrupt):
        os.killpg(child.pid, signal.SIGTERM)
        try:
            child.wait(timeout=15)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait()
        return {'status': 'failed', 'reason': 'timeout or interrupted',
                'durationSeconds': round(time.monotonic() - start, 3)}


def overall(checks):
    if not checks:
        return 'incomplete'
    if any(c['status'] == 'failed' for c in checks):
        return 'failed'
    if any(c['status'] != 'passed' for c in checks):
        return 'incomplete'
    return 'passed'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--profile', choices=['quick', 'local'], default='quick')
    parser.add_argument('--evidence-dir', type=Path, help='Create a new private directory for this verification report')
    parser.add_argument('--output', type=Path, default=ROOT / 'server-rs/target/verification/latest.json')
    args = parser.parse_args()
    before = identity()
    if args.evidence_dir:
        if args.evidence_dir.resolve().is_relative_to(ROOT):
            parser.error('--evidence-dir must be outside the source checkout')
        args.evidence_dir.mkdir(parents=True, exist_ok=False, mode=0o700)
        args.output = args.evidence_dir / 'verification.json'
    report = {'schemaVersion': 2, 'profile': args.profile, 'source': before,
              'startedAt': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()), 'checks': [],
              'limits': ['A local pass does not establish production capacity or inbox delivery.',
                         'Database-backed and signed-image acceptance are not part of this verification.',
                         'Backup restore and 5000-user acceptance are verified separately.']}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    selected_gates = gates(args.profile)
    report['checks'] = [{'name': name, 'status': 'pending'} for name, *_ in selected_gates]
    report['status'] = 'running'
    write_report(args.output, report)
    for index, (name, argv, directory, timeout) in enumerate(selected_gates):
        print(f'Running {name}', flush=True)
        env = dict(os.environ, CARGO_BUILD_JOBS='6')
        result = run_check(argv, ROOT / directory, timeout, env)
        report['checks'][index] = {'name': name, **result}
        print(f'{name}: {result["status"]}', flush=True)
        report['status'] = 'running'
        write_report(args.output, report)
        if result.get('reason') == 'timeout or interrupted':
            break
    if identity() != before:
        report['checks'].append({'name': 'source-stability', 'status': 'failed', 'reason': 'source changed during verification'})
    else:
        report['checks'].append({'name': 'source-stability', 'status': 'passed'})
    report['profileStatus'] = overall([c for c in report['checks'] if c['status'] != 'skipped'])
    report['status'] = overall(report['checks'])
    write_report(args.output, report)
    print(f'Profile: {report["profileStatus"]}; report: {args.output}')
    return 0 if report['status'] == 'passed' else 1


if __name__ == '__main__':
    sys.exit(main())
