import type { Diagnostic } from '@codemirror/lint';

export type CodeEditorMode =
  | 'json'
  | 'json-with-schema'
  | 'instaql'
  | 'cel-in-json'
  | 'readonly-ts'
  | 'html';

const JSON_MODES: ReadonlySet<CodeEditorMode> = new Set<CodeEditorMode>([
  'json',
  'json-with-schema',
  'instaql',
  'cel-in-json',
]);

/** Modes that parse their document as JSON (CEL lives inside JSON strings). */
export function isJsonMode(mode: CodeEditorMode): boolean {
  return JSON_MODES.has(mode);
}

export interface EditorIssue {
  /** Character offsets for the CodeMirror diagnostic and cursor jump. */
  from: number;
  to: number;
  /** 1-based position for the footer. */
  line: number;
  column: number;
  message: string;
  severity: 'error' | 'warning';
}

/** 1-based line and column for a character offset. */
export function lineColumnAt(
  text: string,
  offset: number,
): { line: number; column: number } {
  const clamped = Math.min(Math.max(offset, 0), text.length);
  const before = text.slice(0, clamped);
  const line = before.split('\n').length;
  const lastBreak = before.lastIndexOf('\n');
  return { line, column: clamped - lastBreak };
}

function errorOffset(message: string, text: string): number {
  const raw = /position (\d+)/.exec(message)?.[1];
  if (!raw) {
    return 0;
  }
  const offset = Number.parseInt(raw, 10);
  return Number.isFinite(offset) ? Math.min(offset, text.length) : 0;
}

/**
 * Client-side JSON syntax check. The server stays authoritative for rules and
 * schemas; this only keeps the footer useful while typing.
 */
export function lintDocument(
  mode: CodeEditorMode,
  text: string,
): EditorIssue[] {
  if (!isJsonMode(mode) || text.trim() === '') {
    return [];
  }

  try {
    JSON.parse(text);
    return [];
  } catch (error) {
    const message =
      error instanceof Error ? error.message : 'This is not valid JSON.';
    const from = errorOffset(message, text);
    const { line, column } = lineColumnAt(text, from);
    return [
      {
        from,
        to: Math.min(from + 1, text.length),
        line,
        column,
        message,
        severity: 'error',
      },
    ];
  }
}

/** Map issues to CodeMirror diagnostics, clamped to the current document. */
export function toDiagnostics(
  issues: ReadonlyArray<EditorIssue>,
  documentLength: number,
): Diagnostic[] {
  if (documentLength === 0) {
    return [];
  }

  return issues.map((issue) => {
    const from = Math.min(Math.max(issue.from, 0), documentLength);
    const to = Math.min(Math.max(issue.to, from), documentLength);
    return {
      from,
      to,
      severity: issue.severity,
      message: issue.message,
    };
  });
}
