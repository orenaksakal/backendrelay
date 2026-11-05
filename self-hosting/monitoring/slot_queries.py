#!/usr/bin/env python3
"""Render exporter queries with the expected invalidator replica count."""
import argparse
from pathlib import Path
import yaml

HERE = Path(__file__).resolve().parent
EXPECTED_KEY = 'backendrelay_slot_expected'
MAX_REPLICAS = 1000


def render(expected_replicas=1):
    """Return exporter queries with the expected invalidator replica count applied."""
    if (isinstance(expected_replicas, bool) or not isinstance(expected_replicas, int)
            or not 1 <= expected_replicas <= MAX_REPLICAS):
        raise ValueError('expected replicas must be an integer between 1 and 1000')
    doc = yaml.safe_load((HERE / 'postgres-queries.yml').read_text())
    doc[EXPECTED_KEY]['query'] = f'SELECT {expected_replicas}::float8 AS replicas'
    return doc


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--expected-replicas', type=int, default=1,
                        help='invalidator replicas expected to hold an active slot (default: 1)')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    try:
        doc = render(args.expected_replicas)
    except ValueError as error:
        parser.error(str(error))
    with args.output.open('x') as output:
        yaml.safe_dump(doc, output, sort_keys=False)


if __name__ == '__main__':
    main()
