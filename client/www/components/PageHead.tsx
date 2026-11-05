import {
  Children,
  Fragment,
  isValidElement,
  useEffect,
  type ReactElement,
  type ReactNode,
} from 'react';

type HeadState = {
  baseline: HTMLElement | null;
  stack: Array<{ owner: symbol; node: HTMLElement }>;
};
const states = new Map<string, HeadState>();

function elementsFrom(children: ReactNode): ReactElement[] {
  const elements: ReactElement[] = [];
  Children.forEach(children, (child) => {
    if (!isValidElement(child)) return;
    if (child.type === Fragment) {
      elements.push(
        ...elementsFrom((child.props as { children?: ReactNode }).children),
      );
    } else {
      elements.push(child);
    }
  });
  return elements;
}

function keyFor(element: ReactElement) {
  const props = element.props as Record<string, unknown>;
  if (element.type === 'title') return 'title';
  if (element.type === 'meta') {
    return `meta:${props.charSet ?? props.name ?? props.property ?? props.httpEquiv ?? JSON.stringify(props)}`;
  }
  if (element.type === 'style')
    return `style:${props.id ?? element.key ?? props.children}`;
  return `${String(element.type)}:${element.key ?? JSON.stringify(props)}`;
}

function existingNode(key: string) {
  return Array.from(document.head.children).find((node) => {
    const element = node as HTMLElement;
    if (element.dataset.pageHeadKey === key) return true;
    if (key === 'title') return node.tagName === 'TITLE';
    if (!key.startsWith('meta:') || node.tagName !== 'META') return false;
    const meta = node as HTMLMetaElement;
    const identifier =
      meta.getAttribute('charset') ??
      meta.getAttribute('name') ??
      meta.getAttribute('property') ??
      meta.getAttribute('http-equiv');
    return identifier != null && key === `meta:${identifier}`;
  }) as HTMLElement | undefined;
}

export default function PageHead({ children }: { children: ReactNode }) {
  useEffect(() => {
    const owner = Symbol('PageHead');
    const nodes: Array<{ key: string; node: HTMLElement }> = [];

    for (const child of elementsFrom(children)) {
      if (typeof child.type !== 'string') continue;
      const key = keyFor(child);
      let state = states.get(key);
      if (!state) {
        state = { baseline: existingNode(key) ?? null, stack: [] };
        states.set(key, state);
      }

      const active = state.stack.at(-1)?.node ?? state.baseline;
      active?.remove();
      const node = document.createElement(child.type);
      const props = child.props as Record<string, unknown>;
      for (const [name, value] of Object.entries(props)) {
        if (name === 'children' || value == null || typeof value === 'function')
          continue;
        const attribute = name === 'className' ? 'class' : name;
        if (typeof value === 'boolean') {
          if (value) node.setAttribute(attribute, '');
        } else {
          node.setAttribute(attribute, String(value));
        }
      }
      if (props.children != null) node.textContent = String(props.children);
      node.dataset.pageHeadKey = key;
      document.head.append(node);
      state.stack.push({ owner, node });
      nodes.push({ key, node });
    }

    return () => {
      for (const { key, node } of nodes) {
        const state = states.get(key);
        if (!state) continue;
        const index = state.stack.findIndex(
          (entry) => entry.owner === owner && entry.node === node,
        );
        if (index === -1) continue;
        const wasActive = index === state.stack.length - 1;
        state.stack.splice(index, 1);
        if (wasActive) {
          node.remove();
          const previous = state.stack.at(-1)?.node ?? state.baseline;
          if (previous) document.head.append(previous);
        }
        if (state.stack.length === 0) states.delete(key);
      }
    };
  }, [children]);

  return null;
}
