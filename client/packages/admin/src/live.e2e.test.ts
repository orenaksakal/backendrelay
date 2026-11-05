import { afterEach, describe, expect, it } from 'vitest';
import { init, id, i } from './index.ts';

const apiURI = process.env.INSTANT_API_URI;
const enabled = process.env.INSTANT_ADMIN_LIVE_E2E === '1' && !!apiURI;
const test = enabled ? it : it.skip;
const openSubscriptions: Array<{ close: () => void }> = [];
const schema = i.schema({
  entities: {
    $users: i.entity({
      displayName: i.string().optional(),
    }),
  },
});

afterEach(() => {
  for (const subscription of openSubscriptions.splice(0)) {
    subscription.close();
  }
});

async function createEphemeralApp(): Promise<{
  appId: string;
  adminToken: string;
}> {
  const response = await fetch(`${apiURI}/dash/apps/ephemeral`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ title: `admin-sdk-e2e-${id()}`, schema }),
  });
  if (!response.ok) {
    throw new Error(
      `ephemeral app failed: ${response.status} ${await response.text()}`,
    );
  }
  const { app } = await response.json();
  return { appId: app.id, adminToken: app['admin-token'] };
}

function makeDatabase(appId: string, adminToken: string) {
  return init({
    appId,
    adminToken,
    apiURI,
    disableValidation: true,
  });
}

async function nextWithTimeout<T>(
  iterator: AsyncIterator<T>,
): Promise<IteratorResult<T>> {
  return Promise.race([
    iterator.next(),
    new Promise<never>((_, reject) =>
      setTimeout(
        () => reject(new Error('admin SDK subscription timed out')),
        15_000,
      ),
    ),
  ]);
}

describe('Admin SDK live E2E against Rust', () => {
  test('queries, transacts, subscribes, authenticates, and manages storage', async () => {
    const { appId, adminToken } = await createEphemeralApp();
    const db = makeDatabase(appId, adminToken);

    const todoId = id();
    await db.transact(db.tx.todos[todoId].update({ title: 'initial' }));
    const query = await db.query({ todos: {} });
    expect(query.todos).toHaveLength(1);

    const subscription = db.subscribeQuery({ todos: {} });
    openSubscriptions.push(subscription);
    const iterator = subscription[Symbol.asyncIterator]();
    const initial = await nextWithTimeout(iterator);
    expect(initial.done).toBe(false);
    expect(initial.value?.type).toBe('ok');

    await db.transact(db.tx.todos[id()].update({ title: 'live-refresh' }));
    const refreshed = await nextWithTimeout(iterator);
    expect(refreshed.done).toBe(false);
    expect(refreshed.value?.type).toBe('ok');
    expect(
      refreshed.value?.data.todos.some((todo) => todo.title === 'live-refresh'),
    ).toBe(true);

    const email = `admin-sdk-${id()}@example.test`;
    const firstCode = await db.auth.generateMagicCode(email);
    const checked = await db.auth.checkMagicCode(email, firstCode.code, {
      extraFields: { displayName: 'Admin E2E' },
    });
    expect(checked.created).toBe(true);
    expect(checked.user.email).toBe(email);

    const secondCode = await db.auth.generateMagicCode(email);
    const verified = await db.auth.verifyMagicCode(email, secondCode.code);
    expect(verified.email).toBe(email);

    const path = `admin-sdk-e2e/${id()}.txt`;
    await db.storage.uploadFile(path, Buffer.from('rust-admin-e2e'), {
      contentType: 'text/plain',
    });
    const files = await db.storage.list();
    expect(files.some((file) => file.name === path)).toBe(true);
    const downloadURL = await db.storage.getDownloadUrl(path);
    expect(downloadURL).toMatch(/^https?:\/\//);
    const download = await fetch(downloadURL);
    expect(download.status).toBe(200);
    expect(new Uint8Array(await download.arrayBuffer())).toEqual(
      new Uint8Array(Buffer.from('rust-admin-e2e')),
    );
    await db.storage.delete(path);
    expect((await db.storage.list()).some((file) => file.name === path)).toBe(
      false,
    );
  }, 30_000);
});
