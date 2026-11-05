#!/usr/bin/env node
// P7.10 abuse and isolation drill.
//
// Creates two throwaway apps on staging, measures app B's own read latency,
// then hammers app A (magic-code sends, guest sign-ups, oversize and HTML
// uploads, expensive queries, and many sessions from one public IP) while B
// keeps reading. Every limit is compared with the value the server was
// configured with, and the evidence is written as JSON without tokens.
//
// Alerts and email-provider counts are operator evidence: they need the
// monitoring overlay or the provider console, so the script records the attack
// window for those queries instead of guessing.
import assert from "node:assert/strict";
import { randomBytes, randomUUID } from "node:crypto";
import { mkdir, writeFile } from "node:fs/promises";
import http from "node:http";
import https from "node:https";
import { homedir } from "node:os";
import { join } from "node:path";

import { describeRun, latencyHolds, limitHolds, summary } from "./abuse-model.mjs";

const settings = readSettings();
const { apiOrigin, limits, stamp, stateDir } = settings;
const {
  baselineSeconds,
  hammerSeconds,
  readConcurrency,
  queryConcurrency,
  guestSignups,
  seedRows,
  p95Factor,
  p95FloorMs,
} = settings;

const evidence = {
  schemaVersion: 1,
  startedAt: new Date().toISOString(),
  apiOrigin: apiOrigin.origin,
  limits,
  apps: {},
  baseline: null,
  during: null,
  attacks: {},
  attackWindow: null,
  checks: [],
};

try {
  await drill();
} catch (error) {
  console.error(`abuse drill aborted: ${error.message}`);
  process.exitCode = 2;
}

function readSettings() {
  try {
    const origin = new URL(required("ABUSE_API_ORIGIN").replace(/\/+$/, ""));
    guardOrigin(origin);
    const stamp = new Date().toISOString().replace(/[:.]/g, "-");
    return {
      apiOrigin: origin,
      stamp,
      stateDir:
        process.env.ABUSE_STATE_DIR ??
        join(
          process.env.XDG_STATE_HOME ?? join(homedir(), ".local/state"),
          "backendrelay-abuse",
          stamp,
        ),
      limits: {
        emailSends: integer("ABUSE_EXPECTED_EMAIL_SEND_LIMIT", 20),
        ipSends: integer("ABUSE_EXPECTED_IP_SEND_LIMIT", 30),
        sessionsPerIp: integer("ABUSE_EXPECTED_SESSIONS_PER_IP", 100),
        maxUploadBytes: integer("ABUSE_MAX_UPLOAD_BYTES", 1024 * 1024),
        queryBytes: integer("ABUSE_EXPECTED_QUERY_BYTES", 256 * 1024),
        queryCeilingMs: integer("ABUSE_QUERY_CEILING_MS", 30_000),
      },
      baselineSeconds: integer("ABUSE_BASELINE_SECONDS", 30),
      hammerSeconds: integer("ABUSE_HAMMER_SECONDS", 60),
      readConcurrency: integer("ABUSE_READ_CONCURRENCY", 4),
      queryConcurrency: integer("ABUSE_QUERY_CONCURRENCY", 8),
      guestSignups: integer("ABUSE_GUEST_SIGNUPS", 200),
      seedRows: integer("ABUSE_SEED_ROWS", 2000),
      p95Factor: decimal("ABUSE_P95_FACTOR", 1.5),
      p95FloorMs: integer("ABUSE_P95_FLOOR_MS", 250),
    };
  } catch (error) {
    console.error(`abuse drill aborted: ${error.message}`);
    process.exit(2);
  }
}

