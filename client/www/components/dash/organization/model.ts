import type { OrgWorkspace } from '@/lib/hooks/useWorkspace';
export type { Request } from '@/lib/request';
export { requestError } from '@/lib/request';
export type Organization = Pick<
  OrgWorkspace,
  'org' | 'members' | 'invites' | 'apps'
>;
export type OrganizationTab = 'members' | 'manage';
export type MemberRole = 'owner' | 'admin' | 'collaborator';
export type OrganizationCommand =
  | { kind: 'rename'; title: string }
  | { kind: 'invite'; email: string; role: MemberRole }
  | { kind: 'role'; id: string; role: MemberRole }
  | { kind: 'remove'; id: string }
  | { kind: 'revoke'; id: string };
const rank: Record<string, number> = {
  owner: 4,
  admin: 3,
  collaborator: 2,
  'app-member': 1,
};
export function admin(role: string) {
  return (rank[role] ?? 0) >= 3;
}
export function assignableRoles(role: string): MemberRole[] {
  return (['collaborator', 'admin', 'owner'] as const).filter(
    (r) => admin(role) && rank[r] <= rank[role],
  );
}
export function lastOwner(data: Organization, id: string) {
  return (
    data.members.find((m) => m.id === id)?.role === 'owner' &&
    data.members.filter((m) => m.role === 'owner').length <= 1
  );
}
export function organizationTitle(value: string) {
  const title = value.trim();
  if (!title) throw new Error('Enter an organization name.');
  if ([...title].length > 140)
    throw new Error('Organization names can contain up to 140 characters.');
  return title;
}
export function commandProblem(
  data: Organization,
  email: string,
  selfHosted: boolean,
  command: OrganizationCommand,
): string | null {
  const actor = data.org.role;
  if (command.kind === 'remove' || command.kind === 'role') {
    const member = data.members.find((m) => m.id === command.id);
    if (!member) return 'This member is no longer in the organization.';
    const self = member.email.toLowerCase() === email.toLowerCase();
    if (command.kind === 'remove' && self) {
      if ((rank[actor] ?? 0) < 2)
        return 'Your current role cannot leave through this control.';
    } else if (
      !admin(actor) ||
      (rank[member.role] ?? Infinity) > (rank[actor] ?? 0)
    )
      return 'Your role cannot change this member.';
    if (
      lastOwner(data, command.id) &&
      (command.kind === 'remove' || command.role !== 'owner')
    )
      return 'Assign another owner before removing or demoting the last owner.';
    if (
      command.kind === 'role' &&
      !assignableRoles(actor).includes(command.role)
    )
      return 'You cannot assign this role.';
    if (command.kind === 'role' && command.role === member.role)
      return 'Choose a different role.';
    return null;
  }
  if (!admin(actor))
    return 'Organization administrators or owners are required for this action.';
  if (command.kind === 'invite') {
    if (!selfHosted && !data.org.paid)
      return 'Invitations require a paid organization plan on managed cloud.';
    if (!assignableRoles(actor).includes(command.role))
      return 'You cannot invite this role.';
    const normalized = command.email.trim().toLowerCase();
    if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(normalized))
      return 'Enter a valid email address.';
    if (data.members.some((m) => m.email.toLowerCase() === normalized))
      return 'This person is already a member.';
    if (
      data.invites.some(
        (i) =>
          i.email.toLowerCase() === normalized &&
          i.status === 'pending' &&
          !i.expired,
      )
    )
      return 'This person already has a pending invitation.';
  }
  if (
    command.kind === 'revoke' &&
    !data.invites.some((i) => i.id === command.id && i.status === 'pending')
  )
    return 'This invitation is no longer pending.';
  if (command.kind === 'rename') {
    try {
      if (organizationTitle(command.title) === data.org.title)
        return 'Enter a different organization name.';
    } catch (e) {
      return (e as Error).message;
    }
  }
  return null;
}
export function organizationRequest(
  orgId: string,
  command: OrganizationCommand,
) {
  let suffix = '',
    method = 'POST',
    body: object = {};
  switch (command.kind) {
    case 'rename':
      suffix = 'rename';
      body = { title: organizationTitle(command.title) };
      break;
    case 'invite':
      suffix = 'invite/send';
      body = {
        'invitee-email': command.email.trim().toLowerCase(),
        role: command.role,
      };
      break;
    case 'role':
      suffix = 'members/update';
      body = { id: command.id, role: command.role };
      break;
    case 'remove':
      suffix = 'members/remove';
      method = 'DELETE';
      body = { id: command.id };
      break;
    case 'revoke':
      suffix = 'invite/revoke';
      method = 'DELETE';
      body = { 'invite-id': command.id };
      break;
  }
  return {
    path: `/dash/orgs/${encodeURIComponent(orgId)}/${suffix}`,
    init: { method, body: JSON.stringify(body) },
  };
}
export function organizationBaseline(data: Organization) {
  return JSON.stringify([
    data.org.title,
    data.org.role,
    data.org.paid,
    [...data.members].sort((a, b) => a.id.localeCompare(b.id)),
    [...data.invites].sort((a, b) => a.id.localeCompare(b.id)),
  ]);
}
export function organizationReceipt(
  data: Organization,
  command: OrganizationCommand,
  response: unknown,
): Organization {
  if (!response || typeof response !== 'object' || Array.isArray(response))
    throw new Error(
      'The server returned an unexpected receipt. Refresh before submitting again.',
    );
  switch (command.kind) {
    case 'rename':
      return {
        ...data,
        org: { ...data.org, title: organizationTitle(command.title) },
      };
    case 'remove':
      return {
        ...data,
        members: data.members.filter((m) => m.id !== command.id),
      };
    case 'role':
      return {
        ...data,
        members: data.members.map((m) =>
          m.id === command.id ? { ...m, role: command.role } : m,
        ),
      };
    case 'revoke':
      return {
        ...data,
        invites: data.invites.map((i) =>
          i.id === command.id ? { ...i, status: 'revoked' } : i,
        ),
      };
    case 'invite':
      return data;
  }
}
export function organizationPeople(
  data: Organization,
  search: string,
  source: string,
  role: string,
) {
  const rows = [
    ...data.members.map((m) => ({
      ...m,
      source: 'member',
      status: 'Member',
      date: null as string | null,
    })),
    ...data.invites
      .filter((i) => i.status !== 'accepted')
      .map((i) => ({
        ...i,
        source: 'invite',
        status: i.status === 'pending' && i.expired ? 'Expired' : i.status,
        date: i.sent_at,
      })),
  ];
  return rows
    .filter(
      (p) =>
        (source === 'all' ||
          source === p.source ||
          (source === 'pending' && p.status === 'pending')) &&
        (role === 'all' || p.role === role) &&
        [p.email, p.id, p.status].some((v) =>
          v.toLowerCase().includes(search.trim().toLowerCase()),
        ),
    )
    .sort((a, b) => a.email.localeCompare(b.email));
}
export function organizationAppHref(orgId: string, appId: string) {
  return `/dash?${new URLSearchParams({ org: orgId, app: appId, s: 'main', t: 'home' })}`;
}
