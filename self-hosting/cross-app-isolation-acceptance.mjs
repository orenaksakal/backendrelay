#!/usr/bin/env node
// Black-box cross-app isolation acceptance against the real server, PostgreSQL,
// and RustFS stack. Every check spends an app A credential against app B and
// must fail without returning any of B's data. Raw HTTP and WebSocket frames are
// used on purpose so the test pins the public wire contract, not an SDK.
import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { after, before, test } from "node:test";

const apiOrigin = required("INSTANT_API_ORIGIN").replace(/\/+$/, "");
const s3Origin = required("INSTANT_S3_ORIGIN").replace(/\/+$/, "");
const realtimeOrigin = apiOrigin.replace(/^http/i, "ws");
const CONFIG_APP_ID = "24a4d71b-7bb2-4630-9aee-01146af26239";
const ROOM_TYPE = "cross-app-room-type";
const apps = {};
const sockets = new Set();

before(async () => {
  await check("setup: ephemeral apps A and B have users, rows and files", async () => {
    apps.A = await provisionApp("A");
    apps.B = await provisionApp("B");
  });
});

after(async () => {
  for (const client of [...sockets]) {
    await client.close();
  }
});

test("A's own credentials work inside app A", async () => {
  await check("POST /admin/query returns A's row", async () => {
    const response = await request("POST", "/admin/query", {
      appId: apps.A.id,
      token: apps.A.adminToken,
      body: { query: { isolation_rows: {} } },
    });
    const body = expectOk(response, "A admin query on A");
    assert.match(JSON.stringify(body), new RegExp(apps.A.marker), "A's query did not return A's row");
  });

  await check("POST /admin/transact writes A's row", async () => {
    const response = await request("POST", "/admin/transact", {
      appId: apps.A.id,
      token: apps.A.adminToken,
      body: { steps: [["update", "isolation_rows", randomUUID(), { note: `${apps.A.marker}-second` }]] },
    });
    const body = expectOk(response, "A admin transact on A");
    assert.equal(typeof body?.["tx-id"], "number", "A's transaction response has no tx-id");
  });

  await check("GET /admin/users returns A's user", async () => {
    const response = await request(
      "GET",
      `/admin/users?email=${encodeURIComponent(apps.A.email)}`,
      { appId: apps.A.id, token: apps.A.adminToken },
    );
    const body = expectOk(response, "A admin users on A");
    assert.equal(body?.user?.id, apps.A.user.id, "A's user lookup returned a different user");
  });

  await check("GET /admin/storage/files lists A's file", async () => {
    const listed = await listFiles(apps.A);
    assert.ok(
      listed.some((file) => file.name === apps.A.filePath),
      "A's file listing is missing A's file",
    );
  });

  await check("GET /admin/storage/signed-download-url signs A's file", async () => {
    const url = await signedDownloadUrl(apps.A);
    assert.equal(new URL(url).origin, s3Origin, "the signed URL does not use the public S3 endpoint");
  });
});

test("A's refresh token cannot authenticate a session for app B", async () => {
  await check("POST /runtime/auth/verify_refresh_token rejects A's token on B", async () => {
    const response = await request("POST", "/runtime/auth/verify_refresh_token", {
      body: { "app-id": apps.B.id, "refresh-token": apps.A.user.refresh_token },
    });
    expectRejected(response, "A refresh token on B runtime auth", appSecrets(apps.B));
  });

  await check("WebSocket init rejects A's refresh token on B", async () => {
    const client = await Realtime.open(apps.B.id, "B init");
    try {
      const result = await initialize(client, apps.B, {
        refreshToken: apps.A.user.refresh_token,
      });
      assert.equal(result.op, "error", "A's refresh token initialized a B session");
      assert.ok(
        result.status >= 400 && result.status < 500,
        `expected a 4xx realtime error, got ${result.status}`,
      );
    } finally {
      await client.close();
    }
  });

  await check("WebSocket init accepts A's refresh token on A", async () => {
    const client = await Realtime.open(apps.A.id, "A init");
    try {
      const result = await initialize(client, apps.A, {
        refreshToken: apps.A.user.refresh_token,
      });
      assert.equal(result.op, "init-ok", "A's refresh token did not initialize an A session");
      assert.ok(result["session-id"], "init-ok is missing a session id");
    } finally {
      await client.close();
    }
  });
});

