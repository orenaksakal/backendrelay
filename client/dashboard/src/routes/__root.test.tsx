import { screen } from '@testing-library/react';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

beforeEach(() => {
  resetRuntimeConfig();
});

afterEach(() => {
  resetRuntimeConfig();
});

describe('root route', () => {
  it('shows a plain error page when /config.json is invalid', async () => {
    server.use(http.get('/config.json', () => HttpResponse.json({})));

    render();

    expect(
      await screen.findByRole('heading', {
        level: 1,
        name: 'BackendRelay is not configured',
      }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole('heading', { name: 'BackendRelay dashboard' }),
    ).not.toBeInTheDocument();
  });
});
