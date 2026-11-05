import { beforeEach, describe, expect, it } from 'vitest';
import {
  hiddenColumnsKey,
  namespacePinsKey,
  readHiddenColumns,
  readNamespacePins,
  toggleHiddenColumn,
  toggleNamespacePin,
  writeHiddenColumns,
  writeNamespacePins,
} from './storage';

beforeEach(() => {
  window.localStorage.clear();
});

describe('explorer preferences', () => {
  it('stores namespace pins per app', () => {
    expect(namespacePinsKey('app-1')).toBe('br.nsPins.app-1');
    expect(readNamespacePins('app-1')).toEqual([]);

    expect(toggleNamespacePin('app-1', 'todos')).toEqual(['todos']);
    expect(toggleNamespacePin('app-1', 'goals')).toEqual(['todos', 'goals']);
    expect(readNamespacePins('app-1')).toEqual(['todos', 'goals']);
    expect(readNamespacePins('app-2')).toEqual([]);

    expect(toggleNamespacePin('app-1', 'todos')).toEqual(['goals']);
    expect(window.localStorage.getItem('br.nsPins.app-1')).toBe(
      JSON.stringify(['goals']),
    );
  });

  it('stores hidden columns per app and namespace', () => {
    expect(hiddenColumnsKey('app-1', 'todos')).toBe('br.cols.app-1.todos');

    expect(toggleHiddenColumn('app-1', 'todos', 'title')).toEqual(['title']);
    expect(toggleHiddenColumn('app-1', 'todos', 'done')).toEqual([
      'title',
      'done',
    ]);
    expect(readHiddenColumns('app-1', 'todos')).toEqual(['title', 'done']);
    expect(readHiddenColumns('app-1', 'goals')).toEqual([]);

    writeHiddenColumns('app-1', 'goals', ['owner']);
    expect(readHiddenColumns('app-1', 'goals')).toEqual(['owner']);

    expect(toggleHiddenColumn('app-1', 'todos', 'title')).toEqual(['done']);
  });

  it('ignores corrupt entries instead of failing the screen', () => {
    window.localStorage.setItem('br.nsPins.app-1', '{not-json');
    window.localStorage.setItem('br.cols.app-1.todos', '["ok", 42]');
    expect(readNamespacePins('app-1')).toEqual([]);
    expect(readHiddenColumns('app-1', 'todos')).toEqual([]);

    expect(() => {
      writeNamespacePins('app-1', ['todos']);
      writeHiddenColumns('app-1', 'todos', ['title']);
    }).not.toThrow();
  });
});