async function drill() {
  const appA = await createEphemeralApp("a");
  const appB = await createEphemeralApp("b");
  evidence.apps = {
    a: { id: appA.id, title: appA.title, seededRows: seedRows },
    b: { id: appB.id, title: appB.title, seededRows: 10 },
  };
  console.log(`app A: ${appA.id}`);
  console.log(`app B: ${appB.id}`);

  await seedApp(appA, seedRows);
  await seedApp(appB, 10);

  // Limits that do not need load. Run them first so the load measurements are
  // not disturbed by a fresh session or a huge request body.
  const bodyLimit = await preAuthBodyLimit();
  record("Pre-auth body", bodyLimit.status === 413, {
    expected: "413 for 1 MiB + 1 byte before authentication",
    measured: `HTTP ${bodyLimit.status}`,
  });

  const queryBytes = await queryByteLimit(appA);
  record("InstaQL query size", queryBytes.type === "parameter-limit-exceeded", {
    expected: `parameter-limit-exceeded above ${limits.queryBytes} bytes`,
    measured: `type ${queryBytes.type}`,
  });

  console.log(`measuring app B baseline for ${baselineSeconds}s`);
  evidence.baseline = await measureReads(appB, baselineSeconds);
  console.log(`baseline app B p95: ${formatMs(evidence.baseline.p95Ms)}`);

  const windowStart = new Date().toISOString();
  const started = performance.now();
  const holdUntil = started + hammerSeconds * 1000;
  const attacks = Promise.all([
    magicCodeSends(appA),
    guestSignUpBurst(appA),
    oversizedUpload(appA),
    htmlUpload(appA),
    expensiveQueries(appA),
    sessionCap(appA, holdUntil),
  ]);
  const during = measureReads(appB, hammerSeconds);
  const [attackResults, duringReads] = await Promise.all([attacks, during]);
  const elapsed = performance.now() - started;
  evidence.attackWindow = {
    startedAt: windowStart,
    endedAt: new Date().toISOString(),
    seconds: Math.round(elapsed / 1000),
  };
  evidence.during = duringReads;

  const [magic, guests, oversize, html, queries, sessions] = attackResults;

  const sameEmailOk =
    magic.sameEmailAccepted === limits.emailSends &&
    magic.sameEmailRefusal?.status === 429 &&
    magic.sameEmailRefusal?.type === "rate-limited";
  record("Magic-code sends to one address", sameEmailOk, {
    expected: `exactly ${limits.emailSends} accepted, then 429 rate-limited`,
    measured: `${magic.sameEmailAccepted} accepted, then ${describeRefusal(magic.sameEmailRefusal)}`,
    note:
      magic.sameEmailAccepted === limits.emailSends
        ? "fresh per-address bucket"
        : "the configured per-address limit differs from ABUSE_EXPECTED_EMAIL_SEND_LIMIT",
  });

  const perIpOk =
    magic.ipAccepted === limits.ipSends &&
    magic.ipRefusal?.status === 429 &&
    magic.ipRefusal?.type === "rate-limited";
  record("Magic-code sends from one IP", perIpOk, {
    expected: `exactly ${limits.ipSends} accepted, then 429 rate-limited`,
    measured: `${magic.ipAccepted} accepted, then ${describeRefusal(magic.ipRefusal)}`,
    note:
      magic.ipAccepted === limits.ipSends
        ? "fresh per-IP bucket for this app"
        : "the configured per-IP limit differs from ABUSE_EXPECTED_IP_SEND_LIMIT",
  });

  record("Oversize upload", oversize.status === 413, {
    expected: `413 for a ${limits.maxUploadBytes + 1} byte upload`,
    measured: `HTTP ${oversize.status}`,
  });

  const htmlStored = html.uploadStatus >= 200 && html.uploadStatus < 300;
  const htmlAttachment = /^attachment\b/i.test(html.contentDisposition ?? "");
  const htmlNotInline = !/^text\/html\b/i.test(html.contentType ?? "");
  record("HTML upload", htmlStored && htmlAttachment && htmlNotInline, {
    expected: "2xx upload, attachment disposition, no text/html content type",
    measured: `upload ${html.uploadStatus}, content-disposition ${html.contentDisposition ?? "missing"}, content-type ${html.contentType ?? "missing"}`,
  });

  record("Guest sign-up burst", guests.fiveXx === 0 && guests.accepted > 0, {
    expected: "at least one 2xx and no 5xx; no dedicated guest-sign-up quota exists",
    measured: `${guests.accepted} of ${guests.attempted} accepted, statuses ${JSON.stringify(guests.statuses)}, p95 ${formatMs(guests.p95Ms)}`,
  });

  record("Expensive queries", queries.success > 0 && queries.maxMs <= limits.queryCeilingMs, {
    expected: `at least one 2xx and no request above ${limits.queryCeilingMs} ms`,
    measured: `${queries.success} of ${queries.count} succeeded, statuses ${JSON.stringify(queries.statuses)}, max ${formatMs(queries.maxMs)}`,
  });

  const perSession = limitHolds({
    accepted: sessions.accepted,
    limit: limits.sessionsPerIp,
    refusal: sessions.refusal,
  });
  record("Sessions from one IP", perSession.ok, {
    expected: `at most ${limits.sessionsPerIp} live sessions, then 429 rate-limited`,
    measured: `${sessions.accepted} opened, then ${describeRefusal(sessions.refusal)}`,
    note: perSession.reason,
  });

  const bP95 = latencyHolds(evidence.baseline, duringReads, {
    factor: p95Factor,
    floorMs: p95FloorMs,
  });
  record("App B p95", bP95.ok, {
    expected: `within ${p95Factor}x its baseline or ${p95FloorMs} ms: ${formatMs(evidence.baseline.p95Ms)} baseline`,
    measured: `${formatMs(duringReads.p95Ms)} p95`,
    note: bP95.reason,
  });
  record("App B read errors", duringReads.errors.length === 0, {
    expected: "no non-2xx admin reads",
    measured: `${duringReads.errors.length} errors`,
  });

  evidence.finishedAt = new Date().toISOString();
  const verdict = describeRun(evidence);
  evidence.result = verdict.result;

  await mkdir(stateDir, { recursive: true, mode: 0o700 });
  const resultPath = join(stateDir, "result.json");
  await writeFile(resultPath, `${JSON.stringify(evidence, null, 2)}\n`, { mode: 0o600 });

  for (const check of evidence.checks) {
    const label = check.ok ? "ok  " : "FAIL";
    const note = check.note ? `; ${check.note}` : "";
    console.log(`${label} ${check.name}: ${check.measured} (expected ${check.expected})${note}`);
  }
  console.log(`attack window: ${evidence.attackWindow.startedAt} .. ${evidence.attackWindow.endedAt}`);
  console.log(`result: ${verdict.result} (${verdict.total - verdict.failed.length}/${verdict.total} checks)`);
  console.log(`evidence: ${resultPath}`);
  console.log("Markdown rows for docs/operations.md#abuse-and-isolation-drills:");
  for (const check of evidence.checks) {
    console.log(
      `| ${markdownCell(check.name)} | ${markdownCell(check.expected)} | ${markdownCell(check.measured)} | ${check.ok ? "pass" : "fail"} | ${markdownCell(check.note ?? "")} |`,
    );
  }
  if (verdict.result !== "passed") {
    process.exitCode = 1;
  }
}

