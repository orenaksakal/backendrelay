import { formatCount } from '@/lib/format';

/**
 * Values the Explorer grid renders (C.6): linked entities become label + id
 * chips, and the footer/sidebar counts share one "N+" fallback for servers
 * that don't answer the aggregate query.
 */

export interface LinkedEntity {
  id: string;
  label: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function isUnknownArray(value: unknown): value is unknown[] {
  return Array.isArray(value);
}

/** Attributes that read well as the human label next to an id chip. */
const LABEL_ATTRIBUTES = ['title', 'name', 'email', 'label', 'slug', 'key'];

export function entityLabel(entity: Record<string, unknown>): string {
  for (const attribute of LABEL_ATTRIBUTES) {
    const value = entity[attribute];
    if (typeof value === 'string' && value.trim() !== '') return value;
  }
  return typeof entity['id'] === 'string' ? entity['id'] : 'Unnamed';
}

/**
 * A link cell is an object (cardinality one) or an array (many); missing ids
 * and duplicates are dropped so the popover never repeats a row.
 */
export function linkedEntities(value: unknown): LinkedEntity[] {
  const raw = isUnknownArray(value) ? value : value === null ? [] : [value];
  const seen = new Set<string>();
  const entities: LinkedEntity[] = [];

  for (const entry of raw) {
    if (!isRecord(entry)) continue;
    const id = entry['id'];
    if (typeof id !== 'string' || seen.has(id)) continue;
    seen.add(id);
    entities.push({ id, label: entityLabel(entry) });
  }
  return entities;
}

/** `1,204` when the count is known, otherwise `100+` for the loaded rows. */
export function entityCountLabel(
  total: number | undefined,
  loaded: number,
): string {
  return total === undefined ? `${formatCount(loaded)}+` : formatCount(total);
}

/** Sidebar counts: a total when the aggregate answered, nothing otherwise. */
export function namespaceCountLabel(total: number | undefined): string {
  return total === undefined ? '' : formatCount(total);
}
