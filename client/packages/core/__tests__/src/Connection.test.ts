import { afterEach, expect, test, vi } from 'vitest';

import {
  type EventSourceConstructor,
  type EventSourceType,
  SSEConnection,
} from '../../src/Connection';

const PUSH_FORMAT_HEADER = 'X-Instant-SSE-Push-Format';

class FakeEventSource implements EventSourceType {
  static OPEN = 1;
  static CONNECTING = 0;
  static CLOSED = 2;

  readonly url: string;
  readyState = FakeEventSource.OPEN;
  onopen = null;
  onmessage = null;
  onerror = null;

  constructor(url: string) {
    this.url = url;
  }

  close() {
    this.readyState = FakeEventSource.CLOSED;
  }
}

function response(status: number, format: string | null = null): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    headers: {
      get(name: string) {
        return name === PUSH_FORMAT_HEADER ? format : null;
      },
    },
  } as Response;
}

function openConnection() {
  const connection = new SSEConnection(
    FakeEventSource as unknown as EventSourceConstructor,
    'https://api.example.com/runtime/sse',
  );
  connection.conn.onmessage?.call(connection.conn, {
    data: JSON.stringify({
      op: 'sse-init',
      'machine-id': 'machine',
      'session-id': 'session',
      'sse-token': 'token',
    }),
  } as MessageEvent);
  return connection;
}

async function waitForCalls(
  fetchMock: ReturnType<typeof vi.fn>,
  count: number,
) {
  await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(count));
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

test('SSE POST batches preserve backlog order and bound each request', async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(200));
  vi.stubGlobal('fetch', fetchMock);
  const connection = openConnection();

  for (let index = 0; index < 102; index++) {
    connection.send({ op: 'message', index, 'client-event-id': `${index}` });
  }

  await waitForCalls(fetchMock, 2);
  const batches = fetchMock.mock.calls.map(([, init]) =>
    JSON.parse(init.body).messages.map((message) => message.index),
  );
  expect(batches).toEqual([
    Array.from({ length: 100 }, (_, index) => index),
    [100, 101],
  ]);
});

test('SSE POST negotiates NDJSON after a successful legacy JSON response', async () => {
  const fetchMock = vi
    .fn()
    .mockResolvedValueOnce(response(200, 'ndjson'))
    .mockResolvedValueOnce(response(200, 'ndjson'));
  vi.stubGlobal('fetch', fetchMock);
  const connection = openConnection();

  connection.send({ op: 'first', 'client-event-id': 'first' });
  await waitForCalls(fetchMock, 1);
  connection.send({ op: 'second', 'client-event-id': 'second' });
  await waitForCalls(fetchMock, 2);

  expect(fetchMock.mock.calls[0][1].headers['Content-Type']).toBe(
    'application/json',
  );
  expect(fetchMock.mock.calls[1][1].headers['Content-Type']).toBe(
    'application/x-ndjson',
  );
  expect(fetchMock.mock.calls[1][1].body).toBe(
    '{"machine_id":"machine","session_id":"session","sse_token":"token"}\n{"op":"second","client-event-id":"second"}\n',
  );
});

test('SSE POST falls back to JSON when an NDJSON request is unsupported', async () => {
  const fetchMock = vi
    .fn()
    .mockResolvedValueOnce(response(200, 'ndjson'))
    .mockResolvedValueOnce(response(415))
    .mockResolvedValueOnce(response(200));
  vi.stubGlobal('fetch', fetchMock);
  const connection = openConnection();
  const onerror = vi.fn();
  connection.onerror = onerror;

  connection.send({ op: 'first', 'client-event-id': 'first' });
  await waitForCalls(fetchMock, 1);
  connection.send({ op: 'second', 'client-event-id': 'second' });
  await waitForCalls(fetchMock, 3);

  expect(fetchMock.mock.calls[1][1].headers['Content-Type']).toBe(
    'application/x-ndjson',
  );
  expect(JSON.parse(fetchMock.mock.calls[2][1].body)).toMatchObject({
    messages: [{ op: 'second', 'client-event-id': 'second' }],
  });
  expect(onerror).not.toHaveBeenCalled();
});

test('SSE POST reports terminal request errors without retrying the batch', async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(500));
  vi.stubGlobal('fetch', fetchMock);
  const connection = openConnection();
  const onerror = vi.fn();
  connection.onerror = onerror;

  connection.send({ op: 'message', 'client-event-id': 'message' });
  await vi.waitFor(() => expect(onerror).toHaveBeenCalledTimes(1));

  expect(fetchMock).toHaveBeenCalledTimes(1);
  expect(connection.conn.readyState).toBe(FakeEventSource.CLOSED);
});

