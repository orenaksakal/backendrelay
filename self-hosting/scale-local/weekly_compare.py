#!/usr/bin/env python3
"""Compare weekly scale-local results with the committed regression baseline.

Standard library only: the weekly systemd timer runs this without a virtualenv.
Each `<scenario>.json` in the results directory is a `result.json` from
self-hosting/railway-loadtest/local-scale-run.mjs, copied by weekly.sh.

A run regresses when it reports a client-visible error, or when a latency or
CPU-per-operation metric is more than TOLERANCE_PERCENT above the baseline.
Baseline metrics that are still null (the committed file starts that way) are
reported as unmeasured rather than compared. --update-baseline rewrites the
baseline from the results directory.
"""
import argparse
import json
from pathlib import Path
import sys

TOLERANCE_PERCENT = 20.0
BASELINE = Path(__file__).resolve().with_name('baseline.json')
# Metrics compared against the baseline, keyed by the result.json field they
# are derived from. Each is the worst phase in the run.
METRICS = ('p95OpLatencyMs', 'serverMsPerOp', 'pgMsPerOp')
LABELS = {
    'errorCount': 'errors',
    'p95OpLatencyMs': 'p95 op latency (ms)',
    'serverMsPerOp': 'server ms/op',
    'pgMsPerOp': 'pg ms/op',
}
OPERATIONS = ('read', 'write', 'realtime')


def number(value):
    """Return value as a float, or None when it is absent or not a number."""
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return float(value)
    return None


def maximum(values):
    numbers = [number(value) for value in values]
    numbers = [value for value in numbers if value is not None]
    return max(numbers) if numbers else None


def measure(result):
    """Reduce a local-scale-run result.json to the compared metrics."""
    phases = result.get('phases') or []
    errors = sum(len(phase.get('failures') or []) for phase in phases)
    # A run that failed or was invalid without listing a phase failure still
    # counts as an error: the weekly regression must never pass silently.
    if not errors and (result.get('invalid') or result.get('passed') is False):
        errors = 1
    return {
        'apps': result.get('apps'),
        'usersPerApp': result.get('usersPerApp'),
        'phases': len(phases),
        'errorCount': errors,
        'p95OpLatencyMs': maximum(
            (phase.get(operation) or {}).get('p95')
            for phase in phases
            for operation in OPERATIONS
        ),
        'serverMsPerOp': maximum(phase.get('serverCpuMsPerOp') for phase in phases),
        'pgMsPerOp': maximum(phase.get('pgCpuMsPerOp') for phase in phases),
    }


def row(scenario, metric, baseline, current, status, detail='', change=None):
    return {'scenario': scenario, 'metric': metric, 'baseline': baseline,
            'current': current, 'changePercent': change, 'status': status,
            'detail': detail}


def compare(baseline, measured, tolerance=TOLERANCE_PERCENT):
    """Return one row per compared metric; status is ok/regressed/unmeasured/missing."""
    rows = []
    scenarios = baseline.get('scenarios') or {}
    for scenario, expected in scenarios.items():
        current = measured.get(scenario)
        if current is None:
            rows.append(row(scenario, 'result', None, None, 'missing',
                            'no result file for this scenario'))
            continue
        errors = current.get('errorCount') or 0
        rows.append(row(
            scenario, 'errorCount', number(expected.get('errorCount')), errors,
            'regressed' if errors else 'ok',
            f'{errors} error(s) reported by the run' if errors else '',
        ))
        for metric in METRICS:
            expected_value = number(expected.get(metric))
            current_value = number(current.get(metric))
            if current_value is None:
                rows.append(row(scenario, metric, expected_value, None, 'unmeasured',
                                'run did not report this metric'))
                continue
            if expected_value is None:
                rows.append(row(scenario, metric, None, current_value, 'unmeasured',
                                'baseline has no value yet; run --update-baseline'))
                continue
            change = None
            if expected_value > 0:
                change = (current_value - expected_value) / expected_value * 100
            regressed = current_value > expected_value * (1 + tolerance / 100)
            rows.append(row(scenario, metric, expected_value, current_value,
                            'regressed' if regressed else 'ok',
                            change=change))
    for scenario in measured:
        if scenario not in scenarios:
            rows.append(row(scenario, 'result', None, None, 'unmeasured',
                            'scenario is not in the baseline; run --update-baseline'))
    return rows


def failed(rows):
    return [entry for entry in rows if entry['status'] in ('regressed', 'missing')]


def render(value):
    return '-' if value is None else f'{value:.3f}'


def format_table(rows):
    header = ('scenario', 'metric', 'baseline', 'current', 'change', 'result', 'detail')
    table = [header]
    for entry in rows:
        table.append((
            entry['scenario'],
            LABELS.get(entry['metric'], entry['metric']),
            render(entry['baseline']),
            render(entry['current']),
            '-' if entry['changePercent'] is None else f'{entry["changePercent"]:+.1f}%',
            entry['status'],
            entry['detail'],
        ))
    widths = [max(len(line[column]) for line in table) for column in range(len(header))]
    lines = []
    for line in table:
        cells = [cell.ljust(width) for cell, width in zip(line, widths)]
        lines.append('  '.join(cells).rstrip())
    return '\n'.join(lines)


def load(path):
    with open(path) as handle:
        return json.load(handle)


def collect(directory):
    """Measure every `<scenario>.json` in directory, keyed by scenario name."""
    measured = {}
    for path in sorted(Path(directory).glob('*.json')):
        measured[path.stem] = measure(load(path))
    return measured


def update_baseline(path, baseline, measured):
    scenarios = dict(baseline.get('scenarios') or {})
    for scenario, current in measured.items():
        scenarios[scenario] = {
            'apps': current.get('apps'),
            'usersPerApp': current.get('usersPerApp'),
            'errorCount': current.get('errorCount'),
            **{metric: current.get(metric) for metric in METRICS},
        }
    updated = {}
    if '_note' in baseline:
        updated['_note'] = baseline['_note']
    updated['scenarios'] = scenarios
    Path(path).write_text(json.dumps(updated, indent=2) + '\n')


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--results-dir', required=True,
                        help='directory holding one result.json per scenario, named <scenario>.json')
    parser.add_argument('--baseline', default=str(BASELINE),
                        help='baseline file to compare against or rewrite (default: next to this script)')
    parser.add_argument('--update-baseline', action='store_true',
                        help='rewrite the baseline from the results directory instead of comparing')
    args = parser.parse_args(argv)

    measured = collect(args.results_dir)
    if not measured:
        print(f'no result files in {args.results_dir}', file=sys.stderr)
        return 1
    baseline_path = Path(args.baseline)
    baseline = load(baseline_path) if baseline_path.exists() else {}
    if args.update_baseline:
        update_baseline(baseline_path, baseline, measured)
        print(f'updated baseline {baseline_path} from {len(measured)} scenario(s)')
        return 0

    rows = compare(baseline, measured)
    print(format_table(rows))
    regressions = failed(rows)
    if regressions:
        print(f'weekly scale: REGRESSED ({len(regressions)} check(s) failed)')
        return 1
    print('weekly scale: passed')
    return 0


if __name__ == '__main__':
    sys.exit(main())
