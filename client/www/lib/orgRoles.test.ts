import { expect, it } from 'vitest';
import { getAssignableRoles } from './orgRoles';

it('preserves the original organization role selector without privilege escalation', () => {
  expect(
    getAssignableRoles({ myRole: 'collaborator', theirRole: 'collaborator' }),
  ).toEqual([]);
  expect(getAssignableRoles({ myRole: 'admin', theirRole: 'owner' })).toEqual(
    [],
  );
  expect(
    getAssignableRoles({ myRole: 'admin', theirRole: 'collaborator' }),
  ).toEqual(['admin', 'collaborator']);
  expect(getAssignableRoles({ myRole: 'owner', theirRole: 'admin' })).toEqual([
    'owner',
    'admin',
    'collaborator',
  ]);
});
