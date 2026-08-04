//! # Causal Effect Fence
//!
//! Optimistic-concurrency-control (OCC) fencing for side-effecting tool
//! calls in a multi-agent gateway. Stops the same logical action from
//! executing twice — whether the second attempt arrives in the same
//! instant (a race) or minutes later (a retry/re-dispatch) — and gives
//! every committed effect a content-addressed certificate that causally
//! chains to what it was built on.
//!
//! ## The model
//!
//! - An **intent** is a stable identifier for one logical action ("charge
//!   order #123 for the June invoice"). Two attempts sharing an intent
//!   are, by definition, the *same* action: only one may ever execute,
//!   and every later attempt gets the recorded outcome replayed instead
//!   of running again. Minting a fresh intent for a genuinely new action
//!   is the caller's one responsibility.
//! - A **domain** is a named contention scope (e.g. `"order:123"`,
//!   `"account:456"`) with a monotonically increasing sequence number.
//! - A **read set** ([`ReadSetEntry`]) records which *other* domains (and
//!   at what sequence) an agent's decision to act was based on — its
//!   causal dependencies.
//! - [`prepare_effect_fence`] admits an [`EffectRequest`] through three
//!   gates, in order:
//!   1. **Intent gate** — if this intent already completed, the recorded
//!      [`EffectCert`] is replayed ([`Admission::Replay`]); if it
//!      previously failed, or another attempt is running it right now,
//!      the caller is rejected. Otherwise this attempt takes a lease on
//!      the intent.
//!   2. **Read-set validation** — every dependency must still be at the
//!      sequence the caller observed, or [`FenceError::ReadSetStale`].
//!   3. **Domain reservation** — an atomic `compare_exchange` reserves
//!      the next sequence in the target domain; two same-instant racers
//!      observing the same state can't both win
//!      ([`FenceError::DomainRace`] for the loser).
//! - The caller then actually executes the tool/effect and calls
//!   [`commit_effect_cert`], which re-validates the read set (the world
//!   may have moved while the effect ran) and mints an [`EffectCert`]:
//!   a SHA-256, content-addressed record of exactly what happened,
//!   chained to a `parent` certificate for causal lineage. The intent is
//!   marked done, and its cert is what future duplicate attempts get.
//! - If the effect fails, the caller reports it with [`abort_effect`].
//!   The intent is then **fenced, not silently retryable** — when it is
//!   unknown whether the underlying side effect fired before the
//!   failure, allowing a blind retry is exactly the double-charge risk
//!   this module exists to prevent. [`EffectFence::clear_intent`] is the
//!   explicit escape hatch once the caller has reconciled what actually
//!   happened. A holder that crashes without reporting anything loses
//!   its lease after [`FenceConfig::lease_ttl`], letting a later attempt
//!   take over.
//!
//! ## Example
//!
//! ```rust,ignore
//! use effectfence::fence::{
//!     prepare_effect_fence, commit_effect_cert, Admission, EffectFence, EffectRequest,
//!     VectorClock,
//! };
//!
//! let fence = EffectFence::new();
//! let req = EffectRequest {
//!     intent: format!("charge:{}", order.id),   // same order -> same intent
//!     parent: None,
//!     domain: format!("order:{}", order.id),
//!     tool: "charge_card".into(),
//!     args: serde_json::json!({"amount_cents": order.amount_cents}),
//!     read_set: vec![],
//!     agent: "agent-a".into(),
//!     known_clock: VectorClock::new(),
//! };
//!
//! match prepare_effect_fence(&fence, req)? {
//!     Admission::Fresh(prepared) => {
//!         let result = run_the_actual_charge(&order)?;
//!         let cert = commit_effect_cert(&fence, prepared, result)?;
//!     }
//!     Admission::Replay(cert) => {
//!         // This exact action already ran. Use its recorded result;
//!         // do NOT charge again.
//!     }
//! }
//! ```
//!
//! ## Scope
//!
//! This is an in-memory, single-process fence: state lives behind an
//! `Arc` and is lost on restart. For a gateway scaled across multiple
//! processes, the same intent/domain/read-set model would need to be
//! backed by a shared store (e.g. `SETNX`+CAS in Redis, or a row with an
//! optimistic version column in Postgres).

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Everything an agent submits to [`prepare_effect_fence`] to request
/// admission for one side-effecting action.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EffectRequest {
    /// Stable identifier for the logical action, e.g. `"charge:order-123"`.
    /// Attempts sharing an intent are the SAME action: only one ever
    /// executes; later attempts get the recorded outcome replayed. Mint a
    /// new intent only for a genuinely new action.
    pub intent: String,
    /// Hex SHA-256 hash of the effect certificate this one causally
    /// follows. Omit for a genesis effect with no prior cause.
    #[serde(default)]
    pub parent: Option<String>,
    /// The contention scope this effect will act on, e.g. `"order:123"`.
    pub domain: String,
    /// Name of the tool/effect being fenced, e.g. `"charge_card"`.
    pub tool: String,
    /// Arbitrary JSON arguments for the tool call being fenced.
    #[serde(default)]
    pub args: Value,
    /// Other domains this decision cross-checked, and the sequence
    /// observed for each. Do not include `domain` itself here — its own
    /// sequence is about to be advanced by the reservation, so an entry
    /// for it would go stale the instant that happens. Same-domain
    /// lineage belongs in `parent`.
    #[serde(default)]
    pub read_set: Vec<ReadSetEntry>,
    /// Identifier of the calling agent.
    pub agent: String,
    /// The calling agent's current view of causal history.
    #[serde(default)]
    pub known_clock: VectorClock,
}

