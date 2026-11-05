import { expect, it } from 'vitest';
import { init, id } from '../../src';
import { apiUrl } from './utils/e2e';

declare const __BYOP_APP_ID__: string;
declare const __BYOP_MUTATOR_URL__: string;
declare const __BYOP_MUTATOR_TOKEN__: string;

const enabled =
  !!__BYOP_APP_ID__ && !!__BYOP_MUTATOR_URL__ && !!__BYOP_MUTATOR_TOKEN__;
const test = enabled ? it : it.skip;
const websocketURI = `${apiUrl.replace(/^http/, 'ws')}/runtime/session`;

async function insertVisibleTodo(todoId: string): Promise<void> {
  const response = await fetch(__BYOP_MUTATOR_URL__, {
    method: 'POST',
    headers: { 'content-type': 'text/plain' },
    body: JSON.stringify({
      token: __BYOP_MUTATOR_TOKEN__,
      id: todoId,
      title: 'core-external-wal-refresh',
    }),
  });
  if (!response.ok) {
    throw new Error(
      `BYOP mutation failed: ${response.status} ${await response.text()}`,
    );
  }
}

async function insertOwnedAndForeignTodos(
  ownedId: string,
  foreignId: string,
  ownerId: string,
  ownerAlias: string,
): Promise<void> {
  const response = await fetch(__BYOP_MUTATOR_URL__, {
    method: 'POST',
    headers: { 'content-type': 'text/plain' },
    body: JSON.stringify({
      token: __BYOP_MUTATOR_TOKEN__,
      id: ownedId,
      foreignId,
      ownerId,
      ownerAlias,
      title: 'core-owned-wal-refresh',
    }),
  });
  if (!response.ok) {
    throw new Error(
      `BYOP authenticated mutation failed: ${response.status} ${await response.text()}`,
    );
  }
}

async function rotateOwnedRelationshipAlias(
  todoId: string,
  ownerAlias: string,
): Promise<void> {
  const response = await fetch(__BYOP_MUTATOR_URL__, {
    method: 'POST',
    headers: { 'content-type': 'text/plain' },
    body: JSON.stringify({
      token: __BYOP_MUTATOR_TOKEN__,
      id: todoId,
      ownerAlias,
      title: 'core-rotate-viewer-alias',
    }),
  });
  if (!response.ok) {
    throw new Error(
      `BYOP relationship rotation failed: ${response.status} ${await response.text()}`,
    );
  }
}

async function deleteOwnedAccount(todoId: string): Promise<void> {
  const response = await fetch(__BYOP_MUTATOR_URL__, {
    method: 'POST',
    headers: { 'content-type': 'text/plain' },
    body: JSON.stringify({
      token: __BYOP_MUTATOR_TOKEN__,
      id: todoId,
      title: 'core-delete-owned-account',
    }),
  });
  if (!response.ok) {
    throw new Error(
      `BYOP relationship cascade failed: ${response.status} ${await response.text()}`,
    );
  }
}

async function mutateOwnedTodoOwnerLink(
  todoId: string,
  title: 'core-clear-owner-link' | 'core-restore-owner-link',
): Promise<void> {
  const response = await fetch(__BYOP_MUTATOR_URL__, {
    method: 'POST',
    headers: { 'content-type': 'text/plain' },
    body: JSON.stringify({
      token: __BYOP_MUTATOR_TOKEN__,
      id: todoId,
      title,
    }),
  });
  if (!response.ok) {
    throw new Error(
      `BYOP nullable relationship mutation failed: ${response.status} ${await response.text()}`,
    );
  }
}

async function concurrentlyRotateOwnedRelationship(
  todoId: string,
  ownerAlias: string,
): Promise<void> {
  const response = await fetch(__BYOP_MUTATOR_URL__, {
    method: 'POST',
    headers: { 'content-type': 'text/plain' },
    body: JSON.stringify({
      token: __BYOP_MUTATOR_TOKEN__,
      id: todoId,
      ownerAlias,
      title: 'core-concurrent-relationship-rotation',
    }),
  });
  if (!response.ok) {
    throw new Error(
      `BYOP concurrent relationship rotation failed: ${response.status} ${await response.text()}`,
    );
  }
}

