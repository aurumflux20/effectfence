//! Concurrent DISTINCT calls to one tool must all be admitted.
//!
//! wrap.rs documents: "Non-identical calls pass straight through, unfenced."
//! Two calls with different arguments are different actions — the fence must
//! never refuse one because another happened to be in flight at the same
//! instant. An agent that parallelises tool calls (read two files, query two
//! resources) hits this on every run.

use std::sync::{Arc, Barrier};
use std::thread;

use effectfence::fence::{
    Admission, EffectFence, EffectRequest, VectorClock, prepare_effect_fence,
};

/// Mirror exactly what wrap.rs builds for a tool call.
fn wrap_style_request(tool: &str, unique_arg: u32) -> EffectRequest {
    EffectRequest {
        intent: format!("wrap:{tool}:arg-{unique_arg}"),
        parent: None,
        domain: format!("tool:{tool}"),
        tool: tool.to_string(),
        args: serde_json::json!({ "path": format!("/file/{unique_arg}") }),
        read_set: vec![],
        agent: "effectfence-wrap".to_string(),
        known_clock: VectorClock::new(),
    }
}

#[test]
fn concurrent_distinct_calls_to_one_tool_are_all_admitted() {
    const N: u32 = 64;
    let fence = Arc::new(EffectFence::new());
    let barrier = Arc::new(Barrier::new(N as usize));

    let handles: Vec<_> = (0..N)
        .map(|i| {
            let fence = Arc::clone(&fence);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                prepare_effect_fence(&fence, wrap_style_request("read_file", i))
            })
        })
        .collect();

    let mut admitted = 0;
    let mut refused = Vec::new();
    for h in handles {
        match h.join().expect("thread") {
            Ok(Admission::Fresh(_)) => admitted += 1,
            Ok(Admission::Replay(_)) => panic!("distinct args must never replay another action"),
            Err(e) => refused.push(e.to_string()),
        }
    }

    assert_eq!(
        admitted,
        N,
        "all {N} DISTINCT calls must be admitted; {} refused: {:?}",
        refused.len(),
        refused
    );
}
