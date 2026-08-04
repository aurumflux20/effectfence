//! # Causal Effect Fence
//!
//! Optimistic-concurrency-control (OCC) fencing for side-effecting tool
//! calls in a multi-agent gateway. Prevents two agents from both
//! "winning" a race to mutate the same domain (double-charges, duplicate
//! provisioning, ...) and gives every committed effect a
//! content-addressed certificate that causally chains to what it was
//! built on.
//!
//! ## The model
//!
//! - A **domain** is a named contention scope (e.g. `"order:123"`,
//!   `"account:456"`) with a monotonically increasing sequence number,
//!   tracked by [`DomainFence`].
//! - A **read set** ([`ReadSetEntry`]) records which domains (and at
//!   what sequence) an agent's decision to act was based on — its causal
//!   dependencies.
//! - [`prepare_effect_fence`] validates the read set against current
//!   reality and, if it still holds, atomically reserves the next
//!   sequence number in the target domain via a lock-free
//!   `compare_exchange` — this is the moment a race between two agents
//!   is decided: exactly one reservation wins.
//! - The caller then actually executes the tool/effect and calls
//!   [`commit_effect_cert`], which re-validates the read set one more
//!   time (the effect may have taken a while — the world could have
//!   moved since `prepare`) and, if it still holds, mints an
//!   [`EffectCert`]: a SHA-256, content-addressed record of exactly what
//!   happened, chained to a `parent` certificate for causal lineage.
//!
//! Two agents racing to act on the same domain from the same observed
//! state will have identical `expected_seq` values in
//! [`prepare_effect_fence`]; the `compare_exchange` inside
//! [`DomainFence::reserve`] guarantees only one of them advances the
//! counter. The loser gets [`FenceError::DomainRace`] and must not run
//! its effect.
//!
//! ## Scope
//!
//! This is an in-memory, single-process fence: `DomainFence` state lives
//! behind an `Arc` and is lost on restart. For a gateway scaled across
//! multiple processes, the same domain/read-set/reservation model would
//! need to be backed by a shared store (e.g. a `SETNX`+CAS in Redis, or
//! a row with an optimistic version column in Postgres).

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// A vector clock: one logical counter per agent, used to track causal
/// "happened-before" relationships between effects across a multi-agent
/// swarm.
///
/// Internally backed by a `BTreeMap` (rather than a `HashMap`)
/// specifically so its serialization — and therefore its contribution to
/// an [`EffectCert`]'s content hash — is always in a canonical, sorted
/// key order regardless of insertion order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VectorClock(BTreeMap<String, u64>);

impl VectorClock {
    /// An empty clock (all agents implicitly at 0).
    pub fn new() -> Self {
        Self::default()
    }

    /// This agent's current counter value (0 if never ticked).
    pub fn get(&self, agent: &str) -> u64 {
        self.0.get(agent).copied().unwrap_or(0)
    }

    /// Increment `agent`'s counter and return the new value. Call this
    /// once per causal step an agent takes.
    pub fn tick(&mut self, agent: &str) -> u64 {
        let counter = self.0.entry(agent.to_string()).or_insert(0);
        *counter += 1;
        *counter
    }

    /// Merge `other` into `self`, taking the elementwise maximum of
    /// every agent's counter (the standard vector-clock join). Call this
    /// when an agent observes another agent's clock, so its own view of
    /// "what has happened" catches up.
    pub fn merge(&mut self, other: &VectorClock) {
        for (agent, &count) in &other.0 {
            let entry = self.0.entry(agent.clone()).or_insert(0);
            if count > *entry {
                *entry = count;
            }
        }
    }

    /// Partial order: `true` iff every component of `self` is `<=` the
    /// corresponding component of `other` (a component absent from
    /// either clock is treated as 0). This is "self happened-before-or-
    /// equal-to other".
    pub fn le(&self, other: &VectorClock) -> bool {
        self.0
            .iter()
            .all(|(agent, &count)| other.get(agent) >= count)
    }

    /// `true` iff `self` and `other` are incomparable under [`le`](Self::le)
    /// in either direction — genuinely concurrent, causally unordered
    /// events.
    pub fn concurrent(&self, other: &VectorClock) -> bool {
        !self.le(other) && !other.le(self)
    }