function record(name, ok, { expected, measured, note }) {
  evidence.checks.push({ name, ok, expected, measured, note: note ?? null });
}

async function createEphemeralApp(label) {
  const title = `abuse-drill-${label}-${stamp}`;
  const response = await call("POST", "/dash/apps/ephemeral", { body: { title } });
  if (response.status !== 200) {
    throw new Error(`creating app ${label} failed with HTTP ${response.status}: ${preview(response.text)}`);
  }
  const app = response.body?.app;
  assert.ok(app?.id, `the ${label} app response is missing app.id`);
  assert.ok(app?.["admin-token"], `the ${label} app response is missing admin-token`);
  return { id: app.id, title, token: app["admin-token"] };
}

async function seedApp(app, rows) {
  const chunk = 500;
  for (let offset = 0; offset < rows; offset += chunk) {
    const steps = [];
    for (let index = offset; index < Math.min(offset + chunk, rows); index += 1) {
      steps.push(["update", "abuse_rows", randomUUID(), { note: `seed-${index}` }]);
    }
    const response = await call("POST", "/admin/transact", {
      appId: app.id,
      token: app.token,
      body: { steps },
      timeoutMs: 60_000,
    });
    if (response.status !== 200) {
      throw new Error(`seeding app ${app.id} failed with HTTP ${response.status}: ${preview(response.text)}`);
    }
  }
}

