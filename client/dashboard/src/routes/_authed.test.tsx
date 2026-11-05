import { screen } from '@testing-library/react';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { mockDash } from '@/test/msw/handlers';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';
import { Route } from './_authed';

const API = 'http://localhost:8888';
const DEEP_LINK = '/apps/app-1/overview';

beforeEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
});

afterEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
});

/** Write a cold session: storage only, with the in-memory state cleared. */
function storeSession(): void {
  setSession({ token: mockSignInToken, user: mockSignInUser });
  resetSession();
}

describe('authenticated layout', () => {
  it('restores a stored session before the guard loads protected data', async () => {
    storeSession();
    let authorization: string | null = null;
    server.use(
      http.get(`${API}/dash`, ({ request }) => {
        authorization = request.headers.get('authorization');
        return HttpResponse.json(mockDash);
      }),
    );

    render({ route: DEEP_LINK });

    expect(
      await screen.findByRole('heading', { level: 1, name: 'todo-app' }),
    ).toBeInTheDocument();
    expect(authorization).toBe(`Bearer ${mockSignInToken}`);
  });

  it('renders the authenticated branch on the client only', () => {
    // A server render cannot read `localStorage`, so it must not run the guard
    // (and redirect) before the browser has restored the session.
    expect(Route.options.ssr).toBe(false);
  });

  it('sends a deep link without a session to the sign-in screen', async () => {
    render({ route: DEEP_LINK });

    expect(
      await screen.findByRole('heading', {
        level: 1,
        name: 'Sign in to BackendRelay',
      }),
    ).toBeInTheDocument();
  });
});
