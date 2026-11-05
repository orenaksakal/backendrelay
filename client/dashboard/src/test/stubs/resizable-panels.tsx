import type { ReactNode } from 'react';

/**
 * jsdom has no layout, so the real panel implementation treats every pointer
 * event as a separator drag and steals focus. The dashboard tests only need
 * the panes' children, not measured resizing.
 */
export function Group({ children }: { children?: ReactNode }) {
  return <div>{children}</div>;
}

export function Panel({ children }: { children?: ReactNode }) {
  return <div>{children}</div>;
}

export function Separator() {
  return <div />;
}
