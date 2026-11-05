/**
 * Mock-build stand-in for `@instantdb/react` (D9.1).
 *
 * The dashboard's data screens talk to a live Instant socket. The Playwright
 * mock build has nothing listening, so the Explorer would stay on its skeleton.
 * MSW already answers the dashboard API; this module supplies the small slice
 * of the client contract those screens use, with one in-memory namespace.
 *
 * It is inert unless a spec opts in with `localStorage["br.mockInstant"] = "1"`,
 * so the rest of the mock suite keeps the pre-existing "socket never answers"
 * behaviour it asserts against.
 */

type MockRow = Record<string, unknown>;

interface MockAttr {
  id: string;
  'forward-identity': [string, string, string, boolean?];
  cardinality: 'one';
  'value-type': 'blob';
  'index?'?: boolean;
  'unique?'?: boolean;
  'required?'?: boolean;
  'primary?'?: boolean;
  'checked-data-type'?: 'string' | 'number' | 'boolean';
  catalog?: 'user';
}

interface QuerySub {
  query: Record<string, unknown>;
  callback: (result: unknown) => void;
}

interface Store {
  rows: Record<string, MockRow[]>;
  attrs: Record<string, MockAttr>;
  attrLabels: Record<string, string>;
  queries: Set<QuerySub>;
  attrSubs: Set<(attrs: Record<string, MockAttr>) => void>;
}

interface WriteOp {
  kind: 'update' | 'delete' | 'link' | 'unlink';
  values?: Record<string, unknown>;
}

interface Write {
  namespace: string;
  id: string;
  ops: WriteOp[];
}

interface MockChunk {
  update(values: Record<string, unknown>): MockChunk;
  delete(): MockChunk;
  link(values: Record<string, string | string[]>): MockChunk;
  unlink(values: Record<string, string | string[]>): MockChunk;
}

const MOCK_INSTANT_KEY = 'br.mockInstant';

/**
 * The Schema empty-state spec needs an app with no namespaces; the default
 * store's todos/goals fixtures would hide that path.
 */
const MOCK_EMPTY_KEY = 'br.mockInstantEmpty';

/**
 * The performance spec asks the stub for a large namespace. Keeping the switch
 * in localStorage leaves every other mock-mode spec on the small fixtures.
 */
const MOCK_ROWS_KEY = 'br.mockInstantRows';

const stores = new Map<string, Store>();
const writes = new WeakMap<MockChunk, Write>();

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

/** Text form for sorting and `$like` without stringifying objects. */
function textValue(value: unknown): string {
  if (typeof value === 'string') return value;
  if (typeof value === 'number' || typeof value === 'boolean') {
    return String(value);
  }
  return '';
}

function mockInstantEnabled(): boolean {
  try {
    return window.localStorage.getItem(MOCK_INSTANT_KEY) === '1';
  } catch {
    return false;
  }
}

function mockRowCount(): number {
  try {
    const raw = window.localStorage.getItem(MOCK_ROWS_KEY);
    const count = raw === null ? 0 : Number.parseInt(raw, 10);
    return Number.isFinite(count) && count > 0 ? Math.min(count, 100_000) : 0;
  } catch {
    return 0;
  }
}

function mockStoreEmpty(): boolean {
  try {
    return window.localStorage.getItem(MOCK_EMPTY_KEY) === '1';
  } catch {
    return false;
  }
}

function attribute(
  namespace: string,
  name: string,
  type: 'string' | 'number' | 'boolean',
  extra: Partial<MockAttr> = {},
): MockAttr {
  return {
    id: `attr-${namespace}-${name}`,
    'forward-identity': [`ident-${namespace}-${name}`, namespace, name, true],
    cardinality: 'one',
    'value-type': 'blob',
    'index?': true,
    'checked-data-type': type,
    catalog: 'user',
    ...extra,
  };
}

