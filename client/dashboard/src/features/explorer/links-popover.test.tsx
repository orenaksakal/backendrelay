import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';
import type { AppLink } from '@/lib/instant';
import { LinksPopover } from './links-popover';

const LINK: AppLink = {
  id: 'attr-owner',
  namespace: 'todos',
  label: 'owner',
  targetNamespace: '$users',
  reverseLabel: 'todos',
  cardinality: 'one',
  onDelete: undefined,
};

describe('links popover', () => {
  it('summarises the target count and lists each linked entity', async () => {
    const user = userEvent.setup();
    render(
      <LinksPopover
        appId="app-1"
        link={LINK}
        value={[
          { id: 'user-1', email: 'owner@example.com' },
          { id: 'user-2', name: 'Second user' },
        ]}
      />,
    );

    const trigger = screen.getByRole('button', { name: '2 links' });
    await user.click(trigger);

    expect(
      await screen.findByRole('button', { name: 'Copy ID user-1' }),
    ).toBeInTheDocument();
    expect(screen.getAllByText('owner@example.com').length).toBeGreaterThan(0);
    expect(screen.getAllByText('Second user').length).toBeGreaterThan(0);

    const open = screen.getByRole('link', {
      name: /owner@example\.com/,
    });
    expect(open).toHaveAttribute(
      'href',
      '/apps/app-1/explorer?ns=%24users&where=%7B%22id%22%3A%22user-1%22%7D',
    );
  });

  it('uses the singular label for one link', () => {
    render(
      <LinksPopover
        appId="app-1"
        link={LINK}
        value={{ id: 'user-1', email: 'owner@example.com' }}
      />,
    );
    expect(screen.getByRole('button', { name: '1 link' })).toBeInTheDocument();
  });

  it('renders nothing for an empty or malformed value', () => {
    const { container, rerender } = render(
      <LinksPopover appId="app-1" link={LINK} value={[]} />,
    );
    expect(container).toBeEmptyDOMElement();

    rerender(<LinksPopover appId="app-1" link={LINK} value={null} />);
    expect(container).toBeEmptyDOMElement();
  });
});
