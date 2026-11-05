import { z } from 'zod';

/**
 * C.12 URL state: `?mode=` selects the query or transact editor and `?as=`
 * selects who the debug request runs as. The email address and refresh token
 * are session state, not URL state, so a shared link never carries a credential.
 */

export const PLAYGROUND_MODES = ['query', 'transact'] as const;

export type PlaygroundMode = (typeof PLAYGROUND_MODES)[number];

export const PLAYGROUND_ACTORS = ['admin', 'guest', 'email', 'token'] as const;

export type PlaygroundActorKind = (typeof PLAYGROUND_ACTORS)[number];

export const PLAYGROUND_ACTOR_LABELS: Record<PlaygroundActorKind, string> = {
  admin: 'Admin',
  guest: 'Guest',
  email: 'User email',
  token: 'Token',
};

export function isPlaygroundMode(value: unknown): value is PlaygroundMode {
  return (
    typeof value === 'string' &&
    (PLAYGROUND_MODES as readonly string[]).includes(value)
  );
}

export function isPlaygroundActor(
  value: unknown,
): value is PlaygroundActorKind {
  return (
    typeof value === 'string' &&
    (PLAYGROUND_ACTORS as readonly string[]).includes(value)
  );
}

/** A hand-edited URL falls back to the default instead of failing the route. */
export const playgroundSearchSchema = z.object({
  mode: z.enum(PLAYGROUND_MODES).catch('query'),
  as: z.enum(PLAYGROUND_ACTORS).catch('admin'),
});

export type PlaygroundSearch = z.infer<typeof playgroundSearchSchema>;

export function playgroundSearchFor(state: {
  mode: PlaygroundMode;
  actor: PlaygroundActorKind;
}): PlaygroundSearch {
  return { mode: state.mode, as: state.actor };
}
