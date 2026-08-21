# Commercial support

`effectfence` and [`once`](https://github.com/aurumflux20/once-kernel) are free
and MIT/Apache licensed. Use them, fork them, ship them — no strings.

Some teams want help applying them to a codebase that already has money moving
through it. That is what we do.

The one-sentence version of what you get: **done means done on your sends** —
"success" is only spoken after the operation is admitted exactly once and
receipted, never on a self-report. We run this on our own production mail
path; the audit wires the same discipline into yours.

## Retry Safety Review — $1,200, refunded in full if we find nothing

We read one money path in your codebase — the tool, SDK or service that actually
moves funds — and hunt one specific class of defect: **what happens when a
payment fails ambiguously.** Not "is there an idempotency key" (most competent
teams have that), but the timeout *after* the money may already have moved — the
aborted request that settled anyway, the retry that mints a fresh nonce, the
reservation released on a failure that wasn't one.

**You get:** a written report, each finding tied to your own file and line
numbers, a reproduction where one is possible, and a recommended fix. Five
working days. Written only — no calls.

**You pay $1,200 up front. If we find no real defect, we refund it in full** —
and you keep the report saying so.

**[Book a review — $1,200](https://buy.stripe.com/28E7sL91C9naapQbBVdIA0l)** · after checkout, reply to the receipt with
the repository and which money path matters most. Or email **hello@aurumflux.co**
first if you'd rather talk it through — and if we don't think we can find
anything, we say so before you pay.

For a deeper engagement — installing the full write-authority gateway on a money
path — see [github.com/aurumflux20/seal](https://github.com/aurumflux20/seal).

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
