import assert from 'node:assert/strict';
import test from 'node:test';
import {
  backupExpiry,
  backupSize,
  selectBackups,
} from '../components/dash/backups/model';
import type { InstantAppBackup } from '../lib/types';
const now = Date.parse('2026-09-08T00:00:00Z');
const backup = (
  id: string,
  expires_at: string | null,
  backup_at = '2026-09-07T00:00:00Z',
): InstantAppBackup => ({
  id,
  expires_at,
  backup_at,
  description: id,
  isn: '1',
  files_size: null,
  db_size: 0,
  uncompressed_size: null,
});
const samples = [
  backup('retained', null),
  backup('soon', '2026-09-09T00:00:00Z'),
  backup('expired', '2026-09-07T00:00:00Z'),
  backup('later', '2026-10-01T00:00:00Z', '2026-09-08T00:00:00Z'),
];
test('non-expiring snapshots remain available and expired snapshots are excluded', () => {
  const result = selectBackups(samples, {
    now,
    search: '',
    filter: 'all',
    sort: 'newest',
  });
  assert.equal(result.length, 3);
  assert.equal(result[0].id, 'later');
  assert.ok(result.some((b) => b.id === 'retained'));
});
test('retention and search filters combine; expiry boundary updates with time', () => {
  assert.deepEqual(
    selectBackups(samples, {
      now,
      search: 'SOON',
      filter: 'expiring',
      sort: 'expiry',
    }).map((b) => b.id),
    ['soon'],
  );
  assert.equal(
    backupExpiry(samples[1], Date.parse('2026-09-09T00:00:00Z')),
    'expired',
  );
  assert.equal(
    selectBackups(samples, {
      now,
      search: 'later',
      filter: 'retained',
      sort: 'newest',
    }).length,
    0,
  );
});
test('unavailable sizes are distinct from empty snapshots', () => {
  assert.equal(backupSize(null), 'Unknown');
  assert.equal(backupSize(0), '0 B');
  assert.equal(backupSize(1024), '1 KiB');
});
test('bad expiry metadata stays visible as unknown rather than silently vanishing', () => {
  assert.equal(backupExpiry(backup('bad', 'invalid'), now), 'unknown');
  assert.equal(
    selectBackups([backup('bad', 'invalid')], {
      now,
      search: '',
      filter: 'all',
      sort: 'newest',
    }).length,
    1,
  );
});
