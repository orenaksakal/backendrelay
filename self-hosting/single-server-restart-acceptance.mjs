#!/usr/bin/env node
import { execFile } from "node:child_process";
import { randomUUID } from "node:crypto";
import { promisify } from "node:util";
import { recoveryCommands, recoveryReceipt } from './recovery-drill.mjs';

import { init } from "../client/packages/admin/dist/esm/index.js";

const execFileAsync = promisify(execFile);
const apiOrigin = required("INSTANT_RESTART_API_URL");
const appId = required("INSTANT_RESTART_APP_ID");
const adminToken = required("INSTANT_RESTART_ADMIN_TOKEN");
const composeFile = required("INSTANT_RESTART_COMPOSE_FILE");
const composeProject = required("COMPOSE_PROJECT_NAME");
const timeoutMs = Number(process.env.INSTANT_RESTART_TIMEOUT_MS || 120_000);
const mode = process.env.INSTANT_RESTART_MODE || 'restart';
if (!Number.isInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 600_000) throw new Error('Recovery timeout must be 1..600000 ms');
const faults = recoveryCommands(mode, composeProject, composeFile, apiOrigin);
let recoveryDeadline;
const rowId = randomUUID();
const marker = `single-server-restart-${randomUUID()}`;

const db = init({
  appId,
  adminToken,
  apiURI: apiOrigin,
  disableValidation: true,
});
const subscription = db.subscribeQuery({ restart_acceptance: {} });
const iterator = subscription[Symbol.asyncIterator]();

try {
  await db.transact(
    db.tx.restart_acceptance[rowId].update({ marker, phase: "before-restart" }),
  );
  assertRow(await db.query({ restart_acceptance: {} }), "before-restart");
  await nextRow(iterator, "before-restart");

  const postgresContainer = await composeContainerId("postgres");
  // Default remains docker compose restart --timeout 10 server.
  process.stdout.write(`==> isolated recovery: ${mode}\n`);
  const recoveryStarted = performance.now();
  recoveryDeadline = Date.now() + timeoutMs;
  for (const args of faults) {
    await execFileAsync('docker', args, { timeout: Math.max(1, recoveryDeadline - Date.now()), maxBuffer: 65_536 });
  }
  await waitForServer();

  if ((await composeContainerId("postgres")) !== postgresContainer) {
    throw new Error("PostgreSQL container changed during the server-only restart");
  }
  assertRow(await withTimeout(db.query({ restart_acceptance: {} }), recoveryDeadline - Date.now()), "before-restart");

  await withTimeout(db.transact(
    db.tx.restart_acceptance[rowId].update({ marker, phase: "after-restart" }),
  ), recoveryDeadline - Date.now());
  await nextRow(iterator, "after-restart");
  assertRow(await withTimeout(db.query({ restart_acceptance: {} }), recoveryDeadline - Date.now()), "after-restart");
  process.stdout.write(JSON.stringify(recoveryReceipt(mode, performance.now() - recoveryStarted, timeoutMs)) + '\n');
} finally {
  subscription.close();
}

process.stdout.write("Single-server restart persistence and realtime acceptance passed\n");

async function composeContainerId(service) {
  const { stdout } = await execFileAsync("docker", [
    "compose",
    "--project-name",
    composeProject,
    "--file",
    composeFile,
    "ps",
    "--quiet",
    service,
  ], { timeout: Math.max(1, (recoveryDeadline ?? Date.now() + timeoutMs) - Date.now()), maxBuffer: 65_536 });
  const id = stdout.trim();
  if (!id) throw new Error(`${service} container is not running`);
  return id;
}

async function waitForServer() {
  const deadline = recoveryDeadline ?? Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`${apiOrigin}/health/system`, {
        signal: AbortSignal.timeout(2_000),
      });
      if (response.ok && (await response.json()).wal === "ok") return;
    } catch {}
    await sleep(1_000);
  }
  throw new Error("server did not become healthy after restart");
}

async function nextRow(iterator, phase) {
  const deadline = recoveryDeadline ?? Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const result = await withTimeout(iterator.next(), deadline - Date.now());
    if (result.done) throw new Error("realtime subscription closed during restart");
    if (result.value?.type === "ok" && containsRow(result.value.data, phase)) {
      return;
    }
    if (result.value?.type === "error" && result.value.isClosed) {
      throw new Error("realtime subscription permanently closed during restart");
    }
  }
  throw new Error(`realtime subscription did not deliver phase ${phase}`);
}

function assertRow(data, phase) {
  if (!containsRow(data, phase)) {
    throw new Error(`query did not return persisted phase ${phase}`);
  }
}

function containsRow(data, phase) {
  return data.restart_acceptance?.some(
    (row) => row.id === rowId && row.marker === marker && row.phase === phase,
  );
}

function withTimeout(promise, duration) {
  let timer;
  return Promise.race([
    promise,
    new Promise((_, reject) => {
      timer = setTimeout(
        () => reject(new Error("realtime subscription timed out")),
        Math.max(1, duration),
      );
    }),
  ]).finally(() => clearTimeout(timer));
}

function required(name) {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required`);
  return value;
}

function sleep(duration) {
  return new Promise((resolve) => setTimeout(resolve, duration));
}
