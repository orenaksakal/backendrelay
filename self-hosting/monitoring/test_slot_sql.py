"""Execute exporter SQL against PostgreSQL using synthetic catalog rows only."""
import json
import os
import subprocess
import unittest
from slot_queries import render


class SlotSQLTests(unittest.TestCase):
    def query(self, rows, name):
        fixture = f"""WITH fixture AS (
          SELECT x.slot_name::name, x.slot_type, x.database::name, x.active,
            x.invalidation_reason, x.wal_status, x.confirmed_flush_lsn::pg_lsn,
            x.restart_lsn::pg_lsn
          FROM jsonb_to_recordset('{json.dumps(rows)}'::jsonb) AS x(
            slot_name text, slot_type text, database text, active bool,
            invalidation_reason text, wal_status text, confirmed_flush_lsn text, restart_lsn text)
        ), results AS ("""
        sql = (fixture + render()[name]['query'].replace('pg_replication_slots', 'fixture')
               + ") SELECT coalesce(json_agg(results), '[]'::json) FROM results;")
        result = subprocess.run([os.environ.get('MONITOR_TEST_PSQL', 'psql'), '-X', '-qAt',
                                 '-v', 'ON_ERROR_STOP=1'], input=sql, text=True, capture_output=True, check=True)
        return json.loads(result.stdout)

    def slot(self, **changes):
        fields = dict(slot_name='invalidator_0123456789abcdef', slot_type='logical',
                      database=os.environ['PGDATABASE'], active=False,
                      invalidation_reason=None, wal_status='reserved',
                      confirmed_flush_lsn='0/10', restart_lsn='0/0')
        fields.update(changes)
        return fields

    def slots(self, rows):
        return {row['slot']: row for row in self.query(rows, 'backendrelay_slot')}

    def test_empty_catalog_reports_zero_counts_and_no_slot_rows(self):
        self.assertEqual(self.query([], 'backendrelay_invalidator_slots'),
                         [{'present': 0, 'active': 0}])
        self.assertEqual(self.query([], 'backendrelay_aggregator_slot'),
                         [{'present': 0, 'active': 0}])
        self.assertEqual(self.query([], 'backendrelay_slot'), [])

    def test_only_invalidator_prefixed_and_aggregator_slots_are_monitored(self):
        rows = [
            self.slot(slot_name='invalidator_aaaaaaaa', active=True),
            self.slot(slot_name='invalidator_bbbbbbbb'),
            self.slot(slot_name='aggregator', active=True),
            self.slot(slot_name='unrelated_slot', active=True),
        ]
        slots = self.slots(rows)
        self.assertEqual(set(slots), {'invalidator_aaaaaaaa', 'invalidator_bbbbbbbb', 'aggregator'})
        self.assertEqual(slots['invalidator_aaaaaaaa']['role'], 'invalidator')
        self.assertEqual(slots['aggregator']['role'], 'aggregator')
        self.assertEqual(self.query(rows, 'backendrelay_invalidator_slots')[0],
                         {'present': 2, 'active': 1})
        self.assertEqual(self.query(rows, 'backendrelay_aggregator_slot')[0],
                         {'present': 1, 'active': 1})

    def test_idle_valid_slot_reports_reserved_status_and_position(self):
        row = self.slots([self.slot()])['invalidator_0123456789abcdef']
        self.assertEqual((row['active'], row['invalid'], row['wrong_database_or_type'], row['uninitialized']),
                         (0, 0, 0, 0))
        self.assertEqual(row['confirmed_bytes'], 16)
        self.assertGreaterEqual(row['retained_bytes'], 0)
        self.assertEqual((row['wal_status_reserved'], row['wal_status_extended'],
                          row['wal_status_unreserved'], row['wal_status_lost']), (1, 0, 0, 0))

    def test_lost_slot_and_wrong_database_are_detected(self):
        slot = self.slot()
        slot.update(wal_status='lost', invalidation_reason='wal_removed', confirmed_flush_lsn=None,
                    database='different_database')
        row = self.slots([slot])['invalidator_0123456789abcdef']
        self.assertEqual((row['invalid'], row['uninitialized'], row['wrong_database_or_type']), (1, 1, 1))
        self.assertEqual((row['wal_status_reserved'], row['wal_status_lost']), (0, 1))

    def test_non_reserved_wal_status_is_reported_as_its_own_gauge(self):
        row = self.slots([self.slot(wal_status='extended')])['invalidator_0123456789abcdef']
        self.assertEqual((row['wal_status_reserved'], row['wal_status_extended']), (0, 1))

    def test_expected_replicas_are_rendered_without_naming_slots(self):
        self.assertEqual(render()['backendrelay_slot_expected']['query'],
                         'SELECT 1::float8 AS replicas')
        rendered = render(3)
        self.assertEqual(rendered['backendrelay_slot_expected']['query'],
                         'SELECT 3::float8 AS replicas')
        self.assertNotIn('backendrelay_slots', rendered)
        self.assertIn("LIKE 'invalidator\\_%' ESCAPE '\\'", rendered['backendrelay_slot']['query'])


if __name__ == '__main__':
    unittest.main()
