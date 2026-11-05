import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { describe, expect, it } from 'vitest';
import { i, init, id } from './index.ts';

const clientSchema = i.schema({
  entities: {
    accounts: i.entity({ user_id: i.string() }),
    profiles: i.entity({ viewer_alias: i.string() }),
    todos: i.entity({ title: i.string() }),
  },
  links: {
    accountProfile: {
      forward: { on: 'profiles', has: 'one', label: 'accounts' },
      reverse: { on: 'accounts', has: 'one', label: 'profiles' },
    },
  },
});

const run = promisify(execFile);
const apiURI = process.env.INSTANT_API_URI;
const appId = process.env.INSTANT_BYOP_APP_ID;
const adminToken = process.env.INSTANT_BYOP_ADMIN_TOKEN;
const composeFile = process.env.INSTANT_BYOP_COMPOSE_FILE;
const psqlURL = process.env.INSTANT_BYOP_PSQL_URL;
const psqlBin = process.env.INSTANT_BYOP_PSQL_BIN ?? 'psql';
const schema = process.env.INSTANT_BYOP_SCHEMA;
const enabled =
  process.env.INSTANT_BYOP_LIVE_E2E === '1' &&
  !!apiURI &&
  !!appId &&
  !!adminToken &&
  (!!composeFile || !!psqlURL) &&
  !!schema;
const test = enabled ? it : it.skip;

async function nextWithTimeout<T>(
  iterator: AsyncIterator<T>,
): Promise<IteratorResult<T>> {
  return Promise.race([
    iterator.next(),
    new Promise<never>((_, reject) =>
      setTimeout(
        () => reject(new Error('BYOP Admin SDK subscription timed out')),
        20_000,
      ),
    ),
  ]);
}

async function insertTodo(todoId: string, title: string): Promise<void> {
  // The harness controls `schema`; UUID values are generated locally. Keep the
  // mutation outside Instant so a passing refresh proves wal2json invalidation.
  if (!/^[a-z_][a-z0-9_]*$/.test(schema!)) {
    throw new Error('INSTANT_BYOP_SCHEMA is not a safe PostgreSQL identifier');
  }
  if (
    !/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
      todoId,
    )
  ) {
    throw new Error('generated todo id is not a UUID');
  }
  if (title !== 'external-wal-refresh') {
    throw new Error('unexpected BYOP fixture title');
  }
  const sql = `INSERT INTO ${schema}.todos (id, title) VALUES ('${todoId}', 'external-wal-refresh')`;
  if (psqlURL) {
    const parsed = new URL(psqlURL);
    const database = decodeURIComponent(parsed.pathname.slice(1));
    if (
      (parsed.protocol !== 'postgres:' && parsed.protocol !== 'postgresql:') ||
      !parsed.hostname ||
      !database ||
      database.includes('/')
    ) {
      throw new Error('INSTANT_BYOP_PSQL_URL is not a PostgreSQL URL');
    }
    const env: NodeJS.ProcessEnv = {
      ...process.env,
      PGHOST: parsed.hostname,
      PGPORT: parsed.port || '5432',
      PGDATABASE: database,
    };
    if (parsed.username) env.PGUSER = decodeURIComponent(parsed.username);
    if (parsed.password) env.PGPASSWORD = decodeURIComponent(parsed.password);
    const sslmode = parsed.searchParams.get('sslmode');
    if (sslmode) env.PGSSLMODE = sslmode;
    await run(psqlBin, ['-v', 'ON_ERROR_STOP=1', '-c', sql], {
      env,
    });
    return;
  }
  await run('docker', [
    'compose',
    '-f',
    composeFile!,
    'exec',
    '-T',
    'postgres',
    'psql',
    '-U',
    process.env.INSTANT_BYOP_PSQL_USER ?? 'instant',
    '-d',
    process.env.INSTANT_BYOP_PSQL_DATABASE ?? 'instant',
    '-v',
    'ON_ERROR_STOP=1',
    '-c',
    sql,
  ]);
}

describe('Admin SDK live BYOP E2E against Rust', () => {
  test('queries PostgreSQL and refreshes after an external WAL commit', async () => {
    const db = init({
      appId: appId!,
      adminToken: adminToken!,
      apiURI,
      schema: clientSchema,
      disableValidation: true,
    });

    const initialQuery = await db.query({ todos: {} });
    expect(initialQuery.todos.some((todo) => todo.title === 'seed')).toBe(true);
    const relationshipQuery = await db.query({ accounts: { profiles: {} } });
    const seededAccount = relationshipQuery.accounts.find(
      (account) => account.user_id === '00000000-0000-4000-8000-000000000098',
    );
    expect(seededAccount?.profiles).toEqual(
      expect.objectContaining({ viewer_alias: 'seed-profile' }),
    );
    expect(Array.isArray(seededAccount?.profiles)).toBe(false);

    const subscription = db.subscribeQuery({ todos: {} });
    const iterator = subscription[Symbol.asyncIterator]();
    try {
      const initial = await nextWithTimeout(iterator);
      expect(initial.done).toBe(false);
      expect(initial.value?.type).toBe('ok');

      await insertTodo(id(), 'external-wal-refresh');
      const refreshed = await nextWithTimeout(iterator);
      expect(refreshed.done).toBe(false);
      expect(refreshed.value?.type).toBe('ok');
      expect(
        refreshed.value?.data.todos.some(
          (todo) => todo.title === 'external-wal-refresh',
        ),
      ).toBe(true);
    } finally {
      subscription.close();
    }
  }, 30_000);
});
