import { describe, expect, it } from 'vitest';
import {
  actionRowKey,
  bindRowKey,
  fieldRowKey,
  relationshipRowKey,
} from './document';
import { errorPath, rowErrors } from './validation';

describe('server rule error mapping', () => {
  it('maps action, field, bind and relationship paths onto their rows', () => {
    const { byRow, unattached } = rowErrors([
      { in: ['todos', 'allow', 'view'], message: 'Invalid expression' },
      { in: ['todos', 'fields', 'title'], message: 'Field denied' },
      { in: ['todos', 'bind', 'owner'], message: 'Unknown bind' },
      { in: ['todos', 'allow', 'link', 'author'], message: 'Bad link' },
      { in: [], message: 'Rules must be an object' },
      { in: ['$default', 'allow'], message: 'Missing action' },
    ]);

    expect(byRow.get(actionRowKey('todos', 'view'))).toEqual([
      'Invalid expression',
    ]);
    expect(byRow.get(fieldRowKey('todos', 'title'))).toEqual(['Field denied']);
    expect(byRow.get(bindRowKey('todos', 'owner'))).toEqual(['Unknown bind']);
    expect(byRow.get(relationshipRowKey('todos', 'link', 'author'))).toEqual([
      'Bad link',
    ]);
    expect(unattached.map(errorPath)).toEqual(['Rules', '$default → allow']);
  });

  it('maps the relationship $default path onto its row', () => {
    const { byRow, unattached } = rowErrors([
      { in: ['todos', 'allow', 'link', '$default'], message: 'Bad default' },
    ]);
    expect(byRow.get(relationshipRowKey('todos', 'link', '$default'))).toEqual([
      'Bad default',
    ]);
    expect(unattached).toHaveLength(0);
  });
});
