//! The Storm — `cargo run --example storm`
//!
//! 1,000 attempts to charge the same order — concurrent racers and late
//! retries together — against one EffectFence. Exactly one may execute.
//!
//! This is the demo of the library's whole reason to exist: an agent swarm
//! (or a retry storm) hammers a side effect, and reality only hears it once.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;

use effectfence::fence::{
    Admission, EffectFence, EffectRequest, VectorClock, commit_effect_cert, prepare_effect_fence,
};

const AMOUNT_CENTS: u64 = 4_900;

fn charge_request(agent: String) -> EffectRequest {
    EffectRequest {
        intent: "charge:order-777".to_string(),
        parent: None,
        domain: "order:777".to_string(),
        tool: "charge_card".to_string(),
        args: serde_json::json!({ "order": 777, "amount_cents": AMOUNT_CENTS }),
        read_set: vec![],
        agent,
        known_clock: VectorClock::new(),
    }
}

fn main() {
    const WAVES: usize = 40;
    const RACERS_PER_WAVE: usize = 25; // 40 × 25 = 1,000 attempts

    let fence = EffectFence::new();
    let executions = Arc::new(AtomicUsize::new(0));
    let replays = Arc::new(AtomicUsize::new(0));
    let blocked_in_flight = Arc::new(AtomicUsize::new(0));

    println!();
    println!("  ⚡ effectfence — THE STORM");
    println!("  1,000 attempts to charge order #777 ($49.00): concurrent racers + late retries…");
    println!();

    let start = Instant::now();
    for _wave in 0..WAVES {
        let barrier = Arc::new(Barrier::new(RACERS_PER_WAVE));
        let handles: Vec<_> = (0..RACERS_PER_WAVE)
            .map(|i| {
                let fence = fence.clone();
                let barrier = barrier.clone();
                let executions = executions.clone();
                let replays = replays.clone();
                let blocked = blocked_in_flight.clone();
                thread::spawn(move || {
                    let req = charge_request(format!("agent-{i}"));
                    barrier.wait(); // whole wave fires at the same instant
                    match prepare_effect_fence(&fence, req) {
                        Ok(Admission::Fresh(prepared)) => {
                            // We are the ONE. Run the effect, commit the receipt.
                            let cert = commit_effect_cert(
                                &fence,
                                prepared,
                                serde_json::json!({ "charge_id": "ch_storm_1" }),
                            )
                            .expect("winner's commit");
                            assert!(cert.verify());
                            executions.fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(Admission::Replay(cert)) => {
                            // Action already done — we received the sealed receipt.
                            assert!(cert.verify());
                            replays.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(_) => {
                            // In flight elsewhere / race lost: told to stand down.
                            blocked.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("racer panicked");
        }
    }
    let elapsed = start.elapsed();

    let ex = executions.load(Ordering::SeqCst);
    let rp = replays.load(Ordering::SeqCst);
    let bl = blocked_in_flight.load(Ordering::SeqCst);
    let attempts = WAVES * RACERS_PER_WAVE;
    let defended = (attempts - ex) as u64 * AMOUNT_CENTS;

    println!("  attempts              : {attempts:>6}");
    println!("  ACTUAL EXECUTIONS     : {ex:>6}   ← the whole point");
    println!("  served sealed receipt : {rp:>6}");
    println!("  told to stand down    : {bl:>6}");
    println!("  elapsed               : {:>6.2?}", elapsed);
    println!();
    println!(
        "  💰 double-charges prevented this run: ${:.2}",
        defended as f64 / 100.0
    );
    println!();
    if ex == 1 {
        println!("  ✅ ONE execution. Every other attempt was fenced, replayed, or refused.");
    } else {
        println!("  ❌ FENCE FAILURE ({ex} executions) — file a bug with this output.");
        std::process::exit(1);
    }
    println!();
}
