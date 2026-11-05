import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import type { AppNamespace } from '@/lib/instant';
import { NamespaceSidebar } from './namespace-sidebar';

const BASE = {
  attributes: [],
  links: [],
};

const TODOS: AppNamespace = { ...BASE, name: 'todos', isSystem: false };
const GOALS: AppNamespace = { ...BASE, name: 'goals', isSystem: false };
const USERS: AppNamespace = { ...BASE, name: '$users', isSystem: true };

function renderSidebar() {
  const onSelect = vi.fn();
  const onTogglePin = vi.fn();
  const onNewNamespace = vi.fn();
  render(
    <NamespaceSidebar
      namespaces={[TODOS, GOALS, USERS]}
      counts={{ todos: 1204, goals: 87, $users: 3 }}
      publicWrite={['todos']}
      selected="todos"
      pins={['goals']}
      isLoading={false}
      onSelect={onSelect}
      onTogglePin={onTogglePin}
      onNewNamespace={onNewNamespace}
    />,
  );
  return { onSelect, onTogglePin, onNewNamespace };
}

describe('namespace sidebar', () => {
  it('groups pinned, user and system namespaces with counts', () => {
    renderSidebar();
    const rail = screen.getByRole('complementary', { name: 'Namespaces' });

    expect(within(rail).getByText('Pinned')).toBeInTheDocument();
    expect(within(rail).getByText('Namespaces')).toBeInTheDocument();
    expect(
      within(rail).getByRole('button', { name: /^goals/ }),
    ).toBeInTheDocument();
    expect(
      within(rail).getByRole('button', { name: /^todos/ }),
    ).toBeInTheDocument();
    expect(within(rail).getByText('1,204')).toBeInTheDocument();
    expect(within(rail).getByText('87')).toBeInTheDocument();
    expect(
      within(rail).queryByRole('button', { name: /\$users/ }),
    ).not.toBeInTheDocument();
  });

  it('marks namespaces whose rules allow everyone to write', () => {
    renderSidebar();
    const rail = screen.getByRole('complementary', { name: 'Namespaces' });
    expect(
      within(rail).getByRole('img', {
        name: 'Anyone can write to this namespace',
      }),
    ).toBeInTheDocument();
  });

  it('filters by name and reveals the system group while searching', async () => {
    const user = userEvent.setup();
    renderSidebar();
    const rail = screen.getByRole('complementary', { name: 'Namespaces' });

    await user.type(
      within(rail).getByRole('searchbox', { name: 'Find namespace' }),
      'us',
    );

    await waitFor(() => {
      expect(
        within(rail).queryByRole('button', { name: /^todos/ }),
      ).not.toBeInTheDocument();
    });
    expect(
      within(rail).getByRole('button', { name: /^\$users/ }),
    ).toBeInTheDocument();
  });

  it('expands the system group from its header', async () => {
    const user = userEvent.setup();
    renderSidebar();
    const rail = screen.getByRole('complementary', { name: 'Namespaces' });

    await user.click(within(rail).getByRole('button', { name: 'System' }));
    expect(
      within(rail).getByRole('button', { name: /^\$users/ }),
    ).toBeInTheDocument();
  });

  it('moves focus with arrow keys and selects with Enter', async () => {
    const user = userEvent.setup();
    const { onSelect } = renderSidebar();
    const rail = screen.getByRole('complementary', { name: 'Namespaces' });
    const pinned = within(rail).getByRole('button', { name: /^goals/ });
    const todos = within(rail).getByRole('button', { name: /^todos/ });

    pinned.focus();
    await user.keyboard('{ArrowDown}');
    expect(todos).toHaveFocus();

    await user.keyboard('{Enter}');
    expect(onSelect).toHaveBeenCalledWith('todos');

    await user.keyboard('{ArrowUp}');
    expect(pinned).toHaveFocus();

    await user.keyboard('{End}');
    expect(todos).toHaveFocus();
    await user.keyboard('{Home}');
    expect(pinned).toHaveFocus();
  });

  it('pins without changing the selected namespace', async () => {
    const user = userEvent.setup();
    const { onTogglePin, onSelect } = renderSidebar();
    const rail = screen.getByRole('complementary', { name: 'Namespaces' });

    await user.click(within(rail).getByRole('button', { name: 'Pin todos' }));
    expect(onTogglePin).toHaveBeenCalledWith('todos');
    expect(onSelect).not.toHaveBeenCalled();
    expect(
      within(rail).getByRole('button', { name: 'Unpin goals' }),
    ).toBeInTheDocument();
  });

  it('announces that no namespace matches', async () => {
    const user = userEvent.setup();
    renderSidebar();
    const rail = screen.getByRole('complementary', { name: 'Namespaces' });

    await user.type(
      within(rail).getByRole('searchbox', { name: 'Find namespace' }),
      'zzz',
    );
    expect(
      await within(rail).findByText(/No namespaces match/),
    ).toBeInTheDocument();
  });

  it('opens the new-namespace dialog from the bottom of the rail', async () => {
    const user = userEvent.setup();
    const { onNewNamespace } = renderSidebar();
    const rail = screen.getByRole('complementary', { name: 'Namespaces' });

    await user.click(
      within(rail).getByRole('button', { name: 'New namespace' }),
    );
    expect(onNewNamespace).toHaveBeenCalledTimes(1);
  });
});
