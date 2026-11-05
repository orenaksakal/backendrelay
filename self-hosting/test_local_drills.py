import re
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).parent
DRILLS = ROOT / 'drills'
RECORDER = DRILLS / 'record-drill.sh'
OPERATIONS = ROOT.parent / 'docs/operations.md'
README = DRILLS / 'README.md'
LIFEBOAT_README = DRILLS / 'lifeboat/README.md'
FLY_TEMPLATE = DRILLS / 'lifeboat/fly.toml.example'
ABUSE_README = DRILLS / 'abuse/README.md'
ABUSE_DRIVER = DRILLS / 'abuse/abuse-drill.mjs'
ABUSE_MODEL = DRILLS / 'abuse/abuse-model.mjs'


def run_recorder(*args):
    return subprocess.run(['bash', str(RECORDER), *args],
                          capture_output=True, text=True, encoding='utf-8')


def heading_slug(heading):
    '''GitHub heading anchor for a Markdown heading line.'''
    text = re.sub(r'[^a-z0-9 _-]', '', heading.strip().lower())
    return text.replace(' ', '-')


class RecorderTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.ledger = Path(self.temporary.name) / 'drills.tsv'

    def record(self, *extra):
        return run_recorder('--drill', 'a', '--date', '2026-11-02',
                            '--seconds', '1840', '--ledger', str(self.ledger),
                            *extra)

    def test_script_parses(self):
        subprocess.run(['bash', '-n', str(RECORDER)], check=True)

    def test_help_needs_no_state(self):
        result = run_recorder('--help')
        self.assertEqual(result.returncode, 0, result.stderr)
        for name in ('--drill', '--date', '--seconds', '--status', '--notes',
                     '--ledger'):
            self.assertIn(name, result.stdout)

    def test_missing_values_and_unknown_arguments_are_rejected(self):
        for args in (['--drill'], ['--nope'], ['--drill', 'a', '--date']):
            result = run_recorder(*args)
            self.assertEqual(result.returncode, 2, args)
            self.assertTrue(result.stderr)

    def test_drill_identifier_is_limited_to_the_three_drills(self):
        result = self.record('--drill', 'd')
        self.assertEqual(result.returncode, 2)
        self.assertIn('--drill must be a, b or c', result.stderr)

    def test_date_must_be_a_real_utc_date(self):
        for value in ('2026-13-01', '2026-02-30', '02/11/2026', '2026-1-2'):
            result = run_recorder('--drill', 'a', '--date', value,
                                  '--seconds', '10', '--ledger',
                                  str(self.ledger))
            self.assertEqual(result.returncode, 2, value)
            self.assertIn('--date', result.stderr)

    def test_seconds_must_be_positive_and_bounded(self):
        for value in ('0', '-1', '1.5', 'soon', '999999999'):
            result = run_recorder('--drill', 'a', '--date', '2026-11-02',
                                  '--seconds', value, '--ledger',
                                  str(self.ledger))
            self.assertEqual(result.returncode, 2, value)
            self.assertIn('--seconds', result.stderr)

    def test_status_and_notes_are_validated_for_markdown(self):
        for args in (['--status', 'maybe'],
                     ['--notes', 'a | b'],
                     ['--notes', 'line one\nline two'],
                     ['--notes', 'x' * 301]):
            result = self.record(*args)
            self.assertEqual(result.returncode, 2, args)
        self.assertFalse(self.ledger.exists())

    def test_a_record_writes_one_header_and_the_markdown_row(self):
        result = self.record('--notes', 'bundle 20261101T023000Z')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout,
                         '| a | 2026-11-02 | 1840 s | passed | '
                         'bundle 20261101T023000Z |\n')
        lines = self.ledger.read_text(encoding='utf-8').splitlines()
        self.assertEqual(lines[0], 'date\tdrill\tseconds\tstatus\tnotes')
        self.assertEqual(lines[1],
                         '2026-11-02\ta\t1840\tpassed\tbundle 20261101T023000Z')
        self.assertEqual(self.ledger.stat().st_mode & 0o777, 0o600)

    def test_leading_zeros_are_normalised(self):
        result = run_recorder('--drill', 'c', '--date', '2026-11-02',
                              '--seconds', '0090', '--ledger', str(self.ledger))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('| 90 s |', result.stdout)
        self.assertIn('\t90\t', self.ledger.read_text(encoding='utf-8'))

    def test_existing_ledger_directory_mode_is_not_changed(self):
        directory = Path(self.temporary.name) / 'existing'
        directory.mkdir()
        directory.chmod(0o755)
        result = run_recorder('--drill', 'a', '--date', '2026-11-02',
                              '--seconds', '10', '--ledger',
                              str(directory / 'drills.tsv'))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(directory.stat().st_mode & 0o777, 0o755)

    def test_failed_status_and_empty_notes_are_recorded(self):
        result = self.record('--status', 'failed')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('| failed | — |', result.stdout)
        self.assertIn('\tfailed\t—\n', self.ledger.read_text(encoding='utf-8'))

    def test_records_append_instead_of_replacing(self):
        self.assertEqual(self.record().returncode, 0)
        self.assertEqual(self.record('--status', 'failed').returncode, 0)
        lines = self.ledger.read_text(encoding='utf-8').splitlines()
        self.assertEqual(lines.count('date\tdrill\tseconds\tstatus\tnotes'), 1)
        self.assertEqual(len(lines), 3)

    def test_concurrent_records_share_one_header_and_ordering_lock(self):
        processes = [
            subprocess.Popen(
                ['bash', str(RECORDER), '--drill', 'b', '--date', '2026-11-02',
                 '--seconds', str(index + 1), '--ledger', str(self.ledger)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                encoding='utf-8')
            for index in range(6)
        ]
        for process in processes:
            stdout, stderr = process.communicate(timeout=30)
            self.assertEqual(process.returncode, 0, stderr)
            self.assertTrue(stdout.startswith('| b |'))
        lines = self.ledger.read_text(encoding='utf-8').splitlines()
        self.assertEqual(lines.count('date\tdrill\tseconds\tstatus\tnotes'), 1)
        self.assertEqual(len(lines), 7)


class OperationsRunbookTests(unittest.TestCase):
    def setUp(self):
        self.text = OPERATIONS.read_text(encoding='utf-8')

    def test_drills_anchor_and_both_sections_exist(self):
        # self-hosting/scale-local/drills.sh points at #drills; the failure
        # drills keep their older #failure-drills anchor.
        self.assertEqual(len(re.findall(r'^## Drills$', self.text, re.MULTILINE)), 1)
        self.assertIn('\n### Failure drills\n', self.text)
        self.assertIn('\n### Disaster-recovery drills\n', self.text)
        self.assertLess(self.text.index('### Failure drills'),
                        self.text.index('### Disaster-recovery drills'))

    def test_result_table_has_a_row_for_every_drill(self):
        header = '| Drill | Date | Recovery | Result | Notes |'
        self.assertIn(header, self.text)
        for drill in ('a', 'b', 'c'):
            self.assertRegex(self.text, rf'(?m)^\| {drill} \|')

    def test_runbook_links_resolve(self):
        section = self.text[self.text.index('### Disaster-recovery drills'):]
        targets = re.findall(r'\]\((\.\./[^)#]+)', section)
        self.assertTrue(targets)
        for target in targets:
            self.assertTrue((ROOT.parent / 'docs' / target).resolve().exists(),
                            target)
        for name in ('record-drill.sh', 'README.md', 'lifeboat/fly.toml.example'):
            self.assertTrue((DRILLS / name).exists(), name)

    def test_drill_steps_name_the_safety_checks(self):
        section = self.text[self.text.index('### Disaster-recovery drills'):]
        for phrase in ('offhostVerified', 'server-secrets.tar',
                       'backendrelay-staging_server_secrets',
                       'pg_restore', '/dash/restores/zip',
                       'lifeboat-api.', 'record-drill.sh'):
            self.assertIn(phrase, section)


class AbuseRunbookTests(unittest.TestCase):
    def setUp(self):
        self.text = OPERATIONS.read_text(encoding='utf-8')
        start = self.text.index('### Abuse and isolation drills')
        self.section = self.text[start:self.text.index('### Disaster-recovery drills')]

    def test_abuse_section_sits_between_the_other_drill_sections(self):
        self.assertLess(self.text.index('### Failure drills'),
                        self.text.index('### Abuse and isolation drills'))
        self.assertLess(self.text.index('### Abuse and isolation drills'),
                        self.text.index('### Disaster-recovery drills'))
        self.assertIn('[abuse and isolation drill](#abuse-and-isolation-drills)',
                      self.text)

    def test_abuse_result_table_has_a_row_for_every_control(self):
        self.assertIn('| Check | Expected | Measured | Result | Notes |', self.section)
        for row in ('Magic-code sends to one address', 'Magic-code sends from one IP',
                    'Pre-auth body', 'InstaQL query size', 'Oversize upload',
                    'HTML upload', 'Guest sign-up burst', 'Expensive queries',
                    'Sessions from one IP', 'App B p95', 'App B read errors',
                    'Alerts', 'Email provider'):
            self.assertRegex(self.section, rf'(?m)^\| {re.escape(row)} \|')

    def test_abuse_steps_name_the_limits_and_evidence(self):
        for phrase in ('ABUSE_API_ORIGIN', 'magic-code-rate-limit-per-ip-hour',
                       'INSTANT_STORAGE_MAX_UPLOAD_BYTES', 'max-instaql-query-bytes',
                       'max-live-sessions-per-ip', 'backendrelay_emails_total',
                       'abuse-drill.mjs'):
            self.assertIn(phrase, self.section)

    def test_abuse_kit_exists_and_its_readme_links_resolve(self):
        for path in (ABUSE_README, ABUSE_DRIVER, ABUSE_MODEL):
            self.assertTrue(path.exists(), path)
        text = ABUSE_README.read_text(encoding='utf-8')
        for target in re.findall(r'\]\(([^)#]+)', text):
            if target.startswith(('http://', 'https://', 'mailto:')):
                continue
            self.assertTrue((ABUSE_README.parent / target).resolve().exists(),
                            f'{ABUSE_README}: {target}')


class DrillKitTests(unittest.TestCase):
    def test_readmes_have_no_broken_relative_links(self):
        for document in (README, LIFEBOAT_README, ABUSE_README):
            text = document.read_text(encoding='utf-8')
            for target in re.findall(r'\]\(([^)#]+)', text):
                if target.startswith(('http://', 'https://', 'mailto:')):
                    continue
                self.assertTrue((document.parent / target).resolve().exists(),
                                f'{document}: {target}')

    def test_fly_template_targets_frankfurt_and_the_health_contract(self):
        try:
            import tomllib
        except ImportError:  # pragma: no cover - Python 3.11+ is required
            self.skipTest('tomllib is unavailable')
        config = tomllib.loads(FLY_TEMPLATE.read_text(encoding='utf-8'))
        self.assertEqual(config['primary_region'], 'fra')
        self.assertTrue(config['build']['image'].startswith('REPLACE_WITH_'))
        service = config['http_service']
        self.assertEqual(service['internal_port'], 8888)
        self.assertEqual(service['checks'][0]['path'], '/health/system')
        self.assertEqual(config['env']['PORT'], '8888')
        self.assertIn('REPLACE_WITH_', config['env']['INSTANT_BACKEND_URL'])

    def test_fly_template_keeps_secrets_out_of_the_file(self):
        text = FLY_TEMPLATE.read_text(encoding='utf-8')
        parsed = None
        try:
            import tomllib
            parsed = tomllib.loads(text)
        except ImportError:  # pragma: no cover
            pass
        if parsed is not None:
            for name in ('DATABASE_URL', 'INSTANT_AEAD_KEYSET_JSON',
                         'INSTANT_WEBHOOK_KEYSET_JSON',
                         'INSTANT_RATE_LIMIT_HMAC_KEY',
                         'INSTANT_S3_ACCESS_KEY_ID',
                         'INSTANT_S3_SECRET_ACCESS_KEY',
                         'CLOUDFLARE_EMAIL_API_TOKEN'):
                self.assertNotIn(name, parsed['env'])
        self.assertNotIn('postgresql://', text)
        self.assertNotIn('age1', text)

    def test_lifeboat_instructions_cover_both_providers_and_teardown(self):
        text = LIFEBOAT_README.read_text(encoding='utf-8')
        for phrase in ('Fly.io Frankfurt', 'Railway', 'fra', 'server_secrets',
                       'fly apps destroy', 'point-in-time', 'lifeboat-api.'):
            self.assertIn(phrase, text)


class OperatingCadenceTests(unittest.TestCase):
    HEADING = '## Operating cadence (P8.4)'

    def setUp(self):
        self.text = OPERATIONS.read_text(encoding='utf-8')
        self.assertIn(self.HEADING, self.text)
        self.assertEqual(self.text.count(self.HEADING), 1)
        self.section = self.text[self.text.index(self.HEADING):]

    def rhythm(self, name, following):
        start = self.section.index(f'### {name}')
        end = self.section.index(f'### {following}')
        self.assertLess(start, end)
        return self.section[start:end]

    def test_cadence_follows_onboarding_and_closes_the_page(self):
        self.assertGreater(self.text.index(self.HEADING),
                           self.text.index('## Onboarding an app (P8.3)'))
        self.assertNotIn('\n## ', self.section)

    def test_the_rhythms_and_the_release_process_appear_in_order(self):
        headings = ['### Daily (5 minutes)', '### Weekly', '### Monthly',
                    '### Quarterly', '### Release process']
        positions = [self.section.index(heading) for heading in headings]
        self.assertEqual(positions, sorted(positions))

    def test_daily_routine_covers_the_digest_backup_and_canary(self):
        daily = self.rhythm('Daily (5 minutes)', 'Weekly')
        for phrase in ('alert digest', 'amtool', 'Alert playbooks',
                       'backendrelay_backup_snapshot_seconds',
                       'backendrelay_canary_last_success_timestamp_seconds'):
            self.assertIn(phrase, daily)

    def test_weekly_routine_covers_dependencies_panels_audit_and_sdks(self):
        weekly = self.rhythm('Weekly', 'Monthly')
        for phrase in ('Renovate', 'Per app', 'backendrelay_sdk_sessions',
                       'backendrelay-deploy-audit.timer', 'run-the-audit'):
            self.assertIn(phrase, weekly)

    def test_monthly_routine_covers_restore_audit_sizing_and_secrets(self):
        monthly = self.rhythm('Monthly', 'Quarterly')
        for phrase in ('Disaster-recovery drills', 'make audit',
                       'pnpm.auditConfig.ignoreGhsas', 'performance.md#sizing',
                       'Resize the CAX41', 'Verify the off-host copy'):
            self.assertIn(phrase, monthly)

    def test_quarterly_routine_covers_lifeboat_keys_majors_and_upstream(self):
        quarterly = self.rhythm('Quarterly', 'Release process')
        for phrase in ('lifeboat', 'Key rotation', 'dependency major versions',
                       '2027-08-31', 'upstream-watch'):
            self.assertIn(phrase, quarterly)

    def test_release_process_names_build_staging_canary_watch_and_rollback(self):
        release = self.section[self.section.index('### Release process'):]
        for phrase in ('verify-release-images.sh', 'cosign',
                       'DEPLOY_ENV_FILE=deploy/.env.staging',
                       'bash deploy/deploy.sh', 'Watch for 30 minutes',
                       'backendrelay_canary_propagation_seconds_bucket',
                       'bash deploy/rollback.sh', 'Roll back'):
            self.assertIn(phrase, release)

    def test_cadence_links_point_at_real_files_and_headings(self):
        for target in re.findall(r'\]\(([^)#]+)', self.section):
            if target.startswith(('http://', 'https://', 'mailto:')):
                continue
            self.assertTrue((OPERATIONS.parent / target).resolve().exists(),
                            target)
        slugs = {heading_slug(heading) for heading in
                 re.findall(r'^#{2,6} (.+)$', self.text, re.MULTILINE)}
        anchors = re.findall(r'\]\(#([^)]+)\)', self.section)
        self.assertTrue(anchors)
        for anchor in anchors:
            self.assertIn(anchor, slugs)


if __name__ == '__main__':
    unittest.main()
