import type { CompletionSource } from '@codemirror/autocomplete';
import { autocompletion } from '@codemirror/autocomplete';
import { json } from '@codemirror/lang-json';
import { HighlightStyle, syntaxHighlighting } from '@codemirror/language';
import { linter } from '@codemirror/lint';
import type { Extension } from '@codemirror/state';
import { EditorView } from '@codemirror/view';
import { tags } from '@lezer/highlight';
import CodeMirror, { type ReactCodeMirrorRef } from '@uiw/react-codemirror';
import {
  useCallback,
  useEffect,
  useImperativeHandle,
  useMemo,
  useRef,
  useState,
  type Ref,
} from 'react';
import { cn } from '@/lib/utils';
import type { JsonSchemaDocument } from './json-schema';
import {
  isJsonMode,
  lintDocument,
  toDiagnostics,
  type CodeEditorMode,
  type EditorIssue,
} from './lint';

export type { JsonSchemaDocument };

/** Imperative actions for callers that insert text, such as variable chips. */
export interface CodeEditorHandle {
  /** Inserts at the selection (or cursor) and focuses the editor. */
  insertText: (text: string) => void;
}

export interface CodeEditorProps {
  value: string;
  onChange?: (value: string) => void;
  mode: CodeEditorMode;
  /** Required: editors have no visible label of their own. */
  'aria-label': string;
  /** JSON Schema for `json-with-schema` (lint, completion and hover). */
  schema?: JsonSchemaDocument;
  /** Completion sources; D7.2 fills these in for `cel-in-json`. */
  completions?: CompletionSource | ReadonlyArray<CompletionSource>;
  /** Replaces the built-in JSON syntax check (for example CEL linting). */
  validate?: (value: string) => EditorIssue[];
  /** Called before the cursor jumps; useful for wiring error state. */
  onGoToError?: (issue: EditorIssue) => void;
  /** Exposes `insertText` without taking the component's own ref. */
  handleRef?: Ref<CodeEditorHandle>;
  readOnly?: boolean;
  height?: string;
  className?: string;
}

/** `Array.isArray` does not narrow readonly arrays, so spell out the union. */
function isCompletionList(
  value: CompletionSource | ReadonlyArray<CompletionSource>,
): value is ReadonlyArray<CompletionSource> {
  return Array.isArray(value);
}

/** Editor colours come from the same CSS variables as the rest of the UI. */
const editorTheme = EditorView.theme({
  '&': {
    backgroundColor: 'var(--card)',
    color: 'var(--foreground)',
    fontSize: '13px',
  },
  '&.cm-focused': { outline: '2px solid var(--ring)', outlineOffset: '-1px' },
  '.cm-content': {
    fontFamily:
      'var(--font-mono, "JetBrains Mono Variable", ui-monospace, monospace)',
    padding: '8px 0',
  },
  '.cm-gutters': {
    backgroundColor: 'var(--muted)',
    color: 'var(--muted-foreground)',
    border: 'none',
  },
  '.cm-activeLine': {
    backgroundColor: 'color-mix(in oklab, var(--muted) 60%, transparent)',
  },
  '.cm-activeLineGutter': {
    backgroundColor: 'color-mix(in oklab, var(--muted) 80%, transparent)',
  },
  '.cm-selectionBackground, &.cm-focused .cm-selectionBackground, ::selection':
    {
      backgroundColor: 'color-mix(in oklab, var(--accent) 24%, transparent)',
    },
  '.cm-cursor': { borderLeftColor: 'var(--foreground)' },
  '.cm-tooltip': {
    backgroundColor: 'var(--card)',
    border: '1px solid var(--border)',
    color: 'var(--foreground)',
  },
  '.cm-tooltip-autocomplete > ul > li[aria-selected]': {
    backgroundColor: 'color-mix(in oklab, var(--accent) 24%, transparent)',
    color: 'var(--foreground)',
  },
  '.cm-diagnostic': { fontFamily: 'var(--font-sans, inherit)' },
});

/**
 * Token colours use the same contrast-checked tokens as the rest of the UI, so
 * the editor stays WCAG AA in both themes. The default CodeMirror palette is
 * light-only and fails on the dark card.
 */
const editorHighlightStyle = HighlightStyle.define([
  {
    tag: [tags.comment, tags.meta, tags.processingInstruction],
    color: 'var(--muted-foreground)',
  },
  {
    tag: [
      tags.keyword,
      tags.controlKeyword,
      tags.definitionKeyword,
      tags.moduleKeyword,
      tags.operatorKeyword,
      tags.typeName,
      tags.className,
      tags.operator,
    ],
    color: 'var(--accent)',
  },
  {
    tag: [
      tags.string,
      tags.special(tags.string),
      tags.regexp,
      tags.escape,
      tags.inserted,
    ],
    color: 'var(--success)',
  },
  {
    tag: [tags.number, tags.bool, tags.null, tags.atom],
    color: 'var(--accent)',
  },
  {
    tag: [tags.propertyName, tags.attributeName, tags.variableName],
    color: 'var(--foreground)',
  },
  { tag: [tags.invalid, tags.deleted], color: 'var(--destructive)' },
]);

function buildExtensions(mode: CodeEditorMode, ariaLabel: string): Extension[] {
  const extensions: Extension[] = [];

  switch (mode) {
    case 'readonly-ts':
    case 'html':
      // Language support arrives in a lazy chunk; see the effect below.
      break;
    default:
      extensions.push(json());
      break;
  }

  extensions.push(EditorView.contentAttributes.of({ 'aria-label': ariaLabel }));
  return extensions;
}

