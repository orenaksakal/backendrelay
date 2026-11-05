import { describe, expect, it, vi } from 'vitest';
import {
  SEQUENCE_TIMEOUT_MS,
  createShortcutHandler,
  type ShortcutContext,
  type ShortcutKeyEvent,
  type ShortcutOutcome,
} from './shortcuts';

interface Harness {
  context: ShortcutContext;
  press: (key: string, options?: Partial<ShortcutKeyEvent>) => ShortcutOutcome;
  advance: (ms: number) => void;
}

function createHarness(): Harness {
  let clock = 0;
  const context: ShortcutContext = {
    navigate: vi.fn(),
    openPalette: vi.fn(),
    openShortcutsSheet: vi.fn(),
    toggleSidebar: vi.fn(),
    closeTopLayer: vi.fn(),
    requestPageAction: vi.fn(),
    appId: 'app-1',
    workspaceId: 'personal',
  };
  const handler = createShortcutHandler(
    () => context,
    undefined,
    () => clock,
  );

  return {
    context,
    press: (key, options = {}) => {
      const event: ShortcutKeyEvent = {
        key,
        metaKey: false,
        ctrlKey: false,
        altKey: false,
        shiftKey: false,
        isTrusted: true,
        target: document.body,
        preventDefault: vi.fn(),
        ...options,
      };
      return handler(event);
    },
    advance: (ms) => {
      clock += ms;
    },
  };
}

function element<K extends keyof HTMLElementTagNameMap>(
  tag: K,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  document.body.append(node);
  return node;
}

describe('shortcut ignore rules', () => {
  it('ignores keys typed into form controls and editors', () => {
    const { press, context } = createHarness();
    const input = element('input');
    const textarea = element('textarea');
    const select = element('select');
    const editable = element('div');
    editable.setAttribute('contenteditable', 'true');
    const editor = element('div');
    editor.className = 'cm-editor';
    const editorContent = element('div');
    editor.append(editorContent);

    try {
      for (const target of [input, textarea, select, editable, editorContent]) {
        expect(press('k', { ctrlKey: true, target })).toEqual({
          kind: 'ignored',
          reason: 'editable',
        });
        expect(press('?', { shiftKey: true, target }).kind).toBe('ignored');
      }
      expect(context.openPalette).not.toHaveBeenCalled();
      expect(context.openShortcutsSheet).not.toHaveBeenCalled();
    } finally {
      for (const node of [input, textarea, select, editable, editor]) {
        node.remove();
      }
    }
  });

  it('ignores IME composition and untrusted events', () => {
    const { press, context } = createHarness();

    expect(press('?', { shiftKey: true, isComposing: true })).toEqual({
      kind: 'ignored',
      reason: 'composing',
    });
    expect(press('k', { ctrlKey: true, isTrusted: false })).toEqual({
      kind: 'ignored',
      reason: 'untrusted',
    });
    expect(context.openShortcutsSheet).not.toHaveBeenCalled();
    expect(context.openPalette).not.toHaveBeenCalled();
  });

  it('does not run a shortcut an earlier handler already consumed', () => {
    const { press, context } = createHarness();

    expect(press('k', { ctrlKey: true, defaultPrevented: true })).toEqual({
      kind: 'ignored',
      reason: 'handled',
    });
    expect(context.openPalette).not.toHaveBeenCalled();
  });

  it('runs modifiers and symbols outside editable targets', () => {
    const { press, context } = createHarness();

    expect(press('k', { ctrlKey: true }).kind).toBe('run');
    expect(context.openPalette).toHaveBeenCalledTimes(1);

    expect(press('?', { shiftKey: true }).kind).toBe('run');
    expect(context.openShortcutsSheet).toHaveBeenCalledTimes(1);

    expect(press('[').kind).toBe('run');
    expect(context.toggleSidebar).toHaveBeenCalledTimes(1);
  });
});

describe('G sequences', () => {
  it('runs an app sequence within the one second window', () => {
    const { press, context, advance } = createHarness();

    expect(press('g')).toEqual({ kind: 'pending', steps: ['g'] });
    advance(SEQUENCE_TIMEOUT_MS - 1);
    expect(press('e').kind).toBe('run');
    expect(context.navigate).toHaveBeenCalledWith('/apps/app-1/explorer');
  });

  it('runs the global apps-home sequence', () => {
    const { press, context } = createHarness();

    expect(press('g').kind).toBe('pending');
    expect(press('h').kind).toBe('run');
    expect(context.navigate).toHaveBeenCalledWith('/w/personal');
  });

  it('starts a new sequence once a second has passed', () => {
    const { press, context, advance } = createHarness();

    expect(press('g').kind).toBe('pending');
    advance(SEQUENCE_TIMEOUT_MS);
    expect(press('h')).toEqual({ kind: 'none' });
    expect(context.navigate).not.toHaveBeenCalled();
  });

  it('swallows the next key when the prefix has no match', () => {
    const { press, context } = createHarness();

    expect(press('g').kind).toBe('pending');
    // `n` alone is "new item"; the pending `g` must consume it instead.
    expect(press('n')).toEqual({ kind: 'none' });
    expect(context.requestPageAction).not.toHaveBeenCalled();

    // The prefix is gone, so a following `h` is not "apps home" either.
    expect(press('h')).toEqual({ kind: 'none' });
    expect(context.navigate).not.toHaveBeenCalled();
  });

  it('ignores app sequences outside an app route', () => {
    const harness = createHarness();
    harness.context.appId = undefined;

    expect(harness.press('g').kind).toBe('pending');
    expect(harness.press('e')).toEqual({ kind: 'none' });
    expect(harness.context.navigate).not.toHaveBeenCalled();
  });
});