async function injectFailedRelationshipRotation(
  todoId: string,
  ownerAlias: string,
): Promise<void> {
  const response = await fetch(__BYOP_MUTATOR_URL__, {
    method: 'POST',
    headers: { 'content-type': 'text/plain' },
    body: JSON.stringify({
      token: __BYOP_MUTATOR_TOKEN__,
      id: todoId,
      ownerAlias,
      title: 'core-fail-relationship-rotation',
    }),
  });
  if (response.status !== 500) {
    throw new Error(
      `BYOP failure injection returned unexpected status ${response.status}`,
    );
  }
}

async function recoverRelationshipRotation(
  todoId: string,
  ownerAlias: string,
): Promise<void> {
  const response = await fetch(__BYOP_MUTATOR_URL__, {
    method: 'POST',
    headers: { 'content-type': 'text/plain' },
    body: JSON.stringify({
      token: __BYOP_MUTATOR_TOKEN__,
      id: todoId,
      ownerAlias,
      title: 'core-recover-relationship-rotation',
    }),
  });
  if (!response.ok) {
    throw new Error(
      `BYOP relationship recovery failed: ${response.status} ${await response.text()}`,
    );
  }
}

async function restartByopReplication(): Promise<void> {
  const response = await fetch(__BYOP_MUTATOR_URL__, {
    method: 'POST',
    headers: { 'content-type': 'text/plain' },
    body: JSON.stringify({
      token: __BYOP_MUTATOR_TOKEN__,
      id: id(),
      title: 'core-restart-replication',
    }),
  });
  if (!response.ok) {
    throw new Error(
      `BYOP replication restart failed: ${response.status} ${await response.text()}`,
    );
  }
}

async function rotateAfterReplicationReconnect(
  todoId: string,
  ownerAlias: string,
): Promise<void> {
  const response = await fetch(__BYOP_MUTATOR_URL__, {
    method: 'POST',
    headers: { 'content-type': 'text/plain' },
    body: JSON.stringify({
      token: __BYOP_MUTATOR_TOKEN__,
      id: todoId,
      ownerAlias,
      title: 'core-post-reconnect-rotation',
    }),
  });
  if (!response.ok) {
    throw new Error(
      `BYOP post-reconnect rotation failed: ${response.status} ${await response.text()}`,
    );
  }
}

async function applyContendedRelationshipRotation(
  todoId: string,
  ownerAlias: string,
): Promise<void> {
  const response = await fetch(__BYOP_MUTATOR_URL__, {
    method: 'POST',
    headers: { 'content-type': 'text/plain' },
    body: JSON.stringify({
      token: __BYOP_MUTATOR_TOKEN__,
      id: todoId,
      ownerAlias,
      title: 'core-contended-relationship-rotation',
    }),
  });
  if (!response.ok) {
    throw new Error(
      `BYOP contended relationship rotation failed: ${response.status} ${await response.text()}`,
    );
  }
}

function watchTodos(
  db: ReturnType<typeof init>,
  query: { todos: { owners: { accounts: Record<string, never> } } },
): {
  waitFor: (
    predicate: (todos: Array<{ title: string }>) => boolean,
  ) => Promise<Array<{ title: string }>>;
  unsubscribe: () => void;
} {
  type Todos = Array<{ title: string }>;
  type Waiter = {
    predicate: (todos: Todos) => boolean;
    resolve: (todos: Todos) => void;
    reject: (error: Error) => void;
    timeout: ReturnType<typeof setTimeout>;
  };
  let latest: Todos | undefined;
  let failure: Error | undefined;
  const waiters: Waiter[] = [];
  const unsubscribe = db.subscribeQuery(query, (result) => {
    if (result.error) {
      failure = new Error(result.error.message);
      for (const waiter of waiters.splice(0)) {
        clearTimeout(waiter.timeout);
        waiter.reject(failure);
      }
      return;
    }
    latest = result.data.todos;
    for (let index = waiters.length - 1; index >= 0; index -= 1) {
      const waiter = waiters[index];
      if (waiter.predicate(latest)) {
        waiters.splice(index, 1);
        clearTimeout(waiter.timeout);
        waiter.resolve(latest);
      }
    }
  });
  return {
    waitFor: (predicate) => {
      if (failure) return Promise.reject(failure);
      if (latest && predicate(latest)) return Promise.resolve(latest);
      return new Promise((resolve, reject) => {
        const waiter: Waiter = {
          predicate,
          resolve,
          reject,
          timeout: setTimeout(() => {
            const index = waiters.indexOf(waiter);
            if (index >= 0) waiters.splice(index, 1);
            reject(new Error('Core BYOP subscription timed out'));
          }, 20_000),
        };
        waiters.push(waiter);
      });
    },
    unsubscribe,
  };
}

