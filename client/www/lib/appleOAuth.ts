export function appleClientMeta(teamId: string, keyId: string) {
  return teamId && keyId ? { teamId, keyId } : null;
}