function createStore(): Store {
  const attrSubs = new Set<(attrs: Record<string, MockAttr>) => void>();
  if (mockStoreEmpty()) {
    return {
      attrs: {},
      attrLabels: {},
      queries: new Set(),
      attrSubs,
      rows: {},
    };
  }

  const catalog = [
    attribute('todos', 'id', 'string', {
      'unique?': true,
      'required?': true,
      'primary?': true,
    }),
    attribute('todos', 'title', 'string'),
    attribute('todos', 'done', 'boolean'),
    attribute('goals', 'id', 'string', {
      'unique?': true,
      'required?': true,
      'primary?': true,
    }),
    attribute('goals', 'title', 'string'),
  ];
  const attrs: Record<string, MockAttr> = {};
  const attrLabels: Record<string, string> = {};
  for (const entry of catalog) {
    attrs[entry.id] = entry;
    attrLabels[entry.id] = entry['forward-identity'][2];
  }

  const rows: Record<string, MockRow[]> = {
    todos: [
      { id: 't1', title: 'Buy milk', done: false },
      { id: 't2', title: 'Ship v2', done: true },
    ],
    goals: [{ id: 'g1', title: 'Ship it' }],
    // C.9 needs one catalog file so the Storage list and its sheet render.
    $files: [
      {
        id: 'file-1',
        path: 'notes.txt',
        'content-type': 'text/plain',
        size: 2048,
        serverCreatedAt: '2026-10-01T00:00:00Z',
      },
    ],
  };

  const rowCount = mockRowCount();
  if (rowCount > 0) {
    rows['todos'] = Array.from({ length: rowCount }, (_, index) => ({
      id: `t${String(index + 1)}`,
      title: `Mock todo ${String(index + 1)}`,
      done: index % 2 === 0,
    }));
  }

  return {
    attrs,
    attrLabels,
    queries: new Set(),
    attrSubs: new Set(),
    rows,
  };
}

function storeFor(appId: string): Store {
  let store = stores.get(appId);
  if (store === undefined) {
    store = createStore();
    stores.set(appId, store);
  }
  return store;
}

function matchesWhere(row: MockRow, where: Record<string, unknown>): boolean {
  return Object.entries(where).every(([key, expected]) => {
    const actual = row[key];
    if (isRecord(expected)) {
      const like = expected['$like'] ?? expected['$ilike'];
      if (typeof like === 'string') {
        const needle = like.replaceAll('%', '').toLowerCase();
        return textValue(actual).toLowerCase().includes(needle);
      }
      const equals = expected['$eq'];
      if (equals !== undefined) return actual === equals;
    }
    return actual === expected;
  });
}

function sortRows(rows: MockRow[], order: Record<string, unknown>): MockRow[] {
  const entries = Object.entries(order).filter(
    ([, direction]) => direction === 'asc' || direction === 'desc',
  );
  if (entries.length === 0) return rows;
  return [...rows].sort((a, b) => {
    for (const [key, direction] of entries) {
      const left = textValue(a[key]);
      const right = textValue(b[key]);
      const compared = left.localeCompare(right);
      if (compared !== 0) {
        return direction === 'asc' ? compared : -compared;
      }
    }
    return 0;
  });
}

function evaluate(store: Store, query: Record<string, unknown>): unknown {
  const namespaces = Object.keys(query);
  if (namespaces.length === 0) return { data: {} };

  const forms = namespaces.map((namespace) => {
    const form = query[namespace];
    return isRecord(form) ? form : {};
  });
  const counts = forms.every((form) => {
    const options = isRecord(form['$']) ? form['$'] : undefined;
    return options !== undefined && options['aggregate'] === 'count';
  });
  if (counts) {
    const aggregate: Record<string, { count: number }> = {};
    for (const namespace of namespaces) {
      aggregate[namespace] = {
        count: store.rows[namespace]?.length ?? 0,
      };
    }
    return { aggregate };
  }

  const namespace = namespaces[0];
  if (namespace === undefined) return { data: {} };
  const form = forms[0] ?? {};
  const options = isRecord(form['$']) ? form['$'] : {};
  const where = isRecord(options['where']) ? options['where'] : {};
  const order = isRecord(options['order']) ? options['order'] : {};

  let rows = (store.rows[namespace] ?? []).filter((row) =>
    matchesWhere(row, where),
  );
  rows = sortRows(rows, order);
  const limit = options['limit'];
  if (typeof limit === 'number') rows = rows.slice(0, limit);

  return {
    data: { [namespace]: rows },
    pageInfo: { [namespace]: { hasNextPage: false } },
  };
}

function notify(store: Store): void {
  for (const sub of [...store.queries]) {
    sub.callback(evaluate(store, sub.query));
  }
}

function createChunk(namespace: string, id: string): MockChunk {
  const write: Write = { namespace, id, ops: [] };
  const chunk: MockChunk = {
    update(values) {
      write.ops.push({ kind: 'update', values });
      return chunk;
    },
    delete() {
      write.ops.push({ kind: 'delete' });
      return chunk;
    },
    link(values) {
      write.ops.push({ kind: 'link', values });
      return chunk;
    },
    unlink(values) {
      write.ops.push({ kind: 'unlink', values });
      return chunk;
    },
  };
  writes.set(chunk, write);
  return chunk;
}

