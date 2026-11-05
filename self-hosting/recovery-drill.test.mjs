import test from "node:test";
import assert from "node:assert/strict";
import { recoveryCommands, recoveryReceipt } from "./recovery-drill.mjs";
test("fault injection refuses non-fixture projects and remote APIs", () => {
  for (const project of [
    "production",
    "instant-single-server-restart-",
    "instant-single-server-restart-1-2;rm",
  ]) {
    assert.throws(() =>
      recoveryCommands(
        "server-crash",
        project,
        "fixture.yml",
        "http://127.0.0.1:8888",
      ),
    );
  }
  assert.throws(() =>
    recoveryCommands(
      "restart",
      "instant-single-server-restart-1-2",
      "fixture.yml",
      "https://api.example.com",
    ),
  );
});
test("crash and outage drills preserve database volumes and never build images", () => {
  for (const mode of ["restart", "server-crash", "database-outage"]) {
    const commands = recoveryCommands(
      mode,
      "instant-single-server-restart-1-2",
      "fixture.yml",
      "http://127.0.0.1:8888",
    );
    assert.ok(commands.length > 0);
    assert.ok(
      commands.every(
        (c) => !c.includes("down") && !c.includes("rm") && !c.includes("build"),
      ),
    );
  }
});
test("recovery beyond RTO cannot produce a passed receipt", () => {
  assert.throws(() => recoveryReceipt("restart", 120001, 120000));
  assert.throws(() => recoveryReceipt("restart", NaN, 120000));
  assert.equal(
    recoveryReceipt("restart", 3000, 120000).acknowledgedFixtureRowsLost,
    0,
  );
});
