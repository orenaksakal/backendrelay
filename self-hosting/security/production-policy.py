#!/usr/bin/env python3
"""Read-only production authentication preflight; nonzero on unsafe live state."""
import argparse
from datetime import datetime, timezone
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import subprocess
import sys
import time

HERE = Path(__file__).resolve().parent


def evaluate(state, trusted_proxy_cidrs='', maximum_hourly_capacity=100):
    errors = []
    capacities, toggles = state.get('capacities'), state.get('durableToggles')
    if not isinstance(capacities, list) or not isinstance(toggles, list):
        return {'status': 'failed', 'reasons': ['invalid live configuration projection']}
    config_app = state.get('configApp')
    if not isinstance(config_app, dict):
        errors.append('config app projection missing')
    else:
        if config_app.get('adminTokens') != 0:
            errors.append('config app still has an admin token')
        if config_app.get('members') != 0:
            errors.append('config app still has members or invites')
        if config_app.get('rules') != {'$default': {'allow': {'$default': 'false'}}}:
            errors.append('config app does not deny all access')
    if len(capacities) > 1 or len(toggles) > 1:
        errors.append('ambiguous duplicate authentication settings')
    # Missing or invalid capacity falls back to 20 in the server. Clojure
    # truthiness is intentional: only explicit false/null disable durability.
    raw = capacities[0] if capacities else None
    capacity = raw if type(raw) is int and 0 < raw <= 2**64 - 1 else 20
    durable = not toggles or (toggles[0] is not False and toggles[0] is not None)
    if not durable:
        errors.append('durable send/verify/custom-sender authentication limits disabled')
    if capacity > maximum_hourly_capacity:
        errors.append('effective authentication capacity exceeds approved hourly ceiling')
    if state.get('durableTablePresent') is not True or state.get('durableTableLogged') is not True:
        errors.append('durable rate-limit table missing or not WAL logged')
    if state.get('databaseReadOnly') is not False:
        errors.append('rate-limit database is read-only or its state is unknown')
    proxies = []
    try:
        for value in trusted_proxy_cidrs.split(','):
            if value.strip():
                network = ipaddress.ip_network(value.strip(), strict=False)
                if network.prefixlen == 0:
                    errors.append('trusted proxy configuration trusts the entire internet')
                proxies.append(str(network))
    except ValueError:
        errors.append('trusted proxy configuration contains an invalid CIDR')
    projection = {'durable': durable, 'hourlyCapacity': capacity,
                  'trustedProxyCidrs': sorted(proxies), 'database': state}
    digest = hashlib.sha256(json.dumps(projection, sort_keys=True).encode()).hexdigest()
    return {'status': 'failed' if errors else 'passed', 'reasons': errors,
            'effectiveDurable': durable, 'effectiveHourlyCapacity': capacity,
            'configurationSha256': digest}


def check_live(maximum_hourly_capacity):
    # libpq PGHOST/PGDATABASE/PGUSER/PGPASSFILE environment keeps credentials out
    # of argv, logs, and evidence. Errors are deliberately not persisted.
    try:
        result = subprocess.run(['psql', '-X', '-q', '-A', '-t', '-v', 'ON_ERROR_STOP=1',
                                 '-f', str(HERE / 'production-policy.sql')],
                                capture_output=True, text=True, timeout=10, check=True)
        state = json.loads(result.stdout)
        return evaluate(state, os.getenv('INSTANT_TRUSTED_PROXY_CIDRS', ''), maximum_hourly_capacity)
    except (OSError, subprocess.SubprocessError, ValueError):
        return {'status': 'failed', 'reasons': ['live configuration check unavailable or invalid']}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--maximum-hourly-capacity', type=int, default=100)
    parser.add_argument('--watch-seconds', type=int, default=0)
    parser.add_argument('--metrics-file', type=Path)
    args = parser.parse_args()
    if args.maximum_hourly_capacity <= 0 or args.watch_seconds < 0:
        parser.error('capacity must be positive and watch interval nonnegative')
    previous = None
    while True:
        result = check_live(args.maximum_hourly_capacity)
        result['schemaVersion'] = 1
        result['checkedAt'] = datetime.now(timezone.utc).isoformat()
        digest = result.get('configurationSha256')
        result['configurationChanged'] = previous is not None and digest != previous
        print(json.dumps(result), flush=True)
        if args.metrics_file:
            temporary = args.metrics_file.with_suffix('.tmp')
            temporary.write_text('# HELP backendrelay_auth_policy_safe Effective durable authentication policy.\n'
                                 '# TYPE backendrelay_auth_policy_safe gauge\n'
                                 f'backendrelay_auth_policy_safe {int(result["status"] == "passed")}\n'
                                 '# HELP backendrelay_auth_policy_checked_seconds Last completed policy check.\n'
                                 '# TYPE backendrelay_auth_policy_checked_seconds gauge\n'
                                 f'backendrelay_auth_policy_checked_seconds {int(time.time())}\n')
            temporary.replace(args.metrics_file)
        if not args.watch_seconds:
            return 0 if result['status'] == 'passed' else 1
        previous = digest
        time.sleep(args.watch_seconds)


if __name__ == '__main__':
    sys.exit(main())
