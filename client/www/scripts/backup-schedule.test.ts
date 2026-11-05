import { test } from 'node:test';
import assert from 'node:assert/strict';
import { nextBackupOccurrence } from '../components/dash/backups/schedule';
const now = Date.parse('2026-09-12T17:19:34Z');
test('backup presets compute exact UTC boundaries', () => {
  assert.equal(
    nextBackupOccurrence('0 * * * *', now),
    '2026-09-12T18:00:00.000Z',
  );
  assert.equal(
    nextBackupOccurrence('0 0 * * *', now),
    '2026-09-13T00:00:00.000Z',
  );
  assert.equal(
    nextBackupOccurrence('0 0 * * 0', now),
    '2026-09-13T00:00:00.000Z',
  );
  assert.equal(
    nextBackupOccurrence('0 */6 * * *', now),
    '2026-09-12T18:00:00.000Z',
  );
});
test('cron supports lists and ranges and rejects invalid input', () => {
  assert.equal(
    nextBackupOccurrence('0,30 9-18 * * 1-5', now),
    '2026-09-14T09:00:00.000Z',
  );
  for (const value of ['bad', '60 * * * *', '*/0 * * * *', '0 0 31 2 *'])
    assert.throws(() => nextBackupOccurrence(value, now));
});
test('cron uses OR when both calendar day fields are restricted', () => {
  assert.equal(
    nextBackupOccurrence('0 0 15 * 0', now),
    '2026-09-13T00:00:00.000Z',
  );
});
