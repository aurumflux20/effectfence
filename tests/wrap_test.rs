//! Wrap-mode integration test — the proxy fencing a real child MCP server.
//!
//! The child here is effectfence's own MCP server (`effectfence wrap --
//! effectfence`), which needs no external dependency and gives us a
//! perfect lie detector:
//!
//! The child's `fence_prepare` is itself idempotent — call it twice with
//! the same intent and the CHILD answers "already in flight" the second
//! time. So:
//!
//!   - wrap working: the second identical call never reaches the child;
//!     the proxy replays response #1 verbatim (same fence_token).
//!   - wrap leaking: the child sees call #2 and returns an in-flight
//!     error, which is a different response.
//!
//! Comparing the two responses therefore proves the duplicate was stopped
//! AT THE PROXY, not merely that "nothing crashed".

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Mcp {
    /// Spawn `effectfence wrap -- effectfence` and complete the handshake.
    fn spawn_wrap_of_self() -> Self {
        let exe = env!("CARGO_BIN_EXE_effectfence");
        let mut child = Command::new(exe)
            .args(["wrap", "--", exe])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn wrap");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut mcp = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };

        let init = mcp.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2026-07-28",
                "capabilities": {},
                "clientInfo": {"name": "wrap-test", "version": "0"}
            }),
        );
        let name = init["result"]["serverInfo"]["name"]
            .as_str()
            .expect("serverInfo.name");
        assert!(
            name.starts_with("effectfence-wrap"),
            "expected the wrap proxy to answer, got {name:?}"
        );
        mcp.notify("notifications/initialized");
        mcp
    }

    fn send(&mut self, msg: &serde_json::Value) {
        writeln!(self.stdin, "{msg}").expect("write");
        self.stdin.flush().expect("flush");
    }

    fn notify(&mut self, method: &str) {
        let msg = serde_json::json!({"jsonrpc": "2.0", "method": method});
        self.send(&msg);
    }

    /// Send a request and read until the matching response id arrives.
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }));

        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).expect("read");
            assert!(n > 0, "wrap closed stdout while awaiting id {id}");
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue; // ignore any non-JSON noise
            };
            if value.get("id").and_then(|v| v.as_i64()) == Some(id) {
                return value;
            }
        }
    }

    /// Call a tool and return the parsed JSON of its first text content
    /// block (which is how both servers here encode their payloads).
    fn call_tool(&mut self, name: &str, args: serde_json::Value) -> serde_json::Value {
        let resp = self.request(
            "tools/call",
            serde_json::json!({"name": name, "arguments": args}),
        );
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text content in {resp}"));
        serde_json::from_str(text).unwrap_or_else(|_| serde_json::json!({"raw": text}))
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn prepare_args(intent: &str, domain: &str) -> serde_json::Value {
    serde_json::json!({
        "intent": intent,
        "domain": domain,
        "tool": "charge_card",
        "args": {"amount_cents": 4900},
        "agent": "wrap-test"
    })
}

#[test]
fn wrap_mirrors_the_child_tool_list() {
    let mut mcp = Mcp::spawn_wrap_of_self();
    let resp = mcp.request("tools/list", serde_json::json!({}));
    let tools: Vec<String> = resp["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect();

    for expected in ["fence_prepare", "fence_commit", "fence_abort"] {
        assert!(
            tools.iter().any(|t| t == expected),
            "wrap did not mirror `{expected}` from the child; got {tools:?}"
        );
    }
}

#[test]
fn identical_duplicate_call_is_replayed_at_the_proxy() {
    let mut mcp = Mcp::spawn_wrap_of_self();
    let args = prepare_args("charge:wrap-1", "order:wrap-1");

    let first = mcp.call_tool("fence_prepare", args.clone());
    assert_eq!(
        first["status"], "fresh",
        "first call should reach the child fresh, got {first}"
    );

    // The lie detector: if this reached the child, the child would answer
    // with an in-flight rejection instead of the identical payload.
    let second = mcp.call_tool("fence_prepare", args);
    assert_eq!(
        first, second,
        "duplicate was NOT replayed at the proxy -- it reached the child"
    );
}

#[test]
fn shuffled_argument_key_order_is_still_the_same_action() {
    let mut mcp = Mcp::spawn_wrap_of_self();

    let first = mcp.call_tool(
        "fence_prepare",
        serde_json::json!({
            "intent": "charge:wrap-2", "domain": "order:wrap-2",
            "tool": "charge_card", "args": {"amount_cents": 4900},
            "agent": "wrap-test"
        }),
    );
    assert_eq!(first["status"], "fresh");

    // Same content, different key order -- canonicalization must collapse
    // these to one intent.
    let shuffled = mcp.call_tool(
        "fence_prepare",
        serde_json::json!({
            "agent": "wrap-test", "args": {"amount_cents": 4900},
            "tool": "charge_card", "domain": "order:wrap-2",
            "intent": "charge:wrap-2"
        }),
    );
    assert_eq!(
        first, shuffled,
        "key-order variation defeated dedup -- canonicalization is broken"
    );
}

/// A prepared-effect ticket whose read set can never validate, so the
/// child's `fence_commit` always answers with a tool-level error.
fn doomed_commit_args(intent: &str) -> serde_json::Value {
    serde_json::json!({
        "prepared": {
            "intent": intent,
            "parent": null,
            "domain": "order:doomed",
            "seq": 1,
            "tool": "charge_card",
            "args": {"amount_cents": 4900},
            "vector_clock": {},
            "read_set": [{"domain": "inventory:never-reserved", "seq": 9}],
            "agent": "wrap-test"
        },
        "result": {"charge_id": "ch_1"}
    })
}

#[test]
fn a_tool_level_error_does_not_brick_the_identical_retry() {
    // The child answered definitively (isError), so wrap is supposed to
    // "free the intent so the agent's retry can execute again" -- its own
    // words, and what its instructions promise the agent. If wrap instead
    // FENCES the intent, this exact call is refused at the proxy for the
    // whole result_ttl (24h) with no escape hatch: a transient error from
    // any wrapped tool permanently bricks that call.
    let mut mcp = Mcp::spawn_wrap_of_self();
    let args = doomed_commit_args("charge:wrap-err-1");

    let first = mcp.call_tool("fence_commit", args.clone());
    let first_text = first.to_string();
    assert!(
        first_text.contains("stale"),
        "expected the child's ReadSetStale error to come back, got {first}"
    );

    let second = mcp.call_tool("fence_commit", args);
    let second_text = second.to_string();
    assert!(
        !second_text.contains("call refused"),
        "wrap fenced the intent after a tool-level error -- the identical retry \
         never reached the child and is now bricked. Got: {second}"
    );
    assert!(
        second_text.contains("stale"),
        "the retry should have reached the child and got its error again, got {second}"
    );
}

#[test]
fn genuinely_different_arguments_pass_through_as_new_actions() {
    let mut mcp = Mcp::spawn_wrap_of_self();

    let a = mcp.call_tool(
        "fence_prepare",
        prepare_args("charge:wrap-3", "order:wrap-3"),
    );
    let b = mcp.call_tool(
        "fence_prepare",
        prepare_args("charge:wrap-4", "order:wrap-4"),
    );

    assert_eq!(a["status"], "fresh");
    assert_eq!(b["status"], "fresh");
    assert_ne!(
        a["prepared"]["intent"], b["prepared"]["intent"],
        "distinct actions must not be collapsed into one intent"
    );
}
