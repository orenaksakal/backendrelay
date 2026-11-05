import type {
  Completion,
  CompletionContext,
  CompletionResult,
  CompletionSource,
} from '@codemirror/autocomplete';
import { lineColumnAt, type EditorIssue } from '@/components/code-editor/lint';
import type { PermissionAction } from './effective';

/**
 * C.10/D7.2 CEL support for the permission editor.
 *
 * Completion only offers identifiers that the server accepts in this context:
 * this namespace's attributes and links, `newData` on create/update,
 * `linkedData` on link/unlink, plus the auth, request and bind roots. The
 * syntax check is deliberately smaller than CEL: it catches the mistakes that
 * are visible before the authoritative server validation (D0.5) returns.
 */

export interface CelReference {
  /** Link label on the current namespace. */
  label: string;
  /** Attribute names on the link target, used for `data.ref('label.attr')`. */
  attributes: readonly string[];
}

export interface CelCompletionContext {
  attributes?: readonly string[];
  references?: readonly CelReference[];
  binds?: readonly string[];
  /** `create`/`update` enable `newData`; link/unlink enable `linkedData`. */
  action?: PermissionAction | 'link' | 'unlink';
}

const IDENTIFIER = /^[A-Za-z_][A-Za-z0-9_]*$/;

function option(
  label: string,
  type: Completion['type'] = 'variable',
): Completion {
  return { label, type };
}

function addAttributeOptions(
  target: Completion[],
  root: string,
  attributes: readonly string[] | undefined,
): void {
  for (const attribute of attributes ?? []) {
    if (IDENTIFIER.test(attribute)) target.push(option(`${root}.${attribute}`));
  }
}

export function celOptions(context: CelCompletionContext): Completion[] {
  const options: Completion[] = [
    option('auth'),
    option('auth.id'),
    option('auth.email'),
    option('data'),
    ...(context.action === 'create' || context.action === 'update'
      ? [option('newData')]
      : []),
    ...(context.action === 'link' || context.action === 'unlink'
      ? [option('linkedData')]
      : []),
    option('ruleParams'),
    option('request.time', 'property'),
    option('request.ip', 'property'),
    option('request.origin', 'property'),
    option('request.modifiedFields', 'property'),
  ];

  addAttributeOptions(options, 'data', context.attributes);
  if (context.action === 'create' || context.action === 'update') {
    addAttributeOptions(options, 'newData', context.attributes);
  }
  for (const reference of context.references ?? []) {
    if (!IDENTIFIER.test(reference.label)) continue;
    for (const attribute of reference.attributes) {
      if (IDENTIFIER.test(attribute)) {
        options.push(
          option(`data.ref('${reference.label}.${attribute}')`, 'property'),
        );
      }
    }
  }
  for (const bind of context.binds ?? []) {
    if (IDENTIFIER.test(bind)) options.push(option(bind));
  }

  const seen = new Set<string>();
  return options.filter((candidate) => {
    if (seen.has(candidate.label)) return false;
    seen.add(candidate.label);
    return true;
  });
}

