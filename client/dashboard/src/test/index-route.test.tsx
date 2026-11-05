import { waitFor } from '@testing-library/react';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession, type Session } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { server } from '@/test/msw/node';
import { render } from './render';

const session: Session = {
  token: 'session-token',
  user: { id: 'user-1', email: 'owner@example.com' },
};

const dash = {
  apps: [
    {
      id: 'app-1',
      title: 'todo-app',
      created_at: '2026-01-01T00:00:00Z',
    },
  ],
  orgs: [],
  invites: [],
  superuser: false,
  user: { id: 'user-1', email: 'owner@example.com' },
};

beforeEach(() => {
  resetRuntimeConfig();
  window.localStorage.clear();
  setSession(session);
  server.use(
    http.get('http://localhost:8888/dash', () => HttpResponse.json(dash)),
  );
});

afterEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
});

describe('index route', () => {
  it('redirects to the last workspace and falls back to Personal', async () => {
    const { router } = render();

    await waitFor(() => {
      expect(router.state.location.pathname).toBe('/w/personal');
    });
  });
});
