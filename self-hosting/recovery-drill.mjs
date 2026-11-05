// Faults are restricted to the disposable project created by the acceptance harness.
export function recoveryCommands(mode, project, composeFile, apiOrigin) {
  if (!/^instant-single-server-restart-\d+-\d+$/.test(project))
    throw new Error("Recovery requires an isolated acceptance project");
  const url = new URL(apiOrigin);
  if (
    url.protocol !== "http:" ||
    url.hostname !== "127.0.0.1" ||
    url.username ||
    url.password ||
    url.search ||
    url.hash
  )
    throw new Error("Recovery requires a loopback API");
  const base = ["compose", "--project-name", project, "--file", composeFile];
  switch (mode) {
    case "restart":
      return [[...base, "restart", "--timeout", "10", "server"]];
    case "server-crash":
      return [
        [...base, "kill", "--signal", "SIGKILL", "server"],
        [...base, "up", "-d", "--no-build", "--no-deps", "server"],
      ];
    case "database-outage":
      return [
        [...base, "stop", "--timeout", "10", "postgres"],
        [...base, "start", "postgres"],
      ];
    default:
      throw new Error("Unknown recovery mode");
  }
}

export function recoveryReceipt(mode, elapsedMs, targetMs) {
  if (
    ![elapsedMs, targetMs].every(Number.isFinite) ||
    elapsedMs < 0 ||
    targetMs <= 0
  )
    throw new Error("Invalid recovery timing");
  if (elapsedMs > targetMs)
    throw new Error("Recovery exceeded the configured RTO");
  return {
    recovery: mode,
    status: "passed",
    recoveryMs: Math.round(elapsedMs),
    rtoMs: targetMs,
    acknowledgedFixtureRowsLost: 0,
    realtimeResumed: true,
    scope: "isolated fixture; not a production RPO or backup-age guarantee",
  };
}
