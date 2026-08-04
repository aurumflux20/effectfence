//! Chaos tests: real OS threads racing to run the same effect at the
//! same time, exercising both fencing layers -- the lock-free domain
//! reservation (same-instant races) and the intent ledger (duplicate
//! attempts at the same logical action).
//!
//! ## Why the domain-race synchronization is built the way it is
//!
//! A naive race test spins up two threads behind a single `Barrier` and
//! calls the full `prepare_effect_fence` on both. That does **not**
//! reliably produce a domain race in practice: the critical section it
//! needs to land in (a map lookup plus one atomic `compare_exchange`)
//! takes on the order of nanoseconds, while real OS thread wake-up
//! jitter *after* a barrier release is orders of magnitude larger.
//! Measured empirically on this machine: two threads raced this way
//! collided in 0/500 trials; even 128 threads collided in only ~92% of
//! trials -- nowhere near the "every single time" a correctness test
//! needs.
//!
//! [`read_current_then_race_reserve`] fixes this by putting the
//! `Barrier` *between* the read and the reservation attempt instead of
//! before the whole round trip. Both threads are then structurally
//! guaranteed to have observed the identical `expected_seq` before
//! either can advance it -- the race is forced by construction, not by
//! scheduler luck, while still calling the exact same public
//! `EffectFence::current` / `EffectFence::reserve` methods that
//! `prepare_effect_fence` itself calls internally.
//!
//! The intent-ledger race needs no such trick: the ledger's check-and-
//! claim is a single mutex-serialized critical section, so N threads
//! hitting it together deterministically yield exactly one winner.

use std::sync::{Arc, Barrier};
use std::thread;

use effectfence::fence::{
    Admission, EffectFence, EffectRequest, FenceError, VectorClock, commit_effect_cert,
    prepare_effect_fence,
};

fn charge_request(intent: &str, domain: &str, agent: String) -> EffectRequest {
    EffectRequest {
        intent: intent.to_string(),
        parent: None,
        domain: domain.to_string(),
        tool: "charge_card".to_string(),
        args: serde_json::json!({"amount_cents": 1999}),
        read_set: vec![],
        agent,
        known_clock: VectorClock::new(),
    }
}

/// Read `domain`'s current sequence, then block on `barrier` before
/// attempting to reserve the next slot at that (now potentially stale)
/// expectation. See the module docs for why the barrier sits here.
fn read_current_then_race_reserve(
    fence: &EffectFence,
    domain: &str,
    barrier: &Barrier,
) -> Result<u64, FenceError> {
    let expected = fence.current(domain);
    barrier.wait();
    fence
        .reserve(domain, expected)
        .map_err(|actual| FenceError::DomainRace {
            domain: domain.to_string(),
            expected,
            actual,
        })
}

#[test]
fn two_agents_forced_onto_the_same_domain_exactly_one_wins_every_time() {
    // Repeated to demonstrate this isn't a one-off -- it holds on every
    // trial because the barrier placement makes it structural, not
    // probabilistic.
    for trial in 0..100 {
        let fence = EffectFence::new();
        let domain = format!("order:forced-race-{trial}");
        let barrier = Arc::new(Barrier::new(2));

        let handles: Vec<_> = [(), ()]
            .into_iter()
            .map(|_| {
                let fence = fence.clone();
                let domain = domain.clone();
                let barrier = barrier.clone();
                thread::spawn(move || read_current_then_race_reserve(&fence, &domain, &barrier))
            })
            .collect();

        let results: Vec<_> = handles
            .into_iter()
            .map(|h| h.join().expect("agent thread panicked"))
            .collect();

        let ok_count = results.iter().filter(|r| r.is_ok()).count();
        let race_count = results
            .iter()
            .filter(|r| matches!(r, Err(FenceError::DomainRace { .. })))
            .count();

        assert_eq!(
            ok_count, 1,
            "trial {trial}: expected exactly one agent to win the domain, got {results:?}"
        );
        assert_eq!(
            race_count, 1,
            "trial {trial}: expected exactly one agent to lose with DomainRace, got {results:?}"
        );
        assert_eq!(
            fence.current(&domain),
            1,
            "trial {trial}: the loser's rejected attempt must leave no trace on the domain"
        );
    }
}

