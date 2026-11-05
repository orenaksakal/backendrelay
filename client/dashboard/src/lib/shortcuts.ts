import { useEffect, useRef } from 'react';
import { appHref, appSectionLabel } from '@/lib/navigation';

/**
 * Every keyboard shortcut in one registry (B.7): the `?` sheet, the tooltips
 * and the key handler all read this array, so they cannot drift apart.
 *
 * `keys` is the canonical binding, not display text: steps are separated by
 * spaces (`g h`) and modifiers joined with `+` (`mod+k`). `shortcutCaps()`
 * formats it for the UI.
 */

export type ShortcutScope = 'global' | 'app' | 'page' | 'form';

export type PageActionId = 'search' | 'new' | 'refresh' | 'save' | 'run';

/**
 * Screens listen for this event to answer a page-scoped shortcut (B.7) and
 * call `preventDefault()` when they handled it; `/` then falls back to the
 * command palette on pages without a search field.
 */
export const PAGE_ACTION_EVENT = 'br:page-action';

export interface ShortcutContext {
  navigate: (href: string) => void;
  openPalette: () => void;
  openShortcutsSheet: () => void;
  toggleSidebar: () => void;
  closeTopLayer: () => void;
  /** Hands a page action to the mounted screen; a no-op when none listens. */
  requestPageAction: (action: PageActionId) => void;
  /** Present only on app routes; app-scoped shortcuts are inactive without it. */
  appId?: string;
  /** Used by "Apps home"; absent outside a workspace route. */
  workspaceId?: string;
}

export interface Shortcut {
  id: string;
  /** Canonical binding, for example `g e` or `mod+enter`. */
  keys: string;
  label: string;
  scope: ShortcutScope;
  run: (context: ShortcutContext) => void;
}

/** `G`-sequences for the app sections, in the B.7 order. */
const APP_SECTION_KEYS: Record<string, string> = {
  overview: 'g o',
  explorer: 'g e',
  schema: 'g s',
  users: 'g u',
  storage: 'g f',
  permissions: 'g p',
  auth: 'g a',
  playground: 'g q',
  webhooks: 'g w',
  backups: 'g b',
  settings: 'g ,',
};

/** The `G` hint for a sidebar item, or `undefined` when it has no shortcut. */
export function appSectionShortcutKeys(sectionId: string): string | undefined {
  return APP_SECTION_KEYS[sectionId];
}

function pageAction(action: PageActionId): (context: ShortcutContext) => void {
  return (context) => {
    context.requestPageAction(action);
  };
}

export const SHORTCUTS: readonly Shortcut[] = [
  {
    id: 'command-palette',
    keys: 'mod+k',
    label: 'Command palette',
    scope: 'global',
    run: (context) => {
      context.openPalette();
    },
  },
  {
    id: 'keyboard-shortcuts',
    keys: '?',
    label: 'Keyboard shortcuts',
    scope: 'global',
    run: (context) => {
      context.openShortcutsSheet();
    },
  },
  {
    id: 'apps-home',
    keys: 'g h',
    label: 'Apps home',
    scope: 'global',
    run: (context) => {
      if (context.workspaceId === undefined) return;
      context.navigate(`/w/${encodeURIComponent(context.workspaceId)}`);
    },
  },
  {
    id: 'toggle-sidebar',
    keys: '[',
    label: 'Toggle sidebar',
    scope: 'global',
    run: (context) => {
      context.toggleSidebar();
    },
  },
  {
    id: 'close-layer',
    keys: 'escape',
    label: 'Close the topmost layer',
    scope: 'global',
    run: (context) => {
      context.closeTopLayer();
    },
  },
  ...Object.entries(APP_SECTION_KEYS).map(
    ([section, keys]): Shortcut => ({
      id: `go-${section}`,
      keys,
      label: `Go to ${appSectionLabel(section) ?? section}`,
      scope: 'app',
      run: (context) => {
        if (context.appId === undefined) return;
        context.navigate(appHref(context.appId, section));
      },
    }),
  ),
  {
    id: 'page-search',
    keys: '/',
    label: 'Focus the page search',
    scope: 'page',
    run: pageAction('search'),
  },
  {
    id: 'page-new',
    keys: 'n',
    label: 'New item on this page',
    scope: 'page',
    run: pageAction('new'),
  },
  {
    id: 'page-refresh',
    keys: 'shift+r',
    label: 'Refresh the current list',
    scope: 'page',
    run: pageAction('refresh'),
  },
  {
    id: 'form-save',
    keys: 'mod+s',
    label: 'Save the open sheet or form',
    scope: 'form',
    run: pageAction('save'),
  },
  {
    id: 'form-run',
    keys: 'mod+enter',
    label: 'Run, or save and close',
    scope: 'form',
    run: pageAction('run'),
  },
];

