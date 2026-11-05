import { init } from '@instantdb/react';
import { useEffect, useMemo, useState } from 'react';
import type { EffectiveRole } from '@/lib/api/endpoints/schemas';
import { registerSessionCleanup } from '@/lib/auth/session';
import { roleRank } from '@/lib/organization/model';
import { websocketUrl } from '@/lib/urls';

/**
 * D6.1: one live Instant client per app.
 *
 * The Explorer, Schema, Users and Storage screens read the same app through its
 * admin token, so they share one socket. A client is cached by app id, retained
 * while a screen uses it, and shut down 60s after the last screen releases it,
 * so switching apps costs nothing until the old one has been idle for a minute.
 * Signing out closes every client at once through `registerSessionCleanup`.
 *
 * The connection options mirror the reference implementation
 * (`client/packages/components/src/hooks/useStableDB.ts`); `__adminToken` is
 * how the dashboard acts as the app instead of a signed-in end user.
 */

/** How long an unreferenced app client may stay connected before it closes. */
export const APP_CLIENT_IDLE_MS = 60_000;

export interface AppClientConfig {
  appId: string;
  apiURI: string;
  websocketURI: string;
  adminToken: string;
}

/**
 * The reactor's attribute catalog, trimmed to the fields the dashboard reads.
 * Identities are `[attrId, namespace, label, indexed?]`, where an explicit
 * `false` hides the side from the UI (system labels and `id`).
 */
export type AttrIdentity =
  | readonly [string, string, string]
  | readonly [string, string, string, boolean];

export interface InstantAttr {
  id: string;
  'forward-identity': AttrIdentity;
  /** `null` for attributes without a reverse side, as the reactor sends it. */
  'reverse-identity'?: AttrIdentity | null;
  'index?': boolean;
  'unique?': boolean;
  'required?'?: boolean;
  'primary?'?: boolean;
  cardinality: 'one' | 'many';
  'value-type': 'ref' | 'blob';
  'inferred-types'?: readonly ('string' | 'number' | 'boolean' | 'json')[];
  catalog?: 'user' | 'system';
  'checked-data-type'?: 'string' | 'number' | 'boolean' | 'date';
  'on-delete'?: 'cascade';
  'on-delete-reverse'?: 'cascade';
}

/**
 * One entity transaction chunk (`tx.namespace[id]`). The dashboard builds its
 * writes with the same chunk API the SDK documents, so the server path,
 * webhooks and permission rules are identical to an app write (C.6).
 */
export interface AppTxChunk {
  update(values: Record<string, unknown>): AppTxChunk;
  delete(): AppTxChunk;
  link(values: Record<string, string | string[]>): AppTxChunk;
  unlink(values: Record<string, string | string[]>): AppTxChunk;
}

/** The slice of `@instantdb/react` the data screens use. */
export interface AppInstantClient {
  core: {
    shutdown(): void;
    subscribeQuery(
      query: Record<string, unknown>,
      callback: (result: unknown) => void,
    ): () => void;
    _reactor: {
      subscribeAttrs(
        callback: (attrs: Record<string, InstantAttr>) => void,
      ): () => void;
      /**
       * Raw transaction steps; the Explorer uses it for attribute operations
       * (`add-attr`) and for retracting a triple to unset a field.
       */
      pushOps(ops: ReadonlyArray<ReadonlyArray<unknown>>): Promise<unknown>;
    };
  };
  tx: Record<string, Record<string, AppTxChunk>>;
  transact(chunks: unknown): Promise<unknown>;
}

interface CachedAppClient {
  config: AppClientConfig;
  client: AppInstantClient;
  /** Screens currently mounted on this client; idle closes only at zero. */
  users: number;
  idleTimer: ReturnType<typeof setTimeout> | undefined;
}

const clients = new Map<string, CachedAppClient>();

function sameConnection(a: AppClientConfig, b: AppClientConfig): boolean {
  return (
    a.apiURI === b.apiURI &&
    a.websocketURI === b.websocketURI &&
    a.adminToken === b.adminToken
  );
}

function scheduleIdleClose(appId: string): void {
  const entry = clients.get(appId);
  if (entry === undefined || entry.users > 0) return;
  if (entry.idleTimer !== undefined) clearTimeout(entry.idleTimer);
  entry.idleTimer = setTimeout(() => {
    const current = clients.get(appId);
    if (current === undefined || current.users > 0) return;
    current.idleTimer = undefined;
    closeAppClient(appId);
  }, APP_CLIENT_IDLE_MS);
}

