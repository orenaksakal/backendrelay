import { describe, expect, it } from 'vitest';
import { diffPrefix, unifiedDiff, unifiedDiffText } from './diff';

describe('unified permission diff', () => {
  it('returns no hunks for identical documents', () => {
    expect(unifiedDiff('{\n  "a": 1\n}', '{\n  "a": 1\n}')).toEqual([]);
  });

  it('marks a replaced line with its surrounding context', () => {
    const hunks = unifiedDiff(
      '{\n  "a": 1,\n  "b": 2\n}',
      '{\n  "a": 1,\n  "b": 3\n}',
    );

    expect(hunks).toHaveLength(1);
    expect(hunks[0]).toMatchObject({
      oldStart: 1,
      oldCount: 4,
      newStart: 1,
      newCount: 4,
    });
    expect(hunks[0]?.lines).toEqual([
      { kind: 'context', text: '{' },
      { kind: 'context', text: '  "a": 1,' },
      { kind: 'remove', text: '  "b": 2' },
      { kind: 'add', text: '  "b": 3' },
      { kind: 'context', text: '}' },
    ]);
    expect(unifiedDiffText(hunks)).toBe(
      [
        '@@ -1,4 +1,4 @@',
        ' {',
        '   "a": 1,',
        '-  "b": 2',
        '+  "b": 3',
        ' }',
      ].join('\n'),
    );
  });

  it('reports inserted and deleted lines with prefixes', () => {
    const added = unifiedDiff('a\nb', 'a\nx\nb');
    expect(unifiedDiffText(added)).toBe(
      ['@@ -1,2 +1,3 @@', ' a', '+x', ' b'].join('\n'),
    );

    const removed = unifiedDiff('a\nx\nb', 'a\nb');
    expect(unifiedDiffText(removed)).toBe(
      ['@@ -1,3 +1,2 @@', ' a', '-x', ' b'].join('\n'),
    );

    expect(diffPrefix('add')).toBe('+');
    expect(diffPrefix('remove')).toBe('-');
    expect(diffPrefix('context')).toBe(' ');
  });

  it('splits distant changes into separate hunks', () => {
    const before = Array.from(
      { length: 20 },
      (_, index) => `line ${String(index)}`,
    );
    const after = [...before];
    after[1] = 'changed 1';
    after[18] = 'changed 18';

    const hunks = unifiedDiff(before.join('\n'), after.join('\n'), 2);
    expect(hunks.length).toBe(2);
    expect(hunks[0]?.lines[0]).toEqual({ kind: 'context', text: 'line 0' });
    expect(hunks[1]?.lines.at(-1)).toEqual({
      kind: 'context',
      text: 'line 19',
    });
  });
});
