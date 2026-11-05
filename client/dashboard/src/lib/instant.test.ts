import { act, renderHook } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { resetSession, signOut } from '@/lib/auth/session';
import {
  APP_CLIENT_IDLE_MS,
  appDataAccess,
  appSchemaFromAttrs,
  closeAllAppClients,
  getAppClient,
  peekAppClient,
  retainAppClient,
  TRANSACTION_ACK_TIMEOUT_MS,
  useAppSchema,
  withTransactionTimeout,
  type AppClientConfig,
  type AppInstantClient,
  type InstantAttr,
} from './instant';

const { initMock } = vi.hoisted(() => ({ initMock: vi.fn() }));

vi.mock('@instantdb/react', () => ({ init: initMock }));

const CONFIG: AppClientConfig = {
  appId: 'app-1',
  apiURI: 'https://api.example.com',
  websocketURI: 'wss://api.example.com/runtime/session',
  adminToken: 'app-1-admin-token',
};

const attrListeners = new Set<(attrs: Record<string, InstantAttr>) => void>();
const querySubscriptions: Record<string, unknown>[] = [];
let queryUnsubscribes = 0;

function createFakeClient(): AppInstantClient {
  return {
    core: {
      shutdown: vi.fn(),
      subscribeQuery: (query) => {
        querySubscriptions.push(query);
        return () => {
          queryUnsubscribes += 1;
        };
      },
      _reactor: {
        subscribeAttrs: (callback) => {
          attrListeners.add(callback);
          return () => {
            attrListeners.delete(callback);
          };
        },
        pushOps: vi.fn(() => Promise.resolve(undefined)),
      },
    },
    tx: {},
    transact: vi.fn(() => Promise.resolve(undefined)),
  };
}

function emitAttrs(attrs: Record<string, InstantAttr>): void {
  for (const listener of attrListeners) listener(attrs);
}

/** A small catalog shaped like the server's: id hidden, one link, one system namespace. */
const CATALOG: Record<string, InstantAttr> = {
  'attr-id': {
    id: 'attr-id',
    'forward-identity': ['attr-id', 'todos', 'id', false],
    'index?': false,
    'unique?': true,
    cardinality: 'one',
    'value-type': 'blob',
    'checked-data-type': 'string',
    catalog: 'system',
  },
  'attr-title': {
    id: 'attr-title',
    'forward-identity': ['attr-title', 'todos', 'title'],
    'index?': true,
    'unique?': false,
    'required?': true,
    cardinality: 'one',
    'value-type': 'blob',
    'checked-data-type': 'string',
    'inferred-types': ['string'],
  },
  'attr-done': {
    id: 'attr-done',
    'forward-identity': ['attr-done', 'todos', 'done'],
    'index?': true,
    'unique?': false,
    cardinality: 'one',
    'value-type': 'blob',
    'checked-data-type': 'boolean',
  },
  'attr-createdAt': {
    id: 'attr-createdAt',
    'forward-identity': ['attr-createdAt', 'todos', 'createdAt'],
    'index?': false,
    'unique?': false,
    cardinality: 'one',
    'value-type': 'blob',
    'inferred-types': ['number'],
  },
  'attr-note': {
    id: 'attr-note',
    'forward-identity': ['attr-note', 'todos', 'note'],
    'index?': false,
    'unique?': false,
    cardinality: 'one',
    'value-type': 'blob',
  },
  'attr-owner': {
    id: 'attr-owner',
    'forward-identity': ['attr-owner', 'todos', 'owner'],
    'reverse-identity': ['attr-owner', '$users', 'todos'],
    'index?': true,
    'unique?': false,
    cardinality: 'one',
    'value-type': 'ref',
    'on-delete': 'cascade',
  },
  'attr-secret': {
    id: 'attr-secret',
    'forward-identity': ['attr-secret', '$token', 'secret'],
    'index?': true,
    'unique?': false,
    cardinality: 'one',
    'value-type': 'blob',
    catalog: 'system',
  },
};

beforeEach(() => {
  resetSession();
  initMock.mockReset();
  initMock.mockImplementation(() => createFakeClient());
  attrListeners.clear();
  querySubscriptions.length = 0;
  queryUnsubscribes = 0;
});

afterEach(() => {
  closeAllAppClients();
  resetSession();
  vi.useRealTimers();
});

