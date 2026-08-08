#!/usr/bin/env node
/**
 * `npx effectfence` — launcher for the EffectFence MCP server.
 *
 * EffectFence is a Rust binary. This package exists so that running it does not
 * require a Rust toolchain, which until now it did: the only published artifact
 * was a crates.io crate plus one Linux binary, so a developer on a Mac could not
 * install it at all without first installing Rust.
 *
 * Design notes, since the obvious alternatives are worse:
 *
 *   - No `postinstall` script and no download at install time. A tool whose whole
 *     purpose is guarding side effects should not fetch and execute code from the
 *     network while being installed, and `npm ci --ignore-scripts` (increasingly
 *     the default in CI) would silently produce a broken install.
 *   - Binaries are bundled, so an offline or air-gapped install works.
 *   - The child is spawned with stdio inherited, because MCP speaks JSON-RPC over
 *     stdin/stdout. Buffering or re-encoding here would corrupt the protocol.
 *   - Signals are forwarded, so an MCP client that terminates the server actually
 *     terminates it rather than orphaning a process holding a lock.
 */

import { spawn } from "node:child_process";
import { existsSync, chmodSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));

const TARGETS = {
  "darwin-arm64": "darwin-arm64/effectfence",
  "linux-x64": "linux-x64/effectfence",
  "linux-arm64": "linux-arm64/effectfence",
  "win32-x64": "win32-x64/effectfence.exe",
};

const key = `${process.platform}-${process.arch}`;
const rel = TARGETS[key];

if (!rel) {
  // Be specific about what is missing and what the alternative is. A bare
  // "unsupported platform" sends people to an issue tracker for no reason.
  console.error(
    `effectfence: no prebuilt binary for ${key}.\n` +
      `Supported: ${Object.keys(TARGETS).join(", ")}.\n` +
      `\n` +
      `Intel macOS (darwin-x64) is deliberately absent: GitHub retired the\n` +
      `Intel runners, and shipping a cross-compiled binary that has never been\n` +
      `executed would be worse than shipping none.\n` +
      `\n` +
      `You can still build it yourself with a Rust toolchain:\n` +
      `  cargo install effectfence\n`,
  );
  process.exit(1);
}

const binary = join(here, "..", "vendor", rel);

if (!existsSync(binary)) {
  console.error(
    `effectfence: the binary for ${key} is missing from this package at\n` +
      `  ${binary}\n` +
      `This means the package was published incorrectly — please open an issue at\n` +
      `https://github.com/aurumflux20/effectfence/issues\n`,
  );
  process.exit(1);
}

// npm does not reliably preserve the executable bit through pack/unpack on every
// platform. Setting it is cheap; discovering it is missing via EACCES at runtime
// is not. Failure here is non-fatal — the file may already be correct and owned
// by root in a global install.
if (process.platform !== "win32") {
  try {
    chmodSync(binary, 0o755);
  } catch {
    /* already executable, or not ours to change */
  }
}

const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });

for (const sig of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(sig, () => {
    if (!child.killed) child.kill(sig);
  });
}

child.on("error", (err) => {
  console.error(`effectfence: failed to start the server: ${err.message}`);
  process.exit(1);
});

child.on("exit", (code, signal) => {
  // Reproduce the child's exit faithfully. A supervisor deciding whether to
  // restart needs the real signal, not a flattened exit code.
  if (signal) process.kill(process.pid, signal);
  else process.exit(code ?? 0);
});