test("A's admin token cannot call B's admin routes", async () => {
  const crossAppCalls = [
    ["POST", "/admin/query", { query: { isolation_rows: {} } }],
    ["POST", "/admin/transact", { steps: [["update", "isolation_rows", randomUUID(), { note: "cross-app" }]] }],
    ["GET", `/admin/users?email=${encodeURIComponent(apps.B.email)}`, undefined],
    ["GET", "/admin/storage/files", undefined],
    ["GET", `/admin/storage/signed-download-url?filename=${encodeURIComponent(apps.B.filePath)}`, undefined],
    ["GET", `/admin/rooms/presence?room-type=${encodeURIComponent(ROOM_TYPE)}&room-id=${encodeURIComponent("cross-app-presence")}`, undefined],
  ];
  for (const [method, path, body] of crossAppCalls) {
    await check(`${method} ${path.split("?")[0]} rejects A's admin token on B`, async () => {
      const response = await request(method, path, {
        appId: apps.B.id,
        token: apps.A.adminToken,
        body,
      });
      expectRejected(response, `A admin token on B ${method} ${path}`, appSecrets(apps.B));
    });
  }
});

test("admin impersonation headers cannot cross apps", async () => {
  await check("POST /admin/query rejects as-token for B on A", async () => {
    const response = await request("POST", "/admin/query", {
      appId: apps.A.id,
      token: apps.A.adminToken,
      headers: { "as-token": apps.B.user.refresh_token },
      body: { query: { isolation_rows: {} } },
    });
    expectRejected(response, "as-token B on A", appSecrets(apps.B));
  });

  await check("POST /admin/query rejects as-email for B on A", async () => {
    const response = await request("POST", "/admin/query", {
      appId: apps.A.id,
      token: apps.A.adminToken,
      headers: { "as-email": apps.B.email },
      body: { query: { isolation_rows: {} } },
    });
    expectRejected(response, "as-email B on A", appSecrets(apps.B));
  });

  await check("as-token ignores a foreign admin token (acts as the B user under B's rules)", async () => {
    const body = { query: { isolation_rows: {} } };
    const withForeignToken = await request("POST", "/admin/query", {
      appId: apps.B.id,
      token: apps.A.adminToken,
      headers: { "as-token": apps.B.user.refresh_token },
      body,
    });
    const withoutToken = await request("POST", "/admin/query", {
      appId: apps.B.id,
      headers: { "as-token": apps.B.user.refresh_token },
      body,
    });
    expectOk(withForeignToken, "as-token B on B with A admin token");
    expectOk(withoutToken, "as-token B on B without a bearer token");
    assert.equal(
      withForeignToken.status,
      withoutToken.status,
      "a foreign admin token changed the as-token response status",
    );
    assert.equal(
      withForeignToken.text,
      withoutToken.text,
      "a foreign admin token changed the as-token response body",
    );
  });

  await check("POST /admin/query accepts as-token for A on A", async () => {
    const response = await request("POST", "/admin/query", {
      appId: apps.A.id,
      token: apps.A.adminToken,
      headers: { "as-token": apps.A.user.refresh_token },
      body: { query: { isolation_rows: {} } },
    });
    expectOk(response, "as-token A on A");
  });

  await check("POST /admin/query accepts as-email for A on A", async () => {
    const response = await request("POST", "/admin/query", {
      appId: apps.A.id,
      token: apps.A.adminToken,
      headers: { "as-email": apps.A.email },
      body: { query: { isolation_rows: {} } },
    });
    expectOk(response, "as-email A on A");
  });
});

