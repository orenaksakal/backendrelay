import { describe, expect, test } from 'vitest';

import { getDevtoolSrc } from '../../src/devtool.ts';

const defaultConfig = {
  position: 'bottom-right' as const,
  allowedHosts: ['localhost'],
};

describe('getDevtoolSrc', () => {
  test('requires a dashboard URI instead of defaulting to instantdb.com', () => {
    expect(() => getDevtoolSrc('app-id', defaultConfig)).toThrow(/dashURI/);
  });

  test('uses a configured dashboard URI', () => {
    expect(
      getDevtoolSrc('app-id', {
        ...defaultConfig,
        dashURI: 'https://dash.instant.example/',
      }),
    ).toBe('https://dash.instant.example/_devtool?appId=app-id');
  });
});
