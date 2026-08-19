//! Zero-setup demo — `effectfence demo`.
//!
//! A cautious first-timer will not fire twelve real calls at their production
//! server on a stranger's say-so, and they shouldn't. So this binary talks to
//! ITSELF: `demo` spawns a built-in fake effectful server (`__demo-server`, a
//! `charge_card` tool that mints a fresh id on every call -- a real
//! double-fire), then runs the REAL probe against it, first raw then behind
//! the real fence. Nothing is installed, nothing external fires, and every
//! moving part -- probe, wrap, fence -- is the genuine article, not a mock.

use std::io::{BufRead, Write};

use anyhow::Result;

/// The hidden child: a minimal MCP stdio server with one intentionally
/// unfenced, double-firing tool. Not listed in help; only `demo` spawns it.
pub fn run_demo_server() -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let mut counter: u64 = 0;
    for line in stdin.lock().lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();
        let resp = match method {
            "initialize" => Some(serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": msg["params"]["protocolVersion"]
                        .as_str().unwrap_or("2025-06-18"),
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "effectfence-demo", "version": env!("CARGO_PKG_VERSION")}
                }
            })),
            // Notifications carry no id and get no reply.
            "notifications/initialized" => None,
            "tools/list" => Some(serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"tools": [{
                    "name": "charge_card",
                    "description": "DEMO tool: 'charges a card' by minting a fresh id every call. \
                                    Intentionally has NO idempotency -- it is here to be double-fired.",
                    "inputSchema": {"type": "object", "properties": {"amount": {"type": "integer"}}}
                }]}
            })),
            "tools/call" => {
                counter += 1; // a fresh, distinct effect per call -- the double-fire
                Some(serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "content": [{"type": "text",
                            "text": format!("{{\"charge_id\":\"ch_{counter:05}\",\"status\":\"captured\"}}")}],
                        "isError": false
                    }
                }))
            }
            _ => id.as_ref().map(|_| {
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": "method not found"}
                })
            }),
        };
        if let Some(r) = resp {
            writeln!(out, "{}", serde_json::to_string(&r)?)?;
            out.flush()?;
        }
    }
    Ok(())
}

fn self_exe() -> Result<String> {
    Ok(std::env::current_exe()?.to_string_lossy().to_string())
}

fn probe_args(child: Vec<String>) -> Vec<String> {
    let mut v = vec![
        "--tool".into(),
        "charge_card".into(),
        "--args".into(),
        "{\"amount\":4900}".into(),
        "--calls".into(),
        "12".into(),
        "--".into(),
    ];
    v.extend(child);
    v
}

/// Run the two-act demo: the same twelve calls, raw then fenced.
pub async fn run_demo() -> Result<()> {
    let exe = self_exe()?;

    eprintln!(
        "\n\x1b[1meffectfence demo\x1b[0m — twelve agents reach for one $49 charge at the same instant."
    );
    eprintln!("Nothing is installed and no real charge fires: this binary is talking to itself.\n");
    eprintln!("\x1b[1m── Act 1: the raw server, no fence ──\x1b[0m");

    // Raw: probe -> the built-in double-firing demo server.
    crate::probe::run_probe(probe_args(vec![exe.clone(), "__demo-server".into()])).await?;

    eprintln!("\n\x1b[1m── Act 2: the SAME twelve calls, behind the fence ──\x1b[0m");

    // Fenced: probe -> effectfence wrap -> the same demo server.
    crate::probe::run_probe(probe_args(vec![
        exe.clone(),
        "wrap".into(),
        "--".into(),
        exe.clone(),
        "__demo-server".into(),
    ]))
    .await?;

    eprintln!(
        "\nThat is the whole idea. Now point it at YOUR server (fires real calls, so use a test tool):"
    );
    eprintln!(
        "  \x1b[1meffectfence probe --tool <tool> --args '<json>' --calls 12 -- npx -y <your-mcp-server>\x1b[0m\n"
    );
    Ok(())
}
