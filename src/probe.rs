//! Probe mode — `effectfence probe --tool <name> --calls <N> -- <command that starts any MCP server>`
//!
//! The footprints-on-their-floor instrument. `wrap` proves the fence works
//! on OUR fixtures (turf_war, storm). A buyer's honest question is different:
//! "does MY stack actually double-fire?" The probe answers it ON THEIR STACK.
//!
//! It is a bare MCP CLIENT. It spawns the operator's real server, then fires
//! N byte-identical concurrent calls at one named tool — the twin-caller race
//! that happens when two agents reach for the same action at the same instant.
//! It then reports how many DISTINCT effects landed:
//!
//! ```text
//!   probe ──N identical calls, concurrently──> real tool server
//!   report: 12 calls fired, 12 distinct results  ->  12 executions of one action
//! ```
//!
//! Honesty (measure the instrument): the probe can only see what the server
//! RETURNS. Distinct successful responses to identical calls are undeniable
//! proof of double-execution (12 different charge ids = 12 real charges).
//! Identical responses are INCONCLUSIVE from the response alone — the server
//! may be idempotent, or it may have fired twice without revealing it. The
//! probe says so rather than claiming a zero it cannot prove.
//!
//! Safety: it fires ONLY the one tool the operator names, with the exact args
//! the operator supplies. It never enumerates and hammers a server blindly.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use rmcp::{
    RoleClient, ServiceExt,
    model::CallToolRequestParams,
    service::RunningService,
    transport::{ConfigureCommandExt, TokioChildProcess},
};

/// Parsed `probe` invocation.
struct ProbeArgs {
    tool: String,
    args: serde_json::Value,
    calls: usize,
    child_cmd: Vec<String>,
}

fn parse(raw: &[String]) -> Result<ProbeArgs> {
    let mut tool: Option<String> = None;
    let mut args_json: Option<String> = None;
    let mut calls: usize = 12;
    let mut child_cmd: Vec<String> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--tool" => {
                tool = Some(raw.get(i + 1).context("--tool needs a value")?.clone());
                i += 2;
            }
            "--args" => {
                args_json = Some(raw.get(i + 1).context("--args needs a JSON value")?.clone());
                i += 2;
            }
            "--calls" => {
                calls = raw
                    .get(i + 1)
                    .context("--calls needs a number")?
                    .parse()
                    .context("--calls must be a positive integer")?;
                i += 2;
            }
            "--" => {
                child_cmd = raw[i + 1..].to_vec();
                break;
            }
            other => bail!(
                "probe: unexpected argument `{other}`. \
                 Usage: effectfence probe --tool <name> [--args '<json>'] [--calls N] -- <command> [args...]"
            ),
        }
    }
    let tool = tool.context(
        "probe: --tool is required. \
         Usage: effectfence probe --tool <name> [--args '<json>'] [--calls N] -- <command> [args...]",
    )?;
    if calls == 0 {
        bail!("probe: --calls must be at least 1");
    }
    if child_cmd.is_empty() {
        bail!("probe: no server command after `--`. Usage: ... -- <command> [args...]");
    }
    let args = match args_json {
        Some(s) => serde_json::from_str(&s).context("probe: --args is not valid JSON")?,
        None => serde_json::json!({}),
    };
    Ok(ProbeArgs {
        tool,
        args,
        calls,
        child_cmd,
    })
}

/// Canonical string form of a tool result's *observable payload*, so that two
/// responses are grouped iff they are byte-identical in what they returned.
/// Deliberately ignores protocol-level `_meta`.
fn fingerprint(result: &rmcp::model::CallToolResult) -> String {
    let payload = serde_json::json!({
        "content": &result.content,
        "structured": &result.structured_content,
        "is_error": result.is_error,
    });
    canonical(&payload)
}

