import test from 'node:test';
import assert from 'node:assert/strict';
import {
  connectionMatches,
  emptyWorkspaceDestination,
  filterWorkspaces,
  initialWorkspace,
  resolveWorkspace,
  singleQuery,
  workspaceCacheKey,
  workspaceHref,
} from '../components/dash/shell/model';
test('explicit workspace URLs override saved preferences, including explicit personal recovery', () => {
  assert.equal(initialWorkspace('org-b', 'org-a'), 'org-b');
  assert.equal(initialWorkspace('personal', 'org-a'), 'personal');
  assert.equal(initialWorkspace(null, 'org-a'), 'org-a');
  assert.equal(initialWorkspace(null, null), 'personal');
  assert.equal(singleQuery(['org-a', 'org-b']), null);
});
test('incoming organization links and browser back override the selected workspace', () => {
  const selected = { id: 'org-a', urlOrg: 'org-a' };
  assert.equal(resolveWorkspace(selected, 'org-b'), 'org-b');
  assert.equal(resolveWorkspace(selected, null), 'personal');
  assert.equal(
    resolveWorkspace({ id: 'org-b', urlOrg: 'org-b' }, 'org-a'),
    'org-a',
  );
});
test('explicit selection remains effective while its URL update is pending', () => {
  assert.equal(
    resolveWorkspace({ id: 'org-b', urlOrg: 'org-a' }, 'org-a'),
    'org-b',
  );
  assert.equal(
    resolveWorkspace({ id: 'personal', urlOrg: 'org-a' }, 'org-a'),
    'personal',
  );
  assert.equal(
    resolveWorkspace({ id: 'org-b', urlOrg: 'org-a' }, 'org-b'),
    'org-b',
  );
});
test('workspace links preserve scope without carrying another application or its filters', () => {
  const url = new URL(
    workspaceHref('/dash/user-settings', 'org & b', { tab: 'invites' }),
    'https://fixture.invalid',
  );
  assert.equal(url.searchParams.get('org'), 'org & b');
  assert.equal(url.searchParams.get('tab'), 'invites');
  assert.equal(url.searchParams.get('app'), null);
  assert.equal(url.searchParams.get('filters'), null);
  assert.equal(workspaceHref('/dash', 'personal'), '/dash');
});
test('empty dashboard routes choose invitations before creation and treat omitted invitations as empty', () => {
  assert.equal(emptyWorkspaceDestination(2, 1, 0), null);
  assert.equal(emptyWorkspaceDestination(0, 1, 0), '/dash/user-settings');
  assert.equal(emptyWorkspaceDestination(0, 0, 0), '/dash/onboarding');
  assert.equal(emptyWorkspaceDestination(0, 0, 2), '/dash/new');
});
test('workspace cache keys isolate authentication and skip personal or anonymous reads', () => {
  assert.notDeepEqual(
    workspaceCacheKey('account-a', 'org'),
    workspaceCacheKey('account-b', 'org'),
  );
  assert.notDeepEqual(
    workspaceCacheKey('account-a', 'org'),
    workspaceCacheKey('account-a', 'other-org'),
  );
  assert.equal(workspaceCacheKey(null, 'org'), null);
  assert.equal(workspaceCacheKey('account-a', 'personal'), null);
});
test('workspace picker search matches roles and IDs as well as names', () => {
  const rows = [
    { id: 'one', title: 'Product team', role: 'collaborator' },
    { id: 'two', title: 'Operations', role: 'owner' },
  ];
  assert.deepEqual(
    filterWorkspaces(rows, ' OWNER ').map((r) => r.id),
    ['two'],
  );
  assert.deepEqual(
    filterWorkspaces(rows, 'one').map((r) => r.id),
    ['one'],
  );
  assert.equal(filterWorkspaces(rows, 'missing').length, 0);
});
test('application content requires both the current application and current credential connection', () => {
  assert.equal(connectionMatches(null, 'new-app', 'new-credential'), false);
  assert.equal(
    connectionMatches(
      { appId: 'old-app', adminToken: 'old-credential' },
      'new-app',
      'new-credential',
    ),
    false,
  );
  assert.equal(
    connectionMatches(
      { appId: 'same-app', adminToken: 'old-credential' },
      'same-app',
      'new-credential',
    ),
    false,
  );
  assert.equal(
    connectionMatches(
      { appId: 'same-app', adminToken: 'new-credential' },
      'same-app',
      'new-credential',
    ),
    true,
  );
});