    /// A stable SHA-256 digest of this clock's contents, hex-encoded.
    pub fn digest(&self) -> String {
        let bytes =
            serde_json::to_vec(&self.0).expect("BTreeMap<String, u64> serialization cannot fail");
        hex_encode(&Sha256::digest(&bytes))
    }
}

/// One entry in an effect's read set: "when this decision was made,
/// `domain` was at sequence `seq`".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReadSetEntry {
    pub domain: String,
    pub seq: u64,
}

impl ReadSetEntry {
    pub fn new(domain: impl Into<String>, seq: u64) -> Self {
        Self {
            domain: domain.into(),
            seq,
        }
    }
}

/// Lock-free, per-domain sequence counters.
///
/// Cheap to clone — it's an `Arc` handle to shared state. Clone it into
/// every task/thread/handler that needs to prepare or commit effects.
#[derive(Debug, Clone, Default)]
pub struct DomainFence {
    domains: Arc<Mutex<HashMap<String, Arc<AtomicU64>>>>,
}

impl DomainFence {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (creating if necessary) the atomic counter backing `domain`.
    /// The map lookup/insert is behind a short-held `Mutex`, but the
    /// actual sequence reservation below never holds it — that's a
    /// lock-free `compare_exchange` on the returned `Arc<AtomicU64>`.
    fn counter_for(&self, domain: &str) -> Arc<AtomicU64> {
        let mut domains = self.domains.lock().expect("domain fence mutex poisoned");
        domains
            .entry(domain.to_string())
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone()
    }

    /// The current sequence number for `domain` (0 if never reserved).
    pub fn current(&self, domain: &str) -> u64 {
        self.counter_for(domain).load(Ordering::SeqCst)
    }

    /// Atomically reserve the next sequence number for `domain`, but
    /// only if it is still at `expected_seq`.
    ///
    /// On success, returns `Ok(expected_seq + 1)` — the caller now
    /// exclusively owns that slot in the domain's causal order. On
    /// failure (someone else already advanced the domain), returns
    /// `Err(actual_seq)` with the value that won instead. This is the
    /// single point where a race between concurrent agents on the same
    /// domain is decided; exactly one caller can win for any given
    /// `expected_seq`.
    pub fn reserve(&self, domain: &str, expected_seq: u64) -> Result<u64, u64> {
        let counter = self.counter_for(domain);
        let next_seq = expected_seq + 1;
        counter
            .compare_exchange(expected_seq, next_seq, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_previous| next_seq)
    }
}

/// A content-addressed record of a committed effect.
///
/// The `hash` field is a SHA-256 digest, hex-encoded, computed over
/// every other field (see [`verify`](Self::verify)) — including
/// `parent`, which chains this certificate to whatever it was causally
/// built on. Two certs with the same hash are, by definition, records of
/// the exact same effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EffectCert {
    /// Hex SHA-256 hash of the certificate this one causally follows, or
    /// `None` for a genesis effect with no prior cause.
    pub parent: Option<String>,
    pub domain: String,
    pub seq: u64,
    pub tool: String,
    pub args: Value,
    pub result: Value,
    pub vector_clock: VectorClock,
    pub read_set: Vec<ReadSetEntry>,
    pub agent: String,
    /// Hex-encoded SHA-256 digest of every field above. See
    /// [`verify`](Self::verify).
    pub hash: String,
}

/// The exact set of fields hashed into an [`EffectCert::hash`], in a
/// fixed field order, so the digest is reproducible and independent of
/// how `EffectCert`'s own fields happen to be declared.
#[derive(Serialize)]
struct CertPreimage<'a> {
    parent: &'a Option<String>,
    domain: &'a str,
    seq: u64,
    tool: &'a str,
    args: &'a Value,
    result: &'a Value,
    vector_clock: &'a VectorClock,
    read_set: &'a [ReadSetEntry],
    agent: &'a str,
}

