import { render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { closeAllAppClients, peekAppClient } from '@/lib/instant';
import { loadRuntimeConfig, resetRuntimeConfig } from '@/lib/runtime-config';
import { mockTodoApp } from '@/test/fixtures/api/apps';
import { AppDataGate } from './app-data-gate';

const { initMock } = vi.hoisted(() => ({ initMock: vi.fn() }));

vi.mock('@instantdb/react', () => ({ init: initMock }));

function createFakeClient() {
  return {
    core: {
      shutdown: vi.fn(),
      subscribeQuery: () => () => undefined,
      _reactor: { subscribeAttrs: () => () => undefined },
    },
  };
}

beforeEach(async () => {
  resetRuntimeConfig();
  initMock.mockReset();
  initMock.mockImplementation(() => createFakeClient());
  await loadRuntimeConfig();
});

afterEach(() => {
  closeAllAppClients();
  resetRuntimeConfig();
});

function renderGate(role: 'owner' | 'admin' | 'collaborator' | 'app-member') {
  return render(
    <AppDataGate app={mockTodoApp} role={role} thing="explorer">
      <p>Explorer body</p>
    </AppDataGate>,
  );
}

describe('AppDataGate', () => {
  it('renders the screen and opens the app client for admins', async () => {
    renderGate('admin');

    expect(await screen.findByText('Explorer body')).toBeInTheDocument();
    expect(peekAppClient('app-1')).toBeDefined();
    expect(initMock).toHaveBeenCalledWith(
      expect.objectContaining({
        appId: 'app-1',
        apiURI: 'http://localhost:8888',
        websocketURI: 'ws://localhost:8888/runtime/session',
        __adminToken: 'app-1-admin-token-0123456789',
      }),
    );
  });

  it('lets owners through: the gate is admin-or-above, not admin-only', async () => {
    renderGate('owner');

    expect(await screen.findByText('Explorer body')).toBeInTheDocument();
  });

  it('shows the 403 state below the admin role', () => {
    renderGate('collaborator');

    const alert = screen.getByRole('alert');
    expect(alert).toHaveTextContent("You don't have access to explorer");
    expect(
      screen.getByText('The explorer needs admin access to this app.'),
    ).toBeInTheDocument();
    expect(screen.getByText('This needs the admin role.')).toBeInTheDocument();
    expect(screen.queryByText('Explorer body')).not.toBeInTheDocument();
    expect(peekAppClient('app-1')).toBeUndefined();
    expect(initMock).not.toHaveBeenCalled();
  });

  it('explains a missing admin token instead of connecting', () => {
    render(
      <AppDataGate
        app={{ ...mockTodoApp, admin_token: undefined }}
        // eslint-disable-next-line jsx-a11y/aria-role -- this is the app role, not an ARIA role
        role="admin"
        thing="explorer"
      >
        <p>Explorer body</p>
      </AppDataGate>,
    );

    expect(screen.getByRole('alert')).toHaveTextContent(
      "Couldn't load explorer",
    );
    expect(
      screen.getByText(
        "The admin token for this app isn't available. Reload the app to fetch it again.",
      ),
    ).toBeInTheDocument();
    expect(peekAppClient('app-1')).toBeUndefined();
  });

  it('explains a dashboard without runtime configuration', () => {
    resetRuntimeConfig();

    renderGate('admin');

    expect(
      screen.getByText(
        "The dashboard didn't load its API URL. Reload the page.",
      ),
    ).toBeInTheDocument();
    expect(peekAppClient('app-1')).toBeUndefined();
  });
});