test("presigned storage URLs and file lists cannot cross apps", async () => {
  await check("GET /admin/storage/signed-download-url rejects A's token on B", async () => {
    const response = await request(
      "GET",
      `/admin/storage/signed-download-url?filename=${encodeURIComponent(apps.B.filePath)}`,
      { appId: apps.B.id, token: apps.A.adminToken },
    );
    expectRejected(response, "cross-app download URL", appSecrets(apps.B));
  });

  await check("POST /admin/storage/signed-upload-url rejects A's token on B", async () => {
    const response = await request("POST", "/admin/storage/signed-upload-url", {
      appId: apps.B.id,
      token: apps.A.adminToken,
      body: { filename: apps.B.filePath },
    });
    expectRejected(response, "cross-app upload URL", appSecrets(apps.B));
  });

  await check("A's signed download URL serves A's object", async () => {
    const response = await fetch(apps.A.downloadUrl);
    assert.equal(response.status, 200, `object store returned ${response.status} for A's URL`);
    assert.equal(await response.text(), apps.A.fileContent, "A's signed URL returned different content");
  });

  await check("A's signature does not authorize B's object key", async () => {
    const bFile = apps.B.files.find((file) => file.name === apps.B.filePath);
    assert.ok(bFile, "B's uploaded file is missing from its listing");
    const url = new URL(apps.A.downloadUrl);
    const bucket = url.pathname.split("/")[1];
    const swapped = new URL(url);
    swapped.pathname = `/${bucket}/${bFile.key}`;
    const response = await fetch(swapped);
    const body = await response.text();
    assert.ok(
      response.status >= 400 && response.status < 500,
      `object store accepted a rewritten presigned URL (${response.status})`,
    );
    assert.ok(!body.includes(apps.B.fileContent), "the rewritten URL returned B's file content");
  });

  await check("A's file listing does not include B's file", async () => {
    const listed = await listFiles(apps.A);
    assert.ok(
      !listed.some((file) => file.name === apps.B.filePath),
      "A's file listing includes B's file",
    );
  });
});

test("SSE session credentials cannot cross apps", async () => {
  let session;
  try {
    session = await check("GET /runtime/sse opens an A session", async () => {
      const opened = await openSse(apps.A);
      assert.equal(opened.init.op, "sse-init", "the SSE stream did not start with sse-init");
      assert.ok(opened.init["session-id"], "sse-init is missing a session id");
      assert.ok(opened.init["sse-token"], "sse-init is missing an sse token");
      return opened;
    });

    await check("POST /runtime/sse accepts A's own session credentials", async () => {
      const response = await request("POST", `/runtime/sse?app_id=${encodeURIComponent(apps.A.id)}`, {
        body: ssePushBody(session.init, []),
      });
      expectOk(response, "A SSE push on A");
    });

    await check("POST /runtime/sse rejects A's session credentials on B", async () => {
      const response = await request("POST", `/runtime/sse?app_id=${encodeURIComponent(apps.B.id)}`, {
        body: ssePushBody(session.init, [{ op: "init", "app-id": apps.B.id }]),
      });
      expectRejected(response, "A SSE push on B", appSecrets(apps.B));
    });
  } finally {
    if (session) {
      try {
        await session.reader.cancel();
      } catch {
        // The connection is already gone when the server ended the stream.
      }
      session.controller.abort();
    }
  }
});

