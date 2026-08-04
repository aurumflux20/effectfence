# EffectFence

[![CI](https://github.com/aurumflux20/effectfence/actions/workflows/ci.yml/badge.svg)](https://github.com/aurumflux20/effectfence/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**Your swarm doesn't need more memory. It needs a causal fence around tool side effects.**

EffectFence is a causal concurrency fence for multi-agent tool calls. When more than one agent (or retry, or re-dispatch) can end up trying to run the same side-effecting operation — charge a card, send a payout, provision a resource — EffectFence guarantees exactly one attempt ever executes it: same-instant races are decided by a lock-free reservation, and late duplicates get the recorded outcome replayed instead of running again. Every effect that does run gets a content-addressed certificate chained to whatever it was causally built on.

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

**Lock-free domain fencing** is where same-instant races are decided. Each domain (a named contention scope, e.g. `"order:123"`) has an `AtomicU64` sequence counter. Reserving the next slot is a single `compare_exchange` — no locks held across the decision, and exactly one concurrent caller can win for any given expected sequence.

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

## Quickstart: MCP server

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

Tool input schemas are generated automatically from the Rust types (via `schemars`), so any MCP client can introspect them with `tools/list`.

## Testing

```bash
cargo test    # unit tests + chaos tests
cargo clippy --all-targets
```

`tests/chaos_test.rs` uses real OS threads to prove the two guarantees separately: a forced same-instant domain race (synchronization deliberately constructed so the collision is guaranteed, not hoped for) admits exactly one winner every time, and 16 concurrent duplicates of one intent admit exactly one execution — with late duplicates replaying the committed cert. A 32-thread stress test additionally asserts sequence numbers are never double-allocated.

## License

MIT — see [LICENSE](LICENSE).
