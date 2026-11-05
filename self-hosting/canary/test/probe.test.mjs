import assert from 'node:assert/strict';
import test from 'node:test';

import { classifyFailure, websocketUrl } from '../src/probe.mjs';

test('classifies probe failures by outcome', () => {
  assert.equal(classifyFailure({ name: 'TimeoutError' }), 'timeout');
  assert.equal(classifyFailure({ name: 'AbortError' }), 'timeout');
  assert.equal(classifyFailure({ code: 'ETIMEDOUT' }), 'timeout');
  assert.equal(classifyFailure({ code: 'UND_ERR_CONNECT_TIMEOUT' }), 'timeout');
  assert.equal(classifyFailure(new Error('socket closed')), 'error');
  assert.equal(classifyFailure(undefined), 'error');
});

test('builds the realtime session URL from the public API origin', () => {
  const appId = '22222222-2222-4222-8222-222222222222';
  assert.equal(
    websocketUrl('https://api.example.com', appId),
    `wss://api.example.com/runtime/session?app_id=${appId}`,
  );
  assert.equal(
    websocketUrl('http://127.0.0.1:8888', appId),
    `ws://127.0.0.1:8888/runtime/session?app_id=${appId}`,
  );
});
