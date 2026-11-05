import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';
import { JsonTree } from './json-tree';

describe('JsonTree', () => {
  it('renders objects and arrays as expandable branches', async () => {
    const user = userEvent.setup();
    render(
      <JsonTree
        aria-label="Query result"
        value={{ todos: [{ id: 'todo-1', done: false }], count: 1 }}
      />,
    );

    const tree = screen.getByRole('group', { name: 'Query result' });
    const todos = within(tree).getByRole('button', { name: /todos/ });
    expect(todos).toHaveAttribute('aria-expanded', 'true');
    expect(within(tree).getByText('"todo-1"')).toBeInTheDocument();
    expect(within(tree).getByText('false')).toBeInTheDocument();

    await user.click(todos);
    expect(todos).toHaveAttribute('aria-expanded', 'false');
    expect(within(tree).queryByText('"todo-1"')).not.toBeInTheDocument();
  });

  it('shows null and empty containers without a toggle', () => {
    render(<JsonTree aria-label="Result" value={{ empty: {}, none: null }} />);

    const tree = screen.getByRole('group', { name: 'Result' });
    expect(within(tree).getByText('{}')).toBeInTheDocument();
    expect(within(tree).getByText('null')).toBeInTheDocument();
    expect(
      within(tree).queryByRole('button', { name: /empty/ }),
    ).not.toBeInTheDocument();
  });
});