async function preAuthBodyLimit() {
  const body = Buffer.alloc(1024 * 1024 + 1, "a");
  const response = await call("POST", "/dash/apps", {
    body,
    headers: { "content-type": "application/json" },
    timeoutMs: 60_000,
  });
  return { status: response.status };
}

async function queryByteLimit(app) {
  const client = await openWebSocket(app.id, "query-byte-limit");
  try {
    const init = await initialize(client, app);
    if (init.op !== "init-ok") {
      throw new Error(`the query-byte-limit session did not initialize: ${preview(JSON.stringify(init))}`);
    }
    client.send({
      op: "add-query",
      q: {
        abuse_rows: { $: { limit: 1 } },
        pad: "x".repeat(limits.queryBytes + 1024),
      },
      "client-event-id": randomUUID(),
    });
    const message = await client.waitFor(
      (value) => value.op === "error" || value.op === "add-query-ok",
      "the oversized query result",
    );
    return { type: message.op === "error" ? message.type : "accepted" };
  } finally {
    await client.close();
  }
}

async function measureReads(app, seconds) {
  const deadline = Date.now() + seconds * 1000;
  const samples = [];
  const errors = [];
  await Promise.all(
    Array.from({ length: readConcurrency }, async () => {
      while (Date.now() < deadline) {
        const started = performance.now();
        try {
          const response = await call("POST", "/admin/query", {
            appId: app.id,
            token: app.token,
            body: { query: { abuse_rows: { $: { limit: 10 } } } },
            timeoutMs: 30_000,
          });
          samples.push(performance.now() - started);
          if (response.status !== 200) {
            errors.push(response.status);
          }
        } catch (error) {
          errors.push(error.message);
        }
      }
    }),
  );
  return { ...summary(samples), errors };
}

async function magicCodeSends(app) {
  const sameEmail = `abuse-drill-shared-${stamp}@example.com`;
  let sameEmailAccepted = 0;
  let sameEmailRefusal = null;
  for (let attempt = 0; attempt <= limits.emailSends; attempt += 1) {
    const refusal = await sendMagicCode(app, sameEmail);
    if (refusal === null) {
      sameEmailAccepted += 1;
    } else {
      sameEmailRefusal = refusal;
      break;
    }
  }

  let ipAccepted = sameEmailAccepted;
  let ipRefusal = null;
  for (let attempt = 0; attempt < limits.ipSends + 5; attempt += 1) {
    const refusal = await sendMagicCode(app, `abuse-drill-${attempt}-${stamp}@example.com`);
    if (refusal === null) {
      ipAccepted += 1;
    } else {
      ipRefusal = refusal;
      break;
    }
  }

  const result = {
    sameEmailAccepted,
    sameEmailRefusal,
    ipAccepted,
    ipRefusal,
  };
  evidence.attacks.magicCodes = result;
  return result;
}

async function sendMagicCode(app, email) {
  const response = await call("POST", "/runtime/auth/send_magic_code", {
    body: { "app-id": app.id, email },
    timeoutMs: 60_000,
  });
  if (response.status === 200) {
    return null;
  }
  return { status: response.status, type: response.body?.type ?? null };
}

