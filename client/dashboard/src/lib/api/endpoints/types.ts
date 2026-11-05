import type { z } from 'zod';

export type HttpMethod = 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE';

/** Path parameters and query parameters share one object. */
export type ApiParamValue = string | number | boolean | null | undefined;
export type ApiParams = Record<string, ApiParamValue>;

/**
 * One dashboard API endpoint: `:name` segments are filled from `params`, every
 * other parameter becomes part of the query string.
 */
export interface Endpoint<
  TResponse extends z.ZodType = z.ZodType,
  TBody extends z.ZodType | undefined = undefined,
> {
  readonly method: HttpMethod;
  readonly path: string;
  readonly body?: TBody;
  readonly response: TResponse;
}

/**
 * Declares an endpoint without losing the schema types that `apiFetch` needs.
 * Responses must be `z.looseObject` so fields the dashboard does not use yet
 * pass through untouched.
 */
export function defineEndpoint<
  TResponse extends z.ZodType,
  TBody extends z.ZodType | undefined = undefined,
>(definition: Endpoint<TResponse, TBody>): Endpoint<TResponse, TBody> {
  return definition;
}
