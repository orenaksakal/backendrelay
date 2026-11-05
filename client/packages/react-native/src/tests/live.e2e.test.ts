import { describe, expect, it, vi } from 'vitest';

// React Native installs this side-effect polyfill on-device. jsdom already
// exposes Web Crypto, while loading the package in Node would parse Flow-only
// React Native sources that are deliberately not part of this server canary.
vi.mock('react-native-get-random-values', () => ({}));

vi.mock('@react-native-community/netinfo', () => ({
  default: {
    fetch: async () => ({ isConnected: true }),
    addEventListener: (listener: (state: { isConnected: boolean }) => void) => {
      listener({ isConnected: true });
      return () => undefined;
    },
  },
}));

import {
  i,
  id,
  init,
  StoreInterface,
  type StoreInterfaceStoreName,
} from '../index.js';

const apiURI = process.env.INSTANT_API_URI;
const enabled =
  process.env.INSTANT_REACT_NATIVE_LIVE_E2E === '1' && Boolean(apiURI);
const liveTest = enabled ? it : it.skip;
const schema = i.schema({
  entities: {
    goals: i.entity({
      title: i.string(),
    }),
  },
});

class MemoryStore extends StoreInterface {
  private static readonly stores = new Map<string, Map<string, unknown>>();
  private readonly values: Map<string, unknown>;

  constructor(appId: string, storeName: StoreInterfaceStoreName) {
    super(appId, storeName);
    const key = `${appId}:${storeName}`;
    const values = MemoryStore.stores.get(key) ?? new Map<string, unknown>();
    MemoryStore.stores.set(key, values);
    this.values = values;
  }

  async getItem(key: string): Promise<unknown> {
    return this.values.get(key) ?? null;
  }

  async removeItem(key: string): Promise<void> {
    this.values.delete(key);
  }

  async multiSet(keyValuePairs: Array<[string, unknown]>): Promise<void> {
    for (const [key, value] of keyValuePairs) this.values.set(key, value);
  }

  async getAllKeys(): Promise<string[]> {
    return [...this.values.keys()];
  }
}

async function waitFor<T>(
  subscribe: (
    resolve: (value: T) => void,
    reject: (error: Error) => void,
  ) => () => void,
  predicate: (value: T) => boolean,
  timeout = 15_000,
): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    let unsubscribe: () => void = () => undefined;
    const timer = setTimeout(() => {
      unsubscribe();
      reject(new Error('React Native SDK live subscription timed out'));
    }, timeout);
    unsubscribe = subscribe(
      (value) => {
        if (!predicate(value)) return;
        clearTimeout(timer);
        unsubscribe();
        resolve(value);
      },
      (error) => {
        clearTimeout(timer);
        unsubscribe();
        reject(error);
      },
    );
  });
}

async function waitForAuthentication(core: {
  _reactor: { status: string };
}): Promise<void> {
  const deadline = Date.now() + 15_000;
  while (core._reactor.status !== 'authenticated') {
    if (Date.now() >= deadline) {
      throw new Error('React Native SDK connection did not authenticate');
    }
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
}

async function createEphemeralApp(): Promise<string> {
  const response = await fetch(`${apiURI}/dash/apps/ephemeral`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      title: `react-native-sdk-e2e-${id()}`,
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
  return app.id;
}

describe('React Native SDK live E2E against Rust', () => {
  liveTest(
    'observes a WAL refresh and persists a synced public transaction',
    async () => {
      const appId = await createEphemeralApp();
      const config = {
        appId,
        apiURI,
        websocketURI: apiURI!.replace(/^http/, 'ws') + '/runtime/session',
        devtool: false,
        schema,
        Store: MemoryStore,
      };
      const db = init(config);
      const externalDb = init(config);
      await Promise.all([
        waitForAuthentication(db.core),
        waitForAuthentication(externalDb.core),
      ]);
      const subscribeGoals =
        (database: typeof db) =>
        (
          resolve: (goals: Array<{ title: string }>) => void,
          reject: (error: Error) => void,
        ) =>
          database.core.subscribeQuery({ goals: {} }, (result) => {
            if (result.error) {
              reject(new Error(result.error.message));
              return;
            }
            resolve(result.data.goals);
          });
      const localGoals = subscribeGoals(db);
      const externalGoals = subscribeGoals(externalDb);

      try {
        await waitFor(localGoals, (goals) => goals.length === 0);

        const adminTitle = `react-native-admin-live-${id()}`;
        const externalRefresh = waitFor(localGoals, (goals) =>
          goals.some((goal) => goal.title === adminTitle),
        );
        await externalDb.transact(
          externalDb.tx.goals[id()].update({ title: adminTitle }),
        );
        await externalRefresh;

        const nativeTitle = `react-native-public-live-${id()}`;
        await Promise.all([
          waitForAuthentication(db.core),
          waitForAuthentication(externalDb.core),
        ]);
        const peerRefresh = waitFor(externalGoals, (goals) =>
          goals.some((goal) => goal.title === nativeTitle),
        );
        await expect(
          db.transact(db.tx.goals[id()].update({ title: nativeTitle })),
        ).resolves.toMatchObject({ status: 'synced' });
        await peerRefresh;
      } finally {
        db.core.shutdown();
        externalDb.core.shutdown();
      }
    },
    50_000,
  );
});
