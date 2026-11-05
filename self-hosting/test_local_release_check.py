import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / 'self-hosting/release-check.sh'


class ReleaseCheckContractTests(unittest.TestCase):
    def test_makefile_target_forwards_the_tag(self):
        makefile = (ROOT / 'Makefile').read_text()
        self.assertIn('\nrelease-check:', makefile)
        self.assertIn('make release-check TAG=<tag>', makefile)
        self.assertIn('bash self-hosting/release-check.sh "$(TAG)"', makefile)

    def test_help_documents_the_gate_and_exits_zero(self):
        result = subprocess.run(['bash', str(SCRIPT), '--help'], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('release-check.sh <tag>', result.stdout)

    def test_missing_tag_is_a_usage_error(self):
        result = subprocess.run(['bash', str(SCRIPT)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn('usage:', result.stderr)

    def test_release_procedure_is_documented(self):
        operations = (ROOT / 'docs/operations.md').read_text()
        self.assertIn('## Releases', operations)
        self.assertIn('make release-check TAG=', operations)


if __name__ == '__main__':
    unittest.main()
