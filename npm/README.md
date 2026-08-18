# effectfence

**MCP server that stops agents double-firing side effects. 1,000 racing duplicates, exactly one execution — proven on every commit.**

```bash
npx effectfence wrap -- npx -y your-mcp-server
```

No Rust toolchain. No build step. No install-time downloads.

## Prove your stack double-fires first

Don't take our word for it — check your own server. `probe` fires N byte-identical
calls at one tool **concurrently** (the twin-caller race) and counts distinct effects:

```bash
npx effectfence probe --tool charge_card --args '{"amount":4900}' --calls 12 -- npx -y your-mcp-server
```

```text
  identical calls   : 12
  DISTINCT effects  : 12
  PROVEN DOUBLE-FIRE — 12 identical calls, 12 different results.
```

Then re-run it through the fence and watch `DISTINCT effects` drop to `1`:

```bash
npx effectfence probe --tool charge_card --args '{"amount":4900}' --calls 12 -- npx effectfence wrap -- npx -y your-mcp-server
```

The footprints, then the lock — in two commands.

## Wrap a server you already run (start here)

EffectFence stands **in front of** an existing MCP server and fences every tool call
automatically — no changes to your agent, no remembering to call anything:

```text
agent/client ──MCP──> effectfence wrap ──MCP──> your real tool server
```

The tool list is mirrored 1:1 (same names, schemas, docs). What changes: identical
duplicate calls — same tool, same arguments — execute the child **once**; later
duplicates get the recorded result replayed instead of firing again.

### One-paste recipe: fence a cluster-mutating server

The case this exists for — several agents holding `kubectl` on the same cluster.

**Claude Code:**

```bash
claude mcp add k8s-fenced -- npx -y effectfence wrap -- npx -y kubernetes-mcp-server
```

**Cursor** (`~/.cursor/mcp.json`) **or Claude Desktop** (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "k8s-fenced": {
      "command": "npx",
      "args": ["-y", "effectfence", "wrap", "--", "npx", "-y", "kubernetes-mcp-server"]
    }
  }
}
```

Swap in whichever server holds your write-bearing tools — cloud APIs, deploy tooling,
a payments server. Point every agent at the fenced name and remove access to the raw
one; a fence only works if it is the only door.

### See what it stopped

Call the `fence_stats` tool (no arguments) for live counters since the process
started:

```
effectfence since boot: admitted=1 replayed=995 refused(stale=0 race=4 in-flight=0 failed=0) total=1000 prevented=999
```

`prevented` is every attempt that did **not** run the effect — the duplicate
executions that never happened.

## Explicit fencing (without wrap)

If you want agents to fence deliberately instead — richer control via `read_set`,
`parent`, and `known_clock` — run the server bare and call `fence_prepare` /
`fence_commit` / `fence_abort` yourself:

```json
{
  "mcpServers": {
    "effectfence": {
      "command": "npx",
      "args": ["-y", "effectfence"]
    }
  }
}
```

## What it does

An agent that retries a tool call after a timeout does not know whether the first
attempt landed. The request reached the server, the work happened, the response
never came back. Nothing failed loudly — it succeeded twice.

EffectFence sits in front of those effects. Identical intents are admitted once;
every duplicate is fenced, replayed from the original result, or refused. The
guarantee is enforced under real contention, not assumed:

- **1,000 racing duplicates → exactly one execution.** Run
  `cargo run --release --example storm` against the source and watch it yourself.
- CI runs that storm on a **fresh runner with no cache** on every commit.
- Simultaneity is **forced** with a real barrier, not produced by spawning
  threads quickly and hoping the scheduler cooperates.

## Scope — read this before you rely on it

**This is an in-memory, single-process fence. Its state lives in this server
process and is lost when the process restarts.**

That is enough to close races and duplicates between concurrent threads, tasks
and agents that share **one** running server. It is not enough for:

- **Two instances of this server.** Each has its own state, so the same intent
  can execute once per instance. A horizontally scaled or load-balanced
  deployment does not get exactly-once from this package.
- **A restart mid-flight.** State is not persisted. An intent admitted before a
  restart is unknown to the process that comes back.
- **An agent that bypasses the fence.** It protects effects routed *through* it;
  it cannot stop a caller that holds the credential and calls the provider
  directly. Deploy it at the one choke point your agents actually share.

If you need the guarantee to survive a restart or span processes, you need a
shared store behind it — see [`once-kernel`](https://www.npmjs.com/package/once-kernel)
(Python, Postgres-backed, heartbeat leases and fence tokens) or
[`seal`](https://github.com/aurumflux20/seal), which does cross-process
admission and confirms the result against the payment provider's own records.

We state this here rather than only in the source repo because the limit is the
part you need *before* you deploy, not after.

## Platforms

| Platform | Included |
|---|---|
| macOS Apple silicon (`darwin-arm64`) | yes |
| Linux x64 (`linux-x64`) | yes |
| Linux arm64 (`linux-arm64`) | yes |
| Windows x64 (`win32-x64`) | yes |
| macOS Intel (`darwin-x64`) | **no** — see below |

Every binary was built on its own native runner and made to answer an MCP
`initialize` on that platform before being published. None were cross-compiled.

**Intel macOS is deliberately absent.** GitHub retired the Intel runners, so the
only way to produce that binary would be cross-compiling it on Apple silicon —
shipping something that has never once executed. A missing download costs less
trust than a broken one. If you need it: `cargo install effectfence`.

## Why this package is ~8 MB

It bundles all four platform binaries rather than downloading the right one after
install. That is deliberate:

- **No `postinstall` script.** A tool whose entire job is guarding side effects
  should not fetch and execute code from the network while being installed.
- **Works with `--ignore-scripts`**, which is increasingly the default in CI. A
  download-on-install package silently produces a broken install there.
- **Works offline and air-gapped.**

The cost is that you download three binaries you will not run. A future release
will split these into per-platform packages selected by `optionalDependencies`,
so you fetch only yours. Correctness first, then size.

## Also available

- **Rust crate** — [`effectfence`](https://crates.io/crates/effectfence) to embed the fence directly.
- **[`once-kernel`](https://www.npmjs.com/package/once-kernel)** — the same
  exactly-once guarantee as a TypeScript library, zero dependencies, for when you
  want it inside your own code rather than as a separate MCP server.
- **[`once-kernel` on PyPI](https://pypi.org/project/once-kernel/)** — the Python
  kernel. It shares a payload hash with the TypeScript one (RFC 8785), so a
  Python service and a Node service agree about whether an operation already ran.

## Licence

MIT — same as the [Rust crate](https://crates.io/crates/effectfence) this packages.
