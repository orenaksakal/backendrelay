import type { AppAttribute, AppInstantClient } from '@/lib/instant';

/**
 * Explorer writes (C.6). Everything here goes through the app's Instant client,
 * so an edit from the dashboard takes the same server path, webhooks and
 * permission rules as a write from the app itself.
 *
 * The helpers are pure where they can be: the sheet and grid tests cover value
 * parsing, unset handling and the transaction the sheet builds.
 */

export type ParsedValue =
  | { ok: true; value: unknown }
  | { ok: false; message: string };

/** Plain text for a cell value, used by the inline editor. */
export function cellEditText(value: unknown): string {
  if (value === null || value === undefined) return '';
  if (typeof value === 'string') return value;
  if (typeof value === 'number' || typeof value === 'boolean') {
    return String(value);
  }
  try {
    return JSON.stringify(value);
  } catch {
    return '';
  }
}

/** C.6: string, number, boolean and date cells edit in place. */
export function isInlineEditable(attribute: AppAttribute): boolean {
  if (attribute.name === 'id') return false;
  return (
    attribute.type === 'string' ||
    attribute.type === 'number' ||
    attribute.type === 'boolean' ||
    attribute.type === 'date'
  );
}

function dateValue(raw: string, previous: unknown): ParsedValue {
  const text = raw.trim();
  if (text === '') return { ok: false, message: 'Enter a date.' };
  const timestamp = Date.parse(text);
  if (Number.isNaN(timestamp)) {
    return { ok: false, message: 'Enter a date, for example 2026-01-31.' };
  }
  return {
    ok: true,
    value:
      typeof previous === 'number'
        ? timestamp
        : new Date(timestamp).toISOString(),
  };
}

/**
 * Validates and coerces one inline edit. A value the client cannot store is
 * rejected here, before an optimistic write is sent.
 */
export function parseCellValue(
  attribute: AppAttribute,
  raw: unknown,
  previous: unknown,
): ParsedValue {
  switch (attribute.type) {
    case 'number': {
      if (typeof raw === 'number') {
        return Number.isFinite(raw)
          ? { ok: true, value: raw }
          : { ok: false, message: 'Enter a number.' };
      }
      const text = cellEditText(raw).trim();
      if (text === '') return { ok: false, message: 'Enter a number.' };
      const value = Number(text);
      return Number.isFinite(value)
        ? { ok: true, value }
        : { ok: false, message: 'Enter a number.' };
    }
    case 'boolean': {
      if (typeof raw === 'boolean') return { ok: true, value: raw };
      const text = cellEditText(raw).trim().toLowerCase();
      if (text === 'true' || text === '1' || text === 'yes') {
        return { ok: true, value: true };
      }
      if (text === 'false' || text === '0' || text === 'no') {
        return { ok: true, value: false };
      }
      return { ok: false, message: 'Enter true or false.' };
    }
    case 'date':
      return dateValue(cellEditText(raw), previous);
    default:
      return { ok: true, value: cellEditText(raw) };
  }
}

