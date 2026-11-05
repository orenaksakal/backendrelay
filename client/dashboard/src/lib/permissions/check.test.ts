import { HttpResponse, http } from 'msw';
import { describe, expect, it } from 'vitest';
import { server } from '@/test/msw/node';
import {
  actorHeaders,
  defaultTestQuery,
  permissionsChecksFrom,
  runDebugQuery,
  runDebugTransact,
  searchTestAsUsers,
} from './check';

const API = 'http://localhost:8888';

const config = {
  apiBaseUrl: API,
  appId: 'app-1',
  adminToken: 'admin-token',
};

describe('permissions Test-as helpers', () => {
  it('sends the impersonation header that matches the actor', () => {
    expect(actorHeaders({ kind: 'admin' })).toEqual({});
    expect(actorHeaders({ kind: 'guest' })).toEqual({ 'as-guest': 'true' });
    expect(actorHeaders({ kind: 'user', email: ' ada@example.com ' })).toEqual({
      'as-email': 'ada@example.com',
    });
    expect(actorHeaders({ kind: 'token', token: ' tok ' })).toEqual({
      'as-token': 'tok',
    });
  });

  it('normalizes query checks with their program expression', () => {
    const checks = permissionsChecksFrom(
      [
        {
          entity: 'todos',
          id: 'todo-1',
          label: 'view',
          program: {
            action: 'view',
            code: 'x',
            'display-code': 'auth.id != null',
          },
          check: true,
        },
        { entity: 'goals', id: 'goal-1', program: {}, check: false },
      ],
      'query',
    );

    expect(checks).toEqual([
      {
        namespace: 'todos',
        entityId: 'todo-1',
        action: 'view',
        expression: 'auth.id != null',
        allowed: true,
      },
      {
        namespace: 'goals',
        entityId: 'goal-1',
        action: 'view',
        expression: '',
        allowed: false,
      },
    ]);
  });

  it('normalizes transaction checks with the action name', () => {
    const checks = permissionsChecksFrom(
      [
        {
          etype: 'todos',
          eid: 'todo-2',
          action: 'update',
          program: { action: 'update', 'display-code': 'auth.id != null' },
          'check-pass?': true,
          'check-result': true,
        },
        { etype: 'goals', action: 'delete', 'check-pass?': false },
      ],
      'transact',
    );

    expect(checks).toEqual([
      {
        namespace: 'todos',
        entityId: 'todo-2',
        action: 'update',
        expression: 'auth.id != null',
        allowed: true,
      },
      {
        namespace: 'goals',
        entityId: null,
        action: 'delete',
        expression: '',
        allowed: false,
      },
    ]);
  });

  it('falls back to $users when the app has no user namespace', () => {
    expect(JSON.parse(defaultTestQuery(['$default', '$users']))).toEqual({
      $users: {},
    });
    expect(JSON.parse(defaultTestQuery(['$default', 'todos']))).toEqual({
      todos: {},
    });
  });

  it('sends the debug query with the actor header and rules override', async () => {
    let seen:
      | {
          asEmail: string | null;
          authorization: string | null;
          body: unknown;
        }
      | undefined;
    server.use(
      http.post(`${API}/admin/query_perms_check`, async ({ request }) => {
        seen = {
          asEmail: request.headers.get('as-email'),
          authorization: request.headers.get('authorization'),
          body: await request.json(),
        };
        return HttpResponse.json({
          result: { goals: [] },
          'check-results': [],
        });
      }),
    );

    const response = await runDebugQuery(config, {
      query: { goals: {} },
      rules: { goals: { allow: { view: 'auth.id != null' } } },
      actor: { kind: 'user', email: 'ada@example.com' },
    });

    expect(response.result).toEqual({ goals: [] });
    expect(seen).toMatchObject({
      asEmail: 'ada@example.com',
      authorization: 'Bearer admin-token',
      body: {
        query: { goals: {} },
        'rules-override': { goals: { allow: { view: 'auth.id != null' } } },
        'inference?': false,
      },
    });
  });

  it('keeps the transaction dry run unless the caller commits', async () => {
    let body: unknown = undefined;
    server.use(
      http.post(`${API}/admin/transact_perms_check`, async ({ request }) => {
        body = await request.json();
        return HttpResponse.json({
          'tx-id': 1,
          'all-checks-ok?': true,
          'committed?': false,
          'check-results': [],
        });
      }),
    );

    await runDebugTransact(config, {
      steps: [{ op: 'update', etype: 'goals', id: 'goal-1' }],
      actor: { kind: 'guest' },
      commit: false,
    });

    expect(body).toMatchObject({
      steps: [{ op: 'update', etype: 'goals', id: 'goal-1' }],
      'dangerously-commit-tx': false,
    });
  });

  it('searches $users by email and drops rows without one', async () => {
    let where: unknown = undefined;
    server.use(
      http.post(`${API}/admin/query`, async ({ request }) => {
        const body = (await request.json()) as {
          query?: {
            $users?: { $?: { where?: unknown; limit?: number } };
          };
        };
        where = body.query?.$users?.$?.where;
        return HttpResponse.json({
          data: {
            $users: [
              { id: 'user-1', email: 'ada@example.com' },
              { id: 'user-2', email: null },
            ],
          },
        });
      }),
    );

    const users = await searchTestAsUsers(config, 'ada');

    expect(where).toEqual({ email: { $ilike: '%ada%' } });
    expect(users).toEqual([{ id: 'user-1', email: 'ada@example.com' }]);
  });
});
