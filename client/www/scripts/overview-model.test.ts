import { DashboardRequestError } from '../lib/request';
import test from 'node:test';
import assert from 'node:assert/strict';
import { parseActivity } from '../components/dash/overview/activity';
import type { Request } from '../lib/request';
import {
  startActivityPolling,
  type PollUpdate,
} from '../components/dash/overview/polling';
const tick = () => new Promise<void>((resolve) => setImmediate(resolve));
function timers() {
  let sequence = 0;
  const tasks = new Map<number, { fn: () => void; ms: number }>();
  return {
    tasks,
    schedule: (fn: () => void, ms: number) => {
      const id = ++sequence;
      tasks.set(id, { fn, ms });
      return id;
    },
    cancel: (id: unknown) => {
      tasks.delete(id as number);
    },
    run(ms: number) {
      const entry = [...tasks].find(([, task]) => task.ms === ms);
      assert.ok(entry, `Expected timer at ${ms} ms`);
      tasks.delete(entry[0]);
      entry[1].fn();
    },
  };
}
test('activity validation preserves independent totals and rejects malformed or unsafe counts', () => {
  assert.deepEqual(
    parseActivity({ count: 5, origins: { 'https://portal.example': 2 } }),
    { count: 5, origins: { 'https://portal.example': 2 } },
  );
  for (const value of [
    null,
    {},
    [],
    { count: -1, origins: {} },
    { count: 1, origins: [] },
    { count: 1, origins: { x: '2' } },
    { count: 1, origins: { x: Infinity } },
  ])
    assert.throws(() => parseActivity(value));
});
test('polling prevents overlap and ignores late results after stopping', async () => {
  const clock = timers(),
    updates: PollUpdate[] = [];
  let resolve!: (v: any) => void,
    calls = 0;
  const request: Request = () => {
    calls++;
    return new Promise((r) => {
      resolve = r;
    });
  };
  const poll = startActivityPolling({
    path: '/stats',
    request,
    intervalMs: 5000,
    onUpdate: (u) => updates.push(u),
    ...clock,
  });
  poll.refresh();
  poll.refresh();
  assert.equal(calls, 1);
  poll.stop();
  resolve({ count: 1, origins: {} });
  await tick();
  assert.equal(
    updates.some((u) => u.activity),
    false,
  );
  assert.equal(clock.tasks.size, 0);
});
test('automatic retries stop after three failures and manual retry restarts them', async () => {
  const clock = timers(),
    updates: PollUpdate[] = [];
  let calls = 0;
  const request: Request = async () => {
    calls++;
    throw new DashboardRequestError('Unavailable', 503, {});
  };
  const poll = startActivityPolling({
    path: '/stats',
    request,
    intervalMs: 5000,
    onUpdate: (u) => updates.push(u),
    ...clock,
  });
  await tick();
  clock.run(5000);
  await tick();
  clock.run(5000);
  await tick();
  assert.equal(calls, 3);
  assert.equal(clock.tasks.size, 0);
  assert.equal(updates.at(-2)?.failures, 3);
  poll.refresh();
  await tick();
  assert.equal(calls, 4);
  assert.equal(updates.at(-2)?.failures, 1);
  poll.stop();
});
test('hidden pages pause requests and manual mode has no automatic fetch on resume', async () => {
  const clock = timers();
  let visible = false,
    calls = 0;
  const request: Request = async () => {
    calls++;
    return { count: 0, origins: {} } as any;
  };
  const poll = startActivityPolling({
    path: '/stats',
    request,
    intervalMs: 5000,
    onUpdate: () => {},
    isVisible: () => visible,
    ...clock,
  });
  assert.equal(calls, 0);
  visible = true;
  poll.visibilityChanged();
  await tick();
  assert.equal(calls, 1);
  visible = false;
  poll.visibilityChanged();
  assert.equal(clock.tasks.size, 0);
  poll.stop();
  visible = true;
  const manual = startActivityPolling({
    path: '/stats',
    request,
    intervalMs: 0,
    initialFetch: false,
    onUpdate: () => {},
    isVisible: () => visible,
    ...clock,
  });
  manual.visibilityChanged();
  assert.equal(calls, 1);
  manual.refresh();
  await tick();
  assert.equal(calls, 2);
  assert.equal(clock.tasks.size, 0);
  manual.stop();
});
test('slow requests abort after the timeout and report a recoverable error', async () => {
  const clock = timers(),
    updates: PollUpdate[] = [];
  let signal: AbortSignal | undefined;
  const request: Request = (_path, init) =>
    new Promise((_resolve, reject) => {
      signal = init?.signal as AbortSignal;
      signal.addEventListener('abort', () =>
        reject(new DOMException('Aborted', 'AbortError')),
      );
    });
  const poll = startActivityPolling({
    path: '/stats',
    request,
    intervalMs: 0,
    onUpdate: (u) => updates.push(u),
    ...clock,
  });
  clock.run(15000);
  await tick();
  assert.equal(signal?.aborted, true);
  assert.match(updates.find((u) => u.error)?.error ?? '', /timed out/);
  assert.equal(clock.tasks.size, 0);
  poll.stop();
});

test('revoked activity access clears its snapshot and does not retry on visibility changes', async () => {
  const clock = timers(),
    updates: PollUpdate[] = [];
  let calls = 0;
  const request: Request = async () => {
    if (calls++ === 0)
      return { count: 1, origins: { 'private-origin': 1 } } as any;
    throw new DashboardRequestError('Access revoked', 403, {});
  };
  const poll = startActivityPolling({
    path: '/stats',
    request,
    intervalMs: 5000,
    onUpdate: (update) => updates.push(update),
    ...clock,
  });
  await tick();
  clock.run(5000);
  await tick();
  assert.ok(updates.some((update) => update.activity === null));
  assert.equal(clock.tasks.size, 0);
  poll.visibilityChanged();
  await tick();
  assert.equal(calls, 2);
  poll.stop();
});
