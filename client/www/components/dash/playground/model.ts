import { parsePermsJSON } from '@/lib/parsePermsJSON';
export type Draft = {
  code: string;
  perms: string;
  runAsUser: string;
  ipOverride: string;
  originOverride: string;
  useAppPerms: boolean;
};
export type SavedExample = Draft & { name: string; lastSavedAt: string };
export const normalizeDraft = (input: any, fallback: Draft): Draft => ({
  code: typeof input?.code === 'string' ? input.code : fallback.code,
  perms: typeof input?.perms === 'string' ? input.perms : fallback.perms,
  runAsUser: typeof input?.runAsUser === 'string' ? input.runAsUser : '',
  ipOverride: typeof input?.ipOverride === 'string' ? input.ipOverride : '',
  originOverride:
    typeof input?.originOverride === 'string' ? input.originOverride : '',
  useAppPerms:
    typeof input?.useAppPerms === 'boolean' ? input.useAppPerms : true,
});
export function readExamples(
  value: string | null,
  fallback: Draft,
): SavedExample[] {
  try {
    const items = JSON.parse(value ?? '[]');
    if (!Array.isArray(items)) return [];
    return items
      .filter(
        (item) =>
          typeof item?.name === 'string' &&
          item.name.trim() &&
          typeof item.code === 'string',
      )
      .map((item) => ({
        ...normalizeDraft(item, fallback),
        name: item.name,
        lastSavedAt:
          typeof item.lastSavedAt === 'string' ? item.lastSavedAt : '',
      }));
  } catch {
    return [];
  }
}
export function starterCode(namespace?: string) {
  return namespace
    ? `const result = await db.query(${JSON.stringify({ [namespace]: { $: { limit: 10 } } }, null, 2)});\nconsole.log(result);`
    : '// Select a namespace from the examples menu, or write a query here.\n';
}
export function encodeDraftText(value: string) {
  return btoa(
    Array.from(new TextEncoder().encode(JSON.stringify(value)), (b) =>
      String.fromCharCode(b),
    ).join(''),
  );
}
export function decodeDraftText(value: string | null): string | undefined {
  if (!value) return undefined;
  try {
    const binary = atob(value);
    let result: unknown;
    try {
      result = JSON.parse(
        new TextDecoder('utf-8', { fatal: true }).decode(
          Uint8Array.from(binary, (c) => c.charCodeAt(0)),
        ),
      );
    } catch {
      result = JSON.parse(binary);
    }
    return typeof result === 'string' ? result : undefined;
  } catch {
    return undefined;
  }
}
export function permalink(base: string, draft: Draft): string {
  const url = new URL(base);
  url.searchParams.set('code', encodeDraftText(draft.code));
  url.searchParams.set('perms', encodeDraftText(draft.perms));
  url.searchParams.set('useAppPerms', String(draft.useAppPerms));
  url.searchParams.set('runAs', draft.runAsUser);
  url.searchParams.set('requestIp', draft.ipOverride);
  url.searchParams.set('requestOrigin', draft.originOverride);
  url.searchParams.delete('dangerouslyCommitTx');
  return url.toString();
}
export function draftFromLink(search: string, fallback: Draft): Draft {
  const params = new URLSearchParams(search);
  return {
    ...fallback,
    code: decodeDraftText(params.get('code')) ?? fallback.code,
    perms: decodeDraftText(params.get('perms')) ?? fallback.perms,
    useAppPerms: params.has('useAppPerms')
      ? params.get('useAppPerms') !== 'false'
      : fallback.useAppPerms,
    runAsUser: params.get('runAs') ?? fallback.runAsUser,
    ipOverride: params.get('requestIp') ?? fallback.ipOverride,
    originOverride: params.get('requestOrigin') ?? fallback.originOverride,
  };
}
export function runRules(
  draft: Draft,
  appRules: object | null,
): object | undefined {
  if (draft.useAppPerms) return appRules ?? undefined;
  const result = parsePermsJSON(draft.perms);
  if (result.status === 'error')
    throw new Error(
      'Custom permission rules are not valid JSON5. Fix them before running.',
    );
  const value: unknown = result.value;
  if (!value || typeof value !== 'object' || Array.isArray(value))
    throw new Error('Custom permissions must be a JSON object.');
  return value;
}
export function safePreview(value: unknown, limit = 65536): string {
  const seen = new WeakSet<object>();
  try {
    const text =
      JSON.stringify(
        value,
        (_, v) => {
          if (typeof v === 'bigint') return `${v}n`;
          if (v instanceof Error) return { name: v.name, message: v.message };
          if (v && typeof v === 'object') {
            if (seen.has(v)) return '[Circular or repeated reference]';
            seen.add(v);
          }
          return v;
        },
        2,
      ) ?? String(value);
    return text.length > limit
      ? text.slice(0, limit) + '\n… Preview truncated'
      : text;
  } catch {
    return String(value).slice(0, limit);
  }
}
export type DebugClient = {
  debugQuery: (query: any, options: any) => Promise<any>;
  debugTransact: (steps: any, options: any) => Promise<any>;
};
export type RunOutput = {
  kind: 'query' | 'transaction' | 'log' | 'error' | 'return';
  text: string;
  checks?: string;
  raw?: string;
  duration?: number;
};
export async function executeCode(
  code: string,
  client: DebugClient,
  context: {
    rules: object | undefined;
    ip: string;
    origin: string;
    commit: boolean;
  },
  helpers: { id: any; tx: any; lookup: any },
  emit: (output: RunOutput) => void,
  active: () => boolean = () => true,
) {
  const checkActive = () => {
    if (!active()) throw new Error('This playground run is no longer active.');
  };
  const db = {
    tx: helpers.tx,
    async query(query: any, options?: any) {
      checkActive();
      const start = performance.now();
      const response = await client.debugQuery(query, {
        ...options,
        rules: context.rules,
        ip: context.ip,
        origin: context.origin,
        cardinalityInference: true,
      });
      if (active())
        emit({
          kind: 'query',
          text: safePreview(response.result),
          checks: safePreview(response['check-results']),
          raw: safePreview(response),
          duration: performance.now() - start,
        });
      return response.result;
    },
    async transact(steps: any) {
      checkActive();
      const start = performance.now();
      const response = await client.debugTransact(steps, {
        rules: context.rules,
        ip: context.ip,
        origin: context.origin,
        __dangerouslyCommit: context.commit,
      });
      if (active())
        emit({
          kind: 'transaction',
          text: safePreview({
            'tx-id': response['tx-id'],
            'all-checks-ok?': response['all-checks-ok?'],
            writeRequested: context.commit,
          }),
          checks: safePreview(response['check-results']),
          raw: safePreview(response),
          duration: performance.now() - start,
        });
      return { 'tx-id': response['tx-id'] };
    },
  };
  const log = (kind: 'log' | 'error', args: unknown[]) => {
    if (active()) emit({ kind, text: safePreview(args) });
  };
  const consoleProxy = {
    ...console,
    log: (...args: unknown[]) => log('log', args),
    info: (...args: unknown[]) => log('log', args),
    warn: (...args: unknown[]) => log('log', args),
    error: (...args: unknown[]) => log('error', args),
    debug: (...args: unknown[]) => log('log', args),
  };
  const fn = new Function(
    'db',
    'console',
    'id',
    'tx',
    'lookup',
    `return (async () => {\n${code}\n})()`,
  );
  const result = await fn(
    db,
    consoleProxy,
    helpers.id,
    helpers.tx,
    helpers.lookup,
  );
  if (result !== undefined && active())
    emit({ kind: 'return', text: safePreview(result) });
}

export const draftFingerprint = (draft: Draft) =>
  JSON.stringify(normalizeDraft(draft, draft));
