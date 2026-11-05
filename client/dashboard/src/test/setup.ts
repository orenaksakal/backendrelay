import '@testing-library/jest-dom/vitest';
import { cleanup } from '@testing-library/react';
import { afterAll, afterEach, beforeAll } from 'vitest';
import { server } from './msw/node';

// jsdom has no matchMedia, and the theme provider follows the OS preference.
// A stable light default keeps tests deterministic.
Object.defineProperty(window, 'matchMedia', {
  writable: true,
  value: (query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addEventListener: () => undefined,
    removeEventListener: () => undefined,
    dispatchEvent: () => false,
  }),
});

// jsdom implements neither API. ResizeObserver is used by the resizable
// panels; scrollTo is called by route transitions.
class ResizeObserverStub implements ResizeObserver {
  constructor(private readonly callback: ResizeObserverCallback) {}

  observe(target: Element): void {
    const rect = target.getBoundingClientRect();
    const width = rect.width || 1024;
    const height = rect.height || 768;
    const entry = {
      target,
      contentRect: new DOMRect(0, 0, width, height),
      borderBoxSize: [{ blockSize: height, inlineSize: width }],
      contentBoxSize: [{ blockSize: height, inlineSize: width }],
      devicePixelContentBoxSize: [{ blockSize: height, inlineSize: width }],
    } as ResizeObserverEntry;
    this.callback([entry], this);
  }

  unobserve(): void {}

  disconnect(): void {}
}

Object.defineProperty(globalThis, 'ResizeObserver', {
  writable: true,
  value: ResizeObserverStub,
});

window.scrollTo = () => undefined;

beforeAll(() => {
  server.listen({ onUnhandledFrame: 'error' });
});

afterEach(() => {
  cleanup();
  server.resetHandlers();
});

afterAll(() => {
  server.close();
});
