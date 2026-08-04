# EffectFence

**Your swarm doesn't need more memory. It needs a causal fence around tool side effects.**

EffectFence is a causal concurrency fence for multi-agent tool calls. When more than one agent (or retry, or re-dispatch) can end up trying to run the same side-effecting operation — charge a card, send a payout, provision a resource — EffectFence guarantees exactly one of them wins, and gives every effect that actually runs a content-addressed certificate chained to whatever it was causally built on.

It ships as a Rust library (`core_rs::fence`) and as a stdio [MCP](https://modelcontextprotocol.io) server exposing two tools, `fence_prepare` and `fence_commit`, so agents can route side-effecting tool calls through the fence instead of racing each other directly.

## The problem

In a multi-agent gateway, more than one caller can end up trying to run the same effect at (almost) the same time:

- Two agents independently decide "charge the customer for order #123" needs to happen.
- A supervisor times out waiting for a tool call and re-dispatches it while the original is still in flight.
- A retried or duplicated event triggers the same decision twice.

Naively, either of these can double-run the effect. Naively rejecting every concurrent attempt with no memory of outcome can also mean the effect never runs if the original caller crashed. EffectFence closes this with optimistic concurrency control (OCC) instead of locks: attempts don't block each other, they race, and the fence deterministically decides exactly one winner.

## Architecture

Three pieces compose into the fencing protocol:

**Vector clocks** (`VectorClock`) track causal "happened-before" relationships across agents — one logical counter per agent, joined via elementwise-max `merge`, compared via a `le` partial order, with a `concurrent` check for genuinely unordered events and a stable SHA-256 `digest` for inclusion in certificates.

**OCC read-sets** (`ReadSetEntry`) record the causal dependencies a decision was based on: "when I decided to act, domain `D` was at sequence `S`." Both `prepare_effect_fence` and `commit_effect_cert` validate every entry against live state — if anything moved, the attempt is rejected as stale rather than allowed to act on outdated information.

**Lock-free domain fencing** (`DomainFence`) is where races are actually decided. Each domain (a named contention scope, e.g. `"order:123"`) has an `AtomicU64` sequence counter. Reserving the next slot is a single `compare_exchange` — no locks held across the decision, and exactly one concurrent caller can win for any given expected sequence.

```
   agent A ──┐                     ┌── Ok(seq=N+1)  → run the effect → commit_effect_cert → EffectCert
             ├─ prepare_effect_fence(domain, read_set, ...) ─ DomainFence::reserve (CAS) ─┤
   agent B ──┘                     └── Err(DomainRace)     → do NOT run the effect
```

Every committed effect becomes an `EffectCert`: a SHA-256 content hash over `{parent, domain, seq, tool, args, result, vector_clock, read_set, agent}`, chained to a `parent` cert hash for causal lineage. Two certs with the same hash are, by definition, records of the same effect — `EffectCert::verify()` recomputes the hash and confirms it hasn't been tampered with or hand-built incorrectly.

### Scope

This is an **in-memory, single-process** fence — `DomainFence` state lives behind an `Arc` and is lost on restart. That's enough to close races between concurrent threads/tasks in one gateway process. A gateway horizontally scaled across multiple processes needs the same domain/read-set/reservation model backed by a shared store instead (e.g. `SETNX`+CAS in Redis, or an optimistic version column in Postgres) — the types here are meant to carry over directly to that backend.

## Quickstart: library

```rust
use core_rs::fence::{
    prepare_effect_fence, commit_effect_cert, DomainFence, VectorClock,
};

let fence = DomainFence::new();
let clock = VectorClock::new();

let prepared = prepare_effect_fence(
    &fence,
    None,                                   // parent cert hash, if any
    "order:123",                            // domain
    "charge_card",                          // tool
    serde_json::json!({"amount_cents": 1999}),
    vec![],                                 // read_set (other domains cross-checked)
    "agent-a",
    &clock,
)?;

// ... actually run the charge ...

let cert = commit_effect_cert(&fence, prepared, serde_json::json!({"charge_id": "ch_123"}))?;
assert!(cert.verify());
```

If a second caller races the same domain from the same observed state, its `prepare_effect_fence` call returns `Err(FenceError::DomainRace { .. })` instead of a ticket — it must not run the effect.

## Quickstart: MCP server

Build and run the stdio server:

```bash
cargo build --release
./target/release/core-rs
```

Point any MCP-compatible client (Claude Code, Claude Desktop, etc.) at the binary as a stdio server. It exposes:

- **`fence_prepare`** — `{ domain, tool, args, agent, read_set?, parent?, known_clock? }` → a `PreparedEffect` ticket (JSON) on success, or a `DomainRace` / `ReadSetStale` error. Call this *before* running a side-effecting tool call.
- **`fence_commit`** — `{ prepared, result }` → a verifiable `EffectCert` (JSON) on success, or a `ReadSetStale` error if something the effect depended on changed while it was running. Call this *after* the tool call finishes.

Tool input schemas are generated automatically from the Rust types (via `schemars`), so any MCP client can introspect them with `tools/list`.

## Testing

```bash
cargo test    # unit tests + chaos test
cargo clippy --all-targets
```

`tests/chaos_test.rs` spins up real OS threads to force a genuine race on the same domain (not a hoped-for one — the synchronization is deliberately built so the collision is guaranteed by construction) and asserts exactly one winner, every time, plus a higher-concurrency stress test asserting no two agents are ever allocated the same sequence number.

## License

MIT — see [LICENSE](LICENSE).
