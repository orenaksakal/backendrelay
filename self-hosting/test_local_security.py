import copy
from datetime import datetime, timedelta, timezone
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import yaml


def module(name):
    path = Path(__file__).parent / 'security' / (name + '.py')
    spec = importlib.util.spec_from_file_location(name.replace('-', '_'), path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


scan = module('scan')
policy = module('production-policy')


class ProductionPolicyTests(unittest.TestCase):
    def state(self, **updates):
        return {'capacities': [], 'durableToggles': [], 'durableTablePresent': True,
                'durableTableLogged': True, 'databaseReadOnly': False,
                'configApp': {'adminTokens': 0, 'members': 0,
                              'rules': {'$default': {'allow': {'$default': 'false'}}}},
                **updates}

    def test_absent_settings_have_effective_durable_default(self):
        result = policy.evaluate(self.state())
        self.assertEqual(result['status'], 'passed')
        self.assertEqual(result['effectiveHourlyCapacity'], 20)

    def test_legacy_truthiness_and_live_disable_are_detected(self):
        for value in (False, None):
            self.assertEqual(policy.evaluate(self.state(durableToggles=[value]))['status'], 'failed')
        for value in (True, 0, '', []):
            self.assertEqual(policy.evaluate(self.state(durableToggles=[value]))['status'], 'passed')

    def test_unsafe_and_ambiguous_database_state_fails_closed(self):
        for changes in ({'capacities': [1000]}, {'durableToggles': [True, False]},
                        {'capacities': [10, 20]}, {'durableTablePresent': False},
                        {'durableTableLogged': False}, {'databaseReadOnly': True},
                        {'databaseReadOnly': None}):
            self.assertEqual(policy.evaluate(self.state(**changes))['status'], 'failed', changes)

    def test_config_app_must_be_seeded_and_locked(self):
        unsafe = (
            {'adminTokens': 1, 'members': 0,
             'rules': {'$default': {'allow': {'$default': 'false'}}}},
            {'adminTokens': 0, 'members': 1,
             'rules': {'$default': {'allow': {'$default': 'false'}}}},
            {'adminTokens': 0, 'members': 0, 'rules': {'$default': {'allow': 'true'}}},
            {'adminTokens': 0, 'members': 0, 'rules': None},
        )
        for config_app in unsafe:
            self.assertEqual(policy.evaluate(self.state(configApp=config_app))['status'], 'failed',
                             config_app)
        self.assertEqual(policy.evaluate(self.state(configApp=None))['status'], 'failed')

    def test_fallback_capacity_matches_server_and_change_fingerprint_is_stable(self):
        for value in (False, None, '10', 0, -1, 1.5, 2**64):
            self.assertEqual(policy.evaluate(self.state(capacities=[value]))['effectiveHourlyCapacity'], 20)
        before = policy.evaluate(self.state())
        after = policy.evaluate(self.state(capacities=[25]))
        self.assertNotEqual(before['configurationSha256'], after['configurationSha256'])
        self.assertEqual(before, policy.evaluate(self.state()))

    def test_wildcard_trusted_proxy_fails_and_private_proxy_passes(self):
        for cidr in ('0.0.0.0/0', '::/0', 'not-a-cidr'):
            self.assertEqual(policy.evaluate(self.state(), cidr)['status'], 'failed')
        self.assertEqual(policy.evaluate(self.state(), '10.0.0.0/24,fd00::/64')['status'], 'passed')


class SecurityEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name) / 'manifest.json'
        self.now = datetime.now(timezone.utc)
        self.source = {'revision': 'a' * 40, 'dirty': False, 'sourceSha256': 'b' * 64}
        self.images = {role: 'example/' + role + '@sha256:' + 'c' * 64 for role in scan.POLICY['requiredImageRoles']}
        rows = []
        for index, (name, tool, target) in enumerate(scan.requirements(self.images)):
            payload = {'cargo-audit': {'vulnerabilities': {'found': False, 'count': 0, 'list': []},
                                       'database': {'last-commit': 'a' * 40}},
                       'pnpm': {'metadata': {'vulnerabilities': {'low': 0, 'moderate': 0, 'high': 0, 'critical': 0}}},
                       'bun': {}, 'gitleaks': [],
                       'trivy': {'SchemaVersion': 2, 'ArtifactName': target, 'ArtifactType': 'container_image',
                                 'Metadata': {'ImageID': 'sha256:' + 'c' * 64}, 'Results': [{'Target': 'os'}]}}[tool]
            artifact = self.path.parent / f'{index}.json'
            artifact.write_text(json.dumps(payload))
            rows.append({'name': name, 'tool': tool, 'version': scan.POLICY['tools'][tool],
                         'target': target, 'status': 'passed', 'exitCode': 0,
                         'completedAt': self.now.isoformat(), 'databaseCheckedAt': self.now.isoformat(),
                         'artifact': {'path': artifact.name, 'sha256': scan.sha256(artifact)}})
        self.manifest = {'schemaVersion': 1, 'kind': 'backendrelay-security-scans', 'source': self.source,
                         'images': self.images, 'policySha256': scan.sha256(scan.HERE / 'policy.json'),
                         'status': 'passed', 'completedAt': self.now.isoformat(), 'scans': rows}

    def validate(self, manifest=None):
        self.path.write_text(json.dumps(self.manifest if manifest is None else manifest))
        return scan.validate_manifest(self.path, self.source, self.images, self.now)

    def test_complete_exact_candidate_passes(self):
        self.assertEqual(self.validate(), [])

    def test_missing_duplicate_failed_and_stale_scans_fail(self):
        variants = []
        missing = copy.deepcopy(self.manifest)
        missing['scans'].pop()
        variants.append(missing)
        duplicate = copy.deepcopy(self.manifest)
        duplicate['scans'].append(duplicate['scans'][0])
        variants.append(duplicate)
        for field, value in [('status', 'blocked'), ('version', 'latest'), ('exitCode', 1),
                             ('completedAt', (self.now - timedelta(hours=25)).isoformat()),
                             ('databaseCheckedAt', (self.now - timedelta(hours=25)).isoformat())]:
            changed = copy.deepcopy(self.manifest)
            changed['scans'][0][field] = value
            variants.append(changed)
        for value in variants:
            self.assertTrue(self.validate(value))

    def test_wrong_source_images_and_future_evidence_fail(self):
        for field, value in [('source', {}), ('images', {}), ('policySha256', 'changed'),
                             ('completedAt', (self.now + timedelta(seconds=1)).isoformat())]:
            changed = copy.deepcopy(self.manifest)
            changed[field] = value
            self.assertTrue(self.validate(changed))

    def test_artifact_tampering_and_path_escape_fail(self):
        (self.path.parent / '0.json').write_text('{}')
        self.assertTrue(self.validate())
        self.manifest['scans'][0]['artifact']['path'] = '../outside.json'
        self.assertTrue(self.validate())

    def test_zero_exit_cannot_hide_findings_or_error_json(self):
        artifact = self.path.parent / '0.json'
        artifact.write_text(json.dumps({'error': 'registry offline'}))
        self.manifest['scans'][0]['artifact']['sha256'] = scan.sha256(artifact)
        self.assertTrue(self.validate())
        self.assertFalse(scan.clean_report('bun', {'pkg': [{'severity': 'low'}]}, ''))
        self.assertFalse(scan.clean_report('gitleaks', [{'RuleID': 'secret'}], ''))
        self.assertFalse(scan.clean_report('trivy', {'Results': []}, 'image'))


