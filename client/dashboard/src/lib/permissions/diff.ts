/**
 * C.10 diff review: a small line-based unified diff for the save dialog and
 * the history view. Rules documents are small, so a plain longest-common-
 * subsequence table is enough; very large inputs fall back to a full replace
 * instead of allocating an unbounded matrix.
 */

export type DiffLineKind = 'context' | 'add' | 'remove';

export interface DiffLine {
  kind: DiffLineKind;
  text: string;
}

export interface DiffHunk {
  oldStart: number;
  oldCount: number;
  newStart: number;
  newCount: number;
  lines: readonly DiffLine[];
}

interface PositionedLine extends DiffLine {
  oldLine: number;
  newLine: number;
}

/** Above this many matrix cells the diff degrades to remove-all/add-all. */
const MAX_MATRIX_CELLS = 4_000_000;

function splitLines(text: string): string[] {
  return text === '' ? [] : text.split('\n');
}

function positionedDiff(
  before: readonly string[],
  after: readonly string[],
): PositionedLine[] {
  const beforeCount = before.length;
  const afterCount = after.length;
  const lines: PositionedLine[] = [];

  if (beforeCount * afterCount > MAX_MATRIX_CELLS) {
    for (let index = 0; index < beforeCount; index++) {
      lines.push({
        kind: 'remove',
        text: before[index] ?? '',
        oldLine: index + 1,
        newLine: 1,
      });
    }
    for (let index = 0; index < afterCount; index++) {
      lines.push({
        kind: 'add',
        text: after[index] ?? '',
        oldLine: beforeCount + 1,
        newLine: index + 1,
      });
    }
    return lines;
  }

  const table: number[][] = Array.from({ length: beforeCount + 1 }, () =>
    new Array<number>(afterCount + 1).fill(0),
  );
  for (let row = beforeCount - 1; row >= 0; row--) {
    const rowValues = table[row];
    const nextRow = table[row + 1];
    if (rowValues === undefined || nextRow === undefined) continue;
    for (let column = afterCount - 1; column >= 0; column--) {
      const diagonal = nextRow[column + 1] ?? 0;
      rowValues[column] =
        (before[row] ?? '') === (after[column] ?? '')
          ? diagonal + 1
          : Math.max(nextRow[column] ?? 0, rowValues[column + 1] ?? 0);
    }
  }

  let row = 0;
  let column = 0;
  let oldLine = 1;
  let newLine = 1;
  while (row < beforeCount && column < afterCount) {
    const left = before[row] ?? '';
    const right = after[column] ?? '';
    if (left === right) {
      lines.push({ kind: 'context', text: left, oldLine, newLine });
      row += 1;
      column += 1;
      oldLine += 1;
      newLine += 1;
      continue;
    }
    const skipBefore = table[row + 1]?.[column] ?? 0;
    const skipAfter = table[row]?.[column + 1] ?? 0;
    if (skipBefore >= skipAfter) {
      lines.push({ kind: 'remove', text: left, oldLine, newLine });
      row += 1;
      oldLine += 1;
    } else {
      lines.push({ kind: 'add', text: right, oldLine, newLine });
      column += 1;
      newLine += 1;
    }
  }
  while (row < beforeCount) {
    lines.push({
      kind: 'remove',
      text: before[row] ?? '',
      oldLine,
      newLine,
    });
    row += 1;
    oldLine += 1;
  }
  while (column < afterCount) {
    lines.push({
      kind: 'add',
      text: after[column] ?? '',
      oldLine,
      newLine,
    });
    column += 1;
    newLine += 1;
  }
  return lines;
}

/** Unified hunks with `context` unchanged lines around every change. */
export function unifiedDiff(
  before: string,
  after: string,
  context = 3,
): DiffHunk[] {
  const lines = positionedDiff(splitLines(before), splitLines(after));
  const changed: number[] = [];
  lines.forEach((line, index) => {
    if (line.kind !== 'context') changed.push(index);
  });
  const first = changed[0];
  if (first === undefined) return [];

  const spans: Array<[number, number]> = [];
  let start = Math.max(0, first - context);
  let end = Math.min(lines.length, first + context + 1);
  for (const index of changed.slice(1)) {
    if (index <= end + context) {
      end = Math.min(lines.length, index + context + 1);
      continue;
    }
    spans.push([start, end]);
    start = Math.max(0, index - context);
    end = Math.min(lines.length, index + context + 1);
  }
  spans.push([start, end]);

  const hunks: DiffHunk[] = [];
  for (const [spanStart, spanEnd] of spans) {
    const slice = lines.slice(spanStart, spanEnd);
    const anchor = slice[0];
    if (anchor === undefined) continue;
    const oldCount = slice.filter((line) => line.kind !== 'add').length;
    const newCount = slice.filter((line) => line.kind !== 'remove').length;
    hunks.push({
      oldStart:
        oldCount === 0 ? Math.max(0, anchor.oldLine - 1) : anchor.oldLine,
      oldCount,
      newStart:
        newCount === 0 ? Math.max(0, anchor.newLine - 1) : anchor.newLine,
      newCount,
      lines: slice.map(({ kind, text }) => ({ kind, text })),
    });
  }
  return hunks;
}

export function diffPrefix(kind: DiffLineKind): string {
  if (kind === 'add') return '+';
  if (kind === 'remove') return '-';
  return ' ';
}

/** Plain unified-diff text, for example when a hunk needs to be copied. */
export function unifiedDiffText(hunks: readonly DiffHunk[]): string {
  return hunks
    .map((hunk) =>
      [
        `@@ -${String(hunk.oldStart)},${String(hunk.oldCount)} +${String(hunk.newStart)},${String(hunk.newCount)} @@`,
        ...hunk.lines.map((line) => `${diffPrefix(line.kind)}${line.text}`),
      ].join('\n'),
    )
    .join('\n');
}
