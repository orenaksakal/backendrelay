import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const API = 'http://localhost:8888';
const ROUTE = '/apps/app-1/webhooks/webhook-1';
const URL = 'https://example.com/hooks/instant';

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

async function renderDetail(route = ROUTE) {
  render({ route });
  await waitFor(() => {
    const heading = screen.queryByRole('heading', { level: 1, name: URL });
    const dialog = screen.queryByRole('dialog');
    expect(heading ?? dialog).not.toBeNull();
  });
}

describe('webhook detail page', () => {
  it('shows the signing card and the deliveries table', async () => {
    await renderDetail();

    expect(screen.getByText('Active')).toBeInTheDocument();
    expect(
      screen.getByText('http://localhost:8888/.well-known/webhooks/jwks.json'),
    ).toBeInTheDocument();
    expect(screen.getByText(/@instantdb\/webhooks/)).toBeInTheDocument();

    const table = await screen.findByRole('table', {
      name: 'Webhook deliveries',
    });
    expect(within(table).getAllByText('Delivered').length).toBeGreaterThan(0);
    expect(within(table).getByText('Failed')).toBeInTheDocument();
    expect(within(table).getByText('Pending')).toBeInTheDocument();
    expect(within(table).getByText('120 ms')).toBeInTheDocument();
    expect(within(table).getByText('HTTP 503')).toBeInTheDocument();
  });

  it('loads the next page of deliveries', async () => {
    const user = userEvent.setup();
    await renderDetail();

    await user.click(await screen.findByRole('button', { name: 'Load more' }));

    expect(
      await screen.findByRole('button', { name: '0/327/48953740' }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole('button', { name: 'Load more' }),
    ).not.toBeInTheDocument();
  });

  it('opens a delivery sheet and resends a recent event', async () => {
    const user = userEvent.setup();
    let resent = false;
    server.use(
      http.post(`${API}/dash/apps/app-1/webhooks/webhook-1/events/*`, () => {
        resent = true;
        return HttpResponse.json({
          event: {
            isn: '0/328/48953748',
            status: 'pending',
            attempts: [],
            next_attempt_after: null,
            created_at: '2026-10-26T12:00:00Z',
            updated_at: '2026-10-27T12:00:00Z',
          },
        });
      }),
    );

    await renderDetail(`${ROUTE}?event=0/328/48953748`);

    const sheet = await screen.findByRole('dialog');
    expect(
      within(sheet).getByRole('heading', { name: 'Delivery' }),
    ).toBeInTheDocument();
    expect(await within(sheet).findByText(/Buy milk/)).toBeInTheDocument();
    expect(
      within(sheet).getByRole('heading', { level: 3, name: 'Responses' }),
    ).toBeInTheDocument();

    const resend = within(sheet).getByRole('button', { name: 'Resend' });
    expect(resend).toBeEnabled();
    await user.click(resend);

    await waitFor(() => {
      expect(resent).toBe(true);
    });
    expect(await screen.findByText('Event resent')).toBeInTheDocument();
  });

  it('disables Resend for events older than 30 days', async () => {
    await renderDetail(`${ROUTE}?event=0/301/11111111`);

    const sheet = await screen.findByRole('dialog');
    expect(
      await within(sheet).findByText(
        'The payload snapshot is no longer available.',
      ),
    ).toBeInTheDocument();
    expect(
      within(sheet).getByRole('button', { name: 'Resend' }),
    ).toBeDisabled();
  });

  it('closes the delivery sheet and clears the search param', async () => {
    const user = userEvent.setup();
    await renderDetail(`${ROUTE}?event=0/328/48953748`);

    const sheet = await screen.findByRole('dialog');
    const closeButtons = within(sheet).getAllByRole('button', {
      name: 'Close',
    });
    const close = closeButtons.at(-1);
    if (close === undefined) throw new Error('Close button is missing');
    await user.click(close);

    await waitFor(() => {
      expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    });
  });
});