test("rooms, topics and presence do not cross apps", async () => {
  const roomId = `cross-app-room-${randomUUID()}`;
  const topic = "cross-app-topic";
  const roomA = await Realtime.open(apps.A.id, "A room");
  const roomB = await Realtime.open(apps.B.id, "B room");
  try {
    const sessions = await check("WebSocket init works in A and B", async () => {
      const initA = await initialize(roomA, apps.A, { refreshToken: apps.A.user.refresh_token });
      assert.equal(initA.op, "init-ok", "A's room session did not initialize");
      const initB = await initialize(roomB, apps.B, { refreshToken: apps.B.user.refresh_token });
      assert.equal(initB.op, "init-ok", "B's room session did not initialize");
      return { a: initA["session-id"], b: initB["session-id"] };
    });
    const sessionA = sessions.a;
    const sessionB = sessions.b;

    await check("WebSocket join-room works in A and B", async () => {
      roomA.send({ op: "join-room", "room-type": ROOM_TYPE, "room-id": roomId, data: {} });
      const joinedA = await roomA.waitFor((message) => message.op === "join-room-ok", "A join-room");
      assert.equal(joinedA["room-id"], roomId);
      roomB.send({ op: "join-room", "room-type": ROOM_TYPE, "room-id": roomId, data: {} });
      const joinedB = await roomB.waitFor((message) => message.op === "join-room-ok", "B join-room");
      assert.equal(joinedB["room-id"], roomId);
    });

    await check("A does not receive B's presence for the same room id", async () => {
      await roomA.expectNone(
        (message) => message.op === "refresh-presence" && Object.hasOwn(message.data ?? {}, sessionB),
        "A received B's presence",
      );
    });

    await check("A does not receive B's topic broadcast for the same room id", async () => {
      const marker = `b-broadcast-${randomUUID()}`;
      roomB.send({
        op: "client-broadcast",
        "room-id": roomId,
        topic,
        data: { marker },
      });
      await roomB.waitFor((message) => message.op === "client-broadcast-ok", "B broadcast ack");
      await roomA.expectNone(
        (message) => message.op === "server-broadcast" && message.data?.data?.marker === marker,
        "A received B's broadcast",
      );
    });

    await check("B does not receive A's topic broadcast for the same room id", async () => {
      const marker = `a-broadcast-${randomUUID()}`;
      roomA.send({
        op: "client-broadcast",
        "room-id": roomId,
        topic,
        data: { marker },
      });
      await roomA.waitFor((message) => message.op === "client-broadcast-ok", "A broadcast ack");
      await roomB.expectNone(
        (message) => message.op === "server-broadcast" && message.data?.data?.marker === marker,
        "B received A's broadcast",
      );
    });

    await check("GET /admin/rooms/presence returns only the calling app's peers", async () => {
      const response = await request(
        "GET",
        `/admin/rooms/presence?room-type=${encodeURIComponent(ROOM_TYPE)}&room-id=${encodeURIComponent(roomId)}`,
        { appId: apps.B.id, token: apps.B.adminToken },
      );
      const body = expectOk(response, "B presence on B");
      const sessions = Object.keys(body?.sessions ?? {});
      assert.ok(sessions.includes(sessionB), "B's presence is missing B's own session");
      assert.ok(!sessions.includes(sessionA), "B's presence includes A's session");
      assert.ok(!response.text.includes(apps.A.user.id), "B's presence leaked A's user");
    });
  } finally {
    await roomA.close();
    await roomB.close();
  }
});

test("streams cannot be read across apps", async () => {
  const secret = `cross-app-stream-${randomUUID()}`;
  const writer = await Realtime.open(apps.A.id, "A stream writer");
  try {
    const started = await check("WebSocket start-stream succeeds in A", async () => {
      const init = await initialize(writer, apps.A, { adminToken: apps.A.adminToken });
      assert.equal(init.op, "init-ok", "A's stream session did not initialize");
      writer.send({
        op: "start-stream",
        "client-id": `cross-app-stream-${randomUUID()}`,
        "reconnect-token": randomUUID(),
      });
      const message = await writer.waitFor((value) => value.op === "start-stream-ok", "start-stream");
      assert.ok(message["stream-id"], "start-stream-ok is missing a stream id");
      return message;
    });
    const streamId = started["stream-id"];

    await check("WebSocket append-stream completes A's stream", async () => {
      writer.send({
        op: "append-stream",
        "stream-id": streamId,
        offset: 0,
        chunks: [secret],
        done: true,
      });
      await writer.waitFor(
        (message) => message.op === "stream-flushed" || message.op === "error",
        "append-stream result",
      );
    });

    await check("A can subscribe to A's stream", async () => {
      const reader = await Realtime.open(apps.A.id, "A stream reader");
      try {
        const init = await initialize(reader, apps.A, { adminToken: apps.A.adminToken });
        assert.equal(init.op, "init-ok", "A's stream reader did not initialize");
        reader.send({
          op: "subscribe-stream",
          "stream-id": streamId,
          "client-event-id": randomUUID(),
          offset: 0,
        });
        const replay = await collectStreamReplay(reader, "A stream replay");
        assert.ok(
          replay.messages.some((message) => message.op === "stream-append"),
          "A could not replay A's own stream",
        );
        assert.match(replay.text, new RegExp(secret), "A's stream replay is missing content");
      } finally {
        await reader.close();
      }
    });

    await check("B cannot subscribe to A's stream", async () => {
      const reader = await Realtime.open(apps.B.id, "B stream reader");
      try {
        const init = await initialize(reader, apps.B, { adminToken: apps.B.adminToken });
        assert.equal(init.op, "init-ok", "B's stream reader did not initialize");
        reader.send({
          op: "subscribe-stream",
          "stream-id": streamId,
          "client-event-id": randomUUID(),
          offset: 0,
        });
        const message = await reader.waitFor(
          (value) => value.op === "stream-append" || value.op === "error",
          "B stream replay",
        );
        assert.equal(message.op, "error", "B replayed A's stream");
        assert.ok(
          message.status >= 400 && message.status < 500,
          `expected a 4xx stream error, got ${message.status}`,
        );
        assert.ok(!Object.hasOwn(message, "files"), "B's error response included stream files");
        assert.ok(!JSON.stringify(message).includes(secret), "B's error response leaked A's stream content");
      } finally {
        await reader.close();
      }
    });
  } finally {
    await writer.close();
  }
});

