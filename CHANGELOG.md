# Changelog

All notable changes to EffectFence.

## [0.1.1] — 2026-08-04

### Fixed
- **Precision: retired the "lock-free" claim.** The race *decision* is a
  single atomic `compare_exchange`, but the counter lookup sits behind a
  short mutex — so "lock-free" overstated the hot path. Docs, README,
  crate description, and comments now say exactly what the code does.
  For a correctness library, wording is part of the API.

## [Unreleased]
- Roadmap: crates.io publish · docs.rs reference · wrap-mode proxy (auto-fence any MCP server, zero code change) · distributed backend exploration

## [0.1.0] — 2026-08-04

First public release.

### Added
- **Intent ledger** — attempts sharing an intent are the same logical action:
  one executes, later duplicates replay the recorded certificate; failed or
  unknown outcomes stay fenced until explicitly reconciled (`clear_intent`).
  Crashed holders lose their lease after a TTL.
- **CAS domain fencing** — same-instant races decided by a single
  `compare_exchange`; exactly one winner per expected sequence.
- **Content-addressed EffectCerts** — SHA-256-sealed records of every
  committed effect with causal `parent` chaining and per-agent vector clocks.
- **MCP stdio server** — `fence_prepare` / `fence_commit` / `fence_abort`
  tools for MCP-compatible agents; schemas auto-generated from Rust types.
- **Storm demo** (`cargo run --release --example storm`) — 1,000-attempt
  attack proving exactly-one execution; also enforced in CI on every commit.
- 19 tests: unit + forced-race chaos + live JSON-RPC wire test. CI: test
  matrix, no-default-features build, clippy `-D warnings`, rustfmt, storm gate.

### Honest scope
In-memory, single-process, advisory (fences calls routed through it).
The distributed, unbypassable gateway is roadmap, not fine print.
