import type {
  Completion,
  CompletionContext,
  CompletionResult,
  CompletionSource,
} from '@codemirror/autocomplete';
import type { AppNamespace } from '@/lib/instant';

/**
 * C.12 InstaQL completion: the top level offers the app's namespaces; inside a
 * namespace it offers its attributes and links, and inside that namespace's
 * `$` object the filter keys. Parsing is deliberately tiny: a brace-stack walk
 * that skips strings is enough for completion, and the server stays
 * authoritative for the query itself.
 */

export interface InstaqlNamespace {
  name: string;
  attributes: readonly string[];
  links: readonly string[];
}

const FILTER_KEYS: ReadonlyArray<{ label: string; detail: string }> = [
  { label: 'where', detail: 'Filter clause' },
  { label: 'order', detail: 'Sort order' },
  { label: 'limit', detail: 'Maximum rows' },
  { label: 'fields', detail: 'Returned attributes' },
  { label: 'aggregate', detail: 'Aggregate values' },
];

function option(
  label: string,
  type: Completion['type'],
  detail?: string,
): Completion {
  return detail === undefined ? { label, type } : { label, type, detail };
}

export function instaqlNamespaces(
  namespaces: readonly AppNamespace[],
): InstaqlNamespace[] {
  return namespaces.map((namespace) => ({
    name: namespace.name,
    attributes: namespace.attributes.map((attribute) => attribute.name),
    links: namespace.links.map((link) => link.label),
  }));
}

/** The `"key":` immediately before an opening brace, or `null` at the root. */
function keyBefore(text: string, braceIndex: number): string | null {
  let index = braceIndex - 1;
  while (index >= 0 && /\s/u.test(text[index] ?? '')) index -= 1;
  if (text[index] !== ':') return null;
  index -= 1;
  while (index >= 0 && /\s/u.test(text[index] ?? '')) index -= 1;
  if (text[index] !== '"') return null;
  const close = index;
  index -= 1;
  while (index >= 0) {
    const char = text[index];
    if (char === '"' && text[index - 1] !== '\\') {
      return text.slice(index + 1, close);
    }
    index -= 1;
  }
  return null;
}

/**
 * The chain of object keys around `pos`; the root object contributes `null`.
 * For `{"todos": {"title": "…` with the cursor inside `todos`, the result is
 * `[null, "todos"]`.
 */
export function contextPathAt(text: string, pos: number): Array<string | null> {
  const stack: Array<string | null> = [];
  const end = Math.min(Math.max(pos, 0), text.length);
  let inString = false;
  let index = 0;

  while (index < end) {
    const char = text[index];
    if (inString) {
      if (char === '\\') {
        index += 2;
        continue;
      }
      if (char === '"') inString = false;
      index += 1;
      continue;
    }
    if (char === '"') {
      inString = true;
      index += 1;
      continue;
    }
    if (char === '{') {
      stack.push(keyBefore(text, index));
      index += 1;
      continue;
    }
    if (char === '}') {
      stack.pop();
      index += 1;
      continue;
    }
    index += 1;
  }

  return stack;
}

export function instaqlOptionsAt(
  namespaces: readonly InstaqlNamespace[],
  path: ReadonlyArray<string | null>,
): Completion[] {
  const current = path[path.length - 1] ?? null;
  const parent = path[path.length - 2] ?? null;

  const namespace = namespaces.find((candidate) => candidate.name === current);
  if (namespace !== undefined) {
    const labels = new Set<string>(['$']);
    for (const attribute of namespace.attributes) labels.add(attribute);
    for (const link of namespace.links) labels.add(link);
    return [...labels].map((label) =>
      option(
        label,
        label === '$' ? 'property' : 'variable',
        label === '$' ? 'Query options' : undefined,
      ),
    );
  }

  if (current === '$' && parent !== null) {
    return FILTER_KEYS.map((key) => option(key.label, 'property', key.detail));
  }

  return namespaces.map((candidate) =>
    option(candidate.name, 'property', 'Namespace'),
  );
}

export function instaqlCompletionSource(
  namespaces: readonly AppNamespace[],
): CompletionSource {
  const schema = instaqlNamespaces(namespaces);
  return (context: CompletionContext): CompletionResult | null => {
    const text = context.state.doc.toString();
    const match = context.matchBefore(/[\w$]+/u);
    if (match === null && !context.explicit) return null;
    const prefix = match?.text.toLowerCase() ?? '';
    const options = instaqlOptionsAt(
      schema,
      contextPathAt(text, context.pos),
    ).filter((candidate) => candidate.label.toLowerCase().startsWith(prefix));
    return {
      from: match?.from ?? context.pos,
      options,
      validFor: /^[\w$]*$/u,
    };
  };
}