test("webhook configuration cannot cross apps", async () => {
  await check("GET /dash/apps/{B}/webhooks rejects A's admin token", async () => {
    const response = await request("GET", `/dash/apps/${apps.B.id}/webhooks`, {
      token: apps.A.adminToken,
    });
    expectRejected(response, "A admin token on B webhook config", appSecrets(apps.B));
  });

  await check("GET /dash/apps/{A}/webhooks accepts A's admin token", async () => {
    const response = await request("GET", `/dash/apps/${apps.A.id}/webhooks`, {
      token: apps.A.adminToken,
    });
    const body = expectOk(response, "A admin token on A webhook config");
    assert.ok(Array.isArray(body?.webhooks), "A's webhook config is not a list");
  });
});

test("reserved deployment app ids stay uncreatable", async () => {
  await check("POST /dash/apps rejects the config app id", async () => {
    const response = await request("POST", "/dash/apps", {
      body: { id: CONFIG_APP_ID, title: "cross-app squatter", admin_token: randomUUID() },
    });
    assert.equal(
      response.status,
      400,
      `expected the reserved-id guard to answer 400, got ${response.status}: ${preview(response.text)}`,
    );
    assert.match(response.text, /reserved/i, "the rejection does not name the reserved app id rule");
  });

  await check("POST /admin/query rejects the config app id with an ephemeral admin token", async () => {
    const response = await request("POST", "/admin/query", {
      appId: CONFIG_APP_ID,
      token: apps.A.adminToken,
      body: { query: { isolation_rows: {} } },
    });
    expectRejected(response, "config app with an ephemeral admin token", [
      ...appSecrets(apps.A),
      ...appSecrets(apps.B),
    ]);
  });
});

test("ephemeral app control is bound to its own admin token", async () => {
  await check("POST /dash/apps/ephemeral/{A}/status rejects B's admin token", async () => {
    const response = await request("POST", `/dash/apps/ephemeral/${apps.A.id}/status`, {
      body: { "admin-token": apps.B.adminToken, status: "active" },
    });
    expectRejected(response, "ephemeral status with another app's admin token", appSecrets(apps.A));
  });

  await check("POST /dash/apps/ephemeral/{A}/claim rejects B's admin token", async () => {
    const response = await request("POST", `/dash/apps/ephemeral/${apps.A.id}/claim`, {
      token: apps.B.adminToken,
      body: { token: apps.A.adminToken },
    });
    expectRejected(response, "ephemeral claim with another app's admin token", appSecrets(apps.A));
  });

  await check("POST /dash/apps/ephemeral/{A}/status accepts A's admin token", async () => {
    const response = await request("POST", `/dash/apps/ephemeral/${apps.A.id}/status`, {
      body: { "admin-token": apps.A.adminToken, status: "active" },
    });
    const body = expectOk(response, "ephemeral status with its own admin token");
    assert.equal(body?.status, "active");
  });
});

async function provisionApp(label) {
  const suffix = label.toLowerCase();
  const app = await createEphemeralApp(`cross-app-isolation-${suffix}`);
  app.label = label;
  app.marker = `cross-app-${suffix}-${randomUUID()}`;
  app.email = `cross-app-${suffix}-${randomUUID()}@example.com`;
  app.rowId = randomUUID();
  app.filePath = `cross-app/${suffix}-${randomUUID()}.txt`;
  app.fileContent = `cross-app file ${label} ${randomUUID()}`;
  app.user = await createUser(app);
  await seedRow(app);
  await uploadFile(app);
  app.downloadUrl = await signedDownloadUrl(app);
  app.files = await listFiles(app);
  return app;
}

