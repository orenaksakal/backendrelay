import { Effect } from 'effect';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const mocks = vi.hoisted(() => ({
  loginCommand: vi.fn(),
  readAuthToken: vi.fn(),
}));

vi.mock('../src/commands/login.ts', () => ({
  loginCommand: mocks.loginCommand,
}));

vi.mock('../src/auth/index.ts', () => ({
  readAuthToken: mocks.readAuthToken,
}));

vi.mock('../src/lib/config.ts', () => ({
  getBaseUrl: Effect.succeed('https://api.instant.example'),
}));

import { AuthToken, AuthTokenLive } from '../src/context/authToken.ts';
import { GlobalOpts } from '../src/context/globalOpts.ts';
import { InstantHttp } from '../src/lib/http.ts';

const getToken = Effect.gen(function* () {
  return yield* (yield* AuthToken).getAuthToken;
});

const run = (coerce: boolean) =>
  Effect.runPromise(
    getToken.pipe(
      Effect.provide(AuthTokenLive({ coerce, allowAdminToken: true })),
      Effect.provideService(GlobalOpts, { yes: false }),
      Effect.provideService(InstantHttp, {} as any),
    ),
  );

beforeEach(() => {
  vi.clearAllMocks();
  vi.unstubAllEnvs();
  vi.stubEnv('INSTANT_CLI_AUTH_TOKEN', undefined);
  vi.stubEnv('INSTANT_APP_ADMIN_TOKEN', undefined);
  vi.stubEnv('INSTANT_ADMIN_TOKEN', undefined);
  mocks.readAuthToken.mockResolvedValue(null);
  mocks.loginCommand.mockReturnValue(
    Effect.succeed({ authToken: 'logged-in-token', source: 'file' as const }),
  );
});

describe('auth token coercion', () => {
  it('logs in and uses the resulting token when coercion is enabled', async () => {
    await expect(run(true)).resolves.toBe('logged-in-token');
    expect(mocks.loginCommand).toHaveBeenCalledWith({});
  });

  it('preserves the unauthenticated failure when coercion is disabled', async () => {
    await expect(run(false)).rejects.toThrow('You are not logged in');
    expect(mocks.loginCommand).not.toHaveBeenCalled();
  });
});