async function guestSignUpBurst(app) {
  const statuses = {};
  const samples = [];
  let accepted = 0;
  let fiveXx = 0;
  const concurrency = 4;
  for (let offset = 0; offset < guestSignups; offset += concurrency) {
    const batch = [];
    for (let index = offset; index < Math.min(offset + concurrency, guestSignups); index += 1) {
      batch.push(
        (async () => {
          const started = performance.now();
          try {
            const response = await call("POST", "/runtime/auth/sign_in_guest", {
              body: { "app-id": app.id },
              timeoutMs: 60_000,
            });
            samples.push(performance.now() - started);
            statuses[response.status] = (statuses[response.status] ?? 0) + 1;
            if (response.status >= 200 && response.status < 300) {
              accepted += 1;
            }
            if (response.status >= 500) {
              fiveXx += 1;
            }
          } catch {
            samples.push(performance.now() - started);
            statuses.error = (statuses.error ?? 0) + 1;
            fiveXx += 1;
          }
        })(),
      );
    }
    await Promise.all(batch);
  }
  const result = { attempted: guestSignups, accepted, statuses, fiveXx, p95Ms: summary(samples).p95Ms };
  evidence.attacks.guestSignups = result;
  return result;
}

async function oversizedUpload(app) {
  const body = Buffer.alloc(limits.maxUploadBytes + 1, 0x61);
  const response = await call(
    "PUT",
    `/admin/storage/upload?path=${encodeURIComponent(`abuse-drill/${randomUUID()}-oversize.bin`)}`,
    {
      appId: app.id,
      token: app.token,
      body,
      headers: { "content-type": "application/octet-stream" },
      timeoutMs: 120_000,
    },
  );
  const result = { status: response.status };
  evidence.attacks.oversizedUpload = result;
  return result;
}

async function htmlUpload(app) {
  const path = `abuse-drill/${randomUUID()}.html`;
  const upload = await call("PUT", `/admin/storage/upload?path=${encodeURIComponent(path)}`, {
    appId: app.id,
    token: app.token,
    body: "<!doctype html><script>document.write('abuse drill')</script>",
    headers: { "content-type": "text/html" },
    timeoutMs: 60_000,
  });
  const result = {
    uploadStatus: upload.status,
    contentType: null,
    contentDisposition: null,
  };
  if (upload.status >= 200 && upload.status < 300) {
    const signed = await call(
      "GET",
      `/admin/storage/signed-download-url?filename=${encodeURIComponent(path)}`,
      { appId: app.id, token: app.token },
    );
    const url = signed.body?.data;
    if (typeof url === "string") {
      const download = await fetch(url, {
        redirect: "follow",
        signal: AbortSignal.timeout(30_000),
      });
      // Drain the body; only the headers matter here.
      await download.arrayBuffer();
      result.contentType = download.headers.get("content-type");
      result.contentDisposition = download.headers.get("content-disposition");
    }
  }
  evidence.attacks.htmlUpload = result;
  return result;
}

async function expensiveQueries(app) {
  const deadline = Date.now() + hammerSeconds * 1000;
  const samples = [];
  const statuses = {};
  let count = 0;
  let success = 0;
  await Promise.all(
    Array.from({ length: queryConcurrency }, async () => {
      while (Date.now() < deadline) {
        const started = performance.now();
        try {
          const response = await call("POST", "/admin/query", {
            appId: app.id,
            token: app.token,
            body: { query: { abuse_rows: { $: { limit: 1000 } } } },
            timeoutMs: 60_000,
          });
          samples.push(performance.now() - started);
          count += 1;
          statuses[response.status] = (statuses[response.status] ?? 0) + 1;
          if (response.status >= 200 && response.status < 300) {
            success += 1;
          }
        } catch (error) {
          count += 1;
          statuses.error = (statuses.error ?? 0) + 1;
          samples.push(performance.now() - started);
        }
      }
    }),
  );
  const result = { count, success, statuses, ...summary(samples) };
  evidence.attacks.expensiveQueries = result;
  return result;
}

