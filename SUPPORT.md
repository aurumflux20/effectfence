# Commercial support

`effectfence` and [`once`](https://github.com/aurumflux20/once-kernel) are free
and MIT/Apache licensed. Use them, fork them, ship them — no strings.

Some teams want help applying them to a codebase that already has money moving
through it. That is what we do.

The one-sentence version of what you get: **done means done on your sends** —
"success" is only spoken after the operation is admitted exactly once and
receipted, never on a self-report. We run this on our own production mail
path; the audit wires the same discipline into yours.

## The Fence Audit — 5 working days, fixed scope

**Day 1 — Inventory.** Every side-effecting call in your codebase: what fires,
from where, under what retry and concurrency conditions. You keep the list
whether or not you continue.

**Day 2 — Attack.** We build a storm harness against your top three money or
message paths — concurrent racers plus late retries — and try to make each fire
twice. Findings are reproducible, not theoretical.

**Day 3 — The three decisions**, in a working session, because they take
judgement and cannot be automated:
- **What is the key?** What makes two attempts "the same operation". Get it
  wrong and you either block legitimate work or let real duplicates through.
- **What goes in the fingerprint?** Timestamps, trace ids and retry counters
  have to be excluded, or every retry looks like a different payload.
- **Fail-open or fail-closed, per effect?** A charge must fail closed. A
  notification usually should not. Most teams have never decided this explicitly.

**Day 4 — Wire it in.** Fence the top three paths. Both libraries are yours to
keep; there is no lock-in and nothing to host.

**Day 5 — Prove it.** A storm test in *your* CI that fails the build if a fenced
effect ever executes twice, plus a runbook: what is fenced, what is not, and what
to do when a fence trips at 3am.

Ongoing support and enterprise arrangements exist too. Ask.

## What this is not

Not a security audit. Not a performance review. Not a rewrite — we fence what you
already have. We never sit in your payment path or host anything.

**If we find nothing, we say so** and explain why your existing guards hold. The
inventory is still yours.

## Honest limits, before you ask

`effectfence` is in-memory and single-process today; a durable store is on the
roadmap rather than hidden in the small print. `once` has durable SQLite and
Postgres stores. Neither does exactly-once *delivery* — that is physically
impossible, and anyone claiming otherwise is selling you something. What is
guaranteed is exactly-once **execution**, with at-least-once result delivery.

## Why us

The storm demo — 1,000 concurrent duplicate attempts, exactly one execution —
runs in CI on every commit and fails the build if it ever elects two winners. You
can run it yourself in one command before you talk to us:

```bash
cargo run --release --example storm
```

We built the fence because our own system double-sent an email to a real
prospect. The "already sent" marker lived in a mutable field the handler also
wrote to, so an update clobbered it before the guard read it. The dedup table was
working the whole time — it just wasn't the thing being read. Nobody had written
a bad line of code.

That is what these bugs look like. They don't fail loudly. They succeed twice.

---

**hello@aurumflux.co** — say what your agents touch and where a duplicate would
hurt most. If it's not a fit, we'll tell you that instead.
