import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { mockOrgDetails } from '@/test/fixtures/api/organization';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const API = 'http://localhost:8888';
const ORG_ID = 'org-1';

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

describe('org settings', () => {
  it('renames the organization from the unsaved-changes bar', async () => {
    const user = userEvent.setup();
    let renamed: { title?: string } | undefined = undefined;
    server.use(
      http.post(`${API}/dash/orgs/${ORG_ID}/rename`, async ({ request }) => {
        renamed = (await request.json()) as { title?: string };
        return HttpResponse.json({});
      }),
    );

    render({ route: `/w/${ORG_ID}/settings` });
    const name = await screen.findByLabelText('Name');
    expect(name).toHaveValue('Acme');

    await user.clear(name);
    await user.type(name, 'Acme Labs');
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => {
      expect(renamed).toEqual({ title: 'Acme Labs' });
    });
    expect(await screen.findByText('Org renamed')).toBeInTheDocument();
    expect(screen.getByLabelText('Name')).toHaveValue('Acme Labs');
  });

  it('blocks deletion while the org still has apps', async () => {
    render({ route: `/w/${ORG_ID}/settings` });
    await screen.findByLabelText('Name');

    expect(screen.getByRole('button', { name: 'Delete org' })).toBeDisabled();
    expect(screen.getByText('Move or delete its apps first')).toBeVisible();
  });

  it('deletes an empty org after typing its name', async () => {
    const user = userEvent.setup();
    let deleted = false;
    server.use(
      http.get(`${API}/dash/orgs/${ORG_ID}`, () =>
        HttpResponse.json({ ...mockOrgDetails, apps: [] }),
      ),
      http.delete(`${API}/dash/orgs/${ORG_ID}`, () => {
        deleted = true;
        return HttpResponse.json({ ok: true });
      }),
    );

    const { router } = render({ route: `/w/${ORG_ID}/settings` });
    await screen.findByLabelText('Name');

    await user.click(screen.getByRole('button', { name: 'Delete org' }));
    const dialog = await screen.findByRole('dialog');
    await user.type(within(dialog).getByRole('textbox'), 'Acme');
    await user.click(
      within(dialog).getByRole('button', { name: 'Delete org' }),
    );

    await waitFor(() => {
      expect(deleted).toBe(true);
    });
    await waitFor(() => {
      expect(router.state.location.pathname).toBe('/w/personal');
    });
  });
});