async function createEphemeralApp(title) {
  const response = await request("POST", "/dash/apps/ephemeral", { body: { title } });
  const body = expectOk(response, `create ephemeral app ${title}`);
  const adminToken = body?.app?.["admin-token"];
  assert.ok(body?.app?.id, "the ephemeral app response is missing app.id");
  assert.ok(adminToken, "the ephemeral app response is missing admin-token");
  return { id: body.app.id, adminToken, title };
}

async function createUser(app) {
  const response = await request("POST", "/admin/refresh_tokens", {
    appId: app.id,
    token: app.adminToken,
    body: { email: app.email },
  });
  const body = expectOk(response, `create user in app ${app.label}`);
  assert.ok(body?.user?.refresh_token, "the user response is missing a refresh token");
  assert.equal(body.user.email, app.email);
  return body.user;
}

async function seedRow(app) {
  const response = await request("POST", "/admin/transact", {
    appId: app.id,
    token: app.adminToken,
    body: { steps: [["update", "isolation_rows", app.rowId, { note: app.marker }]] },
  });
  expectOk(response, `seed a row in app ${app.label}`);
}

async function uploadFile(app) {
  const response = await request(
    "PUT",
    `/admin/storage/upload?path=${encodeURIComponent(app.filePath)}`,
    {
      appId: app.id,
      token: app.adminToken,
      body: app.fileContent,
      headers: { "content-type": "text/plain" },
    },
  );
  expectOk(response, `upload a file to app ${app.label}`);
}

async function signedDownloadUrl(app) {
  const response = await request(
    "GET",
    `/admin/storage/signed-download-url?filename=${encodeURIComponent(app.filePath)}`,
    { appId: app.id, token: app.adminToken },
  );
  const body = expectOk(response, `sign a download URL for app ${app.label}`);
  assert.equal(typeof body?.data, "string", "the signed download URL is not a string");
  return body.data;
}

async function listFiles(app) {
  const response = await request("GET", "/admin/storage/files", {
    appId: app.id,
    token: app.adminToken,
  });
  const body = expectOk(response, `list files in app ${app.label}`);
  assert.ok(Array.isArray(body?.data), "the file listing is not an array");
  return body.data;
}

class Realtime {
  constructor(socket, label) {
    this.socket = socket;
    this.label = label;
    this.inbox = [];
    this.failure = undefined;
    socket.addEventListener("message", (event) => {
      if (typeof event.data !== "string") {
        return;
      }
      const parsed = parseJson(event.data);
      if (parsed !== undefined) {
        this.inbox.push(parsed);
      }
    });
    socket.addEventListener("error", () => {
      this.failure = new Error(`${label} websocket reported an error`);
    });
  }

