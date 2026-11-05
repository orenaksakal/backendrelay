import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import {
  mockPersonalApps,
  mockQueryPermissionsCheck,
  mockTransactPermissionsCheck,
} from '@/test/fixtures/api/apps';
import { mockDash } from '@/test/msw/handlers';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const API = 'http://localhost:8888';
const ROUTE = '/apps/app-1/permissions';

vi.mock('@/components/code-editor/code-editor', () => ({
  CodeEditor: (props: {
    value: string;
    onChange?: (value: string) => void;
    mode: string;
    schema?: unknown;
    'aria-label': string;
  }) => (
    <div>
      <textarea
        aria-label={props['aria-label']}
        value={props.value}
        onChange={(event) => {
          props.onChange?.(event.target.value);
        }}
      />
      <span data-testid="editor-mode">{props.mode}</span>
      <span data-testid="editor-schema">
        {props.schema === undefined ? 'no-schema' : 'schema'}
      </span>
    </div>
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

async function renderPermissions(route = `${ROUTE}?view=json`) {
  const result = render({ route });
  await waitFor(() => {
    const heading = screen.queryByRole('heading', {
      level: 1,
      name: 'Permissions',
    });
    const dialog = screen.queryByRole('dialog');
    expect(heading ?? dialog).not.toBeNull();
  });
  return result;
}

function rulesEditor(): HTMLTextAreaElement {
  return screen.getByLabelText<HTMLTextAreaElement>('Permission rules JSON');
}

describe('permissions page', () => {
  it('opens the JSON view with the rules and their schema', async () => {
    await renderPermissions();

    const editor = await screen.findByLabelText('Permission rules JSON');
    expect((editor as HTMLTextAreaElement).value).toContain('"todos"');
    expect(screen.getByTestId('editor-mode')).toHaveTextContent(
      'json-with-schema',
    );
    expect(screen.getByTestId('editor-schema')).toHaveTextContent('schema');
    expect(screen.getByText('rules.json')).toBeInTheDocument();
  });

  it('reviews the diff and saves the parsed draft', async () => {
    const user = userEvent.setup();
    let saved: unknown;
    server.use(
      http.post(`${API}/dash/apps/app-1/rules`, async ({ request }) => {
        const body = (await request.json()) as { code?: unknown };
        saved = body;
        return HttpResponse.json({
          rules: { app_id: 'app-1', code: body.code, version: 4 },
        });
      }),
    );

    await renderPermissions();

    const nextRules = {
      todos: {
        allow: {
          view: 'true',
          create: 'auth.id != null',
          update: 'auth.id != null',
          delete: 'false',
        },
      },
      goals: { allow: { create: 'false', update: 'false', delete: 'false' } },
    };
    fireEvent.change(rulesEditor(), {
      target: { value: JSON.stringify(nextRules, null, 2) },
    });

    expect(screen.getByText('You have unsaved changes')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Save' }));

    const dialog = await screen.findByRole('dialog');
    expect(
      within(dialog).getByText('Review permission changes'),
    ).toBeInTheDocument();
    const diff = dialog.querySelector('pre');
    if (!(diff instanceof HTMLElement)) throw new Error('diff is missing');
    expect(diff).toHaveTextContent(/\+.*auth\.id != null/);

    await user.click(
      within(dialog).getByRole('button', { name: 'Save rules' }),
    );

    await waitFor(() => {
      expect(saved).toEqual({ code: nextRules });
    });
    expect(await screen.findByText('Permissions saved')).toBeInTheDocument();
    await waitFor(() => {
      expect(
        screen.queryByText('You have unsaved changes'),
      ).not.toBeInTheDocument();
    });
  });

  it('keeps a save with invalid JSON off the wire', async () => {
    const user = userEvent.setup();
    let posted = false;
    server.use(
      http.post(`${API}/dash/apps/app-1/rules`, () => {
        posted = true;
        return HttpResponse.json({ rules: null });
      }),
    );

    await renderPermissions();

    fireEvent.change(rulesEditor(), { target: { value: '{"todos":' } });
    await user.click(screen.getByRole('button', { name: 'Save' }));

    const dialog = await screen.findByRole('dialog');
    await user.click(
      within(dialog).getByRole('button', { name: 'Save rules' }),
    );

    expect(
      await within(dialog).findByText('Rules must be valid JSON.'),
    ).toBeInTheDocument();
    expect(posted).toBe(false);
  });

  it('restores a version into the draft without saving it', async () => {
    const user = userEvent.setup();
    const { router } = await renderPermissions(`${ROUTE}?view=history`);

    const table = await screen.findByRole('table', { name: 'Rule versions' });
    expect(within(table).getByText('v3')).toBeInTheDocument();
    expect(within(table).getByText('v2')).toBeInTheDocument();

    await user.click(
      within(table).getByRole('button', { name: 'View version 2' }),
    );

    expect(await screen.findByText('Version 2')).toBeInTheDocument();
    expect(screen.getByText('Current → v2')).toBeInTheDocument();

    await user.click(
      screen.getByRole('button', { name: 'Restore this version' }),
    );
    const confirm = await screen.findByRole('dialog');
    await user.click(
      within(confirm).getByRole('button', { name: 'Restore this version' }),
    );

    await waitFor(() => {
      expect(router.state.location.search).toMatchObject({ view: 'json' });
    });
    const restored = await screen.findByLabelText<HTMLTextAreaElement>(
      'Permission rules JSON',
    );
    expect(restored.value).toContain('"view": "auth.id != null"');
    expect(screen.getByText('You have unsaved changes')).toBeInTheDocument();
  });

  it('stops app members before loading the rules', async () => {
    server.use(
      http.get(`${API}/dash`, () =>
        HttpResponse.json({
          ...mockDash,
          apps: mockPersonalApps.map((app) =>
            app.id === 'app-1' ? { ...app, role: 'app-member' } : app,
          ),
        }),
      ),
    );

    await renderPermissions();

    expect(
      await screen.findByText("You don't have access to permissions"),
    ).toBeInTheDocument();
    expect(
      screen.getByText('This needs the collaborator role.'),
    ).toBeInTheDocument();
    expect(
      screen.queryByLabelText('Permission rules JSON'),
    ).not.toBeInTheDocument();
  });

  it('lists every namespace with its effective state and filters public writes', async () => {
    const user = userEvent.setup();
    const { router } = await renderPermissions(`${ROUTE}?view=overview`);

    const table = await screen.findByRole('table', {
      name: 'Permission overview',
    });
    expect(within(table).getByText('todos')).toBeInTheDocument();
    expect(within(table).getByText('goals')).toBeInTheDocument();
    expect(within(table).getAllByText('Everyone').length).toBeGreaterThan(0);
    expect(within(table).getAllByText('Admin only').length).toBeGreaterThan(0);
    expect(
      await within(table).findByRole('button', { name: 'Open $users rules' }),
    ).not.toHaveTextContent('⚠');
    expect(
      within(table).getByRole('button', { name: 'Open $users create rule' }),
    ).toHaveTextContent('Everyone');

    await user.click(
      screen.getByRole('checkbox', { name: 'Hide system namespaces' }),
    );
    expect(within(table).queryByText('$default')).not.toBeInTheDocument();
    expect(within(table).getByText('todos')).toBeInTheDocument();

    const publicWrites = screen.getByRole('checkbox', {
      name: 'Only public writes',
    });
    await user.click(publicWrites);
    expect(within(table).queryByText('goals')).not.toBeInTheDocument();
    expect(within(table).getByText('todos')).toBeInTheDocument();
    expect(router.state.location.search).toMatchObject({ view: 'overview' });
  });

  it('opens the editor on the namespace and action behind an overview cell', async () => {
    const user = userEvent.setup();
    const { router } = await renderPermissions(`${ROUTE}?view=overview`);

    await user.click(
      await screen.findByRole('button', { name: 'Open goals create rule' }),
    );

    expect(
      await screen.findByRole('heading', { level: 2, name: 'goals' }),
    ).toBeInTheDocument();
    expect(screen.getByRole('tab', { name: 'Editor' })).toHaveAttribute(
      'aria-selected',
      'true',
    );
    expect(router.state.location.search).toMatchObject({
      view: 'editor',
      ns: 'goals',
    });
  });

  it('opens Test-as from the editor for admins', async () => {
    const user = userEvent.setup();
    await renderPermissions(`${ROUTE}?view=editor`);

    await user.click(screen.getByRole('button', { name: 'Test as…' }));

    expect(
      await screen.findByRole('heading', { name: 'Test as…' }),
    ).toBeInTheDocument();
    expect(screen.getByLabelText('InstaQL query')).toBeInTheDocument();
  });

  it('runs a Test-as query with the draft rules and shows the checks', async () => {
    const user = userEvent.setup();
    let body: unknown;
    server.use(
      http.post(`${API}/admin/query_perms_check`, async ({ request }) => {
        body = await request.json();
        return HttpResponse.json(mockQueryPermissionsCheck);
      }),
    );

    await renderPermissions(`${ROUTE}?test=1`);

    expect(
      await screen.findByRole('heading', { name: 'Test as…' }),
    ).toBeInTheDocument();
    expect(screen.getByLabelText('InstaQL query')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Run' }));

    expect(
      await screen.findByRole('group', { name: 'Query result' }),
    ).toBeInTheDocument();
    const checks = await screen.findByRole('table', {
      name: 'Permission checks',
    });
    expect(within(checks).getByText('auth.id != null')).toBeInTheDocument();
    expect(within(checks).getByText('✓ allowed')).toBeInTheDocument();

    expect(body).toMatchObject({
      query: { goals: {} },
    });
    expect(body).toHaveProperty('rules-override');
  });

  it('runs Test-as transactions as a dry run by default', async () => {
    const user = userEvent.setup();
    let body: unknown;
    server.use(
      http.post(`${API}/admin/transact_perms_check`, async ({ request }) => {
        body = await request.json();
        return HttpResponse.json(mockTransactPermissionsCheck);
      }),
    );

    await renderPermissions(`${ROUTE}?test=1`);
    await user.click(screen.getByRole('button', { name: 'Transaction' }));

    expect(screen.getByText(/This runs against real data/)).toBeInTheDocument();
    expect(screen.getByText(/Dry run: nothing is written/)).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Run' }));

    const checks = await screen.findByRole('table', {
      name: 'Permission checks',
    });
    expect(within(checks).getByText('✗ denied')).toBeInTheDocument();
    expect(
      await screen.findByText('Some checks did not pass.'),
    ).toBeInTheDocument();
    expect(body).toMatchObject({ 'dangerously-commit-tx': false });
  });

  it('keeps Test-as away from collaborators even with the URL parameter', async () => {
    server.use(
      http.get(`${API}/dash`, () =>
        HttpResponse.json({
          ...mockDash,
          apps: mockPersonalApps.map((app) =>
            app.id === 'app-1' ? { ...app, role: 'collaborator' } : app,
          ),
        }),
      ),
    );

    await renderPermissions(`${ROUTE}?test=1`);

    expect(
      await screen.findByRole('navigation', { name: 'Permission namespaces' }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole('button', { name: 'Test as…' }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole('heading', { name: 'Test as…' }),
    ).not.toBeInTheDocument();
  });
});
