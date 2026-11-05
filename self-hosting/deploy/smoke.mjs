#!/usr/bin/env node
// One-shot SDK-protocol query used by the deploy kit's smoke test.
//
// It opens the same runtime WebSocket the official SDK uses, initializes with
// the canary app's admin token, subscribes to one query and waits for the
// server's query acknowledgement. It has no dependencies beyond Node 22's
// global WebSocket and fetch-free runtime, so it can run in a pinned node image.
//
// Environment:
//   SMOKE_API_URL          Public API origin, for example https://api.example.com
//   SMOKE_APP_ID           Canary app UUID
//   SMOKE_ADMIN_TOKEN_FILE Root-owned file containing the canary admin token
//   SMOKE_TIMEOUT_SECONDS  Whole-probe timeout (default 30, maximum 120)

import { readFileSync } from 'node:fs';

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

function fail(message) {
  process.stderr.write(`smoke: ${message}\n`);
  process.exit(1);
}

function required(name) {
  const value = (process.env[name] ?? '').trim();
  if (!value) {
    fail(`${name} is required`);
  }
  return value;
}

const apiUrl = required('SMOKE_API_URL').replace(/\/+$/, '');
const appId = required('SMOKE_APP_ID');
const tokenFile = required('SMOKE_ADMIN_TOKEN_FILE');
const timeoutSeconds = Number(process.env.SMOKE_TIMEOUT_SECONDS || '30');

if (!UUID.test(appId)) {
  fail('SMOKE_APP_ID must be a lowercase UUID');
}
if (!Number.isInteger(timeoutSeconds) || timeoutSeconds < 1 || timeoutSeconds > 120) {
  fail('SMOKE_TIMEOUT_SECONDS must be an integer between 1 and 120');
}

let api;
try {
  api = new URL(apiUrl);
} catch {
  fail('SMOKE_API_URL must be an absolute http(s) URL');
}
if (api.protocol !== 'http:' && api.protocol !== 'https:') {
  fail('SMOKE_API_URL must be an absolute http(s) URL');
}
if (api.username || api.password || api.search || api.hash) {
  fail('SMOKE_API_URL must not contain credentials, a query or a fragment');
}

let adminToken;
try {
  adminToken = readFileSync(tokenFile, 'utf8').trim();
} catch {
  fail(`cannot read SMOKE_ADMIN_TOKEN_FILE at ${tokenFile}`);
}
if (!adminToken) {
  fail('SMOKE_ADMIN_TOKEN_FILE is empty');
}

const socketUrl = new URL(`${apiUrl}/runtime/session`);
socketUrl.protocol = api.protocol === 'https:' ? 'wss:' : 'ws:';
socketUrl.searchParams.set('app_id', appId);

const query = { canary: {} };
let settled = false;

const socket = new WebSocket(socketUrl.toString());
const timer = setTimeout(() => finish(1, `no query result within ${timeoutSeconds}s`), timeoutSeconds * 1000);

function finish(code, message) {
  if (settled) {
    return;
  }
  settled = true;
  clearTimeout(timer);
  try {
    socket.close(1000, 'smoke finished');
  } catch {
    // The socket may already be closed.
  }
  if (code === 0) {
    process.stdout.write(`${JSON.stringify({ status: 'passed', appId })}\n`);
  } else {
    process.stderr.write(`smoke: ${message}\n`);
  }
  // Give the close frame a moment to leave before the container exits.
  setTimeout(() => process.exit(code), 100);
}

socket.addEventListener('open', () => {
  socket.send(
    JSON.stringify({
      op: 'init',
      'app-id': appId,
      'client-event-id': 'deploy-smoke-init',
      '__admin-token': adminToken,
      versions: { '@instantdb/core': 'deploy-smoke' },
    }),
  );
});

socket.addEventListener('message', (event) => {
  if (settled) {
    return;
  }
  if (typeof event.data !== 'string' || event.data.length > 128 * 1024) {
    finish(1, 'server sent a non-text or oversized message');
    return;
  }
  let message;
  try {
    message = JSON.parse(event.data);
  } catch {
    finish(1, 'server sent invalid JSON');
    return;
  }
  switch (message.op) {
    case 'init-ok':
      socket.send(
        JSON.stringify({
          op: 'add-query',
          q: query,
          'client-event-id': 'deploy-smoke-query',
        }),
      );
      break;
    case 'add-query-ok':
    case 'add-query-exists':
      finish(0);
      break;
    case 'init-error':
    case 'add-query-error':
    case 'error':
      finish(1, `server rejected the ${message.op}: ${message.message ?? 'no message'}`);
      break;
    default:
      break;
  }
});

socket.addEventListener('error', () => finish(1, 'WebSocket error'));
socket.addEventListener('close', (event) => {
  finish(1, `WebSocket closed before the query result (code ${event.code})`);
});
