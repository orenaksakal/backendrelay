import { execFile } from "node:child_process";
import { createServer } from "node:http";
import { promisify } from "node:util";

const run = promisify(execFile);
const composeFile = process.env.INSTANT_BYOP_COMPOSE_FILE;
const psqlURL = process.env.INSTANT_BYOP_PSQL_URL;
const psqlBin = process.env.INSTANT_BYOP_PSQL_BIN ?? "psql";
const schema = process.env.INSTANT_BYOP_SCHEMA;
const token = process.env.INSTANT_BYOP_MUTATOR_TOKEN;
const slotName = process.env.INSTANT_BYOP_SLOT_NAME;
const port = Number(process.env.INSTANT_BYOP_MUTATOR_PORT ?? "8899");
let psqlEnv;

if (
  (!composeFile && !psqlURL) ||
  !schema ||
  !token ||
  !/^[a-z_][a-z0-9_]*$/.test(schema) ||
  !slotName ||
  !/^instant_byop_[0-9a-f]{32}$/.test(slotName)
) {
  throw new Error("invalid BYOP mutator configuration");
}
if (psqlURL) {
  const parsed = new URL(psqlURL);
  const database = decodeURIComponent(parsed.pathname.slice(1));
  if (
    (parsed.protocol !== "postgres:" && parsed.protocol !== "postgresql:") ||
    !parsed.hostname ||
    !database ||
    database.includes("/")
  ) {
    throw new Error("INSTANT_BYOP_PSQL_URL is not a PostgreSQL URL");
  }
  psqlEnv = {
    ...process.env,
    PGHOST: parsed.hostname,
    PGPORT: parsed.port || "5432",
    PGDATABASE: database,
  };
  if (parsed.username) psqlEnv.PGUSER = decodeURIComponent(parsed.username);
  if (parsed.password) psqlEnv.PGPASSWORD = decodeURIComponent(parsed.password);
  const sslmode = parsed.searchParams.get("sslmode");
  if (sslmode) psqlEnv.PGSSLMODE = sslmode;
}

async function runSql(statement, outputArgs = []) {
  if (psqlURL) {
    return run(
      psqlBin,
      ["-v", "ON_ERROR_STOP=1", ...outputArgs, "-c", statement],
      { env: psqlEnv },
    );
  }
  return run("docker", [
    "compose",
    "-f",
    composeFile,
    "exec",
    "-T",
    "postgres",
    "psql",
    "-U",
    "instant",
    "-d",
    "instant",
    "-v",
    "ON_ERROR_STOP=1",
    ...outputArgs,
    "-c",
    statement,
  ]);
}

async function activeReplicationPid() {
  const result = await runSql(
    `SELECT COALESCE(active_pid::text, '') FROM pg_replication_slots WHERE slot_name = '${slotName}' AND plugin = 'wal2json'`,
    ["-A", "-t", "-q"],
  );
  return result.stdout.trim();
}

async function restartReplication() {
  const previousPid = await activeReplicationPid();
  if (!/^[1-9][0-9]*$/.test(previousPid)) {
    throw new Error("BYOP replication slot is not active");
  }
  const terminated = await runSql(
    `SELECT pg_terminate_backend(active_pid) FROM pg_replication_slots WHERE slot_name = '${slotName}' AND plugin = 'wal2json' AND active_pid = ${previousPid}`,
    ["-A", "-t", "-q"],
  );
  if (terminated.stdout.trim() !== "t") {
    throw new Error("BYOP replication backend was not terminated");
  }
  for (let attempt = 0; attempt < 150; attempt += 1) {
    await new Promise((resolve) => setTimeout(resolve, 200));
    const currentPid = await activeReplicationPid();
    if (/^[1-9][0-9]*$/.test(currentPid) && currentPid !== previousPid) {
      return;
    }
  }
  throw new Error("BYOP replication slot did not reacquire a backend");
}