describe('getAppClient', () => {
  it('opens one client per app with the admin-token options', () => {
    const client = getAppClient(CONFIG);

    expect(initMock).toHaveBeenCalledTimes(1);
    expect(initMock).toHaveBeenCalledWith({
      appId: 'app-1',
      apiURI: 'https://api.example.com',
      websocketURI: 'wss://api.example.com/runtime/session',
      __adminToken: 'app-1-admin-token',
      disableValidation: true,
      devtool: false,
    });
    expect(peekAppClient('app-1')).toBe(client);
    expect(getAppClient(CONFIG)).toBe(client);
    expect(initMock).toHaveBeenCalledTimes(1);
  });

  it('keeps a separate client for every app', () => {
    getAppClient(CONFIG);
    getAppClient({ ...CONFIG, appId: 'app-2' });

    expect(initMock).toHaveBeenCalledTimes(2);
    expect(peekAppClient('app-1')).not.toBe(peekAppClient('app-2'));
    expect(peekAppClient('app-1')).toBeDefined();
    expect(peekAppClient('app-2')).toBeDefined();
  });

  it('replaces the client when the admin token rotates', () => {
    const first = getAppClient(CONFIG);
    const rotated = getAppClient({ ...CONFIG, adminToken: 'rotated-token' });

    expect(rotated).not.toBe(first);
    expect(peekAppClient('app-1')).toBe(rotated);
    // eslint-disable-next-line @typescript-eslint/unbound-method -- asserting on the mock method reference
    expect(first.core.shutdown).toHaveBeenCalledTimes(1);
    expect(initMock).toHaveBeenCalledTimes(2);
  });

  it('keeps a mounted screen attached across a token rotation', () => {
    vi.useFakeTimers();
    getAppClient(CONFIG);
    const release = retainAppClient('app-1');

    const rotated = getAppClient({ ...CONFIG, adminToken: 'rotated-token' });
    vi.advanceTimersByTime(APP_CLIENT_IDLE_MS * 2);
    expect(peekAppClient('app-1')).toBe(rotated);

    release();
    vi.advanceTimersByTime(APP_CLIENT_IDLE_MS);
    expect(peekAppClient('app-1')).toBeUndefined();
  });

  it('closes every client on sign-out', () => {
    getAppClient(CONFIG);
    getAppClient({ ...CONFIG, appId: 'app-2' });

    signOut();

    expect(peekAppClient('app-1')).toBeUndefined();
    expect(peekAppClient('app-2')).toBeUndefined();
  });
});

describe('idle closing', () => {
  it('closes an app client 60s after the last screen releases it', () => {
    vi.useFakeTimers();
    const client = getAppClient(CONFIG);
    const release = retainAppClient('app-1');

    release();
    vi.advanceTimersByTime(APP_CLIENT_IDLE_MS - 1);
    expect(peekAppClient('app-1')).toBe(client);

    vi.advanceTimersByTime(1);
    expect(peekAppClient('app-1')).toBeUndefined();
    // eslint-disable-next-line @typescript-eslint/unbound-method -- asserting on the mock method reference
    expect(client.core.shutdown).toHaveBeenCalledTimes(1);
  });

  it('cancels the close while a screen still holds the client', () => {
    vi.useFakeTimers();
    const client = getAppClient(CONFIG);
    const release = retainAppClient('app-1');

    vi.advanceTimersByTime(APP_CLIENT_IDLE_MS * 2);
    expect(peekAppClient('app-1')).toBe(client);

    release();
    vi.advanceTimersByTime(APP_CLIENT_IDLE_MS);
    expect(peekAppClient('app-1')).toBeUndefined();
  });

  it('ignores a second release of the same retention', () => {
    vi.useFakeTimers();
    const client = getAppClient(CONFIG);
    const release = retainAppClient('app-1');
    const keep = retainAppClient('app-1');

    release();
    release();
    vi.advanceTimersByTime(APP_CLIENT_IDLE_MS);
    expect(peekAppClient('app-1')).toBe(client);

    keep();
    vi.advanceTimersByTime(APP_CLIENT_IDLE_MS);
    expect(peekAppClient('app-1')).toBeUndefined();
  });
});

describe('appDataAccess', () => {
  const input = {
    appId: 'app-1',
    adminToken: 'app-1-admin-token',
    apiBaseUrl: 'https://api.example.com',
  };

  it('refuses collaborators and app members with the explorer copy', () => {
    expect(
      appDataAccess({ ...input, role: 'collaborator' }, 'explorer'),
    ).toEqual({
      status: 'forbidden',
      message: 'The explorer needs admin access to this app.',
    });
    expect(appDataAccess({ ...input, role: 'app-member' }, 'schema')).toEqual({
      status: 'forbidden',
      message: 'The schema needs admin access to this app.',
    });
  });

  it('builds the connection for admins and owners', () => {
    expect(appDataAccess({ ...input, role: 'admin' }, 'explorer')).toEqual({
      status: 'ready',
      config: {
        appId: 'app-1',
        apiURI: 'https://api.example.com',
        websocketURI: 'wss://api.example.com/runtime/session',
        adminToken: 'app-1-admin-token',
      },
    });
    expect(appDataAccess({ ...input, role: 'owner' }, 'explorer').status).toBe(
      'ready',
    );
  });

  it('reports an admin token the payload did not include', () => {
    const access = appDataAccess(
      { ...input, role: 'admin', adminToken: undefined },
      'explorer',
    );

    expect(access.status).toBe('missing-token');
    if (access.status === 'missing-token') {
      expect(access.message).toMatch(/admin token/);
    }
  });

  it('reports missing or unusable runtime configuration', () => {
    expect(
      appDataAccess(
        { ...input, role: 'admin', apiBaseUrl: undefined },
        'explorer',
      ).status,
    ).toBe('missing-config');
    expect(
      appDataAccess(
        { ...input, role: 'admin', apiBaseUrl: 'localhost:8888' },
        'explorer',
      ).status,
    ).toBe('missing-config');
  });
});

