// @vitest-environment jsdom
import React from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { OriginalNavigation, originalTabIds } from './OriginalNavigation';

afterEach(cleanup);
const labels = [
  'Home',
  'Explorer',
  'Schema',
  'Permissions',
  'Auth',
  'Webhooks',
  'Backups',
  'Query Inspector',
  'Sandbox',
  'Admin',
  'OAuth Apps',
];
const items = originalTabIds.map((id, index) => ({ id, label: labels[index] }));
const props = {
  tab: 'home',
  items,
  href: (id: string) => `/dash?app=one&org=team&t=${id}`,
  onNavigate: vi.fn(),
};

describe('original dashboard navigation', () => {
  it('exposes the upstream pages in their original order without category tabs', () => {
    const { container } = render(
      React.createElement(OriginalNavigation, props),
    );
    expect(
      Array.from(
        container.querySelectorAll('nav > div > a'),
        (link) => link.textContent,
      ),
    ).toEqual(labels);
    expect(
      screen.getByRole('link', { name: 'Home' }).getAttribute('aria-current'),
    ).toBe('page');
    expect(
      screen.getByRole('link', { name: 'Explorer' }).getAttribute('href'),
    ).toBe('/dash?app=one&org=team&t=explorer');
  });
  it('does not expose removed tools', () => {
    render(
      React.createElement(OriginalNavigation, {
        ...props,
        items: [...items, { id: 'logs', label: 'Logs' }],
      }),
    );
    expect(screen.queryByRole('link', { name: 'Logs' })).toBeNull();
    expect(screen.queryByText('Rust tools')).toBeNull();
    expect(screen.getAllByRole('link').length).toBe(labels.length);
  });
  it('uses the original desktop selection colors and compact mobile tabs', () => {
    render(React.createElement(OriginalNavigation, props));
    const selected = screen.getByRole('link', { name: 'Home' });
    expect(selected.className).toContain('bg-gray-200');
    expect(selected.className).toContain('md:dark:bg-neutral-600/50');
    expect(selected.className).toContain('md:py-2');
    expect(selected.className).toContain('py-0.5');
    expect(selected.className).not.toContain('bg-gray-200/70');
  });
  it('never exposes role-filtered destinations', () => {
    render(
      React.createElement(OriginalNavigation, {
        ...props,
        items: items.filter((item) => item.id !== 'admin'),
      }),
    );
    expect(screen.queryByRole('link', { name: 'Admin' })).toBeNull();
    expect(screen.queryByText('Rust tools')).toBeNull();
  });
  it('uses client navigation for ordinary clicks and native links for modified clicks', () => {
    const onNavigate = vi.fn();
    render(React.createElement(OriginalNavigation, { ...props, onNavigate }));
    const link = screen.getByRole('link', { name: 'Explorer' });
    fireEvent.click(link);
    expect(onNavigate).toHaveBeenCalledExactlyOnceWith('explorer');
    link.addEventListener('click', (event) => event.preventDefault(), {
      once: true,
    });
    fireEvent.click(link, { ctrlKey: true });
    expect(onNavigate).toHaveBeenCalledTimes(1);
  });
});
