import type { Extension } from '@codemirror/state';
import { jsonSchema } from 'codemirror-json-schema';

/** Shape accepted by `codemirror-json-schema`: a JSON Schema document. */
export type JsonSchemaDocument = Parameters<typeof jsonSchema>[0];

/**
 * JSON Schema extensions (lint, completion and hover).
 *
 * `CodeEditor` imports this module only after a `json-with-schema` editor
 * mounts. The schema library and its Markdown highlighter are large, and most
 * editors never need them, so they stay out of the shared editor chunk (D9.3).
 */
export function jsonSchemaExtensions(schema: JsonSchemaDocument): Extension[] {
  return jsonSchema(schema);
}
