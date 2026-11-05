const assert = require('node:assert/strict');
const { createRequire } = require('node:module');
const { mkdtempSync, writeFileSync, rmSync } = require('node:fs');
const { join, dirname } = require('node:path');
const { tmpdir } = require('node:os');
const test = require('node:test');

const app = createRequire(
  join(__dirname, '../sandbox/react-native-expo/package.json'),
);
const router = createRequire(app.resolve('expo-router/package.json'));
const expo = createRequire(app.resolve('expo/package.json'));
const plugins = createRequire(
  expo.resolve('@expo/config-plugins/package.json'),
);

test('Expo router query strings retain CommonJS parse/stringify and Unicode behavior', () => {
  const query = router('query-string');
  const original = { next: '/café/東京', value: 'a+b & c' };
  assert.deepEqual({ ...query.parse(query.stringify(original)) }, original);
  assert.deepEqual(
    { ...query.parse('tag=one&tag=two&empty=') },
    { empty: '', tag: ['one', 'two'] },
  );
  // Malformed input is accepted by the tolerant decoder without runaway recursion.
  assert.equal(
    typeof query.parse('value=' + '%E0%A4%A'.repeat(1000)).value,
    'string',
  );
});

test('Expo xcode project identifiers retain the expected format and uniqueness', () => {
  const xcode = plugins('xcode');
  const project = xcode.project('unused.pbxproj');
  project.hash = { project: { objects: {} } };
  const identifiers = new Set(
    Array.from({ length: 100 }, () => project.generateUuid()),
  );
  assert.equal(identifiers.size, 100);
  assert.ok([...identifiers].every((value) => /^[A-F0-9]{24}$/.test(value)));
});

test('React Native CLI reads the launcher activity using its upgraded XML parser', () => {
  const sdk = createRequire(
    join(__dirname, '../packages/react-native/package.json'),
  );
  const cli = createRequire(
    sdk.resolve('@react-native-community/cli/package.json'),
  );
  const doctor = createRequire(
    cli.resolve('@react-native-community/cli-doctor/package.json'),
  );
  const platform = createRequire(
    doctor.resolve('@react-native-community/cli-platform-android/package.json'),
  );
  const configPath = platform.resolve(
    '@react-native-community/cli-config-android/package.json',
  );
  const getMainActivity = require(
    join(dirname(configPath), 'build/config/getMainActivity.js'),
  ).default;
  const root = mkdtempSync(join(tmpdir(), 'backendrelay-mobile-contract-'));
  try {
    const manifest = join(root, 'AndroidManifest.xml');
    writeFileSync(
      manifest,
      '<manifest><application><activity android:name=".MainActivity"><intent-filter><action android:name="android.intent.action.MAIN"/><category android:name="android.intent.category.LAUNCHER"/></intent-filter></activity></application></manifest>',
    );
    assert.equal(getMainActivity(manifest), '.MainActivity');
  } finally {
    rmSync(root, { recursive: true });
  }
});