async function runContendedRelationshipRotation(todoId, ownerAlias) {
  const contentionTitles = Array.from(
    { length: 8 },
    (_, index) => `core-owned-contention-${index + 1}`,
  );
  const contenders = contentionTitles.map((contentionTitle) =>
    runSql(
      `DO $byop$ BEGIN UPDATE ${schema}.todos SET title = '${contentionTitle}' WHERE id = '${todoId}' AND owner_id = '00000000-0000-4000-8000-000000000101'; IF NOT FOUND THEN RAISE EXCEPTION 'owned todo was not found for contention worker'; END IF; PERFORM pg_sleep(0.2); UPDATE ${schema}.profiles SET viewer_alias = '${ownerAlias}' WHERE account_id = '00000000-0000-4000-8000-000000000201'; IF NOT FOUND THEN RAISE EXCEPTION 'owned profile was not found for contention worker'; END IF; END; $byop$;`,
    ),
  );
  await Promise.all(contenders);
  const expectedTitles = contentionTitles
    .map((title) => `'${title}'`)
    .join(", ");
  await runSql(
    `DO $byop$ BEGIN UPDATE ${schema}.todos SET title = 'core-owned-contention-converged' WHERE id = '${todoId}' AND title IN (${expectedTitles}) AND owner_id = '00000000-0000-4000-8000-000000000101'; IF NOT FOUND THEN RAISE EXCEPTION 'owned todo contention state was not found'; END IF; UPDATE ${schema}.profiles SET viewer_alias = '${ownerAlias}' WHERE account_id = '00000000-0000-4000-8000-000000000201'; IF NOT FOUND THEN RAISE EXCEPTION 'owned profile contention state was not found'; END IF; END; $byop$;`,
  );
}

function respond(response, status, body) {
  response.writeHead(status, {
    "access-control-allow-origin": "*",
    "content-type": "text/plain; charset=utf-8",
  });
  response.end(body);
}