/**
 * How long the next key of a sequence may take (B.7). A second key that arrives
 * later starts a new sequence instead of completing the prefix.
 */
export const SEQUENCE_TIMEOUT_MS = 1000;

/** The subset of `KeyboardEvent` matching and the ignore rules need. */
export interface ShortcutKeyEvent {
  key: string;
  metaKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
  isComposing?: boolean;
  isTrusted: boolean;
  target: EventTarget | null;
  defaultPrevented?: boolean;
  preventDefault: () => void;
}

export type IgnoreReason = 'untrusted' | 'composing' | 'editable' | 'handled';

export type ShortcutOutcome =
  | { kind: 'ignored'; reason: IgnoreReason }
  | { kind: 'none' }
  | { kind: 'pending'; steps: readonly string[] }
  | { kind: 'run'; shortcut: Shortcut };

const EDITABLE_SELECTOR =
  'input, textarea, select, [contenteditable]:not([contenteditable="false"]), .cm-editor';

/**
 * Shortcuts stay quiet while the user types: in form controls, contenteditable
 * regions and CodeMirror. IME composition and untrusted events never trigger a
 * shortcut either (B.7).
 */
export function shortcutIgnoreReason(
  event: ShortcutKeyEvent,
): IgnoreReason | undefined {
  if (event.defaultPrevented === true) return 'handled';
  if (!event.isTrusted) return 'untrusted';
  if (event.isComposing === true) return 'composing';
  if (isEditableTarget(event.target)) return 'editable';
  return undefined;
}

export function isEditableTarget(target: EventTarget | null): boolean {
  if (!(target instanceof Element)) return false;
  return target.closest(EDITABLE_SELECTOR) !== null;
}

/**
 * The canonical step for an event. Shift is dropped when it only produced a
 * symbol, so `?` matches the `?` binding instead of `shift+?`.
 */
export function describeKeyEvent(event: ShortcutKeyEvent): string {
  const key = event.key.toLowerCase();
  const parts: string[] = [];
  if (event.metaKey || event.ctrlKey) parts.push('mod');
  if (event.altKey) parts.push('alt');
  const shiftedSymbol = key.length === 1 && !/[a-z0-9]/.test(key);
  if (event.shiftKey && !shiftedSymbol) parts.push('shift');
  parts.push(key === ' ' ? 'space' : key);
  return parts.join('+');
}

interface PendingSequence {
  steps: string[];
  at: number;
}

function bindingSteps(keys: string): string[] {
  return keys.split(' ').filter((step) => step !== '');
}

function sameSteps(left: readonly string[], right: readonly string[]): boolean {
  return (
    left.length === right.length &&
    left.every((step, index) => step === right[index])
  );
}

function isPrefix(
  prefix: readonly string[],
  steps: readonly string[],
): boolean {
  return (
    prefix.length > steps.length &&
    steps.every((step, index) => step === prefix[index])
  );
}

function matchOutcome(
  shortcuts: readonly Shortcut[],
  pending: PendingSequence | null,
  descriptor: string,
  at: number,
): ShortcutOutcome {
  const active =
    pending !== null && at - pending.at < SEQUENCE_TIMEOUT_MS
      ? pending.steps
      : [];
  const steps = [...active, descriptor];

  const runnable = shortcuts.find((shortcut) =>
    sameSteps(bindingSteps(shortcut.keys), steps),
  );
  if (runnable !== undefined) return { kind: 'run', shortcut: runnable };

  const continues = shortcuts.some((shortcut) =>
    isPrefix(bindingSteps(shortcut.keys), steps),
  );
  if (continues) return { kind: 'pending', steps };

  return { kind: 'none' };
}

/**
 * Stateful key handler: resolves single keys and `G`-sequences with the 1s
 * timeout. `getContext` is read per event so navigation stays current, and
 * `now` is injectable so the timeout is testable.
 */
