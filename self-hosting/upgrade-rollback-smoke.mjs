#!/usr/bin/env node
// Minimal Admin SDK smoke for the upgrade/rollback acceptance. The official
// SDK smoke harness (run-official-sdk-e2e.sh) always creates its own Compose
// stack, so it cannot target the both-images stack this script drives.
// This checks seeded rows through the Admin SDK, writes a fresh row, and waits
// for a realtime subscription to deliver it.
import { randomUUID } from "node:crypto";

import { init } from "../client/packages/admin/dist/esm/index.js";

const apiURI = required("INSTANT_UPGRADE_API_URL");
const appId = required("INSTANT_UPGRADE_APP_ID");
const adminToken = required("INSTANT_UPGRADE_ADMIN_TOKEN");
const stage = required("INSTANT_UPGRADE_STAGE");
const seedMarker = required("INSTANT_UPGRADE_SEED_MARKER");
const minSeededRows = Number(process.env.INSTANT_UPGRADE_MIN_SEEDED_ROWS || "1");
const timeoutMs = Number(process.env.INSTANT_UPGRADE_SMOKE_TIMEOUT_MS || 60_000);
if (!Number.isInteger(minSeededRows) || minSeededRows < 1) {
  throw new Error("INSTANT_UPGRADE_MIN_SEEDED_ROWS must be a positive integer");
}
if (!Number.isInteger(timeoutMs) || timeoutMs < 1) {
  throw new Error("INSTANT_UPGRADE_SMOKE_TIMEOUT_MS must be a positive integer");
}

const db = init({
  appId,
  adminToken,
  apiURI,
  disableValidation: true,
});
const rowId = randomUUID();
const marker = `smoke-${stage}-${randomUUID()}`;
const subscription = db.subscribeQuery({ upgrade_rollback_items: {} });
const iterator = subscription[Symbol.asyncIterator]();
const deadline = Date.now() + timeoutMs;

try {
  const seeded = await withTimeout(
    db.query({ upgrade_rollback_items: {} }),
    deadline,
  );
  const rows = seeded.upgrade_rollback_items ?? [];
  const seededRows = rows.filter((row) => row.marker === seedMarker);
  if (seededRows.length < minSeededRows) {
    throw new Error(
      `Admin read returned ${seededRows.length} seeded rows, expected ${minSeededRows}`,
    );
  }

  await withTimeout(
    db.transact(
      db.tx.upgrade_rollback_items[rowId].update({
        marker,
        seq: `smoke-${stage}`,
      }),
    ),
    deadline,
  );
  await nextRow(iterator, marker, deadline);

  const reread = await withTimeout(
    db.query({ upgrade_rollback_items: {} }),
    deadline,
  );
  if (!(reread.upgrade_rollback_items ?? []).some((row) => row.marker === marker)) {
    throw new Error("Admin read did not return the fresh smoke row");
  }

  process.stdout.write(
    `stage=${stage} seeded=${seededRows.length} wrote=${marker}\n`,
  );
} finally {
  subscription.close();
}

process.stdout.write(`Upgrade/rollback Admin SDK smoke passed for ${stage}\n`);

async function nextRow(iterator, marker, deadline) {
  while (Date.now() < deadline) {
    const result = await withTimeout(iterator.next(), deadline);
    if (result.done) {
      throw new Error("realtime subscription closed before delivering the smoke row");
    }
    const value = result.value;
    if (value?.type === "error" && value.isClosed) {
      throw new Error("realtime subscription permanently closed during the smoke");
    }
    if (
      value?.type === "ok" &&
      (value.data?.upgrade_rollback_items ?? []).some((row) => row.marker === marker)
    ) {
      return;
    }
  }
  throw new Error("realtime subscription did not deliver the fresh smoke row");
}

function withTimeout(promise, deadline) {
  let timer;
  return Promise.race([
    promise,
    new Promise((_, reject) => {
      timer = setTimeout(
        () => reject(new Error("upgrade/rollback smoke timed out")),
        Math.max(1, deadline - Date.now()),
      );
    }),
  ]).finally(() => clearTimeout(timer));
}

function required(name) {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required`);
  return value;
}
