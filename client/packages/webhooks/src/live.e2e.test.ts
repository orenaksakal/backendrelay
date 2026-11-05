import { getOps, i, id, tx } from '@instantdb/core';
import { describe, expect, it } from 'vitest';

import { Webhooks, type WebhookEventInfo } from './index.js';

const apiURI = process.env.INSTANT_API_URI;
const enabled =
  process.env.INSTANT_WEBHOOKS_LIVE_E2E === '1' && Boolean(apiURI);
const liveTest = enabled ? it : it.skip;
const schema = i.schema({
  entities: {
    goals: i.entity({
      title: i.string(),
    }),
  },
});

async function waitForEvent(
  webhooks: Webhooks<typeof schema>,
  webhookId: string,
  timeout = 20_000,
): Promise<WebhookEventInfo> {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const { events } = await webhooks.manager.listEvents(webhookId);
    if (events[0]) {
      return events[0];
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error('webhook event did not appear');
}

async function createEphemeralApp(): Promise<{
  appId: string;
  adminToken: string;
}> {
  const response = await fetch(`${apiURI}/dash/apps/ephemeral`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      title: `webhooks-sdk-e2e-${id()}`,
      schema,
      rules: {
        code: {
          goals: {
            allow: {
              view: 'true',
              create: 'true',
              update: 'true',
            },
          },
        },
      },
    }),
  });
  if (!response.ok) {
    throw new Error(
      `ephemeral app failed: ${response.status} ${await response.text()}`,
    );
  }
  const { app } = await response.json();
  return { appId: app.id, adminToken: app['admin-token'] };
}

async function createGoal(
  appId: string,
  adminToken: string,
  goalId: string,
  title: string,
): Promise<void> {
  const response = await fetch(`${apiURI}/admin/transact?app_id=${appId}`, {
    method: 'POST',
    headers: {
      authorization: `Bearer ${adminToken}`,
      'content-type': 'application/json',
    },
    body: JSON.stringify({
      steps: getOps(tx.goals[goalId].update({ title })),
      'throw-on-missing-attrs?': true,
    }),
  });
  if (!response.ok) {
    throw new Error(
      `admin transaction failed: ${response.status} ${await response.text()}`,
    );
  }
}

describe('Webhooks SDK live E2E against Rust', () => {
  liveTest(
    'manages a webhook and dispatches its native WAL payload',
    async () => {
      const { appId, adminToken } = await createEphemeralApp();
      const webhooks = new Webhooks({
        appId,
        adminToken,
        apiURI: apiURI!,
        schema,
      });
      let webhookId: string | undefined;

      try {
        const webhook = await webhooks.manager.create({
          // RFC 6666 reserves 100::/64 as a discard-only prefix. It passes the
          // public-literal SSRF check without directing CI traffic at a service.
          url: `https://[100::1]/instantdb-webhooks-e2e/${id()}`,
          namespaces: ['goals'],
          actions: ['create'],
        });
        webhookId = webhook.id;
        expect(webhook.status).toBe('active');
        expect(webhook.namespaces).toEqual(['goals']);
        expect(webhook.actions).toEqual(['create']);
        expect((await webhooks.manager.list()).map(({ id }) => id)).toContain(
          webhook.id,
        );

        const disabled = await webhooks.manager.disable(webhook.id, {
          reason: 'live gate pause',
        });
        expect(disabled.status).toBe('disabled');
        expect(disabled.disabledReason).toBe('live gate pause');
        const enabledWebhook = await webhooks.manager.enable(webhook.id);
        expect(enabledWebhook.status).toBe('active');
        expect(enabledWebhook.disabledReason).toBeNull();
        const updated = await webhooks.manager.update(webhook.id, {
          actions: ['create', 'update'],
        });
        expect(updated.actions).toEqual(['create', 'update']);

        const goalId = id();
        const title = `webhooks-live-${id()}`;
        await createGoal(appId, adminToken, goalId, title);

        const event = await waitForEvent(webhooks, webhook.id);
        const fetchedEvent = await webhooks.manager.getEvent(
          webhook.id,
          event.isn,
        );
        expect(fetchedEvent.isn).toBe(event.isn);
        expect(fetchedEvent.createdAt).toBeInstanceOf(Date);
        const payload = await webhooks.manager.getPayload(
          webhook.id,
          event.isn,
        );
        expect(payload.data).toHaveLength(1);
        expect(payload.data[0]).toMatchObject({
          namespace: 'goals',
          action: 'create',
          id: goalId,
          before: null,
          after: { id: goalId, title },
        });

        let handled = 0;
        await webhooks.processPayload(
          {
            goals: {
              create: (record) => {
                expect(record.after).toMatchObject({ id: goalId, title });
                handled += 1;
              },
            },
          },
          payload,
        );
        expect(handled).toBe(1);
      } finally {
        if (webhookId) {
          const deleted = await webhooks.manager.delete(webhookId);
          expect(deleted.id).toBe(webhookId);
        }
      }
    },
    60_000,
  );
});
