/**
 * Smoke test: the launcher must actually start the server on THIS platform and
 * get a valid MCP `initialize` response back. A launcher that resolves a path
 * successfully but produces a binary that will not run is worthless — that is
 * the whole failure this package exists to remove.
 */
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const launcher = join(here, "..", "bin", "effectfence.js");

const req = JSON.stringify({
  jsonrpc: "2.0", id: 1, method: "initialize",
  params: { protocolVersion: "2026-07-28", capabilities: {}, clientInfo: { name: "smoke", version: "0" } },
});

const child = spawn(process.execPath, [launcher], { stdio: ["pipe", "pipe", "inherit"] });
let out = "";
child.stdout.on("data", (d) => (out += d));
child.stdin.write(req + "\n");
child.stdin.end();

const code = await new Promise((r) => child.on("exit", r));

const fail = (m) => { console.error("FAIL " + m); process.exit(1); };
if (code !== 0) fail(`launcher exited ${code}`);

let msg;
try { msg = JSON.parse(out.split("\n").find((l) => l.trim())); }
catch { fail("server did not emit valid JSON:\n" + out.slice(0, 300)); }

if (!msg?.result?.serverInfo) fail("no serverInfo in initialize response");
if (msg.result.serverInfo.name !== "effectfence") fail(`unexpected server: ${msg.result.serverInfo.name}`);

console.log(`PASS  ${process.platform}-${process.arch}: ${msg.result.serverInfo.name} v${msg.result.serverInfo.version}, protocol ${msg.result.protocolVersion}`);
