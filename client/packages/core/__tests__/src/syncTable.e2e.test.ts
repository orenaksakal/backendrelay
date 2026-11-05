import { expect } from 'vitest';
import { SyncTableCallbackEventType, i, id, init } from '../../src';
import { apiUrl, makeE2ETest } from './utils/e2e';

const schema = i.schema({
  entities: {
    goals: i.entity({
      title: i.string(),
    }),
    users: i.entity({
      name: i.string(),
    }),
    posts: i.entity({
      title: i.string(),
    }),
  },
  links: {
    usersPosts: {
      forward: { on: 'users', has: 'many', label: 'posts' },
      reverse: { on: 'posts', has: 'one', label: 'author' },
    },
  },
});

const test = makeE2ETest({
  schema,
  rules: {
    code: {
      goals: {
        allow: { view: 'true', create: 'true', update: 'true' },
      },
      users: {
        allow: { view: 'true', create: 'true', update: 'true' },
      },
      posts: {
        allow: {
          view: "data.title == 'published'",
          create: 'true',
          update: 'true',
        },
      },
    },
  },
});
const websocketURI = `${apiUrl.replace(/^http/, 'ws')}/runtime/session`;

async function withTimeout<T>(promise: Promise<T>, label: string): Promise<T> {
  let timeout: ReturnType<typeof setTimeout>;
  try {
    return await Promise.race([
      promise,
      new Promise<never>((_, reject) => {
        timeout = setTimeout(
          () => reject(new Error(`Timed out waiting for ${label}`)),
          5000,
        );
      }),
    ]);
  } finally {
    clearTimeout(timeout!);
  }
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, reject, resolve };
}

test('SyncTable completes its initial load and receives a mutation', async ({
  db,
  appId,
  adminToken,
}) => {
  const initialId = id();
  await db.queryOnce({ goals: {} });
  const seed = await db.transact(
    db.tx.goals[initialId].update({ title: 'initial' }),
  );
  expect(seed.status).toBe('synced');

  const adminDb = init({
    appId,
    apiURI: apiUrl,
    websocketURI,
    schema,
    __adminToken: adminToken,
  } as any);
  const initial = deferred<string[]>();
  const update = deferred<string[]>();
  let initialComplete = false;
  void initial.promise.catch(() => undefined);
  void update.promise.catch(() => undefined);
  const unsubscribe = adminDb._syncTableExperimental({ goals: {} }, (event) => {
    if (event.type === SyncTableCallbackEventType.Error) {
      const error = new Error(event.error.message);
      initial.reject(error);
      update.reject(error);
    } else if (event.type === SyncTableCallbackEventType.InitialSyncComplete) {
      const titles = event.data.goals.map((goal) => goal.title).sort();
      if (!initialComplete && titles.length === 1 && titles[0] === 'initial') {
        initialComplete = true;
        initial.resolve(titles);
      } else if (initialComplete && titles.length === 2) {
        update.resolve(titles);
      }
    }
  });

  try {
    await expect(
      withTimeout(initial.promise, 'SyncTable initial completion'),
    ).resolves.toEqual(['initial']);

    await db.transact(db.tx.goals[id()].update({ title: 'updated' }));
    await expect(
      withTimeout(update.promise, 'SyncTable mutation update'),
    ).resolves.toEqual(['initial', 'updated']);
  } finally {
    unsubscribe();
    adminDb.shutdown();
  }
}, 15_000);

test('SyncTable returns combined where and linked results', async ({
  db,
  appId,
  adminToken,
}) => {
  const userId = id();
  const publishedPostId = id();
  await db.transact([
    db.tx.users[userId]
      .update({ name: 'Ada' })
      .link({ posts: publishedPostId }),
    db.tx.posts[publishedPostId].update({ title: 'published' }),
    db.tx.posts[id()].update({ title: 'draft' }),
  ]);

  const adminDb = init({
    appId,
    apiURI: apiUrl,
    websocketURI,
    schema,
    __adminToken: adminToken,
  } as any);
  const initial = deferred<{
    posts: { title: string; author: string | undefined }[];
    users: string[];
  }>();
  void initial.promise.catch(() => undefined);
  const unsubscribe = adminDb._syncTableExperimental(
    {
      posts: { $: { where: { title: 'published' } }, author: {} },
      users: { $: { where: { name: 'Ada' } } },
    },
    (event) => {
      if (event.type === SyncTableCallbackEventType.Error) {
        initial.reject(new Error(event.error.message));
      } else if (
        event.type === SyncTableCallbackEventType.InitialSyncComplete
      ) {
        const result = {
          posts: event.data.posts.map((post) => ({
            title: post.title,
            author: post.author?.name,
          })),
          users: event.data.users.map((user) => user.name),
        };
        if (
          result.posts.length === 1 &&
          result.posts[0].author === 'Ada' &&
          result.users.length === 1
        ) {
          initial.resolve(result);
        }
      }
    },
  );

  try {
    await expect(
      withTimeout(initial.promise, 'SyncTable combined initial completion'),
    ).resolves.toEqual({
      posts: [{ title: 'published', author: 'Ada' }],
      users: ['Ada'],
    });
  } finally {
    unsubscribe();
    adminDb.shutdown();
  }
}, 15_000);

test('SyncTable reapplies guest permissions after data changes', async ({
  db,
}) => {
  const visibleId = id();
  const hiddenId = id();
  await db.transact([
    db.tx.posts[visibleId].update({ title: 'published' }),
    db.tx.posts[hiddenId].update({ title: 'draft' }),
  ]);

  const initial = deferred<string[]>();
  const update = deferred<string[]>();
  let initialComplete = false;
  void initial.promise.catch(() => undefined);
  void update.promise.catch(() => undefined);
  const unsubscribe = db._syncTableExperimental({ posts: {} }, (event) => {
    if (event.type === SyncTableCallbackEventType.Error) {
      const error = new Error(event.error.message);
      initial.reject(error);
      update.reject(error);
    } else if (event.type === SyncTableCallbackEventType.InitialSyncComplete) {
      const titles = event.data.posts.map((post) => post.title).sort();
      if (
        !initialComplete &&
        titles.length === 1 &&
        titles[0] === 'published'
      ) {
        initialComplete = true;
        initial.resolve(titles);
      } else if (initialComplete && titles.length === 2) {
        update.resolve(titles);
      }
    }
  });

  try {
    await expect(
      withTimeout(initial.promise, 'permissioned SyncTable initial completion'),
    ).resolves.toEqual(['published']);

    await db.transact(db.tx.posts[hiddenId].update({ title: 'published' }));
    await expect(
      withTimeout(update.promise, 'permissioned SyncTable refresh'),
    ).resolves.toEqual(['published', 'published']);
  } finally {
    unsubscribe();
  }
}, 15_000);