  static async open(appId, label) {
    const socket = new WebSocket(
      `${realtimeOrigin}/runtime/session?app_id=${encodeURIComponent(appId)}`,
    );
    const client = new Realtime(socket, label);
    sockets.add(client);
    await new Promise((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error(`${label} websocket did not open`)),
        10_000,
      );
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
    assert.equal(this.failure, undefined, this.failure?.message);
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

  async expectNone(predicate, label, windowMs = 1_000) {
    const deadline = Date.now() + windowMs;
    while (Date.now() < deadline) {
      if (this.inbox.some(predicate)) {
        throw new Error(`${this.label}: ${label}`);
      }
      await sleep(25);
    }
  }

  async close() {
    sockets.delete(this);
    try {
      this.socket.close();
    } catch {
      // Closing a socket that already failed is not interesting.
    }
    await sleep(0);
  }
}

async function initialize(client, app, options = {}) {
  const message = { op: "init", "app-id": app.id };
  if (options.refreshToken) {
    message["refresh-token"] = options.refreshToken;
  }
  if (options.adminToken) {
    message["__admin-token"] = options.adminToken;
  }
  client.send(message);
  return client.waitFor(
    (value) => value.op === "init-ok" || value.op === "error",
    "init response",
  );
}

async function openSse(app) {
  const controller = new AbortController();
  const response = await fetch(
    `${apiOrigin}/runtime/sse?app_id=${encodeURIComponent(app.id)}`,
    { headers: { accept: "text/event-stream" }, signal: controller.signal },
  );
  assert.equal(response.status, 200, `SSE open failed with ${response.status}`);
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let pending = "";
  const deadline = Date.now() + 10_000;
  for (;;) {
    if (Date.now() >= deadline) {
      controller.abort();
      throw new Error("timed out waiting for the SSE init event");
    }
    const { value, done } = await reader.read();
    if (done) {
      controller.abort();
      throw new Error("the SSE stream ended before the init event");
    }
    pending += decoder.decode(value, { stream: true });
    let boundary = pending.indexOf("\n\n");
    while (boundary !== -1) {
      const raw = pending.slice(0, boundary);
      pending = pending.slice(boundary + 2);
      const data = raw
        .split("\n")
        .filter((line) => line.startsWith("data:"))
        .map((line) => line.slice("data:".length).trim())
        .join("\n");
      if (data) {
        return { init: JSON.parse(data), reader, controller };
      }
      boundary = pending.indexOf("\n\n");
    }
  }
}

function ssePushBody(init, messages) {
  return {
    machine_id: init["machine-id"],
    session_id: init["session-id"],
    sse_token: init["sse-token"],
    messages,
  };
}

// A finished stream replays as `stream-append` messages. Content that was
// flushed to object storage arrives as presigned file URLs instead of inline
// `content`, and the last message carries `done: true`.
async function collectStreamReplay(reader, label, timeoutMs = 10_000) {
  const deadline = Date.now() + timeoutMs;
  const messages = [];
  const parts = [];
  for (;;) {
    const remaining = deadline - Date.now();
    if (remaining <= 0) {
      throw new Error(`${reader.label}: timed out waiting for ${label}`);
    }
    const message = await reader.waitFor(
      (value) => value.op === "stream-append" || value.op === "error",
      label,
      remaining,
    );
    messages.push(message);
    if (message.op === "error") {
      return { messages, text: parts.join("") };
    }
    if (typeof message.content === "string") {
      parts.push(message.content);
    }
    for (const file of message.files ?? []) {
      const response = await fetchObject(file.url);
      assert.equal(
        response.status,
        200,
        `stream file fetch returned ${response.status}`,
      );
      parts.push(await response.text());
    }
    if (message.done === true) {
      return { messages, text: parts.join("") };
    }
  }
}

// Stream files use presigned URLs, like the storage probe. The test stack
// publishes the object store on INSTANT_S3_ORIGIN; only fall back to that
// origin when the advertised host is not reachable from here.
async function fetchObject(url) {
  try {
    return await fetch(url);
  } catch {
    const publicUrl = new URL(url);
    const origin = new URL(s3Origin);
    publicUrl.protocol = origin.protocol;
    publicUrl.host = origin.host;
    return fetch(publicUrl);
  }
}

function appSecrets(app) {
  return [
    app.user?.id,
    app.user?.email,
    app.user?.refresh_token,
    app.marker,
    app.filePath,
    app.fileContent,
  ].filter((value) => typeof value === "string" && value.length > 0);
}

async function check(label, run) {
  try {
    const result = await run();
    process.stdout.write(`ok   ${label}\n`);
    return result;
  } catch (error) {
    process.stdout.write(`FAIL ${label}: ${error.message}\n`);
    throw error;
  }
}

async function request(method, path, options = {}) {
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
  const response = await fetch(`${apiOrigin}${path}`, {
    method,
    headers,
    body,
    redirect: "manual",
  });
  const text = await response.text();
  return { status: response.status, text, body: parseJson(text) };
}

function expectOk(response, label) {
  assert.ok(
    response.status >= 200 && response.status < 300,
    `${label}: expected 2xx, got ${response.status}: ${preview(response.text)}`,
  );
  return response.body;
}

function expectRejected(response, label, leaks = []) {
  assert.ok(
    response.status >= 400 && response.status < 500,
    `${label}: expected 4xx, got ${response.status}: ${preview(response.text)}`,
  );
  for (const leak of leaks) {
    if (typeof leak !== "string" || leak.length === 0) {
      continue;
    }
    assert.ok(!response.text.includes(leak), `${label}: the response leaked another app's value`);
  }
}

function preview(text) {
  return text.length > 300 ? `${text.slice(0, 300)}...` : text;
}

function parseJson(text) {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

function required(name) {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${name} is required`);
  }
  return value;
}

function sleep(duration) {
  return new Promise((resolve) => setTimeout(resolve, duration));
}