/** `YYYY-MM-DDTHH:mm` for a `datetime-local` control, in the browser's zone. */
export function toDateTimeLocal(value: unknown): string {
  if (value === null || value === undefined || value === '') return '';
  const date =
    value instanceof Date
      ? value
      : new Date(
          typeof value === 'number' || typeof value === 'string' ? value : '',
        );
  if (Number.isNaN(date.getTime())) return '';
  const pad = (part: number) => String(part).padStart(2, '0');
  return `${String(date.getFullYear())}-${pad(date.getMonth() + 1)}-${pad(
    date.getDate(),
  )}T${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/**
 * A date field comes back as an ISO string; empty means the operator cleared
 * the control, which the sheet turns into "unset".
 */
export function fromDateTimeLocal(raw: string): ParsedValue {
  if (raw.trim() === '') return { ok: true, value: null };
  return dateValue(raw, undefined);
}

export interface FieldValue {
  value?: string | number | boolean | null;
  /** The "Clear (unset)" action; the attribute's triple is retracted. */
  unset: boolean;
  /** Raw editor text for a JSON attribute. */
  json?: string;
  /** The value the entity had when the sheet opened, for an exact retract. */
  original?: unknown;
}

export type FieldValues = Record<string, FieldValue>;

/** Entity values as the sheet's controls expect them. */
export function initialFieldValues(
  attributes: ReadonlyArray<AppAttribute>,
  entity: Record<string, unknown> | null,
): FieldValues {
  const fields: FieldValues = {};
  for (const attribute of attributes) {
    if (attribute.name === 'id') continue;
    const value = entity?.[attribute.name];
    if (attribute.type === 'json') {
      fields[attribute.name] = {
        value: undefined,
        unset: false,
        original: value,
        json: value === undefined ? undefined : JSON.stringify(value, null, 2),
      };
      continue;
    }
    if (attribute.type === 'boolean') {
      fields[attribute.name] = {
        value: typeof value === 'boolean' ? value : undefined,
        unset: false,
        original: value,
      };
      continue;
    }
    if (attribute.type === 'number') {
      fields[attribute.name] = {
        value: typeof value === 'number' ? value : undefined,
        unset: false,
        original: value,
      };
      continue;
    }
    if (attribute.type === 'date') {
      fields[attribute.name] = {
        value:
          value === undefined || value === null
            ? undefined
            : toDateTimeLocal(value),
        unset: false,
        original: value,
      };
      continue;
    }
    fields[attribute.name] = {
      value: typeof value === 'string' ? value : undefined,
      unset: false,
      original: value,
    };
  }
  return fields;
}

export interface UnsetAttribute {
  id: string;
  /** The stored value, so the retract triple matches exactly. */
  value: unknown;
}

export interface CollectedEntityFields {
  values: Record<string, unknown>;
  unset: UnsetAttribute[];
  errors: Record<string, string>;
}

const REQUIRED_ERROR = 'This field is required.';

function isBlank(value: unknown): boolean {
  if (value === undefined || value === null) return true;
  return typeof value === 'string' && value.trim() === '';
}

/** Only a value the entity actually had can be retracted. */
function pushUnset(
  unset: UnsetAttribute[],
  attribute: AppAttribute,
  field: FieldValue,
): void {
  if (field.original === undefined) return;
  unset.push({ id: attribute.id, value: field.original });
}

/**
 * Turns the sheet's controls into an update: set values, attribute ids to
 * retract, and per-field errors that keep Save disabled.
 */
export function collectEntityFields(
  attributes: ReadonlyArray<AppAttribute>,
  fields: FieldValues,
): CollectedEntityFields {
  const values: Record<string, unknown> = {};
  const unset: UnsetAttribute[] = [];
  const errors: Record<string, string> = {};

  for (const attribute of attributes) {
    if (attribute.name === 'id') continue;
    const field = fields[attribute.name];
    if (field === undefined) continue;

    if (field.unset) {
      if (attribute.isRequired) {
        errors[attribute.name] = REQUIRED_ERROR;
      } else {
        pushUnset(unset, attribute, field);
      }
      continue;
    }

    if (attribute.type === 'json') {
      if (field.json === undefined) {
        if (attribute.isRequired) errors[attribute.name] = REQUIRED_ERROR;
        continue;
      }
      const text = field.json;
      if (text.trim() === '') {
        if (attribute.isRequired) {
          errors[attribute.name] = REQUIRED_ERROR;
        } else {
          pushUnset(unset, attribute, field);
        }
        continue;
      }
      try {
        values[attribute.name] = JSON.parse(text);
      } catch {
        errors[attribute.name] = 'Enter valid JSON.';
      }
      continue;
    }

    if (attribute.type === 'number') {
      if (field.value === undefined) {
        if (attribute.isRequired) errors[attribute.name] = REQUIRED_ERROR;
        continue;
      }
      if (field.value === null || field.value === '') {
        if (attribute.isRequired) {
          errors[attribute.name] = REQUIRED_ERROR;
        } else {
          pushUnset(unset, attribute, field);
        }
        continue;
      }
      const parsed = parseCellValue(attribute, field.value, field.value);
      if (parsed.ok) {
        values[attribute.name] = parsed.value;
      } else {
        errors[attribute.name] = parsed.message;
      }
      continue;
    }

    if (attribute.type === 'boolean') {
      if (field.value === undefined || field.value === null) {
        if (attribute.isRequired) errors[attribute.name] = REQUIRED_ERROR;
        continue;
      }
      values[attribute.name] = field.value === true;
      continue;
    }

    if (attribute.type === 'date') {
      if (field.value === undefined) {
        if (attribute.isRequired) errors[attribute.name] = REQUIRED_ERROR;
        continue;
      }
      const parsed = fromDateTimeLocal(
        typeof field.value === 'string' ? field.value : '',
      );
      if (!parsed.ok) {
        errors[attribute.name] = parsed.message;
      } else if (parsed.value === null) {
        if (attribute.isRequired) {
          errors[attribute.name] = REQUIRED_ERROR;
        } else {
          pushUnset(unset, attribute, field);
        }
      } else {
        values[attribute.name] = parsed.value;
      }
      continue;
    }

    if (field.value === undefined) {
      if (attribute.isRequired) errors[attribute.name] = REQUIRED_ERROR;
      continue;
    }

    const value = field.value === null ? '' : String(field.value);
    if (attribute.isRequired && isBlank(value)) {
      errors[attribute.name] = REQUIRED_ERROR;
      continue;
    }
    values[attribute.name] = value;
  }

  return { values, unset, errors };
}

export interface EntityWriteInput {
  namespace: string;
  entityId: string;
  values: Record<string, unknown>;
  /** Attributes to retract for "Clear (unset)", with their stored values. */
  unset: ReadonlyArray<UnsetAttribute>;
  /** Link label -> target ids. Cardinality one passes a single id. */
  addLinks: Record<string, string | string[]>;
  removeLinks: Record<string, string | string[]>;
}

export interface EntityWritePlan {
  /** One transact chunk for the values and link changes. */
  chunks: unknown[];
  /** Raw retract steps for unset attributes. */
  unsetSteps: unknown[][];
}

function isEmptyLinkValue(value: string | string[]): boolean {
  return typeof value === 'string' ? value === '' : value.length === 0;
}

/** `link`/`unlink` take a scalar for cardinality one and an array for many. */
export function linkUpdateValue(
  cardinality: 'one' | 'many',
  ids: ReadonlyArray<string>,
): string | string[] {
  if (cardinality === 'one') return ids[0] ?? '';
  return [...ids];
}

/** The transaction the sheet runs; separate from sending it so tests can inspect it. */
export function planEntityWrite(
  client: AppInstantClient,
  input: EntityWriteInput,
): EntityWritePlan {
  const namespace = client.tx[input.namespace];
  const start = namespace?.[input.entityId];
  if (start === undefined) {
    throw new Error(`Could not write to ${input.namespace}.`);
  }

  let chunk = start;
  let used = false;
  for (const [label, ids] of Object.entries(input.removeLinks)) {
    if (isEmptyLinkValue(ids)) continue;
    chunk = chunk.unlink({ [label]: ids });
    used = true;
  }
  if (Object.keys(input.values).length > 0) {
    chunk = chunk.update(input.values);
    used = true;
  }
  for (const [label, ids] of Object.entries(input.addLinks)) {
    if (isEmptyLinkValue(ids)) continue;
    chunk = chunk.link({ [label]: ids });
    used = true;
  }

  return {
    chunks: used ? [chunk] : [],
    unsetSteps: input.unset.map((attribute) => [
      'retract-triple',
      input.entityId,
      attribute.id,
      attribute.value,
    ]),
  };
}

/**
 * Sends a sheet write. Values and links travel in one transact so webhooks and
 * rules see a single change; unset attributes are retracted afterwards.
 */
export async function runEntityWrite(
  client: AppInstantClient,
  plan: EntityWritePlan,
): Promise<void> {
  if (plan.chunks.length > 0) {
    await client.transact(plan.chunks);
  }
  if (plan.unsetSteps.length > 0) {
    await client.core._reactor.pushOps(plan.unsetSteps);
  }
}

export function deleteEntities(
  client: AppInstantClient,
  namespace: string,
  ids: ReadonlyArray<string>,
): Promise<unknown> {
  const namespaceChunks = client.tx[namespace];
  const chunks = ids.map((id) => {
    const chunk = namespaceChunks?.[id];
    if (chunk === undefined) throw new Error(`Could not delete ${id}.`);
    return chunk.delete();
  });
  return client.transact(chunks);
}

/** One inline edit: the value is optimistic and the live query confirms it. */
export function updateCell(
  client: AppInstantClient,
  namespace: string,
  entityId: string,
  attribute: string,
  value: unknown,
): Promise<unknown> {
  const chunk = client.tx[namespace]?.[entityId];
  if (chunk === undefined) {
    return Promise.reject(new Error(`Could not update ${entityId}.`));
  }
  return client.transact(chunk.update({ [attribute]: value }));
}

export interface EntityExport {
  filename: string;
  json: string;
}

function exportDate(now: Date): string {
  const pad = (part: number) => String(part).padStart(2, '0');
  return `${String(now.getFullYear())}-${pad(now.getMonth() + 1)}-${pad(
    now.getDate(),
  )}`;
}

/** `{ns}-{date}.json` with the selected entities under their namespace. */
export function entityExport(
  namespace: string,
  entities: ReadonlyArray<Record<string, unknown>>,
  now: Date = new Date(),
): EntityExport {
  return {
    filename: `${namespace}-${exportDate(now)}.json`,
    json: JSON.stringify({ [namespace]: entities }, null, 2),
  };
}

/** Browser download; the jsdom tests cover the payload instead. */
export function downloadJson(filename: string, json: string): void {
  const blob = new Blob([json], { type: 'application/json' });
  const url = URL.createObjectURL(blob);
  const link = document.createElement('a');
  link.href = url;
  link.download = filename;
  link.rel = 'noopener';
  link.click();
  URL.revokeObjectURL(url);
}

export function newEntityId(): string {
  return crypto.randomUUID();
}

/** C.6 namespace rules: letters, numbers, `_`, `-`, and no `$` prefix. */
export function namespaceNameProblem(
  value: string,
  existing: ReadonlyArray<string> = [],
): string | undefined {
  const name = value.trim();
  if (name === '') return 'Enter a namespace name.';
  if (name.startsWith('$')) return 'Namespace names cannot start with $.';
  if (!/^[A-Za-z0-9_-]+$/.test(name)) {
    return 'Use only letters, numbers, _ and -.';
  }
  if (existing.includes(name)) return 'That namespace already exists.';
  return undefined;
}

/**
 * Creating a namespace means adding its `id` attribute, exactly as the old
 * Explorer did. The server creates the namespace when the attribute lands.
 */
export function newNamespaceOps(
  namespace: string,
  nextId: () => string = () => crypto.randomUUID(),
): unknown[][] {
  const attribute = {
    id: nextId(),
    'forward-identity': [nextId(), namespace, 'id'],
    'value-type': 'blob',
    cardinality: 'one',
    'unique?': true,
    'index?': false,
  };
  return [['add-attr', attribute]];
}
