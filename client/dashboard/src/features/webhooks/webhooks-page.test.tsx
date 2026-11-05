import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const API = 'http://localhost:8888';
const ROUTE = '/apps/app-1/webhooks';

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

async function renderWebhooks() {
  render({ route: ROUTE });
  await waitFor(() => {
    expect(
      screen.getByRole('heading', { level: 1, name: 'Webhooks' }),
    ).toBeInTheDocument();
  });
  await screen.findByRole('table', { name: 'Webhooks' });
}

async function openRowMenu(url: string) {
  const user = userEvent.setup();
  const actions = await screen.findByRole('button', {
    name: `Actions for ${url}`,
  });
  await user.click(actions);
  return user;
}

describe('webhooks page', () => {
  it('lists webhooks with namespaces, actions, status and last delivery', async () => {
    await renderWebhooks();

    const table = await screen.findByRole('table', { name: 'Webhooks' });
    expect(
      within(table).getByRole('link', {
        name: 'https://example.com/hooks/instant',
      }),
    ).toBeInTheDocument();
    expect(
      within(table).getByRole('link', {
        name: 'https://hooks.example.net/receive',
      }),
    ).toBeInTheDocument();

    // Four namespaces collapse to three badges plus "+1".
    expect(within(table).getByText('+1')).toBeInTheDocument();
    expect(within(table).getByText('todos')).toBeInTheDocument();

    // Actions are canonical and labelled.
    expect(within(table).getAllByText('Create')).toHaveLength(1);
    expect(within(table).getAllByText('Update')).toHaveLength(1);
    expect(within(table).getAllByText('Delete')).toHaveLength(1);

    expect(within(table).getAllByText('Active')).toHaveLength(1);
    expect(within(table).getAllByText('Disabled')).toHaveLength(1);

    // The list asks each webhook for its newest event.
    await waitFor(() => {
      expect(within(table).getAllByText('Delivered')).toHaveLength(2);
    });

    expect(
      screen.getByText('1 of 100 active webhooks used.'),
    ).toBeInTheDocument();
  });

  it('creates a webhook from the sheet', async () => {
    const user = userEvent.setup();
    let body: unknown;
    server.use(
      http.post(`${API}/dash/apps/app-1/webhooks`, async ({ request }) => {
        body = await request.json();
        return HttpResponse.json({
          webhook: {
            id: 'webhook-created',
            sink: { url: 'https://example.com/hooks/new' },
            namespaces: ['todos'],
            actions: ['create'],
            status: 'active',
            disabled_reason: null,
            created_at: '2026-10-27T10:00:00Z',
            updated_at: '2026-10-27T10:00:00Z',
          },
        });
      }),
    );

    await renderWebhooks();
    await user.click(screen.getByRole('button', { name: 'Create webhook' }));

    const sheet = await screen.findByRole('dialog');
    expect(
      within(sheet).getByRole('heading', { name: 'Create webhook' }),
    ).toBeInTheDocument();

    await user.type(
      within(sheet).getByRole('textbox', { name: 'URL' }),
      'https://example.com/hooks/new',
    );

    const namespaceInput = within(sheet).getByRole('combobox', {
      name: 'Namespaces',
    });
    await user.click(namespaceInput);
    await user.type(namespaceInput, 'todos');
    await user.click(await screen.findByRole('option', { name: 'todos' }));
    // Dismiss the combobox list before using the footer.
    if (screen.queryByRole('listbox') !== null) {
      await user.keyboard('{Escape}');
    }

    await user.click(
      within(sheet).getByRole('button', { name: 'Create webhook' }),
    );

    await waitFor(() => {
      expect(body).toEqual({
        url: 'https://example.com/hooks/new',
        namespaces: ['todos'],
        actions: ['create'],
      });
    });
    expect(await screen.findByText('Webhook created')).toBeInTheDocument();
  });

  it('refuses private hosts before calling the API', async () => {
    const user = userEvent.setup();
    let called = false;
    server.use(
      http.post(`${API}/dash/apps/app-1/webhooks`, () => {
        called = true;
        return HttpResponse.json({ webhook: {} });
      }),
    );

    await renderWebhooks();
    await user.click(screen.getByRole('button', { name: 'Create webhook' }));
    const sheet = await screen.findByRole('dialog');

    await user.type(
      within(sheet).getByRole('textbox', { name: 'URL' }),
      'https://127.0.0.1/hooks',
    );
    await user.click(
      within(sheet).getByRole('button', { name: 'Create webhook' }),
    );

    expect(
      (
        await within(sheet).findAllByText(
          'The webhook URL must point at a public host.',
        )
      ).length,
    ).toBeGreaterThan(0);
    expect(called).toBe(false);
  });

  it('disables a webhook with a reason', async () => {
    const user = userEvent.setup();
    let body: unknown;
    server.use(
      http.post(
        `${API}/dash/apps/app-1/webhooks/:webhookId/disable`,
        async ({ request }) => {
          body = await request.json();
          return HttpResponse.json({
            webhook: {
              id: 'webhook-1',
              sink: { url: 'https://example.com/hooks/instant' },
              namespaces: ['todos'],
              actions: ['create'],
              status: 'disabled',
              disabled_reason: 'Paused for maintenance',
              created_at: '2026-10-20T10:00:00Z',
              updated_at: '2026-10-27T10:00:00Z',
            },
          });
        },
      ),
    );

    await renderWebhooks();
    await openRowMenu('https://example.com/hooks/instant');
    fireEvent.click(await screen.findByRole('menuitem', { name: 'Disable' }));

    const dialog = await screen.findByRole('dialog');
    await user.type(
      within(dialog).getByRole('textbox', { name: 'Reason (optional)' }),
      'Paused for maintenance',
    );
    await user.click(
      within(dialog).getByRole('button', { name: 'Disable webhook' }),
    );

    await waitFor(() => {
      expect(body).toEqual({ reason: 'Paused for maintenance' });
    });
    expect(await screen.findByText('Webhook disabled')).toBeInTheDocument();
  });

  it('enables a disabled webhook immediately', async () => {
    let called = false;
    server.use(
      http.post(`${API}/dash/apps/app-1/webhooks/:webhookId/enable`, () => {
        called = true;
        return HttpResponse.json({
          webhook: {
            id: 'webhook-2',
            sink: { url: 'https://hooks.example.net/receive' },
            namespaces: ['todos'],
            actions: ['delete'],
            status: 'active',
            disabled_reason: null,
            created_at: '2026-10-10T10:00:00Z',
            updated_at: '2026-10-27T10:00:00Z',
          },
        });
      }),
    );

    await renderWebhooks();
    await openRowMenu('https://hooks.example.net/receive');
    fireEvent.click(await screen.findByRole('menuitem', { name: 'Enable' }));

    await waitFor(() => {
      expect(called).toBe(true);
    });
    expect(await screen.findByText('Webhook enabled')).toBeInTheDocument();
  });

  it('deletes a webhook after confirmation', async () => {
    const user = userEvent.setup();
    let deleted = false;
    server.use(
      http.delete(`${API}/dash/apps/app-1/webhooks/:webhookId`, () => {
        deleted = true;
        return HttpResponse.json({
          webhook: {
            id: 'webhook-1',
            sink: { url: 'https://example.com/hooks/instant' },
            namespaces: ['todos'],
            actions: ['create'],
            status: 'active',
            created_at: '2026-10-20T10:00:00Z',
            updated_at: '2026-10-20T10:00:00Z',
          },
        });
      }),
    );

    await renderWebhooks();
    await openRowMenu('https://example.com/hooks/instant');
    fireEvent.click(await screen.findByRole('menuitem', { name: 'Delete' }));
    const dialog = await screen.findByRole('dialog');
    await user.click(
      within(dialog).getByRole('button', { name: 'Delete webhook' }),
    );

    await waitFor(() => {
      expect(deleted).toBe(true);
    });
    expect(await screen.findByText('Webhook deleted')).toBeInTheDocument();
  });
});
