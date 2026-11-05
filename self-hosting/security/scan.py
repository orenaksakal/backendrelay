#!/usr/bin/env python3
"""Pinned local security scans and fail-closed, candidate-bound evidence validation."""
import argparse
from datetime import datetime, timedelta, timezone
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
POLICY = json.loads((HERE / 'policy.json').read_text())
DIGEST = re.compile(r'^[a-zA-Z0-9._:/-]+@sha256:[a-f0-9]{64}$')


def timestamp():
    return datetime.now(timezone.utc).isoformat()


def sha256(path):
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def identity():
    spec = importlib.util.spec_from_file_location('local_verifier', HERE.parent / 'verify-local.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.identity()


def requirements(images):
    checks = [('rust:' + path, 'cargo-audit', path) for path in POLICY['rustLocks']]
    checks += [('pnpm:' + path, 'pnpm', path) for path in POLICY['pnpmDirectories']]
    checks += [('bun:' + path, 'bun', path) for path in POLICY['bunDirectories']]
    checks += [('secrets:history', 'gitleaks', 'history'), ('secrets:working-tree', 'gitleaks', 'working-tree')]
    checks += [('image:' + role, 'trivy', images.get(role, '')) for role in POLICY['requiredImageRoles']]
    return checks


def recent(value, now, maximum_hours):
    try:
        parsed = datetime.fromisoformat(value.replace('Z', '+00:00'))
        return parsed.tzinfo is not None and timedelta(0) <= now - parsed <= timedelta(hours=maximum_hours)
    except (AttributeError, TypeError, ValueError):
        return False


def clean_report(tool, report, target):
    """Exit zero alone is insufficient: reject empty/error/filtered reports."""
    if tool == 'gitleaks':
        return report == []
    if not isinstance(report, dict):
        return False
    if tool == 'cargo-audit':
        vulnerabilities = report.get('vulnerabilities', {})
        return (vulnerabilities.get('found') is False and vulnerabilities.get('count') == 0
                and vulnerabilities.get('list') == [] and bool(report.get('database')))
    if tool == 'pnpm':
        counts = report.get('metadata', {}).get('vulnerabilities', {})
        return (bool(counts) and all(type(v) is int and v == 0 for v in counts.values())
                and not report.get('advisories') and not report.get('error'))
    if tool == 'bun':
        # Bun --json exposes the raw npm bulk advisory map; an empty map is a
        # clean response. Any advisory, error, or skipped-package entry fails.
        return report == {}
    if tool == 'trivy':
        return (report.get('ArtifactName') == target and report.get('ArtifactType') == 'container_image'
                and report.get('SchemaVersion') == 2 and bool(report.get('Metadata'))
                and isinstance(report.get('Results'), list) and bool(report['Results'])
                and all(not result.get('Vulnerabilities') for result in report['Results']))
    return False


def validate_manifest(path, source, images, now=None):
    """Return issues; [] means all required current candidate scans passed.

    The release verifier may import this directly. This validates evidence
    integrity, not its authorship; review/sign the manifest with release evidence.
    """
    now = now or datetime.now(timezone.utc)
    issues = []
    try:
        manifest = json.loads(path.read_text())
        if manifest.get('schemaVersion') != 1 or manifest.get('kind') != 'backendrelay-security-scans':
            issues.append('unsupported security evidence schema')
        if manifest.get('source') != source or manifest.get('images') != images:
            issues.append('security evidence does not identify the candidate source/images')
        if manifest.get('policySha256') != sha256(HERE / 'policy.json'):
            issues.append('security scan policy changed')
        if not recent(manifest.get('completedAt'), now, POLICY['maxEvidenceAgeHours']):
            issues.append('security evidence is stale, future-dated, or undated')
        if manifest.get('status') != 'passed':
            issues.append('security scans did not pass')
        rows = manifest.get('scans', [])
        checks = {row['name']: row for row in rows}
        expected = requirements(images)
        if len(checks) != len(rows) or set(checks) != {name for name, _, _ in expected}:
            issues.append('required scans are missing, duplicated, or unexpected')
        for name, tool, target in expected:
            row = checks.get(name, {})
            if row.get('tool') != tool or row.get('version') != POLICY['tools'][tool] or row.get('target') != target:
                issues.append(name + ': tool version or target mismatch')
            if row.get('status') != 'passed' or row.get('exitCode') != 0:
                issues.append(name + ': not a successful scan')
            if not recent(row.get('completedAt'), now, POLICY['maxEvidenceAgeHours']):
                issues.append(name + ': stale or absent completion time')
            if tool in ('cargo-audit', 'pnpm', 'bun', 'trivy') and not recent(
                    row.get('databaseCheckedAt'), now, POLICY['maxDatabaseAgeHours']):
                issues.append(name + ': absent or stale vulnerability database evidence')
            artifact = row.get('artifact', {})
            artifact_name = artifact.get('path', '')
            file = (path.parent / artifact_name).resolve()
            if not artifact_name or not file.is_relative_to(path.parent.resolve()) or not file.is_file():
                issues.append(name + ': scan artifact unavailable or outside evidence directory')
            elif sha256(file) != artifact.get('sha256'):
                issues.append(name + ': artifact checksum mismatch')
            elif not clean_report(tool, json.loads(file.read_text()), target):
                issues.append(name + ': report contains findings or is incomplete')
        if set(images) != set(POLICY['requiredImageRoles']) or any(not DIGEST.fullmatch(v) for v in images.values()):
            issues.append('all required candidate and rollback images must be digest-pinned')
    except (OSError, ValueError, TypeError, KeyError, AttributeError):
        issues.append('malformed or unavailable security evidence')
    return issues


def run(argv, cwd=ROOT, timeout=900):
    # Scanner diagnostics may contain source or credentials. Only explicit
    # JSON reports (redacted for secret scans) are retained, never stderr.
    try:
        env = {key: value for key, value in os.environ.items() if not key.startswith('TRIVY_')}
        if argv[0] == 'pnpm':
            # Both Corepack and pnpm can otherwise replace the verified scanner
            # with a project's packageManager version when the cwd changes.
            env['COREPACK_ENABLE_PROJECT_SPEC'] = '0'
            env['npm_config_manage_package_manager_versions'] = 'false'
        result = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, timeout=timeout)
        return result.returncode, result.stdout
    except (OSError, subprocess.SubprocessError):
        return None, b''