impl EffectCert {
    #[allow(clippy::too_many_arguments)]
    fn new(
        parent: Option<String>,
        domain: String,
        seq: u64,
        tool: String,
        args: Value,
        result: Value,
        vector_clock: VectorClock,
        read_set: Vec<ReadSetEntry>,
        agent: String,
    ) -> Self {
        let hash = Self::compute_hash(
            &parent,
            &domain,
            seq,
            &tool,
            &args,
            &result,
            &vector_clock,
            &read_set,
            &agent,
        );
        Self {
            parent,
            domain,
            seq,
            tool,
            args,
            result,
            vector_clock,
            read_set,
            agent,
            hash,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn compute_hash(
        parent: &Option<String>,
        domain: &str,
        seq: u64,
        tool: &str,
        args: &Value,
        result: &Value,
        vector_clock: &VectorClock,
        read_set: &[ReadSetEntry],
        agent: &str,
    ) -> String {
        let preimage = CertPreimage {
            parent,
            domain,
            seq,
            tool,
            args,
            result,
            vector_clock,
            read_set,
            agent,
        };
        let bytes = serde_json::to_vec(&preimage).expect("CertPreimage serialization cannot fail");
        hex_encode(&Sha256::digest(&bytes))
    }

    /// Recompute the hash from this certificate's own fields and compare
    /// it to the stored [`hash`](Self::hash). `false` means the
    /// certificate has been tampered with (or hand-constructed
    /// incorrectly) since it no longer matches its own content address.
    pub fn verify(&self) -> bool {
        let expected = Self::compute_hash(
            &self.parent,
            &self.domain,
            self.seq,
            &self.tool,
            &self.args,
            &self.result,
            &self.vector_clock,
            &self.read_set,
            &self.agent,
        );
        expected == self.hash
    }
}

/// The output of a successful [`prepare_effect_fence`] call: proof that
/// the caller exclusively holds `seq` in `domain`, plus everything
/// needed to mint an [`EffectCert`] once the effect's `result` is known.
/// Pass this to [`commit_effect_cert`] when the tool call finishes.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreparedEffect {
    pub parent: Option<String>,
    pub domain: String,
    pub seq: u64,
    pub tool: String,
    pub args: Value,
    pub vector_clock: VectorClock,
    pub read_set: Vec<ReadSetEntry>,
    pub agent: String,
}

/// Rejections raised by [`prepare_effect_fence`] / [`commit_effect_cert`].
/// Both are "the world moved since you looked" errors — the caller must
/// not run (or must not trust the result of) the effect it was about to
/// commit.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FenceError {
    /// A domain in the effect's read set has moved past the sequence the
    /// caller observed — its decision was based on stale data.
    #[error(
        "read set entry for domain `{domain}` is stale: expected seq {expected}, actual {actual}"
    )]
    ReadSetStale {
        domain: String,
        expected: u64,
        actual: u64,
    },

    /// The target domain itself moved between the caller observing its
    /// sequence and attempting to reserve the next one — another caller
    /// won the race.
    #[error(
        "domain `{domain}` race: expected to reserve seq {expected}, but the domain was already at {actual}"
    )]
    DomainRace {
        domain: String,
        expected: u64,
        actual: u64,
    },
}

/// Validate `read_set` against the current state of `fence`, and, if it
/// still holds, atomically reserve the next sequence number for `domain`.
///
/// This is the read phase of optimistic concurrency control: it never
/// blocks, and it either wins the reservation outright or fails cleanly
/// with [`FenceError::ReadSetStale`] (a causal dependency moved) or
/// [`FenceError::DomainRace`] (another caller won the target domain).
///
/// `known_clock` is the calling agent's current view of causal history;
/// the returned [`PreparedEffect`] carries a clone of it with `agent`'s
/// counter ticked forward by one.
///
/// `read_set` should list *other* domains this decision cross-checked —
/// not `domain` itself. `domain`'s own sequence is about to be advanced
/// by this very call (that's what the reservation is), so a read-set
/// entry recording `domain`'s pre-reservation seq would go stale the
/// instant that happens, and [`commit_effect_cert`] would always reject
/// it. Same-domain lineage (`this effect follows that earlier effect on
/// the same domain`) belongs in `parent`, not `read_set`.
#[allow(clippy::too_many_arguments)]
pub fn prepare_effect_fence(
    fence: &DomainFence,
    parent: Option<String>,
    domain: impl Into<String>,
    tool: impl Into<String>,
    args: Value,
    read_set: Vec<ReadSetEntry>,
    agent: impl Into<String>,
    known_clock: &VectorClock,
) -> Result<PreparedEffect, FenceError> {
    let domain = domain.into();
    let tool = tool.into();
    let agent = agent.into();

    for entry in &read_set {
        let actual = fence.current(&entry.domain);
        if actual != entry.seq {
            return Err(FenceError::ReadSetStale {
                domain: entry.domain.clone(),
                expected: entry.seq,
                actual,
            });
        }
    }

    let expected_seq = fence.current(&domain);
    let seq = fence
        .reserve(&domain, expected_seq)
        .map_err(|actual| FenceError::DomainRace {
            domain: domain.clone(),
            expected: expected_seq,
            actual,
        })?;

    let mut vector_clock = known_clock.clone();
    vector_clock.tick(&agent);

    Ok(PreparedEffect {
        parent,
        domain,
        seq,
        tool,
        args,
        vector_clock,
        read_set,
        agent,
    })
}

