import { z } from 'zod';

/**
 * C.11 URL state: `?tab=` selects the providers, origins, email or test-users
 * tab; `?client=` opens one OAuth client and `?add=` opens a provider's add
 * form. The two drill-in params are mutually exclusive.
 */

export const APP_AUTH_TABS = [
  'providers',
  'origins',
  'email',
  'test-users',
] as const;

export type AppAuthTab = (typeof APP_AUTH_TABS)[number];

export const APP_AUTH_TAB_LABELS: Record<AppAuthTab, string> = {
  providers: 'Providers',
  origins: 'Redirect origins',
  email: 'Email',
  'test-users': 'Test users',
};

export function isAppAuthTab(value: unknown): value is AppAuthTab {
  return (
    typeof value === 'string' &&
    (APP_AUTH_TABS as readonly string[]).includes(value)
  );
}

export const appAuthSearchSchema = z.object({
  tab: z.enum(APP_AUTH_TABS).catch('providers'),
  client: z.string().trim().min(1).optional().catch(undefined),
  add: z.string().trim().min(1).optional().catch(undefined),
});

export type AppAuthSearch = z.infer<typeof appAuthSearchSchema>;

export function appAuthTabSearch(tab: AppAuthTab): AppAuthSearch {
  return { tab };
}

/** Opens one client in the sheet; the add form is closed. */
export function clientSheetSearch(
  search: AppAuthSearch,
  clientId: string,
): AppAuthSearch {
  return { tab: search.tab, client: clientId };
}

/** Opens a provider's add form; any open client sheet is closed. */
export function addProviderSearch(
  search: AppAuthSearch,
  provider: string,
): AppAuthSearch {
  return { tab: search.tab, add: provider };
}

/** Closes whichever sheet is open. */
export function closedSheetSearch(search: AppAuthSearch): AppAuthSearch {
  return { tab: search.tab };
}
