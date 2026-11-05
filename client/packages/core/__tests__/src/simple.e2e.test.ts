import { expect } from 'vitest';
import { i, id, init } from '../../src';
import { apiUrl, e2eTest as test, makeE2ETest } from './utils/e2e';

test('can make a query', async ({ db }) => {
  const result = await db.queryOnce({ todos: {} });

  document.body.innerHTML = '<h1>Hello</h1>';

  expect(result.data.todos.length).toBe(0);
});

const ownershipSchema = i.schema({
  entities: {
    $users: i.entity({ email: i.string().unique().indexed().optional() }),
    todos: i.entity({ title: i.string() }),
  },
  links: {
    todoOwner: {
      forward: { on: 'todos', has: 'one', label: 'owner' },
      reverse: { on: '$users', has: 'many', label: 'todos' },
    },
  },
});

const ownershipTest = makeE2ETest({
  schema: ownershipSchema,
  rules: {
    code: {
      $default: {
        allow: {
          $default: 'false',
          link: { $default: 'false' },
          unlink: { $default: 'false' },
        },
      },
      $users: {
        allow: {
          // These isolated fixtures use guest auth instead of sending email.
          create: 'true',
          view: 'auth.id != null && auth.id == data.id',
          link: {
            todos:
              "auth.id != null && auth.id == data.id && actions.linkedData == 'create'",
          },
        },
      },
      todos: {
        bind: { isOwner: "auth.id != null && auth.id in data.ref('owner.id')" },
        allow: {
          view: 'isOwner',
          create: "isOwner && size(data.ref('owner.id')) == 1",
          link: {
            owner: "linkedData.id == auth.id && actions.data == 'create'",
          },
        },
      },
    },
  },
});

ownershipTest(
  'owner-filtered todos appear live, survive reconnection, and stay private',
  async ({ db, appId }) => {
    let active = db;
    let unsubscribe = () => {};
    try {
      const { user: alice } = await active.auth.signInAsGuest();
      const taskId = id();
      const query = {
        todos: { $: { where: { 'owner.id': alice.id } }, owner: {} },
      };
      let liveIds: string[] | undefined;
      unsubscribe = active.subscribeQuery(query, (result) => {
        if (result.data) liveIds = result.data.todos.map((todo) => todo.id);
      });
      await expect.poll(() => liveIds).toEqual([]);
      await active.transact(
        active.tx.todos[taskId]
          .update({ title: 'Private task' })
          .link({ owner: alice.id }),
      );
      await expect.poll(() => liveIds).toEqual([taskId]);
      const saved = await active.queryOnce(query);
      expect(saved.data.todos.map((todo) => [todo.id, todo.owner?.id])).toEqual(
        [[taskId, alice.id]],
      );

      // A fresh reactor cannot rely on the optimistic relationship triple.
      unsubscribe();
      active.shutdown();
      active = init({
        appId,
        apiURI: apiUrl,
        websocketURI: `${apiUrl.replace(/^http/, 'ws')}/runtime/session`,
        schema: ownershipSchema,
      });
      expect((await active.getAuth())?.id).toBe(alice.id);
      const reloaded = await active.queryOnce(query);
      expect(
        reloaded.data.todos.map((todo) => [todo.id, todo.owner?.id]),
      ).toEqual([[taskId, alice.id]]);

      await active.auth.signOut({ invalidateToken: false });
      expect(
        (await active.queryOnce({ todos: { owner: {} } })).data.todos,
      ).toEqual([]);
      const { user: bob } = await active.auth.signInAsGuest();
      expect((await active.queryOnce(query)).data.todos).toEqual([]);
      const bobTaskId = id();
      await active.transact(
        active.tx.todos[bobTaskId]
          .update({ title: 'Another private task' })
          .link({ owner: bob.id }),
      );
      const bobResult = await active.queryOnce({
        todos: { $: { where: { 'owner.id': bob.id } }, owner: {} },
      });
      expect(
        bobResult.data.todos.map((todo) => [todo.id, todo.owner?.id]),
      ).toEqual([[bobTaskId, bob.id]]);
      await active.auth.signOut();
      await active.auth.signInWithToken(alice.refresh_token);
      const aliceResult = await active.queryOnce({ todos: { owner: {} } });
      expect(
        aliceResult.data.todos.map((todo) => [todo.id, todo.owner?.id]),
      ).toEqual([[taskId, alice.id]]);
    } finally {
      unsubscribe();
      active.shutdown();
    }
  },
);