class ScannerInvocationTests(unittest.TestCase):
    def test_project_cannot_replace_verified_pnpm_scanner(self):
        with patch.dict(os.environ, {'COREPACK_ENABLE_PROJECT_SPEC': '1',
                                    'npm_config_manage_package_manager_versions': 'true'}), \
                patch.object(scan.subprocess, 'run') as invoke:
            invoke.return_value = subprocess.CompletedProcess([], 0, b'10.32.1')
            self.assertEqual(scan.tool_version('pnpm'), scan.POLICY['tools']['pnpm'])
            scan.run(['pnpm', 'audit', '--json', '--audit-level=low'], scan.ROOT / 'client')
        version, audit = invoke.call_args_list
        for call in (version, audit):
            self.assertEqual(call.kwargs['env']['COREPACK_ENABLE_PROJECT_SPEC'], '0')
            self.assertEqual(call.kwargs['env']['npm_config_manage_package_manager_versions'], 'false')
        self.assertEqual(version.kwargs['cwd'], scan.ROOT)
        self.assertEqual(audit.kwargs['cwd'], scan.ROOT / 'client')
        self.assertEqual(audit.args[0], ['pnpm', 'audit', '--json', '--audit-level=low'])

    def test_other_scanners_do_not_receive_pnpm_arguments(self):
        with patch.object(scan.subprocess, 'run') as invoke:
            invoke.return_value = subprocess.CompletedProcess([], 1, b'{"findings": true}')
            self.assertEqual(scan.run(['bun', 'audit', '--json']), (1, b'{"findings": true}'))
        self.assertEqual(invoke.call_args.args[0], ['bun', 'audit', '--json'])


class ComposeBackupTests(unittest.TestCase):
    def test_archive_mode_is_opt_in_and_backup_secrets_have_no_defaults(self):
        seen = []
        for path in sorted(Path(__file__).parent.glob('docker-compose*.yml')):
            if path.name == 'docker-compose.test.yml':
                continue
            doc = yaml.safe_load(path.read_text())
            postgres = doc.get('services', {}).get('postgres')
            if postgres is None:
                continue
            seen.append(path.name)
            command = ' '.join(postgres.get('command', []))
            self.assertIn('archive_mode=${INSTANT_PG_ARCHIVE_MODE:-off}', command, path.name)
            self.assertIn('archive_command=pgbackrest --stanza=backendrelay archive-push %p', command, path.name)
            self.assertIn('archive_timeout=${INSTANT_PG_ARCHIVE_TIMEOUT:-60}', command, path.name)
            self.assertIn('pgbackrest-spool:/var/spool/pgbackrest', ' '.join(postgres.get('volumes', [])), path.name)
            self.assertIn('pgbackrest-spool', doc.get('volumes', {}), path.name)
            env_files = [entry if isinstance(entry, dict) else {'path': entry, 'required': True}
                         for entry in postgres.get('env_file', [])]
            self.assertIn({'path': './pgbackrest.env', 'required': False}, env_files, path.name)
            for key, value in postgres['environment'].items():
                if not key.startswith('PGBACKREST_'):
                    continue
                self.assertNotEqual(value, '', f'{path.name}: {key} must not render empty')
                self.assertNotRegex(value, r'\$\{PGBACKREST_[A-Z0-9_]+:-\}',
                                    f'{path.name}: {key} must have no empty default')
        self.assertEqual(seen, ['docker-compose.local.yml', 'docker-compose.with-caddy.yml',
                                'docker-compose.yml'])

    def test_pgbackrest_env_is_git_ignored(self):
        root = Path(__file__).resolve().parents[1]
        ignored = {line.strip() for line in (root / '.gitignore').read_text().splitlines()}
        self.assertIn('self-hosting/pgbackrest.env', ignored)


if __name__ == '__main__':
    unittest.main()
