import {
  useInfiniteQuery,
  useMutation,
  useQueries,
  useQuery,
  useQueryClient,
  type QueryClient,
} from '@tanstack/react-query';
import { apiFetch } from '@/lib/api/client';
import {
  appEndpoints,
  webhookEndpoints,
  type Webhook,
  type WebhookAction,
  type WebhookEventsPage,
} from '@/lib/api/endpoints';
import { appSchemaFromAttrs, type InstantAttr } from '@/lib/instant';
import {
  anyEventInFlight,
  DELIVERY_POLL_MS,
  latestEvent,
  sortedNamespaces,
  sortedWebhooks,
} from './model';

/**
 * C.13 data: the webhook list (the detail screen reads the same payload), the
 * paginated deliveries, one delivery and its payload snapshot, the schema
 * namespaces the form offers, and the list's last-delivery lookup.
 */

export const webhookQueryKeys = {
  list: (appId: string) => ['dash', 'apps', appId, 'webhooks'] as const,
  events: (appId: string, webhookId: string) =>
    ['dash', 'apps', appId, 'webhooks', webhookId, 'events'] as const,
  event: (appId: string, webhookId: string, isn: string) =>
    [
      'dash',
      'apps',
      appId,
      'webhooks',
      webhookId,
      'events',
      'event',
      isn,
    ] as const,
  payload: (appId: string, webhookId: string, isn: string) =>
    ['dash', 'apps', appId, 'webhooks', webhookId, 'payload', isn] as const,
  namespaces: (appId: string) =>
    ['dash', 'apps', appId, 'webhooks', 'namespaces'] as const,
  lastDelivery: (appId: string, webhookId: string) =>
    ['dash', 'apps', appId, 'webhooks', webhookId, 'last-delivery'] as const,
};

export function useWebhooks(appId: string) {
  return useQuery({
    queryKey: webhookQueryKeys.list(appId),
    queryFn: ({ signal }) =>
      apiFetch(webhookEndpoints.list, { params: { appId }, signal }),
    select: (data) => ({ ...data, webhooks: sortedWebhooks(data.webhooks) }),
  });
}

function updateCachedWebhook(
  queryClient: QueryClient,
  appId: string,
  webhook: Webhook,
): void {
  queryClient.setQueryData<{ webhooks: Webhook[] }>(
    webhookQueryKeys.list(appId),
    (current) => {
      if (current === undefined) return current;
      const exists = current.webhooks.some((entry) => entry.id === webhook.id);
      return {
        webhooks: exists
          ? current.webhooks.map((entry) =>
              entry.id === webhook.id ? webhook : entry,
            )
          : [webhook, ...current.webhooks],
      };
    },
  );
}

function removeCachedWebhook(
  queryClient: QueryClient,
  appId: string,
  webhookId: string,
): void {
  queryClient.setQueryData<{ webhooks: Webhook[] }>(
    webhookQueryKeys.list(appId),
    (current) =>
      current === undefined
        ? current
        : {
            ...current,
            webhooks: current.webhooks.filter(
              (webhook) => webhook.id !== webhookId,
            ),
          },
  );
}

export interface CreateWebhookVariables {
  url: string;
  namespaces: string[];
  actions: WebhookAction[];
}

export function useCreateWebhook(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (body: CreateWebhookVariables) =>
      apiFetch(webhookEndpoints.create, {
        params: { appId },
        body: {
          url: body.url,
          namespaces: body.namespaces,
          actions: body.actions,
        },
      }),
    onSuccess: (result) => {
      updateCachedWebhook(queryClient, appId, result.webhook);
    },
  });
}

export interface UpdateWebhookVariables extends CreateWebhookVariables {
  webhookId: string;
}

export function useUpdateWebhook(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({
      webhookId,
      url,
      namespaces,
      actions,
    }: UpdateWebhookVariables) =>
      apiFetch(webhookEndpoints.update, {
        params: { appId, webhookId },
        body: { url, namespaces, actions },
      }),
    onSuccess: (result) => {
      updateCachedWebhook(queryClient, appId, result.webhook);
    },
  });
}

export function useDeleteWebhook(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (webhookId: string) =>
      apiFetch(webhookEndpoints.remove, { params: { appId, webhookId } }),
    onSuccess: (_result, webhookId) => {
      removeCachedWebhook(queryClient, appId, webhookId);
    },
  });
}

export interface SetWebhookEnabledVariables {
  webhookId: string;
  enabled: boolean;
  /** Optional, shown on the list while the webhook is disabled. */
  reason?: string;
}