export function createShortcutHandler(
  getContext: () => ShortcutContext,
  shortcuts: readonly Shortcut[] = SHORTCUTS,
  now: () => number = () => Date.now(),
): (event: ShortcutKeyEvent) => ShortcutOutcome {
  let pending: PendingSequence | null = null;

  return (event) => {
    const reason = shortcutIgnoreReason(event);
    if (reason !== undefined) {
      pending = null;
      return { kind: 'ignored', reason };
    }

    const context = getContext();
    const available = shortcuts.filter(
      (shortcut) => shortcut.scope !== 'app' || context.appId !== undefined,
    );
    const at = now();
    const outcome = matchOutcome(
      available,
      pending,
      describeKeyEvent(event),
      at,
    );

    if (outcome.kind === 'run') {
      pending = null;
      event.preventDefault();
      outcome.shortcut.run(context);
    } else if (outcome.kind === 'pending') {
      pending = { steps: [...outcome.steps], at };
    } else {
      pending = null;
    }

    return outcome;
  };
}

/** Mounts the global key handler; the context is read through a ref. */
export function useGlobalShortcuts(
  context: ShortcutContext,
  shortcuts: readonly Shortcut[] = SHORTCUTS,
): void {
  const contextRef = useRef(context);

  useEffect(() => {
    contextRef.current = context;
  }, [context]);

  useEffect(() => {
    const handler = createShortcutHandler(() => contextRef.current, shortcuts);
    const listener = (event: KeyboardEvent) => {
      handler(event);
    };
    window.addEventListener('keydown', listener);
    return () => {
      window.removeEventListener('keydown', listener);
    };
  }, [shortcuts]);
}

const KEY_LABELS: Record<string, string> = {
  enter: '↵',
  escape: 'Esc',
  space: 'Space',
  arrowup: '↑',
  arrowdown: '↓',
  arrowleft: '←',
  arrowright: '→',
  backspace: '⌫',
  delete: 'Del',
  tab: 'Tab',
};

function isApplePlatform(): boolean {
  if (typeof navigator === 'undefined') return false;
  return /mac|iphone|ipad|ipod/i.test(
    `${navigator.platform} ${navigator.userAgent}`,
  );
}

function keyLabel(key: string): string {
  const label = KEY_LABELS[key];
  if (label !== undefined) return label;
  if (key.length === 1) return key.toUpperCase();
  return key.charAt(0).toUpperCase() + key.slice(1);
}

/**
 * Display caps for a binding: `mod+k` → `⌘K` (or `Ctrl+K`), `g e` → `G`, `E`,
 * `mod+enter` → `⌘↵`.
 */
export function shortcutCaps(keys: string): string[] {
  return bindingSteps(keys).map((step) => {
    const parts = step.split('+');
    const key = parts.pop() ?? '';
    let prefix = '';
    for (const modifier of parts) {
      if (modifier === 'mod') prefix += isApplePlatform() ? '⌘' : 'Ctrl+';
      else if (modifier === 'shift') prefix += '⇧';
      else if (modifier === 'alt') prefix += isApplePlatform() ? '⌥' : 'Alt+';
    }
    return `${prefix}${keyLabel(key)}`;
  });
}

/** One-line form for tooltips and the palette: `G E`, `Ctrl+K`. */
export function formatShortcut(keys: string): string {
  return shortcutCaps(keys).join(' ');
}

export interface ShortcutGroup {
  scope: ShortcutScope;
  label: string;
  shortcuts: Shortcut[];
}

const SCOPE_ORDER: readonly ShortcutScope[] = ['global', 'app', 'page', 'form'];

const SCOPE_LABELS: Record<ShortcutScope, string> = {
  global: 'Anywhere',
  app: 'In an app',
  page: 'On a page',
  form: 'In a sheet or form',
};

/** Groups the registry for the `?` sheet, in the B.7 order. */
export function groupShortcuts(
  shortcuts: readonly Shortcut[] = SHORTCUTS,
): ShortcutGroup[] {
  return SCOPE_ORDER.map((scope) => ({
    scope,
    label: SCOPE_LABELS[scope],
    shortcuts: shortcuts.filter((shortcut) => shortcut.scope === scope),
  })).filter((group) => group.shortcuts.length > 0);
}
