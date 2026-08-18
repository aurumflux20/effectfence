# EffectFence

[![CI](https://github.com/aurumflux20/effectfence/actions/workflows/ci.yml/badge.svg)](https://github.com/aurumflux20/effectfence/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/effectfence.svg)](https://crates.io/crates/effectfence)
[![docs.rs](https://img.shields.io/docsrs/effectfence)](https://docs.rs/effectfence)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**Your swarm doesn't need more memory. It needs a causal fence around tool side effects.**

```
⚡ effectfence — THE STORM
1,000 attempts to charge order #777 ($49.00): concurrent racers + late retries…

ACTUAL EXECUTIONS     :      1   ← the whole point
served sealed receipt :    995
told to stand down    :      4
elapsed               : 22.33ms

💰 double-charges prevented this run: $48,951.00
✅ ONE execution. Every other attempt was fenced, replayed, or refused.
```

Run the attack yourself:

```bash
cargo run --release --example storm
```

## The multi-agent turf war

The storm above is one action, many racers. The harder case is *different*
agents making *contradictory* decisions on the same production resource — the
coordination failure now reported across production multi-agent systems (~a
third of 2026 multi-agent incidents). Three autonomous SRE agents react to one
latency spike:

```bash
cargo run --example turf_war
```

```
Agent A (autoscaler): scale node pool UP
  ADMITTED. Fence leased cluster:prod-us-east-1 — executing kubectl scale up.
  DONE. EffectCert minted — verify() -> true

Agent B (cost-optimizer): scale the same pool DOWN
  REFUSED before kubectl ran:
     read-set for `metrics:prod-us-east-1` is stale: B decided on seq 0, world is at seq 1.

Agent C (deploy-bot): roll the deployment BACK
  C's causal view is concurrent with A's: true
  -> escalated to a human instead of corrupting the cluster.

  Actions proposed: 3   executed: 1   cluster: intended, not corrupted.
```

Each agent was individually correct for the state it read. Run concurrently
without coordination, all three `kubectl` calls fire and the cluster ends in a
state none of them intended — the $100M outage. The fence lets exactly one act,
refuses the other two *before* their side effect runs, and tells each one why.

Nothing above is mocked — every call is the real crate API.



EffectFence is a causal concurrency fence for multi-agent tool calls. When more than one agent (or retry, or re-dispatch) can end up trying to run the same side-effecting operation — charge a card, send a payout, provision a resource — EffectFence guarantees exactly one attempt ever executes it: same-instant races are decided by an atomic compare-exchange reservation, and late duplicates get the recorded outcome replayed instead of running again. Every effect that does run gets a content-addressed certificate chained to whatever it was causally built on.

It ships as a Rust library (`effectfence::fence`) and as a stdio [MCP](https://modelcontextprotocol.io) server exposing three tools — `fence_prepare`, `fence_commit`, `fence_abort` — so agents can route side-effecting tool calls through the fence instead of racing each other directly.

## The problem

In a multi-agent gateway, more than one caller can end up trying to run the same effect:

- Two agents independently decide "charge the customer for order #123" needs to happen — at the same instant.
- A supervisor times out waiting for a tool call and re-dispatches it while the original is still in flight.
- A retried or duplicated event triggers the same decision again, minutes after the first attempt already succeeded.

Naively, any of these double-runs the effect. Naively rejecting every duplicate with no memory of outcome is also wrong: if the first attempt crashed, the effect never runs at all, and a duplicate that arrives after success gets an error instead of the result it needs. EffectFence closes all of it with optimistic concurrency control (OCC) plus an intent ledger: attempts don't block each other, exactly one executes, and every other attempt learns what actually happened.

## Architecture

Four pieces compose into the fencing protocol:

**The intent ledger** is what stops *duplicates*, not just races. Every effect carries an `intent` — a stable id for the logical action (e.g. `"charge:order-123"`). Attempts sharing an intent are the same action: the first is admitted and holds a lease; concurrent duplicates are told an attempt is in flight; duplicates arriving after success get the recorded certificate replayed verbatim; duplicates after a failure are fenced (the side effect may or may not have fired — that must be reconciled, not blindly retried) until explicitly cleared. Crashed holders lose their lease after a TTL so the action isn't stuck forever.

**Vector clocks** (`VectorClock`) track causal "happened-before" relationships across agents — one logical counter per agent, joined via elementwise-max `merge`, compared via a `le` partial order, with a `concurrent` check for genuinely unordered events and a stable SHA-256 `digest` for inclusion in certificates.

**OCC read-sets** (`ReadSetEntry`) record the causal dependencies a decision was based on: "when I decided to act, domain `D` was at sequence `S`." Both `prepare_effect_fence` and `commit_effect_cert` validate every entry against live state — if anything moved, the attempt is rejected as stale rather than allowed to act on outdated information.

**CAS domain fencing** is where same-instant races are decided. Each domain (a named contention scope, e.g. `"order:123"`) has an `AtomicU64` sequence counter. The *decision* is a single atomic `compare_exchange` — exactly one concurrent caller can win for any given expected sequence. (Precision note: the counter *lookup* sits behind a short mutex; only the race decision itself is lock-free. Ideas for a fully lock-free path are welcome.)

```
              ┌ intent gate ──────── already done? → Replay(recorded cert)   [do NOT run]
              │                      in flight / failed? → rejected          [do NOT run]
 EffectRequest┤
              ├ read-set check ───── dependency moved? → ReadSetStale        [do NOT run]
              │
              └ domain CAS ────────── lost the race? → DomainRace            [do NOT run]
                     │
                     └ Fresh(ticket) → run the effect → commit_effect_cert → EffectCert
                                                      ↘ abort_effect (failed; fenced until reconciled)
```

Every committed effect becomes an `EffectCert`: a SHA-256 content hash over `{intent, parent, domain, seq, tool, args, result, vector_clock, read_set, agent}`, chained to a `parent` cert hash for causal lineage. Two certs with the same hash are, by definition, records of the same effect — `EffectCert::verify()` recomputes the hash and confirms it hasn't been tampered with or hand-built incorrectly.

### Scope

This is an **in-memory, single-process** fence — state lives behind an `Arc` and is lost on restart. That's enough to close races and duplicates between concurrent threads/tasks in one gateway process. Two things it deliberately does not do (yet):

- **Cross-process/cross-restart fencing.** A horizontally scaled gateway needs the same intent/domain/read-set model backed by a shared store (e.g. `SETNX`+CAS in Redis, or an optimistic version column in Postgres) — the types here are meant to carry over directly to that backend.
- **Enforcement.** The fence protects agents that route their effects through it; it cannot stop an agent that bypasses it entirely. Deploy it at the one choke point your agents share (the gateway process that owns the tools).

Memory is bounded: finished outcomes expire after a configurable TTL (`FenceConfig::result_ttl`, swept by `EffectFence::sweep`), queries never create tracking state, and domain counters are tiny and manually evictable (`evict_domain`).

## Quickstart: library

```rust
use effectfence::fence::{
    prepare_effect_fence, commit_effect_cert, Admission, EffectFence, EffectRequest, VectorClock,
};

let fence = EffectFence::new();

let req = EffectRequest {
    intent: "charge:order-123".into(),   // same action -> same intent, always
    parent: None,                        // hash of the cert this follows, if any
    domain: "order:123".into(),          // contention scope
    tool: "charge_card".into(),
    args: serde_json::json!({"amount_cents": 1999}),
    read_set: vec![],                    // other domains this decision cross-checked
    agent: "agent-a".into(),
    known_clock: VectorClock::new(),
};

match prepare_effect_fence(&fence, req)? {
    Admission::Fresh(prepared) => {
        // This attempt won. Actually run the charge...
        let cert = commit_effect_cert(
            &fence,
            prepared,
            serde_json::json!({"charge_id": "ch_123"}),
        )?;
        assert!(cert.verify());
        // (on failure: abort_effect(&fence, prepared, "why") instead)
    }
    Admission::Replay(cert) => {
        // This exact action already ran -- use cert.result, charge nothing.
    }
}
```

A concurrent duplicate of the same intent gets `Err(FenceError::IntentInFlight)`; a same-instant race on the domain gets `Err(FenceError::DomainRace)`; either way it must not run the effect.

## First: does YOUR stack actually double-fire? (probe it)

Before you install a fence, prove you need one — on your own server, not our demo.
`probe` is a bare MCP client. Point it at any MCP server, and it fires N
byte-identical calls at one tool **concurrently** — the twin-caller race that
happens the instant two agents reach for the same action — then reports how many
*distinct* effects actually landed:

```bash
effectfence probe --tool charge_card --args '{"amount":4900}' --calls 12 -- npx -y @your-org/your-mcp-server
```

```text
EffectFence probe — twin-caller race report
--------------------------------------------
  identical calls   : 12
  DISTINCT effects  : 12

  PROVEN DOUBLE-FIRE. 12 byte-identical calls produced 12 DIFFERENT
  results. Each distinct result is a separate real execution of one
  intended action — the duplicate side effect you cannot take back.
```

It fires **only** the one tool you name, with the exact arguments you supply — it
never enumerates and hammers a server blindly. And it is honest about what it can
see: distinct results are undeniable proof of double-execution; identical results
are reported as *inconclusive from the response*, never as a zero it can't prove.

Then re-run the same probe **through the fence** and watch `DISTINCT effects` drop
to `1`:

```bash
effectfence probe --tool charge_card --args '{"amount":4900}' --calls 12 -- effectfence wrap -- npx -y @your-org/your-mcp-server
```

That is the whole pitch in two commands: the footprints, then the lock.

## Quickstart: wrap an existing MCP server (start here)

The fastest way to use EffectFence is to put it **in front of a tool server you
already run**. Agents don't have to remember to call anything — every tool call is
fenced automatically:

```text
agent/client ──MCP──> effectfence wrap ──MCP──> your real tool server
```

```bash
cargo install effectfence
```

Then wrap whatever server owns your dangerous tools:

```bash
effectfence wrap -- npx -y @your-org/your-mcp-server
```

The tool list is mirrored 1:1 from the child (same names, schemas, docs), so nothing
in your agent changes. What changes: identical duplicate calls — same tool, same
arguments — execute the child **once**; later duplicates get the recorded result
replayed, and concurrent identical calls are refused rather than double-firing.

### One-paste recipe: fence a cluster-mutating server

The case this exists for — several agents with `kubectl` on the same cluster.

**Claude Code:**

```bash
claude mcp add k8s-fenced -- effectfence wrap -- npx -y kubernetes-mcp-server
```

**Cursor** (`~/.cursor/mcp.json`) **or Claude Desktop** (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "k8s-fenced": {
      "command": "effectfence",
      "args": ["wrap", "--", "npx", "-y", "kubernetes-mcp-server"]
    }
  }
}
```

Swap `kubernetes-mcp-server` for whichever server holds your write-bearing tools —
cloud APIs, deploy tooling, a payments server. Point every agent at the fenced name
and remove their access to the raw one; the fence is only a fence if it is the only
door.

Watch it work with `fence_stats` (see below) — `replayed` and `refused` are the
duplicate executions that did not happen.

### Honest scope of wrap

Tools only for now (no resource/prompt passthrough). Intent is derived from
`hash(tool + canonical args)`, so **byte-identical** arguments are treated as the same
action — an agent that varies a timestamp in its arguments defeats dedup, and that
direction fails *safe*: the call runs, nothing is corrupted. State is in-memory per
wrap process; run one fenced gateway per set of production-mutating tools.

---

## Quickstart: MCP server (explicit fencing)

Use this when you want agents to fence deliberately — richer control (`read_set`,
`parent`, `known_clock`) than `wrap` derives automatically.

Listed in the [official MCP Registry](https://registry.modelcontextprotocol.io) as
`mcp-name: io.github.aurumflux20/effectfence`

### Install

With a Rust toolchain ([rustup.rs](https://rustup.rs)):

```bash
cargo install effectfence
```

Or build from a clone of this repo:

```bash
cargo build --release   # binary at ./target/release/effectfence
```

### Add it to Claude

**Claude Code** (one command):

```bash
claude mcp add effectfence -- effectfence
```

(If you built from source instead of `cargo install`, use the full path: `claude mcp add effectfence -- /path/to/target/release/effectfence`.)

**Claude Desktop** — add to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "effectfence": {
      "command": "effectfence"
    }
  }
}
```

**Any project (team-shared)** — commit a `.mcp.json` at the project root:

```json
{
  "mcpServers": {
    "effectfence": {
      "command": "effectfence"
    }
  }
}
```

That's it — no configuration, no environment variables, no accounts. The server holds its fence state in memory for the life of the process.

### The tools

It exposes:

- **`fence_prepare`** — `{ intent, domain, tool, args, agent, read_set?, parent?, known_clock? }` → `{status: "fresh", prepared}` when this attempt wins (run the tool, then report back), or `{status: "already_done", cert}` when this exact action already ran (use the recorded result — do NOT run the tool). Errors mean do not run.
- **`fence_commit`** — `{ prepared, result }` → `{status: "committed", cert}`. Later duplicates of the intent now replay this cert.
- **`fence_abort`** — `{ prepared, reason }` → `{status: "aborted"}`. The intent stays fenced until reconciled and cleared.
- **`fence_stats`** — no arguments → live counters since the process started:
  `admitted` (effects that ran), `replayed` (duplicates handed a recorded result),
  `refused` broken out by cause (`stale_read_set`, `domain_race`, `in_flight`,
  `prior_failure`), plus `total_attempts` and `prevented`. `prevented` is the number
  that matters: every attempt that did **not** run the effect.

  ```
  effectfence since boot: admitted=1 replayed=995 refused(stale=0 race=4 in-flight=0 failed=0) total=1000 prevented=999
  ```

Tool input schemas are generated automatically from the Rust types (via `schemars`), so any MCP client can introspect them with `tools/list`.

## Testing

```bash
cargo test    # unit tests + chaos tests
cargo clippy --all-targets
```

`tests/chaos_test.rs` uses real OS threads to prove the two guarantees separately: a forced same-instant domain race (synchronization deliberately constructed so the collision is guaranteed, not hoped for) admits exactly one winner every time, and 16 concurrent duplicates of one intent admit exactly one execution — with late duplicates replaying the committed cert. A 32-thread stress test additionally asserts sequence numbers are never double-allocated.

## Scan your own MCP server

`tools/fencescan.py` finds tools in an MCP server that could fire the same effect
twice. No install, no dependencies, no network:

```bash
python3 tools/fencescan.py /path/to/your-mcp-server
```

It reports **candidates with evidence** and deliberately renders **no verdict**,
because an outsider reading a repository usually cannot prove a double-fire —
the guard often lives in a service the repo calls, or in a sibling SDK, and a
tool whose name sounds like a write may only return a payload for someone else
to sign. Output includes an explicit list of what it cannot see.

It was rewritten after hand-verification killed 4 of its first 7 "confirmations".
Each failure is now a fixed behaviour rather than a caveat:

| It got this wrong | Why | Now |
|---|---|---|
| Flagged read-only tools | A flat window after a tool name ran into the *next* tool, so reads inherited writes' vocabulary | Brace-matched to the tool's own block; a read verb in the name vetoes |
| Said a repo had no idempotency when it had a whole module | `\b(idempot…)` cannot match `deriveIdempotencyKey` — no word boundary before a camelCase capital | Anchors removed; the same blindness hid `requestId`, `clientToken` |
| Found no writes anywhere | Writes live in shared helpers, not in the tool declaration | Collected per repo as corroboration, never claimed as "this tool writes" |
| Missed `method: cond ? "POST" : "GET"` | String literals were stripped before matching, deleting the HTTP verb itself | Matched on the raw line |
| Printed "AT RISK" | That is an accusation, and it was wrong 4 times in 7 | No verdict field exists |

If it flags something in your server and you want a second pair of eyes, open an
issue — a wrong accusation costs more than a missed one, so a false positive here
is worth reporting too.

## Sibling project — `once` (Python)

Same problem, other runtime. [**once**](https://github.com/aurumflux20/once-kernel) (`pip install once-kernel`) is the Python idempotency kernel built on the same idea: a side effect runs exactly once under retries, webhook redelivery, and concurrent workers. It goes further on durability — a Postgres store, heartbeat leases with fence tokens so a stale worker can't resurrect after its lease is reclaimed, and RFC 8785 canonical payload fingerprints.

Use EffectFence when your fence lives in Rust or in front of an MCP server; use `once` when the side effect is Python and you want a durable store. `effectfence wrap` has been proven fencing `once`'s own MCP server.

## Commercial support

The libraries are free and stay free. If you want help applying them to a
codebase that already moves money — a fixed-scope audit of every side-effecting
path, storm-tested, with a CI test that keeps it fenced — see
[SUPPORT.md](SUPPORT.md) or email **hello@aurumflux.co**.

If it isn't a fit we'll say so.

## License

MIT — see [LICENSE](LICENSE).
