import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { mockPersonalApps, mockTodoApp } from '@/test/fixtures/api/apps';
import { mockDash } from '@/test/msw/handlers';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const API = 'http://localhost:8888';

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

function dataRow(table: HTMLElement, index: number): HTMLElement {
  const rows = within(table).getAllByRole('row');
  const row = rows[index + 1];
  if (row === undefined) throw new Error(`No data row ${String(index)}`);
  return row;
}

describe('apps home', () => {
  it('lists the workspace apps with usage, status and IDs', async () => {
    render({ route: '/w/personal' });

    const table = await screen.findByRole('table', { name: 'Apps' });
    const todo = within(table).getByText('todo-app').closest('tr');
    expect(todo).not.toBeNull();
    if (todo === null) return;

    expect(within(todo).getByText('Active')).toBeInTheDocument();
    expect(within(todo).getByText('12')).toBeInTheDocument();
    expect(within(todo).getByText('4.2 MB')).toBeInTheDocument();
    expect(within(todo).getByText('120 MB')).toBeInTheDocument();
    expect(
      within(todo).getByRole('button', { name: 'Copy ID app-1' }),
    ).toBeInTheDocument();

    const readOnly = within(table).getByText('headshot-ai').closest('tr');
    expect(readOnly).not.toBeNull();
    if (readOnly === null) return;
    expect(within(readOnly).getByText('Read-only')).toBeInTheDocument();
    expect(within(readOnly).getByText('never')).toBeInTheDocument();

    expect(within(table).getByText('Disabled')).toBeInTheDocument();
  });

  it('searches titles and IDs, then filters by status', async () => {
    const user = userEvent.setup();
    render({ route: '/w/personal' });
    await screen.findByRole('table', { name: 'Apps' });

    const search = screen.getByRole('searchbox', { name: 'Search apps' });
    await user.type(search, 'headshot');
    await waitFor(() => {
      expect(screen.queryByText('todo-app')).not.toBeInTheDocument();
    });
    expect(screen.getByText('headshot-ai')).toBeInTheDocument();

    await user.clear(search);
    await waitFor(() => {
      expect(screen.getByText('todo-app')).toBeInTheDocument();
    });

    await user.type(search, 'app-3');
    await waitFor(() => {
      expect(screen.queryByText('todo-app')).not.toBeInTheDocument();
    });
    expect(screen.getByText('analytics')).toBeInTheDocument();

    await user.clear(search);
    await waitFor(() => {
      expect(screen.getByText('todo-app')).toBeInTheDocument();
    });

    await user.click(
      screen.getByRole('combobox', { name: 'Filter by status' }),
    );
    await user.click(await screen.findByRole('option', { name: 'Read-only' }));

    await waitFor(() => {
      expect(screen.queryByText('todo-app')).not.toBeInTheDocument();
      expect(screen.queryByText('analytics')).not.toBeInTheDocument();
    });
    expect(screen.getByText('headshot-ai')).toBeInTheDocument();
  });

  it('sorts from the header and keeps ?sort= and ?dir= in the URL', async () => {
    const user = userEvent.setup();
    const { router } = render({ route: '/w/personal' });
    const table = await screen.findByRole('table', { name: 'Apps' });

    expect(dataRow(table, 0)).toHaveTextContent('todo-app');

    await user.click(within(table).getByRole('button', { name: 'Sessions' }));

    await waitFor(() => {
      expect(router.state.location.search).toMatchObject({
        sort: 'sessions',
        dir: 'asc',
      });
    });
    await waitFor(() => {
      expect(dataRow(table, 0)).toHaveTextContent('headshot-ai');
    });
    expect(
      within(table).getByRole('columnheader', { name: 'Sessions' }),
    ).toHaveAttribute('aria-sort', 'ascending');
  });

  it('reads the sort order back out of the URL', async () => {
    render({ route: '/w/personal?sort=sessions&dir=asc' });
    const table = await screen.findByRole('table', { name: 'Apps' });

    expect(dataRow(table, 0)).toHaveTextContent('headshot-ai');
    expect(dataRow(table, 2)).toHaveTextContent('todo-app');
  });

  it('pins an app first and remembers the pin per workspace', async () => {
    const user = userEvent.setup();
    render({ route: '/w/personal' });
    const table = await screen.findByRole('table', { name: 'Apps' });

    await user.click(
      within(table).getByRole('button', { name: 'Pin headshot-ai' }),
    );

    await waitFor(() => {
      expect(dataRow(table, 0)).toHaveTextContent('headshot-ai');
    });
    expect(window.localStorage.getItem('br.pins.personal')).toBe(
      JSON.stringify(['app-2']),
    );

    await user.click(
      within(table).getByRole('button', { name: 'Unpin headshot-ai' }),
    );
    await waitFor(() => {
      expect(dataRow(table, 0)).toHaveTextContent('todo-app');
    });
    expect(window.localStorage.getItem('br.pins.personal')).toBe(
      JSON.stringify([]),
    );
  });

  it('remembers the page size in br.pageSize.apps', async () => {
    const user = userEvent.setup();
    const apps = Array.from({ length: 60 }, (_, index) => ({
      ...mockTodoApp,
      id: `app-${String(index)}`,
      title: `App ${String(index)}`,
    }));
    const summaries = apps.map((app) => ({
      id: app.id,
      liveSessions: 0,
      appBytes: 0,
      storageBytes: 0,
      lastWriteAt: null,
      lastBackupAt: null,
    }));
    server.use(
      http.get(`${API}/dash`, () => HttpResponse.json({ ...mockDash, apps })),
      http.get(`${API}/dash/apps-summary`, () =>
        HttpResponse.json({ apps: summaries }),
      ),
    );

    render({ route: '/w/personal' });
    const table = await screen.findByRole('table', { name: 'Apps' });
    expect(within(table).getAllByRole('row')).toHaveLength(51);

    const pagination = screen.getByRole('navigation', { name: 'Pagination' });
    expect(pagination).toHaveTextContent('Showing 1–50 of 60');

    await user.click(
      within(pagination).getByRole('combobox', { name: 'Rows per page' }),
    );
    await user.click(await screen.findByRole('option', { name: '25' }));

    await waitFor(() => {
      expect(window.localStorage.getItem('br.pageSize.apps')).toBe('25');
    });
    expect(within(table).getAllByRole('row')).toHaveLength(26);
  });

  it('shows the empty state for a workspace without apps', async () => {
    server.use(
      http.get(`${API}/dash`, () =>
        HttpResponse.json({ ...mockDash, apps: [] }),
      ),
    );

    render({ route: '/w/personal' });

    expect(await screen.findByText('No apps yet')).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Create app' })).toHaveAttribute(
      'href',
      '/new?workspace=personal',
    );
    expect(screen.queryByRole('table')).not.toBeInTheDocument();
  });

  it('keeps the table header when filters match nothing and clears them', async () => {
    const user = userEvent.setup();
    render({ route: '/w/personal' });
    await screen.findByRole('table', { name: 'Apps' });

    await user.type(
      screen.getByRole('searchbox', { name: 'Search apps' }),
      'nothing-matches',
    );

    expect(
      await screen.findByText(/No results for these filters/),
    ).toBeInTheDocument();
    expect(screen.getByRole('table', { name: 'Apps' })).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Clear filters' }));

    await waitFor(() => {
      expect(screen.getByText('todo-app')).toBeInTheDocument();
    });
  });

  it('switches to the grid view', async () => {
    const user = userEvent.setup();
    render({ route: '/w/personal' });
    await screen.findByRole('table', { name: 'Apps' });

    await user.click(screen.getByRole('button', { name: 'Grid view' }));

    expect(
      screen.queryByRole('table', { name: 'Apps' }),
    ).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Grid view' })).toHaveAttribute(
      'aria-pressed',
      'true',
    );
    for (const app of mockPersonalApps) {
      expect(screen.getByRole('link', { name: app.title })).toBeInTheDocument();
    }

    await user.click(screen.getByRole('button', { name: 'List view' }));
    expect(
      await screen.findByRole('table', { name: 'Apps' }),
    ).toBeInTheDocument();
  });

  it('surfaces the summary request failure', async () => {
    server.use(
      http.get(`${API}/dash/apps-summary`, () =>
        HttpResponse.json(
          { type: 'database-error', message: 'Usage is unavailable.' },
          { status: 500 },
        ),
      ),
    );

    render({ route: '/w/personal' });

    expect(await screen.findByText("Couldn't load apps")).toBeInTheDocument();
    expect(screen.getByText('Usage is unavailable.')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Retry' })).toBeInTheDocument();
  });
});