test('SSE POST aborts a stalled fetch at its deadline without replaying or draining the queue', async () => {
  vi.useFakeTimers();
  const fetchMock = vi.fn(
    (_url, init) =>
      new Promise<Response>((_resolve, reject) => {
        init?.signal?.addEventListener('abort', () =>
          reject(new Error('aborted')),
        );
      }),
  );
  vi.stubGlobal('fetch', fetchMock);
  const connection = openConnection();
  connection.onerror = vi.fn();
  connection.onclose = vi.fn();

  connection.send({ op: 'transact', 'client-event-id': 'in-flight' });
  await vi.advanceTimersByTimeAsync(0);
  connection.send({ op: 'transact', 'client-event-id': 'queued' });
  await vi.advanceTimersByTimeAsync(9999);
  expect(connection.onerror).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);

  expect(fetchMock.mock.calls[0][1].signal?.aborted).toBe(true);
  expect(connection.onerror).toHaveBeenCalledTimes(1);
  expect(connection.onclose).toHaveBeenCalledTimes(1);
  expect(connection.isOpen()).toBe(false);
  expect(fetchMock).toHaveBeenCalledTimes(1);
  expect(vi.getTimerCount()).toBe(0);
});

test('SSE close aborts an active push without reporting an error or sending queued messages', async () => {
  vi.useFakeTimers();
  const fetchMock = vi.fn(
    (_url, init) =>
      new Promise<Response>((_resolve, reject) => {
        init?.signal?.addEventListener('abort', () =>
          reject(new Error('aborted')),
        );
      }),
  );
  vi.stubGlobal('fetch', fetchMock);
  const connection = openConnection();
  connection.onerror = vi.fn();
  connection.onclose = vi.fn();
  connection.send({ op: 'transact', 'client-event-id': 'in-flight' });
  await vi.advanceTimersByTimeAsync(0);
  connection.send({ op: 'transact', 'client-event-id': 'queued' });
  connection.close();
  connection.close();
  await vi.runAllTimersAsync();

  expect(fetchMock.mock.calls[0][1].signal?.aborted).toBe(true);
  expect(fetchMock).toHaveBeenCalledTimes(1);
  expect(connection.onerror).not.toHaveBeenCalled();
  expect(connection.onclose).toHaveBeenCalledTimes(1);
  expect(vi.getTimerCount()).toBe(0);
});

test('SSE close before the scheduled flush prevents the push', async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(200));
  vi.stubGlobal('fetch', fetchMock);
  const connection = openConnection();
  connection.send({ op: 'transact', 'client-event-id': 'queued' });
  connection.close();
  await Promise.resolve();
  expect(fetchMock).not.toHaveBeenCalled();
});

test('SSE close before init cancels the init deadline even without an onclose listener', async () => {
  vi.useFakeTimers();
  const connection = new SSEConnection(
    FakeEventSource,
    'https://api.example.com/runtime/sse',
  );
  connection.onerror = vi.fn();
  connection.close();
  await vi.runAllTimersAsync();
  expect(connection.onerror).not.toHaveBeenCalled();
  expect(vi.getTimerCount()).toBe(0);
});

test.each([500, 'network error'])(
  'SSE NDJSON does not replay an ambiguous %s failure',
  async (failure) => {
    vi.useFakeTimers();
    const fetchMock = vi.fn().mockResolvedValueOnce(response(200, 'ndjson'));
    if (typeof failure === 'number') {
      fetchMock.mockResolvedValueOnce(response(failure));
    } else {
      fetchMock.mockRejectedValueOnce(new Error(failure));
    }
    vi.stubGlobal('fetch', fetchMock);
    const connection = openConnection();
    connection.onerror = vi.fn();
    connection.onclose = vi.fn();
    connection.send({ op: 'first', 'client-event-id': 'first' });
    await vi.advanceTimersByTimeAsync(0);
    for (let index = 0; index < 101; index++) {
      connection.send({ op: 'transact', 'client-event-id': `${index}` });
    }
    await vi.runAllTimersAsync();
    expect(fetchMock.mock.calls[1][1].headers['Content-Type']).toBe(
      'application/x-ndjson',
    );
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(connection.onerror).toHaveBeenCalledTimes(1);
    expect(connection.onclose).toHaveBeenCalledTimes(1);
    expect(vi.getTimerCount()).toBe(0);
  },
);

test('SSE close ignores a late format rejection instead of sending a fallback', async () => {
  vi.useFakeTimers();
  let resolvePush!: (value: Response) => void;
  const fetchMock = vi
    .fn()
    .mockResolvedValueOnce(response(200, 'ndjson'))
    .mockImplementationOnce(
      () =>
        new Promise<Response>((resolve) => {
          resolvePush = resolve;
        }),
    );
  vi.stubGlobal('fetch', fetchMock);
  const connection = openConnection();
  connection.onerror = vi.fn();
  connection.onclose = vi.fn();
  connection.send({ op: 'first', 'client-event-id': 'first' });
  await vi.advanceTimersByTimeAsync(0);
  expect(vi.getTimerCount()).toBe(0);
  connection.send({ op: 'transact', 'client-event-id': 'second' });
  await vi.advanceTimersByTimeAsync(0);
  connection.close();
  resolvePush(response(415));
  await vi.runAllTimersAsync();
  expect(fetchMock).toHaveBeenCalledTimes(2);
  expect(connection.onerror).not.toHaveBeenCalled();
  expect(connection.onclose).toHaveBeenCalledTimes(1);
  expect(vi.getTimerCount()).toBe(0);
});