#[test]
fn concurrent_duplicates_of_one_intent_admit_exactly_one_and_replay_after_commit() {
    // The double-charge scenario end to end: N agents all decide to run
    // the SAME logical action (same intent) at the same moment. Exactly
    // one may execute; after it commits, any later duplicate gets the
    // recorded cert replayed instead of running.
    const RACERS: usize = 16;
    let fence = EffectFence::new();
    let barrier = Arc::new(Barrier::new(RACERS));

    let handles: Vec<_> = (0..RACERS)
        .map(|i| {
            let fence = fence.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                let req = charge_request("charge:order-777", "order:777", format!("agent-{i}"));
                barrier.wait();
                prepare_effect_fence(&fence, req)
            })
        })
        .collect();

    let results: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().expect("agent thread panicked"))
        .collect();

    let mut winner = None;
    let mut in_flight_rejections = 0;
    for result in results {
        match result {
            Ok(Admission::Fresh(prepared)) => {
                assert!(
                    winner.is_none(),
                    "two agents were both admitted to run the same intent"
                );
                winner = Some(prepared);
            }
            Ok(Admission::Replay(_)) => {
                panic!("nothing has committed yet -- no replay should be possible")
            }
            Err(FenceError::IntentInFlight { intent }) => {
                assert_eq!(intent, "charge:order-777");
                in_flight_rejections += 1;
            }
            Err(other) => panic!("unexpected rejection: {other:?}"),
        }
    }
    let winner = winner.expect("exactly one agent must be admitted");
    assert_eq!(in_flight_rejections, RACERS - 1);
    assert_eq!(
        fence.current("order:777"),
        1,
        "only the single winner may consume a domain sequence"
    );

    // The winner executes and commits...
    let cert = commit_effect_cert(
        &fence,
        winner,
        serde_json::json!({"charge_id": "ch_race_winner"}),
    )
    .expect("winner's commit must succeed");
    assert!(cert.verify());

    // ...and a late duplicate now gets the recorded outcome, not a
    // second execution.
    match prepare_effect_fence(
        &fence,
        charge_request("charge:order-777", "order:777", "agent-late".to_string()),
    )
    .expect("a duplicate of a done intent is not an error")
    {
        Admission::Replay(replayed) => assert_eq!(replayed.hash, cert.hash),
        Admission::Fresh(_) => panic!("late duplicate must NOT be admitted to run"),
    }
    assert_eq!(fence.current("order:777"), 1);
}

#[test]
fn many_distinct_intents_on_one_domain_never_double_allocate_a_sequence() {
    // Genuinely different actions (distinct intents) contending on the
    // same domain through the full public API, under real contention.
    // The safety property that matters: no two winners ever receive the
    // same sequence number, and the winners collectively occupy a
    // gapless 1..=N range.
    const AGENTS: usize = 32;
    let fence = EffectFence::new();
    let domain = "order:stress";
    let barrier = Arc::new(Barrier::new(AGENTS));

    let handles: Vec<_> = (0..AGENTS)
        .map(|i| {
            let fence = fence.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                let req = charge_request(&format!("charge:{i}"), domain, format!("agent-{i}"));
                barrier.wait();
                prepare_effect_fence(&fence, req)
            })
        })
        .collect();

    let results: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().expect("agent thread panicked"))
        .collect();

    let mut seqs: Vec<u64> = results
        .iter()
        .filter_map(|r| match r {
            Ok(Admission::Fresh(prepared)) => Some(prepared.seq),
            _ => None,
        })
        .collect();
    seqs.sort_unstable();

    let mut deduped = seqs.clone();
    deduped.dedup();
    assert_eq!(
        seqs, deduped,
        "two agents were allocated the same sequence number: {seqs:?}"
    );

    let expected: Vec<u64> = (1..=seqs.len() as u64).collect();
    assert_eq!(
        seqs, expected,
        "successful reservations must form a gapless 1..=N range, got {seqs:?}"
    );
    assert_eq!(fence.current(domain), seqs.len() as u64);
}
