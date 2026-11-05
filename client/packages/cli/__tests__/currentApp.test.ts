import { Effect } from 'effect';
import { describe, expect, it, vi } from 'vitest';

vi.mock('../src/commands/login.ts', () => ({
  loginCommand: vi.fn(),
}));
import { resolveConfiguredApp } from '../src/context/currentApp.ts';

const appId = 'a5c9caef-21a3-495a-8f4f-7111c62ff607';

describe('instant.config.ts app resolution', () => {
  it('resolves a named app to its configured ID', async () => {
    await expect(
      Effect.runPromise(
        resolveConfiguredApp('staging', {
          apps: { staging: { id: appId } },
        }),
      ),
    ).resolves.toEqual({ appId, source: 'flag' });
  });

  it('rejects a missing configured app name', async () => {
    const error = await Effect.runPromise(
      resolveConfiguredApp('staging', { apps: {} }).pipe(Effect.flip),
    );

    expect(error.message).toBe(
      'App ID not found in instant.config.ts: staging',
    );
  });

  it('rejects configured app IDs that are not UUIDs', async () => {
    const error = await Effect.runPromise(
      resolveConfiguredApp('staging', {
        apps: { staging: { id: 'not-a-uuid' } },
      }).pipe(Effect.flip),
    );

    expect(error.message).toBe(
      'App ID for staging in instant.config.ts is not a valid UUID',
    );
  });
});