function openClient(config: AppClientConfig): AppInstantClient {
  // `__adminToken` is deliberately absent from `InstantConfig`; passing the
  // object through a variable keeps the excess-property check out of the way.
  const options = {
    appId: config.appId,
    apiURI: config.apiURI,
    websocketURI: config.websocketURI,
    __adminToken: config.adminToken,
    disableValidation: true,
    devtool: false,
  };
  // The SDK's database type is wider than the slice the dashboard uses; the
  // double assertion keeps that trim explicit instead of importing the whole
  // generic surface into every screen.
  return init(options);
}

/**
 * The one client for `appId`, connecting as the app itself. A config change
 * (a rotated admin token, a new API origin) replaces the cached connection.
 */
export function getAppClient(config: AppClientConfig): AppInstantClient {
  const existing = clients.get(config.appId);
  // A token rotation replaces the socket without dropping the screens that are
  // already holding this app open, so the new entry inherits their count.
  let users = 0;
  if (existing !== undefined) {
    if (sameConnection(existing.config, config)) return existing.client;
    users = existing.users;
    closeAppClient(config.appId);
  }

  const client = openClient(config);
  const entry: CachedAppClient = {
    config,
    client,
    users,
    idleTimer: undefined,
  };
  clients.set(config.appId, entry);
  // A client created without a screen (a prefetch, an aborted render) still
  // idles out instead of holding the socket open forever.
  if (users === 0) scheduleIdleClose(config.appId);
  // Sign-out must drop every connection at once. The cleanup set is cleared by
  // `resetSession()`, so register on creation rather than only at import time.
  registerSessionCleanup(closeAllAppClients);
  return client;
}

/** The cached client for `appId`, without creating one. */
export function peekAppClient(appId: string): AppInstantClient | undefined {
  return clients.get(appId)?.client;
}

/**
 * Marks `appId` as in use until the returned release runs. Retaining cancels a
 * pending idle close; the last release restarts the 60s timer.
 */
export function retainAppClient(appId: string): () => void {
  const entry = clients.get(appId);
  if (entry === undefined) return () => undefined;
  entry.users += 1;
  if (entry.idleTimer !== undefined) {
    clearTimeout(entry.idleTimer);
    entry.idleTimer = undefined;
  }

  let released = false;
  return () => {
    if (released) return;
    released = true;
    const current = clients.get(appId);
    if (current === undefined) return;
    current.users = Math.max(0, current.users - 1);
    if (current.users === 0) scheduleIdleClose(appId);
  };
}

/** Shuts down one app client now: sign-out, app teardown and token rotation. */
export function closeAppClient(appId: string): void {
  const entry = clients.get(appId);
  if (entry === undefined) return;
  clients.delete(appId);
  if (entry.idleTimer !== undefined) clearTimeout(entry.idleTimer);
  entry.client.core.shutdown();
}

export function closeAllAppClients(): void {
  for (const appId of [...clients.keys()]) closeAppClient(appId);
}

/** What a data screen needs before it may open the app's client. */
export interface AppDataAccessInput {
  appId: string;
  role: EffectiveRole;
  adminToken: string | undefined;
  apiBaseUrl: string | undefined;
}

export type AppDataAccess =
  | { status: 'ready'; config: AppClientConfig }
  | { status: 'forbidden'; message: string }
  | { status: 'missing-token'; message: string }
  | { status: 'missing-config'; message: string };

/**
 * Role gate plus connection gate for the Explorer, Schema, Users and Storage
 * screens (C.6–C.9): admins and owners get a config, everyone else gets the
 * 403 copy, and an admin without a token gets the reload message.
 */
export function appDataAccess(
  { appId, role, adminToken, apiBaseUrl }: AppDataAccessInput,
  thing: string,
): AppDataAccess {
  if (roleRank(role) < roleRank('admin')) {
    return {
      status: 'forbidden',
      message: `The ${thing} needs admin access to this app.`,
    };
  }
  if (apiBaseUrl === undefined || apiBaseUrl === '') {
    return {
      status: 'missing-config',
      message: "The dashboard didn't load its API URL. Reload the page.",
    };
  }
  const websocketURI = websocketUrl(apiBaseUrl);
  if (websocketURI === undefined) {
    return {
      status: 'missing-config',
      message:
        "The API URL in this deployment can't be used for realtime updates.",
    };
  }
  if (adminToken === undefined || adminToken === '') {
    return {
      status: 'missing-token',
      message:
        "The admin token for this app isn't available. Reload the app to fetch it again.",
    };
  }
  return {
    status: 'ready',
    config: { appId, apiURI: apiBaseUrl, websocketURI, adminToken },
  };
}

