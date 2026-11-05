import { randomUUID } from 'node:crypto';

export const CANARY_ENTITY = 'canary';

export function classifyFailure(error) {
  if (!error) {
    return 'error';
  }
  if (
    error.name === 'TimeoutError' ||
    error.name === 'AbortError' ||
    error.code === 'ETIMEDOUT' ||
    error.code === 'UND_ERR_CONNECT_TIMEOUT'
  ) {
    return 'timeout';
  }
  return 'error';
}

export function websocketUrl(apiUrl, appId) {
  const url = new URL(`${apiUrl}/runtime/session`);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  url.searchParams.set('app_id', appId);
  return url.toString();
}

async function cancelBody(response) {
  try {
    await response.body?.cancel?.();
  } catch {
    // The response body is irrelevant to the probe.
  }
}

async function writeMarker({ apiUrl, appId, adminToken, marker, timeoutMs, fetchImpl, now }) {
  const response = await fetchImpl(`${apiUrl}/admin/transact`, {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${adminToken}`,
      'app-id': appId,
      'content-type': 'application/json',
    },
    body: JSON.stringify({
      steps: [['update', CANARY_ENTITY, marker, { at: now() }]],
    }),
    signal: AbortSignal.timeout(timeoutMs),
  });
  await cancelBody(response);
  if (!response.ok) {
    throw new Error(`canary write failed with status ${response.status}`);
  }
}

/**
 * Writes one canary row, waits for the subscribed refresh carrying its id, and
 * measures only the write-to-refresh interval.
 */
export async function probeRealtime({
  apiUrl,
  appId,
  adminToken,
  timeoutMs,
  fetchImpl = globalThis.fetch,
  WebSocketImpl = globalThis.WebSocket,
  now = Date.now,
  idFactory = randomUUID,
}) {
  const marker = idFactory();
  let socket;
  let timer;
  return new Promise((resolve) => {
    let settled = false;
    let writing = false;
    let writeStarted = 0;

    const finish = (result) => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timer);
      try {
        socket?.close(1000, 'probe finished');
      } catch {
        // The socket may already be closed.
      }
      resolve(result);
    };

    timer = setTimeout(() => finish({ outcome: 'timeout' }), timeoutMs);

    try {
      socket = new WebSocketImpl(websocketUrl(apiUrl, appId));
    } catch (error) {
      finish({ outcome: classifyFailure(error) });
      return;
    }

    socket.addEventListener('open', () => {
      socket.send(
        JSON.stringify({
          op: 'init',
          'app-id': appId,
          '__admin-token': adminToken,
        }),
      );
    });

    socket.addEventListener('error', () => finish({ outcome: 'error' }));
    socket.addEventListener('close', () => finish({ outcome: 'error' }));

    socket.addEventListener('message', (event) => {
      if (settled) {
        return;
      }
      const raw = event.data;
      if (typeof raw !== 'string' || raw.length > 128 * 1024) {
        finish({ outcome: 'error' });
        return;
      }
      let message;
      try {
        message = JSON.parse(raw);
      } catch {
        finish({ outcome: 'error' });
        return;
      }
      if (message.op === 'error') {
        finish({ outcome: 'error' });
        return;
      }
      if (message.op === 'init-ok') {
        socket.send(JSON.stringify({ op: 'add-query', q: { [CANARY_ENTITY]: {} } }));
        return;
      }
      if (message.op === 'add-query-ok' && !writing) {
        writing = true;
        writeStarted = now();
        writeMarker({
          apiUrl,
          appId,
          adminToken,
          marker,
          timeoutMs,
          fetchImpl,
          now,
        }).catch((error) => finish({ outcome: classifyFailure(error) }));
        return;
      }
      if (message.op === 'refresh-ok' && writing && raw.includes(marker)) {
        finish({
          outcome: 'success',
          propagationSeconds: Math.max(0, (now() - writeStarted) / 1000),
        });
      }
    });
  });
}
