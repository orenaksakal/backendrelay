import { describe, expect, it } from 'vitest';
import {
  entityCountLabel,
  entityLabel,
  linkedEntities,
  namespaceCountLabel,
} from './entities';

describe('explorer entity values', () => {
  it('normalises one or many linked entities', () => {
    expect(linkedEntities({ id: 'user-1', email: 'a@example.com' })).toEqual([
      { id: 'user-1', label: 'a@example.com' },
    ]);
    expect(
      linkedEntities([
        { id: 'user-1', email: 'a@example.com' },
        { id: 'user-1', email: 'a@example.com' },
        { id: 'user-2' },
        null,
      ]),
    ).toEqual([
      { id: 'user-1', label: 'a@example.com' },
      { id: 'user-2', label: 'user-2' },
    ]);
    expect(linkedEntities(null)).toEqual([]);
    expect(linkedEntities('nope')).toEqual([]);
  });

  it('prefers a readable attribute for the label', () => {
    expect(entityLabel({ id: 'x', title: 'Buy milk' })).toBe('Buy milk');
    expect(entityLabel({ id: 'x', name: 'Milk' })).toBe('Milk');
    expect(entityLabel({ id: 'x' })).toBe('x');
    expect(entityLabel({})).toBe('Unnamed');
  });

  it('shows N+ until the aggregate count arrives', () => {
    expect(entityCountLabel(1204, 100)).toBe('1,204');
    expect(entityCountLabel(undefined, 100)).toBe('100+');
    expect(namespaceCountLabel(87)).toBe('87');
    expect(namespaceCountLabel(undefined)).toBe('');
  });
});
