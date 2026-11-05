import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { mockDash } from '@/test/msw/handlers';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const API = 'http://localhost:8888';
const ROUTE = '/operator/settings';

vi.mock('@/components/code-editor/code-editor', () => ({
  CodeEditor: (props: {
    value: string;
    onChange?: (value: string) => void;
    'aria-label': string;
  }) => (
    <textarea
      aria-label={props['aria-label']}
      value={props.value}
      onChange={(event) => {
        props.onChange?.(event.target.value);
      }}
    />
  ),
}));

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

async function renderSettings() {
  const result = render({ route: ROUTE });
  await screen.findByRole('heading', { level: 1, name: 'Operator settings' });
  await screen.findByRole('radio', { name: 'Restricted' });
  return result;
}

describe('operator settings', () => {
  it('shows every group with the stored values', async () => {
    await renderSettings();

    expect(
      screen.getByRole('heading', { level: 2, name: 'Sign-ups' }),
    ).toBeInTheDocument();
    expect(screen.getByRole('radio', { name: 'Restricted' })).toBeChecked();
    expect(screen.getByLabelText('Allowed email addresses')).toHaveValue(
      'dev@example.com\nops@example.com',
    );
    expect(
      screen.getByRole('switch', { name: 'Allow ephemeral app creation' }),
    ).toBeChecked();
    expect(screen.getByLabelText('Requests per hour')).toHaveValue('25');
    expect(screen.getByLabelText('Default expiry')).toHaveValue('15');
    expect(screen.getByLabelText('SSRF allowlist IPs')).toHaveValue(
      '10.0.0.25',
    );
    expect(await screen.findByText('todo-app')).toBeInTheDocument();
    expect(
      screen.getByRole('checkbox', { name: 'Throttle todo-app' }),
    ).toBeChecked();
    expect(screen.getByLabelText('Throttle window')).toHaveValue('750');

    const advanced = await screen.findByRole('table', {
      name: 'Advanced flags',
    });
    expect(within(advanced).getByText('team-emails')).toBeInTheDocument();
    expect(within(advanced).getByText('legacy-flag')).toBeInTheDocument();
    expect(
      within(advanced).queryByText('dashboard-signups'),
    ).not.toBeInTheDocument();
  });

  it('returns 403 for an account that is not the operator', async () => {
    server.use(
      http.get(`${API}/dash`, () =>
        HttpResponse.json({ ...mockDash, superuser: false }),
      ),
    );

    render({ route: ROUTE });

    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent(
      "You don't have access to operator settings",
    );
    expect(
      screen.queryByRole('heading', { level: 1, name: 'Operator settings' }),
    ).not.toBeInTheDocument();
  });

  it('saves only the changed group from the unsaved-changes bar', async () => {
    const user = userEvent.setup();
    const writes: Array<{ name: string; value: unknown }> = [];
    server.use(
      http.put(
        `${API}/dash/operator/flags/:name`,
        async ({ params, request }) => {
          const body = (await request.json()) as { value?: unknown };
          const value = body.value;
          writes.push({ name: String(params['name']), value });
          return HttpResponse.json({
            name: String(params['name']),
            value,
          });
        },
      ),
    );

    await renderSettings();

    await user.click(screen.getByRole('radio', { name: 'Closed' }));
    expect(
      await screen.findByText('You have unsaved changes'),
    ).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => {
      expect(writes).toHaveLength(1);
    });
    expect(writes[0]).toEqual({
      name: 'dashboard-signups',
      value: {
        mode: 'closed',
        allowedEmails: ['dev@example.com', 'ops@example.com'],
      },
    });
    expect(
      await screen.findByText('Operator settings saved'),
    ).toBeInTheDocument();
    await waitFor(() => {
      expect(
        screen.queryByText('You have unsaved changes'),
      ).not.toBeInTheDocument();
    });
  });

  it('blocks an invalid number and explains the field', async () => {
    const user = userEvent.setup();
    const writes: string[] = [];
    server.use(
      http.put(`${API}/dash/operator/flags/:name`, ({ params }) => {
        writes.push(String(params['name']));
        return HttpResponse.json({ name: String(params['name']), value: 0 });
      }),
    );

    await renderSettings();

    const rate = screen.getByLabelText('Requests per hour');
    await user.clear(rate);

    expect(
      await screen.findByText('You have unsaved changes'),
    ).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Save' }));

    expect(
      await screen.findByText('Use a whole number of 1 or more.'),
    ).toBeInTheDocument();
    expect(writes).toHaveLength(0);

    await user.click(screen.getByRole('button', { name: 'Reset' }));
    await waitFor(() => {
      expect(screen.getByLabelText('Requests per hour')).toHaveValue('25');
    });
    expect(
      screen.queryByText('You have unsaved changes'),
    ).not.toBeInTheDocument();
  });

  it('stages an advanced flag edit and saves its JSON value', async () => {
    const user = userEvent.setup();
    const writes: Array<{ name: string; value: unknown }> = [];
    server.use(
      http.put(
        `${API}/dash/operator/flags/:name`,
        async ({ params, request }) => {
          const body = (await request.json()) as { value?: unknown };
          const value = body.value;
          writes.push({ name: String(params['name']), value });
          return HttpResponse.json({
            name: String(params['name']),
            value,
          });
        },
      ),
    );

    await renderSettings();

    await user.click(
      await screen.findByRole('button', { name: 'Edit team-emails' }),
    );
    const sheet = await screen.findByRole('dialog');
    const editor = within(sheet).getByLabelText('team-emails JSON value');
    fireEvent.change(editor, {
      target: { value: '[{"email":"new@example.com"}]' },
    });
    await user.click(within(sheet).getByRole('button', { name: 'Apply' }));

    expect(
      await screen.findByText('You have unsaved changes'),
    ).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => {
      expect(writes).toEqual([
        { name: 'team-emails', value: [{ email: 'new@example.com' }] },
      ]);
    });
  });

  it('resets an advanced flag with DELETE on save', async () => {
    const user = userEvent.setup();
    const deletes: string[] = [];
    server.use(
      http.delete(`${API}/dash/operator/flags/:name`, ({ params }) => {
        deletes.push(String(params['name']));
        return HttpResponse.json({
          name: String(params['name']),
          deleted: true,
        });
      }),
    );

    await renderSettings();

    await user.click(
      await screen.findByRole('button', { name: 'Edit storage-quota-bytes' }),
    );
    const sheet = await screen.findByRole('dialog');
    await user.click(
      within(sheet).getByRole('button', { name: 'Reset to default' }),
    );

    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => {
      expect(deletes).toEqual(['storage-quota-bytes']);
    });
  });
});
