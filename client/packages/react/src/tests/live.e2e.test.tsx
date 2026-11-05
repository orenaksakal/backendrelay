import 'fake-indexeddb/auto';

import { init as initAdmin } from '@instantdb/admin';
import { flushSync } from 'react-dom';
import { createRoot } from 'react-dom/client';
import { describe, expect, it } from 'vitest';

import { i, id, init } from '../index.js';

const apiURI = process.env.INSTANT_API_URI;
const enabled = process.env.INSTANT_REACT_LIVE_E2E === '1' && Boolean(apiURI);
const liveTest = enabled ? it : it.skip;
const schema = i.schema({
  entities: {
    goals: i.entity({
      title: i.string(),
    }),
  },
});

async function waitFor(
  assertion: () => void | Promise<void>,
  timeout = 15_000,
) {
  const deadline = Date.now() + timeout;
  let lastError: unknown;
  while (Date.now() < deadline) {
    try {
      await assertion();
      return;
    } catch (error) {
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  }
  throw lastError;
}

async function createEphemeralApp(): Promise<{
  appId: string;
  adminToken: string;
}> {
  const response = await fetch(`${apiURI}/dash/apps/ephemeral`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      title: `react-sdk-e2e-${id()}`,
      schema,
      rules: {
        code: {
          goals: {
            allow: {
              view: 'true',
              create: 'true',
              update: 'true',
            },
          },
        },
      },
    }),
  });
  if (!response.ok) {
    throw new Error(
      `ephemeral app failed: ${response.status} ${await response.text()}`,
    );
  }
  const { app } = await response.json();
  return { appId: app.id, adminToken: app['admin-token'] };
}

describe('React SDK live E2E against Rust', () => {
  liveTest(
    'observes an external write and persists a synced public transaction',
    async () => {
      const { appId, adminToken } = await createEphemeralApp();
      const db = init({
        appId,
        apiURI,
        websocketURI: apiURI!.replace(/^http/, 'ws') + '/runtime/session',
        devtool: false,
        schema,
      });
      const adminDb = initAdmin({
        appId,
        adminToken,
        apiURI,
        schema,
      });
      await waitFor(() => {
        expect(db.core._reactor.status).toBe('authenticated');
      });
      const container = document.createElement('div');
      const root = createRoot(container);
      let query!: ReturnType<typeof db.useQuery<{ goals: {} }>>;

      function QueryHarness() {
        query = db.useQuery({ goals: {} });
        return null;
      }

      try {
        flushSync(() => root.render(<QueryHarness />));
        await waitFor(() => {
          expect(query.isLoading).toBe(false);
          expect(query.data?.goals).toEqual([]);
        });

        const adminTitle = `react-admin-live-${id()}`;
        await adminDb.transact(
          adminDb.tx.goals[id()].update({ title: adminTitle }),
        );
        await waitFor(() => {
          expect(
            query.data?.goals.some((goal) => goal.title === adminTitle),
          ).toBe(true);
        });

        const reactTitle = `react-public-live-${id()}`;
        await waitFor(() => {
          expect(db.core._reactor.status).toBe('authenticated');
        });
        await expect(
          db.transact(db.tx.goals[id()].update({ title: reactTitle })),
        ).resolves.toMatchObject({ status: 'synced' });
        await waitFor(async () => {
          const result = await adminDb.query({ goals: {} });
          expect(result.goals.some((goal) => goal.title === reactTitle)).toBe(
            true,
          );
        });
      } finally {
        root.unmount();
        db.core.shutdown();
      }
    },
    50_000,
  );
});
