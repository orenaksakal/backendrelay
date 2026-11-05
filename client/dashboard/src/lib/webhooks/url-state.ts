import { z } from 'zod';

/**
 * C.13 detail state: `?event=` opens one delivery in the sheet. The list has
 * no URL state; its create/edit sheet is local because it never survives a
 * reload.
 */

export const webhookDetailSearchSchema = z.object({
  event: z.string().trim().min(1).optional().catch(undefined),
});

export type WebhookDetailSearch = z.infer<typeof webhookDetailSearchSchema>;

/** Opens one delivery, keeping the rest of the search untouched. */
export function eventSearchFor(
  search: WebhookDetailSearch,
  isn: string,
): WebhookDetailSearch {
  return { ...search, event: isn };
}

/** Closes the delivery sheet. */
export function closedEventSearch(
  search: WebhookDetailSearch,
): WebhookDetailSearch {
  return search.event === undefined ? search : {};
}