async function sessionCap(app, holdUntil) {
  const sockets = [];
  let accepted = 0;
  let refusal = null;
  for (let attempt = 0; attempt <= limits.sessionsPerIp; attempt += 1) {
    const opened = await openRawSession(app.id);
    if (opened.ok) {
      accepted += 1;
      sockets.push(opened.socket);
      continue;
    }
    refusal = opened;
    break;
  }

  // Hold the sockets for the rest of the hammer window so the cap is exercised
  // for as long as app B is being measured, not for a single instant.
  await sleep(Math.max(0, holdUntil - Date.now()));
  for (const socket of sockets) {
    socket.destroy();
  }
  const result = {
    accepted,
    refusal:
      refusal === null
        ? null
        : { status: refusal.status ?? null, type: refusal.type ?? null, error: refusal.error ?? null },
  };
  evidence.attacks.sessions = result;
  return result;
}

async function openRawSession(appId) {
  const url = new URL("/runtime/session", apiOrigin);
  url.searchParams.set("app_id", appId);
  const transport = url.protocol === "https:" ? https : http;
  return new Promise((resolve) => {
    let settled = false;
    const finish = (value) => {
      if (!settled) {
        settled = true;
        resolve(value);
      }
    };
    const request = transport.request({
      hostname: url.hostname,
      port: url.port || undefined,
      path: `${url.pathname}${url.search}`,
      method: "GET",
      agent: false,
      headers: {
        Connection: "Upgrade",
        Upgrade: "websocket",
        Origin: apiOrigin.origin,
        "Sec-WebSocket-Version": "13",
        "Sec-WebSocket-Key": randomBytes(16).toString("base64"),
      },
    });
    request.setTimeout(15_000, () => {
      request.destroy();
      finish({ ok: false, status: null, error: "timeout" });
    });
    request.on("upgrade", (response, socket) => {
      socket.on("error", () => {});
      finish({ ok: true, status: response.statusCode, socket });
    });
    request.on("response", (response) => {
      let text = "";
      response.setEncoding("utf8");
      response.on("data", (chunk) => {
        text += chunk;
      });
      response.on("end", () => {
        const body = parseJson(text);
        finish({ ok: false, status: response.statusCode, type: body?.type ?? null, text: preview(text) });
      });
      response.on("error", () => finish({ ok: false, status: response.statusCode, text: preview(text) }));
    });
    request.on("error", (error) => finish({ ok: false, status: null, error: error.message }));
    request.end();
  });
}

class WebSocketClient {
  constructor(socket, label) {
    this.socket = socket;
    this.label = label;
    this.inbox = [];
    this.failure = undefined;
    socket.addEventListener("message", (event) => {
      if (typeof event.data === "string") {
        const parsed = parseJson(event.data);
        if (parsed !== undefined) {
          this.inbox.push(parsed);
        }
      }
    });
    socket.addEventListener("error", () => {
      this.failure = new Error(`${label} websocket reported an error`);
    });
  }

  static async open(appId, label) {
    const socket = new WebSocket(
      `${apiOrigin.origin.replace(/^http/i, "ws")}/runtime/session?app_id=${encodeURIComponent(appId)}`,
    );
    const client = new WebSocketClient(socket, label);
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`${label} websocket did not open`)), 10_000);
      socket.addEventListener(
        "open",
        () => {
          clearTimeout(timer);
          resolve();
        },
        { once: true },
      );
      socket.addEventListener(
        "error",
        () => {
          clearTimeout(timer);
          reject(new Error(`${label} websocket failed to open`));
        },
        { once: true },
      );
    });
    return client;
  }

  send(message) {
    this.socket.send(JSON.stringify(message));
  }

  async waitFor(predicate, label, timeoutMs = 10_000) {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const index = this.inbox.findIndex(predicate);
      if (index !== -1) {
        return this.inbox.splice(index, 1)[0];
      }
      if (this.failure !== undefined) {
        throw this.failure;
      }
      if (Date.now() >= deadline) {
        throw new Error(`${this.label}: timed out waiting for ${label}`);
      }
      await sleep(25);
    }
  }

  async close() {
    try {
      this.socket.close();
    } catch {
      // A socket that already failed is not interesting here.
    }
    await sleep(0);
  }
}

