// @vitest-environment jsdom
import React, { createElement } from 'react';
import { act, cleanup, render, screen } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { lazyDashboard } from './LazyDashboard';

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});
test('a failed page import can be retried without losing navigation', async () => {
  vi.spyOn(console, 'error').mockImplementation(() => {});
  const load = vi
    .fn()
    .mockRejectedValueOnce(new Error('offline'))
    .mockResolvedValue({
      default: () => createElement('h1', null, 'Recovered page'),
    });
  const Page = lazyDashboard(load);
  await act(async () => {
    render(
      createElement(
        'main',
        null,
        createElement('nav', null, 'Navigation'),
        createElement(Page),
      ),
    );
  });
  await screen.findByText('This page could not load');
  expect(screen.getByText('Navigation')).toBeTruthy();
  await act(async () => screen.getByText('Retry loading page').click());
  await screen.findByText('Recovered page');
  expect(load).toHaveBeenCalledTimes(2);
});
test('inactive lazy pages do not import their implementation', () => {
  const load = vi.fn(async () => ({
    default: () => createElement('h1', null, 'Heavy page'),
  }));
  lazyDashboard(load);
  expect(load).not.toHaveBeenCalled();
});
