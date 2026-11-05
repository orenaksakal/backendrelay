import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

test('route wrappers agree with the authoritative virtual route configuration', () => {
  const config = JSON.parse(
    readFileSync(new URL('../virtual-routes.json', import.meta.url), 'utf8'),
  );
  for (const route of config.children) {
    const expected = route.type === 'index' ? '/' : route.path;
    const source = readFileSync(
      new URL(`../src/routes/${route.file}`, import.meta.url),
      'utf8',
    );
    const actual = source.match(/createFileRoute\(\s*(['"])(.*?)\1\s*\)/)?.[2];
    assert.equal(
      actual,
      expected,
      `${route.file} was generated with a stale route path`,
    );
  }
});
