import type { AppSchema } from '@/lib/instant';
import type { CelCompletionContext, CelReference } from './cel';
import { readBinds } from './document';
import type { PermissionAction } from './effective';

/**
 * C.10: completion scope for one namespace row. Links point at the target
 * namespace's attributes, which is what `data.ref('label.attr')` reads.
 */
export function celContextFor(
  schema: AppSchema | undefined,
  rules: unknown,
  namespace: string,
  action: PermissionAction | 'link' | 'unlink',
): CelCompletionContext {
  const namespaceSchema = schema?.namespaces.find(
    (candidate) => candidate.name === namespace,
  );
  const references: CelReference[] =
    namespaceSchema?.links.flatMap((link) => {
      const target = schema?.namespaces.find(
        (candidate) => candidate.name === link.targetNamespace,
      );
      return target === undefined
        ? []
        : [
            {
              label: link.label,
              attributes: target.attributes.map((attribute) => attribute.name),
            },
          ];
    }) ?? [];

  return {
    attributes: namespaceSchema?.attributes.map((attribute) => attribute.name),
    references,
    binds: readBinds(rules, namespace).rows.map((row) => row.name),
    action,
  };
}
