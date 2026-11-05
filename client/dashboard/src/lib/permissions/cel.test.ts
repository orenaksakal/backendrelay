import { CompletionContext } from '@codemirror/autocomplete';
import { EditorState } from '@codemirror/state';
import { describe, expect, it } from 'vitest';
import { celCompletionSource, celOptions, lintCelExpression } from './cel';

function complete(expression: string) {
  const state = EditorState.create({ doc: expression });
  const source = celCompletionSource({
    attributes: ['id', 'title', 'done', 'owner'],
    references: [{ label: 'owner', attributes: ['id', 'email'] }],
    binds: ['ownerId'],
    action: 'create',
  });
  const context = new CompletionContext(state, expression.length, true);
  const result = source(context);
  if (result === null || result instanceof Promise) {
    throw new Error('completion did not resolve synchronously');
  }
  return result;
}

describe('CEL completions', () => {
  it('offers the roots available in a create rule', () => {
    const result = complete('');
    const labels = result.options.map((option) => option.label);
    expect(labels).toContain('auth');
    expect(labels).toContain('auth.id');
    expect(labels).toContain('data.title');
    expect(labels).toContain('newData.title');
    expect(labels).toContain("data.ref('owner.email')");
    expect(labels).toContain('ownerId');
    expect(labels).toContain('request.modifiedFields');
    expect(labels).not.toContain('linkedData');
  });

  it('filters by the typed prefix', () => {
    const result = complete('auth.i');
    expect(result.from).toBe(0);
    expect(result.options.map((option) => option.label)).toEqual(['auth.id']);
  });

  it('completes a reference attribute inside data.ref', () => {
    const result = complete("data.ref('owner.");
    expect(result.from).toBe(0);
    expect(result.options.map((option) => option.label)).toContain(
      "data.ref('owner.email')",
    );
  });

  it('adds linkedData and drops newData on link rules', () => {
    const labels = celOptions({ action: 'link' }).map((o) => o.label);
    expect(labels).toContain('linkedData');
    expect(labels).not.toContain('newData');

    const update = celOptions({ action: 'update' }).map((o) => o.label);
    expect(update).toContain('newData');
    expect(update).not.toContain('linkedData');
  });

  it('does not offer attributes with characters CEL cannot read', () => {
    const labels = celOptions({
      attributes: ['content-type', 'title'],
    }).map((o) => o.label);
    expect(labels).toContain('data.title');
    expect(labels).not.toContain('data.content-type');
  });
});

describe('CEL syntax check', () => {
  it('accepts expressions the server compiles', () => {
    for (const expression of [
      'true',
      'false',
      'auth.id != null',
      "auth.id in data.ref('owner.id')",
      '(data.done == true) && auth.email.endsWith("@example.com")',
      'data.count > 3 ? "many" : "few"',
      '[1, 2, 3].exists(x, x > 1)',
      '{"a": 1}["a"] == 1',
    ]) {
      expect(lintCelExpression(expression), expression).toEqual([]);
    }
  });

  it('reports unbalanced delimiters and strings', () => {
    expect(lintCelExpression('(auth.id')[0]?.message).toBe('Unclosed "(".');
    expect(lintCelExpression("auth.id == 'x")[0]?.message).toBe(
      'Unclosed string.',
    );
    expect(lintCelExpression('auth.id)')[0]?.message).toBe('Unexpected ")".');
    expect(lintCelExpression('data.items[0')[0]?.message).toBe('Unclosed "[".');
  });

  it('reports assignment and trailing operators', () => {
    expect(lintCelExpression('auth.id = "x"')[0]?.message).toBe(
      'CEL uses "==" to compare values.',
    );
    expect(lintCelExpression('auth.id === "x"')[0]?.message).toBe(
      'CEL uses "==", not "===".',
    );
    expect(lintCelExpression('auth.id !=')[0]?.message).toBe(
      'Expected an operand after "!=".',
    );
    expect(lintCelExpression('auth.id in')[0]?.message).toBe(
      'Expected an operand after "in".',
    );
    expect(lintCelExpression('auth.')[0]?.message).toBe(
      'Expected a field after ".".',
    );
  });

  it('pins the line and column for a multi-line expression', () => {
    const issue = lintCelExpression('auth.id != null &&\n  data.')[0];
    expect(issue?.line).toBe(2);
    expect(issue?.column).toBe(7);
  });
});
