import json
import re
import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).parent
PROVISION = ROOT / 'provision'
SCRIPT = PROVISION / 'host-bootstrap.sh'


class ProvisionScriptTests(unittest.TestCase):
    def test_script_parses(self):
        subprocess.run(['bash', '-n', str(SCRIPT)], check=True)

    def test_help_needs_no_privileges(self):
        result = subprocess.run(['bash', str(SCRIPT), '--help'],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        for name in ('ADMIN_SSH_CIDR', 'SSH_PORT', '--check', '--cloudflare-ips'):
            self.assertIn(name, result.stdout)

    def test_unknown_argument_is_rejected(self):
        result = subprocess.run(['bash', str(SCRIPT), '--nope'],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn('unknown argument', result.stderr)

    def test_guardrails_are_present(self):
        text = SCRIPT.read_text()
        # Disabling password login is only safe once a key already exists, and
        # the script never resets the firewall or deletes host state.
        self.assertIn('authorized_keys', text)
        self.assertNotIn('ufw --force reset', text)
        self.assertNotIn('rm -rf', text)

    def test_docker_daemon_limits_match_the_compose_kit(self):
        daemon = json.loads((PROVISION / 'docker-daemon.json').read_text())
        self.assertEqual(daemon['log-driver'], 'local')
        self.assertEqual(daemon['log-opts'], {'max-size': '50m', 'max-file': '5'})
        self.assertTrue(daemon['no-new-privileges'])
        compose = (ROOT / 'deploy' / 'docker-compose.yml').read_text()
        self.assertIn('driver: local', compose)
        self.assertIn('max-size: "50m"', compose)
        self.assertIn('max-file: "5"', compose)

    def test_readme_local_links_resolve(self):
        text = (PROVISION / 'README.md').read_text()
        targets = re.findall(r'\]\((\.\.?/[^)#]+)', text)
        self.assertTrue(targets, 'README has no relative links')
        for target in targets:
            self.assertTrue((PROVISION / target).resolve().exists(), target)


if __name__ == '__main__':
    unittest.main()
