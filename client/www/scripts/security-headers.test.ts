import assert from 'node:assert/strict';
import test from 'node:test';
import { dashboardSecurityHeaders } from '../lib/security-headers.mjs';

test('production CSP preserves editor workers and hydration while denying embedding and eval', () => {
  const headers = dashboardSecurityHeaders();
  const csp = headers['Content-Security-Policy'];
  assert.match(csp, /worker-src 'self' blob:/);
  assert.match(csp, /https:\/\/cdn.jsdelivr.net/);
  assert.match(csp, /style-src[^;]*https:\/\/cdn.jsdelivr.net/);
  assert.match(csp, /frame-ancestors 'none'/);
  assert.match(csp, /object-src 'none'/);
  assert.doesNotMatch(csp, /unsafe-eval/);
  assert.equal(headers['Referrer-Policy'], 'no-referrer');
  assert.equal(headers['X-Content-Type-Options'], 'nosniff');
  assert.doesNotMatch(csp, /instantdb\.com/);
  assert.doesNotMatch(csp, /googletagmanager/);
});

test('only the development server enables eval for its tooling', () => {
  assert.match(
    dashboardSecurityHeaders(true)['Content-Security-Policy'],
    /unsafe-eval/,
  );
});

test('local fonts need no upstream origin in the font source list', () => {
  const policy = dashboardSecurityHeaders()['Content-Security-Policy'];
  const directives = policy.split('; ');
  assert.ok(
    directives
      .find((value) => value.startsWith('font-src '))
      ?.includes(" 'self'"),
  );
  assert.doesNotMatch(policy, /instantdb\.com/);
  assert.doesNotMatch(policy, /font-src[^;]*\*/);
  assert.match(policy, /frame-ancestors 'none'/);
});

test('explicit storage origin permits only that local object-store origin', () => {
  const policy = dashboardSecurityHeaders(false, 'http://localhost:19000')[
    'Content-Security-Policy'
  ];
  assert.match(
    policy,
    /img-src 'self' data: blob: https: http:\/\/localhost:19000;/,
  );
  assert.doesNotMatch(policy, /unsafe-eval/);
  assert.match(
    dashboardSecurityHeaders()['Content-Security-Policy'],
    /img-src 'self' data: blob: https:;/,
  );
});

test('storage origin rejects directives, credentials, paths and wildcard hosts', () => {
  for (const origin of [
    'https://store.test; script-src *',
    'https://a:secret@store.test',
    'https://store.test/bucket',
    'https://*.store.test',
    'data:',
    'https://store.test?key=value',
    'https://store.test#fragment',
    'https://store.test\n',
  ]) {
    assert.throws(() => dashboardSecurityHeaders(false, origin));
  }
});
