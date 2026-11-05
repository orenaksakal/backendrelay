import type { OrgSummary } from '@/lib/types';
import { organizationTitle } from '../organization/model';
export type { Request } from '@/lib/request';
export { requestError } from '@/lib/request';
import type { Request } from '@/lib/request';
export type CreationKind = 'application' | 'organization';
export type Target = { id: string; title: string; role: string };
export type CreationDraft = {
  kind: CreationKind;
  title: string;
  orgId?: string;
  id?: string;
  adminToken?: string;
};
export type CreationReceipt = {
  kind: CreationKind;
  id: string;
  title: string;
  orgId?: string;
  recovered?: boolean;
};
export type Recovery = {
  kind: CreationKind;
  title: string;
  orgId?: string;
  id?: string;
  startedAt: string;
  previousOrgIds: string[];
};
export function targets(orgs: OrgSummary[] = []): Target[] {
  return [
    { id: 'personal', title: 'Personal workspace', role: 'owner' },
    ...orgs.filter((o) => ['owner', 'admin', 'collaborator'].includes(o.role)),
  ];
}
export function creationTitle(kind: CreationKind, value: string) {
  if (kind === 'organization') return organizationTitle(value);
  const title = value.trim();
  if (!title) throw new Error('Enter an application name.');
  return title;
}
export function isUuid(value: unknown): value is string {
  return (
    typeof value === 'string' &&
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(
      value,
    )
  );
}
export function creationRequest(draft: CreationDraft) {
  const title = creationTitle(draft.kind, draft.title);
  if (draft.kind === 'organization')
    return {
      path: '/dash/orgs',
      init: { method: 'POST', body: JSON.stringify({ title }) },
    };
  if (!isUuid(draft.id) || !isUuid(draft.adminToken))
    throw new Error(
      'Could not prepare application credentials. Start a new review.',
    );
  if (draft.orgId && !isUuid(draft.orgId))
    throw new Error('Choose a valid organization.');
  return {
    path: '/dash/apps',
    init: {
      method: 'POST',
      body: JSON.stringify({
        id: draft.id,
        admin_token: draft.adminToken,
        title,
        ...(draft.orgId ? { org_id: draft.orgId } : {}),
      }),
    },
  };
}
export function creationReceipt(
  draft: Pick<CreationDraft, 'kind' | 'title' | 'id' | 'orgId'>,
  value: unknown,
  recovered = false,
): CreationReceipt {
  const record =
    value && typeof value === 'object'
      ? (value as Record<string, any>)[
          draft.kind === 'application' ? 'app' : 'org'
        ]
      : null;
  if (
    !record ||
    !isUuid(record.id) ||
    typeof record.title !== 'string' ||
    !record.title.trim() ||
    (draft.kind === 'application' && record.id !== draft.id)
  )
    throw new Error(
      'The creation response is incomplete. Check the previous attempt before creating again.',
    );
  if (
    draft.kind === 'application' &&
    (record.org_id ?? null) !== (draft.orgId ?? null)
  )
    throw new Error(
      'The returned application belongs to a different workspace. Check the previous attempt.',
    );
  return {
    kind: draft.kind,
    id: record.id,
    title: record.title,
    ...(draft.orgId ? { orgId: draft.orgId } : {}),
    ...(recovered ? { recovered: true } : {}),
  };
}
export function recoveryRecord(
  draft: CreationDraft,
  previousOrgIds: string[],
  now = new Date().toISOString(),
): Recovery {
  return {
    kind: draft.kind,
    title: creationTitle(draft.kind, draft.title),
    ...(draft.id ? { id: draft.id } : {}),
    ...(draft.orgId ? { orgId: draft.orgId } : {}),
    startedAt: now,
    previousOrgIds,
  };
}
export function readRecovery(raw: string | null): Recovery | null {
  try {
    const value = JSON.parse(raw ?? 'null');
    if (
      !value ||
      !['application', 'organization'].includes(value.kind) ||
      typeof value.title !== 'string' ||
      !value.title.trim() ||
      typeof value.startedAt !== 'string' ||
      !Number.isFinite(Date.parse(value.startedAt)) ||
      !Array.isArray(value.previousOrgIds) ||
      !value.previousOrgIds.every((v: unknown) => typeof v === 'string') ||
      (value.kind === 'application' && !isUuid(value.id)) ||
      (value.orgId !== undefined && !isUuid(value.orgId))
    )
      return null;
    return recoveryRecord(value, value.previousOrgIds, value.startedAt);
  } catch {
    return null;
  }
}
export function organizationCandidates(recovery: Recovery, orgs: OrgSummary[]) {
  return orgs.filter(
    (o) =>
      !recovery.previousOrgIds.includes(o.id) &&
      o.title === recovery.title &&
      o.role === 'owner',
  );
}
export async function checkCreation(
  request: Request,
  recovery: Recovery,
): Promise<{
  found?: CreationReceipt;
  candidates?: OrgSummary[];
  missing?: boolean;
}> {
  if (recovery.kind === 'application') {
    try {
      const value = await request(
        `/dash/apps/${encodeURIComponent(recovery.id!)}`,
      );
      return { found: creationReceipt(recovery, value, true) };
    } catch (e) {
      if (e && typeof e === 'object' && 'status' in e && e.status === 404)
        return { missing: true };
      throw e;
    }
  }
  const result = await request<{ orgs?: OrgSummary[] }>('/dash');
  if (!Array.isArray(result.orgs))
    throw new Error(
      'The organization inventory is incomplete. Check again before creating another organization.',
    );
  return { candidates: organizationCandidates(recovery, result.orgs) };
}
export function creationHref(
  result: CreationReceipt,
  cli: Record<string, string | undefined> = {},
) {
  const params = new URLSearchParams();
  for (const key of ['ticket', '_cli_oauth_ticket'])
    if (cli[key]) params.set(key, cli[key]!);
  if (result.kind === 'organization') {
    params.set('org', result.id);
    params.set('tab', 'members');
    return `/dash/org?${params}`;
  }
  params.set('s', 'main');
  params.set('t', 'home');
  params.set('app', result.id);
  if (result.orgId) params.set('org', result.orgId);
  return `/dash?${params}`;
}