fn canonical(v: &serde_json::Value) -> String {
    fn go(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(m) => {
                let sorted: BTreeMap<String, serde_json::Value> =
                    m.iter().map(|(k, x)| (k.clone(), go(x))).collect();
                serde_json::to_value(sorted).expect("BTreeMap serialization cannot fail")
            }
            serde_json::Value::Array(a) => serde_json::Value::Array(a.iter().map(go).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string(&go(v)).expect("Value serialization")
}

async fn spawn_child(cmd: &[String]) -> Result<RunningService<RoleClient, ()>> {
    let (program, rest) = cmd.split_first().expect("non-empty checked in parse");
    let transport = TokioChildProcess::new(tokio::process::Command::new(program).configure(|c| {
        c.args(rest);
    }))
    .context("probe: failed to spawn target MCP server")?;
    ().serve(transport)
        .await
        .context("probe: MCP handshake with target server failed")
}

/// Entry point.
pub async fn run_probe(raw: Vec<String>) -> Result<()> {
    let p = parse(&raw)?;

    eprintln!(
        "[effectfence-probe] target: `{}`  tool: `{}`  concurrent calls: {}",
        p.child_cmd.join(" "),
        p.tool,
        p.calls
    );

    let child = Arc::new(spawn_child(&p.child_cmd).await?);

    // Validate the tool exists so a typo doesn't read as "0 effects".
    let tools = child
        .list_all_tools()
        .await
        .context("probe: could not list target tools")?;
    if !tools.iter().any(|t| t.name == p.tool) {
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        bail!(
            "probe: target server has no tool `{}`. Available: {}",
            p.tool,
            names.join(", ")
        );
    }

    let arg_obj = match &p.args {
        serde_json::Value::Object(m) => Some(m.clone()),
        serde_json::Value::Null => None,
        other => bail!("probe: --args must be a JSON object, got {other}"),
    };

    // Fire N byte-identical calls concurrently — the twin-caller race.
    let mut handles = Vec::with_capacity(p.calls);
    for _ in 0..p.calls {
        let child = Arc::clone(&child);
        let mut req = CallToolRequestParams::new(p.tool.clone());
        req.arguments = arg_obj.clone();
        handles.push(tokio::spawn(async move { child.call_tool(req).await }));
    }

    let mut ok_fingerprints: BTreeMap<String, usize> = BTreeMap::new();
    let mut errors = 0usize;
    let mut transport_failures = 0usize;
    for h in handles {
        match h.await {
            Ok(Ok(result)) => {
                if result.is_error.unwrap_or(false) {
                    errors += 1;
                } else {
                    *ok_fingerprints.entry(fingerprint(&result)).or_insert(0) += 1;
                }
            }
            Ok(Err(_)) => transport_failures += 1,
            Err(_) => transport_failures += 1,
        }
    }

    let distinct = ok_fingerprints.len();
    let succeeded: usize = ok_fingerprints.values().sum();

    println!("\n\x1b[1mEffectFence probe — twin-caller race report\x1b[0m");
    println!("{}", "-".repeat(44));
    println!("  tool                : {}", p.tool);
    println!("  identical calls     : {}", p.calls);
    println!("  succeeded           : {succeeded}");
    println!("  tool-level errors   : {errors}");
    println!("  transport failures  : {transport_failures}");
    println!("  DISTINCT effects    : {distinct}");
    println!();

    if distinct > 1 {
        println!(
            "  \x1b[31mPROVEN DOUBLE-FIRE.\x1b[0m {} byte-identical calls produced {} DIFFERENT",
            p.calls, distinct
        );
        println!("  results. Each distinct result is a separate real execution of one");
        println!("  intended action — the duplicate side effect you cannot take back.");
    } else if succeeded > 1 {
        println!(
            "  \x1b[33mINCONCLUSIVE from responses.\x1b[0m {succeeded} calls each returned the SAME"
        );
        println!("  payload. Either the tool is idempotent, or it fired {succeeded} times and");
        println!("  does not reveal it in the response. The probe reports only what the");
        println!("  server shows — it will not claim a zero it cannot prove.");
    } else {
        println!("  Only one call succeeded; no race surface observed at this concurrency.");
    }

    println!();
    println!(
        "  \x1b[1mBehind the fence:\x1b[0m  effectfence wrap -- {}",
        p.child_cmd.join(" ")
    );
    println!(
        "  those {} identical calls admit \x1b[1mexactly one\x1b[0m execution; every duplicate",
        p.calls
    );
    println!("  is handed the recorded result instead of firing again. Verify it:");
    println!("  re-run this probe through `wrap` and DISTINCT effects drops to 1.\n");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{CallToolResult, ContentBlock};

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_requires_tool_and_child_command() {
        assert!(parse(&args(&["--", "python3", "srv.py"])).is_err()); // no --tool
        assert!(parse(&args(&["--tool", "charge"])).is_err()); // no child cmd
    }

    #[test]
    fn parse_reads_tool_args_calls_and_child() {
        let p = parse(&args(&[
            "--tool",
            "charge_card",
            "--args",
            r#"{"amount":4900}"#,
            "--calls",
            "7",
            "--",
            "python3",
            "srv.py",
        ]))
        .unwrap();
        assert_eq!(p.tool, "charge_card");
        assert_eq!(p.calls, 7);
        assert_eq!(p.args, serde_json::json!({"amount": 4900}));
        assert_eq!(
            p.child_cmd,
            vec!["python3".to_string(), "srv.py".to_string()]
        );
    }

    #[test]
    fn parse_defaults_calls_to_twelve_and_args_to_empty_object() {
        let p = parse(&args(&["--tool", "t", "--", "srv"])).unwrap();
        assert_eq!(p.calls, 12);
        assert_eq!(p.args, serde_json::json!({}));
    }

    #[test]
    fn parse_rejects_zero_calls_and_bad_json() {
        assert!(parse(&args(&["--tool", "t", "--calls", "0", "--", "srv"])).is_err());
        assert!(parse(&args(&["--tool", "t", "--args", "{not json", "--", "srv"])).is_err());
    }

    #[test]
    fn canonical_is_key_order_independent() {
        let a = serde_json::json!({"b": 1, "a": {"y": 2, "x": 3}});
        let b = serde_json::json!({"a": {"x": 3, "y": 2}, "b": 1});
        assert_eq!(canonical(&a), canonical(&b));
    }

    #[test]
    fn distinct_results_have_distinct_fingerprints() {
        // Two "charges" with different ids are the double-fire the probe must see.
        let r1 = CallToolResult::success(vec![ContentBlock::text(r#"{"charge_id":"ch_1"}"#)]);
        let r2 = CallToolResult::success(vec![ContentBlock::text(r#"{"charge_id":"ch_2"}"#)]);
        assert_ne!(fingerprint(&r1), fingerprint(&r2));
        // Identical payloads collapse to one bucket (the fenced/replay case).
        let r3 = CallToolResult::success(vec![ContentBlock::text(r#"{"charge_id":"ch_1"}"#)]);
        assert_eq!(fingerprint(&r1), fingerprint(&r3));
    }
}
