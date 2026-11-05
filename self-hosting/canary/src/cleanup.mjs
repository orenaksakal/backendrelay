import { CANARY_ENTITY } from './probe.mjs';

export const CANARY_ROW_LIMIT = 100;

export function rowsToPrune(rows, limit = CANARY_ROW_LIMIT) {
  if (!Array.isArray(rows)) {
    return [];
  }
  const valid = rows.filter(
    (row) => row && typeof row.id === 'string' && row.id.length > 0,
  );
  const sorted = valid.slice().sort((left, right) => {
    const byTime = (Number(right.at) || 0) - (Number(left.at) || 0);
    if (byTime !== 0) {
      return byTime;
    }
    return left.id < right.id ? -1 : left.id > right.id ? 1 : 0;
  });
  return sorted.slice(Math.max(0, limit)).map((row) => row.id);
}

async function cancelBody(response) {
  try {
    await response.body?.cancel?.();
  } catch {
    // The response body is irrelevant to cleanup.
  }
}

export async function cleanupRows({
  apiUrl,
  appId,
  adminToken,
  timeoutMs,
  fetchImpl = globalThis.fetch,
  limit = CANARY_ROW_LIMIT,
}) {
  const headers = {
    Authorization: `Bearer ${adminToken}`,
    'app-id': appId,
    'content-type': 'application/json',
  };
  const queryResponse = await fetchImpl(`${apiUrl}/admin/query`, {
    method: 'POST',
    headers,
    body: JSON.stringify({ query: { [CANARY_ENTITY]: {} } }),
    signal: AbortSignal.timeout(timeoutMs),
  });
  if (!queryResponse.ok) {
    await cancelBody(queryResponse);
    throw new Error(`canary cleanup query failed with status ${queryResponse.status}`);
  }
  const body = await queryResponse.json();
  const rows = body?.[CANARY_ENTITY];
  const ids = rowsToPrune(rows, limit);
  if (!ids.length) {
    return 0;
  }
  const deleteResponse = await fetchImpl(`${apiUrl}/admin/transact`, {
    method: 'POST',
    headers,
    body: JSON.stringify({
      steps: ids.map((id) => ['delete', CANARY_ENTITY, id]),
    }),
    signal: AbortSignal.timeout(timeoutMs),
  });
  await cancelBody(deleteResponse);
  if (!deleteResponse.ok) {
    throw new Error(
      `canary cleanup delete failed with status ${deleteResponse.status}`,
    );
  }
  return ids.length;
}
