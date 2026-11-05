import assert from 'node:assert/strict';
import test from 'node:test';

import { oldestVersion } from './oldest-sdk-version.mjs';

function sample(version, value) {
  return {
    metric: { __name__: 'backendrelay_sdk_sessions', version },
    value: [1_700_000_000, String(value)],
  };
}

test('returns the lowest reported semver and ignores unparseable labels', () => {
  assert.equal(
    oldestVersion([
      sample('1.2.0', 4),
      sample('1.0.66', 2),
      sample('other', 10),
      sample('latest', 2),
      sample('0.9.0', 0),
      sample('1.0.0-rc.1', 1),
    ]),
    '1.0.0-rc.1',
  );
});

test('returns undefined without a live version', () => {
  assert.equal(oldestVersion([]), undefined);
  assert.equal(oldestVersion([sample('other', 12)]), undefined);
});
