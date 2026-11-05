import type { PendingInvite } from '@/lib/api/endpoints/schemas';

/**
 * Where an accepted invite lands (C.18). Org invites open the org's apps page;
 * app invites open that app's overview.
 */
export function inviteDestination(
  invite: Pick<PendingInvite, 'type' | 'foreign_key'>,
): string {
  const id = encodeURIComponent(invite.foreign_key);
  return invite.type === 'org' ? `/w/${id}` : `/apps/${id}/overview`;
}

export function inviteTypeLabel(type: PendingInvite['type']): string {
  return type === 'org' ? 'Organization' : 'App';
}