function createTx(
  store: Store | null,
): Record<string, Record<string, MockChunk>> {
  if (store === null) return {};
  return new Proxy(
    {},
    {
      get(_target, namespace) {
        if (typeof namespace !== 'string') return undefined;
        return new Proxy(
          {},
          {
            get(_inner, id) {
              if (typeof id !== 'string') return undefined;
              return createChunk(namespace, id);
            },
          },
        );
      },
    },
  );
}

function applyChunk(store: Store, chunk: unknown): void {
  if (typeof chunk !== 'object' || chunk === null) return;
  const write = writes.get(chunk as MockChunk);
  if (write === undefined) return;

  const rows = store.rows[write.namespace] ?? [];
  store.rows[write.namespace] = rows;
  let index = rows.findIndex((row) => row['id'] === write.id);
  const row: MockRow = index === -1 ? { id: write.id } : (rows[index] ?? {});

  for (const op of write.ops) {
    if (op.kind === 'update' && op.values !== undefined) {
      Object.assign(row, op.values);
      if (index === -1) {
        rows.push(row);
        index = rows.length - 1;
      }
    } else if (op.kind === 'delete') {
      if (index !== -1) {
        rows.splice(index, 1);
        index = -1;
      }
    } else if (op.kind === 'link' && op.values !== undefined) {
      Object.assign(row, op.values);
    } else if (op.kind === 'unlink' && op.values !== undefined) {
      for (const key of Object.keys(op.values)) {
        Reflect.deleteProperty(row, key);
      }
    }
  }
}

function applyOps(store: Store, ops: unknown): void {
  if (!Array.isArray(ops)) return;
  let attrsChanged = false;
  for (const op of ops) {
    if (!Array.isArray(op)) continue;
    const entries = op as unknown[];
    const [kind, id, attrId] = entries;
    if (kind === 'add-attr' && isRecord(id)) {
      const attr = id as unknown as MockAttr;
      const label = attr['forward-identity'][2];
      if (typeof label !== 'string') continue;
      store.attrs[attr.id] = attr;
      store.attrLabels[attr.id] = label;
      attrsChanged = true;
      continue;
    }
    if (
      kind !== 'retract-triple' ||
      typeof id !== 'string' ||
      typeof attrId !== 'string'
    ) {
      continue;
    }
    const label = store.attrLabels[attrId];
    if (label === undefined) continue;
    for (const rows of Object.values(store.rows)) {
      const row = rows.find((candidate) => candidate['id'] === id);
      if (row !== undefined) Reflect.deleteProperty(row, label);
    }
  }
  if (attrsChanged) {
    for (const callback of [...store.attrSubs]) callback(store.attrs);
  }
  notify(store);
}

/**
 * The slice of `init()` the dashboard calls. The rest of the SDK surface is
 * deliberately absent: mock builds must fail loudly if a screen starts using
 * something this stub does not model.
 */
export function init(options: Record<string, unknown> = {}): unknown {
  const enabled = mockInstantEnabled();
  const appId =
    typeof options['appId'] === 'string' ? options['appId'] : 'mock';
  const store = enabled ? storeFor(appId) : null;

  return {
    core: {
      shutdown: () => undefined,
      subscribeQuery: (
        query: Record<string, unknown>,
        callback: (result: unknown) => void,
      ) => {
        if (store === null) return () => undefined;
        const sub: QuerySub = { query, callback };
        store.queries.add(sub);
        callback(evaluate(store, query));
        return () => {
          store.queries.delete(sub);
        };
      },
      _reactor: {
        subscribeAttrs: (
          callback: (attrs: Record<string, MockAttr>) => void,
        ) => {
          if (store === null) return () => undefined;
          store.attrSubs.add(callback);
          callback(store.attrs);
          return () => {
            store.attrSubs.delete(callback);
          };
        },
        pushOps: (ops: unknown) => {
          if (store !== null) applyOps(store, ops);
          return Promise.resolve();
        },
      },
    },
    tx: createTx(store),
    transact: (chunks: unknown) => {
      if (store !== null) {
        const list: unknown[] = Array.isArray(chunks)
          ? (chunks as unknown[])
          : [chunks];
        for (const chunk of list) applyChunk(store, chunk);
        notify(store);
      }
      return Promise.resolve();
    },
  };
}