function watchAccounts(
  db: ReturnType<typeof init>,
  query: {
    accounts: {
      owners: { todos: Record<string, never> };
      profiles: Record<string, never>;
    };
  },
): {
  waitFor: (
    predicate: (accounts: Array<{ user_id: string }>) => boolean,
  ) => Promise<Array<{ user_id: string }>>;
  unsubscribe: () => void;
} {
  type Accounts = Array<{ user_id: string }>;
  type Waiter = {
    predicate: (accounts: Accounts) => boolean;
    resolve: (accounts: Accounts) => void;
    reject: (error: Error) => void;
    timeout: ReturnType<typeof setTimeout>;
  };
  let latest: Accounts | undefined;
  let failure: Error | undefined;
  const waiters: Waiter[] = [];
  const unsubscribe = db.subscribeQuery(query, (result) => {
    if (result.error) {
      failure = new Error(result.error.message);
      for (const waiter of waiters.splice(0)) {
        clearTimeout(waiter.timeout);
        waiter.reject(failure);
      }
      return;
    }
    latest = result.data.accounts;
    for (let index = waiters.length - 1; index >= 0; index -= 1) {
      const waiter = waiters[index];
      if (waiter.predicate(latest)) {
        waiters.splice(index, 1);
        clearTimeout(waiter.timeout);
        waiter.resolve(latest);
      }
    }
  });
  return {
    waitFor: (predicate) => {
      if (failure) return Promise.reject(failure);
      if (latest && predicate(latest)) return Promise.resolve(latest);
      return new Promise((resolve, reject) => {
        const waiter: Waiter = {
          predicate,
          resolve,
          reject,
          timeout: setTimeout(() => {
            const index = waiters.indexOf(waiter);
            if (index >= 0) waiters.splice(index, 1);
            reject(new Error('Core reverse BYOP subscription timed out'));
          }, 20_000),
        };
        waiters.push(waiter);
      });
    },
    unsubscribe,
  };
}