def tool_version(tool):
    argv = ['cargo', 'audit', '--version'] if tool == 'cargo-audit' else [tool, '--version']
    if tool == 'gitleaks':
        argv = ['gitleaks', 'version']
    code, output = run(argv, timeout=20)
    match = re.search(rb'(?<!\d)(\d+\.\d+\.\d+)(?!\d)', output)
    return match[1].decode() if code == 0 and match else None


def scan(images, output):
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    source = identity()
    manifest = {'schemaVersion': 1, 'kind': 'backendrelay-security-scans', 'source': source,
                'images': images, 'policySha256': sha256(HERE / 'policy.json'),
                'startedAt': timestamp(), 'status': 'incomplete', 'scans': []}
    versions = {tool: tool_version(tool) for tool in POLICY['tools']}
    with tempfile.TemporaryDirectory(prefix='backendrelay-secret-snapshot-') as temporary:
        snapshot = Path(temporary)
        tracked = subprocess.check_output(['git', 'ls-files', '-z', '--cached', '--others', '--exclude-standard'], cwd=ROOT)
        for entry in set(tracked.split(b'\0')) - {b''}:
            relative = Path(os.fsdecode(entry))
            file = ROOT / relative
            if not file.is_file() or file.is_symlink():
                continue
            destination = snapshot / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(file, destination)
        for index, (name, tool, target) in enumerate(requirements(images)):
            row = {'name': name, 'tool': tool, 'version': versions[tool], 'target': target,
                   'status': 'blocked', 'completedAt': timestamp()}
            report = output / f'{index:02d}.json'
            if versions[tool] != POLICY['tools'][tool]:
                row['reason'] = 'required pinned scanner unavailable'
            elif tool == 'trivy' and not DIGEST.fullmatch(target):
                row['reason'] = 'required digest-pinned image missing'
            else:
                cwd = ROOT
                if tool == 'cargo-audit':
                    # Default online database refresh is required. No --no-fetch.
                    argv = ['cargo', 'audit', '--json', '--file', target]
                elif tool in ('pnpm', 'bun'):
                    argv = [tool, 'audit', '--json', '--audit-level=low']
                    cwd = ROOT / target
                elif tool == 'gitleaks':
                    argv = ['gitleaks', 'git' if target == 'history' else 'dir',
                            str(ROOT if target == 'history' else snapshot), '--redact=100',
                            '--report-format=json', '--report-path', str(report), '--no-banner']
                else:
                    argv = ['trivy', 'image', '--config', str(HERE / 'trivy.yaml'),
                            '--scanners=vuln', '--severity=UNKNOWN,LOW,MEDIUM,HIGH,CRITICAL',
                            '--ignorefile', str(HERE / 'empty.trivyignore'), '--exit-code=1', '--format=json', target]
                code, payload = run(argv, cwd)
                row['exitCode'] = code
                if tool != 'gitleaks' and payload:
                    report.write_bytes(payload)
                if report.exists():
                    report.chmod(0o600)
                    row['artifact'] = {'path': report.name, 'sha256': sha256(report)}
                valid_json = False
                try:
                    parsed = json.loads(report.read_bytes())
                    valid_json = clean_report(tool, parsed, target)
                except (OSError, ValueError):
                    pass
                row['status'] = 'passed' if code == 0 and valid_json else ('blocked' if code is None else 'failed')
                row['completedAt'] = timestamp()
                if tool in ('cargo-audit', 'pnpm', 'bun') and row['status'] == 'passed':
                    # These commands refresh from the advisory service by default
                    # and a failed online request fails the command.
                    row['databaseCheckedAt'] = row['completedAt']
                elif tool == 'trivy' and row['status'] == 'passed':
                    code, database = run(['trivy', 'version', '--format=json'], timeout=20)
                    try:
                        row['databaseCheckedAt'] = json.loads(database)['VulnerabilityDB']['UpdatedAt']
                    except (ValueError, KeyError, TypeError):
                        row['status'] = 'blocked'
                        row['reason'] = 'Trivy database freshness unavailable'
            manifest['scans'].append(row)
            print(f'{name}: {row["status"]}', flush=True)
    manifest['completedAt'] = timestamp()
    manifest['status'] = 'passed' if all(row['status'] == 'passed' for row in manifest['scans']) else 'incomplete'
    if identity() != source:
        manifest['status'] = 'failed'
        manifest['reason'] = 'source changed during security scans'
    path = output / 'manifest.json'
    path.write_text(json.dumps(manifest, indent=2) + '\n')
    path.chmod(0o600)
    issues = validate_manifest(path, source, images)
    if issues and manifest['status'] == 'passed':
        manifest['status'] = 'failed'
        path.write_text(json.dumps(manifest, indent=2) + '\n')
    return path, issues


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['scan', 'verify'])
    parser.add_argument('--images', type=Path, required=True, help='JSON map of required roles to exact image digests')
    parser.add_argument('--output', type=Path, help='new private directory outside tracked source')
    parser.add_argument('--manifest', type=Path)
    args = parser.parse_args()
    images = json.loads(args.images.read_text())
    if not isinstance(images, dict) or set(images) != set(POLICY['requiredImageRoles']) or any(
            not isinstance(value, str) or not DIGEST.fullmatch(value) for value in images.values()):
        parser.error('images must contain exactly the required digest-pinned image roles')
    if args.action == 'scan':
        if args.output is None:
            parser.error('--output is required for scan')
        path, issues = scan(images, args.output.resolve())
        print(f'Evidence: {path}')
    else:
        if args.manifest is None:
            parser.error('--manifest is required for verify')
        issues = validate_manifest(args.manifest, identity(), images)
    for issue in issues:
        print(issue)
    return 1 if issues else 0


if __name__ == '__main__':
    sys.exit(main())