/**
 * CodeMirror 6 wrapper (B.4). The editor is controlled: pass `value` and write
 * `onChange` back. Tab is never captured (`indentWithTab={false}`), so Esc then
 * Tab leaves the editor; the hint explains that on focus. JSON problems are
 * listed in the footer with a "Go to error" action.
 */
export function CodeEditor({
  value,
  onChange,
  mode,
  'aria-label': ariaLabel,
  schema,
  completions,
  validate,
  onGoToError,
  handleRef,
  readOnly,
  height = '320px',
  className,
}: CodeEditorProps) {
  const editorRef = useRef<ReactCodeMirrorRef>(null);
  const [focused, setFocused] = useState(false);

  const issues = useMemo(() => {
    const check = validate ?? ((text: string) => lintDocument(mode, text));
    return check(value);
  }, [validate, mode, value]);

  const sources = useMemo<ReadonlyArray<CompletionSource>>(() => {
    if (!completions) {
      return [];
    }
    return isCompletionList(completions) ? [...completions] : [completions];
  }, [completions]);

  // The TypeScript and JSON Schema languages are large. Load them after the
  // editor mounts so the shared editor chunk stays inside its budget (D9.3).
  const extensionKey =
    mode === 'readonly-ts'
      ? 'readonly-ts'
      : mode === 'json-with-schema' && schema !== undefined
        ? 'json-schema'
        : '';
  const [loadedExtensions, setLoadedExtensions] = useState<{
    key: string;
    extensions: Extension[];
  } | null>(null);

  useEffect(() => {
    if (extensionKey === '') {
      return;
    }
    let cancelled = false;
    const loading =
      extensionKey === 'readonly-ts'
        ? import('@codemirror/lang-javascript').then(({ javascript }) => [
            javascript({ typescript: true }),
          ])
        : schema === undefined
          ? Promise.resolve([])
          : import('./json-schema').then(({ jsonSchemaExtensions }) =>
              jsonSchemaExtensions(schema),
            );
    void loading.then((extensions) => {
      if (!cancelled) setLoadedExtensions({ key: extensionKey, extensions });
    });
    return () => {
      cancelled = true;
    };
  }, [extensionKey, schema]);

  const extensions = useMemo(() => {
    const list = [
      editorTheme,
      syntaxHighlighting(editorHighlightStyle),
      ...buildExtensions(mode, ariaLabel),
    ];
    if (loadedExtensions !== null && loadedExtensions.key === extensionKey) {
      list.push(...loadedExtensions.extensions);
    }
    if (sources.length > 0) {
      list.push(autocompletion({ override: [...sources] }));
    }
    if (isJsonMode(mode)) {
      list.push(linter((view) => toDiagnostics(issues, view.state.doc.length)));
    }
    return list;
  }, [mode, ariaLabel, extensionKey, loadedExtensions, sources, issues]);

  const handleChange = useCallback(
    (next: string) => {
      onChange?.(next);
    },
    [onChange],
  );

  const goToIssue = useCallback(
    (issue: EditorIssue) => {
      onGoToError?.(issue);
      const view = editorRef.current?.view;
      if (!view) {
        return;
      }
      view.dispatch({
        selection: { anchor: issue.from },
        scrollIntoView: true,
      });
      view.focus();
    },
    [onGoToError],
  );

  const insertText = useCallback(
    (text: string) => {
      const view = editorRef.current?.view;
      if (!view) {
        // The editor mounts after the first paint; append so the chip still works.
        onChange?.(`${value}${text}`);
        return;
      }
      const { from, to } = view.state.selection.main;
      view.dispatch({
        changes: { from, to, insert: text },
        selection: { anchor: from + text.length },
      });
      view.focus();
    },
    [onChange, value],
  );

  useImperativeHandle(handleRef, () => ({ insertText }), [insertText]);

  return (
    <div
      className={cn(
        'border-border bg-card overflow-hidden rounded-md border',
        className,
      )}
    >
      <CodeMirror
        ref={editorRef}
        value={value}
        height={height}
        theme="none"
        extensions={extensions}
        indentWithTab={false}
        readOnly={readOnly ?? mode === 'readonly-ts'}
        onChange={handleChange}
        onFocus={() => {
          setFocused(true);
        }}
        onBlur={() => {
          setFocused(false);
        }}
        basicSetup={{
          lineNumbers: true,
          foldGutter: false,
          highlightActiveLine: true,
          // The default palette is light-only; editorHighlightStyle replaces it.
          syntaxHighlighting: false,
          // Custom sources replace the defaults; otherwise keep JSON/Schema ones.
          autocompletion: sources.length === 0,
        }}
      />
      {focused ? (
        <p className="text-muted-foreground border-border border-t px-3 py-1.5 text-xs">
          Press Esc then Tab to leave the editor
        </p>
      ) : null}
      {issues.length > 0 ? (
        <div className="border-border border-t px-3 py-2">
          <p className="text-destructive text-xs font-medium">
            {issues.length === 1
              ? '1 problem'
              : `${String(issues.length)} problems`}
          </p>
          <ul className="mt-1 flex flex-col gap-1">
            {issues.map((issue, index) => (
              <li
                key={`${String(issue.from)}-${String(index)}`}
                className="flex items-start gap-2 text-xs"
              >
                <span className="text-destructive min-w-0 flex-1">
                  Line {issue.line}: {issue.message}
                </span>
                <button
                  type="button"
                  aria-label={`Go to error on line ${String(issue.line)}`}
                  className="text-accent inline-flex min-h-6 shrink-0 items-center hover:underline"
                  onClick={() => {
                    goToIssue(issue);
                  }}
                >
                  Go to error
                </button>
              </li>
            ))}
          </ul>
        </div>
      ) : null}
    </div>
  );
}