/** A CodeMirror source; scope it to one rule expression's context. */
export function celCompletionSource(
  context: CelCompletionContext,
): CompletionSource {
  const options = celOptions(context);
  return (source: CompletionContext): CompletionResult | null => {
    const word = source.matchBefore(/[\w$.'"(]+$/u);
    if (word === null) {
      if (!source.explicit) return null;
      return { from: source.pos, options, validFor: /^[\w$.'"(]*$/u };
    }
    const prefix = word.text;
    return {
      from: word.from,
      options: options.filter((candidate) =>
        candidate.label.toLowerCase().startsWith(prefix.toLowerCase()),
      ),
      validFor: /^[\w$.'"(]*$/u,
    };
  };
}

interface Token {
  kind:
    | 'ident'
    | 'number'
    | 'string'
    | 'op'
    | 'open'
    | 'close'
    | 'dot'
    | 'comma'
    | 'colon';
  value: string;
  from: number;
}

const BINARY_OPERATORS = new Set([
  '&&',
  '||',
  '==',
  '!=',
  '<',
  '>',
  '<=',
  '>=',
  '+',
  '-',
  '*',
  '/',
  '%',
  'in',
  '?',
]);

function tokenize(text: string): { tokens: Token[]; issues: EditorIssue[] } {
  const tokens: Token[] = [];
  const issues: EditorIssue[] = [];
  const stack: Array<{ char: string; from: number }> = [];
  let index = 0;

  const issue = (from: number, to: number, message: string): void => {
    const { line, column } = lineColumnAt(text, from);
    issues.push({ from, to, line, column, message, severity: 'error' });
  };

  while (index < text.length) {
    const char = text[index] ?? '';
    const next = text[index + 1] ?? '';

    if (/\s/u.test(char)) {
      index += 1;
      continue;
    }
    if (char === '/' && next === '/') {
      while (index < text.length && text[index] !== '\n') index += 1;
      continue;
    }
    if (char === "'" || char === '"') {
      const quote = char;
      const start = index;
      index += 1;
      let closed = false;
      while (index < text.length) {
        const current = text[index];
        if (current === '\\') {
          index += 2;
          continue;
        }
        if (current === quote) {
          closed = true;
          index += 1;
          break;
        }
        index += 1;
      }
      if (!closed) {
        issue(start, text.length, 'Unclosed string.');
        return { tokens, issues };
      }
      tokens.push({
        kind: 'string',
        value: text.slice(start, index),
        from: start,
      });
      continue;
    }
    if (/[A-Za-z_$]/u.test(char)) {
      const start = index;
      while (index < text.length && /[\w$]/u.test(text[index] ?? '')) {
        index += 1;
      }
      tokens.push({
        kind: 'ident',
        value: text.slice(start, index),
        from: start,
      });
      continue;
    }
    if (/[0-9]/u.test(char)) {
      const start = index;
      while (index < text.length && /[0-9.eE+-]/u.test(text[index] ?? '')) {
        // Stop before a binary sign that is not part of an exponent.
        if (
          ['+', '-'].includes(text[index] ?? '') &&
          !['e', 'E'].includes(text[index - 1] ?? '')
        ) {
          break;
        }
        index += 1;
      }
      tokens.push({
        kind: 'number',
        value: text.slice(start, index),
        from: start,
      });
      continue;
    }
    if (char === '(' || char === '[' || char === '{') {
      stack.push({ char, from: index });
      tokens.push({ kind: 'open', value: char, from: index });
      index += 1;
      continue;
    }
    if (char === ')' || char === ']' || char === '}') {
      const expected = char === ')' ? '(' : char === ']' ? '[' : '{';
      const top = stack.pop();
      if (top === undefined || top.char !== expected) {
        issue(index, index + 1, `Unexpected "${char}".`);
        return { tokens, issues };
      }
      tokens.push({ kind: 'close', value: char, from: index });
      index += 1;
      continue;
    }
    if (char === '.') {
      tokens.push({ kind: 'dot', value: '.', from: index });
      index += 1;
      continue;
    }
    if (char === ',') {
      tokens.push({ kind: 'comma', value: ',', from: index });
      index += 1;
      continue;
    }
    if (char === ':') {
      tokens.push({ kind: 'colon', value: ':', from: index });
      index += 1;
      continue;
    }
    if (char === '=' && next === '=' && text[index + 2] !== '=') {
      tokens.push({ kind: 'op', value: '==', from: index });
      index += 2;
      continue;
    }
    if (char === '=' && next === '=' && text[index + 2] === '=') {
      issue(index, index + 3, 'CEL uses "==", not "===".');
      index += 3;
      continue;
    }
    if (char === '=' && next !== '=') {
      issue(index, index + 1, 'CEL uses "==" to compare values.');
      index += 1;
      continue;
    }
    if (char === '!' && next === '=') {
      tokens.push({ kind: 'op', value: '!=', from: index });
      index += 2;
      continue;
    }
    if (char === '&' && next === '&') {
      tokens.push({ kind: 'op', value: '&&', from: index });
      index += 2;
      continue;
    }
    if (char === '|' && next === '|') {
      tokens.push({ kind: 'op', value: '||', from: index });
      index += 2;
      continue;
    }
    if ((char === '<' && next === '=') || (char === '>' && next === '=')) {
      tokens.push({ kind: 'op', value: `${char}=`, from: index });
      index += 2;
      continue;
    }
    if ('<>+-*/%!?'.includes(char)) {
      tokens.push({ kind: 'op', value: char, from: index });
      index += 1;
      continue;
    }
    issue(index, index + 1, `Unexpected character "${char}".`);
    index += 1;
  }

  for (const opening of stack.reverse()) {
    issue(opening.from, opening.from + 1, `Unclosed "${opening.char}".`);
  }
  return { tokens, issues };
}

/** A fast CEL syntax check; the server remains authoritative (D0.5). */
export function lintCelExpression(expression: string): EditorIssue[] {
  if (expression.trim() === '') return [];
  const { tokens, issues } = tokenize(expression);
  if (issues.length > 0) return issues;

  const last = tokens[tokens.length - 1];
  if (last !== undefined) {
    const trailing =
      (last.kind === 'op' && BINARY_OPERATORS.has(last.value)) ||
      (last.kind === 'ident' && last.value === 'in') ||
      last.kind === 'colon' ||
      last.kind === 'comma' ||
      last.kind === 'open';
    if (trailing) {
      const { line, column } = lineColumnAt(expression, last.from);
      issues.push({
        from: last.from,
        to: last.from + Math.max(last.value.length, 1),
        line,
        column,
        message: `Expected an operand after "${last.value}".`,
        severity: 'error',
      });
    } else if (last.kind === 'dot') {
      const { line, column } = lineColumnAt(expression, last.from);
      issues.push({
        from: last.from,
        to: last.from + 1,
        line,
        column,
        message: 'Expected a field after ".".',
        severity: 'error',
      });
    }
  }
  return issues;
}
