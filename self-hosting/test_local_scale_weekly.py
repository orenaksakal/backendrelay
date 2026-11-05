import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCALE = ROOT / 'self-hosting/scale-local'
COMPARE = SCALE / 'weekly_compare.py'
WEEKLY = SCALE / 'weekly.sh'
BASELINE = SCALE / 'baseline.json'


def comparator():
    spec = importlib.util.spec_from_file_location('weekly_compare', COMPARE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


compare_module = comparator()


def phase(**updates):
    entry = {'name': 'steady', 'passed': True, 'failures': [], 'read': {'p95': 10.0},
             'write': {'p95': 12.0}, 'realtime': {'p95': 30.0},
             'serverCpuMsPerOp': 0.4, 'pgCpuMsPerOp': 0.6}
    entry.update(updates)
    return entry


def result(**updates):
    value = {'passed': True, 'invalid': False, 'apps': 10, 'usersPerApp': 1000,
             'phases': [phase()]}
    value.update(updates)
    return value


def baseline(**metrics):
    return {'scenarios': {'10x1000': {
        'errorCount': 0, 'p95OpLatencyMs': 30.0, 'serverMsPerOp': 0.4,
        'pgMsPerOp': 0.6, **metrics}}}


class MeasureTests(unittest.TestCase):
    def test_worst_phase_and_metric_kind_is_reported(self):
        measured = compare_module.measure(result(phases=[
            phase(read={'p95': 1.0}, write={'p95': 2.0}, realtime={'p95': 3.0},
                  serverCpuMsPerOp=0.1, pgCpuMsPerOp=0.2),
            phase(read={'p95': 4.0}, write={'p95': 12.5}, realtime={'p95': 9.0},
                  serverCpuMsPerOp=0.55, pgCpuMsPerOp=0.9),
        ]))
        self.assertEqual(measured['p95OpLatencyMs'], 12.5)
        self.assertEqual(measured['serverMsPerOp'], 0.55)
        self.assertEqual(measured['pgMsPerOp'], 0.9)
        self.assertEqual(measured['errorCount'], 0)

    def test_phase_failures_are_counted(self):
        measured = compare_module.measure(result(phases=[
            phase(failures=['read latency']), phase(failures=['write', 'realtime'])],
            passed=False))
        self.assertEqual(measured['errorCount'], 3)

    def test_failed_run_without_listed_failures_still_counts_as_an_error(self):
        self.assertEqual(compare_module.measure(result(passed=False))['errorCount'], 1)
        self.assertEqual(compare_module.measure(result(invalid=True))['errorCount'], 1)


class CompareTests(unittest.TestCase):
    def status(self, rows, metric):
        return next(entry['status'] for entry in rows if entry['metric'] == metric)

    def test_identical_run_passes(self):
        rows = compare_module.compare(baseline(), {'10x1000': compare_module.measure(result())})
        self.assertTrue(all(entry['status'] == 'ok' for entry in rows))
        self.assertEqual(compare_module.failed(rows), [])

    def test_values_within_the_tolerance_pass(self):
        measured = compare_module.measure(result(phases=[
            phase(read={'p95': 35.9}, write={'p95': 1.0}, realtime={'p95': 1.0})]))
        rows = compare_module.compare(baseline(), {'10x1000': measured})
        self.assertEqual(self.status(rows, 'p95OpLatencyMs'), 'ok')
        self.assertEqual(compare_module.failed(rows), [])

    def test_values_above_the_tolerance_regress(self):
        measured = compare_module.measure(result(phases=[
            phase(read={'p95': 36.5}, write={'p95': 1.0}, realtime={'p95': 1.0})]))
        rows = compare_module.compare(baseline(), {'10x1000': measured})
        self.assertEqual(self.status(rows, 'p95OpLatencyMs'), 'regressed')
        self.assertEqual(len(compare_module.failed(rows)), 1)

    def test_tolerance_is_twenty_percent(self):
        self.assertEqual(compare_module.TOLERANCE_PERCENT, 20.0)
        measured = compare_module.measure(result(phases=[phase(read={'p95': 30.6})]))
        self.assertEqual(compare_module.failed(
            compare_module.compare(baseline(), {'10x1000': measured})), [])
        self.assertEqual(len(compare_module.failed(
            compare_module.compare(baseline(), {'10x1000': measured}, tolerance=1))), 1)

    def test_cpu_per_operation_regressions_are_flagged(self):
        measured = compare_module.measure(result(phases=[
            phase(serverCpuMsPerOp=0.6, pgCpuMsPerOp=0.8)]))
        rows = compare_module.compare(baseline(), {'10x1000': measured})
        self.assertEqual(self.status(rows, 'serverMsPerOp'), 'regressed')
        self.assertEqual(self.status(rows, 'pgMsPerOp'), 'regressed')

    def test_cpu_improvement_is_not_a_regression(self):
        measured = compare_module.measure(result(phases=[
            phase(serverCpuMsPerOp=0.1, pgCpuMsPerOp=0.1)]))
        rows = compare_module.compare(baseline(), {'10x1000': measured})
        self.assertEqual(compare_module.failed(rows), [])
        self.assertLess(next(entry['changePercent'] for entry in rows
                             if entry['metric'] == 'pgMsPerOp'), 0)

    def test_any_error_is_a_regression(self):
        measured = compare_module.measure(result(phases=[
            phase(failures=['realtime latency'], realtime={'p95': 1.0})], passed=False))
        rows = compare_module.compare(baseline(), {'10x1000': measured})
        self.assertEqual(self.status(rows, 'errorCount'), 'regressed')

    def test_null_baseline_metrics_are_unmeasured_not_regressions(self):
        unfilled = baseline(errorCount=None, p95OpLatencyMs=None,
                            serverMsPerOp=None, pgMsPerOp=None)
        rows = compare_module.compare(unfilled, {'10x1000': compare_module.measure(result())})
        self.assertEqual([entry['status'] for entry in rows], ['ok'] + ['unmeasured'] * 3)
        self.assertEqual(compare_module.failed(rows), [])

    def test_missing_scenario_result_fails(self):
        rows = compare_module.compare(baseline(), {})
        self.assertEqual(rows[0]['status'], 'missing')
        self.assertEqual(len(compare_module.failed(rows)), 1)

    def test_unexpected_scenario_is_reported_without_failing(self):
        rows = compare_module.compare(baseline(), {'10x1000': compare_module.measure(result()),
                                                  '99x20': compare_module.measure(result())})
        unexpected = next(entry for entry in rows if entry['scenario'] == '99x20')
        self.assertEqual(unexpected['status'], 'unmeasured')
        self.assertEqual(compare_module.failed(rows), [])


class CommandLineTests(unittest.TestCase):
    def run_cli(self, arguments):
        return subprocess.run([sys.executable, str(COMPARE), *arguments],
                              capture_output=True, text=True)

    def results(self, directory, scenario, value):
        path = Path(directory) / 'results'
        path.mkdir(exist_ok=True)
        (path / f'{scenario}.json').write_text(json.dumps(value))
        return path

    def test_exit_codes_and_table(self):
        with tempfile.TemporaryDirectory() as directory:
            committed = Path(directory) / 'baseline.json'
            committed.write_text(json.dumps(baseline()))
            results = self.results(directory, '10x1000', result())
            passed = self.run_cli(['--results-dir', str(results), '--baseline', str(committed)])
            self.assertEqual(passed.returncode, 0, passed.stderr)
            self.assertIn('p95 op latency (ms)', passed.stdout)
            self.assertIn('weekly scale: passed', passed.stdout)

            self.results(directory, '10x1000', result(phases=[phase(read={'p95': 100.0})]))
            regressed = self.run_cli(['--results-dir', str(results), '--baseline', str(committed)])
            self.assertEqual(regressed.returncode, 1)
            self.assertIn('REGRESSED', regressed.stdout)

    def test_empty_results_directory_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            empty = Path(directory) / 'empty'
            empty.mkdir()
            completed = self.run_cli(['--results-dir', str(empty)])
            self.assertEqual(completed.returncode, 1)
            self.assertIn('no result files', completed.stderr)

    def test_update_baseline_records_the_latest_run(self):
        with tempfile.TemporaryDirectory() as directory:
            committed = Path(directory) / 'baseline.json'
            committed.write_text(json.dumps({'_note': 'keep me', 'scenarios': {}}))
            results = self.results(directory, '10x1000', result(phases=[
                phase(read={'p95': 21.0}, write={'p95': 1.0}, realtime={'p95': 2.0},
                      serverCpuMsPerOp=0.5, pgCpuMsPerOp=0.7)]))
            updated = self.run_cli(['--results-dir', str(results), '--baseline', str(committed),
                                    '--update-baseline'])
            self.assertEqual(updated.returncode, 0, updated.stderr)
            written = json.loads(committed.read_text())
            self.assertEqual(written['_note'], 'keep me')
            self.assertEqual(set(written['scenarios']), {'10x1000'})
            self.assertEqual(written['scenarios']['10x1000']['p95OpLatencyMs'], 21.0)
            self.assertEqual(written['scenarios']['10x1000']['serverMsPerOp'], 0.5)
            self.assertEqual(written['scenarios']['10x1000']['pgMsPerOp'], 0.7)
            self.assertEqual(written['scenarios']['10x1000']['errorCount'], 0)


class ContractTests(unittest.TestCase):
    def test_committed_baseline_lists_both_weekly_scenarios(self):
        committed = json.loads(BASELINE.read_text())
        self.assertIn('_note', committed)
        self.assertEqual(set(committed['scenarios']), {'10x1000', '50x100'})
        for entry in committed['scenarios'].values():
            for metric in ('errorCount', 'p95OpLatencyMs', 'serverMsPerOp', 'pgMsPerOp'):
                self.assertIn(metric, entry)

    def test_weekly_script_parses(self):
        completed = subprocess.run(['bash', '-n', str(WEEKLY)], capture_output=True, text=True)
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_weekly_script_runs_both_shapes_and_compares(self):
        script = WEEKLY.read_text()
        self.assertIn("'10x1000 10 1000' '50x100 50 100'", script)
        self.assertIn('stack.sh" up', script)
        self.assertIn('stack.sh" down', script)
        self.assertIn('weekly_compare.py', script)
        self.assertIn('result.json', script)

    def test_timer_and_service_contract(self):
        service = (SCALE / 'systemd/backendrelay-weekly-scale.service').read_text()
        timer = (SCALE / 'systemd/backendrelay-weekly-scale.timer').read_text()
        self.assertIn('Type=oneshot', service)
        self.assertIn('Nice=10', service)
        self.assertIn('ExecStart=%h/Projects/backendrelay/self-hosting/scale-local/weekly.sh',
                      service)
        self.assertIn('OnCalendar=Sun 04:00', timer)
        self.assertIn('Persistent=true', timer)
        self.assertIn('WantedBy=timers.target', timer)

    def test_timer_install_is_documented(self):
        readme = (SCALE / 'README.md').read_text()
        operations = (ROOT / 'docs/operations.md').read_text()
        for text in (readme, operations):
            self.assertIn('systemctl --user enable --now backendrelay-weekly-scale.timer', text)


if __name__ == '__main__':
    unittest.main()
