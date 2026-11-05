import { useSyncExternalStore } from 'react';
import { z } from 'zod';
import { setApiToken, setUnauthorizedHandler } from '@/lib/api/client';
import { userSchema } from '@/lib/api/endpoints/schemas';

/** The signed-in session, owned by D3.2. */
export const SESSION_STORAGE_KEY = 'br.session';

/**
 * Upstream's key. A first load copies it into `br.session`; the old entry stays
 * in place until D9.5 deletes the fallback.
 */
export const LEGACY_SESSION_STORAGE_KEY = '@AUTH';

/** Where a signed-out user is sent (`/login?redirect=<current>`). */
export const LOGIN_PATH = '/login';

export const sessionSchema = z.looseObject({
  token: z.string().min(1),
  user: userSchema,
});

export type SessionUser = z.infer<typeof userSchema>;
export type Session = z.infer<typeof sessionSchema>;

export type SessionListener = (session: Session | null) => void;
export type SessionCleanup = () => void;

let current: Session | null = null;
let loaded = false;
let started = false;
const listeners = new Set<SessionListener>();
const cleanups = new Set<SessionCleanup>();

/**
 * Per-session state outside this module registers its teardown here: the query
 * cache from `createQueryClient()` and, from D6.1, the Instant clients.
 */
export function registerSessionCleanup(cleanup: SessionCleanup): () => void {
  cleanups.add(cleanup);
  return () => {
    cleanups.delete(cleanup);
  };
}

/** Splits a stored document into `{token, user}` or `null` when it is not usable. */
export function parseStoredSession(
  raw: string | null | undefined,
): Session | null {
  if (raw === null || raw === undefined || raw === '') return null;
  try {
    const document: unknown = JSON.parse(raw);
    const parsed = sessionSchema.safeParse(document);
    return parsed.success ? parsed.data : null;
  } catch {
    return null;
  }
}

/**
 * Reads `br.session`, migrating `@AUTH` the first time the new key is missing.
 * A present-but-invalid `br.session` wins over the legacy key, so a sign-out is
 * never undone by the fallback.
 */
export function readStoredSession(
  storage: Storage | null = browserStorage(),
): Session | null {
  if (storage === null) return null;

  const stored = readKey(storage, SESSION_STORAGE_KEY);
  if (stored !== null) return parseStoredSession(stored);

  const migrated = parseStoredSession(
    readKey(storage, LEGACY_SESSION_STORAGE_KEY),
  );
  if (migrated === null) return null;
  writeKey(storage, SESSION_STORAGE_KEY, JSON.stringify(migrated));
  return migrated;
}

/** The current session. The first call reads storage and migrates `@AUTH`. */
export function getSession(): Session | null {
  if (!loaded) applySession(readStoredSession());
  return current;
}

/** Stores a freshly signed-in session and hands its token to the API client. */
export function setSession(session: Session): void {
  const storage = browserStorage();
  if (storage !== null) {
    writeKey(storage, SESSION_STORAGE_KEY, JSON.stringify(session));
  }
  applySession(session);
}

/** Removes both keys: leaving `@AUTH` behind would sign the user back in. */
export function clearStoredSession(): void {
  const storage = browserStorage();
  if (storage === null) return;
  removeKey(storage, SESSION_STORAGE_KEY);
  removeKey(storage, LEGACY_SESSION_STORAGE_KEY);
}

/** Clears the session in this tab without running per-session cleanups. */
export function clearSession(): void {
  clearStoredSession();
  applySession(null);
}

/**
 * Ends the session and drops everything derived from it: the API token, the
 * query cache and the Instant clients (through `registerSessionCleanup`).
 */
export function signOut(): void {
  clearSession();
  runCleanups();
}

export function subscribeSession(listener: SessionListener): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** Session snapshot for components. Cross-tab updates arrive through storage. */
export function useSession(): Session | null {
  return useSyncExternalStore(subscribeSession, getSession, getServerSession);
}

/**
 * Starts cross-tab sync, migrates `@AUTH` and installs the API client's 401
 * handler. Idempotent; the root route calls it after the runtime config loads.
 */
export function startSession(): void {
  if (started) return;
  started = true;
  getSession();
  setUnauthorizedHandler(handleUnauthorized);
  if (typeof window !== 'undefined') {
    window.addEventListener('storage', handleStorageEvent);
  }
}

/** `/login?redirect=<path>` for a same-origin path; anything else falls back to `/`. */
export function loginRedirectPath(currentPath: string): string {
  const path =
    currentPath.startsWith('/') && !currentPath.startsWith('//')
      ? currentPath
      : '/';
  return `${LOGIN_PATH}?redirect=${encodeURIComponent(path)}`;
}

/**
 * A 401 ends the session and sends the tab to the login screen. Sign-in
 * requests have no session yet, so their failures stay on the form.
 */
export function handleUnauthorized(): void {
  const wasSignedIn = getSession() !== null;
  signOut();
  if (wasSignedIn) navigateToLogin(loginRedirectPath(currentLocationPath()));
}

/** Replaces the full-page navigation after a 401. Tests pass a spy. */
export function setLoginNavigation(
  navigate: ((path: string) => void) | null,
): void {
  navigateToLogin = navigate ?? defaultLoginNavigation;
}

/** Test seam: forgets in-memory state and removes the storage listener. */
export function resetSession(): void {
  if (typeof window !== 'undefined') {
    window.removeEventListener('storage', handleStorageEvent);
  }
  started = false;
  loaded = false;
  current = null;
  listeners.clear();
  cleanups.clear();
  setApiToken(null);
  setUnauthorizedHandler(null);
  setLoginNavigation(null);
}

let navigateToLogin: (path: string) => void = defaultLoginNavigation;

function defaultLoginNavigation(path: string): void {
  window.location.assign(path);
}

function handleStorageEvent(event: StorageEvent): void {
  if (event.key !== null && event.key !== SESSION_STORAGE_KEY) return;

  const next = event.key === null ? null : parseStoredSession(event.newValue);
  const wasSignedIn = current !== null;
  applySession(next);
  // Another tab signed out: drop the caches this tab still holds.
  if (next === null && wasSignedIn) runCleanups();
}

function applySession(session: Session | null): void {
  loaded = true;
  const changed = current !== session;
  current = session;
  setApiToken(session?.token ?? null);
  if (changed) {
    for (const listener of listeners) listener(current);
  }
}

function runCleanups(): void {
  for (const cleanup of cleanups) {
    try {
      cleanup();
    } catch (error) {
      console.error('Session cleanup failed', error);
    }
  }
}

function getServerSession(): null {
  return null;
}

function currentLocationPath(): string {
  if (typeof window === 'undefined') return '/';
  const { pathname, search, hash } = window.location;
  return `${pathname}${search}${hash}`;
}

function browserStorage(): Storage | null {
  if (typeof window === 'undefined') return null;
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function readKey(storage: Storage, key: string): string | null {
  try {
    return storage.getItem(key);
  } catch {
    return null;
  }
}

function writeKey(storage: Storage, key: string, value: string): void {
  try {
    storage.setItem(key, value);
  } catch {
    /* Keep the session usable when storage is full or blocked. */
  }
}

function removeKey(storage: Storage, key: string): void {
  try {
    storage.removeItem(key);
  } catch {
    /* Keep signing out even when storage is unavailable. */
  }
}