describe('appSchemaFromAttrs', () => {
  it('derives namespaces, attributes and links from the catalog', () => {
    const schema = appSchemaFromAttrs(CATALOG);

    expect(schema.namespaces.map((ns) => ns.name)).toEqual(['$users', 'todos']);

    const todos = schema.namespaces.find((ns) => ns.name === 'todos');
    expect(todos?.isSystem).toBe(false);
    expect(todos?.attributes.map((attr) => attr.name)).toEqual([
      'createdAt',
      'done',
      'note',
      'title',
    ]);
    expect(todos?.links).toEqual([
      {
        id: 'attr-owner',
        namespace: 'todos',
        label: 'owner',
        targetNamespace: '$users',
        reverseLabel: 'todos',
        cardinality: 'one',
        onDelete: 'cascade',
      },
    ]);

    expect(schema.attributes).toHaveLength(4);
    expect(schema.links).toHaveLength(1);
  });

  it('maps each attribute to the type its badge shows', () => {
    const schema = appSchemaFromAttrs(CATALOG);
    const types = Object.fromEntries(
      schema.attributes.map((attr) => [attr.name, attr.type]),
    );

    expect(types).toEqual({
      createdAt: 'number',
      done: 'boolean',
      note: 'any',
      title: 'string',
    });
  });

  it('marks required attributes and hides system catalogs', () => {
    const schema = appSchemaFromAttrs(CATALOG);
    const title = schema.attributes.find((attr) => attr.name === 'title');

    expect(title?.isRequired).toBe(true);
    expect(title?.isIndexed).toBe(true);
    expect(schema.namespaces.map((ns) => ns.name)).not.toContain('$token');
    expect(schema.attributes.map((attr) => attr.name)).not.toContain('id');
    expect(schema.namespaces.find((ns) => ns.name === '$users')?.isSystem).toBe(
      true,
    );
  });

  it('returns an empty schema before the catalog arrives', () => {
    expect(appSchemaFromAttrs(null)).toEqual({
      namespaces: [],
      attributes: [],
      links: [],
    });
  });

  it('treats a null reverse identity as an attribute without a link', () => {
    const schema = appSchemaFromAttrs({
      'attr-blob': {
        id: 'attr-blob',
        'forward-identity': ['ident-blob', 'todos', 'title'],
        'reverse-identity': null,
        'index?': false,
        'unique?': false,
        cardinality: 'one',
        'value-type': 'blob',
      },
      'attr-ref': {
        id: 'attr-ref',
        'forward-identity': ['ident-ref', 'todos', 'owner'],
        'reverse-identity': null,
        'index?': false,
        'unique?': false,
        cardinality: 'one',
        'value-type': 'ref',
      },
    });

    expect(schema.namespaces.map((ns) => ns.name)).toEqual(['todos']);
    expect(schema.attributes.map((attr) => attr.name)).toEqual(['title']);
    expect(schema.links).toEqual([]);
  });
});

describe('useAppSchema', () => {
  it('starts loading and then exposes the connected catalog', () => {
    getAppClient(CONFIG);
    const { result, unmount } = renderHook(() => useAppSchema('app-1'));

    expect(result.current.isLoading).toBe(true);
    expect(result.current.namespaces).toEqual([]);
    expect(querySubscriptions[0]).toEqual({ ____explorer___: {} });

    act(() => {
      emitAttrs(CATALOG);
    });

    expect(result.current.isLoading).toBe(false);
    expect(result.current.namespaces.map((ns) => ns.name)).toEqual([
      '$users',
      'todos',
    ]);
    expect(result.current.links[0]?.label).toBe('owner');

    unmount();
    expect(queryUnsubscribes).toBe(1);
  });

  it('stays empty when no client exists for the app', () => {
    const { result } = renderHook(() => useAppSchema('app-9'));

    expect(result.current.isLoading).toBe(true);
    expect(result.current.namespaces).toEqual([]);
    expect(querySubscriptions).toEqual([]);
  });
});

describe('withTransactionTimeout', () => {
  it('resolves with the transaction result', async () => {
    await expect(withTransactionTimeout(Promise.resolve('ok'))).resolves.toBe(
      'ok',
    );
  });

  it('rejects a transaction the server never acknowledges', async () => {
    vi.useFakeTimers();
    const pending = withTransactionTimeout(new Promise(() => undefined));
    const assertion = expect(pending).rejects.toThrow(
      'The server did not confirm the change. Try again.',
    );

    await vi.advanceTimersByTimeAsync(TRANSACTION_ACK_TIMEOUT_MS);
    await assertion;
  });
});