const server = createServer(async (request, response) => {
  if (request.method === "GET" && request.url === "/health") {
    respond(response, 200, "ok");
    return;
  }
  if (request.method !== "POST" || request.url !== "/insert-visible-todo") {
    respond(response, 404, "not found");
    return;
  }

  const chunks = [];
  let size = 0;
  for await (const chunk of request) {
    size += chunk.length;
    if (size > 4096) {
      respond(response, 413, "request too large");
      return;
    }
    chunks.push(chunk);
  }

  try {
    const body = JSON.parse(Buffer.concat(chunks).toString("utf8"));
    if (body.token !== token) {
      respond(response, 403, "forbidden");
      return;
    }
    if (
      !/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
        body.id,
      )
    ) {
      respond(response, 400, "invalid UUID");
      return;
    }
    if (
      body.title !== "core-external-wal-refresh" &&
      body.title !== "core-owned-wal-refresh" &&
      body.title !== "core-rotate-viewer-alias" &&
      body.title !== "core-clear-owner-link" &&
      body.title !== "core-restore-owner-link" &&
      body.title !== "core-concurrent-relationship-rotation" &&
      body.title !== "core-fail-relationship-rotation" &&
      body.title !== "core-recover-relationship-rotation" &&
      body.title !== "core-restart-replication" &&
      body.title !== "core-post-reconnect-rotation" &&
      body.title !== "core-contended-relationship-rotation" &&
      body.title !== "core-delete-owned-account"
    ) {
      respond(response, 400, "invalid title");
      return;
    }
    if (body.title === "core-restart-replication") {
      await restartReplication();
      respond(response, 204, "");
      return;
    }
    if (body.title === "core-contended-relationship-rotation") {
      if (
        !/^byop-user-[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
          body.ownerAlias,
        )
      ) {
        respond(response, 400, "invalid owner identity");
        return;
      }
      await runContendedRelationshipRotation(body.id, body.ownerAlias);
      respond(response, 204, "");
      return;
    }
    let sql;
    if (body.title === "core-external-wal-refresh") {
      sql = `INSERT INTO ${schema}.todos (id, title, public) VALUES ('${body.id}', '${body.title}', true)`;
    } else if (body.title === "core-owned-wal-refresh") {
      if (
        !/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
          body.foreignId,
        ) ||
        !/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
          body.ownerId,
        ) ||
        !/^byop-user-[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
          body.ownerAlias,
        )
      ) {
        respond(response, 400, "invalid owner identity");
        return;
      }
      sql = `BEGIN; INSERT INTO ${schema}.accounts (id, user_id) VALUES ('00000000-0000-4000-8000-000000000201', '${body.ownerId}'), ('00000000-0000-4000-8000-000000000202', '00000000-0000-4000-8000-000000000099'); INSERT INTO ${schema}.profiles (id, account_id, viewer_alias) VALUES ('00000000-0000-4000-8000-000000000301', '00000000-0000-4000-8000-000000000201', '${body.ownerAlias}'), ('00000000-0000-4000-8000-000000000302', '00000000-0000-4000-8000-000000000202', 'byop-user-00000000-0000-4000-8000-000000000099'); INSERT INTO ${schema}.owners (id, account_id) VALUES ('00000000-0000-4000-8000-000000000101', '00000000-0000-4000-8000-000000000201'), ('00000000-0000-4000-8000-000000000102', '00000000-0000-4000-8000-000000000202'); INSERT INTO ${schema}.todos (id, title, public, owner_id, viewer_id, viewer_alias) VALUES ('${body.id}', 'core-owned-wal-refresh', false, '00000000-0000-4000-8000-000000000101', '${body.ownerId}', '${body.ownerAlias}'), ('${body.foreignId}', 'core-foreign-hidden', false, '00000000-0000-4000-8000-000000000102', '00000000-0000-4000-8000-000000000099', 'byop-user-00000000-0000-4000-8000-000000000099'); COMMIT`;
    } else if (body.title === "core-rotate-viewer-alias") {
      if (
        !/^byop-user-[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
          body.ownerAlias,
        )
      ) {
        respond(response, 400, "invalid owner identity");
        return;
      }
      sql = `WITH updated AS (UPDATE ${schema}.profiles AS profile_row SET viewer_alias = '${body.ownerAlias}' FROM ${schema}.owners AS owner_row JOIN ${schema}.todos AS todo_row ON todo_row.owner_id = owner_row.id WHERE profile_row.account_id = owner_row.account_id AND todo_row.id = '${body.id}' AND todo_row.title = 'core-owned-wal-refresh' RETURNING 1) SELECT 1 / count(*)::int FROM updated`;
    } else if (body.title === "core-clear-owner-link") {
      sql = `WITH updated AS (UPDATE ${schema}.todos SET owner_id = NULL WHERE id = '${body.id}' AND title = 'core-owned-wal-refresh' AND owner_id = '00000000-0000-4000-8000-000000000101' RETURNING 1) SELECT 1 / count(*)::int FROM updated`;
    } else if (body.title === "core-restore-owner-link") {
      sql = `WITH updated AS (UPDATE ${schema}.todos SET owner_id = '00000000-0000-4000-8000-000000000101' WHERE id = '${body.id}' AND title = 'core-owned-wal-refresh' AND owner_id IS NULL AND EXISTS (SELECT 1 FROM ${schema}.owners WHERE id = '00000000-0000-4000-8000-000000000101') RETURNING 1) SELECT 1 / count(*)::int FROM updated`;
    } else if (body.title === "core-concurrent-relationship-rotation") {
      if (
        !/^byop-user-[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
          body.ownerAlias,
        )
      ) {
        respond(response, 400, "invalid owner identity");
        return;
      }
      sql = [
        `WITH updated AS (UPDATE ${schema}.todos SET owner_id = NULL WHERE id = '${body.id}' AND title = 'core-owned-wal-refresh' AND owner_id = '00000000-0000-4000-8000-000000000101' RETURNING 1) SELECT 1 / count(*)::int FROM updated`,
        `DO $byop$ DECLARE restored boolean := false; BEGIN FOR attempt IN 1..1000 LOOP UPDATE ${schema}.todos SET owner_id = '00000000-0000-4000-8000-000000000101', title = 'core-owned-concurrent-restored' WHERE id = '${body.id}' AND title = 'core-owned-wal-refresh' AND owner_id IS NULL; IF FOUND THEN restored := true; EXIT; END IF; PERFORM pg_sleep(0.01); END LOOP; IF NOT restored THEN RAISE EXCEPTION 'concurrent owner clear was not observed'; END IF; UPDATE ${schema}.profiles SET viewer_alias = '${body.ownerAlias}' WHERE account_id = '00000000-0000-4000-8000-000000000201'; IF NOT FOUND THEN RAISE EXCEPTION 'owned profile was not found'; END IF; END; $byop$;`,
      ];
    } else if (
      body.title === "core-fail-relationship-rotation" ||
      body.title === "core-recover-relationship-rotation"
    ) {
      if (
        !/^byop-user-[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
          body.ownerAlias,
        )
      ) {
        respond(response, 400, "invalid owner identity");
        return;
      }
      if (body.title === "core-fail-relationship-rotation") {
        sql = `DO $byop$ BEGIN UPDATE ${schema}.todos SET owner_id = NULL, title = 'core-owned-failure-staged' WHERE id = '${body.id}' AND title = 'core-owned-concurrent-restored' AND owner_id = '00000000-0000-4000-8000-000000000101'; IF NOT FOUND THEN RAISE EXCEPTION 'owned todo was not found for failure injection'; END IF; UPDATE ${schema}.profiles SET viewer_alias = '${body.ownerAlias}' WHERE account_id = '00000000-0000-4000-8000-000000000201'; IF NOT FOUND THEN RAISE EXCEPTION 'owned profile was not found for failure injection'; END IF; RAISE EXCEPTION 'intentional relationship rotation rollback'; END; $byop$;`;
      } else {
        sql = `DO $byop$ BEGIN UPDATE ${schema}.todos SET title = 'core-owned-failure-recovered' WHERE id = '${body.id}' AND title = 'core-owned-concurrent-restored' AND owner_id = '00000000-0000-4000-8000-000000000101'; IF NOT FOUND THEN RAISE EXCEPTION 'owned todo rollback state was not preserved'; END IF; UPDATE ${schema}.profiles SET viewer_alias = '${body.ownerAlias}' WHERE account_id = '00000000-0000-4000-8000-000000000201'; IF NOT FOUND THEN RAISE EXCEPTION 'owned profile rollback state was not preserved'; END IF; END; $byop$;`;
      }
    } else if (body.title === "core-post-reconnect-rotation") {
      if (
        !/^byop-user-[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
          body.ownerAlias,
        )
      ) {
        respond(response, 400, "invalid owner identity");
        return;
      }
      sql = `DO $byop$ BEGIN UPDATE ${schema}.todos SET title = 'core-owned-post-reconnect' WHERE id = '${body.id}' AND title = 'core-owned-failure-recovered' AND owner_id = '00000000-0000-4000-8000-000000000101'; IF NOT FOUND THEN RAISE EXCEPTION 'owned todo recovery state was not found after reconnect'; END IF; UPDATE ${schema}.profiles SET viewer_alias = '${body.ownerAlias}' WHERE account_id = '00000000-0000-4000-8000-000000000201'; IF NOT FOUND THEN RAISE EXCEPTION 'owned profile was not found after reconnect'; END IF; END; $byop$;`;
    } else {
      sql = `WITH target AS (SELECT owner_row.account_id FROM ${schema}.todos AS todo_row JOIN ${schema}.owners AS owner_row ON owner_row.id = todo_row.owner_id WHERE todo_row.id = '${body.id}' AND todo_row.title = 'core-owned-contention-converged'), deleted AS (DELETE FROM ${schema}.accounts AS account_row USING target WHERE account_row.id = target.account_id RETURNING 1) SELECT 1 / count(*)::int FROM deleted`;
    }
    const statements = Array.isArray(sql) ? sql : [sql];
    await Promise.all(statements.map((statement) => runSql(statement)));
    respond(response, 204, "");
  } catch (error) {
    respond(
      response,
      500,
      error instanceof Error ? error.message : "mutation failed",
    );
  }
});

server.listen(port, "127.0.0.1");
