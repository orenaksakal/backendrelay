import { fireEvent, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { mockPersonalApps, mockTodoApp } from '@/test/fixtures/api/apps';
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
    'aria-label': string;
    readOnly?: boolean;
  }) => (
    <textarea
      aria-label={props['aria-label']}
      value={props.value}
      readOnly={props.readOnly}
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

async function renderEditor(route = ROUTE) {
  const result = render({ route });
  await screen.findByRole('heading', { level: 1, name: 'Permissions' });
  await screen.findByRole('heading', { level: 2, name: 'todos' });
  await screen.findByLabelText('todos view rule');
  return result;
}

function ruleEditor(label: string): HTMLTextAreaElement {
  return screen.getByLabelText<HTMLTextAreaElement>(label);
}

describe('permissions visual editor', () => {
  it('opens the Editor by default with one row per action', async () => {
    await renderEditor();

    expect(screen.getByRole('tab', { name: 'Editor' })).toHaveAttribute(
      'aria-selected',
      'true',
    );
    expect(
      await screen.findByRole('heading', { level: 2, name: 'todos' }),
    ).toBeInTheDocument();
    expect(ruleEditor('todos view rule').value).toBe('true');
    expect(ruleEditor('todos delete rule').value).toBe('false');

    expect(screen.getAllByText('Everyone').length).toBeGreaterThan(0);
    expect(screen.getAllByText('Admin only').length).toBeGreaterThan(0);

    const sidebar = screen.getByRole('navigation', {
      name: 'Permission namespaces',
    });
    expect(within(sidebar).getByText('$default')).toBeInTheDocument();
    expect(within(sidebar).getByText('goals')).toBeInTheDocument();
    expect(await within(sidebar).findByText('$users')).toBeInTheDocument();
  });

  it('shows the implicit allow state for a namespace with no rule', async () => {
    const user = userEvent.setup();
    await renderEditor();

    await user.click(screen.getByRole('button', { name: /^goals/ }));

    expect(
      await screen.findByRole('heading', { level: 2, name: 'goals' }),
    ).toBeInTheDocument();
    expect(screen.getByText('No rule — allows everyone')).toBeInTheDocument();
    expect(screen.queryByText('Everyone')).not.toBeInTheDocument();
  });

  it.each([undefined, 'true', true])(
    'does not warn about public registration (%s), but warns on public user updates',
    async (create) => {
      const user = userEvent.setup();
      server.use(
        http.get(`${API}/dash/apps/app-1`, () =>
          HttpResponse.json({
            app: {
              ...mockTodoApp,
              rules: {
                todos: { allow: { create: 'true' } },
                $users: { allow: create === undefined ? {} : { create } },
              },
            },
          }),
        ),
      );
      await renderEditor();

      const sidebar = screen.getByRole('navigation', {
        name: 'Permission namespaces',
      });
      const users = within(sidebar).getByRole('button', { name: '$users' });
      expect(
        within(users).queryByText('Allows public writes'),
      ).not.toBeInTheDocument();
      expect(
        within(sidebar).getByRole('button', { name: /^todos/ }),
      ).toHaveTextContent('Allows public writes');

      await user.click(users);
      expect(screen.getByText('Everyone')).toBeInTheDocument();
      fireEvent.change(ruleEditor('$users update rule'), {
        target: { value: 'true' },
      });
      expect(users).toHaveTextContent('Allows public writes');
    },
  );

  it('locks an action to false and restores the previous expression', async () => {
    const user = userEvent.setup();
    await renderEditor();

    const update = ruleEditor('todos update rule');
    expect(update.value).toBe('auth.id != null');

    await user.click(
      screen.getByRole('button', { name: 'Lock update rule (admin only)' }),
    );
    expect(ruleEditor('todos update rule').value).toBe('false');
    expect(screen.getAllByText('Admin only').length).toBeGreaterThan(0);
    expect(screen.getByText('You have unsaved changes')).toBeInTheDocument();

    await user.click(
      screen.getByRole('button', { name: 'Unlock update rule' }),
    );
    expect(ruleEditor('todos update rule').value).toBe('auth.id != null');
  });

  it('resets one action to inherited from its row menu', async () => {
    const user = userEvent.setup();
    await renderEditor();

    await user.click(
      screen.getByRole('button', { name: 'Create rule actions' }),
    );
    await user.click(
      await screen.findByRole('menuitem', { name: 'Reset to inherited' }),
    );

    expect(ruleEditor('todos create rule').value).toBe('');
    expect(screen.getByText('No rule — allows everyone')).toBeInTheDocument();
    expect(screen.getByText('You have unsaved changes')).toBeInTheDocument();
  });

  it('adds link and field rows and keeps the array bind shape', async () => {
    const user = userEvent.setup();
    server.use(
      http.get(`${API}/dash/apps/app-1`, () =>
        HttpResponse.json({
          app: {
            ...mockTodoApp,
            rules: {
              todos: {
                allow: { view: 'true' },
                bind: ['ownerId', 'auth.id'],
              },
            },
          },
        }),
      ),
    );

    await renderEditor();
    await user.click(
      screen.getByRole('button', {
        name: /Advanced: link \/ unlink/,
      }),
    );

    expect(screen.getByText('No link rules.')).toBeInTheDocument();
    await user.click(screen.getByRole('combobox', { name: 'Add link rule' }));
    await user.click(await screen.findByRole('option', { name: 'owner' }));
    await user.click(screen.getByRole('button', { name: 'Add link rule' }));
    expect(ruleEditor('todos link owner rule').value).toBe('true');

    await user.click(screen.getByRole('combobox', { name: 'Add field rule' }));
    await user.click(await screen.findByRole('option', { name: 'title' }));
    await user.click(screen.getByRole('button', { name: 'Add field rule' }));
    expect(ruleEditor('todos field title rule').value).toBe('true');

    fireEvent.change(ruleEditor('todos bind ownerId'), {
      target: { value: 'auth.id != null' },
    });

    await user.click(screen.getByRole('tab', { name: 'JSON' }));
    const json = screen.getByLabelText<HTMLTextAreaElement>(
      'Permission rules JSON',
    );
    expect(json.value).toContain('"bind": [');
    expect(json.value).toContain('"ownerId"');
    expect(json.value).toContain('"auth.id != null"');
  });

  it('replaces the namespace allow block from a template after confirmation', async () => {
    const user = userEvent.setup();
    await renderEditor();

    await user.click(screen.getByRole('button', { name: 'Templates' }));
    await user.click(
      await screen.findByRole('menuitem', { name: 'Admin only' }),
    );

    const dialog = await screen.findByRole('dialog');
    expect(
      within(dialog).getByText('Replace rules for todos?'),
    ).toBeInTheDocument();
    await user.click(
      within(dialog).getByRole('button', { name: 'Replace rules' }),
    );

    expect(ruleEditor('todos view rule').value).toBe('false');
    expect(screen.getByText('You have unsaved changes')).toBeInTheDocument();
  });

  it('shows the server validation message on the row it belongs to', async () => {
    server.use(
      http.post(`${API}/dash/apps/app-1/rules/validate`, () =>
        HttpResponse.json({
          ok: false,
          errors: [
            {
              in: ['todos', 'allow', 'view'],
              message: 'Unknown variable "nobody"',
            },
          ],
        }),
      ),
    );

    await renderEditor();
    fireEvent.change(ruleEditor('todos view rule'), {
      target: { value: 'nobody.id != null' },
    });

    expect(
      await screen.findByText(
        'Unknown variable "nobody"',
        {},
        { timeout: 3_000 },
      ),
    ).toBeInTheDocument();
    expect(
      screen.queryByText('The server rejected this draft.'),
    ).not.toBeInTheDocument();
  });

  it('makes an unsupported namespace read-only with an explanation', async () => {
    server.use(
      http.get(`${API}/dash/apps/app-1`, () =>
        HttpResponse.json({
          app: {
            ...mockTodoApp,
            rules: { todos: { allow: { view: 'true' }, mystery: 'kept' } },
          },
        }),
      ),
    );

    await renderEditor();

    expect(
      await screen.findByText(
        "This namespace uses settings the visual editor doesn't support. Edit it in JSON.",
      ),
    ).toBeInTheDocument();
    expect(screen.getByText('Unknown setting "mystery".')).toBeInTheDocument();
    expect(ruleEditor('todos view rule')).toHaveAttribute('readonly');
  });

  it('keeps app members out of the editor', async () => {
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

    render({ route: ROUTE });
    await screen.findByRole('heading', { level: 1, name: 'Permissions' });

    expect(
      await screen.findByText("You don't have access to permissions"),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole('tab', { name: 'Editor' }),
    ).not.toBeInTheDocument();
  });
});