/** Column and badge types from the schema screens (C.6, C.7). */
export type AttributeType =
  | 'string'
  | 'number'
  | 'boolean'
  | 'date'
  | 'json'
  | 'any';

export interface AppAttribute {
  id: string;
  namespace: string;
  name: string;
  type: AttributeType;
  isIndexed: boolean;
  isUnique: boolean;
  isRequired: boolean;
  isPrimary: boolean;
}

export interface AppLink {
  id: string;
  /** Forward (owning) namespace. */
  namespace: string;
  /** Forward label, which is the column name in the Explorer. */
  label: string;
  /** Namespace the link points at. */
  targetNamespace: string;
  reverseLabel: string | undefined;
  cardinality: 'one' | 'many';
  onDelete: 'cascade' | undefined;
}

export interface AppNamespace {
  name: string;
  /** `$users`, `$files`, `$streams` and other `$`-prefixed namespaces (C.6). */
  isSystem: boolean;
  attributes: AppAttribute[];
  links: AppLink[];
}

export interface AppSchema {
  namespaces: AppNamespace[];
  /** Every visible attribute, flattened across namespaces. */
  attributes: AppAttribute[];
  /** One entry per relation, from the forward side (C.7 links table). */
  links: AppLink[];
}

function emptyAppSchema(): AppSchema {
  return { namespaces: [], attributes: [], links: [] };
}

/** System catalogs the dashboard always shows next to the user's namespaces. */
const VISIBLE_SYSTEM_NAMESPACES = new Set(['$users', '$files', '$streams']);

function compareNames(a: string, b: string): number {
  if (a === b) return 0;
  return a < b ? -1 : 1;
}

function compareAttributes(a: AppAttribute, b: AppAttribute): number {
  if (a.name === 'id') return b.name === 'id' ? 0 : -1;
  if (b.name === 'id') return 1;
  return compareNames(a.name, b.name);
}

/** `checked-data-type` wins; otherwise the first inferred type, else `any`. */
export function attributeType(attr: InstantAttr): AttributeType {
  const checked = attr['checked-data-type'];
  if (checked !== undefined) return checked;
  return attr['inferred-types']?.[0] ?? 'any';
}

/**
 * Port of the old explorer's `dbAttrsToExplorerSchema`
 * (`client/packages/components/src/schema.ts`), reshaped for this dashboard:
 * system catalog attributes are hidden except the users/files/streams ones,
 * links collapse to one entry with both labels, and attributes sort `id` first.
 */
export function appSchemaFromAttrs(
  rawAttrs: Record<string, InstantAttr> | null | undefined,
): AppSchema {
  if (rawAttrs === null || rawAttrs === undefined) return emptyAppSchema();

  const byNamespace = new Map<
    string,
    { attributes: AppAttribute[]; links: AppLink[] }
  >();
  const ensureNamespace = (name: string) => {
    let entry = byNamespace.get(name);
    if (entry === undefined) {
      entry = { attributes: [], links: [] };
      byNamespace.set(name, entry);
    }
    return entry;
  };

  const attributes: AppAttribute[] = [];
  const links: AppLink[] = [];

  for (const attr of Object.values(rawAttrs)) {
    const [, namespace, label, indexed] = attr['forward-identity'];
    if (indexed === false) continue;
    if (
      attr.catalog === 'system' &&
      !VISIBLE_SYSTEM_NAMESPACES.has(namespace)
    ) {
      continue;
    }

    const entry = ensureNamespace(namespace);
    const reverse = attr['reverse-identity'];
    if (reverse !== undefined && reverse !== null) ensureNamespace(reverse[1]);

    if (attr['value-type'] === 'ref') {
      if (reverse === undefined || reverse === null || reverse[3] === false) {
        continue;
      }
      const link: AppLink = {
        id: attr.id,
        namespace,
        label,
        targetNamespace: reverse[1],
        reverseLabel: reverse[2],
        cardinality: attr.cardinality,
        onDelete: attr['on-delete'],
      };
      entry.links.push(link);
      links.push(link);
      continue;
    }

    const attribute: AppAttribute = {
      id: attr.id,
      namespace,
      name: label,
      type: attributeType(attr),
      isIndexed: attr['index?'],
      isUnique: attr['unique?'],
      isRequired: attr['required?'] === true,
      isPrimary: attr['primary?'] === true,
    };
    entry.attributes.push(attribute);
    attributes.push(attribute);
  }

  for (const entry of byNamespace.values()) {
    entry.attributes.sort(compareAttributes);
    entry.links.sort((a, b) => compareNames(a.label, b.label));
  }

  const namespaces: AppNamespace[] = [...byNamespace.entries()]
    .map(([name, entry]) => ({
      name,
      isSystem: name.startsWith('$'),
      attributes: entry.attributes,
      links: entry.links,
    }))
    .sort((a, b) => compareNames(a.name, b.name));

  attributes.sort((a, b) => {
    const byNamespace = compareNames(a.namespace, b.namespace);
    return byNamespace === 0 ? compareAttributes(a, b) : byNamespace;
  });
  links.sort((a, b) => {
    const byNamespace = compareNames(a.namespace, b.namespace);
    return byNamespace === 0 ? compareNames(a.label, b.label) : byNamespace;
  });

  return { namespaces, attributes, links };
}

