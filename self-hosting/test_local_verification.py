import importlib.util
from pathlib import Path
import sys
import unittest
sys.path.insert(0, str(Path(__file__).parent))

spec = importlib.util.spec_from_file_location('verify_local', Path(__file__).with_name('verify-local.py'))
verify = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verify)

class VerificationTests(unittest.TestCase):
    def test_missing_evidence_is_not_success(self):
        self.assertEqual(verify.overall([{'status': 'passed'}, {'status': 'skipped'}]), 'incomplete')
        self.assertEqual(verify.overall([{'status': 'failed'}, {'status': 'skipped'}]), 'failed')

    def test_timeout_terminates_owned_process(self):
        result = verify.run_check([sys.executable, '-c', 'import time; time.sleep(60)'], verify.ROOT, .02, {})
        self.assertEqual(result['status'], 'failed')
        self.assertLess(result['durationSeconds'], 2)

    def test_real_exit_status_is_preserved(self):
        result = verify.run_check([sys.executable, '-c', 'raise SystemExit(7)'], verify.ROOT, 2, {})
        self.assertEqual(result['exitCode'], 7)
        self.assertEqual(result['status'], 'failed')
