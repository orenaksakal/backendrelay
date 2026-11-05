import { expect, test, vi } from 'vitest';
const { getLocal, setLocal } = vi.hoisted(() => ({
  getLocal: vi.fn(),
  setLocal: vi.fn(),
}));
vi.mock('./config', () => ({ getLocal, setLocal }));
import { getPinnedAppIds, togglePinnedApp } from './pinnedApps';
test.each([{}, 10, 'broken', null])(
  'malformed pinned-app preference does not block sign-in: %j',
  (value) => {
    getLocal.mockReturnValue(value);
    expect([...getPinnedAppIds()]).toEqual([]);
    togglePinnedApp('current');
    expect(setLocal).toHaveBeenLastCalledWith('personal-pinned-apps', [
      'current',
    ]);
  },
);
test('pinned preferences allow only string identifiers', () => {
  getLocal.mockReturnValue(['valid', {}, null, 'valid']);
  expect([...getPinnedAppIds()]).toEqual(['valid']);
});
