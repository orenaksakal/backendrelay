import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import {
  mockOrgDetails,
  mockOrgMembers,
} from '@/test/fixtures/api/organization';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const API = 'http://localhost:8888';
const ORG_ID = 'org-1';

interface MemberBody {
  id?: string;
  role?: string;
}

beforeEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
  setSession({ token: mockSignInToken, user: mockSignInUser });
});

afterEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
});

describe('org members', () => {
  it('changes a role and removes a member', async () => {
    const user = userEvent.setup();
    let updated: MemberBody | undefined = undefined;
    let removed: MemberBody | undefined = undefined;
    server.use(
      http.post(
        `${API}/dash/orgs/${ORG_ID}/members/update`,
        async ({ request }) => {
          updated = (await request.json()) as MemberBody;
          return HttpResponse.json({});
        },
      ),
      http.delete(
        `${API}/dash/orgs/${ORG_ID}/members/remove`,
        async ({ request }) => {
          removed = (await request.json()) as MemberBody;
          return HttpResponse.json({});
        },
      ),
    );

    render({ route: `/w/${ORG_ID}/members` });
    const table = await screen.findByRole('table', {
      name: 'Organization members',
    });

    const role = within(table).getByRole('combobox', {
      name: 'Role for admin@example.com',
    });
    await user.click(role);
    await user.click(
      await screen.findByRole('option', { name: 'Collaborator' }),
    );

    await waitFor(() => {
      expect(updated).toEqual({ id: 'member-2', role: 'collaborator' });
    });
    expect(role).toHaveTextContent('Collaborator');

    const row = within(table).getByText('dev@example.com').closest('tr');
    expect(row).not.toBeNull();
    if (row === null) return;
    await user.click(within(row).getByRole('button', { name: 'Remove' }));

    const dialog = await screen.findByRole('dialog');
    await user.click(
      within(dialog).getByRole('button', { name: 'Remove member' }),
    );

    await waitFor(() => {
      expect(removed).toEqual({ id: 'member-3' });
    });
    await waitFor(() => {
      expect(
        within(table).queryByText('dev@example.com'),
      ).not.toBeInTheDocument();
    });
  });

  it('marks the last owner and leaves their role immutable', async () => {
    render({ route: `/w/${ORG_ID}/members` });
    const table = await screen.findByRole('table', {
      name: 'Organization members',
    });

    expect(within(table).getByText('You')).toBeInTheDocument();
    expect(within(table).getByText(/Last owner/)).toBeInTheDocument();
    expect(
      within(table).queryByRole('combobox', {
        name: 'Role for owner@example.com',
      }),
    ).not.toBeInTheDocument();
    expect(
      within(table).getByRole('button', { name: 'Leave org' }),
    ).toBeDisabled();
  });

  it('lets an owner with a peer leave the organization', async () => {
    const user = userEvent.setup();
    const secondOwner = {
      id: 'member-4',
      email: 'second@example.com',
      role: 'owner' as const,
      created_at: '2026-01-04T00:00:00Z',
    };
    let removed: MemberBody | undefined = undefined;
    server.use(
      http.get(`${API}/dash/orgs/${ORG_ID}`, () =>
        HttpResponse.json({
          ...mockOrgDetails,
          members: [...mockOrgMembers, secondOwner],
        }),
      ),
      http.delete(
        `${API}/dash/orgs/${ORG_ID}/members/remove`,
        async ({ request }) => {
          removed = (await request.json()) as MemberBody;
          return HttpResponse.json({});
        },
      ),
    );

    const { router } = render({ route: `/w/${ORG_ID}/members` });
    const table = await screen.findByRole('table', {
      name: 'Organization members',
    });
    await user.click(within(table).getByRole('button', { name: 'Leave org' }));

    const dialog = await screen.findByRole('dialog');
    await user.click(within(dialog).getByRole('button', { name: 'Leave org' }));

    await waitFor(() => {
      expect(removed).toEqual({ id: 'member-1' });
    });
    await waitFor(() => {
      expect(router.state.location.pathname).toBe('/w/personal');
    });
  });

  it('lists pending invites and revokes one', async () => {
    const user = userEvent.setup();
    let revoked: { 'invite-id'?: string } | undefined = undefined;
    server.use(
      http.delete(
        `${API}/dash/orgs/${ORG_ID}/invite/revoke`,
        async ({ request }) => {
          revoked = (await request.json()) as { 'invite-id'?: string };
          return HttpResponse.json({});
        },
      ),
    );

    render({ route: `/w/${ORG_ID}/members?tab=invites` });
    const table = await screen.findByRole('table', {
      name: 'Pending invites',
    });
    const row = within(table).getByText('new@example.com').closest('tr');
    expect(row).not.toBeNull();
    if (row === null) return;

    await user.click(within(row).getByRole('button', { name: 'Revoke' }));

    await waitFor(() => {
      expect(revoked).toEqual({ 'invite-id': 'invite-org-1' });
    });
    await waitFor(() => {
      expect(
        within(table).queryByText('new@example.com'),
      ).not.toBeInTheDocument();
    });
  });

  it('sends an invite and refuses a duplicate member', async () => {
    const user = userEvent.setup();
    let invited: { 'invitee-email'?: string; role?: string } | undefined =
      undefined;
    server.use(
      http.post(
        `${API}/dash/orgs/${ORG_ID}/invite/send`,
        async ({ request }) => {
          invited = (await request.json()) as {
            'invitee-email'?: string;
            role?: string;
          };
          return HttpResponse.json({});
        },
      ),
    );

    render({ route: `/w/${ORG_ID}/members` });
    await screen.findByRole('table', { name: 'Organization members' });

    await user.click(screen.getByRole('button', { name: 'Invite member' }));
    const dialog = await screen.findByRole('dialog');
    const email = within(dialog).getByLabelText('Email');

    await user.type(email, 'admin@example.com');
    await user.click(
      within(dialog).getByRole('button', { name: 'Send invite' }),
    );
    expect(
      await within(dialog).findByText('This person is already a member.'),
    ).toBeInTheDocument();
    expect(invited).toBeUndefined();

    await user.clear(email);
    await user.type(email, ' New@Example.test ');
    await user.click(
      within(dialog).getByRole('button', { name: 'Send invite' }),
    );

    await waitFor(() => {
      expect(invited).toEqual({
        'invitee-email': 'new@example.test',
        role: 'collaborator',
      });
    });
    expect(await screen.findByText('Invite sent')).toBeInTheDocument();
    await waitFor(() => {
      expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    });
  });
});