export interface AppSchemaState extends AppSchema {
  /**
   * The raw reactor catalog behind `namespaces`/`attributes`/`links`. The
   * Schema screen needs fields this dashboard trims (the identity ids needed
   * to rename an attribute, `on-delete-reverse`, and the code tab's schema
   * generator), so it is exposed alongside the derived shapes.
   */
  attrs: Record<string, InstantAttr> | null;
  /** True until the connection delivers the attribute catalog. */
  isLoading: boolean;
}

/**
 * The app's namespaces, attributes and links (C.6, C.7). The client has to
 * exist already — the app data gate creates it — and the server only pushes
 * attribute changes to connections that hold a query subscription, so the hook
 * keeps the old explorer's dummy query alive.
 */
export function useAppSchema(appId: string): AppSchemaState {
  const client = peekAppClient(appId);
  const [catalog, setCatalog] = useState<{
    appId: string;
    attrs: Record<string, InstantAttr>;
  } | null>(null);

  useEffect(() => {
    if (client === undefined) return;
    const release = retainAppClient(appId);
    const unsubscribeAttrs = client.core._reactor.subscribeAttrs((attrs) => {
      setCatalog({ appId, attrs });
    });
    // The server only pushes attribute changes to connections that hold a query
    // subscription, so keep the old explorer's dummy query alive.
    const unsubscribeQuery = client.core.subscribeQuery(
      { ____explorer___: {} },
      () => undefined,
    );
    return () => {
      unsubscribeAttrs();
      unsubscribeQuery();
      release();
    };
  }, [client, appId]);

  return useMemo(() => {
    const attrs =
      catalog !== null && catalog.appId === appId ? catalog.attrs : null;
    const schema = appSchemaFromAttrs(attrs);
    return { ...schema, attrs, isLoading: attrs === null };
  }, [catalog, appId]);
}

/**
 * How long a schema action waits for its transaction acknowledgement. The
 * reactor rejects a transaction it gets an answer for, so this only catches
 * the answer that never arrives and would otherwise leave the action pending.
 */
export const TRANSACTION_ACK_TIMEOUT_MS = 15_000;

/** Awaits `work`, but turns a permanently pending transaction into an error. */
export async function withTransactionTimeout<T>(
  work: Promise<T>,
  timeoutMs: number = TRANSACTION_ACK_TIMEOUT_MS,
): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      work,
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => {
          reject(
            new Error('The server did not confirm the change. Try again.'),
          );
        }, timeoutMs);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Creates (or reuses) the app's client for as long as the calling screen is
 * mounted, releasing it into the 60s idle window on unmount.
 */
export function useAppClient(
  config: AppClientConfig | null,
): AppInstantClient | null {
  const appId = config?.appId ?? null;
  const apiURI = config?.apiURI ?? null;
  const websocketURI = config?.websocketURI ?? null;
  const adminToken = config?.adminToken ?? null;

  // The reference implementation creates the client in a memo too: the map
  // hands back the same socket for the same app, and a render that React
  // discards only leaves a client that idles out.
  const client = useMemo(
    () =>
      appId === null ||
      apiURI === null ||
      websocketURI === null ||
      adminToken === null
        ? null
        : getAppClient({ appId, apiURI, websocketURI, adminToken }),
    [appId, apiURI, websocketURI, adminToken],
  );

  useEffect(() => {
    if (appId === null) return;
    return retainAppClient(appId);
  }, [appId]);

  return client;
}
