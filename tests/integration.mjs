#!/usr/bin/env bun
// Runs the engine-backed test binaries (the actor scenarios, SQL counts, retention and log
// redaction) against isolated engines and proves nothing outlives them.
//
// Ports: BUDDY_TEST_ENGINE_PORT (default 16420; the engine also takes +1 and +10) and
// BUDDY_TEST_MODEL_PORT (default 19091). Every credential here is a test-only literal.
// Extra arguments go to every test binary, e.g. `bun run test:integration -- speech`.
import { spawn, spawnSync } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const enginePort = Number(process.env.BUDDY_TEST_ENGINE_PORT ?? 16420);
const modelPort = Number(process.env.BUDDY_TEST_MODEL_PORT ?? 19091);
const stateDir = process.env.BUDDY_TEST_STATE_DIR ?? join(tmpdir(), "robot-buddy-it");
const ports = [enginePort, enginePort + 1, enginePort + 10, modelPort];

const env = {
  ...process.env,
  BUDDY_TEST_ENGINE_PORT: String(enginePort),
  BUDDY_TEST_MODEL_PORT: String(modelPort),
  BUDDY_TEST_STATE_DIR: stateDir,
  BUDDY_SIGNING_KEY: "integration-test-signing-key-not-for-production",
  AI_PROVIDER: "openai_compatible",
  AI_BASE_URL: `http://127.0.0.1:${modelPort}/v1`,
  AI_MODEL: "fixture",
  // Web search is routed into the mock, which accepts the CONNECT and never answers, so
  // this fake key never leaves the machine. It gives the restart test a tool in flight.
  BRAVE_SEARCH_API_KEY: "integration-test-search-key-not-real",
  HTTPS_PROXY: `http://127.0.0.1:${modelPort}`,
  https_proxy: `http://127.0.0.1:${modelPort}`,
  NO_PROXY: "127.0.0.1,localhost",
  no_proxy: "127.0.0.1,localhost",
  RIVET_ENGINE_BINARY_PATH: process.env.RIVET_ENGINE_BINARY_PATH ?? "",
};
delete env.HTTP_PROXY;
delete env.http_proxy;
delete env.ALL_PROXY;
delete env.all_proxy;
if (!env.RIVET_ENGINE_BINARY_PATH) delete env.RIVET_ENGINE_BINARY_PATH;

function listeners(port) {
  const out = spawnSync("lsof", ["-nP", `-iTCP:${port}`, "-sTCP:LISTEN", "-t"], {
    encoding: "utf8",
  });
  return out.stdout.split("\n").filter(Boolean);
}

// Listeners that predate the run are not ours; the harness reports them and refuses to start.
const before = new Set(ports.flatMap((port) => listeners(port).map((pid) => `${port}:${pid}`)));

// Each binary owns one engine on the same ports, so they run one at a time, and each must
// leave nothing behind before the next starts.
const SUITES = ["actors", "core_sql_counts", "core_retention", "core_logs"];
const runDirs = () => (existsSync(stateDir) ? readdirSync(stateDir).filter((d) => d.startsWith("run-")) : []);
let child = null;
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => child?.kill(signal));
}

async function run(suite) {
  const args = ["test", "-p", "robot-buddy", "--test", suite, "--", "--ignored", "--nocapture", ...process.argv.slice(2)];
  const started = Date.now();
  child = spawn("cargo", args, { env, stdio: "inherit" });
  const code = await new Promise((resolve) => child.on("exit", (c, s) => resolve(c ?? (s ? 1 : 0))));
  child = null;
  console.log(`integration: ${suite} exited ${code} after ${((Date.now() - started) / 1000).toFixed(1)} s`);

  // The test binary's watchdog stops the engine when the binary exits, then removes the
  // run directory; give it time for both.
  const deadline = Date.now() + 20_000;
  let survivors = [];
  for (;;) {
    survivors = ports
      .flatMap((port) => listeners(port).map((pid) => ({ port, pid })))
      .filter(({ port, pid }) => !before.has(`${port}:${pid}`));
    if ((survivors.length === 0 && runDirs().length === 0) || Date.now() > deadline) break;
    await Bun.sleep(250);
  }
  let leaked = false;
  for (const { port, pid } of survivors) {
    leaked = true;
    const cmd = spawnSync("ps", ["-o", "command=", "-p", pid], { encoding: "utf8" }).stdout.trim();
    console.error(`integration: LEAK after ${suite}: port ${port} still has listener pid ${pid}: ${cmd}`);
  }
  const runs = runDirs();
  if (runs.length > 0) {
    leaked = true;
    console.error(`integration: LEAK after ${suite}: run directories left in ${stateDir}: ${runs.join(", ")}`);
  }
  return { code, leaked };
}

let code = 0;
let leaked = false;
for (const suite of SUITES) {
  const result = await run(suite);
  code ||= result.code;
  leaked ||= result.leaked;
  // A leak would make the next suite refuse its ports.
  if (result.leaked) break;
}
if (!leaked) console.log(`integration: ${SUITES.join(", ")}: no listener on ${ports.join(", ")}; no run directory left`);
process.exit(code !== 0 ? code : leaked ? 1 : 0);
