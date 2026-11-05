import {
  apiSchemaToInstantSchemaDef,
  generateSchemaTypescriptFile,
  type InstantAPIPlatformSchema,
} from '@instantdb/platform';
import type { InstantAttr } from '@/lib/instant';

/**
 * C.7 Code tab: render the app's catalog as `instant.schema.ts` with the same
 * `@instantdb/platform` generator the CLI uses. The conversion mirrors the old
 * explorer (`client/packages/components/src/schema.ts`): system catalogs are
 * hidden except `$users`/`$files`, and `$files`' internal attributes are left
 * out because the server derives them.
 */

export const SCHEMA_TYPESCRIPT_FILENAME = 'instant.schema.ts';

const FILES_URL_ATTR_ID = '96653230-13ff-ffff-2a35-48afffffffff';

const HIDDEN_FILES_LABELS = new Set([
  'content-type',
  'content-disposition',
  'size',
  'location-id',
  'key-version',
]);

const VISIBLE_SYSTEM_NAMESPACES = new Set(['$users', '$files']);

function isHiddenAttr(attr: InstantAttr): boolean {
  const [, namespace, label, visible] = attr['forward-identity'];
  if (visible === false) return true;
  if (attr.catalog === 'system' && !VISIBLE_SYSTEM_NAMESPACES.has(namespace)) {
    return true;
  }
  return namespace === '$files' && HIDDEN_FILES_LABELS.has(label);
}

/** The fields `apiSchemaToInstantSchemaDef` reads, with defaults filled in. */
function platformAttr(attr: InstantAttr): Record<string, unknown> {
  const forward = attr['forward-identity'];
  const reverse = attr['reverse-identity'];
  const required = attr.id === FILES_URL_ATTR_ID ? true : attr['required?'];
  return {
    id: attr.id,
    'forward-identity': [forward[0], forward[1], forward[2]],
    ...(reverse === undefined || reverse === null
      ? {}
      : { 'reverse-identity': [reverse[0], reverse[1], reverse[2]] }),
    'index?': attr['index?'],
    'unique?': attr['unique?'],
    'required?': required ?? false,
    cardinality: attr.cardinality,
    'value-type': attr['value-type'],
    'inferred-types': attr['inferred-types'] ?? null,
    catalog: attr.catalog ?? 'user',
    ...(attr['checked-data-type'] === undefined
      ? {}
      : { 'checked-data-type': attr['checked-data-type'] }),
    ...(attr['on-delete'] === undefined
      ? {}
      : { 'on-delete': attr['on-delete'] }),
    ...(attr['on-delete-reverse'] === undefined
      ? {}
      : { 'on-delete-reverse': attr['on-delete-reverse'] }),
  };
}

export function platformSchemaFromAttrs(
  attrs: Record<string, InstantAttr> | null | undefined,
): InstantAPIPlatformSchema {
  const refs: Record<string, unknown> = {};
  const blobs: Record<string, Record<string, unknown>> = {};
  if (attrs === null || attrs === undefined) {
    return { refs, blobs } as unknown as InstantAPIPlatformSchema;
  }

  for (const attr of Object.values(attrs)) {
    if (isHiddenAttr(attr)) continue;
    const normalized = platformAttr(attr);
    const [, namespace, label] = attr['forward-identity'];
    if (attr['value-type'] === 'ref') {
      const reverse = attr['reverse-identity'];
      if (reverse === undefined || reverse === null) continue;
      const key = JSON.stringify([namespace, label, reverse[1], reverse[2]]);
      refs[key] = normalized;
      continue;
    }
    const entity = (blobs[namespace] ??= {});
    entity[label] = normalized;
  }
  return { refs, blobs } as unknown as InstantAPIPlatformSchema;
}

/** The generated file, or a loading comment before the catalog arrives. */
export function schemaTypescriptFile(
  attrs: Record<string, InstantAttr> | null | undefined,
  moduleName = '@instantdb/core',
): string {
  if (attrs === null || attrs === undefined) {
    return '/* Loading schema… */';
  }
  const schema = apiSchemaToInstantSchemaDef(platformSchemaFromAttrs(attrs));
  return generateSchemaTypescriptFile(schema, schema, moduleName);
}

/** Browser download for the Code tab; jsdom tests cover the generated text. */
export function downloadTextFile(
  filename: string,
  text: string,
  type = 'text/typescript',
): void {
  const blob = new Blob([text], { type });
  const url = URL.createObjectURL(blob);
  const link = document.createElement('a');
  link.href = url;
  link.download = filename;
  link.rel = 'noopener';
  link.click();
  URL.revokeObjectURL(url);
}

export function downloadSchemaTypescript(code: string): void {
  downloadTextFile(SCHEMA_TYPESCRIPT_FILENAME, code);
}