test('applies custom auth.ref and data.ref rules across BYOP WAL refreshes', async () => {
  const db = init({
    appId: __BYOP_APP_ID__,
    apiURI: apiUrl,
    websocketURI,
  });
  const query = { todos: { owners: { accounts: {} } } };
  const reverseQuery = { accounts: { owners: { todos: {} }, profiles: {} } };

  try {
    const initial = await db.queryOnce(query);
    expect(initial.data.todos.map((todo) => todo.title)).toEqual(['seed']);

    const anonymous = watchTodos(db, query);

    try {
      await anonymous.waitFor((todos) =>
        todos.some((todo) => todo.title === 'seed'),
      );
      await insertVisibleTodo(id());
      await anonymous.waitFor((todos) =>
        todos.some((todo) => todo.title === 'core-external-wal-refresh'),
      );
    } finally {
      anonymous.unsubscribe();
    }

    const username = `byop-user-${id()}`;
    const rotatedUsername = `byop-user-${id()}`;
    const concurrentUsername = `byop-user-${id()}`;
    const recoveryUsername = `byop-user-${id()}`;
    const reconnectUsername = `byop-user-${id()}`;
    const contentionUsername = `byop-user-${id()}`;
    const { user } = await db.auth.signInAsGuest({
      extraFields: { username },
    });
    expect(user.isGuest).toBe(true);
    const teamId = id();
    const rotatedTeamId = id();
    const concurrentTeamId = id();
    const recoveryTeamId = id();
    const reconnectTeamId = id();
    const contentionTeamId = id();
    await db.transact([
      db.tx.teams[teamId].update({ slug: username }),
      db.tx.teams[rotatedTeamId].update({ slug: rotatedUsername }),
      db.tx.teams[concurrentTeamId].update({ slug: concurrentUsername }),
      db.tx.teams[recoveryTeamId].update({ slug: recoveryUsername }),
      db.tx.teams[reconnectTeamId].update({ slug: reconnectUsername }),
      db.tx.teams[contentionTeamId].update({ slug: contentionUsername }),
      db.tx.$users[user.id].link({
        teams: [
          teamId,
          rotatedTeamId,
          concurrentTeamId,
          recoveryTeamId,
          reconnectTeamId,
          contentionTeamId,
        ],
      }),
    ]);

    const authenticated = watchTodos(db, query);
    const reverse = watchAccounts(db, reverseQuery);
    try {
      await authenticated.waitFor((todos) =>
        todos.some((todo) => todo.title === 'core-external-wal-refresh'),
      );
      await reverse.waitFor((accounts) => accounts.length === 0);
      const ownedId = id();
      const foreignId = id();
      await insertOwnedAndForeignTodos(ownedId, foreignId, user.id, username);
      const todos = await authenticated.waitFor((rows) =>
        rows.some((todo) => todo.title === 'core-owned-wal-refresh'),
      );
      expect(
        todos.some((todo) => todo.title === 'core-owned-wal-refresh'),
      ).toBe(true);
      expect(todos.some((todo) => todo.title === 'core-foreign-hidden')).toBe(
        false,
      );
      const owned = todos.find(
        (todo) => todo.title === 'core-owned-wal-refresh',
      ) as
        | ({
            owners?:
              | {
                  accounts?: { user_id?: string } | Array<{ user_id?: string }>;
                }
              | Array<{
                  accounts?: { user_id?: string } | Array<{ user_id?: string }>;
                }>;
          } & {
            title: string;
          })
        | undefined;
      const owners = Array.isArray(owned?.owners)
        ? owned.owners
        : owned?.owners
          ? [owned.owners]
          : [];
      const accounts = owners.flatMap((owner) =>
        Array.isArray(owner.accounts)
          ? owner.accounts
          : owner.accounts
            ? [owner.accounts]
            : [],
      );
      expect(accounts.map((account) => account.user_id)).toEqual([user.id]);

      const reverseAccounts = await reverse.waitFor((rows) =>
        rows.some((account) => account.user_id === user.id),
      );
      expect(reverseAccounts).toHaveLength(1);
      const reverseAccount = reverseAccounts[0] as {
        user_id: string;
        profiles?: Array<{ viewer_alias?: string }> | { viewer_alias?: string };
        owners?:
          | Array<{
              todos?: Array<{ title?: string }> | { title?: string };
            }>
          | {
              todos?: Array<{ title?: string }> | { title?: string };
            };
      };
      const reverseOwners = Array.isArray(reverseAccount.owners)
        ? reverseAccount.owners
        : reverseAccount.owners
          ? [reverseAccount.owners]
          : [];
      const reverseTodos = reverseOwners.flatMap((owner) =>
        Array.isArray(owner.todos)
          ? owner.todos
          : owner.todos
            ? [owner.todos]
            : [],
      );
      expect(reverseTodos.map((todo) => todo.title)).toEqual([
        'core-owned-wal-refresh',
      ]);
      const profiles = Array.isArray(reverseAccount.profiles)
        ? reverseAccount.profiles
        : reverseAccount.profiles
          ? [reverseAccount.profiles]
          : [];
      expect(profiles.map((profile) => profile.viewer_alias)).toEqual([
        username,
      ]);

      await db.transact(db.tx.$users[user.id].unlink({ teams: teamId }));
      await reverse.waitFor((accounts) => accounts.length === 0);

      await rotateOwnedRelationshipAlias(ownedId, rotatedUsername);
      const rotatedAccounts = await reverse.waitFor((rows) =>
        rows.some((account) => account.user_id === user.id),
      );
      expect(rotatedAccounts).toHaveLength(1);
      const rotatedAccount = rotatedAccounts[0] as {
        profiles?: Array<{ viewer_alias?: string }> | { viewer_alias?: string };
      };
      const rotatedProfiles = Array.isArray(rotatedAccount.profiles)
        ? rotatedAccount.profiles
        : rotatedAccount.profiles
          ? [rotatedAccount.profiles]
          : [];
      expect(rotatedProfiles.map((profile) => profile.viewer_alias)).toEqual([
        rotatedUsername,
      ]);

      await mutateOwnedTodoOwnerLink(ownedId, 'core-clear-owner-link');
      await reverse.waitFor((accounts) => accounts.length === 0);
      const afterClear = await authenticated.waitFor(
        (todos) =>
          !todos.some((todo) => todo.title === 'core-owned-wal-refresh'),
      );
      expect(
        afterClear.some((todo) => todo.title === 'core-external-wal-refresh'),
      ).toBe(true);

      await mutateOwnedTodoOwnerLink(ownedId, 'core-restore-owner-link');
      await reverse.waitFor((rows) =>
        rows.some((account) => account.user_id === user.id),
      );
      const afterRestore = await authenticated.waitFor((todos) =>
        todos.some((todo) => todo.title === 'core-owned-wal-refresh'),
      );
      expect(
        afterRestore.some((todo) => todo.title === 'core-external-wal-refresh'),
      ).toBe(true);

      await concurrentlyRotateOwnedRelationship(ownedId, concurrentUsername);
      const concurrentAccounts = await reverse.waitFor((rows) =>
        rows.some((account) => {
          const accountProfiles = (
            account as {
              profiles?:
                | Array<{ viewer_alias?: string }>
                | { viewer_alias?: string };
            }
          ).profiles;
          const values = Array.isArray(accountProfiles)
            ? accountProfiles
            : accountProfiles
              ? [accountProfiles]
              : [];
          return values.some(
            (profile) => profile.viewer_alias === concurrentUsername,
          );
        }),
      );
      expect(concurrentAccounts).toHaveLength(1);
      const afterConcurrentRotation = await authenticated.waitFor((todos) =>
        todos.some((todo) => todo.title === 'core-owned-concurrent-restored'),
      );
      expect(
        afterConcurrentRotation.some(
          (todo) => todo.title === 'core-owned-wal-refresh',
        ),
      ).toBe(false);
      expect(
        afterConcurrentRotation.some(
          (todo) => todo.title === 'core-external-wal-refresh',
        ),
      ).toBe(true);

      await injectFailedRelationshipRotation(ownedId, recoveryUsername);
      const afterFailedForward = await db.queryOnce(query);
      expect(
        afterFailedForward.data.todos.some(
          (todo) => todo.title === 'core-owned-concurrent-restored',
        ),
      ).toBe(true);
      expect(
        afterFailedForward.data.todos.some(
          (todo) => todo.title === 'core-owned-failure-staged',
        ),
      ).toBe(false);
      const afterFailedReverse = await db.queryOnce(reverseQuery);
      const rollbackAccount = afterFailedReverse.data.accounts.find(
        (account) => account.user_id === user.id,
      ) as
        | {
            profiles?:
              | Array<{ viewer_alias?: string }>
              | { viewer_alias?: string };
            owners?:
              | Array<{
                  todos?: Array<{ title?: string }> | { title?: string };
                }>
              | {
                  todos?: Array<{ title?: string }> | { title?: string };
                };
          }
        | undefined;
      const rollbackProfiles = Array.isArray(rollbackAccount?.profiles)
        ? rollbackAccount.profiles
        : rollbackAccount?.profiles
          ? [rollbackAccount.profiles]
          : [];
      expect(rollbackProfiles.map((profile) => profile.viewer_alias)).toEqual([
        concurrentUsername,
      ]);
      const rollbackOwners = Array.isArray(rollbackAccount?.owners)
        ? rollbackAccount.owners
        : rollbackAccount?.owners
          ? [rollbackAccount.owners]
          : [];
      const rollbackTodos = rollbackOwners.flatMap((owner) =>
        Array.isArray(owner.todos)
          ? owner.todos
          : owner.todos
            ? [owner.todos]
            : [],
      );
      expect(rollbackTodos.map((todo) => todo.title)).toEqual([
        'core-owned-concurrent-restored',
      ]);

      await recoverRelationshipRotation(ownedId, recoveryUsername);
      await reverse.waitFor((rows) =>
        rows.some((account) => {
          const accountProfiles = (
            account as {
              profiles?:
                | Array<{ viewer_alias?: string }>
                | { viewer_alias?: string };
            }
          ).profiles;
          const values = Array.isArray(accountProfiles)
            ? accountProfiles
            : accountProfiles
              ? [accountProfiles]
              : [];
          return values.some(
            (profile) => profile.viewer_alias === recoveryUsername,
          );
        }),
      );
      const afterRecovery = await authenticated.waitFor((todos) =>
        todos.some((todo) => todo.title === 'core-owned-failure-recovered'),
      );
      expect(
        afterRecovery.some(
          (todo) => todo.title === 'core-owned-concurrent-restored',
        ),
      ).toBe(false);
      expect(
        afterRecovery.some(
          (todo) => todo.title === 'core-external-wal-refresh',
        ),
      ).toBe(true);

      await restartByopReplication();
      await rotateAfterReplicationReconnect(ownedId, reconnectUsername);
      const afterReconnectAccounts = await reverse.waitFor((rows) =>
        rows.some((account) => {
          const accountProfiles = (
            account as {
              profiles?:
                | Array<{ viewer_alias?: string }>
                | { viewer_alias?: string };
            }
          ).profiles;
          const values = Array.isArray(accountProfiles)
            ? accountProfiles
            : accountProfiles
              ? [accountProfiles]
              : [];
          return values.some(
            (profile) => profile.viewer_alias === reconnectUsername,
          );
        }),
      );
      expect(afterReconnectAccounts).toHaveLength(1);
      const afterReconnect = await authenticated.waitFor((todos) =>
        todos.some((todo) => todo.title === 'core-owned-post-reconnect'),
      );
      expect(
        afterReconnect.some(
          (todo) => todo.title === 'core-owned-failure-recovered',
        ),
      ).toBe(false);
      expect(
        afterReconnect.some(
          (todo) => todo.title === 'core-external-wal-refresh',
        ),
      ).toBe(true);

      await applyContendedRelationshipRotation(ownedId, contentionUsername);
      const afterContentionAccounts = await reverse.waitFor((rows) =>
        rows.some((account) => {
          const accountProfiles = (
            account as {
              profiles?:
                | Array<{ viewer_alias?: string }>
                | { viewer_alias?: string };
            }
          ).profiles;
          const values = Array.isArray(accountProfiles)
            ? accountProfiles
            : accountProfiles
              ? [accountProfiles]
              : [];
          return values.some(
            (profile) => profile.viewer_alias === contentionUsername,
          );
        }),
      );
      expect(afterContentionAccounts).toHaveLength(1);
      const afterContention = await authenticated.waitFor((todos) =>
        todos.some((todo) => todo.title === 'core-owned-contention-converged'),
      );
      expect(
        afterContention.some(
          (todo) => todo.title === 'core-owned-post-reconnect',
        ),
      ).toBe(false);
      expect(
        afterContention.some(
          (todo) => todo.title === 'core-external-wal-refresh',
        ),
      ).toBe(true);

      await deleteOwnedAccount(ownedId);
      await reverse.waitFor((accounts) => accounts.length === 0);
      const afterCascade = await authenticated.waitFor(
        (todos) =>
          !todos.some(
            (todo) => todo.title === 'core-owned-contention-converged',
          ),
      );
      expect(
        afterCascade.some((todo) => todo.title === 'core-external-wal-refresh'),
      ).toBe(true);
    } finally {
      authenticated.unsubscribe();
      reverse.unsubscribe();
    }
  } finally {
    db.shutdown();
  }
}, 220_000);
