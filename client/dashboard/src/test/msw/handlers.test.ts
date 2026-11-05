import { describe, expect, it } from 'vitest';

describe('MSW handlers', () => {
  it('serves the public config', async () => {
    const response = await fetch('http://localhost:8888/dash/config');

    expect(response.status).toBe(200);
    const body: unknown = await response.json();
    expect(body).toMatchObject({
      version: 'dev',
      signup: 'open',
      googleLogin: false,
    });
  });
});