/// Finalize a [`PreparedEffect`] into an [`EffectCert`] once the tool
/// call's `result` is known.
///
/// This re-validates every entry in the prepared read set against
/// `fence` one more time — the same check [`prepare_effect_fence`]
/// performed, re-run because the effect's execution may have taken long
/// enough for one of its causal dependencies to have moved in the
/// meantime. This doubles as the certificate's causal-lineage check:
/// the read set *is* the set of causal dependencies this effect's
/// result is only valid under, so re-affirming it before minting the
/// cert is exactly what "this cert's lineage is still valid" means here.
///
/// Note that `domain`'s own sequence reservation (from `prepare`) is
/// never revisited here: other agents are expected to keep advancing
/// the same domain concurrently, and a gap left by an effect that never
/// committed is a normal, harmless outcome — not a correctness issue.
pub fn commit_effect_cert(
    fence: &DomainFence,
    prepared: PreparedEffect,
    result: Value,
) -> Result<EffectCert, FenceError> {
    for entry in &prepared.read_set {
        let actual = fence.current(&entry.domain);
        if actual != entry.seq {
            return Err(FenceError::ReadSetStale {
                domain: entry.domain.clone(),
                expected: entry.seq,
                actual,
            });
        }
    }

    Ok(EffectCert::new(
        prepared.parent,
        prepared.domain,
        prepared.seq,
        prepared.tool,
        prepared.args,
        result,
        prepared.vector_clock,
        prepared.read_set,
        prepared.agent,
    ))
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: serde_json::Value) -> Value {
        v
    }

    #[test]
    fn vector_clock_tick_and_get() {
        let mut vc = VectorClock::new();
        assert_eq!(vc.get("agent-a"), 0);
        assert_eq!(vc.tick("agent-a"), 1);
        assert_eq!(vc.tick("agent-a"), 2);
        assert_eq!(vc.get("agent-a"), 2);
        assert_eq!(vc.get("agent-b"), 0);
    }

    #[test]
    fn vector_clock_merge_takes_elementwise_max() {
        let mut a = VectorClock::new();
        a.tick("x"); // x=1
        a.tick("x"); // x=2
        let mut b = VectorClock::new();
        b.tick("x"); // x=1
        b.tick("y"); // y=1
        b.tick("y"); // y=2

        a.merge(&b);
        assert_eq!(a.get("x"), 2);
        assert_eq!(a.get("y"), 2);
    }

    #[test]
    fn vector_clock_partial_order_and_concurrency() {
        let mut a = VectorClock::new();
        a.tick("x");
        let mut b = a.clone();
        b.tick("x"); // b strictly ahead of a on x

        assert!(a.le(&b));
        assert!(!b.le(&a));
        assert!(!a.concurrent(&b));

        let mut c = VectorClock::new();
        c.tick("y"); // c has y=1, x=0 -- incomparable with a (x=1, y=0)
        assert!(!a.le(&c));
        assert!(!c.le(&a));
        assert!(a.concurrent(&c));
    }

    #[test]
    fn vector_clock_digest_is_stable_and_order_independent() {
        let mut a = VectorClock::new();
        a.tick("agent-a");
        a.tick("agent-b");

        let mut b = VectorClock::new();
        b.tick("agent-b");
        b.tick("agent-a");

        assert_eq!(a, b);
        assert_eq!(a.digest(), b.digest());
    }

    #[test]
    fn domain_fence_reserve_advances_and_rejects_stale_expectation() {
        let fence = DomainFence::new();
        assert_eq!(fence.current("d"), 0);

        assert_eq!(fence.reserve("d", 0), Ok(1));
        assert_eq!(fence.current("d"), 1);

        // Retrying with the now-stale expectation of 0 must fail with
        // the actual current value.
        assert_eq!(fence.reserve("d", 0), Err(1));

        assert_eq!(fence.reserve("d", 1), Ok(2));
        assert_eq!(fence.current("d"), 2);
    }

    #[test]
    fn prepare_then_commit_happy_path_produces_a_verifiable_cert() {
        let fence = DomainFence::new();
        let clock = VectorClock::new();

        let prepared = prepare_effect_fence(
            &fence,
            None,
            "order:1",
            "charge_card",
            args(serde_json::json!({"amount_cents": 1999})),
            vec![],
            "agent-a",
            &clock,
        )
        .expect("first attempt on a free domain must succeed");

        assert_eq!(prepared.seq, 1);
        assert_eq!(prepared.vector_clock.get("agent-a"), 1);

        let cert = commit_effect_cert(&fence, prepared, serde_json::json!({"charge_id": "ch_123"}))
            .expect("commit with an untouched read set must succeed");

        assert!(cert.verify());
        assert_eq!(cert.domain, "order:1");
        assert_eq!(cert.seq, 1);
        assert!(cert.parent.is_none());
    }

    #[test]
    fn commit_fails_if_a_read_set_dependency_moved_since_prepare() {
        let fence = DomainFence::new();
        let clock = VectorClock::new();

        // Establish a dependency domain at seq 1.
        fence.reserve("inventory:sku-1", 0).unwrap();

        let prepared = prepare_effect_fence(
            &fence,
            None,
            "order:2",
            "reserve_inventory",
            args(serde_json::json!({})),
            vec![ReadSetEntry::new("inventory:sku-1", 1)],
            "agent-a",
            &clock,
        )
        .expect("read set matches current state");

        // Something else advances the dependency while our effect is
        // "running".
        fence.reserve("inventory:sku-1", 1).unwrap();

        let err = commit_effect_cert(&fence, prepared, serde_json::json!({}))
            .expect_err("dependency moved -- commit must be rejected");
        assert_eq!(
            err,
            FenceError::ReadSetStale {
                domain: "inventory:sku-1".to_string(),
                expected: 1,
                actual: 2,
            }
        );
    }

    #[test]
    fn prepare_succeeds_sequentially_even_after_other_reservations_on_the_domain() {
        // `prepare_effect_fence` always reads the domain's current
        // sequence immediately before reserving the next one, so in a
        // single-threaded test there is no window for another actor to
        // invalidate that read -- it just picks up wherever the domain
        // currently is. `FenceError::DomainRace` can only actually be
        // observed when two callers race concurrently for the same
        // `expected_seq`; see `tests/chaos_test.rs` for that scenario.
        let fence = DomainFence::new();
        let clock = VectorClock::new();

        fence.reserve("order:3", 0).unwrap(); // domain is now at seq 1

        let prepared = prepare_effect_fence(
            &fence,
            None,
            "order:3",
            "charge_card",
            args(serde_json::json!({})),
            vec![],
            "agent-b",
            &clock,
        )
        .expect("prepare reads current state fresh, so it just claims the next slot");

        assert_eq!(prepared.seq, 2);
    }

    #[test]
    fn cert_chain_records_parent_for_causal_lineage() {
        let fence = DomainFence::new();
        let clock = VectorClock::new();

        let first = commit_effect_cert(
            &fence,
            prepare_effect_fence(
                &fence,
                None,
                "order:4",
                "charge_card",
                args(serde_json::json!({})),
                vec![],
                "agent-a",
                &clock,
            )
            .unwrap(),
            serde_json::json!({"charge_id": "ch_1"}),
        )
        .unwrap();

        // Note: same-domain lineage is expressed via `parent`, not via a
        // read-set entry for "order:4" itself -- `prepare` is about to
        // advance order:4's own sequence as part of reserving it, so a
        // read-set entry recording its pre-reservation seq would go
        // stale the instant that reservation happens, and `commit`
        // would always reject it. `read_set` is for *other* domains
        // this decision cross-checked.
        let second = commit_effect_cert(
            &fence,
            prepare_effect_fence(
                &fence,
                Some(first.hash.clone()),
                "order:4",
                "send_receipt",
                args(serde_json::json!({})),
                vec![],
                "agent-b",
                &first.vector_clock,
            )
            .unwrap(),
            serde_json::json!({"receipt_id": "rc_1"}),
        )
        .unwrap();

        assert_eq!(second.parent, Some(first.hash));
        assert!(second.vector_clock.get("agent-a") >= 1);
        assert!(second.vector_clock.get("agent-b") >= 1);
    }
}
