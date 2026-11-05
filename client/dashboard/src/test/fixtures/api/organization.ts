import type { OrgDetails } from '@/lib/api/endpoints/schemas';

/**
 * C.17 fixtures. The org has one app, one member per role tier and one pending
 * invite, which covers every branch of the members and settings screens.
 */
type OrgMember = OrgDetails['members'][number];
type OrgInvite = OrgDetails['invites'][number];
type OrgApp = OrgDetails['apps'][number];

export const mockOrgSummary: OrgDetails['org'] = {
  id: 'org-1',
  title: 'Acme',
  created_at: '2026-01-01T00:00:00Z',
  updated_at: '2026-01-01T00:00:00Z',
  role: 'owner',
  paid: false,
};

export const mockOrgApp: OrgApp = {
  id: 'app-org-1',
  title: 'team-app',
  created_at: '2026-01-05T00:00:00Z',
  status: 'active',
  org_id: 'org-1',
  role: 'owner',
};

export const mockOrgMembers: OrgMember[] = [
  {
    id: 'member-1',
    email: 'owner@example.com',
    role: 'owner',
    created_at: '2026-01-01T00:00:00Z',
  },
  {
    id: 'member-2',
    email: 'admin@example.com',
    role: 'admin',
    created_at: '2026-01-02T00:00:00Z',
  },
  {
    id: 'member-3',
    email: 'dev@example.com',
    role: 'collaborator',
    created_at: '2026-01-03T00:00:00Z',
  },
];

export const mockOrgInvites: OrgInvite[] = [
  {
    id: 'invite-org-1',
    email: 'new@example.com',
    status: 'pending',
    role: 'collaborator',
    sent_at: '2026-02-01T00:00:00Z',
    expired: false,
  },
];

export const mockOrgDetails: OrgDetails = {
  org: mockOrgSummary,
  apps: [mockOrgApp],
  members: mockOrgMembers,
  invites: mockOrgInvites,
};
