export type AuthUser = { id: string; email: string; created_at: string };
export type AuthInfo = {
  token: string | undefined;
  user: AuthUser | undefined;
};
export const emptyAuth: AuthInfo = { token: undefined, user: undefined };

export function parseAuthInfo(value: string | null): AuthInfo {
  try {
    const auth = JSON.parse(value ?? 'null');
    if (typeof auth?.token !== 'string' || !auth.token) return emptyAuth;
    if (
      auth.user != null &&
      (typeof auth.user.id !== 'string' ||
        typeof auth.user.email !== 'string' ||
        typeof auth.user.created_at !== 'string')
    )
      return emptyAuth;
    return {
      token: auth.token,
      user:
        auth.user == null
          ? undefined
          : {
              id: auth.user.id,
              email: auth.user.email,
              created_at: auth.user.created_at,
            },
    };
  } catch {
    return emptyAuth;
  }
}

export function readAuthInfo(): AuthInfo {
  try {
    return parseAuthInfo(window.localStorage.getItem('@AUTH'));
  } catch {
    return emptyAuth;
  }
}

export function persistAuthInfo(auth: AuthInfo) {
  try {
    window.localStorage.setItem('@AUTH', JSON.stringify(auth));
  } catch {
    /* Keep this session usable even when browser storage is full. */
  }
}