/**
 * Enable is immediate; disable can carry a reason. Both write the
 * acknowledged row into the cached list so the pill and the menu agree.
 */
export function useSetWebhookEnabled(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ webhookId, enabled, reason }: SetWebhookEnabledVariables) =>
      enabled
        ? apiFetch(webhookEndpoints.enable, { params: { appId, webhookId } })
        : apiFetch(webhookEndpoints.disable, {
            params: { appId, webhookId },
            body:
              reason === undefined || reason.trim() === '' ? {} : { reason },
          }),
    onSuccess: (result) => {
      updateCachedWebhook(queryClient, appId, result.webhook);
    },
  });
}

/**
 * The list's "last delivery" column. The server's list endpoint does not
 * return delivery state, so each row asks for the newest event on its own
 * webhook. The page shows at most 100 webhooks and the results are cached.
 */
export function useWebhookLastDeliveries(
  appId: string,
  webhookIds: readonly string[],
) {
  return useQueries({
    queries: webhookIds.map((webhookId) => ({
      queryKey: webhookQueryKeys.lastDelivery(appId, webhookId),
      queryFn: ({ signal }) =>
        apiFetch(webhookEndpoints.events, {
          params: { appId, webhookId },
          signal,
        }),
      select: (data: WebhookEventsPage) => latestEvent(data.events),
      staleTime: 30_000,
    })),
  }).map((query) => ({
    event: query.data,
    isLoading: query.isPending,
  }));
}

export function useWebhookEvents(appId: string, webhookId: string) {
  return useInfiniteQuery({
    queryKey: webhookQueryKeys.events(appId, webhookId),
    queryFn: ({ pageParam, signal }) =>
      apiFetch(webhookEndpoints.events, {
        params: { appId, webhookId, after: pageParam },
        signal,
      }),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) =>
      last.pageInfo.hasNextPage
        ? (last.pageInfo.endCursor ?? undefined)
        : undefined,
    // A pending or processing delivery changes within seconds (C.13).
    refetchInterval: (query) => {
      const events = query.state.data?.pages.flatMap((page) => page.events);
      return events !== undefined && anyEventInFlight(events)
        ? DELIVERY_POLL_MS
        : false;
    },
  });
}

export function useWebhookEvent(
  appId: string,
  webhookId: string,
  isn: string | undefined,
) {
  return useQuery({
    queryKey: webhookQueryKeys.event(appId, webhookId, isn ?? ''),
    queryFn: ({ signal }) =>
      apiFetch(webhookEndpoints.event, {
        params: { appId, webhookId, isn },
        slashedPathParams: ['isn'],
        signal,
      }),
    enabled: isn !== undefined,
  });
}

export function useWebhookPayload(
  appId: string,
  webhookId: string,
  isn: string | undefined,
) {
  return useQuery({
    queryKey: webhookQueryKeys.payload(appId, webhookId, isn ?? ''),
    queryFn: ({ signal }) =>
      apiFetch(webhookEndpoints.payload, {
        params: { appId, webhookId, isn },
        slashedPathParams: ['isn'],
        signal,
      }),
    enabled: isn !== undefined,
  });
}

export function useResendWebhookEvent(appId: string, webhookId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (isn: string) =>
      apiFetch(webhookEndpoints.resendEvent, {
        params: { appId, webhookId, isn },
        slashedPathParams: ['isn'],
      }),
    onSuccess: (result) => {
      queryClient.setQueryData(
        webhookQueryKeys.event(appId, webhookId, result.event.isn),
        result,
      );
      void queryClient.invalidateQueries({
        queryKey: webhookQueryKeys.events(appId, webhookId),
      });
      void queryClient.invalidateQueries({
        queryKey: webhookQueryKeys.lastDelivery(appId, webhookId),
      });
    },
  });
}

/**
 * The namespaces the create/edit form offers. `schema/pull` needs the
 * collaborator role, which is also what webhooks need, so this costs no extra
 * permission.
 */
export function useWebhookNamespaces(appId: string) {
  return useQuery({
    queryKey: webhookQueryKeys.namespaces(appId),
    queryFn: async ({ signal }): Promise<string[]> => {
      const result = await apiFetch(appEndpoints.schemaPull, {
        params: { appId },
        signal,
      });
      const attrs: Record<string, InstantAttr> = {};
      for (const attribute of result.attrs) {
        attrs[attribute.id] = attribute as unknown as InstantAttr;
      }
      return sortedNamespaces(
        appSchemaFromAttrs(attrs).namespaces.map((namespace) => namespace.name),
      );
    },
    staleTime: 60_000,
  });
}