/// Tunables for an [`EffectFence`].
#[derive(Debug, Clone, Copy)]
pub struct FenceConfig {
    /// How long an in-flight intent lease is honored before its holder is
    /// presumed crashed and a later attempt may take over. Must comfortably
    /// exceed the longest effect you expect to run — if a still-running
    /// holder outlives its lease, a second attempt can be admitted while
    /// the first is executing.
    pub lease_ttl: Duration,
    /// How long a finished intent's outcome (done or failed) is remembered
    /// for replay/fencing before it is evicted and the intent becomes
    /// runnable again.
    pub result_ttl: Duration,
}

impl Default for FenceConfig {
    fn default() -> Self {
        Self {
            lease_ttl: Duration::from_secs(60),
            result_ttl: Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// Per-intent state. Not part of the public API.
#[derive(Debug)]
enum IntentState {
    /// An attempt holds the lease and is (presumably) executing.
    InFlight { lease_expires_at: Instant },
    /// The effect ran and this is its recorded certificate.
    Done {
        cert: Arc<EffectCert>,
        recorded_at: Instant,
    },
    /// An attempt reported failure (or its commit was rejected). Fenced —
    /// see [`EffectFence::clear_intent`].
    Failed {
        reason: Arc<str>,
        recorded_at: Instant,
    },
}

#[derive(Debug)]
struct Inner {
    config: FenceConfig,
    /// Per-domain sequence counters decided by atomic CAS. The map lookup/insert is
    /// behind a short-held `Mutex`, but sequence reservation itself never
    /// holds it — that's a `compare_exchange` on the `Arc<AtomicU64>`.
    domains: Mutex<HashMap<String, Arc<AtomicU64>>>,
    /// The intent ledger: which logical actions are running, done, or
    /// failed. This is what stops a *later* duplicate, not just a
    /// same-instant race.
    intents: Mutex<HashMap<String, IntentState>>,
}

/// The fence: per-domain sequence counters plus the intent ledger.
///
/// Cheap to clone — it's an `Arc` handle to shared state. Clone it into
/// every task/thread/handler that needs to prepare or commit effects.
#[derive(Debug, Clone)]
pub struct EffectFence {
    inner: Arc<Inner>,
}

impl Default for EffectFence {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectFence {
    /// A fence with default tunables ([`FenceConfig::default`]).
    pub fn new() -> Self {
        Self::with_config(FenceConfig::default())
    }

    /// A fence with explicit tunables.
    pub fn with_config(config: FenceConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                domains: Mutex::new(HashMap::new()),
                intents: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The current sequence number for `domain` (0 if never reserved).
    ///
    /// Purely a read: asking about a domain does NOT create tracking
    /// state for it.
    pub fn current(&self, domain: &str) -> u64 {
        let domains = self.inner.domains.lock().expect("fence mutex poisoned");
        domains
            .get(domain)
            .map(|counter| counter.load(Ordering::SeqCst))
            .unwrap_or(0)
    }

    /// Atomically reserve the next sequence number for `domain`, but
    /// only if it is still at `expected_seq`.
    ///
    /// On success, returns `Ok(expected_seq + 1)` — the caller now
    /// exclusively owns that slot in the domain's causal order. On
    /// failure (someone else already advanced the domain), returns
    /// `Err(actual_seq)` with the value that won instead. This is the
    /// single point where a same-instant race between concurrent agents
    /// on the same domain is decided; exactly one caller can win for any
    /// given `expected_seq`.
    pub fn reserve(&self, domain: &str, expected_seq: u64) -> Result<u64, u64> {
        let counter = {
            let mut domains = self.inner.domains.lock().expect("fence mutex poisoned");
            domains
                .entry(domain.to_string())
                .or_insert_with(|| Arc::new(AtomicU64::new(0)))
                .clone()
        };
        let next_seq = expected_seq + 1;
        counter
            .compare_exchange(expected_seq, next_seq, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_previous| next_seq)
    }

    /// Number of domains currently tracked. Observability/metrics.
    pub fn domain_count(&self) -> usize {
        self.inner
            .domains
            .lock()
            .expect("fence mutex poisoned")
            .len()
    }

    /// Number of intents currently in the ledger (in-flight + remembered
    /// outcomes). Observability/metrics.
    pub fn intent_count(&self) -> usize {
        self.inner
            .intents
            .lock()
            .expect("fence mutex poisoned")
            .len()
    }

    /// Forcibly forget any record of `intent`, making it runnable again.
    /// Returns whether a record was actually removed.
    ///
    /// This is the manual-reconciliation escape hatch for a `Failed`
    /// intent — e.g. an operator confirms from the downstream system's
    /// own records that the failed attempt never actually took effect.
    /// Do not call it reflexively: clearing an intent that *did* execute
    /// reopens the door to the double-execution this module exists to
    /// prevent.
    pub fn clear_intent(&self, intent: &str) -> bool {
        self.inner
            .intents
            .lock()
            .expect("fence mutex poisoned")
            .remove(intent)
            .is_some()
    }

    /// Evict a domain's sequence counter entirely. Returns whether one
    /// existed. Operator tool for bounding memory when domain names are
    /// unbounded (each costs only its name plus a counter, but they are
    /// never dropped automatically — a fresh reservation after eviction
    /// restarts that domain's sequence at 1).
    pub fn evict_domain(&self, domain: &str) -> bool {
        self.inner
            .domains
            .lock()
            .expect("fence mutex poisoned")
            .remove(domain)
            .is_some()
    }

    /// Evict expired intent records (finished outcomes past
    /// [`FenceConfig::result_ttl`], abandoned leases past
    /// [`FenceConfig::lease_ttl`]) to bound memory growth. Expiry is also
    /// enforced lazily per-intent on every admission check, so this only
    /// reclaims memory for intents never looked up again. Safe to call
    /// periodically from a background sweep.
    pub fn sweep(&self) {
        let now = Instant::now();
        let result_ttl = self.inner.config.result_ttl;
        let mut intents = self.inner.intents.lock().expect("fence mutex poisoned");
        intents.retain(|_, state| match state {
            IntentState::InFlight { lease_expires_at } => now < *lease_expires_at,
            IntentState::Done { recorded_at, .. } | IntentState::Failed { recorded_at, .. } => {
                now.saturating_duration_since(*recorded_at) < result_ttl
            }
        });
    }

    /// Record a terminal state for `intent`. Private: reached only via
    /// commit/abort so the ledger can't be driven into `Done` without a
    /// prepared attempt actually finishing.
    fn record_outcome(&self, intent: &str, state: IntentState) {
        self.inner
            .intents
            .lock()
            .expect("fence mutex poisoned")
            .insert(intent.to_string(), state);
    }

    /// Drop an in-flight lease after a failed admission (read-set or
    /// domain-race rejection), so the intent doesn't stay blocked by an
    /// attempt that never ran anything.
    fn release_intent(&self, intent: &str) {
        self.inner
            .intents
            .lock()
            .expect("fence mutex poisoned")
            .remove(intent);
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
    /// The intent this effect executed under. Replays of the same intent
    /// return this same certificate.
    pub intent: String,
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
    intent: &'a str,
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
    fn new(prepared: PreparedEffect, result: Value) -> Self {
        let hash = Self::compute_hash(
            &prepared.intent,
            &prepared.parent,
            &prepared.domain,
            prepared.seq,
            &prepared.tool,
            &prepared.args,
            &result,
            &prepared.vector_clock,
            &prepared.read_set,
            &prepared.agent,
        );
        Self {
            intent: prepared.intent,
            parent: prepared.parent,
            domain: prepared.domain,
            seq: prepared.seq,
            tool: prepared.tool,
            args: prepared.args,
            result,
            vector_clock: prepared.vector_clock,
            read_set: prepared.read_set,
            agent: prepared.agent,
            hash,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn compute_hash(
        intent: &str,
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
            intent,
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
            &self.intent,
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

/// The output of a successful fresh admission: proof that the caller
/// exclusively holds `seq` in `domain` and the lease on `intent`, plus
/// everything needed to mint an [`EffectCert`] once the effect's
/// `result` is known. Pass this to [`commit_effect_cert`] when the tool
/// call finishes, or to [`abort_effect`] if it fails.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreparedEffect {
    pub intent: String,
    pub parent: Option<String>,
    pub domain: String,
    pub seq: u64,
    pub tool: String,
    pub args: Value,
    pub vector_clock: VectorClock,
    pub read_set: Vec<ReadSetEntry>,
    pub agent: String,
}

/// The outcome of a successful [`prepare_effect_fence`] call.
#[derive(Debug)]
pub enum Admission {
    /// This attempt won. Execute the effect, then report back via
    /// [`commit_effect_cert`] or [`abort_effect`].
    Fresh(PreparedEffect),
    /// This exact intent already executed; here is its recorded
    /// certificate. Do NOT run the effect again.
    Replay(Arc<EffectCert>),
}

/// Rejections raised by [`prepare_effect_fence`] / [`commit_effect_cert`].
/// Every variant means the same thing for the caller: do not run (or do
/// not trust the result of) the effect.
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

    /// Another attempt currently holds the lease on this intent and is
    /// (presumably) executing it right now. Wait for its outcome instead
    /// of running the effect yourself.
    #[error("intent `{intent}` is already in flight: another attempt is executing it right now")]
    IntentInFlight { intent: String },

    /// An earlier attempt at this intent finished without a recorded
    /// success, so whether the side effect actually fired is unknown.
    /// Fenced until explicitly cleared via [`EffectFence::clear_intent`]
    /// after reconciling with the downstream system.
    #[error(
        "intent `{intent}` previously failed ({reason}); outcome is unknown -- reconcile with \
         the downstream system, then clear_intent to allow a retry"
    )]
    IntentFailed { intent: String, reason: String },
}

/// Admit `req` through the three gates (intent ledger, read-set
/// validation, atomic CAS domain reservation) described in the module
/// docs.
///
/// Returns [`Admission::Fresh`] with a ticket when this attempt wins —
/// the caller must then execute the effect and report back via
/// [`commit_effect_cert`] or [`abort_effect`] — or [`Admission::Replay`]
/// with the recorded certificate when this intent already executed.
/// Never blocks.
///
/// On rejection ([`FenceError::ReadSetStale`], [`FenceError::DomainRace`],
/// [`FenceError::IntentInFlight`], [`FenceError::IntentFailed`]) the
/// caller must not run the effect. A rejected attempt releases any
/// intent lease it briefly took, leaving no trace.
pub fn prepare_effect_fence(
    fence: &EffectFence,
    req: EffectRequest,
) -> Result<Admission, FenceError> {
    let now = Instant::now();
    let config = fence.inner.config;

    // Gate 1: the intent ledger. Checked before the read set on purpose:
    // a duplicate of an already-executed action deserves its recorded
    // outcome even if the world has since moved past the duplicate's
    // (stale) read snapshot.
    {
        let mut intents = fence.inner.intents.lock().expect("fence mutex poisoned");
        match intents.get(&req.intent) {
            Some(IntentState::Done { cert, recorded_at })
                if now.saturating_duration_since(*recorded_at) < config.result_ttl =>
            {
                return Ok(Admission::Replay(cert.clone()));
            }
            Some(IntentState::Failed {
                reason,
                recorded_at,
            }) if now.saturating_duration_since(*recorded_at) < config.result_ttl => {
                return Err(FenceError::IntentFailed {
                    intent: req.intent.clone(),
                    reason: reason.to_string(),
                });
            }
            Some(IntentState::InFlight { lease_expires_at }) if now < *lease_expires_at => {
                return Err(FenceError::IntentInFlight {
                    intent: req.intent.clone(),
                });
            }
            // No record, an expired lease (holder presumed crashed), or
            // an expired outcome -- all claimable.
            _ => {}
        }
        intents.insert(
            req.intent.clone(),
            IntentState::InFlight {
                lease_expires_at: now + config.lease_ttl,
            },
        );
    }

    // Gate 2: read-set validation.
    for entry in &req.read_set {
        let actual = fence.current(&entry.domain);
        if actual != entry.seq {
            fence.release_intent(&req.intent);
            return Err(FenceError::ReadSetStale {
                domain: entry.domain.clone(),
                expected: entry.seq,
                actual,
            });
        }
    }

    // Gate 3: CAS domain reservation (the race decision is a single
    // compare_exchange; the counter lookup is behind a short mutex).
    let expected_seq = fence.current(&req.domain);
    let seq = match fence.reserve(&req.domain, expected_seq) {
        Ok(seq) => seq,
        Err(actual) => {
            fence.release_intent(&req.intent);
            return Err(FenceError::DomainRace {
                domain: req.domain.clone(),
                expected: expected_seq,
                actual,
            });
        }
    };

    let mut vector_clock = req.known_clock;
    vector_clock.tick(&req.agent);

    Ok(Admission::Fresh(PreparedEffect {
        intent: req.intent,
        parent: req.parent,
        domain: req.domain,
        seq,
        tool: req.tool,
        args: req.args,
        vector_clock,
        read_set: req.read_set,
        agent: req.agent,
    }))
}

/// Finalize a [`PreparedEffect`] into an [`EffectCert`] once the tool
/// call's `result` is known, and record it in the intent ledger so any
/// later duplicate of this intent is replayed instead of re-run.
///
/// Re-validates every read-set entry one more time — the effect may have
/// taken long enough for a causal dependency to move in the meantime. If
/// one did, the commit is rejected AND the intent is marked failed (the
/// effect physically ran; refusing to certify it must not quietly make
/// the intent runnable again) — reconcile, then
/// [`EffectFence::clear_intent`].
///
/// `domain`'s own reservation is never revisited: other intents advancing
/// the same domain while this effect ran is normal, and a sequence gap
/// left by an aborted attempt is harmless.
pub fn commit_effect_cert(
    fence: &EffectFence,
    prepared: PreparedEffect,
    result: Value,
) -> Result<EffectCert, FenceError> {
    for entry in &prepared.read_set {
        let actual = fence.current(&entry.domain);
        if actual != entry.seq {
            fence.record_outcome(
                &prepared.intent,
                IntentState::Failed {
                    reason: Arc::from(format!(
                        "commit rejected: read-set entry for `{}` went stale (expected {}, \
                         actual {}) while the effect was running",
                        entry.domain, entry.seq, actual
                    )),
                    recorded_at: Instant::now(),
                },
            );
            return Err(FenceError::ReadSetStale {
                domain: entry.domain.clone(),
                expected: entry.seq,
                actual,
            });
        }
    }

    let intent = prepared.intent.clone();
    let cert = EffectCert::new(prepared, result);
    fence.record_outcome(
        &intent,
        IntentState::Done {
            cert: Arc::new(cert.clone()),
            recorded_at: Instant::now(),
        },
    );
    Ok(cert)
}

/// Report that a prepared effect failed (or that its outcome is
/// unknown). The intent is recorded as failed and stays fenced —
/// duplicates get [`FenceError::IntentFailed`] instead of silently
/// re-running an action whose side effect may or may not have fired —
/// until [`EffectFence::clear_intent`] is called after reconciliation.
pub fn abort_effect(fence: &EffectFence, prepared: PreparedEffect, reason: impl Into<String>) {
    fence.record_outcome(
        &prepared.intent,
        IntentState::Failed {
            reason: Arc::from(reason.into()),
            recorded_at: Instant::now(),
        },
    );
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

    fn req(intent: &str, domain: &str, agent: &str) -> EffectRequest {
        EffectRequest {
            intent: intent.to_string(),
            parent: None,
            domain: domain.to_string(),
            tool: "charge_card".to_string(),
            args: serde_json::json!({"amount_cents": 1999}),
            read_set: vec![],
            agent: agent.to_string(),
            known_clock: VectorClock::new(),
        }
    }

    fn fresh(admission: Admission) -> PreparedEffect {
        match admission {
            Admission::Fresh(prepared) => prepared,
            Admission::Replay(cert) => panic!("expected Fresh, got Replay of {}", cert.hash),
        }
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
    fn domain_reserve_advances_and_rejects_stale_expectation() {
        let fence = EffectFence::new();
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
    fn asking_about_a_domain_does_not_create_tracking_state() {
        let fence = EffectFence::new();
        assert_eq!(fence.current("ghost:1"), 0);
        assert_eq!(fence.current("ghost:2"), 0);
        assert_eq!(
            fence.domain_count(),
            0,
            "current() must be a pure read -- queries must not grow the domain map"
        );

        fence.reserve("real:1", 0).unwrap();
        assert_eq!(fence.domain_count(), 1);
        assert!(fence.evict_domain("real:1"));
        assert_eq!(fence.domain_count(), 0);
    }

    #[test]
    fn prepare_then_commit_happy_path_produces_a_verifiable_cert() {
        let fence = EffectFence::new();

        let prepared = fresh(
            prepare_effect_fence(&fence, req("charge:order-1", "order:1", "agent-a"))
                .expect("first attempt on a free intent+domain must succeed"),
        );

        assert_eq!(prepared.seq, 1);
        assert_eq!(prepared.vector_clock.get("agent-a"), 1);

        let cert = commit_effect_cert(&fence, prepared, serde_json::json!({"charge_id": "ch_123"}))
            .expect("commit with an untouched read set must succeed");

        assert!(cert.verify());
        assert_eq!(cert.intent, "charge:order-1");
        assert_eq!(cert.domain, "order:1");
        assert_eq!(cert.seq, 1);
        assert!(cert.parent.is_none());
    }

    #[test]
    fn late_duplicate_of_a_done_intent_is_replayed_not_rerun() {
        // THE headline guarantee: a second attempt that arrives well
        // after the first completed (not a same-instant race) still must
        // not re-run the effect -- it gets the recorded cert instead,
        // and the domain sequence does not advance.
        let fence = EffectFence::new();

        let prepared = fresh(
            prepare_effect_fence(&fence, req("charge:order-2", "order:2", "agent-a")).unwrap(),
        );
        let cert =
            commit_effect_cert(&fence, prepared, serde_json::json!({"charge_id": "ch_1"})).unwrap();
        assert_eq!(fence.current("order:2"), 1);

        // A different agent, arbitrarily later, same intent:
        let replayed =
            match prepare_effect_fence(&fence, req("charge:order-2", "order:2", "agent-b"))
                .expect("duplicate of a done intent is not an error")
            {
                Admission::Replay(cert) => cert,
                Admission::Fresh(_) => panic!("duplicate intent must NOT be admitted to run"),
            };

        assert_eq!(replayed.hash, cert.hash);
        assert_eq!(
            fence.current("order:2"),
            1,
            "a replayed duplicate must not consume a new domain sequence"
        );
    }

    #[test]
    fn duplicate_of_an_in_flight_intent_is_rejected() {
        let fence = EffectFence::new();

        let _prepared = fresh(
            prepare_effect_fence(&fence, req("charge:order-3", "order:3", "agent-a")).unwrap(),
        );

        // Not yet committed -- a second attempt must be told to wait.
        let err = prepare_effect_fence(&fence, req("charge:order-3", "order:3", "agent-b"))
            .expect_err("an in-flight intent must reject duplicates");
        assert_eq!(
            err,
            FenceError::IntentInFlight {
                intent: "charge:order-3".to_string()
            }
        );
    }

    #[test]
    fn aborted_intent_is_fenced_until_explicitly_cleared() {
        let fence = EffectFence::new();

        let prepared = fresh(
            prepare_effect_fence(&fence, req("charge:order-4", "order:4", "agent-a")).unwrap(),
        );
        abort_effect(&fence, prepared, "card declined");

        match prepare_effect_fence(&fence, req("charge:order-4", "order:4", "agent-a")) {
            Err(FenceError::IntentFailed { intent, reason }) => {
                assert_eq!(intent, "charge:order-4");
                assert!(reason.contains("card declined"));
            }
            other => panic!("expected IntentFailed, got {other:?}"),
        }

        assert!(fence.clear_intent("charge:order-4"));
        let retried = fresh(
            prepare_effect_fence(&fence, req("charge:order-4", "order:4", "agent-a"))
                .expect("cleared intent must be runnable again"),
        );
        assert_eq!(retried.seq, 2, "the aborted attempt's seq 1 stays burned");
    }

    #[test]
    fn expired_lease_can_be_taken_over_by_a_later_attempt() {
        let fence = EffectFence::with_config(FenceConfig {
            lease_ttl: Duration::from_millis(5),
            ..FenceConfig::default()
        });

        // First attempt takes the lease and then "crashes" (never
        // commits or aborts).
        let _abandoned = fresh(
            prepare_effect_fence(&fence, req("charge:order-5", "order:5", "agent-a")).unwrap(),
        );

        std::thread::sleep(Duration::from_millis(20));

        let takeover = fresh(
            prepare_effect_fence(&fence, req("charge:order-5", "order:5", "agent-b"))
                .expect("an expired lease must be claimable"),
        );
        assert_eq!(takeover.seq, 2);
    }

    #[test]
    fn rejected_admission_releases_the_intent_lease() {
        let fence = EffectFence::new();

        // Force a ReadSetStale rejection: claim a dependency at seq 1,
        // then submit a request that believes it's still at 0.
        fence.reserve("inventory:sku-9", 0).unwrap();
        let mut r = req("charge:order-6", "order:6", "agent-a");
        r.read_set = vec![ReadSetEntry::new("inventory:sku-9", 0)];

        let err = prepare_effect_fence(&fence, r).expect_err("stale read set must reject");
        assert!(matches!(err, FenceError::ReadSetStale { .. }));

        // The rejection must not leave the intent blocked.
        let ok = fresh(
            prepare_effect_fence(&fence, {
                let mut r = req("charge:order-6", "order:6", "agent-a");
                r.read_set = vec![ReadSetEntry::new("inventory:sku-9", 1)];
                r
            })
            .expect("a corrected retry of a rejected (never-run) attempt must be admitted"),
        );
        assert_eq!(ok.intent, "charge:order-6");
    }

    #[test]
    fn commit_rejection_marks_the_intent_failed_not_runnable() {
        let fence = EffectFence::new();

        fence.reserve("inventory:sku-1", 0).unwrap();
        let mut r = req("reserve:order-7", "order:7", "agent-a");
        r.tool = "reserve_inventory".to_string();
        r.read_set = vec![ReadSetEntry::new("inventory:sku-1", 1)];

        let prepared = fresh(prepare_effect_fence(&fence, r).expect("read set matches"));

        // The dependency moves while the effect is "running"...
        fence.reserve("inventory:sku-1", 1).unwrap();

        let err = commit_effect_cert(&fence, prepared, serde_json::json!({}))
            .expect_err("dependency moved -- commit must be rejected");
        assert!(matches!(err, FenceError::ReadSetStale { .. }));

        // ...and because the effect DID physically run, the intent must
        // now be fenced as failed, not quietly runnable again.
        match prepare_effect_fence(&fence, req("reserve:order-7", "order:7", "agent-a")) {
            Err(FenceError::IntentFailed { .. }) => {}
            other => panic!("expected IntentFailed after a rejected commit, got {other:?}"),
        }
    }

    #[test]
    fn sweep_evicts_expired_outcomes() {
        let fence = EffectFence::with_config(FenceConfig {
            result_ttl: Duration::from_millis(5),
            ..FenceConfig::default()
        });

        let prepared = fresh(
            prepare_effect_fence(&fence, req("charge:order-8", "order:8", "agent-a")).unwrap(),
        );
        commit_effect_cert(&fence, prepared, serde_json::json!({})).unwrap();
        assert_eq!(fence.intent_count(), 1);

        std::thread::sleep(Duration::from_millis(20));
        fence.sweep();
        assert_eq!(fence.intent_count(), 0, "expired outcome must be evicted");

        // With the outcome expired, the intent is genuinely runnable
        // again (result_ttl is the caller's chosen replay horizon).
        let rerun = fresh(
            prepare_effect_fence(&fence, req("charge:order-8", "order:8", "agent-a")).unwrap(),
        );
        assert_eq!(rerun.seq, 2);
    }

    #[test]
    fn different_intents_on_the_same_domain_queue_up_sequentially() {
        // Two genuinely different actions touching the same domain are
        // both allowed -- they serialize onto consecutive sequence
        // numbers. Stopping *duplicates of the same action* is the
        // intent ledger's job (see the replay test), not the domain
        // counter's.
        let fence = EffectFence::new();

        let first = fresh(
            prepare_effect_fence(&fence, req("charge:june", "account:9", "agent-a")).unwrap(),
        );
        commit_effect_cert(&fence, first, serde_json::json!({})).unwrap();

        let second = fresh(
            prepare_effect_fence(&fence, req("charge:july", "account:9", "agent-b"))
                .expect("a different intent is a different action and may proceed"),
        );
        assert_eq!(second.seq, 2);
    }

    #[test]
    fn cert_chain_records_parent_for_causal_lineage() {
        let fence = EffectFence::new();

        let first = commit_effect_cert(
            &fence,
            fresh(
                prepare_effect_fence(&fence, req("charge:order-10", "order:10", "agent-a"))
                    .unwrap(),
            ),
            serde_json::json!({"charge_id": "ch_1"}),
        )
        .unwrap();

        // Same-domain lineage is expressed via `parent`, not via a
        // read-set entry for the domain itself (which the reservation
        // would immediately invalidate).
        let mut receipt_req = req("receipt:order-10", "order:10", "agent-b");
        receipt_req.tool = "send_receipt".to_string();
        receipt_req.parent = Some(first.hash.clone());
        receipt_req.known_clock = first.vector_clock.clone();

        let second = commit_effect_cert(
            &fence,
            fresh(prepare_effect_fence(&fence, receipt_req).unwrap()),
            serde_json::json!({"receipt_id": "rc_1"}),
        )
        .unwrap();

        assert_eq!(second.parent, Some(first.hash));
        assert!(second.vector_clock.get("agent-a") >= 1);
        assert!(second.vector_clock.get("agent-b") >= 1);
        assert!(second.verify());
    }
}
