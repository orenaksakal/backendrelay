import type { Role } from '@/pages/dash';

const roleScores: Record<Role, number> = {
  owner: 4,
  admin: 3,
  collaborator: 2,
  'app-member': 1,
};

export function getAssignableRoles({
  theirRole,
  myRole,
}: {
  theirRole: Role;
  myRole: Role;
}): Role[] {
  if (
    roleScores[myRole] <= roleScores.collaborator ||
    roleScores[theirRole] > roleScores[myRole]
  )
    return [];
  return (Object.keys(roleScores) as Role[]).filter(
    (role) => role !== 'app-member' && roleScores[role] <= roleScores[myRole],
  );
}