async function openWebSocket(appId, label) {
  return WebSocketClient.open(appId, label);
}

async function initialize(client, app) {
  client.send({ op: "init", "app-id": app.id, "__admin-token": app.token });
  return client.waitFor(
    (value) => value.op === "init-ok" || value.op === "error",
    "init response",
  );
}

async function call(method, path, options = {}) {
  const headers = { ...(options.headers ?? {}) };
  if (options.appId) {
    headers["app-id"] = options.appId;
  }
  if (options.token) {
    headers.authorization = `Bearer ${options.token}`;
  }
  let body;
  if (options.body !== undefined) {
    if (typeof options.body === "string" || options.body instanceof Uint8Array) {
      body = options.body;
    } else {
      headers["content-type"] ??= "application/json";
      body = JSON.stringify(options.body);
    }
  }
  const response = await fetch(`${apiOrigin.origin}${path}`, {
    method,
    headers,
    body,
    redirect: "manual",
    signal: AbortSignal.timeout(options.timeoutMs ?? 30_000),
  });
  const text = await response.text();
  return { status: response.status, text, body: parseJson(text) };
}

function guardOrigin(origin) {
  if (origin.protocol !== "https:" && origin.protocol !== "http:") {
    throw new Error("ABUSE_API_ORIGIN must be an absolute http(s) URL");
  }
  if (origin.username || origin.password || origin.search || origin.hash) {
    throw new Error("ABUSE_API_ORIGIN must not carry credentials, a query or a fragment");
  }
  const loopback = ["127.0.0.1", "::1", "localhost"].includes(origin.hostname);
  if (loopback && process.env.ABUSE_ALLOW_LOOPBACK !== "true") {
    throw new Error("refusing a loopback origin; set ABUSE_ALLOW_LOOPBACK=true only for a local smoke run");
  }
  if (origin.hostname.startsWith("api.") && process.env.ABUSE_ALLOW_PRODUCTION !== "true") {
    throw new Error("refusing a production api. hostname; run the drill against staging-api. instead");
  }
}

function required(name) {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${name} is required`);
  }
  return value;
}

function integer(name, fallback) {
  const raw = process.env[name];
  if (raw === undefined || raw.trim() === "") {
    return fallback;
  }
  if (!/^[1-9][0-9]*$/.test(raw.trim())) {
    throw new Error(`${name} must be a positive integer`);
  }
  return Number(raw.trim());
}

function decimal(name, fallback) {
  const raw = process.env[name];
  if (raw === undefined || raw.trim() === "") {
    return fallback;
  }
  const value = Number(raw.trim());
  if (!Number.isFinite(value) || value <= 0) {
    throw new Error(`${name} must be a positive number`);
  }
  return value;
}

function describeRefusal(refusal) {
  if (refusal === null || refusal === undefined) {
    return "no refusal";
  }
  if (refusal.error) {
    return `transport error ${refusal.error}`;
  }
  return `HTTP ${refusal.status}${refusal.type ? ` ${refusal.type}` : ""}`;
}

function formatMs(value) {
  return Number.isFinite(value) ? `${value.toFixed(1)} ms` : "unmeasured";
}

function markdownCell(value) {
  return String(value ?? "").replace(/\|/g, "\\|").replace(/\s+/g, " ").trim();
}

function preview(text) {
  return typeof text === "string" && text.length > 300 ? `${text.slice(0, 300)}...` : text;
}

function parseJson(text) {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

function sleep(duration) {
  return new Promise((resolve) => setTimeout(resolve, duration));
}
